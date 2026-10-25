//! Exact-source provisional migration transforms and proofs (#361).
use crate::{
    Ctx, migration_declaration as declaration,
    re::{Re, group},
    released_migrations as guard,
};
use declaration::{occurrences, path, version_spans};
use guard::{MANIFEST, MIGRATION_DIR, MIGRATION_PATH as STORE, Result, TOKEN};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};
const STORE_TESTS: &str = guard::REWIND_TESTS;
const DECLARATIONS: &str = declaration::DECLARATIONS;

fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("invalid provisional unit: missing string {key}"))
}
fn int(v: &Value, key: &str) -> Result<i64> {
    v[key]
        .as_i64()
        .ok_or_else(|| format!("invalid provisional unit: missing integer {key}"))
}
fn list<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    v[key]
        .as_array()
        .ok_or_else(|| format!("invalid provisional unit: missing list {key}"))
}
fn null() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}
fn git_command(repo: &Path) -> Command {
    let mut c = Command::new("git");
    for (key, _) in std::env::vars_os() {
        let k = key.to_string_lossy();
        if k.starts_with("GIT_") || k.starts_with("RSI_") || k.starts_with("PYTHON") {
            c.env_remove(key);
        }
    }
    c.env("HOME", "/nonexistent")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", null())
        .env("GIT_TERMINAL_PROMPT", "0");
    c.args([
        "--no-optional-locks",
        "-c",
        &format!("core.hooksPath={}", null()),
        "-c",
        "commit.gpgsign=false",
        "-c",
        "rerere.enabled=false",
        "-c",
        "gc.auto=0",
        "-C",
    ])
    .arg(repo);
    c
}
fn execute(
    repo: &Path,
    args: &[&str],
    data: Option<&[u8]>,
    env: &[(&str, &str)],
) -> Result<Output> {
    let mut c = git_command(repo);
    c.args(args)
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    if let Some(data) = data {
        // Drain stdout/stderr while feeding large blobs to avoid pipe deadlock.
        let mut stdin = child.stdin.take().ok_or("missing git input pipe")?;
        return std::thread::scope(|scope| {
            let writer = scope.spawn(move || stdin.write_all(data));
            let output = child.wait_with_output().map_err(|e| e.to_string())?;
            writer
                .join()
                .map_err(|_| "git input writer panicked")?
                .map_err(|e| e.to_string())?;
            Ok(output)
        });
    }
    child.wait_with_output().map_err(|e| e.to_string())
}
fn run(repo: &Path, args: &[&str], data: Option<&[u8]>, env: &[(&str, &str)]) -> Result<Vec<u8>> {
    let done = execute(repo, args, data, env)?;
    if !done.status.success() {
        return Err(format!(
            "git {} refused: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&done.stderr).trim()
        ));
    }
    Ok(done.stdout)
}
fn text_run(repo: &Path, args: &[&str]) -> Result<String> {
    String::from_utf8(run(repo, args, None, &[])?)
        .map(|s| s.trim().into())
        .map_err(|e| e.to_string())
}
pub fn oid(value: &str, argument: &str) -> Result<()> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!(
            "noncanonical commit ID for {argument}: pass a full 40-hex SHA (git rev-parse <ref>)"
        ));
    }
    Ok(())
}
fn show(repo: &Path, rev: &str, name: &str) -> Result<Option<Vec<u8>>> {
    let result = execute(repo, &["show", &format!("{rev}:{name}")], None, &[])?;
    if result.status.success() {
        return Ok(Some(result.stdout));
    }
    // Only a path the revision's tree does not list is absent; any other failure is inconclusive
    // and must refuse rather than read as "no such file".
    let listed = execute(
        repo,
        &["ls-tree", "--name-only", rev, "--", name],
        None,
        &[],
    )?;
    if listed.status.success() && listed.stdout.is_empty() {
        Ok(None)
    } else {
        Err(format!(
            "cannot read {rev}:{name}: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ))
    }
}
fn ancestor(repo: &Path, old: &str, new: &str) -> Result<bool> {
    Ok(
        execute(repo, &["merge-base", "--is-ancestor", old, new], None, &[])?
            .status
            .success(),
    )
}
fn changed(repo: &Path, base: &str, source: &str) -> Result<BTreeSet<String>> {
    let bytes = run(
        repo,
        &["diff", "--no-renames", "--name-only", "-z", base, source],
        None,
        &[],
    )?;
    let names = bytes
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|s| String::from_utf8(s.to_vec()).map_err(|e| e.to_string()))
        .collect::<Result<BTreeSet<_>>>()?;
    if names.len() > declaration::MAX_CHANGED_PATHS {
        return Err("accepted source path count exceeds provisional bound".into());
    }
    Ok(names)
}
fn protected_paths(repo: &Path, rev: &str, manifest: &Value) -> Result<BTreeSet<String>> {
    let mut paths = guard::tracked_source_paths(Some(manifest));
    let listing = text_run(
        repo,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            rev,
            "--",
            "crates/rsid-store/src/store/migrations/",
            "crates/rsid/src/store/migrations/",
        ],
    )?;
    for name in listing.lines() {
        if guard::migration_file_version(name)?.is_some() {
            paths.insert(name.into());
        }
    }
    Ok(paths)
}
fn revision_inventory(repo: &Path, rev: &str, base: Option<&str>) -> Result<Value> {
    let raw = show(repo, rev, MANIFEST)?.ok_or_else(|| format!("{MANIFEST} missing at {rev}"))?;
    let parse = || -> Result<Value> {
        let manifest = guard::load_manifest(
            std::str::from_utf8(&raw).map_err(|e| e.to_string())?,
            &format!("{rev}:{MANIFEST}"),
        )?;
        let mut files = guard::Files::new();
        for name in protected_paths(repo, rev, &manifest)? {
            let data =
                show(repo, rev, &name)?.ok_or("manifest names a missing protected source")?;
            files.insert(name, String::from_utf8(data).map_err(|e| e.to_string())?);
        }
        let base_manifest = base
            .map(|b| show(repo, b, MANIFEST))
            .transpose()?
            .flatten()
            .map(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()))
            .transpose()?;
        Ok(guard::validated_inventory(
            &manifest,
            &guard::inventory(&files)?,
            "provisional inventory",
            base_manifest.as_ref(),
        )?
        .0)
    };
    parse().map_err(|e| format!("invalid migration inventory at {rev}: {e}"))
}
fn introduced_ranges(
    repo: &Path,
    base: &str,
    source_path: &str,
    data: &[u8],
) -> Result<Vec<std::ops::Range<usize>>> {
    let Some(old) = show(repo, base, source_path)? else {
        return Ok(if data.is_empty() {
            vec![]
        } else {
            vec![0..data.len()]
        });
    };
    let mut counts: BTreeMap<&[u8], usize> = BTreeMap::new();
    for l in old.split_inclusive(|b| *b == b'\n') {
        *counts.entry(l).or_default() += 1;
    }
    let mut ranges = Vec::new();
    let mut offset = 0;
    for l in data.split_inclusive(|b| *b == b'\n') {
        match counts.get_mut(l) {
            Some(n) if *n > 0 => *n -= 1,
            _ => ranges.push(offset..offset + l.len()),
        }
        offset += l.len();
    }
    Ok(ranges)
}
fn in_ranges(ranges: &[std::ops::Range<usize>], start: usize, end: usize) -> bool {
    ranges.iter().any(|r| r.start <= start && end <= r.end)
}
fn validate_limits(raw: &[u8], d: &Value, new: i64) -> Result<()> {
    if raw.len() > declaration::MAX_DECLARATION_BYTES {
        return Err(format!(
            "declaration is {} bytes, over the {} byte limit",
            raw.len(),
            declaration::MAX_DECLARATION_BYTES
        ));
    }
    let files = list(d, "files")?;
    if files.len() > declaration::MAX_DECLARED_FILES {
        return Err(format!(
            "`files` has {} entries, over the {} limit",
            files.len(),
            declaration::MAX_DECLARED_FILES
        ));
    }
    let mut paths = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    for f in files {
        let p = string(f, "path")?;
        let dest = string(f, "path_template")?.replace(TOKEN, &new.to_string());
        if !paths.insert(p) {
            return Err(format!("duplicate `path` {p}"));
        }
        if !destinations.insert(dest.clone()) {
            return Err(format!(
                "duplicate destination {dest} from `path_template` of {p}"
            ));
        }
        let count = list(f, "sites")?.len();
        if count > declaration::MAX_SITES {
            return Err(format!(
                "`sites` of {p} has {count} entries, over the {} limit",
                declaration::MAX_SITES
            ));
        }
    }
    Ok(())
}
fn validate_source_declarations(
    repo: &Path,
    base: &str,
    source: &str,
    target: &str,
    old: i64,
    new: i64,
) -> Result<()> {
    for name in changed(repo, base, source)? {
        if !name.starts_with(DECLARATIONS) || !name.ends_with(".json") {
            continue;
        }
        let Some(raw) = show(repo, source, &name)? else {
            continue;
        };
        let target_raw = show(repo, target, &name)?;
        if Some(&raw) == target_raw.as_ref() {
            continue;
        }
        let d: Value = serde_json::from_slice(&raw)
            .map_err(|e| format!("invalid provisional site declaration {name}: {e}"))?;
        guard::validate_declaration_shape(&d, &name, Some(old))
            .map_err(|e| format!("invalid provisional file declaration: {e}"))?;
        let validate = || -> Result<()> {
            validate_limits(&raw, &d, new)?;
            if target_raw.is_none() {
                for f in list(&d, "files")? {
                    let p = string(f, "path")?;
                    let original = show(repo, source, p)?.ok_or_else(|| {
                        format!("`source_blob` of {p} does not match the source file")
                    })?;
                    if guard::digest(&original) != string(f, "source_blob")? {
                        return Err(format!(
                            "`source_blob` of {p} does not match the source file"
                        ));
                    }
                    if string(f, "path_template")?.replace(TOKEN, &old.to_string()) != p {
                        return Err(format!("`path_template` of {p} does not match its path"));
                    }
                    for site in list(f, "sites")? {
                        let a = string(site, "anchor")?;
                        if occurrences(&original, a.as_bytes()).len() != 1 {
                            return Err(format!(
                                "anchor in {p} is missing or duplicated: {}",
                                crate::py::repr(a)
                            ));
                        }
                    }
                }
            }
            Ok(())
        };
        validate().map_err(|e| format!("invalid provisional site declaration {name}: {e}"))?;
    }
    Ok(())
}

