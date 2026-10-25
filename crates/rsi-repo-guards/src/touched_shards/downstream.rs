//! Pinned workspace reverse dependency closure, including renamed, workspace,
//! target, dev and build dependencies. Runtime observer selection is not yet
//! integrated; callers must not mistake compile coverage for test execution.
use super::git::{Repository, Texts, changed_line_ranges};
use super::slow::only_test_lines;
use super::source::{code_text, inline_scopes, mount_closure, normalize, rust_functions};
use crate::re::Re;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct Package {
    pub manifest: String,
    pub directory: String,
    pub library: Option<String>,
    pub dependencies: BTreeSet<String>,
    pub data: toml::Value,
}

/// One element of a compiled fnmatch pattern.
enum GlobElement {
    Star,
    Any,
    Class { negate: bool, chars: Vec<u8> },
    Literal(u8),
}

/// Most `pattern weight x (path bytes + 1)` cells one match may fill (#459, #463); an
/// element weighs 1 and a character class weighs its length plus 1.
const GLOB_CELL_BUDGET: usize = 16_000_000;

fn compile_glob(pattern: &[u8]) -> Vec<GlobElement> {
    let mut elements = Vec::new();
    // A `[` closes at the first `]` after it. The last `]` bounds every search at once:
    // a `[` at or beyond it can never close, so it is a literal without rescanning the
    // pattern (one scan per unmatched `[` is quadratic in a run of them, #463).
    let last_close = pattern.iter().rposition(|b| *b == b']');
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            // `**` and `*` match the same strings: collapse a run
            b'*' => {
                if !matches!(elements.last(), Some(GlobElement::Star)) {
                    elements.push(GlobElement::Star);
                }
                i += 1;
            }
            b'?' => {
                elements.push(GlobElement::Any);
                i += 1;
            }
            b'[' => match last_close
                .filter(|last| *last > i)
                .and_then(|_| pattern[i + 1..].iter().position(|b| *b == b']'))
            {
                Some(n) => {
                    let mut chars = &pattern[i + 1..i + 1 + n];
                    let negate = chars.first() == Some(&b'!');
                    if negate {
                        chars = &chars[1..];
                    }
                    elements.push(GlobElement::Class {
                        negate,
                        chars: chars.to_vec(),
                    });
                    i += n + 2;
                }
                None => {
                    elements.push(GlobElement::Literal(b'['));
                    i += 1;
                }
            },
            c => {
                elements.push(GlobElement::Literal(c));
                i += 1;
            }
        }
    }
    elements
}

fn class_hit(chars: &[u8], byte: u8) -> bool {
    let (mut hit, mut i) = (false, 0);
    while i < chars.len() {
        if i + 2 < chars.len() && chars[i + 1] == b'-' {
            hit |= chars[i] <= byte && byte <= chars[i + 2];
            i += 3;
        } else {
            hit |= chars[i] == byte;
            i += 1;
        }
    }
    hit
}

/// fnmatchcase-style workspace globs (a `*` can cross `/`, matching the
/// historical planner, rather than platform-dependent filesystem glob rules).
/// Iterative (no recursion, so no pattern can exhaust the stack) and bounded:
/// a pattern too large to match within `GLOB_CELL_BUDGET` is a typed refusal.
fn glob(pattern: &str, path: &str) -> Result<bool, String> {
    let elements = compile_glob(pattern.trim_end_matches('/').as_bytes());
    let s = path.as_bytes();
    // A class is read byte by byte for every path byte, so it weighs its length.
    let weight = elements.iter().fold(0usize, |sum, element| {
        sum.saturating_add(match element {
            GlobElement::Class { chars, .. } => chars.len().saturating_add(1),
            _ => 1,
        })
    });
    if weight.saturating_mul(s.len() + 1) > GLOB_CELL_BUDGET {
        return Err(format!(
            "workspace member pattern is too large to match (weight {weight} over {} pattern elements x {} path bytes)",
            elements.len(),
            s.len()
        ));
    }
    // `next[j]`: the elements after the current one match `s[j..]`.
    let mut next: Vec<bool> = (0..=s.len()).map(|j| j == s.len()).collect();
    let mut row = vec![false; s.len() + 1];
    for element in elements.iter().rev() {
        for j in (0..=s.len()).rev() {
            row[j] = match element {
                GlobElement::Star => next[j] || (j < s.len() && row[j + 1]),
                GlobElement::Any => j < s.len() && next[j + 1],
                GlobElement::Class { negate, chars } => {
                    j < s.len() && (class_hit(chars, s[j]) != *negate) && next[j + 1]
                }
                GlobElement::Literal(c) => j < s.len() && s[j] == *c && next[j + 1],
            };
        }
        std::mem::swap(&mut next, &mut row);
    }
    Ok(next[0])
}

