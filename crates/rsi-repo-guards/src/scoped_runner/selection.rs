//! Selector grammar and binary-scoped coverage ported from scoped-test.
use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Selection {
    pub filter: String,
    pub kind: String,
    pub target: String,
    pub substring: String,
    pub exact: Option<String>,
    pub regex: Option<String>,
    #[serde(default)]
    pub priority: bool,
    #[serde(default)]
    pub bounded: bool,
    #[serde(default)]
    pub direct_only: bool,
    #[serde(default)]
    pub distance: usize,
}

fn word(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_alphanumeric() || c == '_')
}
fn name(text: &str) -> bool {
    !text.is_empty() && text.split('-').all(word)
}
fn test_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
}

impl Selection {
    pub fn matches(&self, test: &str) -> Result<bool, String> {
        if let Some(pattern) = &self.regex {
            Regex::new(pattern)
                .map_err(|e| e.to_string())?
                .is_match(test)
                .map_err(|e| e.to_string())
        } else if let Some(exact) = &self.exact {
            Ok(test == exact)
        } else {
            Ok(test.contains(&self.substring))
        }
    }
    pub fn for_target(&self, kinds: &[String], target: &str) -> bool {
        kinds.contains(&self.kind) && (self.kind == "lib" || self.target == target)
    }
}

pub type Plan = BTreeMap<String, Vec<Selection>>;

pub fn selection_plan(
    filters: &[String],
    shards: &BTreeMap<String, Vec<String>>,
    priority: &[String],
) -> Result<Plan, String> {
    let mut plan = Plan::new();
    let mut seen = BTreeSet::new();
    for value in filters.iter().chain(priority) {
        if !seen.insert(value) {
            continue;
        }
        let Some((package, expression)) = value.split_once('=') else {
            return Err(format!("refusing unscoped filter: {value}"));
        };
        if !name(package) || expression.is_empty() {
            return Err(format!("refusing unscoped filter: {value}"));
        }
        let mut owners = vec![package.to_string()];
        let mut selection = Selection {
            filter: value.clone(),
            kind: "lib".into(),
            substring: expression.into(),
            priority: priority.contains(value),
            ..Selection::default()
        };
        let atom;
        if let Some(rest) = expression.strip_prefix("shard:") {
            let Some((shard, rest)) = rest.split_once(':') else {
                return Err(format!("unsupported shard filter: {value}"));
            };
            if !["rsid", "rsid-store"].contains(&package) || !name(shard) {
                return Err(format!("unsupported shard filter: {value}"));
            }
            atom = rest.strip_prefix("test(").and_then(|s| s.strip_suffix(')'));
            if atom.is_none_or(str::is_empty) {
                return Err(format!("unsupported shard filter: {value}"));
            }
            owners = shards
                .get(shard)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("unknown shard: {shard}"))?
                .clone();
        } else if expression.starts_with("bin:") || expression.starts_with("test:") {
            let (kind, rest) = expression
                .split_once(':')
                .ok_or_else(|| format!("invalid test target: {value}"))?;
            selection.kind = kind.into();
            selection.substring.clear();
            if let Some((target, rest)) = rest.split_once(':') {
                selection.target = target.into();
                atom = rest.strip_prefix("test(").and_then(|s| s.strip_suffix(')'));
                if atom.is_none_or(str::is_empty) {
                    return Err(format!("invalid test target: {value}"));
                }
            } else {
                selection.target = rest.into();
                atom = None;
            }
            if !name(&selection.target) {
                return Err(format!("invalid test target: {value}"));
            }
        } else if expression.starts_with("test(") {
            atom = expression
                .strip_prefix("test(")
                .and_then(|s| s.strip_suffix(')'));
            if atom.is_none_or(str::is_empty) {
                return Err(format!("invalid library test atom: {value}"));
            }
        } else {
            if !test_name(expression) {
                return Err(format!("unsupported filter: {value}"));
            }
            atom = None;
        }
        if let Some(atom) = atom {
            if test_name(atom) {
                selection.substring = atom.into();
            } else if let Some(exact) = atom.strip_prefix('=').filter(|s| test_name(s)) {
                selection.exact = Some(exact.into());
            } else if let Some(pattern) = atom
                .strip_prefix('/')
                .and_then(|s| s.strip_suffix('/'))
                .filter(|s| !s.is_empty())
            {
                if !pattern
                    .chars()
                    .all(|c| c.is_alphanumeric() || "_:^$|()?.*+[]\\-".contains(c))
                {
                    return Err(format!("unsupported test atom: {value}"));
                }
                Regex::new(pattern).map_err(|e| format!("invalid regex in {value}: {e}"))?;
                selection.regex = Some(pattern.into());
            } else {
                return Err(format!("unsupported test atom: {value}"));
            }
        }
        for owner in owners {
            plan.entry(owner).or_default().push(selection.clone());
        }
    }
    Ok(plan)
}

