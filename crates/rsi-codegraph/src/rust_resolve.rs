//! Cross-file Rust reference resolution.
//!
//! The resolver builds a per-crate module tree from the extractor's scope
//! facts (`mod` items, `#[path]`, Cargo targets), resolves `use` imports
//! (aliases, groups, globs, `self`/`super`/`crate`, workspace crates), then
//! places call, type and trait references.
//!
//! A reference becomes an `Extracted` edge only when exactly one target is
//! proven by the module tree, the imports, `Self`, or a receiver whose type the
//! source states. A bounded set of candidates (same-named overloads, or a
//! method name when the receiver type is unknown) becomes `Inferred` edges
//! whose evidence names the candidate count. Everything else stays unresolved.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::cargo_metadata::CargoWorkspace;
use crate::scope::{NodeIdx, Receiver, RefSite, SelfRef, Shape, Sym};
use crate::{
    EvidenceFact, FactKey, FactProvenance, NodeKind, RelationFact, RelationKind,
    UnresolvedReferenceFact, UnresolvedReferenceKind,
    staged::{FileDiagnostic, PerFileFacts},
};

type FileId = usize;
type ModId = usize;
type CrateId = usize;

/// The most same-named candidates an `Inferred` edge set may name.
pub(crate) const MAX_INFERRED_CANDIDATES: usize = 3;
const MAX_WALK_DEPTH: usize = 24;

/// Method names that std and ubiquitous traits define on many types. A project
/// method with the same name is not evidence for a call on an unknown receiver.
const COMMON_METHOD_NAMES: &[&str] = &[
    "new",
    "default",
    "from",
    "into",
    "try_from",
    "try_into",
    "clone",
    "clone_from",
    "to_string",
    "to_owned",
    "fmt",
    "eq",
    "ne",
    "cmp",
    "partial_cmp",
    "hash",
    "len",
    "is_empty",
    "get",
    "get_mut",
    "insert",
    "remove",
    "push",
    "pop",
    "iter",
    "iter_mut",
    "into_iter",
    "next",
    "map",
    "map_err",
    "and_then",
    "unwrap",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "expect",
    "ok",
    "err",
    "ok_or",
    "ok_or_else",
    "as_str",
    "as_ref",
    "as_mut",
    "as_bytes",
    "as_slice",
    "borrow",
    "borrow_mut",
    "lock",
    "read",
    "write",
    "send",
    "recv",
    "contains",
    "contains_key",
    "join",
    "split",
    "trim",
    "starts_with",
    "ends_with",
    "parse",
    "extend",
    "clear",
    "drain",
    "retain",
    "sort",
    "first",
    "last",
    "entry",
    "or_insert",
    "take",
    "replace",
    "set",
    "with",
    "build",
    "run",
    "drop",
    "deref",
    "index",
    "add",
    "sub",
    "mul",
    "div",
    "neg",
    "not",
    "poll",
    "close",
    "open",
    "flush",
    "from_str",
    "display",
    "path",
    "name",
    "id",
    "kind",
    "key",
    "value",
    "values",
    "keys",
    "min",
    "max",
    "abs",
    "to_vec",
    "collect",
    "filter",
    "fold",
    "count",
    "enumerate",
    "zip",
    "rev",
    "chain",
    "skip",
    "any",
    "all",
    "find",
    "position",
    "cloned",
    "copied",
    "flatten",
    "filter_map",
    "flat_map",
    "for_each",
    "sum",
    "product",
    "unwrap_err",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "lines",
    "chars",
    "bytes",
    "append",
    "truncate",
    "swap",
    "resize",
    "context",
    "with_context",
];

/// Names that start a std or extern-crate path outside the corpus.
const EXTERNAL_ROOTS: &[&str] = &["std", "core", "alloc", "proc_macro", "test"];

