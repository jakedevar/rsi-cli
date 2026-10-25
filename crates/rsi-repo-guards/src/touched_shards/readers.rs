//! Pinned source reads. Lexical include edges never come from quoted examples.
use super::{
    filters::{BROAD_TEST_LIMIT, Observer},
    identity::{Item, ItemReferences},
    source::{self, SourceTree},
};
use crate::re::{Re, group, start};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Include {
    pub offset: usize,
    pub target: String,
}

pub const INCLUDE_EXTERNAL_CALLERS: usize = 12;

pub fn code_includes(text: &str) -> Result<Vec<Include>, String> {
    let code = source::code_text(text)?;
    Ok(Re::new(r#"\binclude_(?:str|bytes)!\(\s*"([^"]+)"\s*\)"#)
        .find_all(text)
        .into_iter()
        .filter(|c| code.get(start(c)..start(c) + 8) == Some("include_"))
        .map(|c| Include {
            offset: start(&c),
            target: group(&c, 1).into(),
        })
        .collect())
}

fn below(module: &str, parent: &str) -> bool {
    module == parent || module.starts_with(&format!("{parent}::"))
}

/// Source scanners observe the read source even when their helper is large.
/// File-level named constants lead to readers of that binding; an unnamed
/// include conservatively selects the containing module's tests.
pub fn source_scanner_tests(
    sources: &SourceTree,
    index: &ItemReferences,
    changed: &BTreeSet<String>,
) -> Result<Vec<Observer>, String> {
    let mut helpers = BTreeSet::<Item>::new();
    let mut modules = BTreeSet::new();
    for (path, text) in &sources.texts {
        if !source::lib_source(path) {
            continue;
        }
        let Some(module) = sources.module(path) else {
            continue;
        };
        if source::file_gated_off(text, &sources.host) {
            continue;
        }
        let functions: Vec<_> = index.functions.iter().filter(|f| f.path == *path).collect();
        let code = source::code_text(text)?;
        for include in code_includes(text)? {
            let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
            let target = source::normalize(&format!("{parent}/{}", include.target));
            if !changed.contains(&target) {
                continue;
            }
            let line = text[..include.offset]
                .bytes()
                .filter(|b| *b == b'\n')
                .count();
            if let Some(f) = functions
                .iter()
                .filter(|f| f.function.start <= line && line < f.function.end)
                .min_by_key(|f| f.function.end - f.function.start)
            {
                helpers.insert((module.clone(), f.function.name.clone()));
            } else if let Some(c) = Re::new(r"\b(?:const|static)\s+(\w+)\s*:[^=\n]*=\s*$")
                .search(&code[..include.offset])
            {
                for f in &functions {
                    if f.words.contains(group(&c, 1)) {
                        helpers.insert((module.clone(), f.function.name.clone()));
                    }
                }
            } else {
                modules.insert(module.clone());
            }
        }
    }
    let mut definitions = BTreeMap::<&str, BTreeSet<&str>>::new();
    for f in &index.functions {
        if !source::is_test(&f.function.attrs) {
            definitions
                .entry(&f.function.name)
                .or_default()
                .insert(&f.module);
        }
    }
    let reads = |module: &str, words: &BTreeSet<String>, helpers: &BTreeSet<Item>| {
        helpers.iter().any(|(owner, name)| {
            words.contains(name)
                && (below(module, owner)
                    || definitions
                        .get(name.as_str())
                        .is_some_and(|defs| defs.len() == 1))
        })
    };
    loop {
        let callers: BTreeSet<_> = index
            .functions
            .iter()
            .filter(|f| !source::is_test(&f.function.attrs) && reads(&f.module, &f.words, &helpers))
            .map(|f| (f.module.clone(), f.function.name.clone()))
            .collect();
        if callers.is_subset(&helpers) {
            break;
        }
        helpers.extend(callers);
    }
    let mut picked = BTreeSet::new();
    for f in &index.functions {
        if source::is_test(&f.function.attrs)
            && (helpers.contains(&(f.module.clone(), f.function.name.clone()))
                || reads(&f.module, &f.words, &helpers)
                || modules.iter().any(|m| below(&f.module, m)))
        {
            for shard in source::shards_in(&f.function.attrs) {
                picked.insert(Observer {
                    module: f.module.clone(),
                    name: f.function.name.clone(),
                    shard,
                });
            }
        }
    }
    Ok(picked.into_iter().collect())
}

/// A data file under src/ is observed through the const/static/function that
/// includes it. Follow internal helpers to closure and only one layer of
/// external callers: a central wrapper must not fan out to the whole daemon.
/// Err means the existing broader selection is still required.
pub fn include_item_tests(
    sources: &SourceTree,
    index: &ItemReferences,
    changed: &BTreeSet<String>,
) -> Result<Vec<Observer>, String> {
    let data: BTreeSet<_> = changed
        .iter()
        .filter(|p| !p.ends_with(".rs") && Re::new(r"^crates/[^/]+/src/").is_match(p))
        .cloned()
        .collect();
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut seeds = BTreeMap::<String, BTreeSet<String>>::new();
    let mut owned = BTreeSet::new();
    for (path, text) in &sources.texts {
        if !text.contains("include_") {
            continue;
        }
        for include in code_includes(text)? {
            let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
            let target = source::normalize(&format!("{parent}/{}", include.target));
            if !data.contains(&target) {
                continue;
            }
            let module = sources
                .module(path)
                .filter(|_| source::lib_source(path))
                .ok_or("included data has a reader outside traced library modules")?;
            let line = text[..include.offset]
                .bytes()
                .filter(|b| *b == b'\n')
                .count();
            let function = index
                .functions
                .iter()
                .filter(|f| f.path == *path && f.function.start <= line && line < f.function.end)
                .min_by_key(|f| f.function.end - f.function.start);
            let name = if let Some(f) = function {
                f.function.name.clone()
            } else {
                let code = source::code_text(text)?;
                let c = Re::new(r"\b(?:const|static)\s+(\w+)\s*:[^=;]*=\s*$")
                    .search(&code[..include.offset])
                    .ok_or("included data has no named owning item")?;
                group(&c, 1).into()
            };
            seeds.entry(module).or_default().insert(name);
            owned.insert(target);
        }
    }
    if owned != data {
        return Err("changed data has no complete include-item ownership proof".into());
    }
    for (path, text) in &sources.texts {
        for target in &data {
            let base = target.rsplit('/').next().unwrap_or(target);
            if text.contains(base) {
                let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
                if !code_includes(text)?
                    .iter()
                    .any(|i| source::normalize(&format!("{parent}/{}", i.target)) == *target)
                {
                    return Err("included data has another literal reader".into());
                }
            }
        }
    }
    let mut names = BTreeSet::new();
    for (module, seed) in &seeds {
        let mut reached = seed.clone();
        loop {
            let callers: BTreeSet<_> = index
                .functions
                .iter()
                .filter(|f| {
                    !source::is_test(&f.function.attrs)
                        && below(&f.module, module)
                        && !reached.contains(&f.function.name)
                        && !f.words.is_disjoint(&reached)
                })
                .map(|f| f.function.name.clone())
                .collect();
            if callers.is_empty() {
                break;
            }
            reached.extend(callers);
        }
        let outside: BTreeSet<_> = index
            .functions
            .iter()
            .filter(|f| {
                !source::is_test(&f.function.attrs)
                    && !below(&f.module, module)
                    && !reached.contains(&f.function.name)
                    && !f.words.is_disjoint(&reached)
            })
            .map(|f| f.function.name.clone())
            .collect();
        if outside.len() > INCLUDE_EXTERNAL_CALLERS {
            return Err(format!(
                "included data exceeds the {INCLUDE_EXTERNAL_CALLERS}-external-caller bound"
            ));
        }
        names.extend(reached);
        names.extend(outside);
    }
    let mut picked = BTreeSet::new();
    for f in &index.functions {
        if source::is_test(&f.function.attrs)
            && (!f.words.is_disjoint(&names)
                || seeds
                    .iter()
                    .any(|(m, seed)| seed.contains(&f.function.name) && below(&f.module, m)))
        {
            for shard in source::shards_in(&f.function.attrs) {
                picked.insert(Observer {
                    module: f.module.clone(),
                    name: f.function.name.clone(),
                    shard,
                });
            }
        }
    }
    if picked
        .iter()
        .map(|o| (&o.module, &o.name))
        .collect::<BTreeSet<_>>()
        .len()
        > BROAD_TEST_LIMIT
    {
        return Err(format!(
            "included data exceeds the {BROAD_TEST_LIMIT}-test bound"
        ));
    }
    Ok(picked.into_iter().collect())
}
