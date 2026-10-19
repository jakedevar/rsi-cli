use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use tree_sitter::Node;

use super::Builder;
use crate::scope::{
    FnDecl, ImplDecl, ItemDecl, MethodDecl, ModuleDecl, Receiver, RefSite, SelfRef, Shape,
    StructDecl, Sym, TraitDecl, UseDecl,
};
use crate::{FactKey, NodeKind, RelationKind, UnresolvedReferenceKind};

/// Attribute path tails that mark a test function (`#[test]`,
/// `#[tokio::test(flavor = "multi_thread")]`, `#[sqlx::test]`, `#[rstest]`).
const TEST_ATTRIBUTES: &[&str] = &[
    "test",
    "rstest",
    "test_case",
    "wasm_bindgen_test",
    "quickcheck",
];

/// Wrapper types whose methods are reached through auto-deref.
const DEREF_WRAPPERS: &[&str] = &["Box", "Arc", "Rc"];

/// Macros whose arguments are comma (or, for `vec!`, semicolon) separated
/// expressions.
const EXPRESSION_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "format",
    "format_args",
    "print",
    "println",
    "eprint",
    "eprintln",
    "write",
    "writeln",
    "panic",
    "todo",
    "unimplemented",
    "unreachable",
    "vec",
    "dbg",
    "params",
    "anyhow",
    "bail",
    "ensure",
    "info",
    "debug",
    "warn",
    "error",
    "trace",
];

const WRAPPER_PREFIX: &[u8] = b"fn w(){let _=";
const WRAPPER_SUFFIX: &[u8] = b";}";
/// Bounds extra facts so a very large test file stays under the staged
/// per-file fact ceiling.
const MAX_MACRO_CALLS_PER_FILE: usize = 10_000;
const MAX_MACRO_DEPTH: u8 = 3;
const MAX_REFERENCE_BYTES: usize = 4096;

/// Source bytes plus the offset that maps them back to the original file, so
/// macro-argument chunks parsed from a scratch buffer keep exact file spans.
struct Src<'s> {
    bytes: &'s [u8],
    origin: isize,
    /// Names bound by patterns inside the current macro chunk; a use of one
    /// refers to that binding, not to an enclosing function's.
    bound: &'s HashSet<String>,
}

impl<'s> Src<'s> {
    fn text(&self, node: Node<'_>) -> &'s str {
        std::str::from_utf8(&self.bytes[node.byte_range()]).unwrap_or("")
    }

    fn orig(&self, offset: usize) -> usize {
        usize::try_from(isize::try_from(offset).unwrap_or(0) + self.origin).unwrap_or(0)
    }
}

#[derive(Clone)]
enum LocalType {
    Typed(Vec<String>),
    Ctor { ty: Vec<String>, assoc: String },
}

/// Names bound in one function body. A name bound more than once may refer
/// to different values at different uses, so only single bindings carry a type.
#[derive(Default)]
struct FnBindings {
    counts: HashMap<String, u32>,
    types: HashMap<String, LocalType>,
    nested_fns: HashSet<String>,
}

#[derive(Clone)]
struct Frame {
    /// Interned scope string of the nearest module.
    module: Sym,
    module_path: Rc<str>,
    /// Directly inside a module (not a function, impl or trait body).
    module_level: bool,
    self_ref: SelfRef,
    generics: Rc<Vec<String>>,
    func: Option<Rc<FnBindings>>,
}

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
    let bound = HashSet::new();
    let source = builder.file;
    let src = Src {
        bytes: &source.bytes,
        origin: 0,
        bound: &bound,
    };
    let module = builder.sym(&path);
    let frame = Frame {
        module,
        module_path: Rc::from(path.as_str()),
        module_level: true,
        self_ref: SelfRef::None,
        generics: Rc::new(Vec::new()),
        func: None,
    };
    walk(root, builder, &src, &file, &path, false, &frame);
}