/// Prelude and ubiquitous std items; a path through one never reaches the corpus.
const PRELUDE_NAMES: &[&str] = &[
    "Vec", "String", "Option", "Result", "Some", "None", "Ok", "Err", "Box", "Arc", "Rc", "Cow",
    "HashMap", "HashSet", "BTreeMap", "BTreeSet", "Default", "From", "Into", "Iterator",
    "ToString", "Drop", "Clone",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ItemRef {
    file: FileId,
    node: NodeIdx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Entity {
    Module(ModId),
    Item(ItemRef),
    /// A path that provably leaves the corpus (std, an extern crate).
    External,
}

pub(crate) struct Outcome {
    pub relations: Vec<RelationFact>,
    /// Indexes into the file's `unresolved_references` proven by this pass.
    pub dropped: HashSet<usize>,
}

pub(crate) fn resolve(files: &[PerFileFacts], cargo: Option<&CargoWorkspace>) -> Vec<Outcome> {
    let tree = Tree::build(files, cargo);
    let index = Index::build(&tree);
    let resolver = Resolver {
        tree: &tree,
        index: &index,
    };
    (0..files.len())
        .map(|file| resolver.file_outcome(file))
        .collect()
}

// ------------------------------------------------------------------- tree

struct CrateData {
    name: String,
    root: ModId,
    is_lib: bool,
    package: String,
    deps: HashMap<String, CrateId>,
}

struct UseRef {
    file: FileId,
    index: usize,
}

#[derive(Default)]
struct ModData {
    crate_id: CrateId,
    parent: Option<ModId>,
    node: Option<ItemRef>,
    children: HashMap<String, ModId>,
    items: HashMap<String, Vec<ItemRef>>,
    uses: Vec<UseRef>,
    filled: bool,
}

struct RootSpec {
    crate_name: String,
    path: String,
    is_lib: bool,
    package: String,
}

struct Tree<'a> {
    files: &'a [PerFileFacts],
    crates: Vec<CrateData>,
    modules: Vec<ModData>,
    /// Per file: interned scope string to the module it names.
    module_syms: Vec<HashMap<Sym, ModId>>,
    /// The crate each Rust file belongs to.
    file_crate: Vec<Option<CrateId>>,
    memo: RefCell<HashMap<(ModId, String, bool), Vec<Entity>>>,
    active: RefCell<HashSet<(ModId, String, bool)>>,
}

impl<'a> Tree<'a> {
    #[allow(clippy::too_many_lines)] // Roots, claiming, orphans and dependency wiring share one pass.
    fn build(files: &'a [PerFileFacts], cargo: Option<&CargoWorkspace>) -> Self {
        let rust_files = files
            .iter()
            .enumerate()
            .filter(|(_, file)| {
                file.diagnostic == FileDiagnostic::Parsed
                    && file.file.relative_path.ends_with(".rs")
                    && file.nodes.first().is_some_and(|n| n.kind == NodeKind::File)
            })
            .map(|(id, file)| (file.file.relative_path.as_str(), id))
            .collect::<HashMap<_, _>>();
        let mut tree = Self {
            files,
            crates: Vec::new(),
            modules: Vec::new(),
            module_syms: vec![HashMap::new(); files.len()],
            file_crate: vec![None; files.len()],
            memo: RefCell::new(HashMap::new()),
            active: RefCell::new(HashSet::new()),
        };
        let metadata = cargo.filter(|metadata| !metadata.packages.is_empty());
        let mut roots = metadata.map_or_else(
            || convention_roots(files, &rust_files),
            |metadata| metadata_roots(metadata, &rust_files),
        );
        roots.sort_by(|left, right| left.path.cmp(&right.path));
        let mut claimed = vec![false; files.len()];
        for root in &roots {
            let Some(&file) = rust_files.get(root.path.as_str()) else {
                continue;
            };
            if claimed[file] {
                continue;
            }
            claimed[file] = true;
            let crate_id = tree.crates.len();
            let module = tree.new_module(crate_id, None);
            tree.crates.push(CrateData {
                name: root.crate_name.clone(),
                root: module,
                is_lib: root.is_lib,
                package: root.package.clone(),
                deps: HashMap::new(),
            });
            let node = ItemRef { file, node: 0 };
            tree.fill_module(
                module,
                file,
                &root.path,
                Some(node),
                true,
                &rust_files,
                &mut claimed,
            );
        }
        tree.attach_orphans(&roots, &rust_files, &mut claimed);
        tree.wire_dependencies(metadata);
        tree
    }

    fn new_module(&mut self, crate_id: CrateId, parent: Option<ModId>) -> ModId {
        self.modules.push(ModData {
            crate_id,
            parent,
            ..ModData::default()
        });
        self.modules.len() - 1
    }

    fn file_text(&self, file: FileId, sym: Sym) -> &str {
        self.files[file].scope.text(sym)
    }

    /// Register one module's items, imports and declared child modules.
    #[allow(clippy::too_many_arguments)]
    fn fill_module(
        &mut self,
        module: ModId,
        file: FileId,
        scope: &str,
        node: Option<ItemRef>,
        root_like: bool,
        rust_files: &HashMap<&str, FileId>,
        claimed: &mut [bool],
    ) {
        if self.modules[module].filled {
            return;
        }
        self.modules[module].filled = true;
        self.modules[module].node = node;
        let crate_id = self.modules[module].crate_id;
        if self.file_crate[file].is_none() {
            self.file_crate[file] = Some(crate_id);
        }
        let files = self.files;
        let facts = &files[file];
        if let Some(sym) = facts.scope.syms.iter().position(|text| text == scope)
            && let Ok(sym) = Sym::try_from(sym)
        {
            self.module_syms[file].insert(sym, module);
        }
        let mut items: Vec<(String, ItemRef)> = Vec::new();
        for item in &facts.scope.items {
            if facts.scope.text(item.module) != scope || item.kind == NodeKind::Module {
                continue;
            }
            items.push((
                facts.scope.text(item.name).to_owned(),
                ItemRef {
                    file,
                    node: item.node,
                },
            ));
        }
        let mut uses = Vec::new();
        for (index, decl) in facts.scope.uses.iter().enumerate() {
            if !decl.local && facts.scope.text(decl.module) == scope {
                uses.push(UseRef { file, index });
            }
        }
        let mut declared: Vec<(String, bool, Option<String>, NodeIdx)> = Vec::new();
        for decl in &facts.scope.modules {
            if facts.scope.text(decl.parent) == scope {
                declared.push((
                    facts.scope.text(decl.name).to_owned(),
                    decl.inline,
                    decl.path_attr.map(|path| facts.scope.text(path).to_owned()),
                    decl.node,
                ));
            }
        }
        let file_path = facts.file.relative_path.clone();
        for (name, item) in items {
            self.modules[module]
                .items
                .entry(name)
                .or_default()
                .push(item);
        }
        self.modules[module].uses.extend(uses);
        let directory = children_directory(&file_path, scope, root_like);
        for (name, inline, path_attr, decl_node) in declared {
            let child_node = Some(ItemRef {
                file,
                node: decl_node,
            });
            if inline {
                let child = self.new_module(crate_id, Some(module));
                self.modules[module].children.insert(name.clone(), child);
                let child_scope = format!("{scope}::{name}");
                self.fill_module(
                    child,
                    file,
                    &child_scope,
                    child_node,
                    root_like,
                    rust_files,
                    claimed,
                );
                continue;
            }
            let candidates = path_attr.map_or_else(
                || {
                    vec![
                        join_path(&directory, &format!("{name}.rs")),
                        join_path(&directory, &format!("{name}/mod.rs")),
                    ]
                },
                |attr| vec![join_path(&parent_directory(&file_path), &attr)],
            );
            let Some((child_path, child_file)) = candidates
                .into_iter()
                .find_map(|path| rust_files.get(path.as_str()).map(|file| (path, *file)))
            else {
                continue;
            };
            if claimed[child_file] {
                continue;
            }
            claimed[child_file] = true;
            let child = self.new_module(crate_id, Some(module));
            self.modules[module].children.insert(name, child);
            let child_root_like = child_path.ends_with("/mod.rs") || child_path == "mod.rs";
            self.fill_module(
                child,
                child_file,
                &child_path,
                child_node,
                child_root_like,
                rust_files,
                claimed,
            );
        }
    }

    /// A file no `mod` item reaches is attached by its path under the crate's
    /// `src` directory, the way a conventional layout declares it.
    fn attach_orphans(
        &mut self,
        roots: &[RootSpec],
        rust_files: &HashMap<&str, FileId>,
        claimed: &mut [bool],
    ) {
        let mut orphans: Vec<(&str, FileId)> = rust_files
            .iter()
            .filter(|(_, file)| !claimed[**file])
            .map(|(path, file)| (*path, *file))
            .collect();
        orphans.sort_by(|left, right| {
            (left.0.matches('/').count(), left.0).cmp(&(right.0.matches('/').count(), right.0))
        });
        let crate_dirs = roots
            .iter()
            .filter(|root| {
                root.path.ends_with("/lib.rs")
                    || root.path.ends_with("/main.rs")
                    || root.path == "src/lib.rs"
                    || root.path == "src/main.rs"
            })
            .filter_map(|root| {
                let directory = parent_directory(&root.path);
                let crate_id = rust_files
                    .get(root.path.as_str())
                    .and_then(|file| self.file_crate[*file])?;
                Some((directory, crate_id))
            })
            .collect::<Vec<_>>();
        for (path, file) in orphans {
            if claimed[file] {
                continue;
            }
            let Some((directory, crate_id)) = crate_dirs
                .iter()
                .filter(|(directory, _)| {
                    directory.is_empty() || path.starts_with(&format!("{directory}/"))
                })
                .max_by_key(|(directory, _)| directory.len())
            else {
                continue;
            };
            let relative = if directory.is_empty() {
                path
            } else {
                &path[directory.len() + 1..]
            };
            let Some(stem) = relative.strip_suffix(".rs") else {
                continue;
            };
            let mut segments = stem.split('/').collect::<Vec<_>>();
            if segments.first() == Some(&"bin") {
                continue;
            }
            if segments.last() == Some(&"mod") {
                segments.pop();
            }
            let Some(last) = segments.pop() else {
                continue;
            };
            let mut current = self.crates[*crate_id].root;
            for segment in segments {
                current = self.child_or_virtual(current, segment, *crate_id);
            }
            let module = self.child_or_virtual(current, last, *crate_id);
            if self.modules[module].filled {
                continue;
            }
            claimed[file] = true;
            let root_like = path.ends_with("/mod.rs");
            self.fill_module(
                module,
                file,
                path,
                Some(ItemRef { file, node: 0 }),
                root_like,
                rust_files,
                claimed,
            );
        }
    }

    fn child_or_virtual(&mut self, parent: ModId, name: &str, crate_id: CrateId) -> ModId {
        if let Some(child) = self.modules[parent].children.get(name) {
            return *child;
        }
        let child = self.new_module(crate_id, Some(parent));
        self.modules[parent].children.insert(name.to_owned(), child);
        child
    }

    fn wire_dependencies(&mut self, metadata: Option<&CargoWorkspace>) {
        let libs = self
            .crates
            .iter()
            .enumerate()
            .filter(|(_, krate)| krate.is_lib)
            .map(|(id, krate)| (krate.package.clone(), krate.name.clone(), id))
            .collect::<Vec<_>>();
        for crate_id in 0..self.crates.len() {
            let mut deps = HashMap::new();
            let package = self.crates[crate_id].package.clone();
            if let Some(metadata) = metadata {
                for dependency in metadata
                    .dependencies
                    .iter()
                    .filter(|dependency| dependency.owner_package_id == package)
                {
                    if let Some((_, _, id)) = libs
                        .iter()
                        .find(|(lib_package, _, _)| *lib_package == dependency.package_id)
                    {
                        deps.insert(dependency.alias.replace('-', "_"), *id);
                    }
                }
                for (lib_package, name, id) in &libs {
                    if *lib_package == package && *id != crate_id {
                        deps.insert(name.clone(), *id);
                    }
                }
            } else {
                for (_, name, id) in &libs {
                    if *id != crate_id {
                        deps.insert(name.clone(), *id);
                    }
                }
            }
            self.crates[crate_id].deps = deps;
        }
    }

    fn module_of(&self, file: FileId, sym: Sym) -> Option<ModId> {
        self.module_syms.get(file)?.get(&sym).copied()
    }

    fn crate_root(&self, module: ModId) -> ModId {
        self.crates[self.modules[module].crate_id].root
    }

    fn node(&self, item: ItemRef) -> Option<&crate::NodeFact> {
        self.files.get(item.file)?.nodes.get(item.node as usize)
    }

    fn kind(&self, item: ItemRef) -> Option<NodeKind> {
        self.node(item).map(|node| node.kind)
    }

    /// Resolve `name` in `module`: declarations, explicit imports, then globs;
    /// with `prelude`, then extern crates and std.
    fn lookup(&self, module: ModId, name: &str, prelude: bool) -> Vec<Entity> {
        let key = (module, name.to_owned(), prelude);
        if let Some(hit) = self.memo.borrow().get(&key) {
            return hit.clone();
        }
        if !self.active.borrow_mut().insert(key.clone()) {
            return Vec::new();
        }
        let result = self.lookup_uncached(module, name, prelude);
        self.active.borrow_mut().remove(&key);
        self.memo.borrow_mut().insert(key, result.clone());
        result
    }

    fn lookup_uncached(&self, module: ModId, name: &str, prelude: bool) -> Vec<Entity> {
        let data = &self.modules[module];
        let mut found: Vec<Entity> = Vec::new();
        if let Some(child) = data.children.get(name) {
            found.push(Entity::Module(*child));
        }
        if let Some(items) = data.items.get(name) {
            found.extend(items.iter().map(|item| Entity::Item(*item)));
        }
        for reference in &data.uses {
            let decl = &self.files[reference.file].scope.uses[reference.index];
            if decl.glob {
                continue;
            }
            let bound = decl.alias.map_or_else(
                || {
                    decl.path
                        .last()
                        .map_or("", |last| self.file_text(reference.file, *last))
                },
                |alias| self.file_text(reference.file, alias),
            );
            if bound != name {
                continue;
            }
            let segments = decl
                .path
                .iter()
                .map(|sym| self.file_text(reference.file, *sym))
                .collect::<Vec<_>>();
            found.extend(self.walk(module, &segments, &|_, _| Vec::new()));
        }
        if found.is_empty() {
            for reference in &data.uses {
                let decl = &self.files[reference.file].scope.uses[reference.index];
                if !decl.glob {
                    continue;
                }
                let segments = decl
                    .path
                    .iter()
                    .map(|sym| self.file_text(reference.file, *sym))
                    .collect::<Vec<_>>();
                for entity in self.walk(module, &segments, &|_, _| Vec::new()) {
                    if let Entity::Module(target) = entity
                        && target != module
                    {
                        found.extend(self.lookup(target, name, false));
                    }
                }
            }
        }
        if found.is_empty() && prelude {
            found.extend(self.extern_name(module, name));
        }
        found.sort_by_key(entity_order);
        found.dedup();
        found
    }

    fn extern_name(&self, module: ModId, name: &str) -> Option<Entity> {
        let krate = &self.crates[self.modules[module].crate_id];
        if let Some(dep) = krate.deps.get(name) {
            return Some(Entity::Module(self.crates[*dep].root));
        }
        (EXTERNAL_ROOTS.contains(&name) || PRELUDE_NAMES.contains(&name))
            .then_some(Entity::External)
    }

    /// Follow `segments` from `module`. `assoc` steps from a type item to its
    /// associated functions; imports pass a step that finds none.
    fn walk(
        &self,
        module: ModId,
        segments: &[&str],
        assoc: &dyn Fn(ItemRef, &str) -> Vec<Entity>,
    ) -> Vec<Entity> {
        let Some((first, rest)) = segments.split_first() else {
            return Vec::new();
        };
        let mut current = match *first {
            "crate" => vec![Entity::Module(self.crate_root(module))],
            "self" => vec![Entity::Module(module)],
            "super" => self.modules[module]
                .parent
                .map(Entity::Module)
                .into_iter()
                .collect(),
            name => self.lookup(module, name, true),
        };
        for (depth, segment) in rest.iter().enumerate() {
            if current.is_empty() || depth > MAX_WALK_DEPTH {
                return Vec::new();
            }
            current = self.step(&current, segment, assoc);
        }
        current
    }

    fn step(
        &self,
        current: &[Entity],
        segment: &str,
        assoc: &dyn Fn(ItemRef, &str) -> Vec<Entity>,
    ) -> Vec<Entity> {
        let mut next = Vec::new();
        for entity in current {
            match entity {
                Entity::Module(module) => match segment {
                    "super" => next.extend(self.modules[*module].parent.map(Entity::Module)),
                    "self" => next.push(Entity::Module(*module)),
                    name => next.extend(self.lookup(*module, name, false)),
                },
                Entity::Item(item) => next.extend(assoc(*item, segment)),
                Entity::External => next.push(Entity::External),
            }
        }
        next.sort_by_key(entity_order);
        next.dedup();
        next
    }
}

fn entity_order(entity: &Entity) -> (u8, usize, u32) {
    match entity {
        Entity::Module(module) => (0, *module, 0),
        Entity::Item(item) => (1, item.file, item.node),
        Entity::External => (2, 0, 0),
    }
}

fn parent_directory(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(String::new, |(directory, _)| directory.to_owned())
}

/// Directory a module's `mod x;` children live in: next to a crate root or
/// `mod.rs`, otherwise under a directory named after the file; each inline
/// module in the scope adds one component.
fn children_directory(file_path: &str, scope: &str, root_like: bool) -> String {
    let directory = parent_directory(file_path);
    let mut base = if root_like {
        directory
    } else {
        let stem = file_path
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".rs"))
            .unwrap_or("");
        join_path(&directory, stem)
    };
    if let Some(inline) = scope.strip_prefix(file_path)
        && let Some(inline) = inline.strip_prefix("::")
    {
        for segment in inline.split("::") {
            base = join_path(&base, segment);
        }
    }
    base
}

