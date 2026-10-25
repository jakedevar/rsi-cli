//! Bounded observer walk from already-proven seeds. Diff classification and
//! seed proofs (RPC arms, fields, relocation, include readers) remain separate;
//! this API must not be used to guess a seed for an unclassified edit.
use super::{
    filters::{self, Observer},
    identity::{ImplKind, Item, ItemReferences},
    seeds::{self, Proof},
    source,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct Seeds {
    pub changed: BTreeSet<String>,
    pub tokens: BTreeSet<String>,
    pub mounted: BTreeSet<String>,
    /// Types uniquely resolved across the old/new source proof. Merely seeing
    /// a capitalized word is insufficient to treat a common method as typed.
    pub types: BTreeSet<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Stop {
    NoSeeds,
    UnresolvedIdentity,
    FunctionBound,
    TestBound,
    Hub,
}
#[derive(Clone, Debug)]
pub struct Trace {
    /// Some only when the walk proves complete. Empty is a valid settled trace.
    pub tests: Option<Vec<Observer>>,
    /// Settled direct observations remain priorities when tests is None; the
    /// broad fallback still supplies coverage. Distances are nearest first.
    pub direct: BTreeMap<Observer, usize>,
    pub hubs: BTreeMap<String, usize>,
    pub layers: usize,
    pub stop: Option<Stop>,
    pub diagnostic: Option<String>,
}
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub functions: usize,
    pub tests: usize,
    pub large_function: usize,
    pub hub: usize,
}
impl Default for Bounds {
    fn default() -> Self {
        Self {
            functions: filters::NARROW_FUNCTION_CAP,
            tests: filters::NARROW_TEST_CAP,
            large_function: filters::LARGE_FUNCTION_LINES,
            hub: filters::NARROW_FUNCTION_CAP,
        }
    }
}

fn subtree(module: &str, parent: &str) -> bool {
    module == parent || module.starts_with(&format!("{parent}::"))
}
fn local_observer(module: &str, words: &BTreeSet<String>, qualified: &BTreeSet<Item>) -> bool {
    qualified
        .iter()
        .any(|(owner, name)| words.contains(name) && subtree(module, owner))
}

/// Stop before accepting an overflowing layer; preserve every earlier layer's
/// proved observer and report why broad module coverage must remain. Limits
/// count test identities once even when the test compiles in multiple shards.
///
/// This is the generic seeded walk. Private relocation/initializer exceptions,
/// leaf facade tracing and RPC-dispatch specialization are not supplied here.
/// Callers must retain broad coverage for those edits until their proofs exist.
/// Use `trace_proof` for proven private fields and relocation test methods.
pub fn trace_tokens(index: &mut ItemReferences, seeds: &Seeds, bounds: Bounds) -> Trace {
    trace_with_words(index, seeds, bounds, None, &BTreeMap::new(), false)
}

/// Apply hunk proof obligations before walking. Keeping the word view local
/// prevents one field edit from changing another edit's observer inventory.
/// An error requires broad coverage, just like an unproved hunk seed.
pub fn trace_proof(
    index: &mut ItemReferences,
    proof: &Proof,
    bounds: Bounds,
) -> Result<Trace, String> {
    if proof.leaf && !proof.seeds.mounted.is_empty() {
        return Err("a leaf module declaration cannot be traced to the daemon tests".into());
    }
    let words = seeds::observer_words(index, proof)?;
    let mut test_methods = BTreeMap::new();
    for f in &index.functions {
        if f.function.attrs.contains("#[cfg(test)]") {
            if let Some(owner) = f
                .owner
                .as_ref()
                .and_then(|o| o.name.as_ref())
                .filter(|o| proof.moved.contains(*o))
            {
                if test_methods
                    .insert(f.function.name.clone(), owner.clone())
                    .is_some_and(|prior| prior != *owner)
                {
                    return Err("relocated test method has ambiguous concrete receivers".into());
                }
            }
        }
    }
    Ok(trace_with_words(
        index,
        &proof.seeds,
        bounds,
        Some(&words),
        &test_methods,
        proof.leaf,
    ))
}

fn trace_with_words(
    index: &mut ItemReferences,
    seeds: &Seeds,
    bounds: Bounds,
    words: Option<&[BTreeSet<String>]>,
    test_methods: &BTreeMap<String, String>,
    follow_large: bool,
) -> Trace {
    let mut reached: BTreeSet<String> = seeds.changed.union(&seeds.tokens).cloned().collect();
    let mut distance: BTreeMap<String, usize> = reached.iter().cloned().map(|s| (s, 0)).collect();
    let mut qualified = BTreeSet::new();
    let mut identified = BTreeSet::new();
    let mut identified_callers = BTreeSet::new();
    let mut typed_tests = BTreeSet::new();
    let mut result = Trace {
        tests: None,
        direct: BTreeMap::new(),
        hubs: BTreeMap::new(),
        layers: 0,
        stop: None,
        diagnostic: None,
    };
    if reached.is_empty() && seeds.mounted.is_empty() {
        result.stop = Some(Stop::NoSeeds);
        result.diagnostic = Some("no executable trace seeds were resolved".into());
        return result;
    }
    let mut definitions = BTreeMap::<String, BTreeSet<String>>::new();
    for f in &index.functions {
        if !source::is_test(&f.function.attrs) {
            definitions
                .entry(f.function.name.clone())
                .or_default()
                .insert(f.module.clone());
        }
    }
    loop {
        for (name, owner) in test_methods {
            if reached.remove(name) {
                typed_tests.insert(owner.clone());
            }
        }
        let mut callers = BTreeSet::new();
        let mut local_callers = BTreeSet::new();
        let mut resolved = BTreeMap::new();
        let mut stopped = None;
        // Resolve after the immutable inventory scan to keep the index cache
        // available across every source without cloning/scanning the tree again.
        let mut needs_identity = BTreeSet::new();
        for (i, f) in index.functions.iter().enumerate() {
            let words = words.map_or(&f.words, |w| &w[i]);
            let is_test = source::is_test(&f.function.attrs);
            let observes = !words.is_disjoint(&reached)
                || local_observer(&f.module, words, &qualified)
                || identified_callers.contains(&(f.module.clone(), f.function.name.clone()))
                || f.test_helper && !words.is_disjoint(&typed_tests);
            if is_test
                || f.function.end - f.function.start > bounds.large_function
                    && !f.test_helper
                    && !follow_large
                || reached.contains(&f.function.name)
                || test_methods.contains_key(&f.function.name)
                || !observes
            {
                continue;
            }
            let common = definitions
                .get(&f.function.name)
                .is_some_and(|m| m.len() > 1);
            let owner = f
                .owner
                .as_ref()
                .filter(|o| o.kind == ImplKind::Inherent)
                .and_then(|o| o.name.as_ref())
                .filter(|name| seeds.types.contains(*name));
            if common && owner.is_none() {
                if f.function.body.trim_start().starts_with("pub") {
                    let key = (f.module.clone(), f.function.name.clone());
                    if !identified.contains(&key) {
                        needs_identity.insert(key);
                    }
                } else {
                    local_callers.insert((f.module.clone(), f.function.name.clone()));
                }
                continue;
            }
            if let Some(owner) = owner.filter(|_| common) {
                if index.frequency.get(owner).copied().unwrap_or(0) > bounds.hub {
                    result.hubs.insert(owner.clone(), index.frequency[owner]);
                    if index.frequency.get(&f.function.name).copied().unwrap_or(0) > bounds.hub {
                        result
                            .hubs
                            .insert(f.function.name.clone(), index.frequency[&f.function.name]);
                        continue;
                    }
                    callers.insert(f.function.name.clone());
                } else {
                    callers.insert(owner.clone());
                }
            } else {
                callers.insert(f.function.name.clone());
            }
        }
        for (owner, name) in needs_identity {
            let refs = index.callers(&owner, &name);
            if let Some(reason) = refs.unresolved {
                stopped = Some((
                    Stop::UnresolvedIdentity,
                    format!("ambiguous exported helper: {owner}::{name} ({reason})"),
                ));
                break;
            }
            resolved.insert((owner, name), refs.functions);
        }
        if let Some((stop, diagnostic)) = stopped {
            result.stop = Some(stop);
            result.diagnostic = Some(diagnostic);
            break;
        }
        callers.retain(|name| !reached.contains(name));
        local_callers.retain(|item| !qualified.contains(item));
        for (name, owner) in test_methods {
            if index.functions.iter().enumerate().any(|(i, f)| {
                let words = words.map_or(&f.words, |w| &w[i]);
                f.function.name == *name
                    && !source::is_test(&f.function.attrs)
                    && !words.is_disjoint(&reached)
            }) {
                typed_tests.insert(owner.clone());
            }
        }
        if callers.is_empty() && local_callers.is_empty() && resolved.is_empty() {
            break;
        }
        if reached.len()
            + callers.len()
            + qualified.len()
            + local_callers.len()
            + identified.len()
            + resolved.len()
            > bounds.functions
        {
            result.stop = Some(Stop::FunctionBound);
            result.diagnostic = Some(format!(
                "observer trace exceeds {} functions",
                bounds.functions
            ));
            break;
        }
        result.layers += 1;
        for name in callers {
            distance.insert(name.clone(), result.layers);
            reached.insert(name);
        }
        qualified.extend(local_callers);
        identified.extend(resolved.keys().cloned());
        identified_callers.extend(resolved.into_values().flatten());
    }
    for (i, f) in index.functions.iter().enumerate() {
        let words = words.map_or(&f.words, |w| &w[i]);
        if !source::is_test(&f.function.attrs) {
            continue;
        }
        let mut hops = words
            .intersection(&reached)
            .map(|word| distance[word] + 1)
            .min();
        if seeds.changed.contains(&f.function.name) || !words.is_disjoint(&seeds.tokens) {
            hops = Some(1);
        }
        if hops.is_none()
            && (local_observer(&f.module, words, &qualified)
                || !words.is_disjoint(&typed_tests)
                || identified_callers.contains(&(f.module.clone(), f.function.name.clone()))
                || seeds
                    .mounted
                    .iter()
                    .any(|module| subtree(&f.module, module)))
        {
            hops = Some(result.layers + 1);
        }
        if let Some(hops) = hops {
            for shard in source::shards_in(&f.function.attrs) {
                let test = Observer {
                    module: f.module.clone(),
                    name: f.function.name.clone(),
                    shard,
                };
                result
                    .direct
                    .entry(test)
                    .and_modify(|n| *n = (*n).min(hops))
                    .or_insert(hops);
            }
        }
    }
    if result.stop.is_none() && !result.hubs.is_empty() {
        result.stop = Some(Stop::Hub);
        let names = result
            .hubs
            .iter()
            .map(|(token, count)| format!("{token} ({count} functions)"))
            .collect::<Vec<_>>()
            .join(", ");
        result.diagnostic = Some(format!(
            "observer trace stops at high-fan-in {names}: its direct observers are named, its far observers are left to the module filter"
        ));
    }
    let count = result
        .direct
        .keys()
        .map(|o| (&o.module, &o.name))
        .collect::<BTreeSet<_>>()
        .len();
    if result.stop.is_none() && count > bounds.tests {
        result.stop = Some(Stop::TestBound);
        result.diagnostic = Some(format!(
            "observer trace selects {count} tests, exceeding the {}-test bound",
            bounds.tests
        ));
    }
    if result.stop.is_none() {
        result.tests = Some(result.direct.keys().cloned().collect());
    }
    result
}