#[allow(clippy::too_many_lines)] // One syntax walk keeps declaration scope and reference ownership aligned.
fn walk(
    node: Node<'_>,
    builder: &mut Builder<'_>,
    src: &Src<'_>,
    owner: &FactKey,
    scope: &str,
    in_impl: bool,
    frame: &Frame,
) {
    // Only the root may contain an error while admitting independent siblings.
    // Descending into a malformed declaration could assign its children a
    // fabricated scope even when their individual syntax nodes are clean.
    if node.has_error() && node.kind() != "source_file" {
        return;
    }
    let mut declared_owner = None;
    let mut declared_scope = None;
    let mut next_impl = in_impl;
    let mut child_frame: Option<Frame> = None;
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
        child_frame = record_declaration(node, builder, src, frame, kind, &display);
        declared_owner = Some(key);
        declared_scope = Some(qualified);
    }
    let frame = child_frame.as_ref().unwrap_or(frame);
    if !node.has_error() {
        match node.kind() {
            "call_expression" => record_call(builder, src, node, owner, frame),
            "macro_invocation" => record_macro(builder, src, node, owner, frame, 0),
            "use_declaration" => {
                let raw = builder
                    .text(node)
                    .trim()
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .to_owned();
                if !raw.is_empty() {
                    let end = node.end_byte().min(node.start_byte() + 4096);
                    let reference = builder.unresolved(
                        owner,
                        UnresolvedReferenceKind::Import,
                        &raw,
                        node.start_byte(),
                        end,
                    );
                    record_uses(builder, src, node, reference, frame);
                }
            }
            "impl_item" => {
                if let Some(target) = node.child_by_field_name("trait") {
                    let raw = builder.text(target).to_owned();
                    if !raw.is_empty() && raw.len() <= 4096 {
                        let reference = builder.unresolved(
                            declared_owner.as_ref().unwrap_or(owner),
                            UnresolvedReferenceKind::ImplTrait,
                            &raw,
                            target.start_byte(),
                            target.end_byte(),
                        );
                        let outer = frame_outer(frame);
                        let shape = path_shape(builder, src, target);
                        push_site(
                            builder,
                            reference,
                            target.start_byte(),
                            &outer,
                            false,
                            shape,
                        );
                        if let SelfRef::Impl(index) = frame.self_ref
                            && let Some(decl) = builder.scope.impls.get_mut(index as usize)
                        {
                            decl.trait_reference = Some(reference);
                        }
                    }
                }
            }
            "scoped_type_identifier" => {
                let raw = builder.text(node).to_owned();
                if !raw.is_empty() && raw.len() <= 4096 {
                    let reference = builder.unresolved(
                        owner,
                        UnresolvedReferenceKind::Type,
                        &raw,
                        node.start_byte(),
                        node.end_byte(),
                    );
                    let shape = path_shape(builder, src, node);
                    push_site(builder, reference, node.start_byte(), frame, false, shape);
                }
            }
            _ => {}
        }
    }
    let next_owner = declared_owner.as_ref().unwrap_or(owner);
    let next_scope = declared_scope.as_deref().unwrap_or(scope);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(
            child, builder, src, next_owner, next_scope, next_impl, frame,
        );
    }
}