pub fn inspect(
    repo: &Path,
    base: &str,
    source: &str,
    target: &str,
    allow_noop: bool,
) -> Result<Option<Value>> {
    for (arg, value) in [("--base", base), ("--source", source), ("--target", target)] {
        oid(value, arg)?;
        run(
            repo,
            &["cat-file", "-e", &format!("{value}^{{commit}}")],
            None,
            &[],
        )?;
    }
    if !ancestor(repo, base, source)? {
        return Err("accepted base is not an ancestor of source".into());
    }
    let bm = revision_inventory(repo, base, None)?;
    let sm = revision_inventory(repo, source, Some(base))?;
    let tm = revision_inventory(repo, target, None)?;
    let prior = int(&bm, "latest_schema_version")?;
    let old = int(&sm, "latest_schema_version")?;
    let latest = int(&tm, "latest_schema_version")?;
    let mut texts = Vec::new();
    for name in changed(repo, base, source)? {
        if name.starts_with(DECLARATIONS) && name.ends_with(".json") {
            if let Some(raw) = show(repo, source, &name)? {
                texts.push(String::from_utf8(raw).map_err(|e| e.to_string())?);
            }
        }
    }
    let gaps = guard::parse_pending_predecessors(&texts, prior)
        .map_err(|e| format!("source changed released prefix: {e}"))?;
    guard::validate_append_only(&bm, &sm, &gaps)
        .map_err(|e| format!("source changed released prefix: {e}"))?;
    guard::validate_append_only(&bm, &tm, &[])
        .map_err(|e| format!("target changed accepted base's released prefix: {e}"))?;
    if old == prior {
        return Ok(None);
    }
    if sm.get("migration_dir").is_some() != tm.get("migration_dir").is_some() {
        return Err(format!(
            "migration layout differs between source and target; port the provisional migration to {MIGRATION_DIR}/vNNN.rs before landing"
        ));
    }
    let base_blocks = bm["blocks"].as_object().ok_or("invalid base blocks")?;
    let extras: Vec<_> = sm["blocks"]
        .as_object()
        .ok_or("invalid source blocks")?
        .keys()
        .filter(|k| !base_blocks.contains_key(*k))
        .cloned()
        .collect();
    if old != prior + 1 + gaps.len() as i64 || extras != [old.to_string()] {
        return Err("accepted source has more than one provisional migration".into());
    }
    if !gaps.is_empty() && latest < old - 1 {
        return Err(format!(
            "declared pending predecessor migrations {gaps:?} are not on the target yet; land the sibling slice first"
        ));
    }
    if latest < prior {
        return Err("target schema head is behind the accepted base".into());
    }
    if latest >= i32::MAX as i64 {
        return Err("no representable next i32 schema version".into());
    }
    validate_source_declarations(repo, base, source, target, old, latest + 1)?;
    let noop = || json!({"base":base,"source":source,"target":target,"old_version":old,"new_version":old,"renumber_noop":true});
    if allow_noop && latest + 1 == old {
        return Ok(Some(noop()));
    }
    let all_changed = changed(repo, base, source)?;
    let declarations: Vec<_> = all_changed
        .iter()
        .filter(|n| n.starts_with(DECLARATIONS) && n.ends_with(".json"))
        .collect();
    if declarations.len() != 1 {
        return Err("one new provisional site declaration is required".into());
    }
    let declaration_path = path(declarations[0])?;
    if show(repo, base, declaration_path)?.is_some() {
        return Err("provisional site declaration must be newly added".into());
    }
    let raw =
        show(repo, source, declaration_path)?.ok_or("provisional site declaration is missing")?;
    let d: Value =
        serde_json::from_slice(&raw).map_err(|_| "invalid provisional site declaration")?;
    // A byte-identical declaration already on the target skips new-source
    // validation above. Match the historical outer-envelope checks here, but
    // do not revalidate its old file/site shape before the already-landed path.
    if raw.len() > declaration::MAX_DECLARATION_BYTES {
        return Err(format!(
            "declaration is {} bytes, over the {} byte limit",
            raw.len(),
            declaration::MAX_DECLARATION_BYTES
        ));
    }
    if d.as_object().is_none_or(|m| {
        !["schema_version", "version", "files"]
            .iter()
            .all(|k| m.contains_key(*k))
            || m.keys().any(|k| {
                !["schema_version", "version", "files", "pending_predecessors"]
                    .contains(&k.as_str())
            })
    }) || d["schema_version"].as_i64() != Some(1)
        || d["version"].as_i64() != Some(old)
        || d["files"].as_array().is_none_or(Vec::is_empty)
    {
        return Err("invalid provisional declaration shape or version".into());
    }
    let declared_files = list(&d, "files")?;
    if declared_files.len() > declaration::MAX_DECLARED_FILES {
        return Err(format!(
            "`files` has {} entries, over the {} limit",
            declared_files.len(),
            declaration::MAX_DECLARED_FILES
        ));
    }
    if allow_noop
        && sm.get("migration_dir").is_some()
        && show(repo, target, declaration_path)?.is_some()
        && tm["blocks"].get(old.to_string()).is_some()
    {
        let unit_file = format!("{}/v{old:03}.rs", string(&sm, "migration_dir")?);
        if !declared_files.iter().any(|f| f["path"] == unit_file) {
            return Err(format!("migration file must be declared: {unit_file}"));
        }
        if show(repo, source, &unit_file)? != show(repo, target, &unit_file)? {
            return Err(format!(
                "declared migration {unit_file} is already on rolling with differing content; re-merge the source onto origin/rolling, retain rolling's released migration, and re-submit the merged commit"
            ));
        }
        let mut n = noop();
        n["migration_already_landed"] = json!(true);
        return Ok(Some(n));
    }
    validate_limits(&raw, &d, latest + 1)?;
    let mut declared_paths = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    let mut replacements = Vec::new();
    for f in declared_files {
        let p = path(string(f, "path")?)?;
        let template = string(f, "path_template")?;
        let rendered = template.replace(TOKEN, "VERSION");
        path(&rendered)?;
        if rendered.matches("VERSION").count() != template.matches(TOKEN).count() {
            return Err("invalid provisional path template".into());
        }
        if template.replace(TOKEN, &old.to_string()) != p {
            return Err("provisional path template does not match source".into());
        }
        if !version_spans(p.as_bytes(), old).is_empty() && !template.contains(TOKEN) {
            return Err(format!("undeclared version-bearing filename: {p}"));
        }
        let dest = template.replace(TOKEN, &(latest + 1).to_string());
        path(&dest)?;
        if !declared_paths.insert(p.to_string())
            || !destinations.insert(dest.clone())
            || !all_changed.contains(p)
        {
            return Err("duplicate, unchanged, or colliding provisional file".into());
        }
        if guard::STORE_ROOTS
            .iter()
            .any(|r| p.starts_with(&format!("{r}/")))
            && !guard::STORE_ROOTS
                .iter()
                .any(|r| p == format!("{r}/mod.rs"))
            && guard::migration_file_version(p)?.is_none()
            && !guard::tracked_source_paths(Some(&sm)).contains(p)
        {
            return Err(format!(
                "migration helper or rewind source is not pinned: {p}"
            ));
        }
        let original = show(repo, source, p)?
            .ok_or_else(|| format!("source blob digest mismatch or binary file: {p}"))?;
        if original.len() > declaration::MAX_SOURCE_BLOB_BYTES
            || original.contains(&0)
            || guard::digest(&original) != string(f, "source_blob")?
        {
            return Err(format!("source blob digest mismatch or binary file: {p}"));
        }
        let introduced = introduced_ranges(repo, base, p, &original)?;
        let mut covered = vec![false; original.len()];
        for site in list(f, "sites")? {
            let before = string(site, "anchor")?;
            let replacement = string(site, "replacement")?;
            if before.is_empty()
                || !replacement.contains(TOKEN)
                || replacement.replace(TOKEN, &old.to_string()) != before
            {
                return Err("site replacement must be a pure version substitution".into());
            }
            let locations = occurrences(&original, before.as_bytes());
            if locations.len() != 1 || version_spans(before.as_bytes(), old).is_empty() {
                return Err(format!(
                    "site anchor is missing, duplicate, or unversioned: {p}"
                ));
            }
            let start = locations[0];
            let end = start + before.len();
            if !introduced.iter().any(|r| r.start < end && start < r.end) {
                return Err(format!("declared site on an unchanged base line: {p}"));
            }
            if covered[start..end].iter().any(|v| *v) {
                return Err("overlapping provisional site anchors".into());
            }
            covered[start..end].fill(true);
            replacements.push(json!({"path":p,"target_path":dest,"before":before,"after":replacement.replace(TOKEN,&(latest+1).to_string()),"scope":site["scope"],"source_blob":guard::digest(&original),"before_offset":start}));
        }
        for span in version_spans(&original, old) {
            if !covered[span.clone()].iter().all(|v| *v)
                && in_ranges(&introduced, span.start, span.end)
            {
                return Err(format!(
                    "undeclared version-bearing site: {p}:{}",
                    span.start
                ));
            }
        }
        if dest != p && show(repo, source, &dest)?.is_some() {
            return Err(format!("destination already exists in source: {dest}"));
        }
        if dest != p && show(repo, target, &dest)?.is_some() {
            return Err(format!("destination already exists in target: {dest}"));
        }
        if show(repo, base, p)?.is_none() && show(repo, target, &dest)?.is_some() {
            return Err(format!("new source file collides with target: {dest}"));
        }
    }
    for filename in all_changed.difference(&declared_paths) {
        if filename == MANIFEST
            || filename == declaration_path
            || Path::new(filename)
                .file_name()
                .is_some_and(|n| n == "Cargo.lock" || n == "Cargo.toml")
        {
            continue;
        }
        let content = show(repo, source, filename)?.unwrap_or_default();
        let ranges = introduced_ranges(repo, base, filename, &content)?;
        if !version_spans(filename.as_bytes(), old).is_empty() {
            return Err(format!(
                "undeclared version-bearing file or site: {filename}"
            ));
        }
        for span in version_spans(&content, old) {
            if in_ranges(&ranges, span.start, span.end) {
                return Err(format!(
                    "undeclared version-bearing file or site: {filename}:{}",
                    span.start
                ));
            }
        }
    }
    let exact = |p: &str, scope: &str, before: &str| {
        replacements
            .iter()
            .any(|s| s["path"] == p && s["scope"] == scope && s["before"] == before)
    };
    if sm.get("migration_dir").is_some() {
        let unit_file = format!("{MIGRATION_DIR}/v{old:03}.rs");
        let f = declared_files
            .iter()
            .find(|f| f["path"] == unit_file)
            .ok_or_else(|| format!("migration file must be declared: {unit_file}"))?;
        if f["path_template"] != format!("{MIGRATION_DIR}/v${{VERSION}}.rs") {
            return Err(
                "migration file path_template must be vNNN.rs with the version token".into(),
            );
        }
        if !exact(&unit_file, "unit", &format!("if version < {old} {{")) {
            return Err("migration gate needs an exact unit site".into());
        }
    } else {
        if !declared_paths.contains(STORE) {
            return Err("schema constant and migration gate must be declared".into());
        }
        if !exact(
            STORE,
            "head",
            &format!("pub const LATEST_SCHEMA_VERSION: i32 = {old};"),
        ) || !exact(STORE, "unit", &format!("if version < {old} {{"))
        {
            return Err("schema constant and migration gate need exact head/unit sites".into());
        }
    }
    let mut unit = json!({"base":base,"source":source,"target":target,"declaration":declaration_path,"old_version":old,"new_version":latest+1,"source_manifest":sm,"target_manifest":tm,"files":declared_files,"sites":replacements});
    if unit["source_manifest"].get("migration_dir").is_none() {
        unit["base_latest"] = json!(prior);
    }
    Ok(Some(unit))
}

