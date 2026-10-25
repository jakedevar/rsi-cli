//! Pure planning helpers ported from the retained scoped-test: ordering,
//! bounded worker selection, deferred coverage, slow tier and host sizing.
use super::selection::{Plan, Selection};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const THREADS_MIN: usize = 2;
pub const THREADS_MAX: usize = 12;
/// A package that owns a changed module runs even in a spent time box.
pub const SPENT_BOX_SEC: f64 = 1e-3;
pub const DEFERRED_ROUTES: &[&str] = &["merge_queue_gate", "pinned_full_qa"];
pub const E2E_TARGET: &str = "e2e_tui";

pub type Ordered = Vec<(String, Vec<Selection>)>;

pub fn plan_filter_count(plan: &Ordered) -> usize {
    plan.iter()
        .flat_map(|(_, selections)| selections.iter())
        .filter(|s| !s.direct_only)
        .map(|s| s.filter.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

/// Added/changed tests first, then packages that own a changed module, then
/// packages holding a proven direct observer, then the most specific: fewest
/// broad (non-exact, non-regex) filters, then fewest filters.
pub fn package_order(
    plan: Plan,
    changed_packages: &BTreeSet<String>,
    direct_packages: &BTreeSet<String>,
) -> Ordered {
    let mut items: Ordered = plan.into_iter().collect();
    items.sort_by_key(|(package, selections)| {
        let broad = selections
            .iter()
            .filter(|s| s.exact.is_none() && s.regex.is_none())
            .count();
        (
            !selections.iter().any(|s| s.priority),
            !changed_packages.contains(package),
            !direct_packages.contains(package),
            broad,
            selections.len(),
            package.clone(),
        )
    });
    items
}

/// `clamp((host threads - 1-minute load) / 2, 2, 12)`.
pub fn adaptive_threads(host_threads: usize, load1: Option<f64>) -> usize {
    let free = host_threads as f64 - load1.unwrap_or(0.0);
    (free / 2.0).clamp(THREADS_MIN as f64, THREADS_MAX as f64) as usize
}

/// (logical threads, 1-minute load or None), read at run time.
pub fn host_capacity() -> (usize, Option<f64>) {
    let threads = std::thread::available_parallelism().map_or(2, usize::from);
    #[cfg(unix)]
    let load = {
        let mut samples = [0f64; 1];
        // SAFETY: getloadavg writes at most one f64 to the supplied buffer.
        (unsafe { libc::getloadavg(samples.as_mut_ptr(), 1) } == 1).then_some(samples[0])
    };
    #[cfg(not(unix))]
    let load = None;
    (threads, load)
}

pub fn selects_e2e(selections: &[Selection]) -> bool {
    selections
        .iter()
        .any(|s| s.kind == "test" && s.target == E2E_TARGET)
}

/// `[package, binary]` pairs from the single shared prebuild list.
pub fn e2e_prebuild_pairs(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some(package), Some(binary), None) => pairs.push((package.into(), binary.into())),
            _ => return Err(format!("malformed e2e prebuild line: {line:?}")),
        }
    }
    Ok(pairs)
}
pub fn e2e_prebuild_command(cargo: &[String], text: &str) -> Result<Vec<String>, String> {
    let pairs = e2e_prebuild_pairs(text)?;
    let mut command = cargo.to_vec();
    command.push("build".into());
    let mut seen = BTreeSet::new();
    for (package, _) in &pairs {
        if seen.insert(package.clone()) {
            command.extend(["-p".into(), package.clone()]);
        }
    }
    for (_, binary) in &pairs {
        command.extend(["--bin".into(), binary.clone()]);
    }
    Ok(command)
}

/// Opt-in features of the selected library tests, including direct observers.
pub fn required_features(
    inventory: &[(String, Vec<String>)],
    selections: &[Selection],
) -> Result<Vec<String>, String> {
    let mut features = BTreeSet::new();
    for (name, required) in inventory {
        for selection in selections.iter().filter(|s| s.kind == "lib") {
            if selection.matches(name)? {
                features.extend(required.iter().cloned());
                break;
            }
        }
    }
    Ok(features.into_iter().collect())
}