/// The frame an impl's own header is read in: the surrounding module, not the
/// impl body it just opened.
fn frame_outer(frame: &Frame) -> Frame {
    Frame {
        self_ref: SelfRef::None,
        ..frame.clone()
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

/// A test attribute may carry a path and arguments: only the path's last
/// segment decides. `#[cfg(test)]` and `#[ignore]` do not make a function a test.
fn is_test(node: Node<'_>, builder: &Builder<'_>) -> bool {
    let mut sibling = node.prev_named_sibling();
    while let Some(attribute) = sibling {
        match attribute.kind() {
            "attribute_item" => {
                let text = builder.text(attribute);
                let inner = text
                    .trim_start_matches('#')
                    .trim_start_matches('!')
                    .trim_start_matches('[')
                    .trim_end_matches(']');
                let path = inner
                    .split(|c: char| c == '(' || c == '=' || c.is_whitespace())
                    .next()
                    .unwrap_or("");
                let tail = path.rsplit("::").next().unwrap_or("");
                if TEST_ATTRIBUTES.contains(&tail) {
                    return true;
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sibling = attribute.prev_named_sibling();
    }
    false
}

fn path_attribute(node: Node<'_>, builder: &mut Builder<'_>) -> Option<Sym> {
    let mut sibling = node.prev_named_sibling();
    while let Some(attribute) = sibling {
        match attribute.kind() {
            "attribute_item" => {
                let text = builder.text(attribute);
                if let Some(rest) = text.strip_prefix("#[path")
                    && let Some(start) = rest.find('"')
                    && let Some(end) = rest[start + 1..].find('"')
                {
                    let value = rest[start + 1..start + 1 + end].to_owned();
                    return Some(builder.sym(&value));
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sibling = attribute.prev_named_sibling();
    }
    None
}

/// Record a declaration's resolver facts. Returns a replacement frame for the
/// declaration's children when the declaration opens a new scope.
#[allow(clippy::too_many_lines)] // One match keeps every declaration form's scope bookkeeping together.
fn record_declaration(
    node: Node<'_>,
    builder: &mut Builder<'_>,
    src: &Src<'_>,
    frame: &Frame,
    kind: NodeKind,
    display: &str,
) -> Option<Frame> {
    let index = builder.last_node();
    let name = builder.sym(display);
    match node.kind() {
        "mod_item" => {
            if !frame.module_level {
                return None;
            }
            let inline = node.child_by_field_name("body").is_some();
            let path_attr = if inline {
                None
            } else {
                path_attribute(node, builder)
            };
            builder.scope.modules.push(ModuleDecl {
                node: index,
                name,
                parent: frame.module,
                inline,
                path_attr,
            });
            builder.scope.items.push(ItemDecl {
                node: index,
                name,
                kind,
                module: frame.module,
            });
            inline.then(|| {
                let module_path = format!("{}::{display}", frame.module_path);
                Frame {
                    module: builder.sym(&module_path),
                    module_path: Rc::from(module_path.as_str()),
                    module_level: true,
                    self_ref: SelfRef::None,
                    generics: Rc::new(Vec::new()),
                    func: None,
                }
            })
        }
        "function_item" => {
            let nested = frame.func.is_some();
            let outer_generics = if nested {
                Vec::new()
            } else {
                frame.generics.as_ref().clone()
            };
            let mut generics = outer_generics;
            generics.extend(type_parameter_names(node, src));
            let direct_member = node
                .parent()
                .and_then(|parent| parent.parent())
                .is_some_and(|grand| matches!(grand.kind(), "impl_item" | "trait_item"));
            let method = MethodDecl {
                node: index,
                name,
                has_self: has_self_parameter(node),
            };
            let self_ref = if nested {
                SelfRef::None
            } else {
                frame.self_ref
            };
            let mut register_fn = false;
            match (direct_member, frame.self_ref) {
                (true, SelfRef::Impl(at)) => {
                    if let Some(decl) = builder.scope.impls.get_mut(at as usize) {
                        decl.methods.push(method);
                    }
                    register_fn = true;
                }
                (true, SelfRef::Trait(at)) => {
                    if let Some(decl) = builder.scope.traits.get_mut(at as usize) {
                        decl.methods.push(method);
                    }
                    register_fn = true;
                }
                _ => {
                    if frame.module_level && matches!(kind, NodeKind::Function | NodeKind::Test) {
                        builder.scope.items.push(ItemDecl {
                            node: index,
                            name,
                            kind,
                            module: frame.module,
                        });
                        register_fn = true;
                    }
                }
            }
            if register_fn {
                let ret = node
                    .child_by_field_name("return_type")
                    .and_then(|ty| type_path(src, ty, &generics, true))
                    .map(|path| intern_path(builder, &path));
                builder.scope.fns.push(FnDecl { node: index, ret });
            }
            let bindings = fn_bindings(node, src, &generics);
            Some(Frame {
                module: frame.module,
                module_path: frame.module_path.clone(),
                module_level: false,
                self_ref,
                generics: Rc::new(generics),
                func: Some(Rc::new(bindings)),
            })
        }
        "struct_item" | "enum_item" | "type_item" | "const_item" | "static_item" => {
            if frame.module_level {
                builder.scope.items.push(ItemDecl {
                    node: index,
                    name,
                    kind,
                    module: frame.module,
                });
                if node.kind() == "struct_item" {
                    let generics = type_parameter_names(node, src);
                    let fields = struct_fields(node, src, &generics, builder);
                    builder.scope.structs.push(StructDecl {
                        node: index,
                        module: frame.module,
                        fields,
                    });
                }
            }
            None
        }
        "trait_item" => {
            if frame.module_level {
                builder.scope.items.push(ItemDecl {
                    node: index,
                    name,
                    kind,
                    module: frame.module,
                });
            }
            let at = u32::try_from(builder.scope.traits.len()).unwrap_or(u32::MAX);
            builder.scope.traits.push(TraitDecl {
                node: index,
                module: frame.module,
                methods: Vec::new(),
            });
            Some(Frame {
                module: frame.module,
                module_path: frame.module_path.clone(),
                module_level: false,
                self_ref: SelfRef::Trait(at),
                generics: Rc::new(type_parameter_names(node, src)),
                func: None,
            })
        }
        "impl_item" => {
            let generics = type_parameter_names(node, src);
            let self_ty = node
                .child_by_field_name("type")
                .and_then(|ty| type_path(src, ty, &generics, false))
                .map(|path| intern_path(builder, &path));
            let trait_ty = node
                .child_by_field_name("trait")
                .and_then(|ty| type_path(src, ty, &generics, false))
                .map(|path| intern_path(builder, &path));
            let generic_syms = generics.iter().map(|g| builder.sym(g)).collect();
            let at = u32::try_from(builder.scope.impls.len()).unwrap_or(u32::MAX);
            builder.scope.impls.push(ImplDecl {
                node: index,
                module: frame.module,
                generics: generic_syms,
                self_ty,
                trait_ty,
                trait_reference: None,
                methods: Vec::new(),
            });
            Some(Frame {
                module: frame.module,
                module_path: frame.module_path.clone(),
                module_level: false,
                self_ref: SelfRef::Impl(at),
                generics: Rc::new(generics),
                func: None,
            })
        }
        _ => None,
    }
}

fn intern_path(builder: &mut Builder<'_>, path: &[String]) -> Vec<Sym> {
    path.iter().map(|segment| builder.sym(segment)).collect()
}

fn has_self_parameter(function: Node<'_>) -> bool {
    function
        .child_by_field_name("parameters")
        .is_some_and(|parameters| {
            let mut cursor = parameters.walk();
            parameters
                .named_children(&mut cursor)
                .any(|parameter| parameter.kind() == "self_parameter")
        })
}

fn type_parameter_names(node: Node<'_>, src: &Src<'_>) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(parameters) = node.child_by_field_name("type_parameters") {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if matches!(parameter.kind(), "type_parameter" | "const_parameter")
                && let Some(name) = parameter.child_by_field_name("name")
            {
                names.push(src.text(name).to_owned());
            }
        }
    }
    names
}

fn struct_fields(
    node: Node<'_>,
    src: &Src<'_>,
    generics: &[String],
    builder: &mut Builder<'_>,
) -> Vec<(Sym, Vec<Sym>)> {
    let mut fields = Vec::new();
    let Some(body) = node.child_by_field_name("body") else {
        return fields;
    };
    if body.kind() != "field_declaration_list" {
        return fields;
    }
    let mut cursor = body.walk();
    for field in body.named_children(&mut cursor) {
        if field.kind() != "field_declaration" {
            continue;
        }
        let (Some(name), Some(ty)) = (
            field.child_by_field_name("name"),
            field.child_by_field_name("type"),
        ) else {
            continue;
        };
        if let Some(path) = type_path(src, ty, generics, true) {
            let name = builder.sym(src.text(name));
            fields.push((name, intern_path(builder, &path)));
        }
    }
    fields
}

/// The path a type names, when it is a plain path. With `unwrap`, references
/// and `Box`/`Arc`/`Rc` give the pointee, since methods reach it by auto-deref.
/// A generic parameter or any non-path type yields `None`.
fn type_path(
    src: &Src<'_>,
    node: Node<'_>,
    generics: &[String],
    unwrap: bool,
) -> Option<Vec<String>> {
    let path = match node.kind() {
        "type_identifier" => vec![src.text(node).to_owned()],
        "scoped_type_identifier" => path_segments(src, node)?,
        "generic_type" => {
            let base = type_path(src, node.child_by_field_name("type")?, generics, false)?;
            let wrapper = base
                .last()
                .is_some_and(|last| DEREF_WRAPPERS.contains(&last.as_str()));
            if unwrap && wrapper {
                let arguments = node.child_by_field_name("type_arguments")?;
                let mut cursor = arguments.walk();
                let inner = arguments
                    .named_children(&mut cursor)
                    .find(|argument| argument.kind() != "lifetime")?;
                return type_path(src, inner, generics, true);
            }
            base
        }
        "reference_type" if unwrap => {
            return type_path(src, node.child_by_field_name("type")?, generics, true);
        }
        _ => return None,
    };
    if path.first().is_some_and(|first| generics.contains(first)) {
        return None;
    }
    Some(path)
}

/// Segments of a path node (`a::b::c`, `crate::x`, `Type::<T>::f`). Anything
/// with a qualified self type (`<T as Trait>::f`) is not a plain path.
fn path_segments(src: &Src<'_>, node: Node<'_>) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" | "type_identifier" | "crate" | "self" | "super" => {
            Some(vec![src.text(node).to_owned()])
        }
        "scoped_identifier" | "scoped_type_identifier" => {
            let mut segments = match node.child_by_field_name("path") {
                Some(path) => path_segments(src, path)?,
                None => Vec::new(),
            };
            segments.push(src.text(node.child_by_field_name("name")?).to_owned());
            Some(segments)
        }
        "generic_type" | "generic_type_with_turbofish" => {
            path_segments(src, node.child_by_field_name("type")?)
        }
        _ => None,
    }
}

fn path_shape(builder: &mut Builder<'_>, src: &Src<'_>, node: Node<'_>) -> Shape {
    match path_segments(src, node) {
        Some(segments) if !segments.is_empty() => Shape::Path(intern_path(builder, &segments)),
        _ => Shape::Opaque,
    }
}

fn push_site(
    builder: &mut Builder<'_>,
    reference: u32,
    start: usize,
    frame: &Frame,
    shadowed: bool,
    shape: Shape,
) {
    builder.scope.sites.push(RefSite {
        reference,
        start_byte: u32::try_from(start).unwrap_or(u32::MAX),
        module: frame.module,
        self_ref: frame.self_ref,
        shadowed,
        shape,
    });
}

// ---------------------------------------------------------------- imports

fn record_uses(
    builder: &mut Builder<'_>,
    src: &Src<'_>,
    declaration: Node<'_>,
    reference: u32,
    frame: &Frame,
) {
    let Some(argument) = declaration.child_by_field_name("argument") else {
        return;
    };
    let mut leaves = Vec::new();
    flatten_use(src, argument, &[], &mut leaves);
    for (path, alias, glob) in leaves {
        if path.is_empty() {
            continue;
        }
        let path = intern_path(builder, &path);
        let alias = alias.map(|alias| builder.sym(&alias));
        builder.scope.uses.push(UseDecl {
            reference,
            module: frame.module,
            path,
            alias,
            glob,
            local: !frame.module_level,
        });
    }
}

type UseLeaf = (Vec<String>, Option<String>, bool);

fn flatten_use(src: &Src<'_>, node: Node<'_>, prefix: &[String], out: &mut Vec<UseLeaf>) {
    match node.kind() {
        "identifier" | "crate" | "super" | "scoped_identifier" => {
            if let Some(segments) = path_segments(src, node) {
                out.push(([prefix, &segments].concat(), None, false));
            }
        }
        "self" => {
            // `use a::b::{self}` imports `a::b` itself.
            if !prefix.is_empty() {
                out.push((prefix.to_vec(), None, false));
            }
        }
        "use_as_clause" => {
            let alias = node
                .child_by_field_name("alias")
                .map(|alias| src.text(alias).to_owned());
            if let Some(path) = node.child_by_field_name("path") {
                let segments = if path.kind() == "self" {
                    Some(Vec::new())
                } else {
                    path_segments(src, path)
                };
                if let Some(segments) = segments {
                    out.push(([prefix, &segments].concat(), alias, false));
                }
            }
        }
        "scoped_use_list" => {
            let head = node
                .child_by_field_name("path")
                .and_then(|path| path_segments(src, path))
                .unwrap_or_default();
            let next = [prefix, &head].concat();
            if let Some(list) = node.child_by_field_name("list") {
                flatten_use(src, list, &next, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                flatten_use(src, child, prefix, out);
            }
        }
        "use_wildcard" => {
            let mut cursor = node.walk();
            let head = node
                .named_children(&mut cursor)
                .next()
                .and_then(|path| path_segments(src, path))
                .unwrap_or_default();
            out.push(([prefix, &head].concat(), None, true));
        }
        _ => {}
    }
}

// ---------------------------------------------------------- function scope

fn fn_bindings(function: Node<'_>, src: &Src<'_>, generics: &[String]) -> FnBindings {
    let mut bindings = FnBindings::default();
    if let Some(parameters) = function.child_by_field_name("parameters") {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if parameter.kind() != "parameter" {
                continue;
            }
            let Some(pattern) = parameter.child_by_field_name("pattern") else {
                continue;
            };
            let mut names = Vec::new();
            pattern_names(src, pattern, &mut names);
            for name in &names {
                *bindings.counts.entry(name.clone()).or_default() += 1;
            }
            if let [name] = names.as_slice()
                && is_simple_pattern(pattern)
                && let Some(ty) = parameter.child_by_field_name("type")
                && let Some(path) = type_path(src, ty, generics, true)
            {
                bindings.types.insert(name.clone(), LocalType::Typed(path));
            }
        }
    }
    if let Some(body) = function.child_by_field_name("body") {
        scan_bindings(src, body, generics, &mut bindings);
    }
    bindings
}

fn is_simple_pattern(pattern: Node<'_>) -> bool {
    match pattern.kind() {
        "identifier" => true,
        "mut_pattern" => {
            let mut cursor = pattern.walk();
            let rest = pattern
                .named_children(&mut cursor)
                .filter(|child| child.kind() != "mutable_specifier")
                .collect::<Vec<_>>();
            matches!(rest.as_slice(), [only] if only.kind() == "identifier")
        }
        _ => false,
    }
}

fn scan_bindings(src: &Src<'_>, node: Node<'_>, generics: &[String], out: &mut FnBindings) {
    match node.kind() {
        "function_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.nested_fns.insert(src.text(name).to_owned());
            }
            return;
        }
        "token_tree" => return,
        "let_declaration" => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                let mut names = Vec::new();
                pattern_names(src, pattern, &mut names);
                for name in &names {
                    *out.counts.entry(name.clone()).or_default() += 1;
                }
                if let [name] = names.as_slice()
                    && is_simple_pattern(pattern)
                    && let Some(hint) = let_type(src, node, generics)
                {
                    out.types.insert(name.clone(), hint);
                }
            }
        }
        "for_expression" | "let_condition" => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                count_pattern(src, pattern, out);
            }
        }
        "closure_parameters" | "match_pattern" => count_pattern(src, node, out),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        scan_bindings(src, child, generics, out);
    }
}

