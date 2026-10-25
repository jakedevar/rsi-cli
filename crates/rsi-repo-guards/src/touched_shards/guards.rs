//! Declared coverage and guard observers: `tools/test-guards.json` (additive,
//! exact cross-module observers), `scripts/test-coverage.json` (declared rules
//! that replace a module filter when every changed line is inside a named
//! function), source-observer pins, the agent verb catalog pins and the
//! workspace manifest-dir guard. Everything reads pinned text, never Cargo.
use super::{
    filters::{self, NARROW_TEST_CAP, Observer},
    identity::{ItemReferences, SourceScopes},
    selection,
    source::{self, SourceTree, package_for},
};
use crate::{re::Re, rsid_test_shards::Shard};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

pub const GUARD_DECLARATIONS: &str = "tools/test-guards.json";
pub const COVERAGE_DECLARATIONS: &str = "scripts/test-coverage.json";
pub const AGENT_VERB_PIN_CAP: usize = 80;
/// {changed source: ((test module, test name), ...)}: source readers whose data an
/// rsid test consumes through a call, so no include edge shows them (#2949).
pub const SOURCE_OBSERVER_PINS: &[(&str, &[(&str, &str)])] = &[(
    "crates/rsi-common/src/agent_control_schema.rs",
    &[(
        "rpc::tests",
        "operator_daemon_config_is_absent_from_every_agent_surface",
    )],
)];
const AGENT_VERB_PIN_MODULES: [&str; 2] = ["rpc::", "session::harness::tools::"];

static TEST_ATTR: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[(?:[a-z_]+::)?test\b"));
static IGNORE_ATTR: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[ignore\b"));
static VERB_NAME: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"operator_only|catalog|agent_verb|read_verb|agent_gate"));

/// A production module without tests is observed by its nearest parent's test
/// modules; never broaden all the way to the crate's tests.
pub fn sibling_behaviour_filters(sources: &SourceTree, path: &str) -> Result<Vec<String>, String> {
    let Some(module) = sources.module(path) else {
        return Ok(Vec::new());
    };
    if !sources.lib_mounted(path) || source::is_test_module(&module) || sources.has_tests(path)? {
        return Ok(Vec::new());
    }
    let package = package_for(path);
    let mut parent = module.rsplit_once("::").map_or("", |(p, _)| p).to_owned();
    while !parent.is_empty() {
        let prefix = format!("{parent}::");
        let mut found = BTreeSet::new();
        for (p, m) in &sources.mounts.modules {
            if package_for(p) != package || !m.starts_with(&prefix) {
                continue;
            }
            let head = m[prefix.len()..].split("::").next().unwrap_or("");
            if !source::is_test_module(head) {
                continue;
            }
            for shard in sources.shards_for(p)? {
                if sources.filter_matches(&shard, m) {
                    found.insert(format!(
                        "rsid=shard:{shard}:{}",
                        filters::anchored_filterset(m)
                    ));
                }
            }
        }
        if !found.is_empty() {
            return Ok(found.into_iter().collect());
        }
        parent = parent.rsplit_once("::").map_or("", |(p, _)| p).to_owned();
    }
    Ok(Vec::new())
}

/// The exact tests `SOURCE_OBSERVER_PINS` names for the changed sources, and
/// the pins whose test no longer exists (reported rather than silently dropped).
pub fn source_observer_pins(
    index: &ItemReferences,
    changed: &BTreeSet<String>,
    comment_only: &BTreeSet<String>,
) -> (Vec<Observer>, Vec<(String, String)>) {
    let wanted: BTreeSet<(String, String)> = SOURCE_OBSERVER_PINS
        .iter()
        .filter(|(p, _)| changed.contains(*p) && !comment_only.contains(*p))
        .flat_map(|(_, pins)| pins.iter().map(|(m, n)| ((*m).to_owned(), (*n).to_owned())))
        .collect();
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for f in &index.functions {
        let key = (f.module.clone(), f.function.name.clone());
        if source::is_test(&f.function.attrs) && wanted.contains(&key) {
            seen.insert(key);
            for shard in source::shards_in(&f.function.attrs) {
                found.push(Observer {
                    module: f.module.clone(),
                    name: f.function.name.clone(),
                    shard,
                });
            }
        }
    }
    (found, wanted.difference(&seen).cloned().collect())
}