pub fn nextest_required(config: &str) -> Vec<u64> {
    let Some(start) = config.find("nextest-version") else {
        return vec![];
    };
    let rest = &config[start..];
    let Some(open) = rest.find('{') else {
        return vec![];
    };
    let Some(close) = rest[open..].find('}') else {
        return vec![];
    };
    let table = &rest[open..open + close];
    let Some(required) = table.find("required") else {
        return vec![];
    };
    let tail = &table[required..];
    let Some(quote) = tail.find('"') else {
        return vec![];
    };
    tail[quote + 1..]
        .split('"')
        .next()
        .unwrap_or("")
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect()
}

pub fn version_tuple(text: &str) -> Vec<u64> {
    text.split('.')
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// Slow tier decision: explicit filters run exactly what they name.
pub fn slow_tier_decision(derived: &Value, explicit: bool) -> Value {
    if explicit {
        return json!({"mode":"full","reason":"explicit_filters"});
    }
    match derived.get("slow_tier") {
        Some(value) if value.as_object().is_some_and(|o| !o.is_empty()) => value.clone(),
        _ => json!({"mode":"full","reason":"no_decision"}),
    }
}
pub fn slow_tier_entry(decision: &Value) -> Value {
    let mut entry = json!({
        "mode": decision.get("mode").cloned().unwrap_or_else(|| json!("full")),
        "reason": decision.get("reason").cloned().unwrap_or(Value::Null),
        "skipped_tests": 0,
        "package": null,
    });
    for key in ["path", "line"] {
        if let Some(value) = decision.get(key) {
            entry[key] = value.clone();
        }
    }
    entry
}
pub fn slow_tier_summary(entry: &Value) -> String {
    if entry["mode"] == "excluded" {
        format!(
            "slow tier excluded ({}): {} released-migration tests are left to the merge-queue full QA; a diff that touches a migration, the runner or such a test runs them",
            entry["reason"].as_str().unwrap_or(""),
            entry["tests"].as_u64().unwrap_or(0)
        )
    } else {
        let at = entry["path"]
            .as_str()
            .map(|p| format!(" at {p}"))
            .unwrap_or_default();
        format!(
            "slow tier runs in full ({}{at})",
            entry["reason"].as_str().unwrap_or("")
        )
    }
}

fn items(value: &Value, key: &str) -> Vec<Value> {
    value[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item.is_object())
        .cloned()
        .collect()
}

/// {filter expression: its tests}: one selection per expression, whatever
/// shard label carries it. Only an identical expression is merged.
pub fn distinct_expressions(entries: &[Value]) -> BTreeMap<String, u64> {
    let mut expressions = BTreeMap::<String, u64>::new();
    for item in entries {
        let filter = item["filter"].as_str().unwrap_or("");
        let expression = filter
            .split_once(":test(")
            .map_or("", |(_, tail)| tail)
            .to_string();
        let tests = item["tests"].as_u64().unwrap_or(0);
        let slot = expressions.entry(expression).or_default();
        *slot = (*slot).max(tests);
    }
    expressions
}

/// Anchored `/^module::/)` expression -> its module.
fn anchored_module(expression: &str) -> Option<&str> {
    let inner = expression.strip_prefix("/^")?.strip_suffix("::/)")?;
    (!inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':'))
    .then_some(inner)
}

pub fn scope_tests_at_most(entries: &[Value]) -> u64 {
    let expressions = distinct_expressions(entries);
    let anchored: Vec<&str> = expressions
        .keys()
        .filter_map(|expression| anchored_module(expression))
        .collect();
    expressions
        .iter()
        .filter(|(expression, _)| {
            anchored_module(expression).is_none_or(|module| {
                !anchored
                    .iter()
                    .any(|other| module.starts_with(&format!("{other}::")))
            })
        })
        .map(|(_, tests)| *tests)
        .sum()
}

