//! Closed private-module seams and indentation-only relocation alignment.
//! The bounded matcher implements longest contiguous matches with earliest
//! old/new ties, without popular-line suppression. Exhaustion keeps coverage.
use super::{seeds, source};
use crate::re::{Re, group};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct Relocation {
    pub moved: BTreeMap<String, String>,
    /// One-based inclusive private module regions in the candidate.
    pub regions: Vec<(usize, usize)>,
}

pub fn private_move(before: &str, after: &str) -> Result<Relocation, String> {
    let before = source::code_text(before)?;
    let after = source::code_text(after)?;
    let header = Re::new(r"^(?:pub(?:\([^)]*\))?\s+)?struct\s+(\w+)\s*\{");
    let old_types: BTreeSet<_> = before
        .lines()
        .filter_map(|r| header.search(r))
        .map(|c| group(&c, 1).to_owned())
        .collect();
    let rows: Vec<_> = after.lines().collect();
    let module = Re::new(r"^mod\s+(\w+)\s*\{$");
    let mut result = Relocation::default();
    for (start, row) in rows.iter().enumerate() {
        let Some(c) = module.search(row) else {
            continue;
        };
        let Some(end) = (start + 1..rows.len()).find(|&i| rows[i] == "}") else {
            continue;
        };
        let types: BTreeSet<_> = rows[start + 1..end]
            .iter()
            .filter_map(|r| r.strip_prefix("    ").and_then(|r| header.search(r)))
            .map(|c| group(&c, 1).to_owned())
            .collect();
        if types.is_disjoint(&old_types) {
            continue;
        }
        for name in types {
            if result.moved.insert(name, group(&c, 1).into()).is_some() {
                return Err("a relocated type has multiple private owners".into());
            }
        }
        result.regions.push((start + 1, end + 1));
    }
    Ok(result)
}

fn normalized(text: &str, relocation: &Relocation, candidate: bool) -> Result<Vec<String>, String> {
    let types = relocation.moved.keys().cloned().collect();
    let mut owners = seeds::type_owners(text, &types)?;
    let rows: Vec<_> = text.lines().collect();
    let mut literal_rows = BTreeSet::new();
    for lex in source::rust_lexemes(text)? {
        if lex.kind == source::LexemeKind::Literal && text[lex.start..lex.end].contains('\n') {
            let first = text[..lex.start].bytes().filter(|b| *b == b'\n').count();
            let last = first
                + text[lex.start..lex.end]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count();
            literal_rows.extend(first..=last);
        }
    }
    // Attributes move with their following item, including a stacked derive.
    for i in (0..rows.len().saturating_sub(1)).rev() {
        if rows[i].trim_start().starts_with("#[") {
            owners[i] = owners[i + 1].clone();
        }
    }
    Ok(rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            if !literal_rows.contains(&i)
                && (owners[i].is_some()
                    || candidate
                        && relocation
                            .regions
                            .iter()
                            .any(|(a, b)| *a <= i + 1 && i + 1 <= *b))
            {
                r.trim().to_owned()
            } else {
                (*r).to_owned()
            }
        })
        .collect())
}

pub fn relocation_lines(
    before: &str,
    after: &str,
    relocation: &Relocation,
    comparisons: usize,
) -> Result<(BTreeSet<usize>, BTreeSet<usize>), String> {
    let a = normalized(before, relocation, false)?;
    let b = normalized(after, relocation, true)?;
    let mut positions = BTreeMap::<&str, Vec<usize>>::new();
    for (j, row) in b.iter().enumerate() {
        positions.entry(row).or_default().push(j);
    }
    let mut budget = comparisons;
    let mut pending = vec![(0, a.len(), 0, b.len())];
    let mut matches = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = pending.pop() {
        let mut best = (alo, blo, 0);
        let mut previous = BTreeMap::<usize, usize>::new();
        for (i, row) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut current = BTreeMap::new();
            if let Some(js) = positions.get(row.as_str()) {
                for &j in js
                    .iter()
                    .skip_while(|&&j| j < blo)
                    .take_while(|&&j| j < bhi)
                {
                    budget = budget
                        .checked_sub(1)
                        .ok_or("private relocation alignment exceeds its comparison bound")?;
                    let length = j
                        .checked_sub(1)
                        .and_then(|j| previous.get(&j))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    current.insert(j, length);
                    if length > best.2 {
                        best = (i + 1 - length, j + 1 - length, length);
                    }
                }
            }
            previous = current;
        }
        let (i, j, n) = best;
        if n == 0 {
            continue;
        }
        matches.push(best);
        if alo < i && blo < j {
            pending.push((alo, i, blo, j));
        }
        if i + n < ahi && j + n < bhi {
            pending.push((i + n, ahi, j + n, bhi));
        }
    }
    matches.sort_unstable();
    matches.push((a.len(), b.len(), 0));
    let (mut old, mut new) = (BTreeSet::new(), BTreeSet::new());
    let (mut ai, mut bj) = (0, 0);
    for (i, j, n) in matches {
        old.extend(ai + 1..i + 1);
        new.extend(bj + 1..j + 1);
        ai = i + n;
        bj = j + n;
    }
    Ok((old, new))
}