fn count_pattern(src: &Src<'_>, pattern: Node<'_>, out: &mut FnBindings) {
    let mut names = Vec::new();
    pattern_names(src, pattern, &mut names);
    for name in names {
        *out.counts.entry(name).or_default() += 1;
    }
}

/// Every identifier inside a pattern. This over-approximates (enum paths such
/// as `None` count too), which only makes receiver typing more conservative.
fn pattern_names(src: &Src<'_>, node: Node<'_>, out: &mut Vec<String>) {
    if matches!(node.kind(), "identifier" | "shorthand_field_identifier") {
        out.push(src.text(node).to_owned());
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        pattern_names(src, child, out);
    }
}

/// The type a `let x = ..` statement fixes for `x`: a declared type, a struct
/// literal, or `Type::assoc(..)` (checked later against `assoc`'s return type).
fn let_type(src: &Src<'_>, declaration: Node<'_>, generics: &[String]) -> Option<LocalType> {
    if let Some(ty) = declaration.child_by_field_name("type") {
        return type_path(src, ty, generics, true).map(LocalType::Typed);
    }
    let value = declaration.child_by_field_name("value")?;
    match value.kind() {
        "struct_expression" => {
            let name = value.child_by_field_name("name")?;
            let path = path_segments(src, name)?;
            (!generics.contains(path.first()?)).then_some(LocalType::Typed(path))
        }
        "call_expression" => {
            let function = value.child_by_field_name("function")?;
            if function.kind() != "scoped_identifier" {
                return None;
            }
            let mut path = path_segments(src, function)?;
            let assoc = path.pop()?;
            if path.is_empty() || generics.contains(path.first()?) {
                return None;
            }
            Some(LocalType::Ctor { ty: path, assoc })
        }
        _ => None,
    }
}