/// Additive exact cross-module observers; the entire candidate table is
/// validated. `table` is the pinned `tools/test-guards.json` (None: older tree).
pub fn declared_guard_filters(
    sources: &SourceTree,
    table: Option<&str>,
    changed: &BTreeSet<String>,
    shards: &[Shard],
) -> Result<(Vec<String>, Vec<Value>), String> {
    let Some(table) = table else {
        return Ok((Vec::new(), Vec::new()));
    };
    let config: Value = serde_json::from_str(table).map_err(|e| e.to_string())?;
    let invalid = || "expected version: 1 and a guards array".to_owned();
    let object = config.as_object().ok_or_else(invalid)?;
    let guards = object
        .get("guards")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if object.len() != 2
        || object
            .get("version")
            .is_none_or(|v| !v.is_i64() || v.as_i64() != Some(1))
    {
        return Err(invalid());
    }
    let pattern = Re::new(r"^[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*$");
    let mut filters_out = Vec::new();
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, rule) in guards.iter().enumerate() {
        let label = format!("guards[{index}]");
        let rule = rule
            .as_object()
            .filter(|r| {
                r.len() == 3
                    && ["package", "test", "modules"]
                        .iter()
                        .all(|k| r.contains_key(*k))
            })
            .ok_or_else(|| format!("{label}: expected package, test and modules"))?;
        let package = rule["package"]
            .as_str()
            .filter(|p| *p == "rsid" || *p == "rsid-store")
            .ok_or_else(|| format!("{label}: package must be rsid or rsid-store"))?;
        let test = rule["test"]
            .as_str()
            .filter(|t| pattern.is_match(t))
            .ok_or_else(|| format!("{label}: expected a full test name"))?;
        let modules: Vec<&str> = rule["modules"]
            .as_array()
            .filter(|m| !m.is_empty())
            .and_then(|m| {
                m.iter()
                    .map(|v| v.as_str().filter(|s| pattern.is_match(s)))
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|m| m.iter().collect::<BTreeSet<_>>().len() == m.len())
            .ok_or_else(|| {
                format!("{label}: expected unique module names in a nonempty modules array")
            })?;
        if !seen.insert((package, test)) {
            return Err(format!("{label}: duplicate test {package}::{test}"));
        }
        let paths: BTreeMap<&String, &String> = sources
            .mounts
            .modules
            .iter()
            .filter(|(p, _)| package_for(p).as_deref() == Some(package) && sources.lib_mounted(p))
            .collect();
        for module in &modules {
            if !paths.values().any(|m| m.as_str() == *module) {
                return Err(format!(
                    "{label}: unknown file-backed module {package}::{module}"
                ));
            }
        }
        let mut matches: Vec<BTreeSet<String>> = Vec::new();
        for (path, module) in &paths {
            let text = sources.texts.get(*path).map_or("", String::as_str);
            let leaf = test.rsplit("::").next().unwrap_or(test);
            if !text.contains(leaf) {
                continue;
            }
            let scopes = SourceScopes::new(module, text)?;
            for f in source::rust_functions(text, &sources.host) {
                let scope = &scopes.lines[f.start].module;
                let full = [scope.as_str(), f.name.as_str()]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join("::");
                if full == test && TEST_ATTR.is_match(&f.attrs) && !IGNORE_ATTR.is_match(&f.attrs) {
                    matches.push(source::shards_in(&f.attrs));
                }
            }
        }
        if matches.len() != 1 || matches[0].is_empty() {
            return Err(format!(
                "{label}: unknown or non-runnable sharded test {package}::{test}"
            ));
        }
        let mine: Vec<&String> = matches[0].iter().collect();
        for shard in &mine {
            let declares = shards
                .iter()
                .find(|s| &s.name == *shard)
                .is_some_and(|s| s.packages.iter().any(|p| p == package));
            if !declares {
                return Err(format!(
                    "{label}: test shard does not declare package {package}"
                ));
            }
        }
        let exact: Vec<String> = mine
            .iter()
            .map(|shard| format!("rsid=shard:{shard}:test(={test})"))
            .collect();
        if exact.iter().any(|f| f.len() > filters::GATE_FILTER_LENGTH) {
            return Err(format!(
                "{label}: exact test filter exceeds {} bytes",
                filters::GATE_FILTER_LENGTH
            ));
        }
        let touched: Vec<&String> = paths
            .iter()
            .filter(|(p, m)| {
                changed.contains(**p)
                    && modules
                        .iter()
                        .any(|g| m.as_str() == *g || m.starts_with(&format!("{g}::")))
            })
            .map(|(p, _)| *p)
            .collect();
        if !touched.is_empty() || changed.contains(GUARD_DECLARATIONS) {
            filters_out.extend(exact.iter().cloned());
            selected.push(json!({
                "package": package,
                "test": test,
                "paths": touched,
                "filters": exact,
            }));
        }
    }
    let mut seen = BTreeSet::new();
    filters_out.retain(|f| seen.insert(f.clone()));
    Ok((filters_out, selected))
}

