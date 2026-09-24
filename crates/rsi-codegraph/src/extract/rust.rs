use tree_sitter::Node;

use super::Builder;
use crate::{FactKey, NodeKind, RelationKind, UnresolvedReferenceKind};

pub(super) fn extract(root: Node<'_>, builder: &mut Builder<'_>) {
    let path = builder.file.relative_path.clone();
    let file_name = path.rsplit('/').next().unwrap_or(&path).to_owned();
    let file = builder.node(
        NodeKind::File,
        "rust",
        &path,
        &file_name,
        0,
        builder.file.bytes.len(),
    );
    walk(root, builder, &file, &path, false);
}

#[allow(clippy::too_many_lines)] // One syntax walk keeps declaration scope and reference ownership aligned.
fn walk(node: Node<'_>, builder: &mut Builder<'_>, owner: &FactKey, scope: &str, in_impl: bool) {
    // Only the root may contain an error while admitting independent siblings.
    // Descending into a malformed declaration could assign its children a
    // fabricated scope even when their individual syntax nodes are clean.
    if node.has_error() && node.kind() != "source_file" {
        return;
    }
    let mut declared_owner = None;
    let mut declared_scope = None;
    let mut next_impl = in_impl;
    if !node.has_error()
        && let Some((kind, name_node, display)) = declaration(node, builder, in_impl)
    {
        let qualified = format!("{scope}::{display}");
        let key = builder.node(
            kind,
            "rust",
            &qualified,
            &display,
            name_node.start_byte(),
            name_node.end_byte(),
        );
        builder.relation(
            if kind == NodeKind::Method {
                RelationKind::HasMethod
            } else {
                RelationKind::Declares
            },
            owner,
            &key,
            "rust declaration",
            name_node.start_byte(),
            name_node.end_byte(),
        );
        if kind == NodeKind::Impl {
            next_impl = true;
        }
        declared_owner = Some(key);
        declared_scope = Some(qualified);
    }
    if !node.has_error() {
        match node.kind() {
            "call_expression" => {
                if let Some(target) = node.child_by_field_name("function") {
                    let raw = builder.text(target).to_owned();
                    if !raw.is_empty() && target.end_byte() - target.start_byte() <= 4096 {
                        builder.unresolved(
                            owner,
                            UnresolvedReferenceKind::Call,
                            &raw,
                            target.start_byte(),
                            target.end_byte(),
                        );
                    }
                }
            }
            "use_declaration" => {
                let raw = builder
                    .text(node)
                    .trim()
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .to_owned();
                if !raw.is_empty() {
                    let end = node.end_byte().min(node.start_byte() + 4096);
                    builder.unresolved(
                        owner,
                        UnresolvedReferenceKind::Import,
                        &raw,
                        node.start_byte(),
                        end,
                    );
                }
            }
            "impl_item" => {
                if let Some(target) = node.child_by_field_name("trait") {
                    let raw = builder.text(target).to_owned();
                    if !raw.is_empty() && raw.len() <= 4096 {
                        builder.unresolved(
                            declared_owner.as_ref().unwrap_or(owner),
                            UnresolvedReferenceKind::ImplTrait,
                            &raw,
                            target.start_byte(),
                            target.end_byte(),
                        );
                    }
                }
            }
            "scoped_type_identifier" => {
                let raw = builder.text(node).to_owned();
                if !raw.is_empty() && raw.len() <= 4096 {
                    builder.unresolved(
                        owner,
                        UnresolvedReferenceKind::Type,
                        &raw,
                        node.start_byte(),
                        node.end_byte(),
                    );
                }
            }
            _ => {}
        }
    }
    let next_owner = declared_owner.as_ref().unwrap_or(owner);
    let next_scope = declared_scope.as_deref().unwrap_or(scope);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, builder, next_owner, next_scope, next_impl);
    }
}

fn declaration<'tree>(
    node: Node<'tree>,
    builder: &Builder<'_>,
    in_impl: bool,
) -> Option<(NodeKind, Node<'tree>, String)> {
    let kind = match node.kind() {
        "mod_item" => NodeKind::Module,
        "function_item" if is_test(node, builder) => NodeKind::Test,
        "function_item" if in_impl => NodeKind::Method,
        "function_item" => NodeKind::Function,
        "struct_item" => NodeKind::Struct,
        "trait_item" => NodeKind::Trait,
        "enum_item" => NodeKind::Enum,
        "enum_variant" => NodeKind::EnumVariant,
        "type_item" => NodeKind::TypeAlias,
        "const_item" => NodeKind::Const,
        "static_item" => NodeKind::Static,
        "impl_item" => NodeKind::Impl,
        _ => return None,
    };
    let name = if kind == NodeKind::Impl {
        node.child_by_field_name("type")?
    } else {
        node.child_by_field_name("name")?
    };
    let text = builder.text(name).to_owned();
    if text.is_empty() || text.len() > crate::MAX_NAME_BYTES {
        return None;
    }
    Some((kind, name, text))
}

fn is_test(node: Node<'_>, builder: &Builder<'_>) -> bool {
    let mut sibling = node.prev_named_sibling();
    while let Some(attribute) = sibling {
        if attribute.kind() != "attribute_item" {
            break;
        }
        let text = builder.text(attribute);
        if text == "#[test]" || text.ends_with("::test]") {
            return true;
        }
        sibling = attribute.prev_named_sibling();
    }
    false
}