// -------------------------------------------------------------------- calls

fn record_call(
    builder: &mut Builder<'_>,
    src: &Src<'_>,
    call: Node<'_>,
    owner: &FactKey,
    frame: &Frame,
) {
    let Some(function) = call.child_by_field_name("function") else {
        return;
    };
    let length = function.end_byte() - function.start_byte();
    if length == 0 || length > MAX_REFERENCE_BYTES {
        return;
    }
    let raw = src.text(function).to_owned();
    if raw.is_empty() {
        return;
    }
    let (shape, shadowed) = call_shape(builder, src, function, frame);
    let start = src.orig(function.start_byte());
    let end = src.orig(function.end_byte());
    let reference = builder.unresolved(owner, UnresolvedReferenceKind::Call, &raw, start, end);
    push_site(builder, reference, start, frame, shadowed, shape);
}

fn call_shape(
    builder: &mut Builder<'_>,
    src: &Src<'_>,
    function: Node<'_>,
    frame: &Frame,
) -> (Shape, bool) {
    let mut function = function;
    while function.kind() == "generic_function" {
        match function.child_by_field_name("function") {
            Some(inner) => function = inner,
            None => return (Shape::Opaque, false),
        }
    }
    match function.kind() {
        "identifier" => {
            let name = src.text(function);
            let shadowed = is_local_name(src, frame, name);
            (Shape::Path(vec![builder.sym(name)]), shadowed)
        }
        "scoped_identifier" => (path_shape(builder, src, function), false),
        "field_expression" => {
            let (Some(value), Some(field)) = (
                function.child_by_field_name("value"),
                function.child_by_field_name("field"),
            ) else {
                return (Shape::Opaque, false);
            };
            if field.kind() != "field_identifier" {
                return (Shape::Opaque, false);
            }
            let receiver = receiver_of(builder, src, value, frame);
            let name = builder.sym(src.text(field));
            (Shape::Method { receiver, name }, false)
        }
        _ => (Shape::Opaque, false),
    }
}