/// The worker's bounded view of a derivation whose traces stopped at a cap.
pub fn bounded_worker_selection(derived: &Value) -> Option<Value> {
    let broad: Vec<Value> = items(derived, "broad_filters")
        .into_iter()
        .filter(|item| item["filter"].as_str().is_some_and(|f| !f.is_empty()))
        .collect();
    let filter_of = |item: &Value| item["filter"].as_str().unwrap_or("").to_string();
    let unbounded: BTreeSet<String> = broad
        .iter()
        .filter(|item| !item["worker_bounded"].as_bool().unwrap_or(false))
        .map(filter_of)
        .collect();
    let bounded: BTreeSet<String> = broad
        .iter()
        .filter(|item| item["worker_bounded"].as_bool().unwrap_or(false))
        .map(filter_of)
        .filter(|f| !unbounded.contains(f))
        .collect();
    let suggested: Vec<String> = derived["suggested_filters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let planned: Vec<String> = suggested
        .iter()
        .filter(|v| bounded.contains(*v))
        .cloned()
        .collect();
    if planned.is_empty() {
        return None;
    }
    let mut covered: BTreeSet<String> = planned.iter().cloned().collect();
    let mut restored: Vec<String> = Vec::new();
    let merged = items(derived, "merged_filters");
    loop {
        let mut settled = true;
        for item in &merged {
            let (Some(value), Some(by)) = (item["filter"].as_str(), item["covered_by"].as_str())
            else {
                continue;
            };
            if value.is_empty()
                || !covered.contains(by)
                || covered.contains(value)
                || restored.iter().any(|r| r == value)
            {
                continue;
            }
            settled = false;
            if bounded.contains(value)
                || item["reason"]
                    .as_str()
                    .is_some_and(|r| r.starts_with("identical expression"))
            {
                covered.insert(value.into());
            } else {
                restored.push(value.into());
            }
        }
        if settled {
            break;
        }
    }
    let entries: Vec<Value> = broad
        .iter()
        .filter(|item| bounded.contains(item["filter"].as_str().unwrap_or("")))
        .cloned()
        .collect();
    let stays: BTreeSet<String> = unbounded
        .iter()
        .filter(|v| suggested.contains(v))
        .cloned()
        .collect();
    let mut filters: Vec<String> = Vec::new();
    for value in suggested
        .iter()
        .filter(|v| !bounded.contains(*v))
        .chain(restored.iter())
        .chain(planned.iter())
    {
        if !filters.contains(value) {
            filters.push(value.clone());
        }
    }
    Some(json!({
        "filters": filters, "bounded": planned, "restored": restored,
        "entries": entries, "broad_filters_left": stays,
    }))
}

pub fn deferred_record(bounded: &Value, head: &str, direct: &Value) -> Value {
    let entries: Vec<Value> = bounded["entries"].as_array().cloned().unwrap_or_default();
    let stops: BTreeSet<String> = entries
        .iter()
        .map(|item| {
            item["stop"]
                .as_str()
                .map_or_else(|| item["stop"].to_string(), str::to_owned)
        })
        .collect();
    let keep = [
        "filter",
        "tests",
        "stop",
        "reason",
        "path",
        "direct_tests",
        "hubs",
    ];
    json!({
        "policy": "bounded_worker_selection",
        "reason": format!("the observer trace stopped at a cap ({}): the worker runs the changed modules' tests and the proven direct observers nearest first and does not widen to the module filters", stops.into_iter().collect::<Vec<_>>().join(", ")),
        "filters": entries.iter().map(|item| Value::Object(keep.iter().filter_map(|k| item.get(*k).map(|v| ((*k).to_string(), v.clone()))).collect())).collect::<Vec<_>>(),
        "bounded_filters": bounded["bounded"],
        "restored_filters": bounded["restored"],
        "scope_tests_at_most": scope_tests_at_most(&entries),
        "scope_basis": "derived source inventory, before the kept changed-module and direct-observer tests are subtracted",
        "direct_observers": if direct.is_object() { direct.clone() } else { json!({}) },
        "tests": null,
        "tests_basis": "pending: counted from the package test listings when the run lists them",
        "routes": DEFERRED_ROUTES,
        "candidate": head,
        "status": "pending",
        "status_basis": "deferred coverage is not passed, run or fully verified here: the merge-queue gate runs suggested_filters, the full-library QA runs the whole candidate once it is landed",
    })
}

/// Why the derivation exited non-zero, from its own receipt, so a red is never silent.
pub fn derive_failure_reasons(derived: &Value, code: i32) -> Vec<String> {
    let mut reasons = Vec::new();
    if derived["merge_clean"] == false {
        reasons.push(
            "candidate does not merge cleanly with the base: merge origin/rolling and retry"
                .to_string(),
        );
    }
    if derived["migrations"]["numbering_conflict"]
        .as_bool()
        .unwrap_or(false)
    {
        reasons.push("new migration numbering conflicts with the base head".into());
    }
    if derived["migrations"]["stray_pre_split"]
        .as_bool()
        .unwrap_or(false)
    {
        reasons.push("stray pre-split migration file".into());
    }
    if let Some(audits) = derived["audits"].as_object() {
        for (name, audit) in audits {
            if audit["verdict"] == "fail" {
                reasons.push(format!(
                    "audit {name} failed: {}",
                    audit["detail"].as_str().unwrap_or("null")
                ));
            }
        }
    }
    if let Some(failed) = derived["shards"]["compiled_failed"]
        .as_array()
        .filter(|a| !a.is_empty())
    {
        reasons.push(format!(
            "shard compile failed: {}",
            failed
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if reasons.is_empty() {
        reasons.push(format!("derivation exited {code}"));
    }
    reasons
}

/// Receipt fields for a broad-observer fallback (#2105), or None.
pub fn broad_observer_fields(derived: &Value) -> Option<Value> {
    let entries = items(derived, "broad_filters");
    if !(derived["broad"].as_bool().unwrap_or(false) || !entries.is_empty()) {
        return None;
    }
    let selections = distinct_expressions(&entries);
    let keep = [
        "filter",
        "tests",
        "reason",
        "direct_tests",
        "trace_layers",
        "trace_stopped",
        "hubs",
    ];
    Some(json!({
        "uncertain": "broad_observers",
        "broad_filter_count": selections.len(),
        "broad_filter_entries": entries.len(),
        "broad_tests": selections.values().sum::<u64>(),
        "broad_filters": entries.iter().map(|item| Value::Object(keep.iter().filter_map(|k| item.get(*k).map(|v| ((*k).to_string(), v.clone()))).collect())).collect::<Vec<_>>(),
    }))
}

/// Workspace members named by the root manifest's `members = [...]`.
pub fn workspace_members(manifest: &str) -> BTreeSet<String> {
    let Some(start) = manifest.find("members") else {
        return BTreeSet::new();
    };
    let rest = &manifest[start..];
    let (Some(open), Some(close)) = (rest.find('['), rest.find(']')) else {
        return BTreeSet::new();
    };
    if close < open {
        return BTreeSet::new();
    }
    rest[open + 1..close]
        .split('"')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, name)| name.to_string())
        .collect()
}

/// `name = "..."` of a manifest's `[package]` table.
pub fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = trimmed.strip_prefix("name") {
                let rest = rest.trim_start().strip_prefix('=')?.trim();
                return rest.strip_prefix('"')?.split('"').next().map(str::to_owned);
            }
        }
    }
    None
}