const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "rsi rolling landing"),
    ("GIT_AUTHOR_EMAIL", "rsi-rolling-land@rsi.invalid"),
    ("GIT_COMMITTER_NAME", "rsi rolling landing"),
    ("GIT_COMMITTER_EMAIL", "rsi-rolling-land@rsi.invalid"),
];
fn replace_once(data: &[u8], before: &[u8], after: &[u8]) -> Result<Vec<u8>> {
    let positions = occurrences(data, before);
    if positions.len() != 1 {
        return Err("source site changed during transform".into());
    }
    let start = positions[0];
    let mut out = Vec::with_capacity(data.len() + after.len());
    out.extend_from_slice(&data[..start]);
    out.extend_from_slice(after);
    out.extend_from_slice(&data[start + before.len()..]);
    Ok(out)
}
fn preview_transformed_manifest(repo: &Path, unit: &Value) -> Result<Value> {
    let version = int(unit, "new_version")?;
    let source = string(unit, "source")?;
    let entries = list(unit, "files")?;
    let mut files = guard::Files::new();
    for name in protected_paths(repo, source, &unit["source_manifest"])? {
        let mut original = show(repo, source, &name)?
            .ok_or_else(|| format!("protected source disappeared: {name}"))?;
        let mut dest = name.clone();
        if let Some(f) = entries.iter().find(|f| f["path"] == name) {
            if guard::digest(&original) != string(f, "source_blob")? {
                return Err(format!("source blob changed: {name}"));
            }
            for site in list(f, "sites")? {
                original = replace_once(
                    &original,
                    string(site, "anchor")?.as_bytes(),
                    string(site, "replacement")?
                        .replace(TOKEN, &version.to_string())
                        .as_bytes(),
                )
                .map_err(|_| format!("source site changed: {name}"))?;
            }
            dest = string(f, "path_template")?.replace(TOKEN, &version.to_string());
        }
        files.insert(
            dest,
            String::from_utf8(original)
                .map_err(|e| format!("cannot recompute transformed inventory: {e}"))?,
        );
    }
    guard::inventory(&files).map_err(|e| format!("cannot recompute transformed inventory: {e}"))
}
fn index_update(
    repo: &Path,
    env: &[(&str, &str)],
    name: &str,
    data: &[u8],
    mode: &str,
) -> Result<()> {
    let blob = String::from_utf8(run(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(data),
        &[],
    )?)
    .map_err(|e| e.to_string())?;
    run(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{},{name}", blob.trim()),
        ],
        None,
        env,
    )?;
    Ok(())
}
pub fn transform(repo: &Path, unit: &Value) -> Result<Value> {
    let source = string(unit, "source")?;
    let version = int(unit, "new_version")?;
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let index = temp.path().join("index");
    let index = index.to_str().ok_or("non-UTF8 private index path")?;
    let env = [("GIT_INDEX_FILE", index)];
    run(repo, &["read-tree", source], None, &env)?;
    let mut paths = serde_json::Map::new();
    for f in list(unit, "files")? {
        let old = string(f, "path")?;
        let new = string(f, "path_template")?.replace(TOKEN, &version.to_string());
        let original = show(repo, source, old)?
            .ok_or_else(|| format!("source blob changed during transform: {old}"))?;
        if guard::digest(&original) != string(f, "source_blob")? {
            return Err(format!("source blob changed during transform: {old}"));
        }
        let mut converted = original;
        for site in list(f, "sites")? {
            converted = replace_once(
                &converted,
                string(site, "anchor")?.as_bytes(),
                string(site, "replacement")?
                    .replace(TOKEN, &version.to_string())
                    .as_bytes(),
            )
            .map_err(|_| format!("source site changed during transform: {old}"))?;
        }
        let entry = text_run(repo, &["ls-tree", source, "--", old])?;
        let mode = entry.split(' ').next().unwrap_or_default();
        if !matches!(mode, "100644" | "100755") {
            return Err(format!("unsupported source file mode: {old}"));
        }
        if new != old {
            run(
                repo,
                &["update-index", "--force-remove", "--", old],
                None,
                &env,
            )?;
        }
        index_update(repo, &env, &new, &converted, mode)?;
        paths.insert(old.into(), json!(new));
    }
    let manifest = preview_transformed_manifest(repo, unit)?;
    index_update(
        repo,
        &env,
        MANIFEST,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?
        )
        .as_bytes(),
        "100644",
    )?;
    let tree =
        String::from_utf8(run(repo, &["write-tree"], None, &env)?).map_err(|e| e.to_string())?;
    let tree = tree.trim();
    let transformed = String::from_utf8(run(
        repo,
        &["commit-tree", tree, "-p", source],
        Some(b"rsi landing: provisional migration transform\n"),
        &IDENTITY,
    )?)
    .map_err(|e| e.to_string())?;
    Ok(
        json!({"transformed_source":transformed.trim(),"paths":paths,"transformed_manifest":manifest,"transformed_tree":tree}),
    )
}