fn join_path(directory: &str, name: &str) -> String {
    let mut parts: Vec<&str> = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn metadata_roots(metadata: &CargoWorkspace, rust_files: &HashMap<&str, FileId>) -> Vec<RootSpec> {
    let mut roots = Vec::new();
    for package in &metadata.packages {
        for target in &package.targets {
            if !rust_files.contains_key(target.source_path.as_str()) {
                continue;
            }
            let is_lib = target.kinds.iter().any(|kind| {
                matches!(
                    kind.as_str(),
                    "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                )
            });
            roots.push(RootSpec {
                crate_name: target.name.replace('-', "_"),
                path: target.source_path.clone(),
                is_lib,
                package: package.id.clone(),
            });
        }
    }
    roots
}

/// Cargo's default target layout, for a corpus without Cargo metadata.
fn convention_roots(files: &[PerFileFacts], rust_files: &HashMap<&str, FileId>) -> Vec<RootSpec> {
    let mut package_names: HashMap<String, String> = HashMap::new();
    for file in files {
        let path = file.file.relative_path.as_str();
        if path == "Cargo.toml" || path.ends_with("/Cargo.toml") {
            if let Some(node) = file.nodes.iter().find(|node| node.kind == NodeKind::Crate) {
                package_names.insert(parent_directory(path), node.name.clone());
            }
        }
    }
    let mut roots = Vec::new();
    for path in rust_files.keys() {
        let parts = path.split('/').collect::<Vec<_>>();
        let Some((directory, is_lib)) = convention_root(&parts) else {
            continue;
        };
        let package = package_names.get(&directory).cloned().unwrap_or_else(|| {
            directory
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or("crate")
                .to_owned()
        });
        roots.push(RootSpec {
            crate_name: package.replace('-', "_"),
            path: (*path).to_owned(),
            is_lib,
            package: directory,
        });
    }
    roots
}

/// The package directory and library-ness when `parts` is a Cargo target root.
fn convention_root(parts: &[&str]) -> Option<(String, bool)> {
    let count = parts.len();
    let file = *parts.last()?;
    let directory = |up: usize| parts[..count - up].join("/");
    if count >= 2 {
        let parent = parts[count - 2];
        if parent == "src" && file == "lib.rs" {
            return Some((directory(2), true));
        }
        if parent == "src" && file == "main.rs" {
            return Some((directory(2), false));
        }
        if matches!(parent, "tests" | "examples" | "benches") && file.ends_with(".rs") {
            return Some((directory(2), false));
        }
    }
    if count >= 3 {
        let (grand, parent) = (parts[count - 3], parts[count - 2]);
        if grand == "src" && parent == "bin" && file.ends_with(".rs") {
            return Some((directory(3), false));
        }
        if matches!(grand, "tests" | "examples" | "benches") && file == "main.rs" {
            return Some((directory(3), false));
        }
    }
    if count >= 4 && parts[count - 4] == "src" && parts[count - 3] == "bin" && file == "main.rs" {
        return Some((directory(4), false));
    }
    None
}

// ------------------------------------------------------------------ index

type ImplRef = (FileId, usize);

#[derive(Default)]
struct Index {
    impls_by_type: HashMap<ItemRef, Vec<ImplRef>>,
    impls_of_trait: HashMap<ItemRef, Vec<ImplRef>>,
    impl_self: HashMap<ImplRef, ItemRef>,
    impl_trait: HashMap<ImplRef, ItemRef>,
    structs: HashMap<ItemRef, (FileId, usize)>,
    traits: HashMap<ItemRef, (FileId, usize)>,
    fn_ret: HashMap<ItemRef, Vec<String>>,
    /// Methods callable with method syntax, by name.
    methods_by_name: HashMap<String, Vec<ItemRef>>,
}

impl Index {
    fn build(tree: &Tree<'_>) -> Self {
        let mut index = Self::default();
        for (file, facts) in tree.files.iter().enumerate() {
            if facts.diagnostic != FileDiagnostic::Parsed {
                continue;
            }
            let scope = &facts.scope;
            for (at, decl) in scope.structs.iter().enumerate() {
                index.structs.insert(
                    ItemRef {
                        file,
                        node: decl.node,
                    },
                    (file, at),
                );
            }
            for (at, decl) in scope.traits.iter().enumerate() {
                let item = ItemRef {
                    file,
                    node: decl.node,
                };
                index.traits.insert(item, (file, at));
                for method in decl.methods.iter().filter(|method| method.has_self) {
                    index
                        .methods_by_name
                        .entry(scope.text(method.name).to_owned())
                        .or_default()
                        .push(ItemRef {
                            file,
                            node: method.node,
                        });
                }
            }
            for decl in &scope.fns {
                if let Some(ret) = &decl.ret {
                    index.fn_ret.insert(
                        ItemRef {
                            file,
                            node: decl.node,
                        },
                        ret.iter().map(|sym| scope.text(*sym).to_owned()).collect(),
                    );
                }
            }
            for (at, decl) in scope.impls.iter().enumerate() {
                for method in decl.methods.iter().filter(|method| method.has_self) {
                    index
                        .methods_by_name
                        .entry(scope.text(method.name).to_owned())
                        .or_default()
                        .push(ItemRef {
                            file,
                            node: method.node,
                        });
                }
                let Some(module) = tree.module_of(file, decl.module) else {
                    continue;
                };
                let impl_ref = (file, at);
                if let Some(path) = &decl.self_ty
                    && let Some(item) = unique_type(tree, module, scope.path_text(path))
                    && matches!(tree.kind(item), Some(NodeKind::Struct | NodeKind::Enum))
                {
                    index.impls_by_type.entry(item).or_default().push(impl_ref);
                    index.impl_self.insert(impl_ref, item);
                }
                if let Some(path) = &decl.trait_ty
                    && let Some(item) = unique_type(tree, module, scope.path_text(path))
                    && tree.kind(item) == Some(NodeKind::Trait)
                {
                    index.impls_of_trait.entry(item).or_default().push(impl_ref);
                    index.impl_trait.insert(impl_ref, item);
                }
            }
        }
        for methods in index.methods_by_name.values_mut() {
            methods.sort();
            methods.dedup();
        }
        index
    }
}

/// Exactly one struct, enum, trait or alias item at a module-relative path.
fn unique_type(tree: &Tree<'_>, module: ModId, segments: Vec<&str>) -> Option<ItemRef> {
    let entities = tree.walk(module, &segments, &|_, _| Vec::new());
    unique_item(tree, &entities, |kind| {
        matches!(
            kind,
            NodeKind::Struct | NodeKind::Enum | NodeKind::Trait | NodeKind::TypeAlias
        )
    })
}

fn unique_item(
    tree: &Tree<'_>,
    entities: &[Entity],
    accept: impl Fn(NodeKind) -> bool,
) -> Option<ItemRef> {
    let mut found = None;
    for entity in entities {
        match entity {
            Entity::Item(item) if tree.kind(*item).is_some_and(&accept) => {
                if found.replace(*item).is_some() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    found
}

// --------------------------------------------------------------- resolver

struct Found {
    targets: Vec<ItemRef>,
    tier: FactProvenance,
    label: String,
}

struct Resolver<'a, 'b> {
    tree: &'b Tree<'a>,
    index: &'b Index,
}

impl Resolver<'_, '_> {
    fn file_outcome(&self, file: FileId) -> Outcome {
        let facts = &self.tree.files[file];
        let mut outcome = Outcome {
            relations: Vec::new(),
            dropped: HashSet::new(),
        };
        if facts.diagnostic != FileDiagnostic::Parsed
            || (facts.scope.sites.is_empty() && facts.scope.uses.is_empty())
        {
            return outcome;
        }
        let sites = facts
            .scope
            .sites
            .iter()
            .map(|site| (site.reference as usize, site))
            .collect::<HashMap<_, _>>();
        let mut uses: HashMap<usize, Vec<usize>> = HashMap::new();
        for (at, decl) in facts.scope.uses.iter().enumerate() {
            uses.entry(decl.reference as usize).or_default().push(at);
        }
        for (at, reference) in facts.unresolved_references.iter().enumerate() {
            match reference.kind {
                UnresolvedReferenceKind::Import => {
                    if let Some(leaves) = uses.get(&at) {
                        self.resolve_import(file, at, reference, leaves, &mut outcome);
                    }
                }
                UnresolvedReferenceKind::Call
                | UnresolvedReferenceKind::Type
                | UnresolvedReferenceKind::ImplTrait => {
                    let Some(site) = sites.get(&at) else {
                        continue;
                    };
                    if site.start_byte as usize != reference.span.start_byte {
                        continue;
                    }
                    let Some(module) = self.tree.module_of(file, site.module) else {
                        continue;
                    };
                    let found = match reference.kind {
                        UnresolvedReferenceKind::Call => self.resolve_call(file, module, site),
                        _ => self.resolve_type_like(file, module, site, reference.kind),
                    };
                    if let Some(found) = found {
                        self.emit(at, reference, &found, &mut outcome);
                    }
                }
                _ => {}
            }
        }
        outcome
    }

    fn emit(
        &self,
        at: usize,
        reference: &UnresolvedReferenceFact,
        found: &Found,
        outcome: &mut Outcome,
    ) {
        let kind = match reference.kind {
            UnresolvedReferenceKind::Call => RelationKind::Calls,
            UnresolvedReferenceKind::Type => RelationKind::UsesType,
            UnresolvedReferenceKind::ImplTrait => RelationKind::Implements,
            _ => return,
        };
        let mut emitted = false;
        for target in &found.targets {
            let Some(node) = self.tree.node(*target) else {
                continue;
            };
            if node.key == reference.owner {
                continue;
            }
            outcome.relations.push(relation(
                reference,
                &node.key,
                kind,
                found.tier,
                &found.label,
            ));
            emitted = true;
        }
        // Only a proven site stops being "unresolved"; guesses keep it listed.
        if emitted && found.tier == FactProvenance::Extracted {
            outcome.dropped.insert(at);
        }
    }

    fn resolve_import(
        &self,
        file: FileId,
        at: usize,
        reference: &UnresolvedReferenceFact,
        leaves: &[usize],
        outcome: &mut Outcome,
    ) {
        let facts = &self.tree.files[file];
        let mut all_proven = !leaves.is_empty();
        let mut seen: BTreeSet<(FileId, NodeIdx)> = BTreeSet::new();
        for leaf in leaves {
            let decl = &facts.scope.uses[*leaf];
            let Some(module) = self.tree.module_of(file, decl.module) else {
                all_proven = false;
                continue;
            };
            let segments = facts.scope.path_text(&decl.path);
            let entities = self.tree.walk(module, &segments, &|_, _| Vec::new());
            let mut targets: Vec<(&FactKey, ItemRef)> = Vec::new();
            let mut external = false;
            for entity in &entities {
                match entity {
                    Entity::Module(target) => {
                        if let Some(item) = self.tree.modules[*target].node
                            && let Some(node) = self.tree.node(item)
                        {
                            targets.push((&node.key, item));
                        }
                    }
                    Entity::Item(item) => {
                        if let Some(node) = self.tree.node(*item) {
                            targets.push((&node.key, *item));
                        }
                    }
                    Entity::External => external = true,
                }
            }
            targets.sort_by_key(|(_, item)| *item);
            targets.dedup_by_key(|(_, item)| *item);
            if external || targets.is_empty() || targets.len() > MAX_INFERRED_CANDIDATES {
                all_proven = false;
                continue;
            }
            let tier = if targets.len() == 1 {
                FactProvenance::Extracted
            } else {
                all_proven = false;
                FactProvenance::Inferred
            };
            let label = if tier == FactProvenance::Extracted {
                "resolved import".to_owned()
            } else {
                format!(
                    "inferred: import path names {} items (one per namespace or cfg variant)",
                    targets.len()
                )
            };
            for (key, item) in targets {
                if *key == reference.owner || !seen.insert((item.file, item.node)) {
                    continue;
                }
                outcome.relations.push(relation(
                    reference,
                    key,
                    RelationKind::Imports,
                    tier,
                    &label,
                ));
            }
        }
        if all_proven {
            outcome.dropped.insert(at);
        }
    }

    fn resolve_type_like(
        &self,
        file: FileId,
        module: ModId,
        site: &RefSite,
        kind: UnresolvedReferenceKind,
    ) -> Option<Found> {
        let Shape::Path(path) = &site.shape else {
            return None;
        };
        let scope = &self.tree.files[file].scope;
        let segments = scope.path_text(path);
        if segments.first() == Some(&"Self") {
            return None;
        }
        let entities = self.tree.walk(module, &segments, &|_, _| Vec::new());
        let item = unique_item(self.tree, &entities, |node_kind| {
            if kind == UnresolvedReferenceKind::ImplTrait {
                node_kind == NodeKind::Trait
            } else {
                matches!(
                    node_kind,
                    NodeKind::Struct | NodeKind::Enum | NodeKind::Trait | NodeKind::TypeAlias
                )
            }
        })?;
        Some(Found {
            targets: vec![item],
            tier: FactProvenance::Extracted,
            label: if kind == UnresolvedReferenceKind::ImplTrait {
                "resolved trait path".into()
            } else {
                "resolved type path".into()
            },
        })
    }

    // ---- calls

    fn resolve_call(&self, file: FileId, module: ModId, site: &RefSite) -> Option<Found> {
        let scope = &self.tree.files[file].scope;
        match &site.shape {
            Shape::Path(path) => {
                if site.shadowed && path.len() == 1 {
                    return None;
                }
                let segments = scope.path_text(path);
                let entities = self.path_entities(file, module, site.self_ref, &segments);
                self.callable_found(&entities, "resolved path")
            }
            Shape::Method { receiver, name } => {
                let name = scope.text(*name);
                self.resolve_method(file, module, site, receiver, name)
            }
            Shape::Opaque => None,
        }
    }

    fn path_entities(
        &self,
        file: FileId,
        module: ModId,
        self_ref: SelfRef,
        segments: &[&str],
    ) -> Vec<Entity> {
        let assoc = |item: ItemRef, name: &str| self.assoc_entities(item, name);
        if segments.first() == Some(&"Self") {
            let Some(item) = self.self_item(file, self_ref) else {
                return Vec::new();
            };
            let mut current = vec![Entity::Item(item)];
            for segment in &segments[1..] {
                current = self.tree.step(&current, segment, &assoc);
            }
            return current;
        }
        self.tree.walk(module, segments, &assoc)
    }

    fn self_item(&self, file: FileId, self_ref: SelfRef) -> Option<ItemRef> {
        match self_ref {
            SelfRef::None => None,
            SelfRef::Impl(at) => self.index.impl_self.get(&(file, at as usize)).copied(),
            SelfRef::Trait(at) => {
                let decl = self.tree.files[file].scope.traits.get(at as usize)?;
                Some(ItemRef {
                    file,
                    node: decl.node,
                })
            }
        }
    }

    /// Associated functions named `name` on a type or trait item.
    fn assoc_entities(&self, item: ItemRef, name: &str) -> Vec<Entity> {
        self.owner_methods(item, name, None)
            .into_iter()
            .map(Entity::Item)
            .collect()
    }

    /// Methods named `name` reachable on `item`. `want_self` filters by method
    /// syntax. An inherent method hides a trait method of the same name.
    fn owner_methods(&self, item: ItemRef, name: &str, want_self: Option<bool>) -> Vec<ItemRef> {
        let kind = self.tree.kind(item);
        let mut inherent: Vec<ItemRef> = Vec::new();
        let mut traited: Vec<ItemRef> = Vec::new();
        if kind == Some(NodeKind::Trait) {
            if let Some((file, at)) = self.index.traits.get(&item) {
                let scope = &self.tree.files[*file].scope;
                for method in &scope.traits[*at].methods {
                    if scope.text(method.name) == name
                        && want_self.is_none_or(|want| want == method.has_self)
                    {
                        traited.push(ItemRef {
                            file: *file,
                            node: method.node,
                        });
                    }
                }
            }
            for impl_ref in self.index.impls_of_trait.get(&item).into_iter().flatten() {
                self.impl_methods(*impl_ref, name, want_self, &mut traited);
            }
        } else {
            for impl_ref in self.index.impls_by_type.get(&item).into_iter().flatten() {
                let decl = &self.tree.files[impl_ref.0].scope.impls[impl_ref.1];
                if decl.trait_ty.is_none() {
                    self.impl_methods(*impl_ref, name, want_self, &mut inherent);
                    continue;
                }
                let before = traited.len();
                self.impl_methods(*impl_ref, name, want_self, &mut traited);
                if traited.len() == before
                    && let Some(trait_item) = self.index.impl_trait.get(impl_ref)
                    && let Some((file, at)) = self.index.traits.get(trait_item)
                {
                    let scope = &self.tree.files[*file].scope;
                    for method in &scope.traits[*at].methods {
                        if scope.text(method.name) == name
                            && want_self.is_none_or(|want| want == method.has_self)
                        {
                            traited.push(ItemRef {
                                file: *file,
                                node: method.node,
                            });
                        }
                    }
                }
            }
        }
        let mut chosen = if inherent.is_empty() {
            traited
        } else {
            inherent
        };
        chosen.sort();
        chosen.dedup();
        chosen
    }

    fn impl_methods(
        &self,
        impl_ref: ImplRef,
        name: &str,
        want_self: Option<bool>,
        out: &mut Vec<ItemRef>,
    ) {
        let scope = &self.tree.files[impl_ref.0].scope;
        for method in &scope.impls[impl_ref.1].methods {
            if scope.text(method.name) == name
                && want_self.is_none_or(|want| want == method.has_self)
            {
                out.push(ItemRef {
                    file: impl_ref.0,
                    node: method.node,
                });
            }
        }
    }

    fn callable_found(&self, entities: &[Entity], label: &str) -> Option<Found> {
        if entities.is_empty() {
            return None;
        }
        let mut targets = Vec::new();
        for entity in entities {
            match entity {
                Entity::Item(item)
                    if matches!(
                        self.tree.kind(*item),
                        Some(NodeKind::Function | NodeKind::Method)
                    ) =>
                {
                    targets.push(*item);
                }
                // A module, a constructor, or a path leaving the corpus: no edge.
                _ => return None,
            }
        }
        targets.sort();
        targets.dedup();
        self.tiered(targets, label, "ambiguous: the path names")
    }

    /// One target is proven; a few are inferred overloads; more stay unresolved.
    fn tiered(&self, targets: Vec<ItemRef>, label: &str, ambiguity: &str) -> Option<Found> {
        match targets.len() {
            0 => None,
            1 => Some(Found {
                targets,
                tier: FactProvenance::Extracted,
                label: label.into(),
            }),
            count if count <= MAX_INFERRED_CANDIDATES => Some(Found {
                label: format!("inferred: {ambiguity} {count} candidates, none provably chosen"),
                targets,
                tier: FactProvenance::Inferred,
            }),
            _ => None,
        }
    }

    fn resolve_method(
        &self,
        file: FileId,
        module: ModId,
        site: &RefSite,
        receiver: &Receiver,
        name: &str,
    ) -> Option<Found> {
        let scope = &self.tree.files[file].scope;
        match receiver {
            Receiver::SelfValue => {
                let item = self.self_item(file, site.self_ref)?;
                self.typed_method(item, name, "resolved Self method")
            }
            Receiver::SelfField(fields) => {
                let mut current = self.self_item(file, site.self_ref)?;
                for field in fields {
                    let (struct_file, at) = self.index.structs.get(&current)?;
                    let facts = &self.tree.files[*struct_file].scope;
                    let decl = &facts.structs[*at];
                    let (_, path) = decl
                        .fields
                        .iter()
                        .find(|(field_name, _)| facts.text(*field_name) == scope.text(*field))?;
                    let struct_module = self.tree.module_of(*struct_file, decl.module)?;
                    let segments = facts.path_text(path);
                    current =
                        self.type_item(*struct_file, struct_module, SelfRef::None, &segments)?;
                }
                self.typed_method(current, name, "resolved field type method")
            }
            Receiver::Typed(path) => {
                let segments = scope.path_text(path);
                let item = self.type_item(file, module, site.self_ref, &segments)?;
                self.typed_method(item, name, "resolved receiver type method")
            }
            Receiver::Ctor { ty, assoc } => {
                let segments = scope.path_text(ty);
                let item = self.type_item(file, module, site.self_ref, &segments)?;
                let constructors = self.owner_methods(item, scope.text(*assoc), Some(false));
                let [constructor] = constructors.as_slice() else {
                    return None;
                };
                let ret = self.index.fn_ret.get(constructor)?;
                let own_name = &self.tree.node(item)?.name;
                let last = ret.last()?;
                if ret.len() != 1 || (last.as_str() != "Self" && last != own_name) {
                    return None;
                }
                self.typed_method(item, name, "resolved constructed receiver method")
            }
            Receiver::Unknown => self.infer_by_name(file, name),
        }
    }

    fn typed_method(&self, item: ItemRef, name: &str, label: &str) -> Option<Found> {
        let targets = self.owner_methods(item, name, Some(true));
        self.tiered(targets, label, "the receiver type has")
    }

    /// Exactly one struct, enum or trait the path names from this scope.
    fn type_item(
        &self,
        file: FileId,
        module: ModId,
        self_ref: SelfRef,
        segments: &[&str],
    ) -> Option<ItemRef> {
        let entities = self.path_entities(file, module, self_ref, segments);
        unique_item(self.tree, &entities, |kind| {
            matches!(kind, NodeKind::Struct | NodeKind::Enum | NodeKind::Trait)
        })
    }

    /// The receiver type is unknown: name the same-named project methods, but
    /// only when there are few and the name is not a std staple.
    fn infer_by_name(&self, file: FileId, name: &str) -> Option<Found> {
        if COMMON_METHOD_NAMES.contains(&name) {
            return None;
        }
        let all = self.index.methods_by_name.get(name)?;
        let crate_id = self.tree.file_crate[file];
        let same = all
            .iter()
            .copied()
            .filter(|item| crate_id.is_some() && self.tree.file_crate[item.file] == crate_id)
            .collect::<Vec<_>>();
        let (pool, scope_label) = if same.is_empty() {
            (all.clone(), "in the workspace")
        } else {
            (same, "in the same crate")
        };
        let count = pool.len();
        if count == 0 || count > MAX_INFERRED_CANDIDATES {
            return None;
        }
        Some(Found {
            targets: pool,
            tier: FactProvenance::Inferred,
            label: format!(
                "inferred: receiver type unknown; {count} method(s) named `{name}` {scope_label}"
            ),
        })
    }
}

fn relation(
    reference: &UnresolvedReferenceFact,
    target: &FactKey,
    kind: RelationKind,
    tier: FactProvenance,
    label: &str,
) -> RelationFact {
    let digest = blake3::hash(format!("{}\0{}\0{kind:?}", reference.key.0, target.0).as_bytes())
        .to_hex()
        .to_string();
    RelationFact {
        key: FactKey(format!("v1:r:{digest}")),
        owner_file: reference.span.path.clone(),
        site_anchor: format!("v1:{digest}"),
        kind,
        source: reference.owner.clone(),
        target: target.clone(),
        provenance: tier,
        evidence: vec![EvidenceFact {
            label: label.into(),
            span: reference.span.clone(),
        }],
    }
}