/// Workspace packages under `crates/` with a changed Rust source or manifest,
/// plus `extra` (the downstream compile set). A docs-only file inside a crate
/// does not select its package; a root manifest or lock change selects none.
pub fn linux_lane_packages(
    changed: &str,
    root_manifest: Option<&str>,
    manifest_of: impl Fn(&str) -> Option<String>,
    extra: &BTreeSet<String>,
) -> Vec<String> {
    let mut packages = extra.clone();
    let Some(root) = root_manifest else {
        return packages.into_iter().collect();
    };
    let members = workspace_members(root);
    for path in changed.lines() {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() < 3
            || parts[0] != "crates"
            || !(path.ends_with(".rs") || parts[parts.len() - 1] == "Cargo.toml")
        {
            continue;
        }
        let member = format!("{}/{}", parts[0], parts[1]);
        if !members.contains(&member) {
            continue;
        }
        if let Some(name) = manifest_of(&member).as_deref().and_then(package_name) {
            packages.insert(name);
        }
    }
    packages.into_iter().collect()
}

/// Select the proven direct observers of a bounded plan in whatever package
/// and binary they sit; they carry no coverage entry and no required match.
pub fn with_direct_observers(plan: &mut Plan, direct: &Plan) {
    for (package, selections) in direct {
        plan.entry(package.clone())
            .or_default()
            .extend(selections.iter().cloned().map(|mut s| {
                s.priority = false;
                s.direct_only = true;
                s
            }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoped_runner::selection::selection_plan;
    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn package_order_puts_priority_changed_direct_then_most_specific_first() {
        let plan = selection_plan(
            &strings(&["a=foo", "b=test(=x)", "c=bar", "d=baz", "e=qux"]),
            &BTreeMap::new(),
            &strings(&["e=qux"]),
        )
        .unwrap();
        let ordered = package_order(
            plan,
            &BTreeSet::from(["c".to_string()]),
            &BTreeSet::from(["d".to_string()]),
        );
        let names: Vec<_> = ordered.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names, ["e", "c", "d", "b", "a"]);
        assert_eq!(plan_filter_count(&ordered), 5);
    }
    #[test]
    fn threads_follow_free_capacity_within_bounds() {
        assert_eq!(adaptive_threads(16, Some(2.0)), 7);
        assert_eq!(adaptive_threads(4, Some(6.0)), 2);
        assert_eq!(adaptive_threads(64, None), 12);
    }
    #[test]
    fn nextest_requirement_and_e2e_prebuild_are_parsed_exactly() {
        assert_eq!(
            nextest_required(
                "nextest-version = { required = \"0.9.70\", recommended = \"0.9.80\" }"
            ),
            vec![0, 9, 70]
        );
        assert_eq!(nextest_required(""), Vec::<u64>::new());
        assert!(
            version_tuple("0.9.80")
                >= nextest_required("nextest-version = { required = \"0.9.70\" }")
        );
        let command = e2e_prebuild_command(
            &strings(&["cargo"]),
            "# shared\nrsid rsid\nrsid rsi-e2e-seed # note\nrsi rsi\n",
        )
        .unwrap();
        assert_eq!(
            command,
            strings(&[
                "cargo",
                "build",
                "-p",
                "rsid",
                "-p",
                "rsi",
                "--bin",
                "rsid",
                "--bin",
                "rsi-e2e-seed",
                "--bin",
                "rsi"
            ])
        );
        assert!(e2e_prebuild_command(&[], "one two three").is_err());
    }
    #[test]
    fn required_features_union_only_selected_library_tests() {
        let plan = selection_plan(
            &strings(&["rsid=test(=a::one)", "rsid=bin:tool"]),
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
        let inventory = vec![
            ("a::one".to_string(), strings(&["f1"])),
            ("a::two".to_string(), strings(&["f2"])),
        ];
        assert_eq!(
            required_features(&inventory, &plan["rsid"]).unwrap(),
            strings(&["f1"])
        );
    }
    #[test]
    fn bounded_selection_restores_absorbed_named_filters_and_keeps_unbounded_broad() {
        let derived = json!({
            "suggested_filters": ["rsid=/^a::/", "rsid=named", "rsid=broad", "rsid=plain"],
            "broad_filters": [
                {"filter": "rsid=/^a::/", "worker_bounded": true, "stop": "tests", "tests": 30},
                {"filter": "rsid=broad", "worker_bounded": false, "tests": 99}
            ],
            "merged_filters": [{"filter": "rsid=named", "covered_by": "rsid=/^a::/", "reason": "contained"}],
        });
        let bounded = bounded_worker_selection(&derived).unwrap();
        assert_eq!(bounded["bounded"], json!(["rsid=/^a::/"]));
        assert_eq!(bounded["restored"], json!(["rsid=named"]));
        assert_eq!(bounded["broad_filters_left"], json!(["rsid=broad"]));
        assert_eq!(
            bounded["filters"],
            json!(["rsid=named", "rsid=broad", "rsid=plain", "rsid=/^a::/"])
        );
        let deferred = deferred_record(&bounded, "head", &json!({"tests": 3}));
        assert_eq!(deferred["status"], "pending");
        assert_eq!(deferred["candidate"], "head");
        assert_eq!(deferred["routes"], json!(DEFERRED_ROUTES));
        assert!(bounded_worker_selection(&json!({"suggested_filters":["x=y"]})).is_none());
    }
    #[test]
    fn scope_counts_each_expression_once_and_drops_nested_anchored_modules() {
        let entries = vec![
            json!({"filter": "rsid=shard:one:test(/^store::/)", "tests": 10}),
            json!({"filter": "rsid=shard:two:test(/^store::/)", "tests": 10}),
            json!({"filter": "rsid=shard:one:test(/^store::inner::/)", "tests": 4}),
            json!({"filter": "rsid=shard:one:test(other)", "tests": 5}),
        ];
        assert_eq!(distinct_expressions(&entries).len(), 3);
        assert_eq!(scope_tests_at_most(&entries), 15);
    }
    #[test]
    fn linux_lane_selects_changed_rust_crates_plus_downstream_only() {
        let root = "[workspace]\nmembers = [\n  \"crates/rsi\",\n  \"crates/rsid\",\n  \"crates/gone\",\n]\n";
        assert_eq!(
            workspace_members(root),
            BTreeSet::from([
                "crates/rsi".to_string(),
                "crates/rsid".to_string(),
                "crates/gone".to_string()
            ])
        );
        let manifest = |member: &str| match member {
            "crates/rsi" => Some(
                "[package]\nname = \"rsi\"\nversion = \"1\"\n[dependencies]\nname = \"not-this\"\n"
                    .to_string(),
            ),
            "crates/rsid" => Some("[package]\nname = \"rsid\"\n".to_string()),
            _ => None,
        };
        let changed = "crates/rsi/src/a.rs\ncrates/rsid/README.md\ncrates/rsid/Cargo.toml\ncrates/gone/src/x.rs\ncrates/other/src/y.rs\nCargo.toml\nCargo.lock\ncrates/rsi/docs/n.rs.txt\n";
        let extra = BTreeSet::from(["downstream".to_string()]);
        assert_eq!(
            linux_lane_packages(changed, Some(root), manifest, &extra),
            vec![
                "downstream".to_string(),
                "rsi".to_string(),
                "rsid".to_string()
            ]
        );
        // No readable root manifest: only the downstream set remains.
        assert_eq!(
            linux_lane_packages(changed, None, manifest, &extra),
            vec!["downstream".to_string()]
        );
        assert_eq!(package_name("[lib]\nname = \"x\"\n"), None);
    }
    #[test]
    fn slow_tier_and_failure_reasons_are_explicit() {
        assert_eq!(
            slow_tier_decision(&json!({}), true)["reason"],
            "explicit_filters"
        );
        assert_eq!(
            slow_tier_decision(&json!({}), false)["reason"],
            "no_decision"
        );
        let entry = slow_tier_entry(&json!({"mode":"excluded","reason":"no trigger","line":4}));
        assert_eq!(entry["line"], 4);
        assert!(slow_tier_summary(&entry).starts_with("slow tier excluded (no trigger)"));
        assert_eq!(
            derive_failure_reasons(
                &json!({"merge_clean": false, "audits": {"x": {"verdict":"fail","detail":"d"}}}),
                1
            ),
            vec![
                "candidate does not merge cleanly with the base: merge origin/rolling and retry"
                    .to_string(),
                "audit x failed: d".to_string()
            ]
        );
        assert_eq!(
            derive_failure_reasons(&json!({}), 3),
            vec!["derivation exited 3".to_string()]
        );
        assert!(broad_observer_fields(&json!({})).is_none());
        assert_eq!(
            broad_observer_fields(&json!({"broad": true})).unwrap()["broad_filter_count"],
            0
        );
    }
}