pub fn build_command(
    cargo: &[String],
    package: &str,
    selections: &[Selection],
    features: &[String],
    all_tests: bool,
) -> Vec<String> {
    let mut command = cargo.to_vec();
    command.extend(["test".into(), "-p".into(), package.into()]);
    if all_tests {
        command.push("--tests".into());
    }
    if selections.iter().any(|s| s.kind == "lib") {
        command.push("--lib".into());
    }
    let targets: BTreeSet<_> = selections
        .iter()
        .filter(|s| s.kind != "lib")
        .map(|s| (&s.kind, &s.target))
        .collect();
    for (kind, target) in targets {
        command.extend([format!("--{kind}"), target.clone()]);
    }
    if !features.is_empty() {
        command.extend(["--features".into(), features.join(",")]);
    }
    command.extend(["--no-run".into(), "--message-format=json".into()]);
    command
}

pub fn batches(names: &[String], max_tests: usize, max_bytes: usize) -> Vec<Vec<String>> {
    let mut result = Vec::new();
    let mut batch = Vec::new();
    let mut bytes = 0;
    for name in names {
        if !batch.is_empty() && (batch.len() >= max_tests || bytes + name.len() + 1 > max_bytes) {
            result.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        batch.push(name.clone());
        bytes += name.len() + 1;
    }
    if !batch.is_empty() {
        result.push(batch);
    }
    result
}

pub type Rank = (bool, bool, usize, usize, usize);
pub fn ranked_tiers(
    selected: &BTreeSet<String>,
    sizes: &BTreeMap<String, usize>,
    priority: &BTreeSet<String>,
    changed: &BTreeMap<String, usize>,
    direct: &BTreeMap<String, usize>,
) -> BTreeMap<Rank, Vec<String>> {
    let mut tiers = BTreeMap::<Rank, Vec<String>>::new();
    for name in selected {
        let size = sizes.get(name).copied().unwrap_or(1);
        let rank = (
            !priority.contains(name),
            !changed.contains_key(name),
            changed.get(name).copied().unwrap_or(0),
            direct.get(name).copied().unwrap_or(1 << 30),
            usize::from(size > 24) + usize::from(size > 60),
        );
        tiers.entry(rank).or_default().push(name.clone());
    }
    tiers
}

#[derive(Default, Debug, Serialize)]
pub struct Coverage {
    pub selected: usize,
    pub run: Vec<String>,
    pub partial: Vec<serde_json::Value>,
    pub not_run: Vec<String>,
    pub unlisted_packages: Vec<String>,
    /// Cap-overflow filters: how many of their matches were kept, run and deferred (#2592).
    pub bounded: Vec<serde_json::Value>,
    /// Exact listed test identities no plan filter or rank kept.
    pub deferred_tests: usize,
}
/// Coverage is established from listed binary-scoped identities, never from a
/// raw count of stdout markers or a filter having been planned.
pub fn coverage(
    plan: &Plan,
    listed: &BTreeSet<String>,
    counts: &BTreeMap<String, (usize, usize)>,
    kept: &BTreeMap<String, [usize; 3]>,
    deferred_tests: usize,
) -> Coverage {
    let mut carriers = BTreeMap::<String, BTreeSet<String>>::new();
    let mut bounded_carriers = BTreeSet::<String>::new();
    for (package, selections) in plan {
        for s in selections.iter().filter(|s| !s.direct_only) {
            if s.bounded {
                bounded_carriers.insert(s.filter.clone());
            } else {
                carriers
                    .entry(s.filter.clone())
                    .or_default()
                    .insert(package.clone());
            }
        }
    }
    let mut result = Coverage {
        selected: carriers.len(),
        unlisted_packages: plan
            .keys()
            .filter(|p| !listed.contains(*p))
            .cloned()
            .collect(),
        deferred_tests,
        bounded: bounded_carriers
            .into_iter()
            .map(|filter| {
                let [kept, executed, deferred] = kept.get(&filter).copied().unwrap_or_default();
                serde_json::json!({"filter": filter, "kept": kept, "executed": executed, "deferred": deferred})
            })
            .collect(),
        ..Coverage::default()
    };
    for (filter, packages) in carriers {
        let (selected, executed) = counts.get(&filter).copied().unwrap_or_default();
        if (executed > 0 && executed == selected) || (selected == 0 && packages.is_subset(listed)) {
            result.run.push(filter);
        } else if executed > 0 {
            result
                .partial
                .push(serde_json::json!({"filter":filter,"selected":selected,"executed":executed}));
        } else {
            result.not_run.push(filter);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn selector_grammar_preserves_targets_atoms_and_shard_owners() {
        let shards = BTreeMap::from([("store".into(), strings(&["rsid-store"]))]);
        let filters = strings(&[
            "rsid=shard:store:test(=tests::one)",
            "rsi=bin:rsi:test(/^tests::(one|two)$/)",
            "rsi=test:e2e_tui",
            "rsi=test(foo)",
            "rsi=tests::bar",
        ]);
        let plan = selection_plan(&filters, &shards, &strings(&["rsi=test(foo)"])).unwrap();
        assert!(plan["rsid-store"][0].matches("tests::one").unwrap());
        assert!(plan["rsi"][0].matches("tests::two").unwrap());
        assert!(plan["rsi"][1].for_target(&strings(&["test"]), "e2e_tui"));
        assert!(plan["rsi"][2].priority);
        assert_eq!(plan["rsi"].len(), 4);
    }
    #[test]
    fn unsafe_or_unscoped_filters_fail_closed() {
        for filter in [
            "rsid",
            "*=foo",
            "rsi=",
            "rsi=*",
            "rsi=bin:../tool",
            "rsi=test(=)",
            "rsi=test(/[/)",
            "rsi=shard:a:test(x)",
            "rsid=shard:unknown:test(x)",
            "rsi=test(foo);shell",
        ] {
            assert!(
                selection_plan(&strings(&[filter]), &BTreeMap::new(), &[]).is_err(),
                "{filter}"
            );
        }
    }
    #[test]
    fn targeted_builds_do_not_widen_and_batches_respect_argument_budget() {
        let plan = selection_plan(
            &strings(&["rsi=bin:a", "rsi=bin:a:test(x)", "rsi=test:b"]),
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
        assert_eq!(
            build_command(
                &strings(&["cargo"]),
                "rsi",
                &plan["rsi"],
                &strings(&["fixture"]),
                false
            ),
            strings(&[
                "cargo",
                "test",
                "-p",
                "rsi",
                "--bin",
                "a",
                "--test",
                "b",
                "--features",
                "fixture",
                "--no-run",
                "--message-format=json"
            ])
        );
        assert_eq!(
            batches(&strings(&["a", "bb", "ccc", "d"]), 2, 5),
            vec![strings(&["a", "bb"]), strings(&["ccc"]), strings(&["d"])]
        );
    }
    #[test]
    fn priorities_changed_modules_and_direct_observers_precede_broad_tests() {
        let selected = BTreeSet::from_iter(strings(&["added", "changed", "direct", "far"]));
        let tiers = ranked_tiers(
            &selected,
            &BTreeMap::new(),
            &BTreeSet::from(["added".into()]),
            &BTreeMap::from([("changed".into(), 2)]),
            &BTreeMap::from([("direct".into(), 1)]),
        );
        assert_eq!(
            tiers.into_values().flatten().collect::<Vec<_>>(),
            strings(&["added", "changed", "direct", "far"])
        );
    }
    #[test]
    fn bounded_filters_report_kept_run_and_deferred_apart_from_run_filters() {
        let mut plan =
            selection_plan(&strings(&["rsi=a", "rsi=b"]), &BTreeMap::new(), &[]).unwrap();
        for selection in plan.get_mut("rsi").unwrap() {
            if selection.filter == "rsi=b" {
                selection.bounded = true;
            }
        }
        let coverage = coverage(
            &plan,
            &BTreeSet::from(["rsi".into()]),
            &BTreeMap::from([("rsi=a".into(), (1, 1))]),
            &BTreeMap::from([("rsi=b".into(), [4, 3, 9])]),
            9,
        );
        // A cap-overflow filter is never "run": it keeps only some of its matches.
        assert_eq!(coverage.run, strings(&["rsi=a"]));
        assert_eq!(
            coverage.bounded,
            vec![serde_json::json!({"filter":"rsi=b","kept":4,"executed":3,"deferred":9})]
        );
        assert_eq!(coverage.deferred_tests, 9);
    }
    #[test]
    fn coverage_requires_listings_and_preserves_partial_counts() {
        let plan = selection_plan(
            &strings(&["rsi=a", "rsi=b", "rsi=c", "rsid=d"]),
            &BTreeMap::new(),
            &[],
        )
        .unwrap();
        let coverage = coverage(
            &plan,
            &BTreeSet::from(["rsi".into()]),
            &BTreeMap::from([("rsi=a".into(), (2, 2)), ("rsi=b".into(), (3, 1))]),
            &BTreeMap::new(),
            0,
        );
        assert_eq!(coverage.run, strings(&["rsi=a", "rsi=c"]));
        assert_eq!(
            coverage.partial,
            vec![serde_json::json!({"filter":"rsi=b","selected":3,"executed":1})]
        );
        assert_eq!(coverage.not_run, strings(&["rsid=d"]));
        assert_eq!(coverage.unlisted_packages, strings(&["rsid"]));
    }
}