/// #417: the workspace guard reads raw Rust files beneath `crates/`, including
/// tests and comments. `files` lists the candidate's tracked paths.
pub fn manifest_dir_guard_filters(
    files: &BTreeSet<String>,
    changed: &BTreeSet<String>,
) -> Vec<String> {
    const GUARD: &str = "crates/rsi-common/tests/manifest_dir_guard.rs";
    if !files.contains(GUARD) {
        return Vec::new();
    }
    let read = changed.iter().any(|path| {
        path.starts_with("crates/")
            && path.ends_with(".rs")
            && !path.split('/').any(|part| part == "target")
            && path.rsplit('/').next() != Some("build.rs")
            && path != "crates/rsi-common/src/manifest_dir.rs"
    });
    if read {
        vec!["rsi-common=test:manifest_dir_guard:test(=workspace_code_never_bakes_the_manifest_path_at_compile_time)".into()]
    } else {
        Vec::new()
    }
}

/// Named filters for the agent verb catalog tests (#1440), per shard and module.
pub fn verb_pin_filters(index: &ItemReferences) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut found: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for f in &index.functions {
        let module = format!("{}::", f.module);
        if source::is_test(&f.function.attrs)
            && AGENT_VERB_PIN_MODULES.iter().any(|m| module.starts_with(m))
            && (VERB_NAME.is_match(&f.function.name)
                || f.words.contains("AGENT_VERBS")
                || f.words.contains("READ_VERBS"))
        {
            for shard in source::shards_in(&f.function.attrs) {
                found
                    .entry((shard, f.module.clone()))
                    .or_default()
                    .insert(f.function.name.clone());
            }
        }
    }
    found
}

fn contained(
    text: &str,
    lines: &BTreeSet<usize>,
    names: &BTreeSet<String>,
    prefixes: &[String],
    host: &str,
) -> bool {
    let rows: Vec<_> = text.lines().collect();
    let functions = source::rust_functions(text, host);
    for &number in lines {
        let row = number
            .checked_sub(1)
            .and_then(|i| rows.get(i))
            .map_or("", |r| r.trim());
        if row.is_empty() || row.starts_with("//") {
            continue;
        }
        let inside = functions.iter().any(|f| {
            f.attr_start < number
                && number <= f.end
                && (names.contains(&f.name) || prefixes.iter().any(|p| f.name.starts_with(p)))
        });
        if !inside {
            return false;
        }
    }
    true
}

/// `(filters, rule)` a declared rule gives the diff of `path` (#1585): used only
/// when every changed line, old and new, lies in a function the rule names and
/// the rule's tests resolve to at most `NARROW_TEST_CAP` sharded tests, every
/// exact name included. Anything else is `None` and the caller derives module filters.
pub fn declared_coverage(
    declarations: Option<&str>,
    old_text: Option<&str>,
    new_text: &str,
    hunks: &[super::git::Hunk],
    path: &str,
    index: &ItemReferences,
    host: &str,
) -> Option<(Vec<String>, Value)> {
    let rules: Value = serde_json::from_str(declarations?).ok()?;
    let rule = rules
        .get("rules")
        .and_then(Value::as_array)?
        .iter()
        .find(|r| r.get("path").and_then(Value::as_str) == Some(path))?;
    let old_text = old_text?;
    let strings = |key: &str| -> Vec<String> {
        rule.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let names: BTreeSet<String> = strings("functions").into_iter().collect();
    let prefixes = strings("function_prefixes");
    let old_lines = selection::changed_lines(hunks, true);
    let new_lines = selection::changed_lines(hunks, false);
    if (old_lines.is_empty() && new_lines.is_empty())
        || !contained(old_text, &old_lines, &names, &prefixes, host)
        || !contained(new_text, &new_lines, &names, &prefixes, host)
    {
        return None;
    }
    let specs: Vec<&Value> = match rule.get("tests") {
        Some(Value::Array(list)) => list.iter().collect(),
        Some(single) => vec![single],
        None => vec![&Value::Null],
    };
    let mut picked = BTreeSet::new();
    for spec in specs {
        let list = |key: &str| -> Vec<String> {
            spec.get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let exact: BTreeSet<String> = list("names").into_iter().collect();
        let test_prefixes = list("prefixes");
        let module = spec.get("module").and_then(Value::as_str);
        let mut got = BTreeSet::new();
        for f in &index.functions {
            if source::is_test(&f.function.attrs)
                && Some(f.module.as_str()) == module
                && (exact.contains(&f.function.name)
                    || test_prefixes.iter().any(|p| f.function.name.starts_with(p)))
            {
                for shard in source::shards_in(&f.function.attrs) {
                    got.insert(Observer {
                        module: f.module.clone(),
                        name: f.function.name.clone(),
                        shard,
                    });
                }
            }
        }
        let found: BTreeSet<_> = got.iter().map(|o| o.name.clone()).collect();
        if got.is_empty() || !exact.is_subset(&found) {
            return None;
        }
        picked.extend(got);
    }
    let picked: Vec<_> = picked.into_iter().collect();
    let tests: BTreeSet<_> = picked.iter().map(|o| (&o.module, &o.name)).collect();
    if picked.is_empty() || tests.len() > NARROW_TEST_CAP {
        return None;
    }
    Some((filters::traced_filters(&picked), rule.clone()))
}
