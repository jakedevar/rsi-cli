use tree_sitter::Node;

use super::Builder;
use crate::{FactKey, NodeKind, RelationKind, UnresolvedReferenceKind};

#[allow(clippy::too_many_lines)] // Table context and source ownership stay in one ordered syntax pass.
pub(super) fn extract(root: Node<'_>, builder: &mut Builder<'_>) {
    let path = builder.file.relative_path.clone();
    let file_name = path.rsplit('/').next().unwrap_or(&path).to_owned();
    let file = builder.node(
        NodeKind::File,
        "toml",
        &path,
        &file_name,
        0,
        builder.file.bytes.len(),
    );
    if file_name != "Cargo.toml" {
        return;
    }
    let mut owner = file.clone();
    let mut cursor = root.walk();
    for table in root.named_children(&mut cursor) {
        if !matches!(table.kind(), "table" | "table_array_element") {
            continue;
        }
        let Some(header) = table.named_child(0) else {
            continue;
        };
        let section = builder.text(header).to_owned();
        if section == "workspace" {
            owner = builder.node(
                NodeKind::Workspace,
                "cargo_toml",
                &format!("{path}::workspace"),
                "workspace",
                header.start_byte(),
                header.end_byte(),
            );
            builder.relation(
                RelationKind::Declares,
                &file,
                &owner,
                "workspace manifest",
                header.start_byte(),
                header.end_byte(),
            );
        }
        if let Some((prefix, _)) = section.rsplit_once('.')
            && prefix.ends_with("dependencies")
        {
            let alias = last_key(header);
            dependency(builder, &owner, alias);
        }
        let mut pairs = table.walk();
        for pair in table
            .named_children(&mut pairs)
            .filter(|node| node.kind() == "pair")
        {
            let (Some(key), Some(value)) = (pair.named_child(0), pair.named_child(1)) else {
                continue;
            };
            let key_text = builder.text(key);
            if section == "package" && key_text == "name" {
                if let Some((name, start, end)) = string_content(builder, value) {
                    owner = builder.node(
                        NodeKind::Crate,
                        "cargo_toml",
                        &format!("{path}::package:{name}"),
                        &name,
                        start,
                        end,
                    );
                    builder.relation(
                        RelationKind::Declares,
                        &file,
                        &owner,
                        "package manifest",
                        start,
                        end,
                    );
                }
            } else if matches!(
                section.as_str(),
                "lib" | "bin" | "example" | "test" | "bench"
            ) && key_text == "name"
            {
                if let Some((name, start, end)) = string_content(builder, value) {
                    let target = builder.node(
                        NodeKind::CargoTarget,
                        "cargo_toml",
                        &format!("{path}::{section}:{name}"),
                        &name,
                        start,
                        end,
                    );
                    builder.relation(
                        RelationKind::Contains,
                        &owner,
                        &target,
                        "cargo target",
                        start,
                        end,
                    );
                }
            } else if section.ends_with("dependencies") {
                dependency(builder, &owner, first_key(key));
            }
        }
    }
}

fn string_content(builder: &Builder<'_>, node: Node<'_>) -> Option<(String, usize, usize)> {
    if node.kind() != "string" {
        return None;
    }
    let text = builder.text(node);
    let quote = text.as_bytes().first().copied()?;
    if !matches!(quote, b'"' | b'\'') || text.as_bytes().last().copied() != Some(quote) {
        return None;
    }
    let content = &text[1..text.len() - 1];
    if content.is_empty() || content.contains(['\n', '\r', '\\']) {
        return None;
    }
    Some((
        content.to_owned(),
        node.start_byte() + 1,
        node.end_byte() - 1,
    ))
}

fn first_key(mut key: Node<'_>) -> Node<'_> {
    while key.kind() == "dotted_key" {
        key = key.named_child(0).unwrap_or(key);
    }
    key
}

fn last_key(mut key: Node<'_>) -> Node<'_> {
    while key.kind() == "dotted_key" {
        key = key
            .named_child(key.named_child_count().saturating_sub(1))
            .unwrap_or(key);
    }
    key
}

fn dependency(builder: &mut Builder<'_>, owner: &FactKey, key: Node<'_>) {
    let raw = builder.text(key);
    let alias = raw.trim_matches(['"', '\'']);
    if alias.is_empty() || alias.len() > crate::MAX_NAME_BYTES {
        return;
    }
    let offset = usize::from(alias.len() != raw.len());
    let start = key.start_byte() + offset;
    builder.unresolved(
        owner,
        UnresolvedReferenceKind::Other,
        alias,
        start,
        start + alias.len(),
    );
}
