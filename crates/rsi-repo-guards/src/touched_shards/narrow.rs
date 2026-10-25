//! `narrow_tests`: the tests that can observe the diff of one source, or `None`
//! (uncertain) with the reason and the nearest proven observers. This joins the
//! independently proven pieces: RPC registration arms, hunk seed proofs
//! (including private relocation and initializers) and the bounded observer walk.
//! An uncertain trace never narrows coverage; its direct observers are ordering
//! hints for the broad fallback.
use super::{
    filters::{BROAD_TEST_LIMIT, NARROW_TEST_CAP, Observer},
    git::Hunk,
    identity::ItemReferences,
    rpc, seeds,
    source::SourceTree,
    trace::{self, Bounds, Stop, Trace},
};
use crate::re::Re;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The tests a stopped (or complete) trace proved, nearest first, with the hubs
/// it did not follow. Hints for the broad fallback, which stays the coverage.
#[derive(Clone, Debug, Default)]
pub struct Direct {
    pub tests: BTreeMap<Observer, usize>,
    pub hubs: BTreeMap<String, usize>,
    pub layers: usize,
    pub reason: Option<String>,
}

impl Direct {
    /// The part of a stopped trace a broad filter's receipt entry reports (#2588).
    pub fn summary(&self) -> Vec<(&'static str, Value)> {
        if self.tests.is_empty() {
            return Vec::new();
        }
        let tests: BTreeSet<_> = self.tests.keys().map(|o| (&o.module, &o.name)).collect();
        vec![
            ("direct_tests", json!(tests.len())),
            ("trace_layers", json!(self.layers)),
            ("trace_stopped", json!(self.reason)),
            (
                "hubs",
                Value::Array(
                    self.hubs
                        .iter()
                        .map(|(token, functions)| json!({"token": token, "functions": functions}))
                        .collect(),
                ),
            ),
        ]
    }
}

#[derive(Clone, Debug, Default)]
pub struct Narrow {
    /// `Some` only when the trace is complete; `Some([])` is settled with no
    /// runtime observer, distinct from `None` (unresolved).
    pub picked: Option<Vec<Observer>>,
    pub diagnostics: Vec<String>,
    pub direct: Direct,
}

fn stop_reason(stop: &Stop, diagnostic: Option<&str>) -> &'static str {
    match stop {
        Stop::FunctionBound => "function bound",
        Stop::TestBound => "test bound",
        Stop::Hub => "hub",
        Stop::NoSeeds => "no seeds",
        Stop::UnresolvedIdentity => {
            if diagnostic.is_some_and(|d| d.starts_with("ambiguous receiver")) {
                "unresolved receiver"
            } else {
                "unresolved helper identity"
            }
        }
    }
}

fn from_trace(trace: Trace) -> Narrow {
    let mut result = Narrow::default();
    let uncertain = trace.tests.is_none();
    if uncertain {
        if let Some(d) = &trace.diagnostic {
            result.diagnostics.push(d.clone());
        }
        // A stopped trace hands its settled layers to the broad fallback; a
        // complete one has nothing further to hint.
        if let Some(stop) = &trace.stop {
            result.direct = Direct {
                tests: trace.direct.clone(),
                hubs: trace.hubs.clone(),
                layers: trace.layers,
                reason: Some(stop_reason(stop, trace.diagnostic.as_deref()).to_owned()),
            };
        }
    }
    result.picked = trace.tests;
    result
}

/// The enum names declared by either side that resolve to one declaration: a
/// complete named trace of a private enum may reach the module gate's threshold.
fn resolved_enums(
    sources: &SourceTree,
    old: &SourceTree,
    path: &str,
    types: &BTreeSet<String>,
) -> BTreeSet<String> {
    let pattern = Re::new(r"\benum\s+(\w+)");
    let mut enums = BTreeSet::new();
    for tree in [old, sources] {
        if let Some(text) = tree.texts.get(path) {
            enums.extend(pattern.findall1(text));
        }
    }
    enums.intersection(types).cloned().collect()
}

pub fn narrow_tests(
    sources: &SourceTree,
    old: &SourceTree,
    index: &mut ItemReferences,
    path: &str,
    hunks: &[Hunk],
) -> Narrow {
    let uncertain = |reason: String| Narrow {
        picked: None,
        diagnostics: vec![reason],
        direct: Direct::default(),
    };
    match rpc::changed_dispatch_trace(sources, old, index, path, hunks, Bounds::default()) {
        Ok(Some(trace)) => return from_trace(trace),
        Ok(None) => (),
        Err(reason) => return uncertain(reason),
    }
    let proof = match seeds::changed_seed_proof(sources, old, index, path, hunks) {
        Ok(proof) => proof,
        Err(reason) => return uncertain(reason),
    };
    if proof.seeds.changed.is_empty()
        && proof.seeds.tokens.is_empty()
        && proof.seeds.mounted.is_empty()
    {
        return uncertain("no executable trace seeds were resolved".into());
    }
    // Concrete private types and enums can have users in several modules: keep
    // their complete named trace up to a module gate's threshold.
    let types: BTreeSet<_> = index
        .type_counts
        .iter()
        .filter(|(_, n)| **n == 1)
        .map(|(name, _)| name.clone())
        .collect();
    let enums = resolved_enums(sources, old, path, &types);
    let mut bounds = Bounds::default();
    bounds.tests = if !proof.moved.is_empty() || !proof.seeds.tokens.is_disjoint(&enums) {
        BROAD_TEST_LIMIT
    } else {
        NARROW_TEST_CAP
    };
    match trace::trace_proof(index, &proof, bounds) {
        Ok(trace) => from_trace(trace),
        Err(reason) => uncertain(reason),
    }
}
