//! Staged desktop selector from desktop-test-gate.py (#363). Selection is not
//! execution: no canonical caller switches until bounded lane execution and
//! all its behavior tests are ported too. Keep missing-selector refusals strict.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use toml::Value;

pub const SCRIPT: &str = "scripts/desktop-test-gate.py";
pub const MANIFEST: &str = "desktop/src-tauri/Cargo.toml";
pub const OUTSIDE_INPUTS: &[&str] = &["Cargo.toml", "docs/keybindings.md", "crates/rsi/src"];
const DEPENDENCY_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];
type Table = toml::value::Table;

pub fn static_inputs() -> Vec<String> {
    std::iter::once("desktop")
        .chain(OUTSIDE_INPUTS.iter().copied())
        .map(str::to_owned)
        .collect()
}
pub fn under(path: &str, inputs: &[String]) -> bool {
    let path = path.replace('\\', "/");
    inputs
        .iter()
        .any(|input| path == *input || path.starts_with(&format!("{input}/")))
}
pub fn selected(paths: &[String], inputs: &[String]) -> bool {
    paths.iter().any(|path| under(path, inputs))
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn dependency_tables(value: &Value, strict: bool) -> Result<Vec<&Table>, String> {
    fn add<'a>(
        tables: &mut Vec<&'a Table>,
        value: Option<&'a Value>,
        strict: bool,
        error: &str,
    ) -> Result<(), String> {
        if let Some(value) = value {
            match value.as_table() {
                Some(table) => tables.push(table),
                None if strict => return Err(error.into()),
                None => {}
            }
        }
        Ok(())
    }
    let mut tables = Vec::new();
    for name in DEPENDENCY_TABLES {
        add(
            &mut tables,
            value.get(name),
            strict,
            "invalid dependency table",
        )?;
    }
    if let Some(workspace) = value.get("workspace") {
        if strict && !workspace.is_table() {
            return Err("invalid workspace table".into());
        }
        add(
            &mut tables,
            workspace.get("dependencies"),
            strict,
            "invalid dependency table",
        )?;
    }
    if let Some(targets) = value.get("target") {
        match targets.as_table() {
            Some(targets) => {
                for target in targets.values() {
                    if strict && !target.is_table() {
                        return Err("invalid target dependency table".into());
                    }
                    for name in DEPENDENCY_TABLES {
                        add(
                            &mut tables,
                            target.get(name),
                            strict,
                            "invalid dependency table",
                        )?;
                    }
                }
            }
            None if strict => return Err("invalid target table".into()),
            None => {}
        }
    }
    if let Some(patches) = value.get("patch") {
        match patches.as_table() {
            Some(patches) => {
                for patch in patches.values() {
                    add(&mut tables, Some(patch), strict, "invalid dependency table")?;
                }
            }
            None if strict => return Err("invalid patch table".into()),
            None => {}
        }
    }
    add(
        &mut tables,
        value.get("replace"),
        strict,
        "invalid dependency table",
    )?;
    Ok(tables)
}

/// POSIX normalization independent of the host OS (manifest paths are Git
/// paths). The strict mode refuses platform-dependent and escaping paths.
fn directory_at(
    parent: &str,
    path: &str,
    allow_root: bool,
    strict: bool,
) -> Result<String, String> {
    if strict && (path.contains(['\\', ':']) || path.starts_with('/')) {
        return Err(format!("unresolvable dependency path {path:?}"));
    }
    let joined = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("{parent}/{path}")
    };
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else {
                    parts.push(part);
                }
            }
            _ => parts.push(part),
        }
    }
    let directory = parts.join("/");
    if directory.is_empty() && allow_root {
        return Ok(directory);
    }
    // The tolerant legacy selector deliberately drops even a '..prefix' path.
    if directory.is_empty()
        || absolute
        || (strict && (directory == ".." || directory.starts_with("../")))
        || (!strict && directory.starts_with(".."))
    {
        return Err(format!(
            "dependency path leaves the repository or names its root: {path:?}"
        ));
    }
    Ok(directory)
}