fn attribute_lines_start(data: &[u8], start: usize) -> usize {
    let mut line_start = data[..start]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |p| p + 1);
    if !data[line_start..start].iter().all(u8::is_ascii_whitespace) {
        return start;
    }
    while line_start > 0 {
        let previous = data[..line_start - 1]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |p| p + 1);
        let content = &data[previous..line_start];
        let first = content
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(content.len());
        if !content[first..].starts_with(b"#[") {
            break;
        }
        line_start = previous;
    }
    line_start
}
fn teardown_blocks(data: &[u8]) -> Result<BTreeMap<i64, std::ops::Range<usize>>> {
    let header = Re::new(r"^    if active >= (\d+) && target < (\d+) \{\n\z");
    let lines: Vec<_> = data.split_inclusive(|b| *b == b'\n').collect();
    let mut blocks = BTreeMap::new();
    let mut index = 0;
    let mut offset = 0;
    while index < lines.len() {
        let line = std::str::from_utf8(lines[index]).map_err(|e| e.to_string())?;
        let Some(c) = header.search(line) else {
            offset += lines[index].len();
            index += 1;
            continue;
        };
        if group(&c, 1) != group(&c, 2) {
            return Err("rewind teardown header names two versions".into());
        }
        let version = group(&c, 1).parse::<i64>().map_err(|e| e.to_string())?;
        let end = (index + 1..lines.len())
            .find(|&i| lines[i] == b"    }\n")
            .ok_or("rewind teardown block is not closed")?;
        let size = lines[index..=end].iter().map(|l| l.len()).sum::<usize>();
        if blocks.insert(version, offset..offset + size).is_some() {
            return Err(format!("duplicate rewind teardown block V{version}"));
        }
        offset += size;
        index = end + 1;
    }
    Ok(blocks)
}
fn ordered_teardown_insertions(ours: &[&[u8]], theirs: &[&[u8]]) -> Result<Option<Vec<u8>>> {
    let mut parsed = BTreeMap::new();
    for side in [ours, theirs] {
        let data = side.concat();
        let blocks = teardown_blocks(&data)?;
        if blocks.is_empty() || blocks.values().map(|r| r.len()).sum::<usize>() != data.len() {
            return Ok(None);
        }
        for (v, span) in blocks {
            if parsed.insert(v, data[span].to_vec()).is_some() {
                return Ok(None);
            }
        }
    }
    Ok(Some(
        parsed.into_iter().rev().flat_map(|(_, b)| b).collect(),
    ))
}
fn top_level_statements(data: &[u8]) -> Option<Vec<&[u8]>> {
    let mut statements = Vec::new();
    let (mut depth, mut quoted, mut begin, mut i) = (0, false, 0, 0);
    while i < data.len() {
        let b = data[i];
        if quoted {
            if b == b'\\' {
                i += 1;
            } else if b == b'"' {
                quoted = false;
            }
        } else {
            match b {
                b'"' => quoted = true,
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth < 0 {
                        return None;
                    }
                }
                b';' if depth == 0 => {
                    statements.push(&data[begin..i]);
                    begin = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if depth != 0 || quoted || !data[begin..].iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        Some(statements)
    }
}
fn whole_statements(data: &[u8]) -> bool {
    if !data.starts_with(b"    ") || data.starts_with(b"     ") || !data.ends_with(b"\n") {
        return false;
    }
    let last = data.iter().rfind(|b| !b.is_ascii_whitespace());
    if !matches!(last, Some(b';' | b'}')) {
        return false;
    }
    let (mut depth, mut quoted, mut i) = (0, false, 0);
    while i < data.len() {
        let b = data[i];
        if quoted {
            if b == b'\\' {
                i += 1;
            } else if b == b'"' {
                quoted = false;
            }
        } else {
            match b {
                b'"' => quoted = true,
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    depth == 0 && !quoted
}
fn fixture_insertion_declared(side: &[&[u8]], versions: &BTreeSet<i64>) -> bool {
    let data = side.concat();
    if data.is_empty() || !whole_statements(&data) {
        return false;
    }
    let Some(statements) = top_level_statements(&data).filter(|s| !s.is_empty()) else {
        return false;
    };
    let probe = Re::new(
        r#"\A\s*let\s+([a-z_][a-z0-9_]*)(?:\s*:\s*[A-Za-z0-9_<>]+)?\s*=\s*connection\s*\.query_row\(\s*"\s*SELECT\b[^"\\]*"\s*,\s*\[\]\s*,\s*\|row\|\s*row\.get(?:::<[^<>;]*>)?\(0\)\s*,?\s*\)\s*(?:\.optional\(\)\s*)?\.unwrap\(\)\s*\z"#,
    );
    let assertion = Re::new(
        r#"\A\s*assert_eq!\(\s*([A-Za-z_][A-Za-z0-9_]*)(?:\.contains\("[^"\\]*"\))?\s*,\s*(?:target_version\s*>=\s*(\d+)|if\s+target_version\s*>=\s*(\d+)\s*\{\s*\d+\s*\}\s*else\s*\{\s*\d+\s*\})\s*,\s*"fixture V\{target_version\}: [^"\\]*"\s*,?\s*\)\s*\z"#,
    );
    let mut pending: Option<String> = None;
    for s in statements {
        let Ok(s) = std::str::from_utf8(s) else {
            return false;
        };
        if let Some(c) = probe.search(s) {
            if pending.is_some() {
                return false;
            }
            pending = Some(group(&c, 1).into());
            continue;
        }
        let Some(c) = assertion.search(s) else {
            return false;
        };
        let version = if group(&c, 2).is_empty() {
            group(&c, 3)
        } else {
            group(&c, 2)
        }
        .parse::<i64>()
        .ok();
        if pending.as_deref() != Some(group(&c, 1))
            || !version.is_some_and(|n| versions.contains(&n))
        {
            return false;
        }
        pending = None;
    }
    pending.is_none()
}
fn mark_site(
    mask: &mut [bool],
    data: &[u8],
    value: &[u8],
    message: &str,
    attributes: bool,
) -> Result<()> {
    let found = occurrences(data, value);
    if found.len() != 1 {
        return Err(message.into());
    }
    let mut start = found[0];
    let mut end = start + value.len();
    if data.get(end) == Some(&b'\n') {
        end += 1;
    }
    if attributes {
        start = attribute_lines_start(data, start);
    }
    mask[start..end].fill(true);
    Ok(())
}
fn allowed_mask(unit: &Value, filename: &str, data: &[u8]) -> Result<Vec<bool>> {
    let mut mask = vec![false; data.len()];
    for site in list(unit, "sites")? {
        if site["target_path"] == filename {
            mark_site(
                &mut mask,
                data,
                string(site, "after")?.as_bytes(),
                &format!("transformed site is ambiguous: {filename}"),
                true,
            )?;
        }
    }
    if guard::migration_file_version(filename)?.is_some() {
        mask.fill(true);
    }
    if filename == STORE {
        let blocks = guard::migration_blocks(&guard::Files::from([(
            STORE.into(),
            String::from_utf8(data.to_vec()).map_err(|e| e.to_string())?,
        )]))?;
        let block = blocks
            .get(&int(unit, "new_version")?)
            .ok_or("transformed migration gate is missing or ambiguous")?;
        let found = occurrences(data, block.as_bytes());
        if found.len() != 1 {
            return Err("transformed migration gate is missing or ambiguous".into());
        }
        mask[found[0]..found[0] + block.len()].fill(true);
    }
    if filename == STORE_TESTS {
        if let Some(span) = teardown_blocks(data)?.get(&int(unit, "new_version")?) {
            mask[span.clone()].fill(true);
        }
    }
    Ok(mask)
}
fn prior_refusal(source: &str, error: &str, declarations: &[String]) -> String {
    format!(
        "prior provisional source {source}{} is the owner of this refusal, not the source being landed: {error}",
        if declarations.is_empty() {
            String::new()
        } else {
            format!(" ({})", declarations.join(", "))
        }
    )
}
fn target_tail(unit: &Value) -> Result<std::ops::RangeInclusive<i64>> {
    let prior = unit["base_latest"]
        .as_i64()
        .unwrap_or(int(&unit["source_manifest"], "latest_schema_version")? - 1);
    Ok(prior + 1..=int(&unit["target_manifest"], "latest_schema_version")?)
}
fn target_migration_mask(
    repo: &Path,
    unit: &mut Value,
    filename: &str,
    data: &[u8],
) -> Result<Vec<bool>> {
    let mut mask = vec![false; data.len()];
    let tail = target_tail(unit)?;
    if tail.is_empty() {
        return Ok(mask);
    }
    if guard::migration_file_version(filename)?.is_some_and(|n| tail.contains(&n)) {
        mask.fill(true);
    }
    if filename == STORE {
        let constant = format!(
            "pub const LATEST_SCHEMA_VERSION: i32 = {};",
            int(&unit["target_manifest"], "latest_schema_version")?
        );
        // The legacy constant owns its exact span, without swallowing neighbours.
        let positions = occurrences(data, constant.as_bytes());
        if positions.len() != 1 {
            return Err("target schema constant is ambiguous".into());
        }
        mask[positions[0]..positions[0] + constant.len()].fill(true);
        let blocks = guard::migration_blocks(&guard::Files::from([(
            STORE.into(),
            String::from_utf8(data.to_vec()).map_err(|e| e.to_string())?,
        )]))?;
        for n in tail.clone() {
            let message = format!("target migration block V{n} is ambiguous");
            let block = blocks.get(&n).ok_or_else(|| message.clone())?;
            mark_site(&mut mask, data, block.as_bytes(), &message, false)?;
        }
    }
    if filename == STORE_TESTS {
        for (n, span) in teardown_blocks(data)? {
            if tail.contains(&n) {
                mask[span].fill(true);
            }
        }
    }
    let prior_units = unit
        .get("prior_units")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let priors = prior_units
        .as_array()
        .filter(|v| v.len() <= 64)
        .ok_or("invalid prior provisional proof list")?;
    let mut by_candidate = BTreeMap::new();
    for p in priors {
        by_candidate.insert(string(p, "unit_candidate")?.to_string(), p.clone());
    }
    if unit.get("_verified_target_units").is_none() {
        unit["_verified_target_units"] = json!(discover_target_provisional_units(
            repo,
            string(unit, "base")?,
            string(unit, "target")?
        )?);
    }
    for p in list(unit, "_verified_target_units")? {
        by_candidate
            .entry(string(p, "unit_candidate")?.to_string())
            .or_insert_with(|| p.clone());
    }
    for prior in by_candidate.values() {
        if prior.as_object().is_none_or(|m| {
            m.len() != 4
                || !["base", "source", "target", "unit_candidate"]
                    .iter()
                    .all(|k| m.contains_key(*k))
        }) {
            return Err("invalid prior provisional proof binding".into());
        }
        let source = string(prior, "source")?;
        let result = || -> Result<Value> {
            let previous = inspect(
                repo,
                string(prior, "base")?,
                source,
                string(prior, "target")?,
                false,
            )?
            .ok_or("prior provisional source is not a migration")?;
            prove(
                repo,
                &previous,
                string(prior, "unit_candidate")?,
                string(unit, "target")?,
            )
        };
        let proof = result().map_err(|e| prior_refusal(source, &e, &[]))?;
        for site in list(&proof, "sites")? {
            if site["target_path"] == filename {
                mark_site(
                    &mut mask,
                    data,
                    string(site, "after")?.as_bytes(),
                    &format!("prior proved site is ambiguous: {filename}"),
                    true,
                )?;
            }
        }
    }
    Ok(mask)
}
fn discover_target_provisional_units(repo: &Path, base: &str, target: &str) -> Result<Vec<Value>> {
    if !ancestor(repo, base, target)? {
        return Err("accepted base is outside target ancestry".into());
    }
    let history = text_run(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H %P",
            &format!("{base}..{target}"),
        ],
    )?;
    let history: Vec<_> = history.lines().collect();
    if history.len() > 4096 {
        return Err("target ancestry exceeds provisional proof search bound".into());
    }
    let mut units = Vec::new();
    for line in history.into_iter().rev() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() != 3 {
            continue;
        }
        let (candidate, parent, source) = (parts[0], parts[1], parts[2]);
        let source_base = text_run(repo, &["merge-base", parent, source])?;
        let mut declarations = Vec::new();
        for name in changed(repo, &source_base, source)? {
            if name.starts_with(DECLARATIONS) && name.ends_with(".json") {
                let parent_blob = show(repo, parent, &name)?;
                if parent_blob.is_some() && parent_blob == show(repo, source, &name)? {
                    eprintln!("skipping inherited landed declaration {name} from {source}");
                } else {
                    declarations.push(name);
                }
            }
        }
        if declarations.is_empty() {
            continue;
        }
        let mut malformed = None;
        for name in &declarations {
            if let Some(raw) = show(repo, source, name)? {
                let result = serde_json::from_slice::<Value>(&raw)
                    .map_err(|e| e.to_string())
                    .and_then(|d| guard::validate_declaration_shape(&d, name, None));
                if let Err(e) = result {
                    malformed = Some(e);
                    break;
                }
            }
        }
        if let Some(error) = malformed {
            eprintln!("skipping malformed landed declaration from {source}: {error}");
            continue;
        }
        let result = || -> Result<()> {
            let previous = inspect(repo, &source_base, source, parent, false)?
                .ok_or("target provisional declaration has no appended migration")?;
            prove(repo, &previous, candidate, target)?;
            Ok(())
        };
        if let Err(error) = result() {
            if error.starts_with("undeclared version-bearing") {
                eprintln!("skipping unprovable landed declaration from {source}: {error}");
                continue;
            }
            return Err(prior_refusal(source, &error, &declarations));
        }
        units.push(
            json!({"base":source_base,"source":source,"target":parent,"unit_candidate":candidate}),
        );
        if units.len() > 64 {
            return Err("target provisional proof count exceeds bound".into());
        }
    }
    Ok(units)
}

/// SequenceMatcher's no-junk, leftmost-longest matching blocks. Using the same
/// tie order matters: a different diff algorithm can authorize different lines.
fn matching_blocks(a: &[&[u8]], b: &[&[u8]]) -> Vec<(usize, usize, usize)> {
    let mut positions: BTreeMap<&[u8], Vec<usize>> = BTreeMap::new();
    for (j, line) in b.iter().enumerate() {
        positions.entry(*line).or_default().push(j);
    }
    let mut queue = vec![(0, a.len(), 0, b.len())];
    let mut blocks = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (mut best_i, mut best_j, mut best_size) = (alo, blo, 0);
        let mut lengths = BTreeMap::new();
        for (i, line) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut new_lengths = BTreeMap::new();
            if let Some(js) = positions.get(line) {
                for &j in js.iter().filter(|j| blo <= **j && **j < bhi) {
                    let size = if j == 0 {
                        1
                    } else {
                        lengths.get(&(j - 1)).copied().unwrap_or(0) + 1
                    };
                    new_lengths.insert(j, size);
                    if size > best_size {
                        best_i = i + 1 - size;
                        best_j = j + 1 - size;
                        best_size = size;
                    }
                }
            }
            lengths = new_lengths;
        }
        if best_size > 0 {
            blocks.push((best_i, best_j, best_size));
            if alo < best_i && blo < best_j {
                queue.push((alo, best_i, blo, best_j));
            }
            if best_i + best_size < ahi && best_j + best_size < bhi {
                queue.push((best_i + best_size, ahi, best_j + best_size, bhi));
            }
        }
    }
    blocks.sort_unstable();
    let mut merged: Vec<(usize, usize, usize)> = Vec::new();
    for (i, j, n) in blocks {
        if let Some(last) = merged.last_mut() {
            if last.0 + last.2 == i && last.1 + last.2 == j {
                last.2 += n;
                continue;
            }
        }
        merged.push((i, j, n));
    }
    merged.push((a.len(), b.len(), 0));
    merged
}
fn changed_lines_are_declared(
    base: &[&[u8]],
    theirs: &[&[u8]],
    source: &[u8],
    mask: &[bool],
) -> bool {
    let chunk = theirs.concat();
    if chunk.is_empty() {
        return false;
    }
    let found = occurrences(source, &chunk);
    if found.len() != 1 {
        return false;
    }
    let mut offsets = vec![found[0]];
    for l in theirs {
        offsets.push(offsets.last().copied().unwrap_or_default() + l.len());
    }
    let mut previous = 0;
    for (_, j, n) in matching_blocks(base, theirs) {
        for k in previous..j {
            if !theirs[k].iter().all(u8::is_ascii_whitespace)
                && !mask[offsets[k]..offsets[k + 1]].iter().all(|b| *b)
            {
                return false;
            }
        }
        previous = j + n;
    }
    true
}
fn resolve_diff3(
    repo: &Path,
    unit: &mut Value,
    filename: &str,
    contents: &[u8],
) -> Result<Vec<u8>> {
    let transformed = show(repo, string(unit, "transformed_source")?, filename)?
        .ok_or_else(|| format!("conflicted path missing in transformed source: {filename}"))?;
    let target = show(repo, string(unit, "target")?, filename)?
        .ok_or_else(|| format!("conflicted path missing in transformed source: {filename}"))?;
    let mask = allowed_mask(unit, filename, &transformed)?;
    let target_mask = target_migration_mask(repo, unit, filename, &target)?;
    let lines: Vec<_> = contents.split_inclusive(|b| *b == b'\n').collect();
    let mut output = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with(b"<<<<<<< ") {
            output.extend_from_slice(lines[i]);
            i += 1;
            continue;
        }
        i += 1;
        let start = i;
        while i < lines.len() && !lines[i].starts_with(b"||||||| ") {
            i += 1;
        }
        if i == lines.len() {
            return Err(format!("conflict lacks diff3 base: {filename}"));
        }
        let ours = &lines[start..i];
        i += 1;
        let start = i;
        while i < lines.len() && lines[i] != b"=======\n" {
            i += 1;
        }
        if i == lines.len() {
            return Err(format!("malformed diff3 conflict: {filename}"));
        }
        let base = &lines[start..i];
        i += 1;
        let start = i;
        while i < lines.len() && !lines[i].starts_with(b">>>>>>> ") {
            i += 1;
        }
        if i == lines.len() {
            return Err(format!("unproved migration conflict: {filename}"));
        }
        let theirs = &lines[start..i];
        let fixture = filename == STORE_TESTS && base.is_empty();
        if !(changed_lines_are_declared(base, theirs, &transformed, &mask)
            || (fixture
                && fixture_insertion_declared(
                    theirs,
                    &BTreeSet::from([int(unit, "new_version")?]),
                )))
            || !(changed_lines_are_declared(base, ours, &target, &target_mask)
                || (fixture && fixture_insertion_declared(ours, &target_tail(unit)?.collect())))
        {
            return Err(format!("unproved migration conflict: {filename}"));
        }
        i += 1;
        if base.is_empty() {
            if ours == theirs {
                output.extend(ours.concat());
            } else if filename == STORE_TESTS {
                if let Some(ordered) = ordered_teardown_insertions(ours, theirs)? {
                    output.extend(ordered);
                } else {
                    output.extend(ours.concat());
                    output.extend(theirs.concat());
                }
            } else {
                output.extend(ours.concat());
                output.extend(theirs.concat());
            }
            continue;
        }
        if base.len() != 1 || ours.is_empty() || theirs.is_empty() {
            return Err(format!("semantic replacement conflict: {filename}"));
        }
        let head_sites: Vec<_> = list(unit, "sites")?
            .iter()
            .filter(|s| {
                s["target_path"] == filename
                    && s["scope"] == "head"
                    && s["after"]
                        .as_str()
                        .is_some_and(|a| !occurrences(theirs[0], a.as_bytes()).is_empty())
            })
            .collect();
        if head_sites.len() != 1 {
            return Err(format!("non-head replacement conflict: {filename}"));
        }
        let site = head_sites[0];
        let expected = string(site, "before")?.replace(
            &int(unit, "old_version")?.to_string(),
            &(int(unit, "new_version")? - 1).to_string(),
        );
        if occurrences(ours[0], expected.as_bytes()).is_empty()
            || occurrences(theirs[0], string(site, "after")?.as_bytes()).is_empty()
        {
            return Err(format!("target head site changed semantically: {filename}"));
        }
        output.extend_from_slice(theirs[0]);
        output.extend(ours[1..].concat());
        output.extend(theirs[1..].concat());
    }
    Ok(output)
}
fn safe_destination(root: &Path, name: &str, message: &str) -> Result<std::path::PathBuf> {
    path(name)?;
    let dest = root.join(name);
    let base = root.canonicalize().map_err(|e| e.to_string())?;
    let resolved = if dest.exists() {
        dest.canonicalize()
    } else {
        dest.parent()
            .ok_or(message)?
            .canonicalize()
            .map(|p| p.join(dest.file_name().unwrap_or_default()))
    }
    .map_err(|e| e.to_string())?;
    if dest.is_symlink() || !resolved.starts_with(base) {
        return Err(message.into());
    }
    Ok(dest)
}
pub fn resolve(repo: &Path, worktree: &Path, unit: &mut Value) -> Result<Value> {
    let unmerged = run(
        worktree,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
        None,
        &[],
    )?;
    let paths: Vec<_> = unmerged
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|b| String::from_utf8(b.to_vec()).map_err(|e| e.to_string()))
        .collect::<Result<_>>()?;
    let mut allowed = BTreeSet::from([MANIFEST.to_string()]);
    for site in list(unit, "sites")? {
        allowed.insert(string(site, "target_path")?.into());
    }
    if paths.iter().any(|p| !allowed.contains(p)) {
        return Err(format!("nonmigration conflict: {}", paths.join(", ")));
    }
    for p in &paths {
        if p == MANIFEST {
            continue;
        }
        let dest = safe_destination(
            worktree,
            p,
            &format!("conflict path escapes private candidate: {p}"),
        )?;
        let content = std::fs::read(&dest).map_err(|e| e.to_string())?;
        std::fs::write(dest, resolve_diff3(repo, unit, p, &content)?).map_err(|e| e.to_string())?;
        run(worktree, &["add", "--", p], None, &[])?;
    }
    let mut protected = guard::tracked_source_paths(Some(&unit["target_manifest"]));
    protected.extend(guard::tracked_source_paths(Some(
        &unit["transformed_manifest"],
    )));
    let dir = worktree.join(MIGRATION_DIR);
    if dir.is_dir()
        && !dir.is_symlink()
        && dir
            .canonicalize()
            .map_err(|e| e.to_string())?
            .starts_with(worktree.canonicalize().map_err(|e| e.to_string())?)
    {
        for e in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let name = format!("{MIGRATION_DIR}/{}", e.file_name().to_string_lossy());
            if e.path().is_file() && guard::migration_file_version(&name)?.is_some() {
                protected.insert(name);
            }
        }
    }
    let mut files = guard::Files::new();
    for p in protected {
        let dest = safe_destination(
            worktree,
            &p,
            &format!("protected path escapes private candidate: {p}"),
        )?;
        files.insert(
            p,
            std::fs::read_to_string(dest)
                .map_err(|e| format!("cannot derive merged migration inventory: {e}"))?
                .replace("\r\n", "\n"),
        );
    }
    let manifest = guard::inventory(&files)
        .map_err(|e| format!("cannot derive merged migration inventory: {e}"))?;
    let pin = safe_destination(
        worktree,
        MANIFEST,
        "migration inventory path escapes private candidate",
    )?;
    std::fs::write(
        pin,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?
        ),
    )
    .map_err(|e| e.to_string())?;
    run(worktree, &["add", "--", MANIFEST], None, &[])?;
    if !run(
        worktree,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
        None,
        &[],
    )?
    .is_empty()
    {
        return Err("unmerged paths remain after provisional resolution".into());
    }
    run(worktree, &["diff", "--cached", "--check"], None, &[])?;
    Ok(json!({"resolved_paths":paths,"candidate_manifest":manifest}))
}
pub fn build_candidate(repo: &Path, unit: &mut Value, scratch: &Path) -> Result<Value> {
    let parent = repo
        .parent()
        .ok_or("private repository has no parent")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !scratch.is_dir()
        || !scratch
            .canonicalize()
            .map_err(|e| e.to_string())?
            .starts_with(parent)
    {
        return Err("provisional scratch must be inside the private landing workspace".into());
    }
    let temporary = tempfile::Builder::new()
        .prefix("rsi-migration-build-")
        .tempdir_in(scratch)
        .map_err(|e| e.to_string())?;
    let worktree = temporary.path().join("tree");
    let wt = worktree.to_str().ok_or("non-UTF8 private worktree")?;
    run(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            wt,
            string(unit, "target")?,
        ],
        None,
        &[],
    )?;
    let result = (|| -> Result<Value> {
        let merged = execute(
            &worktree,
            &[
                "-c",
                "merge.conflictStyle=diff3",
                "merge",
                "--no-ff",
                "--no-commit",
                "--no-stat",
                "-q",
                string(unit, "transformed_source")?,
            ],
            None,
            &IDENTITY,
        )?;
        if !merged.status.success()
            && run(
                &worktree,
                &["diff", "--name-only", "--diff-filter=U", "-z"],
                None,
                &[],
            )?
            .is_empty()
        {
            return Err(format!(
                "provisional merge refused: {}",
                String::from_utf8_lossy(&merged.stderr).trim()
            ));
        }
        let resolution = resolve(repo, &worktree, unit)?;
        let tree = text_run(&worktree, &["write-tree"])?;
        let candidate = String::from_utf8(run(
            repo,
            &[
                "commit-tree",
                &tree,
                "-p",
                string(unit, "target")?,
                "-p",
                string(unit, "source")?,
                "-m",
                "rsi landing: renumber provisional migration",
            ],
            None,
            &IDENTITY,
        )?)
        .map_err(|e| e.to_string())?;
        let candidate = candidate.trim();
        prove(repo, unit, candidate, candidate)?;
        Ok(json!({"candidate":candidate,"tree":tree,"resolved_paths":resolution["resolved_paths"]}))
    })();
    let cleanup = execute(repo, &["worktree", "remove", "--force", wt], None, &[])?;
    if !cleanup.status.success() {
        return Err(format!(
            "provisional worktree cleanup failed: {}; build_error={}",
            String::from_utf8_lossy(&cleanup.stderr).trim(),
            result.as_ref().err().map_or("None", String::as_str)
        ));
    }
    result
}
pub fn prove(
    repo: &Path,
    unit: &Value,
    unit_candidate: &str,
    final_candidate: &str,
) -> Result<Value> {
    oid(unit_candidate, "commit")?;
    oid(final_candidate, "commit")?;
    let parents = text_run(repo, &["show", "-s", "--format=%P", unit_candidate])?;
    if parents.split_whitespace().collect::<Vec<_>>()
        != [string(unit, "target")?, string(unit, "source")?]
    {
        return Err("provisional candidate lacks exact (target, source) parents".into());
    }
    if !ancestor(repo, unit_candidate, final_candidate)? {
        return Err("provisional candidate is not in final candidate ancestry".into());
    }
    let fm = revision_inventory(repo, final_candidate, None)?;
    guard::validate_append_only(&unit["target_manifest"], &fm, &[])
        .map_err(|e| format!("final candidate changed released prefix: {e}"))?;
    let version = int(unit, "new_version")?;
    let transformed = preview_transformed_manifest(repo, unit)?;
    if fm["blocks"].get(version.to_string()) != transformed["blocks"].get(version.to_string()) {
        return Err("provisional migration block changed after transform".into());
    }
    for (name, entry) in transformed["protected_sections"]
        .as_object()
        .ok_or("invalid transformed protected sections")?
    {
        if unit["target_manifest"]["protected_sections"]
            .get(name)
            .is_none()
            && fm["protected_sections"].get(name) != Some(entry)
        {
            return Err(format!("provisional protected section changed: {name}"));
        }
    }
    let mut sites = Vec::new();
    for site in list(unit, "sites")? {
        let p = string(site, "path")?;
        let dest = string(site, "target_path")?;
        let missing = || format!("proof blob missing or changed: {p}");
        let original = show(repo, string(unit, "source")?, p)?.ok_or_else(missing)?;
        let candidate = show(repo, final_candidate, dest)?.ok_or_else(missing)?;
        let target = show(repo, string(unit, "target")?, dest)?;
        if guard::digest(&original) != string(site, "source_blob")? {
            return Err(missing());
        }
        let file = list(unit, "files")?
            .iter()
            .find(|f| f["path"] == p)
            .ok_or("proof file missing")?;
        let file_site = list(file, "sites")?
            .iter()
            .find(|s| s["anchor"] == site["before"])
            .ok_or("proof site missing")?;
        let expected = string(file_site, "replacement")?.replace(
            TOKEN,
            &if site["scope"] == "head" {
                int(&fm, "latest_schema_version")?
            } else {
                version
            }
            .to_string(),
        );
        let before = occurrences(&original, string(site, "before")?.as_bytes());
        let after = occurrences(&candidate, expected.as_bytes());
        if before.len() != 1 || after.len() != 1 {
            return Err(format!("proved span missing or ambiguous: {dest}"));
        }
        sites.push(json!({"path":p,"target_path":dest,"scope":site["scope"],"before":site["before"],"after":expected,"before_offset":before[0],"after_offset":after[0],"source_blob":guard::digest(&original),"target_blob":target.as_ref().map(guard::digest),"candidate_blob":guard::digest(&candidate)}));
    }
    let manifest_digest = |rev: &str| -> Result<String> {
        Ok(guard::digest(
            show(repo, rev, MANIFEST)?.ok_or("proof manifest missing")?,
        ))
    };
    Ok(
        json!({"schema_version":1,"base":unit["base"],"source":unit["source"],"target":unit["target"],"unit_candidate":unit_candidate,"candidate":final_candidate,"old_version":unit["old_version"],"assigned_version":version,"final_version":fm["latest_schema_version"],"declaration":unit["declaration"],"source_manifest_blob":manifest_digest(string(unit,"source")?)?,"target_manifest_blob":manifest_digest(string(unit,"target")?)?,"candidate_manifest_blob":manifest_digest(final_candidate)?,"sites":sites}),
    )
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut flags = BTreeSet::new();
    let mut values = BTreeMap::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--transform" | "--resolve" | "--build" | "--prove" => {
                flags.insert(arg.as_str());
            }
            "--scratch" | "--unit-candidate" | "--candidate" | "--unit-file" | "--worktree"
            | "--repo" | "--base" | "--source" | "--target" => {
                let Some(v) = it.next() else {
                    let _ = writeln!(err, "{arg} requires a value");
                    return 2;
                };
                values.insert(arg.as_str(), v.as_str());
            }
            "-h" | "--help" => {
                let _ = writeln!(
                    out,
                    "usage: rolling-migration-renumber --repo PATH [--transform|--resolve|--build|--prove] [--base SHA --source SHA --target SHA] [--unit-file PATH]"
                );
                return 0;
            }
            _ => {
                let _ = writeln!(err, "unrecognized argument: {arg}");
                return 2;
            }
        }
    }
    let process = || -> Result<Value> {
        let repo = ctx.path(values.get("--repo").ok_or("--repo is required")?);
        if flags.contains("--resolve") || flags.contains("--prove") || flags.contains("--build") {
            let file = values
                .get("--unit-file")
                .ok_or(if flags.contains("--resolve") {
                    "--resolve requires --unit-file and --worktree"
                } else {
                    "--unit-file is required"
                })?;
            if flags.contains("--resolve") && !values.contains_key("--worktree") {
                return Err("--resolve requires --unit-file and --worktree".into());
            }
            let mut unit: Value =
                serde_json::from_slice(&std::fs::read(ctx.path(file)).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            let fresh = inspect(
                &repo,
                string(&unit, "base")?,
                string(&unit, "source")?,
                string(&unit, "target")?,
                false,
            )?
            .ok_or("provisional unit changed before conflict resolution")?;
            if fresh
                .as_object()
                .ok_or("invalid fresh unit")?
                .iter()
                .any(|(k, v)| unit.get(k) != Some(v))
            {
                return Err("provisional unit changed before conflict resolution".into());
            }
            if flags.contains("--resolve") {
                resolve(&repo, &ctx.path(values["--worktree"]), &mut unit)
            } else if flags.contains("--build") {
                build_candidate(
                    &repo,
                    &mut unit,
                    &ctx.path(
                        values
                            .get("--scratch")
                            .ok_or("--build requires --scratch")?,
                    ),
                )
            } else {
                prove(
                    &repo,
                    &unit,
                    values
                        .get("--unit-candidate")
                        .ok_or("--prove requires --unit-candidate and --candidate")?,
                    values
                        .get("--candidate")
                        .ok_or("--prove requires --unit-candidate and --candidate")?,
                )
            }
        } else {
            let base = values
                .get("--base")
                .ok_or("--base, --source and --target are required")?;
            let source = values
                .get("--source")
                .ok_or("--base, --source and --target are required")?;
            let target = values
                .get("--target")
                .ok_or("--base, --source and --target are required")?;
            let Some(mut unit) =
                inspect(&repo, base, source, target, flags.contains("--transform"))?
            else {
                if flags.contains("--transform") {
                    return Err("provisional declaration has no appended migration".into());
                }
                return Ok(Value::Null);
            };
            if flags.contains("--transform") && unit["renumber_noop"] != true {
                let transformed = transform(&repo, &unit)?;
                for (k, v) in transformed.as_object().ok_or("invalid transform")? {
                    unit[k] = v.clone();
                }
            }
            Ok(unit)
        }
    };
    match process() {
        Ok(value) => {
            let _ = writeln!(out, "{value}");
            0
        }
        Err(error) => {
            let _ = writeln!(err, "provisional migration refused: {error}");
            2
        }
    }
}
#[cfg(test)]
mod tests;
