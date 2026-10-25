//! `difflib.SequenceMatcher(autojunk=False).get_opcodes()` for line sequences:
//! longest contiguous matches with the earliest `a` then earliest `b` tie
//! break, recursing on both sides, merged where adjacent.
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Equal,
    Replace,
    Delete,
    Insert,
}

/// Matching blocks `(a, b, size)` ending with the `(len(a), len(b), 0)` sentinel.
/// `budget` bounds elementary comparisons; exhausting it is `None`, never a
/// partial diff.
pub fn matching_blocks(
    a: &[String],
    b: &[String],
    mut budget: usize,
) -> Option<Vec<(usize, usize, usize)>> {
    let mut positions = BTreeMap::<&str, Vec<usize>>::new();
    for (j, row) in b.iter().enumerate() {
        positions.entry(row).or_default().push(j);
    }
    let mut pending = vec![(0, a.len(), 0, b.len())];
    let mut found = Vec::new();
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
                    budget = budget.checked_sub(1)?;
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
        found.push(best);
        if alo < i && blo < j {
            pending.push((alo, i, blo, j));
        }
        if i + n < ahi && j + n < bhi {
            pending.push((i + n, ahi, j + n, bhi));
        }
    }
    found.sort_unstable();
    let mut merged: Vec<(usize, usize, usize)> = Vec::new();
    let (mut i1, mut j1, mut k1) = (0, 0, 0);
    for (i2, j2, k2) in found {
        if i1 + k1 == i2 && j1 + k1 == j2 {
            k1 += k2;
        } else {
            if k1 > 0 {
                merged.push((i1, j1, k1));
            }
            (i1, j1, k1) = (i2, j2, k2);
        }
    }
    if k1 > 0 {
        merged.push((i1, j1, k1));
    }
    merged.push((a.len(), b.len(), 0));
    Some(merged)
}

/// `(tag, a_start, a_end, b_start, b_end)` for every region.
pub fn opcodes(
    a: &[String],
    b: &[String],
    budget: usize,
) -> Option<Vec<(Tag, usize, usize, usize, usize)>> {
    let mut codes = Vec::new();
    let (mut i, mut j) = (0, 0);
    for (ai, bj, size) in matching_blocks(a, b, budget)? {
        let tag = match (i < ai, j < bj) {
            (true, true) => Some(Tag::Replace),
            (true, false) => Some(Tag::Delete),
            (false, true) => Some(Tag::Insert),
            _ => None,
        };
        if let Some(tag) = tag {
            codes.push((tag, i, ai, j, bj));
        }
        i = ai + size;
        j = bj + size;
        if size > 0 {
            codes.push((Tag::Equal, ai, i, bj, j));
        }
    }
    Some(codes)
}
