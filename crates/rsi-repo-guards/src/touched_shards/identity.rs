//! Bounded library item identity. Names alone never prove a call edge when
//! imports, inline modules or reexports can supply different items.
use super::{
    filters::REEXPORT_DEPTH,
    source::{self, Function, SourceTree},
};
use crate::re::{Re, group};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

static OPEN: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"^([ \t]*)(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(mod|impl|trait)\b\s*(\w*)")
});
static USE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"(?m)^([ \t]*)(pub(?:\([^)]*\))?[ \t]+)?use[ \t]+([^;]+);"));
static ALIAS: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\btype\s+(\w+)(?:<[^=;]*>)?\s*=\s*&?\s*((?:\w+::)*\w+)"));
static WORD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b[A-Za-z_][A-Za-z_0-9]*\b"));
static QUALIFIER: LazyLock<Re> = LazyLock::new(|| Re::new(r"((?:[A-Za-z_]\w*\s*::\s*)+)$"));
static GENERIC_ARGS: LazyLock<Re> = LazyLock::new(|| Re::new(r"::<[^<>]*>"));

pub type Item = (String, String);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub path: Vec<String>,
    pub bound: Option<String>,
    pub glob: bool,
}

/// Expand `a::{b, c as d, e::*, self}` without splitting nested use trees.
/// Malformed syntax is an error, never an empty successful import inventory.
pub fn expand_use(tree: &str) -> Result<Vec<Import>, String> {
    fn expand(tree: &str, prefix: &[String], depth: usize) -> Result<Vec<Import>, String> {
        if depth > 64 {
            return Err("use tree nesting exceeds 64".into());
        }
        let tree = tree.trim();
        if let Some(open) = tree.find('{') {
            if !tree.ends_with('}') {
                return Err(format!("unclosed use tree: {tree}"));
            }
            let mut head = prefix.to_vec();
            head.extend(
                tree[..open]
                    .split("::")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            );
            let inside = &tree[open + 1..tree.len() - 1];
            let mut level = 0usize;
            let mut first = 0;
            let mut found = Vec::new();
            for (index, ch) in inside
                .char_indices()
                .chain(std::iter::once((inside.len(), ',')))
            {
                match ch {
                    '{' => level += 1,
                    '}' => level = level.checked_sub(1).ok_or("unbalanced use tree")?,
                    ',' if level == 0 => {
                        if !inside[first..index].trim().is_empty() {
                            found.extend(expand(&inside[first..index], &head, depth + 1)?);
                        }
                        first = index + 1;
                    }
                    _ => (),
                }
            }
            if level != 0 {
                return Err("unbalanced use tree".into());
            }
            return Ok(found);
        }
        if tree.contains(['}', '{', ';']) {
            return Err(format!("invalid use tree: {tree}"));
        }
        let (tree, alias) = tree
            .split_once(" as ")
            .map_or((tree, None), |(p, a)| (p.trim(), Some(a.trim())));
        let mut path = prefix.to_vec();
        let mut parts: Vec<_> = tree
            .split("::")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        let glob = parts.last() == Some(&"*");
        if glob || parts.last() == Some(&"self") {
            parts.pop();
        }
        path.extend(parts.into_iter().map(str::to_owned));
        if path.is_empty() {
            return Err("empty use path".into());
        }
        let bound = if glob {
            None
        } else {
            alias.map(str::to_owned).or_else(|| path.last().cloned())
        };
        Ok(vec![Import { path, bound, glob }])
    }
    expand(
        &tree.split_whitespace().collect::<Vec<_>>().join(" "),
        &[],
        0,
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImplKind {
    Inherent,
    Trait,
    TraitDefinition,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplOwner {
    pub kind: ImplKind,
    pub name: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Scope {
    pub module: String,
    pub owner: Option<ImplOwner>,
}
#[derive(Clone, Debug)]
pub struct SourceScopes {
    pub module: String,
    pub lines: Vec<Scope>,
    pub modules: BTreeSet<String>,
    pub imports: BTreeMap<String, Vec<Import>>,
}

fn impl_owner(row: &str, kind: &str, name: &str) -> Option<ImplOwner> {
    if kind == "mod" {
        return None;
    }
    if kind == "trait" {
        return Some(ImplOwner {
            kind: ImplKind::TraitDefinition,
            name: Some(name.into()),
        });
    }
    let mut rest = row.split_once("impl")?.1.trim_start();
    if rest.starts_with('<') {
        let mut depth = 0;
        let mut end = None;
        for (i, ch) in rest.char_indices() {
            if ch == '<' {
                depth += 1;
            }
            if ch == '>' {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
        }
        rest = end.map_or("", |end| &rest[end..]);
    }
    let (kind, rest) = rest
        .split_once(" for ")
        .map_or((ImplKind::Inherent, rest), |(_, r)| (ImplKind::Trait, r));
    let matched = Re::new(r"^\s*(?:&\s*(?:mut\s+)?)?(?:dyn\s+)?((?:\w+::)*\w+)").search(rest);
    Some(matched.map_or(
        ImplOwner {
            kind: ImplKind::Unknown,
            name: None,
        },
        |c| ImplOwner {
            kind,
            name: Some(group(&c, 1).rsplit("::").next().unwrap_or("").into()),
        },
    ))
}

impl SourceScopes {
    pub fn new(module: &str, text: &str) -> Result<Self, String> {
        let code = source::code_text(text)?;
        let mut result = Self {
            module: module.into(),
            lines: Vec::new(),
            modules: BTreeSet::from([module.into()]),
            imports: BTreeMap::new(),
        };
        let mut stack: Vec<(String, String, String, Option<ImplOwner>)> = Vec::new();
        for row in code.split('\n') {
            if let Some(c) = OPEN.search(row) {
                if !row.trim_end().ends_with([';', '}'])
                    && (group(&c, 2) != "mod" || row.trim_end().ends_with('{'))
                {
                    stack.push((
                        group(&c, 1).into(),
                        group(&c, 2).into(),
                        group(&c, 3).into(),
                        impl_owner(row, group(&c, 2), group(&c, 3)),
                    ));
                }
            }
            let module = std::iter::once(module)
                .chain(
                    stack
                        .iter()
                        .filter(|(_, k, _, _)| k == "mod")
                        .map(|(_, _, n, _)| n.as_str()),
                )
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("::");
            result.modules.insert(module.clone());
            result.lines.push(Scope {
                module,
                owner: stack.iter().rev().find_map(|(_, _, _, o)| o.clone()),
            });
            if stack
                .last()
                .is_some_and(|(i, _, _, _)| row.trim_end() == format!("{i}}}"))
            {
                stack.pop();
            }
        }
        for c in USE.find_all(&code) {
            let line = code[..crate::re::start(&c)]
                .bytes()
                .filter(|b| *b == b'\n')
                .count();
            result
                .imports
                .entry(result.lines[line].module.clone())
                .or_default()
                .extend(expand_use(group(&c, 3))?);
        }
        Ok(result)
    }
}

#[derive(Clone, Debug)]
struct Definition {
    method: bool,
    public: bool,
    owner: Option<ImplOwner>,
    receiver: bool,
}
#[derive(Clone, Debug)]
pub struct IndexedFunction {
    pub path: String,
    pub module: String,
    pub effective: String,
    pub function: Function,
    pub owner: Option<ImplOwner>,
    pub words: BTreeSet<String>,
    pub test_helper: bool,
}
#[derive(Clone, Debug)]
pub struct Callers {
    /// Proven functions by file-module identity (as used by the observer walk).
    pub functions: BTreeSet<Item>,
    /// An unresolved reference always retains broad selection, even when some
    /// callers were independently settled. Those callers are ordering hints.
    pub unresolved: Option<String>,
}

/// One index per pinned source tree, reused for every changed item. The cache
/// retains failures as well as successes so diagnostics are stable on repeats.
pub struct ItemReferences {
    pub files: BTreeMap<String, SourceScopes>,
    pub known: BTreeSet<String>,
    pub functions: Vec<IndexedFunction>,
    pub type_counts: BTreeMap<String, usize>,
    pub frequency: BTreeMap<String, usize>,
    definitions: BTreeMap<String, BTreeMap<String, Vec<Definition>>>,
    imports: BTreeMap<String, Vec<Import>>,
    aliases: BTreeMap<String, BTreeSet<String>>,
    cache: BTreeMap<Item, Callers>,
}

impl ItemReferences {
    pub fn new(sources: &SourceTree) -> Result<Self, String> {
        let mut result = Self {
            files: BTreeMap::new(),
            known: BTreeSet::new(),
            functions: Vec::new(),
            type_counts: BTreeMap::new(),
            frequency: BTreeMap::new(),
            definitions: BTreeMap::new(),
            imports: BTreeMap::new(),
            aliases: BTreeMap::new(),
            cache: BTreeMap::new(),
        };
        for (path, text) in &sources.texts {
            let code = source::code_text(text)?;
            for c in Re::new(r"\b(?:struct|enum)\s+(\w+)").find_all(&code) {
                *result.type_counts.entry(group(&c, 1).into()).or_default() += 1;
            }
            let sharded = source::lib_source(path);
            let Some(module) = sources.trace_module(path) else {
                continue;
            };
            let scopes = SourceScopes::new(&module, text)?;
            if sharded {
                result.known.extend(scopes.modules.clone());
            }
            let code_rows: Vec<_> = code.lines().collect();
            for f in source::rust_functions(text, &sources.host) {
                if !code_rows
                    .get(f.start)
                    .is_some_and(|row| Re::new(&format!(r"\bfn\s+{}\b", f.name)).is_match(row))
                {
                    continue;
                }
                let scope = &scopes.lines[f.start];
                let code = source::code_text(&f.body)?;
                let words: BTreeSet<_> = WORD
                    .find_all(&f.body)
                    .iter()
                    .map(|c| group(c, 0).to_owned())
                    .collect();
                let words: BTreeSet<_> = words.into_iter().filter(|w| w != &f.name).collect();
                for word in &words {
                    *result.frequency.entry(word.clone()).or_default() += 1;
                }
                if sharded && !source::is_test(&f.attrs) {
                    let receiver = Re::new(r"^[^(]*\bfn\s+\w+\s*(?:<[^(]*>)?\s*\(\s*(?:&\s*(?:'\w+\s+)?)?(?:mut\s+)?self\b").is_match(&code);
                    result
                        .definitions
                        .entry(f.name.clone())
                        .or_default()
                        .entry(scope.module.clone())
                        .or_default()
                        .push(Definition {
                            method: scope.owner.is_some(),
                            public: f.body.trim_start().starts_with("pub"),
                            owner: scope.owner.clone(),
                            receiver,
                        });
                }
                result.functions.push(IndexedFunction {
                    path: path.clone(),
                    module: module.clone(),
                    effective: scope.module.clone(),
                    test_helper: source::is_test_module(&scope.module)
                        || f.attrs.contains("#[cfg(test)]"),
                    function: f,
                    owner: scope.owner.clone(),
                    words,
                });
            }
            // Leaf names participate in bounded token tracing. Their exported
            // cross-crate identity is not a sharded-library module proof.
            if !sharded {
                continue;
            }
            for c in ALIAS.find_all(&code) {
                result
                    .aliases
                    .entry(group(&c, 1).into())
                    .or_default()
                    .insert(group(&c, 2).rsplit("::").next().unwrap_or("").into());
            }
            for (module, imports) in &scopes.imports {
                result
                    .imports
                    .entry(module.clone())
                    .or_default()
                    .extend(imports.clone());
                for import in imports {
                    if let (Some(bound), Some(last)) = (&import.bound, import.path.last()) {
                        if bound != last {
                            result
                                .aliases
                                .entry(bound.clone())
                                .or_default()
                                .insert(last.clone());
                        }
                    }
                }
            }
            result.files.insert(path.clone(), scopes);
        }
        Ok(result)
    }

    fn imports(&self, module: &str) -> &[Import] {
        self.imports.get(module).map_or(&[], Vec::as_slice)
    }
    fn local_path(&self, parts: &[String], caller: &str, depth: usize) -> bool {
        let Some(head) = parts.first() else {
            return false;
        };
        if matches!(head.as_str(), "crate" | "self" | "super")
            || self.known.contains(&join(caller, head))
        {
            return true;
        }
        depth < 3
            && self.imports(caller).iter().any(|i| {
                i.bound.as_ref() == Some(head)
                    && i.path != parts
                    && self.local_path(&i.path, caller, depth + 1)
            })
    }

    /// Resolve modules through explicit aliases, module reexports and glob
    /// chains. Every recursive path shares a finite depth budget.
    pub fn module_path(&self, parts: &[String], caller: &str) -> Result<Option<String>, String> {
        self.module_at(parts, caller, 0)
    }
    fn module_at(
        &self,
        parts: &[String],
        caller: &str,
        depth: usize,
    ) -> Result<Option<String>, String> {
        if depth >= REEXPORT_DEPTH {
            return Err(format!("module re-export depth exceeds {REEXPORT_DEPTH}"));
        }
        let mut here: Vec<String> = caller
            .split("::")
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let mut rest = parts;
        let mut anchored = false;
        if rest.first().is_some_and(|s| s == "crate") {
            here.clear();
            rest = &rest[1..];
            anchored = true;
        } else {
            while rest.first().is_some_and(|s| s == "self" || s == "super") {
                if rest[0] == "super" && here.pop().is_none() {
                    return Ok(None);
                }
                rest = &rest[1..];
                anchored = true;
            }
        }
        let absolute = here
            .iter()
            .chain(rest)
            .cloned()
            .collect::<Vec<_>>()
            .join("::");
        if self.known.contains(&absolute) {
            return Ok(Some(absolute));
        }
        if !anchored && !rest.is_empty() {
            let bindings: BTreeSet<_> = self
                .imports(caller)
                .iter()
                .filter(|i| i.bound.as_ref() == rest.first())
                .map(|i| i.path.clone())
                .collect();
            if bindings.len() > 1 {
                return Err(format!("conflicting module imports in {caller}"));
            }
            if let Some(binding) = bindings.first() {
                if let Some(mut current) = self.module_at(binding, caller, depth + 1)? {
                    for segment in &rest[1..] {
                        let Some(next) = self.module_segment(&current, segment, depth + 1)? else {
                            return Ok(None);
                        };
                        current = next;
                    }
                    return Ok(Some(current));
                }
                return Ok(None);
            }
        }
        let mut current = String::new();
        for segment in here.iter().chain(rest) {
            let Some(next) = self.module_segment(&current, segment, depth + 1)? else {
                return Ok(None);
            };
            current = next;
        }
        Ok(self.known.contains(&current).then_some(current))
    }
    fn module_segment(
        &self,
        module: &str,
        segment: &str,
        depth: usize,
    ) -> Result<Option<String>, String> {
        if depth >= REEXPORT_DEPTH {
            return Err(format!("module re-export depth exceeds {REEXPORT_DEPTH}"));
        }
        let child = join(module, segment);
        if self.known.contains(&child) {
            return Ok(Some(child));
        }
        let bindings: Vec<_> = self
            .imports(module)
            .iter()
            .filter(|i| i.bound.as_deref() == Some(segment))
            .collect();
        let mut found = BTreeSet::new();
        if !bindings.is_empty() {
            for i in bindings {
                if let Some(target) = self.module_at(&i.path, module, depth + 1)? {
                    found.insert(target);
                }
            }
        } else {
            for i in self.imports(module).iter().filter(|i| i.glob) {
                if let Some(target) = self.module_at(&i.path, module, depth + 1)? {
                    if let Some(next) = self.module_segment(&target, segment, depth + 1)? {
                        found.insert(next);
                    }
                }
            }
        }
        if found.len() > 1 {
            return Err(format!("ambiguous re-exported module {child}"));
        }
        Ok(found.into_iter().next())
    }

    pub fn item_pairs(&self, module: &str, name: &str) -> Result<BTreeSet<Item>, String> {
        self.pairs_at(module, name, &[])
    }
    fn pairs_at(&self, module: &str, name: &str, stack: &[Item]) -> Result<BTreeSet<Item>, String> {
        let key = (module.into(), name.into());
        if self
            .definitions
            .get(name)
            .and_then(|defs| defs.get(module))
            .is_some_and(|defs| defs.iter().any(|d| !d.method))
        {
            return Ok(BTreeSet::from([key]));
        }
        if stack.contains(&key) {
            return Err(format!("a re-export cycle through `{module}::{name}`"));
        }
        if stack.len() >= REEXPORT_DEPTH {
            return Err(format!(
                "a re-export chain of `{module}::{name}` deeper than {REEXPORT_DEPTH} modules"
            ));
        }
        let mut stack = stack.to_vec();
        stack.push(key);
        let mut found = BTreeSet::new();
        let explicit: Vec<_> = self
            .imports(module)
            .iter()
            .filter(|i| i.bound.as_deref() == Some(name) && i.path.len() >= 2)
            .collect();
        if !explicit.is_empty() {
            for import in explicit {
                let head = &import.path[..import.path.len() - 1];
                if let Some(target) = self.module_path(head, module)? {
                    found.extend(self.pairs_at(&target, import.path.last().unwrap(), &stack)?);
                } else if self.local_path(head, module, 0) {
                    return Err(format!(
                        "a re-export `{}` that names no known module",
                        import.path.join("::")
                    ));
                }
            }
        } else {
            for import in self.imports(module).iter().filter(|i| i.glob) {
                if let Some(target) = self.module_path(&import.path, module)? {
                    found.extend(self.pairs_at(&target, name, &stack)?);
                } else if import
                    .path
                    .last()
                    .is_some_and(|s| s.starts_with(char::is_uppercase))
                {
                    continue;
                } else if self.local_path(&import.path, module, 0) {
                    return Err(format!(
                        "a glob import `{}::*` of no known module",
                        import.path.join("::")
                    ));
                }
            }
        }
        if found.len() > 1 {
            return Err(format!(
                "`{module}::{name}` may name {} different items",
                found.len()
            ));
        }
        Ok(found)
    }

    fn identifiers(
        &self,
        ours: &BTreeSet<Item>,
        name: &str,
    ) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
        let mut names = BTreeMap::from([(name.into(), BTreeSet::new())]);
        for _ in 0..REEXPORT_DEPTH {
            let mut grew = false;
            for (module, imports) in &self.imports {
                for i in imports {
                    let (Some(last), Some(bound)) = (i.path.last(), &i.bound) else {
                        continue;
                    };
                    if last == bound
                        || bound == "_"
                        || !names.contains_key(last)
                        || names.get(bound).is_some_and(|m| m.contains(module))
                    {
                        continue;
                    }
                    let head = &i.path[..i.path.len() - 1];
                    let target = if head.is_empty() {
                        Some(module.clone())
                    } else {
                        self.module_path(head, module)?
                    };
                    let Some(target) = target else {
                        if self.local_path(head, module, 0) {
                            return Err(format!(
                                "{module} renames it in a `use` of a module this scan cannot place"
                            ));
                        }
                        continue;
                    };
                    let pairs = self
                        .item_pairs(&target, last)
                        .map_err(|why| format!("{module} renames it in a `use` ({why})"))?;
                    if !pairs.is_disjoint(ours) {
                        grew |= names
                            .entry(bound.clone())
                            .or_default()
                            .insert(module.clone());
                    }
                }
            }
            if !grew {
                return Ok(names);
            }
        }
        Err(format!(
            "a chain of `use .. as` renames of it deeper than {REEXPORT_DEPTH}"
        ))
    }

    pub fn callers(&mut self, owner: &str, name: &str) -> Callers {
        let key = (owner.into(), name.into());
        if let Some(found) = self.cache.get(&key) {
            return found.clone();
        }
        let found = self.scan(owner, name);
        self.cache.insert(key, found.clone());
        found
    }
    fn scan(&self, owner: &str, name: &str) -> Callers {
        let mut found = Callers {
            functions: BTreeSet::new(),
            unresolved: None,
        };
        let scan = (|| -> Result<(), String> {
            if !self.known.contains(owner) {
                return Err("a crate this scan does not read calls it".into());
            }
            let inline: BTreeSet<_> = self
                .files
                .values()
                .filter(|s| s.module == owner)
                .flat_map(|s| s.modules.clone())
                .collect();
            let owned: Vec<_> = self
                .definitions
                .get(name)
                .into_iter()
                .flat_map(|m| m.iter())
                .filter(|(m, _)| inline.contains(*m))
                .flat_map(|(m, defs)| defs.iter().filter(|d| d.public).map(move |d| (m, d)))
                .collect();
            if owned.is_empty() {
                return Err("no definition of it is in a library source".into());
            }
            let ours: BTreeSet<Item> = owned
                .iter()
                .filter(|(_, d)| !d.method)
                .map(|(m, _)| ((*m).clone(), name.into()))
                .collect();
            let methods: Vec<_> = owned
                .iter()
                .filter(|(_, d)| d.method)
                .map(|(_, d)| *d)
                .collect();
            let types: BTreeSet<_> = methods
                .iter()
                .filter_map(|d| {
                    d.owner
                        .as_ref()
                        .filter(|o| o.kind == ImplKind::Inherent)
                        .and_then(|o| o.name.clone())
                })
                .collect();
            let trait_method = methods.iter().any(|d| {
                d.owner
                    .as_ref()
                    .is_none_or(|o| o.kind != ImplKind::Inherent || o.name.is_none())
            });
            let receiver = methods.iter().any(|d| d.receiver);
            let identifiers = self.identifiers(&ours, name)?;
            for (ident, _) in identifiers {
                for f in &self.functions {
                    if !source::lib_source(&f.path) {
                        continue;
                    }
                    if !f.words.contains(&ident) {
                        continue;
                    }
                    let events = self.events(f, &ident).map_err(|why| {
                        format!(
                            "{}::{} refers to it through {why}",
                            f.module, f.function.name
                        )
                    })?;
                    let mut observes = false;
                    for event in events {
                        match event {
                            Event::Pairs(pairs) => observes |= !pairs.is_disjoint(&ours),
                            Event::Receiver
                                if ident == name
                                    && !methods.is_empty()
                                    && (receiver || trait_method) =>
                            {
                                return Err(format!(
                                    "{}::{} refers to it through a method call on an unknown receiver",
                                    f.module, f.function.name
                                ));
                            }
                            Event::Associated(qualifier)
                                if ident == name && !methods.is_empty() =>
                            {
                                if trait_method {
                                    return Err(format!(
                                        "{}::{} refers to it through a `{qualifier}::` call of a trait method",
                                        f.module, f.function.name
                                    ));
                                }
                                let mut names = BTreeSet::from([qualifier.clone()]);
                                names.extend(
                                    self.aliases.get(&qualifier).cloned().unwrap_or_default(),
                                );
                                observes |= !names.is_disjoint(&types);
                            }
                            _ => (),
                        }
                    }
                    if observes {
                        found
                            .functions
                            .insert((f.module.clone(), f.function.name.clone()));
                    }
                }
            }
            Ok(())
        })();
        if let Err(why) = scan {
            found.unresolved = Some(why);
        }
        found
    }

    fn events(&self, f: &IndexedFunction, name: &str) -> Result<Vec<Event>, String> {
        let code = source::code_text(&f.function.body)?;
        let code = GENERIC_ARGS.sub(&code, "");
        let word = Re::new(&format!(r"\b{name}\b"));
        let bound = local_binding(&code, name);
        let mut events = Vec::new();
        for hit in word.find_all(&code) {
            let before = code[..crate::re::start(&hit)].trim_end();
            let after = code[crate::re::end(&hit)..].trim_start();
            if Re::new(r"\bfn$").is_match(before)
                || after.starts_with('!') && !after.starts_with("!=")
                || Re::new(r"\blet(?:\s+mut)?$").is_match(before)
            {
                continue;
            }
            if before.ends_with('.') && !before.ends_with("..") {
                events.push(Event::Receiver);
                continue;
            }
            if before.ends_with("::") {
                let Some(c) = QUALIFIER.search(before) else {
                    return Err("an unreadable path".into());
                };
                let parts: Vec<String> = group(&c, 1)
                    .split("::")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                let last = parts.last().unwrap();
                if last.starts_with(char::is_uppercase) {
                    let qualifier = if last == "Self" {
                        f.owner
                            .as_ref()
                            .and_then(|o| o.name.clone())
                            .ok_or("Self:: outside an impl block")?
                    } else {
                        last.clone()
                    };
                    events.push(Event::Associated(qualifier));
                    continue;
                }
                if after.starts_with("::") {
                    let mut next = parts.clone();
                    next.push(name.into());
                    if self.module_path(&next, &f.effective)?.is_some() {
                        continue;
                    }
                }
                if let Some(module) = self.module_path(&parts, &f.effective)? {
                    let pairs = self.item_pairs(&module, name)?;
                    if pairs.is_empty() {
                        return Err(format!(
                            "a path `{}` that names no defining module",
                            parts.join("::")
                        ));
                    }
                    events.push(Event::Pairs(pairs));
                } else if self.local_path(&parts, &f.effective, 0) {
                    return Err(format!(
                        "a path `{}` that names no defining module",
                        parts.join("::")
                    ));
                }
                continue;
            }
            if bound.is_some_and(|at| crate::re::start(&hit) >= at)
                || closure_parameter(before, after)
            {
                continue;
            }
            if !after.is_empty()
                && !after.starts_with(['(', ',', ')', ';', '}', ']'])
                && !after.starts_with("::")
            {
                continue;
            }
            if after.starts_with("::") && self.module_path(&[name.into()], &f.effective)?.is_some()
            {
                continue;
            }
            let pairs = self.item_pairs(&f.effective, name)?;
            if !pairs.is_empty() {
                events.push(Event::Pairs(pairs));
            }
        }
        Ok(events)
    }
}

#[derive(Debug)]
enum Event {
    Pairs(BTreeSet<Item>),
    Receiver,
    Associated(String),
}
fn join(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.into()
    } else {
        format!("{module}::{name}")
    }
}
fn local_binding(code: &str, name: &str) -> Option<usize> {
    let header = code.split('{').next().unwrap_or("");
    if Re::new(&format!(r"[(,]\s*(?:mut\s+)?{name}\s*:(?!:)")).is_match(header) {
        return Some(0);
    }
    if code
        .split_once('\n')
        .is_some_and(|(_, rest)| Re::new(&format!(r"\bfn\s+{name}\b")).is_match(rest))
    {
        return Some(0);
    }
    let mut first = None;
    for pattern in [
        format!(r"\blet\b[^=;]*?\b{name}\b"),
        format!(r"\bfor\b[^{{}};]*?\b{name}\b[^{{}};]*?\bin\b"),
    ] {
        if let Some(c) = Re::new(&pattern).search(code) {
            let start = crate::re::start(&c);
            first = Some(first.map_or(start, |p: usize| p.min(start)));
        }
    }
    first
}
fn closure_parameter(before: &str, after: &str) -> bool {
    if !after.starts_with(['|', ',', ':']) || after.starts_with("||") {
        return false;
    }
    let Some(bar) = before.rfind('|') else {
        return false;
    };
    (bar == 0 || !before[..bar].ends_with('|')) && !before[bar + 1..].contains(['{', '}', ';', '='])
}
