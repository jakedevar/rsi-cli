//! Hunk-to-seed proofs and concrete private-field initializer classification.
//! Failure to prove any changed executable line keeps the broad fallback.
use super::{
    filters,
    git::{Hunk, Texts},
    identity::{ImplKind, ItemReferences, SourceScopes},
    relocation::{self, Relocation},
    selection,
    source::{self, LexemeKind, SourceTree},
    trace::Seeds,
};
use crate::re::{Re, group};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

static WORD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b[A-Za-z_][A-Za-z_0-9]*\b"));
static STRUCT: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:struct|union)\s+(\w+)\b[^;{]*\{\s*$")
});
static ENUM: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?enum\s+(\w+)\s*\{$"));
static FIELD: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^(?:pub(?:\([^)]*\))?\s+)?([A-Za-z_]\w*)\s*:\s*[^{};]+,$"));
static ITEM: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"^(?:pub(?:\([^)]*\))?\s+)?(?:impl|struct|enum|union|trait|fn|mod)\b")
});
static MECHANICAL: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"^(?:[A-Za-z_]\w*::)+[A-Za-z_]\w*\((?:(?:[A-Za-z_]\w*::)+[A-Za-z_]\w*\(\)|None|true|false|\d+)?\)$",
    )
});
static CALL: LazyLock<Re> = LazyLock::new(|| Re::new(r"((?:[A-Za-z_]\w*::)+)([A-Za-z_]\w*)\("));
static NOISE: LazyLock<BTreeSet<&'static str>> = LazyLock::new(|| {
    "self Self super crate pub let mut impl struct enum match return async await move true false None Some Ok Err where trait type const static mod use while loop else for ref dyn Box Vec Option Result String str bool u8 u16 u32 u64 usize i32 i64 f64 serde json into from with this that".split_whitespace().collect()
});
static WRAPPERS: LazyLock<BTreeSet<&'static str>> = LazyLock::new(|| {
    "Cell RefCell Mutex RwLock Arc Rc Box Option Vec HashMap HashSet BTreeMap BTreeSet VecDeque"
        .split_whitespace()
        .collect()
});

fn comments_masked(text: &str) -> Result<String, String> {
    let mut bytes = text.as_bytes().to_vec();
    for lex in source::rust_lexemes(text)? {
        if lex.kind == LexemeKind::Comment {
            for b in &mut bytes[lex.start..lex.end] {
                if *b != b'\n' && *b != b'\r' {
                    *b = b' ';
                }
            }
        }
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// One owner per line of rustfmt structs/enums/impls. Inline module scopes and
/// generic inherent/trait impl headers share the item resolver's interpretation.
pub fn type_owners(text: &str, types: &BTreeSet<String>) -> Result<Vec<Option<String>>, String> {
    let code = source::code_text(text)?;
    let scopes = SourceScopes::new("", text)?;
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut result = Vec::new();
    for (index, row) in code.lines().enumerate() {
        if let Some(c) = STRUCT.search(row).or_else(|| ENUM.search(row)) {
            if types.contains(group(&c, 2)) {
                stack.push((group(&c, 1).into(), group(&c, 2).into()));
            }
        }
        let owner = scopes
            .lines
            .get(index)
            .and_then(|s| s.owner.as_ref())
            .filter(|o| matches!(o.kind, ImplKind::Inherent | ImplKind::Trait))
            .and_then(|o| o.name.as_ref())
            .filter(|n| types.contains(*n))
            .cloned()
            .or_else(|| stack.last().map(|(_, n)| n.clone()));
        result.push(owner);
        if stack.last().is_some_and(|(i, _)| row == format!("{i}}}")) {
            stack.pop();
        }
    }
    Ok(result)
}

/// Only unit enums are mechanically traceable by the concrete type. Payloads,
/// discriminants and variant attributes need a richer proof and stay broad.
pub fn enum_item_lines(
    text: &str,
    types: &BTreeSet<String>,
) -> Result<BTreeMap<usize, String>, String> {
    let code = source::code_text(text)?;
    let rows: Vec<_> = code.lines().collect();
    let mut items = BTreeMap::new();
    for (start, row) in rows.iter().enumerate() {
        let Some(c) = ENUM.search(row) else {
            continue;
        };
        let owner = group(&c, 2);
        if !types.contains(owner) {
            continue;
        }
        let indent = group(&c, 1);
        let Some(end) = (start + 1..rows.len()).find(|i| rows[*i] == format!("{indent}}}")) else {
            continue;
        };
        let variant = Re::new(&format!(
            r"^{}    [A-Za-z_]\w*,$",
            crate::re::escape(indent)
        ));
        if rows[start + 1..end]
            .iter()
            .any(|r| !r.trim().is_empty() && !variant.is_match(r))
        {
            continue;
        }
        for number in start + 1..=end + 1 {
            items.insert(number, owner.into());
        }
        let mut attr = start;
        while attr > 0 && rows[attr - 1].trim_start().starts_with("#[") {
            items.insert(attr, owner.into());
            attr -= 1;
        }
    }
    Ok(items)
}

pub fn struct_field_owner<'a>(rows: &[&'a str], index: usize) -> Option<String> {
    let row = rows.get(index)?;
    let indent = row.len() - row.trim_start().len();
    for earlier in rows[..index].iter().rev() {
        if earlier.trim().is_empty() {
            continue;
        }
        let depth = earlier.len() - earlier.trim_start().len();
        if depth >= indent {
            continue;
        }
        return STRUCT
            .search(earlier)
            .filter(|_| depth + 4 == indent)
            .map(|c| group(&c, 2).into());
    }
    None
}

