//! Filter wire grammar, coverage containment and observer distance ranking.
use crate::re::{Re, group};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

pub const GATE_FILTER_LENGTH: usize = 256;
pub const GATE_FILTER_LIMIT: usize = 12;
pub const BROAD_TEST_LIMIT: usize = 60;
pub const NARROW_TEST_CAP: usize = 24;
pub const NARROW_FUNCTION_CAP: usize = 150;
pub const REEXPORT_DEPTH: usize = 6;
pub const LARGE_FUNCTION_LINES: usize = 60;
pub const RARE_TOKEN_FUNCTIONS: usize = 10;
pub const WORKER_BOUNDED_STOPS: &[&str] = &["function bound", "test bound", "hub"];

static SHARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"^rsid=shard:([a-z]+-\d+):(.+)$"));
static SIMPLE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^test\(([A-Za-z0-9_:]+)\)$"));
static ANCHORED: LazyLock<Re> = LazyLock::new(|| Re::new(r"^test\(/\^([A-Za-z0-9_:]+)::/\)$"));
static PREFIX: LazyLock<Re> = LazyLock::new(|| Re::new(r"^test\(/\^([A-Za-z0-9_:]+)"));

pub fn direct_filterset(module: &str) -> String {
    format!("test(/^{module}::[A-Za-z0-9_]+$/)")
}
pub fn anchored_filterset(module: &str) -> String {
    format!("test(/^{module}::/)")
}
pub fn named_filterset(module: &str, names: &[String]) -> String {
    let names: BTreeSet<_> = names.iter().collect();
    let alternatives = if names.len() == 1 {
        names.first().map_or(String::new(), |n| (*n).clone())
    } else {
        format!(
            "(?:{})",
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join("|")
        )
    };
    format!("test(/^{module}::(?:\\w+::)*{alternatives}$/)")
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub struct Observer {
    pub module: String,
    pub name: String,
    pub shard: String,
}

pub fn traced_filters(picked: &[Observer]) -> Vec<String> {
    let mut grouped: BTreeMap<(&str, &str), BTreeSet<String>> = BTreeMap::new();
    for test in picked {
        grouped
            .entry((&test.shard, &test.module))
            .or_default()
            .insert(test.name.clone());
    }
    let mut filters = Vec::new();
    for ((shard, module), names) in grouped {
        let prefix = format!("rsid=shard:{shard}:");
        let mut chunk = Vec::new();
        for name in names {
            let mut next = chunk.clone();
            next.push(name.clone());
            if !chunk.is_empty()
                && prefix.len() + named_filterset(module, &next).len() > GATE_FILTER_LENGTH
            {
                filters.push(format!("{prefix}{}", named_filterset(module, &chunk)));
                chunk.clear();
            }
            chunk.push(name);
        }
        filters.push(format!("{prefix}{}", named_filterset(module, &chunk)));
    }
    filters
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RankedFilter {
    pub filter: String,
    pub distance: usize,
}
pub fn direct_observer_filters(found: &BTreeMap<Observer, usize>) -> Vec<RankedFilter> {
    let mut by_distance: BTreeMap<usize, Vec<Observer>> = BTreeMap::new();
    for (observer, distance) in found {
        by_distance
            .entry(*distance)
            .or_default()
            .push(observer.clone());
    }
    by_distance
        .into_iter()
        .flat_map(|(distance, tests)| {
            traced_filters(&tests)
                .into_iter()
                .map(move |filter| RankedFilter { filter, distance })
        })
        .collect()
}
pub fn direct_observer_summary(found: &BTreeMap<Observer, usize>) -> serde_json::Value {
    let mut nearest: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (o, distance) in found {
        nearest
            .entry((&o.module, &o.name))
            .and_modify(|d| *d = (*d).min(*distance))
            .or_insert(*distance);
    }
    let mut counts = BTreeMap::new();
    for distance in nearest.values() {
        *counts.entry(distance.to_string()).or_insert(0usize) += 1;
    }
    serde_json::json!({"tests":nearest.len(),"by_distance":counts})
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergedFilter {
    pub filter: String,
    pub covered_by: String,
    pub reason: String,
}
fn unique(filters: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    filters
        .iter()
        .filter(|v| seen.insert((*v).clone()))
        .cloned()
        .collect()
}

/// Only proven same-shard containment; no regex broadening or budget truncation.
pub fn covered_filters(filters: &[String], merged: &mut Vec<MergedFilter>) -> Vec<String> {
    let filters = unique(filters);
    let mut modules = Vec::new();
    for value in &filters {
        if let Some(c) = SHARD.search(value) {
            if let Some(scope) = SIMPLE.search(group(&c, 2)) {
                modules.push((
                    value,
                    group(&c, 1).to_owned(),
                    group(&scope, 1).to_owned(),
                    false,
                ));
            } else if let Some(scope) = ANCHORED.search(group(&c, 2)) {
                modules.push((
                    value,
                    group(&c, 1).to_owned(),
                    group(&scope, 1).to_owned(),
                    true,
                ));
            }
        }
    }
    let mut kept = Vec::new();
    for value in &filters {
        let covering = SHARD.search(value).and_then(|c| {
            let expression = group(&c, 2);
            let scope = PREFIX
                .search(expression)
                .or_else(|| SIMPLE.search(expression))?;
            let scope = group(&scope, 1);
            modules
                .iter()
                .find(|(other, shard, module, anchored)| {
                    *other != value
                        && shard == group(&c, 1)
                        && if *anchored {
                            scope.starts_with(&format!("{module}::"))
                        } else {
                            scope.contains(module)
                        }
                })
                .map(|(v, _, _, _)| (*v).clone())
        });
        if let Some(covered_by) = covering {
            merged.push(MergedFilter {
                filter: value.clone(),
                covered_by,
                reason: "same-shard module filter already selects every matching test".into(),
            });
        } else {
            kept.push(value.clone());
        }
    }
    kept
}

pub fn unsharded_filters(
    filters: &[String],
    shard_packages: &BTreeMap<String, Vec<String>>,
    merged: &mut Vec<MergedFilter>,
) -> Vec<String> {
    let mut seen = BTreeMap::new();
    let mut kept = Vec::new();
    for value in unique(filters) {
        let key = if let Some(c) = SHARD.search(&value) {
            let mut packages = shard_packages
                .get(group(&c, 1))
                .cloned()
                .unwrap_or_else(|| vec!["rsid".into()]);
            packages.sort();
            (packages, group(&c, 2).to_owned())
        } else {
            (Vec::new(), value.clone())
        };
        if let Some(covered_by) = seen.get(&key) {
            merged.push(MergedFilter {
                filter: value,
                covered_by: String::clone(covered_by),
                reason: "identical expression selects the same unsharded library packages".into(),
            });
        } else {
            seen.insert(key, value.clone());
            kept.push(value);
        }
    }
    kept
}
