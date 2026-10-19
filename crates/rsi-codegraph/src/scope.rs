//! Syntax-local Rust facts recorded during extraction so the resolver never
//! re-parses source. None of this is persisted: it is recomputed from exact
//! bytes with the raw facts, and only resolved relations and unresolved sites
//! are staged.
//!
//! Strings are interned per file ([`Sym`]) and file-local nodes are named by
//! their index in `PerFileFacts::nodes` ([`NodeIdx`]) to keep the cached raw
//! facts of a large checkout small.

use crate::NodeKind;

/// Index into [`RustScope::syms`].
pub type Sym = u32;
/// Index into the owning file's `nodes`.
pub type NodeIdx = u32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RustScope {
    pub syms: Vec<String>,
    /// Module-level items that form a module's namespace.
    pub items: Vec<ItemDecl>,
    pub modules: Vec<ModuleDecl>,
    /// Flattened `use` leaves.
    pub uses: Vec<UseDecl>,
    pub impls: Vec<ImplDecl>,
    pub traits: Vec<TraitDecl>,
    pub structs: Vec<StructDecl>,
    pub fns: Vec<FnDecl>,
    /// One per recorded Rust reference that the resolver can place.
    pub sites: Vec<RefSite>,
}

impl RustScope {
    #[must_use]
    pub fn text(&self, sym: Sym) -> &str {
        self.syms.get(sym as usize).map_or("", String::as_str)
    }

    #[must_use]
    pub fn path_text(&self, path: &[Sym]) -> Vec<&str> {
        path.iter().map(|sym| self.text(*sym)).collect()
    }
}

/// A named item declared directly in a module (not in a function or impl body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemDecl {
    pub node: NodeIdx,
    pub name: Sym,
    pub kind: NodeKind,
    /// Interned scope string of the declaring module.
    pub module: Sym,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDecl {
    pub node: NodeIdx,
    pub name: Sym,
    /// Interned scope string of the declaring module.
    pub parent: Sym,
    /// `mod name { .. }` (true) or `mod name;` (false).
    pub inline: bool,
    /// `#[path = ".."]` on a file module.
    pub path_attr: Option<Sym>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseDecl {
    /// Index of the declaration's `Import` unresolved reference.
    pub reference: u32,
    pub module: Sym,
    pub path: Vec<Sym>,
    /// Bound name when renamed (`as x`); `_` binds nothing.
    pub alias: Option<Sym>,
    pub glob: bool,
    /// Declared inside a function body: it names nothing outside it.
    pub local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDecl {
    pub node: NodeIdx,
    pub name: Sym,
    pub has_self: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplDecl {
    pub node: NodeIdx,
    pub module: Sym,
    pub generics: Vec<Sym>,
    pub self_ty: Option<Vec<Sym>>,
    pub trait_ty: Option<Vec<Sym>>,
    /// Index of the `ImplTrait` unresolved reference.
    pub trait_reference: Option<u32>,
    pub methods: Vec<MethodDecl>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitDecl {
    pub node: NodeIdx,
    pub module: Sym,
    /// Methods with default bodies (required methods have no node).
    pub methods: Vec<MethodDecl>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructDecl {
    pub node: NodeIdx,
    pub module: Sym,
    pub fields: Vec<(Sym, Vec<Sym>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnDecl {
    pub node: NodeIdx,
    /// Return type when it is a plain path (wrappers such as `Arc` removed).
    pub ret: Option<Vec<Sym>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfRef {
    None,
    Impl(u32),
    Trait(u32),
}

/// A receiver whose type the extractor could state from syntax alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Receiver {
    SelfValue,
    /// `self.a.b`
    SelfField(Vec<Sym>),
    /// A local bound exactly once with a declared type.
    Typed(Vec<Sym>),
    /// A local bound exactly once to `Type::assoc(..)`.
    Ctor {
        ty: Vec<Sym>,
        assoc: Sym,
    },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// `f`, `a::b::f`, a type path or a trait path.
    Path(Vec<Sym>),
    Method {
        receiver: Receiver,
        name: Sym,
    },
    Opaque,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSite {
    /// Index into the file's `unresolved_references` at extraction time.
    pub reference: u32,
    /// Guards against stale alignment: the reference's span start.
    pub start_byte: u32,
    pub module: Sym,
    pub self_ref: SelfRef,
    /// A bare name that is also a local binding or nested function.
    pub shadowed: bool,
    pub shape: Shape,
}
