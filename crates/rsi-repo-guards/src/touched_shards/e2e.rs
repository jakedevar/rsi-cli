//! #2321: `rsi=test:e2e_tui:test(<module>::)` for each e2e_tui module that
//! drives a changed TUI file. The map is `scripts/e2e-tui-surfaces.json` at the
//! candidate head. A module the e2e root does not mount, or that holds no test,
//! is dropped and reported. When every module is selected the whole target is
//! suggested instead (the same run, one filter).
use super::source::{holds_runnable_test, normalize};
use crate::re::{Re, group};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const E2E_SURFACE_CONFIG: &str = "scripts/e2e-tui-surfaces.json";

/// `*` stays in one path segment, `**` crosses directories.
pub fn glob_regex(pattern: &str) -> Re {
    let mut out = String::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            out.push_str(".*");
            i += 2;
        } else if chars[i] == '*' {
            out.push_str("[^/]*");
            i += 1;
        } else {
            out.push_str(&crate::re::escape(&chars[i].to_string()));
            i += 1;
        }
    }
    Re::new(&format!("^(?:{out})$"))
}

pub struct E2e {
    pub filters: Vec<String>,
    pub selected: Vec<Value>,
    pub dropped: Vec<Value>,
}

pub fn e2e_surface_filters(
    read: &dyn Fn(&str) -> Option<String>,
    changed: &BTreeSet<String>,
    comment_only: &BTreeSet<String>,
    host: &str,
) -> Result<E2e, String> {
    let none = || E2e {
        filters: Vec::new(),
        selected: Vec::new(),
        dropped: Vec::new(),
    };
    let Some(shown) = read(E2E_SURFACE_CONFIG) else {
        return Ok(none());
    };
    let config: Value =
        serde_json::from_str(&shown).map_err(|e| format!("{E2E_SURFACE_CONFIG}: {e}"))?;
    let field = |key: &str| -> Result<String, String> {
        config
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("{E2E_SURFACE_CONFIG}: missing {key}"))
    };
    let (package, target, root) = (field("package")?, field("target")?, field("root")?);
    let Some(root_text) = read(&root) else {
        return Ok(none());
    };
    let base_dir = root.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
    let mut mounted = BTreeMap::new();
    let mount = Re::new(r#"(?:#\[path\s*=\s*"([^"]+)"\]\s*)?(?:pub\s+)?mod\s+(\w+)\s*;"#);
    for c in mount.find_all(&root_text) {
        let relative = match group(&c, 1) {
            "" => format!("{target}/{}.rs", group(&c, 2)),
            custom => custom.to_owned(),
        };
        mounted.insert(
            group(&c, 2).to_owned(),
            normalize(&format!("{base_dir}/{relative}")),
        );
    }
    let mut holds_tests = BTreeMap::new();
    for (module, source) in &mounted {
        let holds = match read(source) {
            Some(text) => holds_runnable_test(&text, host)?,
            None => false,
        };
        holds_tests.insert(module.clone(), holds);
    }
    let candidates: Vec<_> = changed
        .iter()
        .filter(|p| !comment_only.contains(*p))
        .collect();
    let mut result = none();
    let mut seen = BTreeSet::new();
    let surfaces = config
        .get("surfaces")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{E2E_SURFACE_CONFIG}: missing surfaces"))?;
    for surface in surfaces {
        let patterns: Vec<Re> = surface
            .get("paths")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(glob_regex).collect())
            .unwrap_or_default();
        let paths: BTreeSet<&String> = candidates
            .iter()
            .copied()
            .filter(|p| patterns.iter().any(|rx| rx.is_match(p)))
            .collect();
        if paths.is_empty() {
            continue;
        }
        let modules = surface
            .get("modules")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        for module in modules {
            if seen.contains(module) {
                continue;
            }
            if !holds_tests.get(module).copied().unwrap_or(false) {
                result.dropped.push(json!({
                    "filter": format!("{package}=test:{target}:test({module}::)"),
                    "reason": format!("module `{module}` is not mounted by {root} or holds no test"),
                }));
                continue;
            }
            seen.insert(module);
            result.selected.push(json!({
                "module": module,
                "paths": paths,
                "reason": surface.get("reason").and_then(Value::as_str).unwrap_or(""),
            }));
        }
    }
    if result.selected.is_empty() {
        return Ok(result);
    }
    let every: BTreeSet<&str> = holds_tests
        .iter()
        .filter(|(_, ok)| **ok)
        .map(|(m, _)| m.as_str())
        .collect();
    result.filters = if every.is_subset(&seen) {
        vec![format!("{package}=test:{target}")]
    } else {
        result
            .selected
            .iter()
            .map(|item| {
                format!(
                    "{package}=test:{target}:test({}::)",
                    item["module"].as_str().unwrap_or("")
                )
            })
            .collect()
    };
    Ok(result)
}