pub fn outside_inputs(mut read: impl FnMut(&str) -> Option<String>) -> Vec<String> {
    let mut inputs: BTreeSet<_> = OUTSIDE_INPUTS.iter().map(|s| (*s).to_owned()).collect();
    let root = read("Cargo.toml").and_then(|s| s.parse::<Value>().ok());
    let mut workspace_paths = BTreeMap::new();
    if let Some(table) = root
        .as_ref()
        .and_then(|v| v.get("workspace"))
        .and_then(|v| v.get("dependencies"))
        .and_then(Value::as_table)
    {
        for (name, spec) in table {
            if let Some(path) = spec.get("path").and_then(Value::as_str) {
                if let Ok(directory) = directory_at("", path, false, false) {
                    workspace_paths.insert(name, directory);
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut queue = vec![MANIFEST.to_owned()];
    while let Some(path) = queue.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some(manifest) = read(&path).and_then(|s| s.parse::<Value>().ok()) else {
            continue;
        };
        for table in dependency_tables(&manifest, false).unwrap_or_default() {
            for (name, spec) in table {
                let directory = if spec.get("workspace").and_then(Value::as_bool) == Some(true)
                    && workspace_paths.contains_key(name)
                {
                    workspace_paths.get(name).cloned()
                } else {
                    spec.get("path")
                        .and_then(Value::as_str)
                        .and_then(|p| directory_at(parent(&path), p, false, false).ok())
                };
                if let Some(directory) = directory {
                    queue.push(format!("{directory}/Cargo.toml"));
                    if directory != "desktop" && !directory.starts_with("desktop/") {
                        inputs.insert(directory);
                    }
                }
            }
        }
    }
    inputs.into_iter().collect()
}

pub fn trigger_inputs(read: impl FnMut(&str) -> Option<String>) -> Vec<String> {
    std::iter::once("desktop".into())
        .chain(outside_inputs(read))
        .collect()
}

fn manifest_at(read: &mut impl FnMut(&str) -> Option<String>, path: &str) -> Result<Value, String> {
    read(path)
        .ok_or_else(|| format!("cannot read {path}"))?
        .parse::<Value>()
        .map_err(|e| format!("cannot parse {path}: {e}"))
}
fn workspace_for(
    read: &mut impl FnMut(&str) -> Option<String>,
    path: &str,
    manifest: &Value,
) -> Result<(String, Table), String> {
    let explicit = manifest.get("package").and_then(|v| v.get("workspace"));
    let mut directory = parent(path).to_owned();
    if let Some(explicit) = explicit {
        directory = directory_at(
            &directory,
            explicit.as_str().ok_or("invalid package.workspace")?,
            true,
            true,
        )?;
    }
    loop {
        let path = if directory.is_empty() {
            "Cargo.toml".into()
        } else {
            format!("{directory}/Cargo.toml")
        };
        if read(&path).is_some() {
            let manifest = manifest_at(read, &path)?;
            if let Some(workspace) = manifest.get("workspace").and_then(Value::as_table) {
                let table = match workspace.get("dependencies") {
                    Some(v) => v
                        .as_table()
                        .ok_or("invalid workspace.dependencies")?
                        .clone(),
                    None => Table::new(),
                };
                return Ok((directory, table));
            }
        }
        if explicit.is_some() || directory.is_empty() {
            return Err(format!("cannot resolve workspace for {path}"));
        }
        directory = parent(&directory).to_owned();
    }
}

pub fn missing_selector_inputs(
    mut read: impl FnMut(&str) -> Option<String>,
) -> Result<Vec<String>, String> {
    let mut inputs: BTreeSet<_> = static_inputs().into_iter().collect();
    if read(MANIFEST).is_none() {
        return Ok(inputs.into_iter().collect());
    }
    let mut queue = vec![MANIFEST.to_owned()];
    let mut seen = BTreeSet::new();
    while let Some(path) = queue.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let manifest = manifest_at(&mut read, &path)?;
        for table in dependency_tables(&manifest, true)? {
            for (name, original) in table {
                let (directory, spec) =
                    if original.get("workspace").and_then(Value::as_bool) == Some(true) {
                        let (directory, workspace) = workspace_for(&mut read, &path, &manifest)?;
                        (
                            directory,
                            workspace
                                .get(name)
                                .ok_or_else(|| {
                                    format!("cannot resolve workspace dependency {name} in {path}")
                                })?
                                .clone(),
                        )
                    } else {
                        (parent(&path).to_owned(), original.clone())
                    };
                if !spec.is_str() && !spec.is_table() {
                    return Err(format!("invalid dependency {name} in {path}"));
                }
                if let Some(dep) = spec.get("path") {
                    let directory = directory_at(
                        &directory,
                        dep.as_str()
                            .ok_or_else(|| format!("unresolvable dependency path {dep}"))?,
                        false,
                        true,
                    )?;
                    queue.push(format!("{directory}/Cargo.toml"));
                    inputs.insert(directory);
                }
            }
        }
    }
    Ok(inputs.into_iter().collect())
}

/// Candidate selector presence is checked against committed content, never a
/// dirty worktree. On a missing selector, an unknown closure fails closed.
pub fn lane_selected_with(
    mut read: impl FnMut(&str) -> Option<String>,
    changed: &[String],
) -> Result<bool, String> {
    if read(SCRIPT).is_none() {
        if changed.is_empty() {
            return Ok(false);
        }
        let mut inputs = static_inputs();
        inputs.push(SCRIPT.into());
        let mut touched: Vec<_> = changed.iter().filter(|path| under(path, &inputs)).collect();
        if touched.is_empty() {
            let closure = missing_selector_inputs(&mut read).map_err(|e| format!("{SCRIPT} is missing from the candidate; cannot establish desktop dependency closure: {e}"))?;
            touched = changed
                .iter()
                .filter(|path| under(path, &closure))
                .collect();
        }
        if let Some(first) = touched.first() {
            return Err(format!(
                "{SCRIPT} is missing from the candidate but the diff touches {first} ({} path(s)); restore the script or land the change without it",
                touched.len()
            ));
        }
    }
    let inputs = trigger_inputs(read);
    Ok(selected(changed, &inputs))
}

pub fn lane_selected(repo: &Path, base: &str, head: &str) -> Result<bool, String> {
    let changed = Command::new("git")
        .current_dir(repo)
        .args([
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            base,
            head,
            "--",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !changed.status.success() {
        return Err(String::from_utf8_lossy(&changed.stderr).into_owned());
    }
    let paths = changed
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect::<Vec<_>>();
    lane_selected_with(
        |path| {
            let out = Command::new("git")
                .current_dir(repo)
                .args(["show", &format!("{head}:{path}")])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        },
        &paths,
    )
}

#[cfg(test)]
#[path = "desktop_selection_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "desktop_checkout_tests.rs"]
mod checkout_tests;
