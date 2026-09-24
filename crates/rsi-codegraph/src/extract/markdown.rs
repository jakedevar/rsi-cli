use tree_sitter::Node;

use super::Builder;
use crate::{NodeKind, RelationKind, UnresolvedReferenceKind};

pub(super) fn extract(root: Node<'_>, builder: &mut Builder<'_>) {
    let path = builder.file.relative_path.clone();
    let bytes = builder.file.bytes.clone();
    let mut fenced = Vec::new();
    fenced_ranges(root, &mut fenced);
    fenced.sort_unstable();
    let document = builder.node(
        NodeKind::MarkdownDocument,
        "markdown",
        &path,
        path.rsplit('/').next().unwrap_or(&path),
        0,
        bytes.len(),
    );
    let mut current = document.clone();
    let mut offset = 0;
    let mut next_fence = 0;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        while next_fence < fenced.len() && fenced[next_fence].1 <= offset {
            next_fence += 1;
        }
        if next_fence < fenced.len()
            && fenced[next_fence].0 < offset + line.len()
            && offset < fenced[next_fence].1
        {
            offset += line.len();
            continue;
        }
        let content = std::str::from_utf8(line).unwrap_or("");
        let trimmed = content.trim_end_matches(['\r', '\n']);
        if let Some((level, title, start)) = heading(trimmed) {
            let kind = if title.eq_ignore_ascii_case("Stage contract") {
                NodeKind::StageContract
            } else if finding_id(title).is_some() {
                NodeKind::ResearchFinding
            } else if title.starts_with("ADR ") || title.starts_with("ADR-") {
                NodeKind::DecisionRecord
            } else if title.starts_with("Plan item ") {
                NodeKind::PlanItem
            } else {
                NodeKind::MarkdownHeading
            };
            let qualified = format!("{path}#{level}:{title}");
            let key = builder.node(
                kind,
                "markdown",
                &qualified,
                title,
                offset + start,
                offset + trimmed.len(),
            );
            builder.relation(
                RelationKind::Contains,
                &document,
                &key,
                "markdown heading",
                offset + start,
                offset + trimmed.len(),
            );
            current = key;
        }
        let inline_code = code_spans(trimmed);
        for (at, id) in finding_refs(trimmed)
            .into_iter()
            .filter(|(at, id)| !inside_code(&inline_code, *at, at + id.len()))
        {
            builder.unresolved(
                &current,
                UnresolvedReferenceKind::Finding,
                &id,
                offset + at,
                offset + at + id.len(),
            );
        }
        if let Some(start) = trimmed
            .match_indices("Rationale:")
            .map(|(start, _)| start)
            .find(|start| !inside_code(&inline_code, *start, start + "Rationale:".len()))
        {
            let marker = builder.node(
                NodeKind::RationaleMarker,
                "markdown",
                &format!("{path}#rationale:{offset}"),
                "Rationale",
                offset + start,
                offset + start + "Rationale".len(),
            );
            builder.relation(
                RelationKind::Contains,
                &document,
                &marker,
                "rationale marker",
                offset + start,
                offset + start + "Rationale".len(),
            );
        }
        offset += line.len();
    }
}

fn inside_code(ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
    ranges
        .iter()
        .any(|(left, right)| *left < end && start < *right)
}

fn code_spans(line: &str) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'`' {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at] == b'`' {
            at += 1;
        }
        let width = at - start;
        let mut close = at;
        while close < bytes.len() {
            if bytes[close] != b'`' {
                close += 1;
                continue;
            }
            let end = close
                + bytes[close..]
                    .iter()
                    .take_while(|byte| **byte == b'`')
                    .count();
            if end - close == width {
                spans.push((start, end));
                at = end;
                break;
            }
            close = end;
        }
    }
    spans
}

fn fenced_ranges(node: Node<'_>, ranges: &mut Vec<(usize, usize)>) {
    if matches!(node.kind(), "fenced_code_block" | "indented_code_block") {
        ranges.push((node.start_byte(), node.end_byte()));
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        fenced_ranges(child, ranges);
    }
}

fn heading(line: &str) -> Option<(usize, &str, usize)> {
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&level) || line.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    let title = line[level + 1..].trim();
    if title.is_empty() {
        return None;
    }
    Some((level, title, level + 1))
}

fn finding_id(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.len() >= 5
        && bytes[0] == b'F'
        && bytes[1] == b'-'
        && bytes[2..5].iter().all(u8::is_ascii_digit)
    {
        Some(&text[..5])
    } else {
        None
    }
}

fn finding_refs(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    for start in 0..bytes.len().saturating_sub(4) {
        if bytes[start] == b'F'
            && bytes[start + 1] == b'-'
            && bytes[start + 2..start + 5].iter().all(u8::is_ascii_digit)
            && (start == 0 || !bytes[start - 1].is_ascii_alphanumeric())
            && (start + 5 == bytes.len() || !bytes[start + 5].is_ascii_alphanumeric())
        {
            result.push((start, text[start..start + 5].to_owned()));
        }
    }
    result
}
