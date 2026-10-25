//! Authoritative ordered rsid/rsid-store source and runtime shard inventory.
//! The historical checker is a read-only specification, never an executable oracle.
use crate::{
    Ctx,
    cli::{self, Opt},
    re::{self, Re},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::Path,
    sync::LazyLock,
};
pub mod cfg;
pub mod mounts;
pub mod runtime;
pub use cfg::Host;
pub use mounts::{
    check_shard_module_mounts, mount_declarations, mounted_sources, unmounted_test_sources,
};
pub use runtime::{RuntimeExceptions, check_runtime};

pub const PACKAGES: &[&str] = &["rsid", "rsid-store"];
pub const SHARD_KIND_ORDER: &[&str] = &["store", "session", "memory", "other"];
pub const SHARD_GATE_FIX: &str = "the shard gate must be its own line, exactly `#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-X\"))]`, directly above `#[test]` (#1621); put `target_os` or any other condition in a separate `#[cfg(...)]` line above the shard gate";
static SHARD_GATE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r#"^#\[cfg\(any\(not\(feature = "test-shard-mode"\), feature = "test-shard-([a-z]+-[0-9]+)"\)\)\]$"#,
    )
});
static ATTR: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^[ \t]*#\[\s*(?:test|tokio::test(?:\([^]\n]*\))?)\s*\]"));
static FUNCTION: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\b(?:async\s+)?fn\s+([A-Za-z_][A-Za-z_0-9]*)\s*\("));

/// Ordering is part of the runner contract: kind/number, then PACKAGES order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shard {
    pub name: String,
    pub packages: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestIdentity {
    pub source: String,
    pub name: String,
    pub line: usize,
    pub shard: String,
}
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    /// Sorted source paths, then attribute order within each source.
    pub tests: Vec<TestIdentity>,
    pub gated: usize,
    pub platform_conditional: BTreeMap<String, BTreeMap<String, usize>>,
}
pub fn package_of(path: &str) -> Result<&'static str, String> {
    PACKAGES
        .iter()
        .copied()
        .find(|p| path.starts_with(&format!("crates/{p}/")))
        .ok_or_else(|| format!("{path}: not under a sharded rsid package"))
}
pub fn read_shard_packages(root: &Path) -> Result<Vec<Shard>, String> {
    read_packages(root, PACKAGES)
}
/// Explicit package list supports isolated fixtures without process-global state.
pub fn read_packages(root: &Path, packages: &[&str]) -> Result<Vec<Shard>, String> {
    let mut found: Vec<Shard> = Vec::new();
    for package in packages {
        let path = root.join(format!("crates/{package}/Cargo.toml"));
        let cargo: toml::Value = toml::from_str(
            &fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?,
        )
        .map_err(|e| e.to_string())?;
        let features = cargo
            .get("features")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| "'features'".to_string())?;
        let empty = toml::Value::Array(Vec::new());
        if features.get("default") != Some(&empty)
            || features.get("test-shard-mode") != Some(&empty)
        {
            return Err(format!(
                "{package} default/test-shard-mode features changed"
            ));
        }
        let mut count = 0;
        for (feature, deps) in features {
            let Some(shard) = feature.strip_prefix("test-shard-").filter(|s| *s != "mode") else {
                continue;
            };
            static NAME: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[a-z]+-[0-9]+$"));
            if !NAME.is_match(shard) {
                return Err(format!("invalid shard feature {feature}"));
            }
            if deps
                .as_array()
                .is_none_or(|a| a.len() != 1 || a[0].as_str() != Some("test-shard-mode"))
            {
                return Err(format!("{feature} must imply only test-shard-mode"));
            }
            if let Some(entry) = found.iter_mut().find(|s| s.name == shard) {
                entry.packages.push((*package).into());
            } else {
                found.push(Shard {
                    name: shard.into(),
                    packages: vec![(*package).into()],
                });
            }
            count += 1;
        }
        if count == 0 {
            return Err(format!("{package} declares no library shard features"));
        }
    }
    fn key(s: &Shard) -> (usize, usize, &str) {
        let (kind, number) = s.name.rsplit_once('-').unwrap_or(("", "0"));
        let number = number.trim_start_matches('0');
        (
            SHARD_KIND_ORDER
                .iter()
                .position(|k| *k == kind)
                .unwrap_or(SHARD_KIND_ORDER.len()),
            number.len(),
            number,
        )
    }
    // Stable ties retain manifest insertion order, including unknown kinds and
    // equivalent numeric spellings. Compare decimal strings without overflow.
    found.sort_by(|a, b| key(a).cmp(&key(b)));
    Ok(found)
}
pub type Sources = BTreeMap<String, String>;
pub fn read_sources(root: &Path) -> Result<Sources, String> {
    fn visit(root: &Path, path: &Path, files: &mut Sources) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                visit(root, &entry.path(), files)?;
            } else if entry.path().extension().is_some_and(|e| e == "rs") {
                let name = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(
                    name,
                    fs::read_to_string(entry.path()).map_err(|e| e.to_string())?,
                );
            }
        }
        Ok(())
    }
    let mut files = Sources::new();
    for p in PACKAGES {
        visit(root, &root.join(format!("crates/{p}/src")), &mut files)?;
    }
    Ok(files)
}
pub fn attribute_block(lines: &[&str], index: usize) -> String {
    let mut collected = Vec::new();
    for line in lines[..index].iter().rev() {
        let s = line.trim();
        if s.is_empty() || (s.ends_with([';', '{', '}']) && !s.starts_with("#[")) {
            break;
        }
        collected.push(s);
    }
    collected.join("\n")
}
pub fn extract_tests(path: &str, source: &str) -> Result<Vec<(String, usize, bool)>, String> {
    let code = crate::rust_source::mask(source, path)?;
    extract_masked(path, source, &code)
}
fn extract_masked(
    path: &str,
    source: &str,
    code: &str,
) -> Result<Vec<(String, usize, bool)>, String> {
    let raw: Vec<&str> = source.lines().collect();
    let lines: Vec<&str> = code.lines().collect();
    if raw.len() != lines.len() {
        return Err(format!("{path}: scanner lost line alignment"));
    }
    let mut tests = Vec::new();
    let mut names = BTreeSet::new();
    for (i, line) in lines.iter().enumerate() {
        if !ATTR.is_match(line) {
            continue;
        }
        let mut name = None;
        for next in lines.iter().skip(i + 1).take(15) {
            if ATTR.is_match(next) {
                break;
            }
            if let Some(m) = FUNCTION.search(next) {
                name = Some(re::group(&m, 1).to_string());
                break;
            }
        }
        let name = name.ok_or_else(|| {
            format!(
                "{path}:{}: test attribute has no following function within 15 lines",
                i + 1
            )
        })?;
        if !names.insert(name.clone()) {
            return Err(format!(
                "{path}: duplicate test function names in one source file"
            ));
        }
        tests.push((
            name,
            i + 1,
            i > 0
                && raw[i - 1].trim_start().starts_with(
                    "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-",
                ),
        ));
    }
    Ok(tests)
}
fn helper_message(path: &str, line: usize, kind: &str) -> String {
    format!(
        "{path}:{line}: ungated {kind} in a module whose `use` items are shard-gated; helpers follow their file's shard gate, so a shard build without the gated imports cannot compile it (add the file's `#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-...\"))]` above it)"
    )
}
fn check_helpers(path: &str, source: &str, code: &str) -> Result<(), String> {
    static ITEM: LazyLock<Re> = LazyLock::new(|| {
        Re::new(
            r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|unsafe|const|extern)\s+)*(fn|struct|enum|impl|trait|static|type|union|const|mod)\b",
        )
    });
    static USE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\b"));
    static TEST: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[\s*(?:test|tokio::test)\b"));
    #[derive(Default)]
    struct Scope {
        gated: bool,
        gated_use: bool,
        items: Vec<(usize, String)>,
    }
    let raw: Vec<&str> = source.lines().collect();
    let mut stack = vec![Scope::default()];
    let mut pending = false;
    for (i, line) in code.lines().enumerate() {
        let item = ITEM.search(line);
        if item.is_some() || USE.is_match(line) {
            let attrs = attribute_block(&raw, i);
            let gated = attrs.contains("test-shard-");
            let scope = stack
                .last_mut()
                .ok_or_else(|| format!("{path}: unbalanced braces"))?;
            if let Some(item) = item {
                pending = gated;
                let kind = re::group(&item, 1);
                if kind != "mod" && !gated && !TEST.is_match(&attrs) && !scope.gated {
                    scope.items.push((i + 1, kind.into()));
                }
            } else {
                scope.gated_use |= gated;
            }
        }
        for b in line.bytes() {
            if b == b'{' {
                stack.push(Scope {
                    gated: pending || stack.last().is_some_and(|s| s.gated),
                    ..Scope::default()
                });
                pending = false;
            } else if b == b'}' {
                if stack.len() == 1 {
                    return Err(format!("{path}:{}: unbalanced closing brace", i + 1));
                }
                let scope = stack.pop().unwrap_or_default();
                if scope.gated_use && !scope.gated {
                    if let Some((line, kind)) = scope.items.first() {
                        return Err(helper_message(path, *line, kind));
                    }
                }
            }
        }
    }
    if let Some(scope) = stack.first() {
        if scope.gated_use {
            if let Some((line, kind)) = scope.items.first() {
                return Err(helper_message(path, *line, kind));
            }
        }
    }
    Ok(())
}
fn ungated_test_fix(lines: &[&str], line: usize, package: &str, shards: &[Shard]) -> String {
    let shard = lines.iter().find_map(|s| {
        SHARD_GATE
            .search(s.trim())
            .map(|m| re::group(&m, 1).to_string())
            .filter(|name| {
                shards
                    .iter()
                    .any(|s| s.name == *name && s.packages.iter().any(|p| p == package))
            })
    });
    let suggestion = if shard.is_some() {
        "the shard this file's other tests use"
    } else {
        "other-01 for a file with no gated tests yet"
    };
    let indent = &lines[line - 1][..lines[line - 1].len() - lines[line - 1].trim_start().len()];
    format!(
        "add this line directly above `#[test]` ({suggestion}):\n    {indent}#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-{}\"))]\n  Keep `use super::*` out of a module whose tests may all be cfg'd out in a shard build (it becomes an unused import); import only what the gated tests use, inside them",
        shard.as_deref().unwrap_or("other-01")
    )
}
pub fn source_inventory(
    files: &Sources,
    shards: &[Shard],
    require_gates: bool,
    host: &Host,
) -> Result<Inventory, String> {
    let mut inventory = Inventory::default();
    let mut file_shards = BTreeMap::new();
    let mut ungated = Vec::new();
    let excluded_files = mounts::host_excluded_module_files(files, host)?;
    for (path, source) in files {
        let package = package_of(path)?;
        let bin = path == "crates/rsid/src/main.rs" || path.starts_with("crates/rsid/src/bin/");
        let code = crate::rust_source::mask(source, path)?;
        cfg::check_shard_cfg_attributes(path, source, &code)?;
        if !bin {
            check_helpers(path, source, &code)?;
        }
        let raw: Vec<&str> = source.lines().collect();
        let ranges = mounts::host_excluded_line_ranges(&raw, host)?;
        for (name, line, has_gate) in extract_masked(path, source, &code)? {
            let previous = if line > 1 { raw[line - 2] } else { "" };
            if bin {
                if has_gate || previous.contains("test-shard-") {
                    return Err(format!(
                        "{path}:{line}: binary test has a library shard gate"
                    ));
                }
                inventory.tests.push(TestIdentity {
                    source: path.clone(),
                    name,
                    line,
                    shard: "bin".into(),
                });
                continue;
            }
            if let Some(m) = SHARD_GATE.search(previous.trim()) {
                let shard = re::group(&m, 1).to_string();
                let Some(entry) = shards.iter().find(|s| s.name == shard) else {
                    return Err(format!("{path}:{line}: unknown shard {shard}"));
                };
                if !entry.packages.iter().any(|p| p == package) {
                    return Err(format!(
                        "{path}:{line}: shard {shard} is not a {package} feature"
                    ));
                }
                let indent = |s: &str| s.len() - s.trim_start().len();
                if indent(previous) != indent(raw[line - 1]) {
                    return Err(format!("{path}:{line}: incorrect shard gate indentation"));
                }
                let before = if line > 2 { raw[line - 3] } else { "" };
                if before.trim_start().starts_with("#[cfg(") && before.contains("test-shard-") {
                    return Err(format!("{path}:{line}: duplicate shard gates"));
                }
                if cfg::host_excluded(before, host)
                    || excluded_files.contains(path)
                    || ranges.iter().any(|(a, b)| *a < line && line < *b)
                {
                    *inventory
                        .platform_conditional
                        .entry(shard.clone())
                        .or_default()
                        .entry(name.clone())
                        .or_default() += 1;
                }
                if file_shards.get(path).is_some_and(|s| s != &shard) {
                    return Err(format!("{path}:{line}: one source file spans shards"));
                }
                file_shards.insert(path.clone(), shard.clone());
                inventory.gated += 1;
                inventory.tests.push(TestIdentity {
                    source: path.clone(),
                    name,
                    line,
                    shard,
                });
            } else if attribute_block(&raw, line - 1).contains("test-shard-") {
                return Err(format!(
                    "{path}:{line}: incorrect shard gate; {SHARD_GATE_FIX}"
                ));
            } else if require_gates {
                return Err(format!(
                    "{path}:{line}: ungated test blocks a bounded lib harness; {}",
                    ungated_test_fix(&raw, line, package, shards)
                ));
            } else {
                ungated.push((path.clone(), name, line));
            }
        }
    }
    for (source, name, line) in ungated {
        let shard = file_shards
            .get(&source)
            .ok_or_else(|| format!("{source}:{line}: ungated test has no declared file shard"))?
            .clone();
        inventory.tests.push(TestIdentity {
            source,
            name,
            line,
            shard,
        });
    }
    Ok(inventory)
}
pub fn validate_inventory(
    files: &Sources,
    inventory: &Inventory,
    shards: &[Shard],
) -> Result<(), String> {
    let unmounted = unmounted_test_sources(files)?;
    if !unmounted.is_empty() {
        return Err(format!(
            "tests in files no `mod`, `#[path]` or `include!` declaration reaches (rustc never compiles them): {}; a `#[path]`-mounted file's `mod tests;` loads `tests.rs` beside it, so mount the test file with an explicit `#[path = \"<stem>/tests.rs\"]`",
            unmounted.join(", ")
        ));
    }
    check_shard_module_mounts(files, inventory)?;
    for shard in shards {
        for package in &shard.packages {
            if !inventory.tests.iter().any(|t| {
                t.shard == shard.name && package_of(&t.source).ok() == Some(package.as_str())
            }) {
                return Err(format!(
                    "{package} declares shard {} but has no test in it",
                    shard.name
                ));
            }
        }
    }
    Ok(())
}
/// `target_directory` of `cargo metadata --format-version 1` output (the shell
/// runner's `cargo metadata | …` target-directory lookup, no interpreter needed).
pub fn metadata_target_directory(metadata: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(metadata).map_err(|e| format!("cargo metadata is not JSON: {e}"))?;
    match value.get("target_directory").and_then(|v| v.as_str()) {
        Some(dir) if !dir.is_empty() => Ok(dir.to_string()),
        _ => Err("cargo metadata has no target_directory".to_string()),
    }
}
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    const USAGE: &str = "[--require-gates] [--list-shards] [--list-shard-packages] [--shards-only] [--runtime-dir RUNTIME_DIR] [--runtime-shard RUNTIME_SHARD] [--root ROOT] [--target-directory]";
    let opts = [
        Opt {
            name: "--require-gates",
            value: false,
        },
        Opt {
            name: "--list-shards",
            value: false,
        },
        Opt {
            name: "--list-shard-packages",
            value: false,
        },
        Opt {
            name: "--shards-only",
            value: false,
        },
        Opt {
            name: "--runtime-dir",
            value: true,
        },
        Opt {
            name: "--runtime-shard",
            value: true,
        },
        Opt {
            name: "--root",
            value: true,
        },
        Opt {
            name: "--target-directory",
            value: false,
        },
    ];
    let p = match cli::parse(
        "check-rsid-test-shards",
        USAGE,
        args,
        &opts,
        &[],
        Some(0),
        err,
    ) {
        Ok(p) => p,
        Err(e) => return e.code(),
    };
    if p.flag("--target-directory") {
        let mut metadata = String::new();
        let read = std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut metadata)
            .map_err(|e| format!("cannot read cargo metadata: {e}"));
        return match read.and_then(|_| metadata_target_directory(&metadata)) {
            Ok(dir) => {
                let _ = writeln!(out, "{dir}");
                0
            }
            Err(e) => {
                let _ = writeln!(err, "rsid test shard check failed: {e}");
                1
            }
        };
    }
    if p.value("--runtime-shard").is_some() && p.value("--runtime-dir").is_none() {
        let _ = writeln!(
            err,
            "usage: check-rsid-test-shards {USAGE}\ncheck-rsid-test-shards: error: --runtime-shard requires --runtime-dir"
        );
        return 2;
    }
    let run = || -> Result<Vec<String>, String> {
        let root = ctx.path(p.value("--root").unwrap_or("."));
        let shards = read_shard_packages(&root)?;
        if let Some(s) = p.value("--runtime-shard") {
            if !shards.iter().any(|v| v.name == s) {
                return Err(format!("unknown runtime shard {s}"));
            }
        }
        let mut output = Vec::new();
        let inventory = if p.flag("--shards-only")
            && (p.flag("--list-shards") || p.flag("--list-shard-packages"))
            && p.value("--runtime-dir").is_none()
        {
            None
        } else {
            let files = read_sources(&root)?;
            let inventory = source_inventory(
                &files,
                &shards,
                p.flag("--require-gates") || p.value("--runtime-dir").is_some(),
                &Host::default(),
            )?;
            validate_inventory(&files, &inventory, &shards)?;
            if let Some(dir) = p.value("--runtime-dir") {
                let selected: Vec<String> = shards
                    .iter()
                    .filter(|s| p.value("--runtime-shard").is_none_or(|v| v == s.name))
                    .map(|s| s.name.clone())
                    .collect();
                output.push(check_runtime(
                    &ctx.path(dir),
                    &inventory,
                    &shards,
                    &selected,
                    &RuntimeExceptions::default(),
                )?);
            }
            Some(inventory)
        };
        if p.flag("--list-shard-packages") {
            output.extend(
                shards
                    .iter()
                    .map(|s| format!("{} {}", s.name, s.packages.join(" "))),
            );
        } else if p.flag("--list-shards") {
            output.extend(shards.iter().map(|s| s.name.clone()));
        } else if let Some(inventory) = inventory {
            let total = inventory.tests.len();
            let bin = inventory.tests.iter().filter(|t| t.shard == "bin").count();
            output.push(format!("static inventory: {total} test identities ({} library, {bin} binary); {}/{} library gates present",total-bin,inventory.gated,total-bin));
        }
        Ok(output)
    };
    match run() {
        Ok(lines) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            0
        }
        Err(e) => {
            let _ = writeln!(err, "rsid test shard check failed: {e}");
            1
        }
    }
}
#[cfg(test)]
mod tests;