fn is_local_name(src: &Src<'_>, frame: &Frame, name: &str) -> bool {
    src.bound.contains(name)
        || frame
            .func
            .as_ref()
            .is_some_and(|func| func.counts.contains_key(name) || func.nested_fns.contains(name))
}

fn receiver_of(
    builder: &mut Builder<'_>,
    src: &Src<'_>,
    value: Node<'_>,
    frame: &Frame,
) -> Receiver {
    match value.kind() {
        "self" => Receiver::SelfValue,
        "identifier" => {
            let name = src.text(value);
            if src.bound.contains(name) {
                return Receiver::Unknown;
            }
            let Some(func) = frame.func.as_ref() else {
                return Receiver::Unknown;
            };
            if func.counts.get(name).copied() != Some(1) {
                return Receiver::Unknown;
            }
            match func.types.get(name) {
                Some(LocalType::Typed(path)) => Receiver::Typed(intern_path(builder, path)),
                Some(LocalType::Ctor { ty, assoc }) => Receiver::Ctor {
                    ty: intern_path(builder, ty),
                    assoc: builder.sym(assoc),
                },
                None => Receiver::Unknown,
            }
        }
        "field_expression" => {
            let mut fields = Vec::new();
            let mut cursor_node = value;
            loop {
                let Some(field) = cursor_node.child_by_field_name("field") else {
                    return Receiver::Unknown;
                };
                if field.kind() != "field_identifier" {
                    return Receiver::Unknown;
                }
                fields.push(src.text(field).to_owned());
                let Some(inner) = cursor_node.child_by_field_name("value") else {
                    return Receiver::Unknown;
                };
                match inner.kind() {
                    "self" => break,
                    "field_expression" => cursor_node = inner,
                    _ => return Receiver::Unknown,
                }
            }
            fields.reverse();
            Receiver::SelfField(intern_path(builder, &fields))
        }
        _ => Receiver::Unknown,
    }
}