/// A constructor only stores a mechanical initial value when every call is
/// `new`/`default`, with at most one nested such call or a literal argument.
pub fn mechanical_types(value: &str) -> Option<BTreeSet<String>> {
    if !MECHANICAL.is_match(value) {
        return None;
    }
    let mut types = BTreeSet::new();
    for c in CALL.find_all(value) {
        if !matches!(group(&c, 2), "new" | "default") {
            return None;
        }
        for name in group(&c, 1)
            .split("::")
            .filter(|s| s.starts_with(char::is_uppercase) && !WRAPPERS.contains(*s))
        {
            types.insert(name.into());
        }
    }
    Some(types)
}

/// Every field-shaped row must be the one private declaration or a mechanical
/// initializer. A second type, a public field, a macro, parameter or pattern is
/// insufficient proof, even if an unrelated test happens to mention the name.
pub fn unique_private_field_owner(
    texts: &Texts,
    field: &str,
    types: &BTreeSet<String>,
) -> Result<Option<String>, String> {
    let pattern = Re::new(&format!(r"\b{}\s*:(?!:)", crate::re::escape(field)));
    let initializer = Re::new(&format!(r"^{}\s*:\s*(.+),$", crate::re::escape(field)));
    let declaration = Re::new(&format!(r"^{}\s*:\s*[^{{}};]+,$", crate::re::escape(field)));
    let mut declared = Vec::new();
    for text in texts.values().filter(|t| t.contains(field)) {
        let code = source::code_text(text)?;
        if !pattern.is_match(&code) {
            continue;
        }
        if Re::new(r"\bmacro_rules\s*!").is_match(&code) {
            return Ok(None);
        }
        let rows: Vec<_> = code.lines().collect();
        for (index, row) in rows.iter().enumerate().filter(|(_, r)| pattern.is_match(r)) {
            let row = row.trim();
            if initializer
                .search(row)
                .is_some_and(|c| mechanical_types(group(&c, 1)).is_some())
            {
                continue;
            }
            let Some(owner) = struct_field_owner(&rows, index) else {
                return Ok(None);
            };
            if !declaration.is_match(row) {
                return Ok(None);
            }
            declared.push(owner);
        }
    }
    Ok(if declared.len() == 1 && types.contains(&declared[0]) {
        declared.pop()
    } else {
        None
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Initializer {
    pub field: String,
    pub types: BTreeSet<String>,
}
/// Prove that this row initializes the actual owning struct literal. `Self`
/// refers to the surrounding impl's type, never an unrelated similarly shaped
/// literal. Destructuring patterns and arbitrary expressions remain readers.
pub fn initializer_value(
    rows: &[&str],
    index: usize,
    fields: &BTreeMap<String, String>,
    enclosing: Option<&str>,
) -> Option<Initializer> {
    let row = rows.get(index)?.trim();
    let c = Re::new(r"^(\w+)\s*:\s*(.+),$").search(row)?;
    let field = group(&c, 1);
    let owner = fields.get(field)?;
    let types = mechanical_types(group(&c, 2))?;
    let literal = Re::new(&format!(
        r"(?:^|[\s=(])(Self|{})\s*\{{$",
        crate::re::escape(owner)
    ));
    let mut depth = 0isize;
    for row in rows[..index].iter().rev() {
        depth += row.matches('}').count() as isize - row.matches('{').count() as isize;
        if depth >= 0 {
            continue;
        }
        let row = row.trim();
        let c = literal.search(row)?;
        let pattern = Re::new(&format!(
            r"^(?:let|if\s+let|while\s+let)\s+(?:mut\s+)?(?:Self|{})\s*\{{$",
            crate::re::escape(owner)
        ));
        if ITEM.is_match(row)
            || pattern.is_match(row)
            || group(&c, 1) == "Self" && enclosing != Some(owner.as_str())
        {
            return None;
        }
        return Some(Initializer {
            field: field.into(),
            types,
        });
    }
    None
}

/// Only declared cfg/test function bodies and inline test module spans get the
/// large-helper exception. A production function named `test_something` does not.
pub fn test_module_regions(text: &str) -> Result<Vec<(usize, usize)>, String> {
    let code = source::code_text(text)?;
    let rows: Vec<_> = code.lines().collect();
    let opener = Re::new(r"^(\s*)mod\s+(\w+)\s*\{$");
    let mut regions = Vec::new();
    for (start, row) in rows.iter().enumerate() {
        if let Some(c) = opener
            .search(row)
            .filter(|c| source::is_test_module(group(c, 2)))
        {
            if let Some(end) =
                (start + 1..rows.len()).find(|i| rows[*i] == format!("{}}}", group(&c, 1)))
            {
                regions.push((start + 1, end + 1));
            }
        }
    }
    Ok(regions)
}

#[derive(Clone, Debug, Default)]
pub struct Proof {
    pub seeds: Seeds,
    /// Fields whose mechanical initializers must be removed from the observer
    /// words before walking. This proof alone does not implement that omission.
    pub initializers: BTreeSet<String>,
    /// Candidate-tree ownership proofs used to classify and omit initializers.
    pub private_fields: BTreeMap<String, String>,
    /// Concrete types contained by a proven closed private relocation seam.
    pub moved: BTreeSet<String>,
    /// Leaf edits may be observed through large public/daemon wrappers.
    pub leaf: bool,
}
#[derive(Clone, Debug)]
pub struct SeedContext<'a> {
    pub frequency: &'a BTreeMap<String, usize>,
    pub parent: &'a str,
    pub types: &'a BTreeSet<String>,
    pub private_fields: &'a BTreeMap<String, String>,
    pub host: &'a str,
}

/// Classify every changed line. Both old and new sides must pass separately;
/// deletions are never dismissed just because the candidate's body looks safe.
/// Private relocation uses the specialized old/new assembler below; RPC arms
/// still need their own proof. Unfamiliar lines refuse guessed seeds.
pub fn trace_seeds(
    text: &str,
    lines: &BTreeSet<usize>,
    context: &SeedContext<'_>,
) -> Result<Proof, String> {
    trace_relocated_seeds(text, lines, context, &Relocation::default(), false)
}

fn trace_relocated_seeds(
    text: &str,
    lines: &BTreeSet<usize>,
    context: &SeedContext<'_>,
    relocation: &Relocation,
    candidate: bool,
) -> Result<Proof, String> {
    let masked = comments_masked(text)?;
    let rows: Vec<_> = masked.lines().collect();
    let code = source::code_text(text)?;
    let code_rows: Vec<_> = code.lines().collect();
    let owners = type_owners(text, context.types)?;
    let enums = enum_item_lines(text, context.types)?;
    let enum_types: BTreeSet<_> = enums.values().cloned().collect();
    let functions: Vec<_> = source::rust_functions(text, context.host)
        .into_iter()
        .filter(|f| {
            code_rows
                .get(f.start)
                .is_some_and(|row| Re::new(&format!(r"\bfn\s+{}\b", f.name)).is_match(row))
        })
        .collect();
    let mut test_regions = test_module_regions(text)?;
    test_regions.extend(
        functions
            .iter()
            .filter(|f| source::is_test(&f.attrs) || f.attrs.contains("#[cfg(test)]"))
            .map(|f| (f.start + 1, f.end)),
    );
    let frequency = |name: &str| context.frequency.get(name).copied().unwrap_or(0);
    let mut proof = Proof::default();
    proof.seeds.types = context.types.clone();
    proof.private_fields = context.private_fields.clone();
    proof.moved = relocation.moved.keys().cloned().collect();
    let direct_use = Re::new(r"^use\s+([\w:]+);$");
    let grouped_use = Re::new(r"^use\s+([\w:]+)::\{([\w, ]+)\};$");
    let mut parent_imports = BTreeSet::new();
    for row in &code_rows {
        if let Some(c) = direct_use.search(row) {
            parent_imports.insert(group(&c, 1).to_owned());
        } else if let Some(c) = grouped_use.search(row) {
            for name in group(&c, 2)
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                parent_imports.insert(format!("{}::{name}", group(&c, 1)));
            }
        }
    }
    for &number in lines {
        let Some(row) = number
            .checked_sub(1)
            .and_then(|i| rows.get(i))
            .map(|s| s.trim())
        else {
            return Err(format!("changed line {number} is outside the snapshot"));
        };
        if row.is_empty() {
            continue;
        }
        let inside = functions
            .iter()
            .filter(|f| f.attr_start < number && number <= f.end)
            .min_by_key(|f| f.end - f.attr_start);
        let owner = owners.get(number - 1).and_then(|o| o.as_deref());
        if let Some(f) = inside {
            if let Some(found) = initializer_value(&rows, number - 1, context.private_fields, owner)
            {
                if found
                    .types
                    .iter()
                    .any(|kind| frequency(kind) > filters::RARE_TOKEN_FUNCTIONS)
                {
                    return Err("mechanical initializer names a common type".into());
                }
                proof.seeds.tokens.insert(found.field.clone());
                proof.seeds.tokens.extend(found.types);
                proof.initializers.insert(found.field);
                continue;
            }
            if owner.is_some_and(|o| enum_types.contains(o)) {
                proof.seeds.tokens.insert(owner.unwrap().into());
                continue;
            }
            let test_helper = source::is_test_module(context.parent)
                || test_regions
                    .iter()
                    .any(|(a, b)| *a <= number && number <= *b);
            if f.end - f.attr_start > filters::LARGE_FUNCTION_LINES && !test_helper {
                let rare: BTreeSet<_> = WORD
                    .find_all(row)
                    .iter()
                    .map(|c| group(c, 0))
                    .filter(|word| {
                        word.len() >= 4
                            && !NOISE.contains(*word)
                            && frequency(word) <= filters::RARE_TOKEN_FUNCTIONS
                    })
                    .map(str::to_owned)
                    .collect();
                if !rare.is_empty() {
                    proof.seeds.tokens.extend(rare);
                    continue;
                }
            }
            proof.seeds.changed.insert(f.name.clone());
            continue;
        }
        // Declaration proofs read code with literals masked. A fixture string
        // that happens to spell a struct or reexport is not a declaration.
        let declaration = code_rows.get(number - 1).map_or("", |r| r.trim());
        let private_region = candidate
            && relocation
                .regions
                .iter()
                .any(|(a, b)| *a <= number && number <= *b);
        if private_region {
            if Re::new(r"^mod\s+\w+\s*\{$").is_match(declaration) || declaration == "}" {
                continue;
            }
            if direct_use
                .search(declaration)
                .is_some_and(|c| parent_imports.contains(group(&c, 1)))
            {
                continue;
            }
        }
        let moved_declaration = STRUCT
            .search(declaration)
            .map(|c| group(&c, 2).to_owned())
            .or_else(|| {
                if !private_region || !declaration.starts_with("#[") {
                    return None;
                }
                code_rows[number..]
                    .iter()
                    .map(|r| r.trim())
                    .find(|r| !r.is_empty() && !r.starts_with("#["))
                    .and_then(|r| STRUCT.search(r))
                    .map(|c| group(&c, 2).to_owned())
            });
        if let Some(kind) = moved_declaration.filter(|k| relocation.moved.contains_key(k)) {
            if frequency(&kind) > filters::RARE_TOKEN_FUNCTIONS {
                return Err("relocated type is not a rare trace seed".into());
            }
            proof.seeds.tokens.insert(kind);
            continue;
        }
        if let Some(owner) = enums.get(&number) {
            if frequency(owner) > filters::NARROW_FUNCTION_CAP {
                return Err("unit enum exceeds the function bound".into());
            }
            proof.seeds.tokens.insert(owner.clone());
            continue;
        }
        if owner.is_some() && declaration == "}" {
            continue;
        }
        if let Some(c) =
            Re::new(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;").search(declaration)
        {
            proof.seeds.mounted.insert(if context.parent.is_empty() {
                group(&c, 1).into()
            } else {
                format!("{}::{}", context.parent, group(&c, 1))
            });
            continue;
        }
        let exported =
            Re::new(r"^pub(?:\([^)]*\))?\s+use\s+(?:[A-Za-z_]\w*::)+([A-Za-z_]\w*)\s*;$")
                .search(declaration);
        let field = FIELD
            .search(declaration)
            .filter(|_| struct_field_owner(&rows, number - 1).is_some());
        if let Some(c) = exported.as_ref() {
            let kind = group(c, 1);
            if relocation.moved.get(kind).is_some_and(|module| {
                Re::new(&format!(
                    r"^pub(?:\([^)]*\))?\s+use\s+{}::{}\s*;$",
                    crate::re::escape(module),
                    crate::re::escape(kind)
                ))
                .is_match(declaration)
            }) {
                continue;
            }
        }
        let identifier = exported
            .as_ref()
            .map(|c| group(c, 1))
            .or_else(|| field.as_ref().map(|c| group(c, 1)));
        let typed_field = field.is_some()
            && owner.is_some_and(|o| {
                context.types.contains(o)
                    && frequency(o) > 0
                    && frequency(o) <= filters::NARROW_FUNCTION_CAP
            })
            && identifier.is_some_and(|i| frequency(i) > filters::RARE_TOKEN_FUNCTIONS);
        let moved_field = field.is_some()
            && owner.is_some_and(|o| {
                relocation.moved.contains_key(o) && frequency(o) <= filters::RARE_TOKEN_FUNCTIONS
            });
        let identifier = if typed_field || moved_field {
            owner
        } else {
            identifier
        };
        let unresolved = identifier.is_none_or(|i| {
            NOISE.contains(i) || !typed_field && frequency(i) > filters::RARE_TOKEN_FUNCTIONS
        });
        if unresolved {
            if let Some(c) = field.as_ref() {
                let field = group(c, 1);
                if !declaration.starts_with("pub")
                    && owner
                        .is_some_and(|o| context.private_fields.get(field).is_some_and(|p| p == o))
                    && struct_field_owner(&rows, number - 1).as_deref() == owner
                {
                    proof.seeds.tokens.insert(field.into());
                    continue;
                }
            }
            return Err(format!(
                "changed declaration or common identifier cannot be resolved at line {number}"
            ));
        }
        proof.seeds.tokens.insert(identifier.unwrap().into());
    }
    Ok(proof)
}

/// Assemble old/new proofs using one already-built candidate index. No source
/// or Cargo helper executes. `trace::trace_proof` applies the resulting
/// initializer and private-relocation obligations; RPC/leaf work remains.
pub fn changed_seed_proof(
    sources: &SourceTree,
    old: &SourceTree,
    index: &ItemReferences,
    path: &str,
    hunks: &[Hunk],
) -> Result<Proof, String> {
    let parent = sources
        .trace_module(path)
        .ok_or("changed source has no trace module")?;
    let types: BTreeSet<_> = index
        .type_counts
        .iter()
        .filter(|(_, n)| **n == 1)
        .map(|(name, _)| name.clone())
        .collect();
    let relocation = relocation::private_move(
        old.texts.get(path).map_or("", String::as_str),
        sources.texts.get(path).map_or("", String::as_str),
    )?;
    if relocation.moved.keys().any(|kind| !types.contains(kind)) {
        return Err("relocated type has no unique concrete declaration".into());
    }
    let (old_lines, new_lines) = if relocation.moved.is_empty() {
        (
            selection::changed_lines(hunks, true),
            selection::changed_lines(hunks, false),
        )
    } else {
        relocation::relocation_lines(
            old.texts.get(path).map_or("", String::as_str),
            sources.texts.get(path).map_or("", String::as_str),
            &relocation,
            2_000_000,
        )?
    };
    // Only field-shaped changed rows can introduce the initializer exception.
    // Check each candidate once against the entire pinned candidate tree,
    // including sources outside the changed module and quoted-fixture masking.
    let mut candidates = BTreeSet::new();
    for (tree, lines) in [(old, &old_lines), (sources, &new_lines)] {
        if lines.is_empty() {
            continue;
        }
        let text = tree
            .texts
            .get(path)
            .ok_or("changed side has no source snapshot")?;
        let code = source::code_text(text)?;
        let rows: Vec<_> = code.lines().collect();
        for &number in lines {
            if let Some(c) = number
                .checked_sub(1)
                .and_then(|i| rows.get(i))
                .and_then(|r| Re::new(r"^(\w+)\s*:(?!:)").search(r.trim()))
            {
                candidates.insert(group(&c, 1).to_owned());
            }
        }
    }
    let mut fields = BTreeMap::new();
    for field in candidates {
        if index.frequency.get(&field).copied().unwrap_or(0) <= filters::NARROW_FUNCTION_CAP {
            if let Some(owner) = unique_private_field_owner(&sources.texts, &field, &types)? {
                fields.insert(field, owner);
            }
        }
    }
    let context = SeedContext {
        frequency: &index.frequency,
        parent: &parent,
        types: &types,
        private_fields: &fields,
        host: &sources.host,
    };
    let mut result = Proof::default();
    result.private_fields = fields.clone();
    result.leaf = source::leaf_package(path).is_some();
    for (tree, lines, candidate) in [(old, &old_lines, false), (sources, &new_lines, true)] {
        if lines.is_empty() {
            continue;
        }
        let text = tree
            .texts
            .get(path)
            .ok_or("changed side has no source snapshot")?;
        let proof = trace_relocated_seeds(text, lines, &context, &relocation, candidate)?;
        result.seeds.changed.extend(proof.seeds.changed);
        result.seeds.tokens.extend(proof.seeds.tokens);
        result.seeds.mounted.extend(proof.seeds.mounted);
        result.seeds.types.extend(proof.seeds.types);
        result.initializers.extend(proof.initializers);
        result.moved.extend(proof.moved);
    }
    Ok(result)
}

/// An observer keeps a field word if *any* mention is a read, write, pattern,
/// comment or literal. Only functions whose every mention initializes the
/// proven owning struct may omit it. Constructor type words are omitted only
/// when every mention of that type occurs on those same initializer rows.
/// The immutable item index and its lexical caller cache remain reusable for
/// another changed file: this view never mutates them or their frequencies.
pub fn observer_words(
    index: &ItemReferences,
    proof: &Proof,
) -> Result<Vec<BTreeSet<String>>, String> {
    if proof
        .initializers
        .iter()
        .any(|f| !proof.private_fields.contains_key(f))
    {
        return Err("initializer omission has no private-field ownership proof".into());
    }
    let mut result = Vec::with_capacity(index.functions.len());
    for f in &index.functions {
        let mut words = f.words.clone();
        if proof.initializers.is_disjoint(&words) {
            result.push(words);
            continue;
        }
        let rows: Vec<_> = f.function.body.lines().collect();
        // Code masking is used for ownership, but mentions deliberately retain
        // comments/literals: source-scanning tests are observers too.
        let code = source::code_text(&f.function.body)?;
        let code_rows: Vec<_> = code.lines().collect();
        let line_words: Vec<BTreeSet<_>> = rows
            .iter()
            .map(|r| {
                WORD.find_all(r)
                    .iter()
                    .map(|c| group(c, 0).to_owned())
                    .collect()
            })
            .collect();
        let enclosing = f.owner.as_ref().and_then(|o| o.name.as_deref());
        let mut initializing = BTreeMap::<String, BTreeSet<usize>>::new();
        for field in proof.initializers.intersection(&f.words) {
            let mentions: Vec<_> = line_words
                .iter()
                .enumerate()
                .filter(|(_, w)| w.contains(field))
                .map(|(i, _)| i)
                .collect();
            let found: Option<Vec<_>> = mentions
                .iter()
                .map(|&i| {
                    // A comment/string spelling a constructor is still a reader.
                    // Require the whole original row to be code (apart from space).
                    (rows[i].trim() == code_rows[i].trim())
                        .then(|| initializer_value(&code_rows, i, &proof.private_fields, enclosing))
                        .flatten()
                        .filter(|v| &v.field == field)
                })
                .collect();
            if let Some(found) = found.filter(|v| !v.is_empty()) {
                words.remove(field);
                for (i, value) in mentions.into_iter().zip(found) {
                    for kind in value.types {
                        initializing.entry(kind).or_default().insert(i);
                    }
                }
            }
        }
        for (kind, initializers) in initializing {
            if line_words
                .iter()
                .enumerate()
                .all(|(i, w)| !w.contains(&kind) || initializers.contains(&i))
            {
                words.remove(&kind);
            }
        }
        result.push(words);
    }
    Ok(result)
}
