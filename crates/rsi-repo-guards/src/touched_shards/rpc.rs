//! Method-to-handler registration proofs. Dispatcher plumbing never becomes a
//! generic caller edge to every RPC family.
use super::{
    filters::Observer,
    git::Hunk,
    identity::{Item, ItemReferences},
    readers, selection,
    source::{self, SourceTree},
    trace::{Bounds, Stop, Trace},
};
use crate::re::{Re, group};
use std::collections::{BTreeMap, BTreeSet};
pub const RPC: &str = "crates/rsid/src/rpc.rs";

fn comments_masked(text: &str) -> Result<String, String> {
    let mut bytes = text.as_bytes().to_vec();
    for lex in source::rust_lexemes(text)? {
        if lex.kind == source::LexemeKind::Comment {
            for b in &mut bytes[lex.start..lex.end] {
                if *b != b'\n' && *b != b'\r' {
                    *b = b' ';
                }
            }
        }
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}
fn is_dispatcher(body: &str) -> Result<bool, String> {
    Ok(Re::new(r"\bmatch\s+request\.method\.as_str\(\)\s*\{").is_match(&source::code_text(body)?))
}

/// Every noncomment changed row must belong to a simple literal/family arm.
/// Both sides are independently proved by `changed_dispatch_trace`.
pub fn dispatch_seeds(
    text: &str,
    lines: &BTreeSet<usize>,
    sources: &SourceTree,
) -> Result<BTreeSet<String>, String> {
    let original = comments_masked(text)?;
    let lexical = source::code_text(text)?;
    let rows: Vec<_> = original.lines().collect();
    let code: Vec<_> = lexical.lines().collect();
    let header = Re::new(r"\bmatch\s+request\.method\.as_str\(\)\s*\{");
    let headers: Vec<_> = code
        .iter()
        .enumerate()
        .filter(|(_, r)| header.is_match(r))
        .map(|(i, _)| i)
        .collect();
    if headers.len() != 1 {
        return Err("RPC dispatcher match is not unique".into());
    }
    let begin = headers[0];
    let indent = &code[begin][..code[begin].len() - code[begin].trim_start().len()];
    let end = (begin + 1..rows.len())
        .find(|&i| code[i] == format!("{indent}}}") || code[i] == format!("{indent}}};"))
        .ok_or("RPC dispatcher match is not closed")?;
    let prefix = format!("{indent}    ");
    let starts: Vec<_> = (begin + 1..end)
        .filter(|&i| {
            rows[i].starts_with(&prefix)
                && !rows[i].starts_with(&format!("{prefix} "))
                && code[i].contains("=>")
        })
        .collect();
    let arms: Vec<_> = starts
        .iter()
        .copied()
        .zip(starts.iter().skip(1).copied().chain([end]))
        .collect();
    let mut chosen = BTreeSet::new();
    for &line in lines {
        let i = line
            .checked_sub(1)
            .filter(|&i| i < rows.len())
            .ok_or("RPC changed row is outside its snapshot")?;
        if rows[i].trim().is_empty() {
            continue;
        }
        chosen.insert(
            *arms
                .iter()
                .find(|&&(a, b)| a <= i && i < b)
                .ok_or("RPC changed row is outside a registration arm")?,
        );
    }
    let mut seeds = BTreeSet::new();
    let handler =
        Re::new(r"^\s*\{?\s*self\.(handle_\w+)\((?:request)?\)\s*(?:\.await)?\s*\}?\s*,?\s*$");
    let literals = Re::new(r#"^\s*"[A-Za-z_]\w*"(?:\s*\|\s*"[A-Za-z_]\w*")*\s*$"#);
    for (first, last) in chosen {
        let arm = rows[first..last]
            .iter()
            .map(|r| r.trim())
            .collect::<Vec<_>>()
            .join(" ");
        let (pattern, body) = arm
            .split_once("=>")
            .ok_or("RPC registration arm has no body")?;
        let c = handler
            .search(body)
            .ok_or("RPC registration arm is not a simple handler call")?;
        seeds.insert(group(&c, 1).into());
        if literals.is_match(pattern) {
            for c in Re::new(r#""([A-Za-z_]\w*)""#).find_all(pattern) {
                seeds.insert(group(&c, 1).into());
            }
        } else {
            let family = Re::new(r"^\s*method if (\w+)::is_operator_method\(method\)\s*$")
                .search(pattern)
                .ok_or("RPC registration predicate is not a qualified family")?;
            let module = format!("rpc::{}", group(&family, 1));
            let mut predicates = Vec::new();
            for (path, text) in &sources.texts {
                if sources.module(path).as_deref() == Some(module.as_str()) {
                    source::code_text(text)?;
                    predicates.extend(
                        source::rust_functions(text, &sources.host)
                            .into_iter()
                            .filter(|f| f.name == "is_operator_method"),
                    );
                }
            }
            if predicates.len() != 1 {
                return Err("RPC family predicate is not unique".into());
            }
            let body = comments_masked(&predicates[0].body)?;
            // Only real ordinary literals contribute method names; fixture
            // strings containing quotes do not invent additional methods.
            let mut methods = BTreeSet::new();
            for lex in source::rust_lexemes(&body)? {
                if lex.kind == source::LexemeKind::Literal {
                    if let Some(c) =
                        Re::new(r#"^"([A-Z][A-Za-z0-9_]*)"$"#).search(&body[lex.start..lex.end])
                    {
                        methods.insert(group(&c, 1).to_owned());
                    }
                }
            }
            if methods.is_empty() {
                return Err("RPC family predicate names no methods".into());
            }
            seeds.extend(methods);
        }
    }
    Ok(seeds)
}

fn below(module: &str, owner: &str) -> bool {
    module == owner || module.starts_with(&format!("{owner}::"))
}

/// Qualified helper walk, bounded independently of the dispatcher size. Its
/// settled priorities survive any unresolved identity or overflowing layer.
pub fn dispatch_observers(
    index: &mut ItemReferences,
    seeds: &BTreeSet<String>,
    dispatchers: &BTreeSet<String>,
    bounds: Bounds,
) -> Trace {
    let mut result = Trace {
        tests: None,
        direct: BTreeMap::new(),
        hubs: BTreeMap::new(),
        layers: 0,
        stop: None,
        diagnostic: None,
    };
    let mut definitions = BTreeMap::<String, BTreeSet<String>>::new();
    let mut exported = BTreeSet::new();
    for f in &index.functions {
        if !source::is_test(&f.function.attrs) {
            definitions
                .entry(f.function.name.clone())
                .or_default()
                .insert(f.module.clone());
        }
        if f.function.body.trim_start().starts_with("pub") {
            exported.insert((f.module.clone(), f.function.name.clone()));
        }
    }
    let mut reached = BTreeMap::<Item, usize>::new();
    let mut identified = BTreeSet::new();
    let mut identified_callers = BTreeMap::<Item, usize>::new();
    let hops = |module: &str,
                name: &str,
                words: &BTreeSet<String>,
                reached: &BTreeMap<Item, usize>,
                identified: &BTreeSet<Item>,
                callers: &BTreeMap<Item, usize>| {
        if !words.is_disjoint(seeds) {
            return Some(1);
        }
        let mut near = callers.get(&(module.into(), name.into())).copied();
        for (item, distance) in reached {
            if !identified.contains(item)
                && words.contains(&item.1)
                && (below(module, &item.0)
                    || definitions.get(&item.1).is_some_and(|d| d.len() == 1))
            {
                near = Some(near.map_or(distance + 1, |n| n.min(distance + 1)));
            }
        }
        near
    };
    loop {
        let mut next = reached.clone();
        for f in &index.functions {
            if source::is_test(&f.function.attrs)
                || f.function.end - f.function.start > bounds.large_function
                || dispatchers.contains(&f.function.name)
                || f.function.name == "is_operator_method"
            {
                continue;
            }
            if let Some(h) = hops(
                &f.module,
                &f.function.name,
                &f.words,
                &reached,
                &identified,
                &identified_callers,
            ) {
                next.entry((f.module.clone(), f.function.name.clone()))
                    .and_modify(|n| *n = (*n).min(h))
                    .or_insert(h);
            }
        }
        if next == reached {
            break;
        }
        if next.len() > bounds.functions {
            result.stop = Some(Stop::FunctionBound);
            result.diagnostic = Some(format!(
                "qualified registration trace exceeds {} functions",
                bounds.functions
            ));
            break;
        }
        let mut new_identified = identified.clone();
        let mut new_callers = identified_callers.clone();
        let mut unresolved = None;
        for (item, distance) in &next {
            if identified.contains(item) && reached.get(item).is_some_and(|d| d <= distance)
                || !exported.contains(item)
                || definitions.get(&item.1).is_none_or(|d| d.len() < 2)
            {
                continue;
            }
            let refs = index.callers(&item.0, &item.1);
            if let Some(reason) = refs.unresolved {
                unresolved = Some(format!(
                    "ambiguous exported helper: {}::{} ({reason})",
                    item.0, item.1
                ));
                break;
            }
            new_identified.insert(item.clone());
            for caller in refs.functions {
                new_callers
                    .entry(caller)
                    .and_modify(|n| *n = (*n).min(distance + 1))
                    .or_insert(distance + 1);
            }
        }
        if let Some(reason) = unresolved {
            result.stop = Some(Stop::UnresolvedIdentity);
            result.diagnostic = Some(reason);
            break;
        }
        reached = next;
        identified = new_identified;
        identified_callers = new_callers;
        result.layers += 1;
    }
    for f in &index.functions {
        if !source::is_test(&f.function.attrs) {
            continue;
        }
        if let Some(h) = hops(
            &f.module,
            &f.function.name,
            &f.words,
            &reached,
            &identified,
            &identified_callers,
        ) {
            for shard in source::shards_in(&f.function.attrs) {
                result.direct.insert(
                    Observer {
                        module: f.module.clone(),
                        name: f.function.name.clone(),
                        shard,
                    },
                    h,
                );
            }
        }
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
            "qualified registration trace selects {count} tests, exceeding the {}-test bound",
            bounds.tests
        ));
    }
    if result.stop.is_none() {
        result.tests = Some(result.direct.keys().cloned().collect());
    }
    result
}

/// Some means this edit used the registration specialization; None delegates
/// an ordinary non-dispatcher edit to the generic hunk proof. Err retains broad
/// coverage and cannot be replaced by a trace through dispatcher plumbing.
pub fn changed_dispatch_trace(
    sources: &SourceTree,
    old: &SourceTree,
    index: &mut ItemReferences,
    path: &str,
    hunks: &[Hunk],
    bounds: Bounds,
) -> Result<Option<Trace>, String> {
    if path != RPC {
        return Ok(None);
    }
    let mut all = BTreeSet::new();
    let mut dispatchers = BTreeSet::new();
    let mut inside = false;
    let mut invalid = false;
    for (tree, is_old) in [(old, true), (sources, false)] {
        let text = tree.texts.get(path).map_or("", String::as_str);
        let lines = selection::changed_lines(hunks, is_old);
        for f in source::rust_functions(text, &tree.host) {
            if is_dispatcher(&f.body)? {
                inside |= lines.iter().any(|n| f.attr_start < *n && *n <= f.end);
                dispatchers.insert(f.name);
            }
        }
        match dispatch_seeds(text, &lines, tree) {
            Ok(seeds) => all.extend(seeds),
            Err(_) => invalid = true,
        }
    }
    if invalid {
        if inside {
            return Err(
                "RPC edit extends beyond simple method-to-handler registration arms".into(),
            );
        }
        return Ok(None);
    }
    let mut result = dispatch_observers(index, &all, &dispatchers, bounds);
    let scanners = readers::source_scanner_tests(sources, index, &BTreeSet::from([RPC.into()]))?;
    for scanner in scanners {
        result.direct.entry(scanner).or_insert(result.layers + 1);
    }
    if result.tests.is_some() {
        result.tests = Some(result.direct.keys().cloned().collect());
    }
    Ok(Some(result))
}