// ------------------------------------------------------------------ macros

#[derive(Clone, Copy, PartialEq, Eq)]
enum MacroKind {
    /// Comma (or semicolon) separated expressions.
    Expressions,
    /// Only the first argument is an expression (`matches!`).
    FirstOnly,
    /// `json!`: values after `:` and `,`, recursing into `{}` and `[]`.
    Json,
}

fn macro_kind(name: &str) -> Option<MacroKind> {
    if EXPRESSION_MACROS.contains(&name) {
        Some(MacroKind::Expressions)
    } else if name == "matches" {
        Some(MacroKind::FirstOnly)
    } else if name == "json" {
        Some(MacroKind::Json)
    } else {
        None
    }
}

fn record_macro(
    builder: &mut Builder<'_>,
    src: &Src<'_>,
    node: Node<'_>,
    owner: &FactKey,
    frame: &Frame,
    depth: u8,
) {
    if depth >= MAX_MACRO_DEPTH {
        return;
    }
    let Some(name) = node.child_by_field_name("macro") else {
        return;
    };
    let name = src.text(name);
    let name = name.rsplit("::").next().unwrap_or(name);
    let Some(kind) = macro_kind(name) else {
        return;
    };
    let mut cursor = node.walk();
    let Some(tree) = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "token_tree")
        .last()
    else {
        return;
    };
    let mut chunks = Vec::new();
    macro_chunks(tree, kind, &mut chunks);
    if kind == MacroKind::FirstOnly {
        chunks.truncate(1);
    }
    for (start, end) in chunks {
        if builder.macro_calls >= MAX_MACRO_CALLS_PER_FILE {
            return;
        }
        parse_chunk(builder, src, start, end, owner, frame, depth);
    }
}