fn glob_any(patterns: &[&str], path: &str) -> Result<bool, String> {
    for pattern in patterns {
        if glob(pattern, path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn workspace_test_graph(
    texts: &Texts,
    files: &BTreeSet<String>,
) -> Result<BTreeMap<String, Package>, String> {
    let Some(text) = texts.get("Cargo.toml") else {
        return Ok(BTreeMap::new());
    };
    let root: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let workspace = root.get("workspace");
    let strings = |key| {
        workspace
            .and_then(|v| v.get(key))
            .and_then(toml::Value::as_array)
            .map(|v| v.iter().filter_map(toml::Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let members = strings("members");
    let excludes = strings("exclude");
    let mut packages = BTreeMap::new();
    for (manifest, text) in texts
        .iter()
        .filter(|(p, _)| *p == "Cargo.toml" || p.ends_with("/Cargo.toml"))
    {
        let directory = manifest.rsplit_once('/').map_or("", |(p, _)| p);
        if manifest != "Cargo.toml"
            && (!glob_any(&members, directory)? || glob_any(&excludes, directory)?)
        {
            continue;
        }
        let data: toml::Value = toml::from_str(text).map_err(|e| format!("{manifest}: {e}"))?;
        let Some(package) = data.get("package") else {
            continue;
        };
        let name = package
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| format!("{manifest}: package.name missing"))?;
        let library = data
            .get("lib")
            .and_then(|v| v.get("path"))
            .and_then(toml::Value::as_str)
            .unwrap_or("src/lib.rs");
        let library = normalize(format!("{directory}/{library}").trim_start_matches('/'));
        packages.insert(
            name.to_owned(),
            Package {
                manifest: manifest.clone(),
                directory: directory.into(),
                library: files.contains(&library).then_some(library),
                dependencies: BTreeSet::new(),
                data,
            },
        );
    }
    let by_directory: BTreeMap<_, _> = packages
        .iter()
        .map(|(n, p)| {
            (
                if p.directory.is_empty() {
                    "."
                } else {
                    &p.directory
                }
                .to_owned(),
                n.clone(),
            )
        })
        .collect();
    for package in packages.values_mut() {
        let mut sections = vec![&package.data];
        if let Some(targets) = package.data.get("target").and_then(toml::Value::as_table) {
            sections.extend(targets.values());
        }
        for section in sections {
            for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
                let Some(deps) = section.get(kind).and_then(toml::Value::as_table) else {
                    continue;
                };
                for (alias, spec) in deps {
                    if !spec.is_table() {
                        continue;
                    }
                    let inherited = spec
                        .get("workspace")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false);
                    let spec = if inherited {
                        workspace
                            .and_then(|v| v.get("dependencies"))
                            .and_then(|v| v.get(alias))
                    } else {
                        Some(spec)
                    };
                    let Some(path) = spec
                        .and_then(|v| v.get("path"))
                        .and_then(toml::Value::as_str)
                    else {
                        continue;
                    };
                    let directory = if inherited {
                        normalize(path)
                    } else {
                        normalize(&format!("{}/{path}", package.directory).trim_start_matches('/'))
                    };
                    if let Some(name) = by_directory.get(&directory) {
                        package.dependencies.insert(name.clone());
                    }
                }
            }
        }
    }
    Ok(packages)
}

pub fn public_api_edit(text: &str, lines: &BTreeSet<usize>, host: &str) -> Result<bool, String> {
    if only_test_lines(text, lines, host) {
        return Ok(false);
    }
    let code = code_text(text)?;
    if !Re::new(r"\bpub\s+(?!\()|\bimpl\b|#\s*\[\s*macro_export\b").is_match(&code) {
        return Ok(false);
    }
    let scopes = inline_scopes("", text)?;
    let private: Vec<_> = rust_functions(text, host)
        .into_iter()
        .filter(|f| {
            !Re::new(r"^\s*pub\s").is_match(&f.body)
                && !scopes[f.start].1
                && !f.attrs.contains("macro_export")
        })
        .collect();
    let rows: Vec<_> = code.lines().collect();
    Ok(lines.iter().any(|n| {
        n.checked_sub(1)
            .and_then(|i| rows.get(i))
            .is_some_and(|r| !r.trim().is_empty())
            && !only_test_lines(text, &BTreeSet::from([*n]), host)
            && !private.iter().any(|f| f.start < *n && *n <= f.end)
    }))
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ChangedLibrary {
    pub package: String,
    pub paths: Vec<String>,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PendingPackage {
    pub package: String,
    pub status: String,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CompilePlan {
    pub policy: String,
    pub changed_libraries: Vec<ChangedLibrary>,
    pub packages: Vec<PendingPackage>,
}

pub fn reverse_closure(
    edges: &BTreeMap<String, BTreeSet<String>>,
    affected: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut reverse = BTreeSet::new();
    let mut frontier = affected.clone();
    while !frontier.is_empty() {
        let next: BTreeSet<_> = edges
            .iter()
            .filter(|(_, deps)| !deps.is_disjoint(&frontier))
            .map(|(n, _)| n.clone())
            .filter(|n| !reverse.contains(n))
            .collect();
        reverse.extend(next.iter().cloned());
        frontier = next;
    }
    reverse
}

/// One revision's workspace: tracked paths, Rust and manifest texts, packages.
pub struct Snapshot {
    pub files: BTreeSet<String>,
    pub texts: Texts,
    pub packages: BTreeMap<String, Package>,
}

pub fn snapshot(repo: &Repository, revision: &str) -> Result<Snapshot, String> {
    let files: BTreeSet<_> = repo
        .files(revision)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    let paths: Vec<_> = files
        .iter()
        .filter(|p| p.ends_with(".rs") || *p == "Cargo.toml" || p.ends_with("/Cargo.toml"))
        .cloned()
        .collect();
    let texts = repo.blobs(revision, &paths).map_err(|e| e.to_string())?;
    let packages = workspace_test_graph(&texts, &files)?;
    Ok(Snapshot {
        files,
        texts,
        packages,
    })
}

/// Changed public library surfaces to transitive workspace test-target builds,
/// from two already-loaded snapshots (`[base, head]`).
pub fn compile_plan_with(
    repo: &Repository,
    snapshots: &[Snapshot; 2],
    base: &str,
    head: &str,
    changed: &[String],
    host: &str,
) -> Result<CompilePlan, String> {
    let mut affected: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut head_packages = BTreeSet::new();
    if changed.iter().any(|p| p.ends_with(".rs")) {
        for (side, snapshot) in snapshots.iter().enumerate() {
            if side == 1 {
                head_packages = snapshot.packages.keys().cloned().collect();
            }
            for (name, package) in &snapshot.packages {
                edges
                    .entry(name.clone())
                    .or_default()
                    .extend(package.dependencies.clone());
                let Some(library) = &package.library else {
                    continue;
                };
                let mounted = mount_closure(&snapshot.texts, library, host);
                for path in changed.iter().filter(|p| mounted.modules.contains_key(*p)) {
                    let diff = repo.diff(base, head, path).map_err(|e| e.to_string())?;
                    let mut lines = BTreeSet::new();
                    for h in changed_line_ranges(&diff) {
                        let (start, count) = if side == 0 {
                            (h.old_first, h.old_count)
                        } else {
                            (h.new_first, h.new_count)
                        };
                        lines.extend(start..start + count);
                    }
                    if public_api_edit(&snapshot.texts[path], &lines, host)? {
                        affected
                            .entry(name.clone())
                            .or_default()
                            .insert(path.clone());
                    }
                }
            }
        }
    }
    let reverse = reverse_closure(&edges, &affected.keys().cloned().collect());
    Ok(CompilePlan {
        policy: "workspace_reverse_dependencies".into(),
        changed_libraries: affected
            .into_iter()
            .map(|(package, paths)| ChangedLibrary {
                package,
                paths: paths.into_iter().collect(),
            })
            .collect(),
        packages: reverse
            .intersection(&head_packages)
            .map(|n| PendingPackage {
                package: n.clone(),
                status: "pending".into(),
            })
            .collect(),
    })
}

pub fn downstream_compile_plan(
    repo: &Repository,
    base: &str,
    head: &str,
    changed: &[String],
    host: &str,
) -> Result<CompilePlan, String> {
    if !changed.iter().any(|p| p.ends_with(".rs")) {
        return compile_plan_with(
            repo,
            &[
                Snapshot {
                    files: BTreeSet::new(),
                    texts: Texts::new(),
                    packages: BTreeMap::new(),
                },
                Snapshot {
                    files: BTreeSet::new(),
                    texts: Texts::new(),
                    packages: BTreeMap::new(),
                },
            ],
            base,
            head,
            changed,
            host,
        );
    }
    let snapshots = [snapshot(repo, base)?, snapshot(repo, head)?];
    compile_plan_with(repo, &snapshots, base, head, changed, host)
}