/// Byte ranges of the expression arguments inside a macro's token tree.
fn macro_chunks<'t>(tree: Node<'t>, kind: MacroKind, out: &mut Vec<(usize, usize)>) {
    let count = tree.child_count();
    if count < 2 {
        return;
    }
    let mut segments: Vec<Vec<Node<'t>>> = vec![Vec::new()];
    for index in 1..count - 1 {
        let Some(child) = tree.child(index) else {
            continue;
        };
        let separator = match kind {
            MacroKind::Json => matches!(child.kind(), "," | ":"),
            _ => matches!(child.kind(), "," | ";"),
        };
        if separator {
            segments.push(Vec::new());
        } else if let Some(last) = segments.last_mut() {
            last.push(child);
        }
    }
    for tokens in segments {
        let (Some(first), Some(last)) = (tokens.first(), tokens.last()) else {
            continue;
        };
        let group = first.kind() == "token_tree"
            && first
                .child(0)
                .is_some_and(|open| matches!(open.kind(), "{" | "["));
        if kind == MacroKind::Json && tokens.len() == 1 && group {
            macro_chunks(*first, kind, out);
            continue;
        }
        out.push((first.start_byte(), last.end_byte()));
    }
}

fn parse_chunk(
    builder: &mut Builder<'_>,
    src: &Src<'_>,
    start: usize,
    end: usize,
    owner: &FactKey,
    frame: &Frame,
    depth: u8,
) {
    let Some(text) = src.bytes.get(start..end) else {
        return;
    };
    // A call needs parentheses; most `assert!` arguments are plain values.
    if !text.contains(&b'(') {
        return;
    }
    let mut buffer = Vec::with_capacity(WRAPPER_PREFIX.len() + text.len() + WRAPPER_SUFFIX.len());
    buffer.extend_from_slice(WRAPPER_PREFIX);
    buffer.extend_from_slice(text);
    buffer.extend_from_slice(WRAPPER_SUFFIX);
    let Some(parser) = builder.rust_parser() else {
        return;
    };
    let Some(tree) = parser.parse(&buffer, None) else {
        return;
    };
    let root = tree.root_node();
    // A chunk that is not one clean expression (a pattern, `name: value`, a
    // tracing field) yields no facts rather than guessed ones.
    if root.has_error() {
        return;
    }
    let mut bound = src.bound.clone();
    collect_bound_names(&buffer, root, &mut bound);
    let origin = isize::try_from(src.orig(start)).unwrap_or(0)
        - isize::try_from(WRAPPER_PREFIX.len()).unwrap_or(0);
    let chunk = Src {
        bytes: &buffer,
        origin,
        bound: &bound,
    };
    visit_chunk(builder, &chunk, root, owner, frame, depth);
}

fn collect_bound_names(buffer: &[u8], node: Node<'_>, out: &mut HashSet<String>) {
    let scratch = HashSet::new();
    let src = Src {
        bytes: buffer,
        origin: 0,
        bound: &scratch,
    };
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        match current.kind() {
            "let_declaration" | "for_expression" | "let_condition" => {
                if let Some(pattern) = current.child_by_field_name("pattern") {
                    let mut names = Vec::new();
                    pattern_names(&src, pattern, &mut names);
                    out.extend(names);
                }
            }
            "closure_parameters" | "match_pattern" => {
                let mut names = Vec::new();
                pattern_names(&src, current, &mut names);
                out.extend(names);
            }
            _ => {}
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
}

fn visit_chunk(
    builder: &mut Builder<'_>,
    chunk: &Src<'_>,
    node: Node<'_>,
    owner: &FactKey,
    frame: &Frame,
    depth: u8,
) {
    match node.kind() {
        "call_expression" => {
            if builder.macro_calls >= MAX_MACRO_CALLS_PER_FILE {
                return;
            }
            builder.macro_calls += 1;
            record_call(builder, chunk, node, owner, frame);
        }
        "macro_invocation" => {
            record_macro(builder, chunk, node, owner, frame, depth + 1);
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_chunk(builder, chunk, child, owner, frame, depth);
    }
}
