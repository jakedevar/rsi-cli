//! Synthetic path-list proof (#199): a path in a closed list of string literals
//! that only in-memory code consumes is a fabricated fixture input, not a read.
//! The proof fails closed: an unknown callee, callback, macro, receiver, foreign
//! type or lexing ambiguity keeps the reader.
use super::source::{code_text, rust_functions_ungated};
use crate::re::{Re, end, group, start};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::LazyLock;

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

static OBSERVER_WORDS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    set(&[
        "fs",
        "File",
        "OpenOptions",
        "read_to_string",
        "read_to_end",
        "read_dir",
        "read_link",
        "metadata",
        "symlink_metadata",
        "canonicalize",
        "exists",
        "try_exists",
        "is_file",
        "is_dir",
        "env",
        "current_dir",
        "Command",
        "WalkDir",
        "walkdir",
        "tempdir",
        "TempDir",
        "manifest_dir",
        "CARGO_MANIFEST_DIR",
        "unsafe",
        "asm",
        "extern",
    ])
});
static SAFE_MACROS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    set(&[
        "assert",
        "assert_eq",
        "assert_ne",
        "debug_assert",
        "debug_assert_eq",
        "debug_assert_ne",
        "vec",
        "format",
        "matches",
        "panic",
        "write",
        "writeln",
        "unreachable",
        "todo",
        "unimplemented",
        "println",
        "eprintln",
        "dbg",
        "concat",
        "stringify",
    ])
});
static STD_NAMES: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    set(&[
        "new",
        "from",
        "into",
        "len",
        "iter",
        "iter_mut",
        "into_iter",
        "map",
        "filter",
        "collect",
        "contains",
        "insert",
        "push",
        "extend",
        "clone",
        "cloned",
        "copied",
        "unwrap",
        "expect",
        "is_empty",
        "to_owned",
        "to_string",
        "as_str",
        "as_ref",
        "join",
        "get",
        "any",
        "all",
        "cmp",
        "eq",
        "starts_with",
        "ends_with",
        "default",
        "sort",
        "dedup",
        "retain",
        "remove",
        "first",
        "last",
        "keys",
        "values",
        "chain",
        "zip",
        "enumerate",
        "rev",
        "take",
        "skip",
        "find",
        "position",
        "unwrap_or",
        "unwrap_or_default",
        "ok",
        "err",
        "is_some",
        "is_none",
        "is_ok",
        "is_err",
        "and_then",
        "ok_or",
        "with_capacity",
        "next",
        "peek",
        "pop",
        "max",
        "min",
        "count",
        "lines",
        "split",
        "split_once",
        "rsplit",
        "rsplit_once",
        "strip_prefix",
        "strip_suffix",
        "trim",
        "trim_start",
        "trim_end",
        "trim_matches",
        "trim_start_matches",
        "trim_end_matches",
        "contains_key",
        "entry",
        "or_default",
        "or_insert_with",
        "or_insert",
        "try_from",
        "from_str",
        "as_bytes",
        "chars",
        "is_some_and",
        "is_none_or",
        "map_or",
        "map_or_else",
        "flat_map",
        "filter_map",
        "flatten",
        "find_map",
        "then",
        "then_some",
        "saturating_sub",
        "saturating_add",
        "to_vec",
        "as_slice",
        "peekable",
        "sort_by_key",
        "get_or_insert",
        "get_or_insert_with",
        "or_else",
        "as_mut",
        "extend_from_slice",
        "is_ascii_digit",
        "is_ascii_alphanumeric",
        "is_ascii_whitespace",
        "is_alphabetic",
        "is_alphanumeric",
        "is_whitespace",
        "windows",
        "or",
        "to_ascii_lowercase",
        "to_lowercase",
    ])
});
static STD_TYPES: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    set(&[
        "Vec", "String", "Option", "Result", "HashMap", "HashSet", "BTreeMap", "BTreeSet",
        "VecDeque", "Box", "Rc", "Arc", "Cow", "Ordering", "Range", "Self", "Path", "PathBuf",
        "Some", "None", "Ok", "Err", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16",
        "i32", "i64", "i128", "isize", "f32", "f64", "bool", "char", "str",
    ])
});
static STD_MODULES: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    set(&[
        "mem",
        "cmp",
        "iter",
        "str",
        "char",
        "slice",
        "array",
        "collections",
    ])
});
static ROOT_MODULES: LazyLock<BTreeSet<String>> =
    LazyLock::new(|| set(&["self", "super", "crate"]));
const PURE_PATHS: &[(&str, &str)] = &[
    ("toml", "from_str"),
    ("serde_json", "from_str"),
    ("serde_json", "from_slice"),
    ("serde_json", "to_string"),
    ("serde_json", "to_value"),
    ("serde_json", "from_value"),
];
static KEYWORDS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    set(&[
        "if", "while", "match", "for", "in", "return", "loop", "else", "fn", "let", "mut", "move",
        "as", "where", "impl", "pub", "dyn", "ref", "async", "await", "break", "continue", "use",
        "mod", "const", "static", "struct", "enum", "trait", "type", "crate", "self", "super",
        "Self", "unsafe",
    ])
});

static WORD: LazyLock<Re> = LazyLock::new(|| Re::new(r"[A-Za-z_]\w*"));
static CALL: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"(?P<dot>\.\s*)?\b(?P<name>[A-Za-z_]\w*)(?:(?P<bang>!)\s*[(\[{]|\s*(?:::<[^()]*>)?\s*\()",
    )
});
static FN_NAMES: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bfn\s+([A-Za-z_]\w*)"));
static TYPE_NAMES: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\b(?:struct|enum|union|trait|type)\s+([A-Z]\w*)"));
static MOD_NAMES: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bmod\s+([A-Za-z_]\w*)"));
static UNSAFE_SIGNATURE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\b(?:impl|dyn|fn|Fn|FnMut|FnOnce|where)\b"));
static CALLBACK_TYPE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\b(?:dyn|impl)\s+\w|\bfn\s*\(|\bFn(?:Mut|Once)?\s*[(<]"));

pub fn words(text: &str) -> Vec<String> {
    WORD.find_all(text)
        .iter()
        .map(|c| group(c, 0).to_owned())
        .collect()
}
fn word_set(text: &str) -> BTreeSet<String> {
    words(text).into_iter().collect()
}
fn capture_all(re: &Re, text: &str) -> BTreeSet<String> {
    re.findall1(text).into_iter().collect()
}
fn closure_binding(name: &str) -> Re {
    Re::new(&format!(
        r"\blet\s+(?:mut\s+)?{}\s*(?::[^=;]*)?=\s*(?:move\s*)?\|",
        crate::re::escape(name)
    ))
}
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}
fn first_upper(word: &str) -> bool {
    word.chars().next().is_some_and(char::is_uppercase)
}
/// Python `str.isupper()`: at least one cased character and none lowercase.
fn is_upper(word: &str) -> bool {
    word.chars().any(char::is_uppercase) && !word.chars().any(char::is_lowercase)
}

/// Offset of the nearest unmatched opening bracket before `position`.
pub fn enclosing(code: &str, position: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 0usize;
    for index in (0..position.min(bytes.len())).rev() {
        match bytes[index] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' => {
                if depth == 0 {
                    return Some(index);
                }
                depth -= 1;
            }
            _ => (),
        }
    }
    None
}

/// Whether the closed string list around `[start, end)` is consumed in memory,
/// directly: its first real consumer must be a `for .. in` head, a safe macro
/// or a call of a project function.
pub fn list_consumer_ok(
    code: &str,
    start_at: usize,
    end_at: usize,
    resolves: &dyn Fn(&str) -> bool,
) -> bool {
    let bytes = code.as_bytes();
    let mut left = start_at;
    while left > 0 && (is_space(bytes[left - 1]) || bytes[left - 1] == b',') {
        left -= 1;
    }
    let mut right = end_at;
    while right < bytes.len() && (is_space(bytes[right]) || bytes[right] == b',') {
        right += 1;
    }
    if left == 0 || right >= bytes.len() || bytes[left - 1] != b'[' || bytes[right] != b']' {
        return false;
    }
    let mut position = left - 1;
    let after = right + 1;
    let mut rest = code[after..].trim_start();
    if rest.starts_with('[') {
        if let Some(reslice) = Re::new(r"^\[\s*\.\.\s*\]").search(rest) {
            rest = rest[end(&reslice)..].trim_start();
        }
    }
    if rest.starts_with(['.', '?']) {
        return false;
    }
    static REF: LazyLock<Re> = LazyLock::new(|| Re::new(r"&\s*(?:mut\s+)?$"));
    static IN: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bin$"));
    static VEC: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bvec\s*!$"));
    static MACRO: LazyLock<Re> = LazyLock::new(|| Re::new(r"([A-Za-z_]\w*)\s*!$"));
    static CALLEE: LazyLock<Re> =
        LazyLock::new(|| Re::new(r"(\.\s*)?\b([A-Za-z_]\w*)(?:\s*::\s*<[^()]*>)?$"));
    for _ in 0..8 {
        let mut before = code[..position].trim_end().to_owned();
        if let Some(c) = REF.search(&before) {
            before.truncate(start(&c));
        }
        let before = before.trim_end();
        if IN.is_match(before) {
            return true;
        }
        let Some(opener) = enclosing(code, before.len()) else {
            return false;
        };
        let head = code[..opener].trim_end();
        match bytes[opener] {
            b'{' => return false,
            b'[' => {
                if head.ends_with('!') {
                    if !VEC.is_match(head) {
                        return false;
                    }
                } else if IN.is_match(head) {
                    return true;
                } else if !(head.is_empty() || head.ends_with(['&', '(', ',', '=', '[', ':'])) {
                    return false;
                }
                position = opener;
                continue;
            }
            _ => (),
        }
        if head.ends_with('!') {
            return MACRO
                .search(head)
                .is_some_and(|c| SAFE_MACROS.contains(group(&c, 1)));
        }
        let Some(callee) = CALLEE.search(head) else {
            position = opener;
            continue;
        };
        let name = group(&callee, 2);
        if KEYWORDS.contains(name) || first_upper(name) {
            position = opener;
            continue;
        }
        return group(&callee, 1).is_empty() && resolves(name);
    }
    false
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallKind {
    Macro,
    Method,
    Bare,
    Path(String),
}
/// `[(name, kind)]` for every call of `body`.
pub fn calls(body: &str) -> Vec<(String, CallKind)> {
    let mut found = Vec::new();
    for c in CALL.find_all(body) {
        let name = c.name("name").map_or("", |m| m.as_str());
        let name_start = c.name("name").map_or(0, |m| m.start());
        let before = &body[..name_start];
        static FN_BEFORE: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bfn\s+$"));
        if KEYWORDS.contains(name) || FN_BEFORE.is_match(before) || first_upper(name) {
            continue;
        }
        if c.name("bang").is_some() {
            found.push((name.to_owned(), CallKind::Macro));
        } else if c.name("dot").is_some() {
            found.push((name.to_owned(), CallKind::Method));
        } else if let Some(q) = Re::new(r"([A-Za-z_]\w*)\s*::\s*$").search(before) {
            found.push((name.to_owned(), CallKind::Path(group(&q, 1).to_owned())));
        } else {
            found.push((name.to_owned(), CallKind::Bare));
        }
    }
    found
}

/// Variant names of the enums declared in `code`: values, not foreign types.
pub fn variants(code: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let bytes = code.as_bytes();
    for c in Re::new(r"\benum\s+[A-Za-z_]\w*[^{;]*\{").find_all(code) {
        let mut depth = 1i32;
        let mut index = end(&c);
        let mut segment = String::new();
        while index < bytes.len() && depth != 0 {
            let ch = bytes[index];
            if matches!(ch, b'{' | b'(' | b'[') {
                depth += 1;
            } else if matches!(ch, b'}' | b')' | b']') {
                depth -= 1;
            }
            if (depth == 1 && ch == b',') || depth == 0 {
                if let Some(f) = Re::new(r"\b([A-Z]\w*)").search(&segment) {
                    names.insert(group(&f, 1).to_owned());
                }
                segment.clear();
            } else {
                segment.push(ch as char);
            }
            index += 1;
        }
    }
    names
}

/// The text of `struct|enum|union name`, fields and variants included.
pub fn declaration(code: &str, name: &str) -> Option<String> {
    let m = Re::new(&format!(
        r"\b(?:struct|enum|union)\s+{}\b",
        crate::re::escape(name)
    ))
    .search(code)?;
    let bytes = code.as_bytes();
    let mut index = end(&m);
    let mut depth = 0i32;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'(' => depth += 1,
            b'}' | b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(code[start(&m)..=index].to_owned());
                }
            }
            b';' if depth == 0 => return Some(code[start(&m)..index].to_owned()),
            _ => (),
        }
        index += 1;
    }
    None
}

/// Top-level comma-separated pieces (brackets and generics balanced).
pub fn split(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut pieces = Vec::new();
    let mut depth = 0i32;
    let mut current = Vec::new();
    for (index, &ch) in bytes.iter().enumerate() {
        if matches!(ch, b'(' | b'[' | b'{' | b'<') {
            depth += 1;
        } else if matches!(ch, b')' | b']' | b'}') {
            depth -= 1;
        } else if ch == b'>' && !(index > 0 && bytes[index - 1] == b'-') {
            depth -= 1;
        }
        if ch == b',' && depth == 0 {
            pieces.push(String::from_utf8_lossy(&current).into_owned());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    pieces.push(String::from_utf8_lossy(&current).into_owned());
    pieces
        .into_iter()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

/// `({callback field: type}, declaration without those types)`, or `None` when
/// callbacks sit anywhere other than a braced struct field.
pub fn callback_fields(declaration: &str) -> Option<(BTreeMap<String, String>, String)> {
    if !CALLBACK_TYPE.is_match(declaration) {
        return Some((BTreeMap::new(), declaration.to_owned()));
    }
    let m = Re::new(r"(?s)^struct\s+\w+[^{;(]*\{(.*)\}$").search(declaration)?;
    let mut fields = BTreeMap::new();
    let mut plain = Vec::new();
    for piece in split(group(&m, 1)) {
        let (name, kind) = piece.split_once(':').unwrap_or((&piece, ""));
        if CALLBACK_TYPE.is_match(kind) {
            let name = name.trim();
            let name = name.strip_prefix("pub").unwrap_or(name).trim();
            fields.insert(name.to_owned(), kind.trim().to_owned());
        } else {
            plain.push(piece.clone());
        }
    }
    Some((fields, plain.join(" ")))
}

/// `[(header, body)]` of every block whose header matches `opener`, braces balanced.
pub fn blocks(code: &str, opener: &Re) -> Vec<(String, String)> {
    let bytes = code.as_bytes();
    let mut found = Vec::new();
    for c in opener.find_all(code) {
        let mut depth = 1i32;
        let mut index = end(&c);
        while index < bytes.len() && depth != 0 {
            if bytes[index] == b'{' {
                depth += 1;
            } else if bytes[index] == b'}' {
                depth -= 1;
            }
            index += 1;
        }
        found.push((
            group(&c, 1).to_owned(),
            code[end(&c)..index.saturating_sub(1).max(end(&c))].to_owned(),
        ));
    }
    found
}

/// `(type text, value text)` of `const|static name: T = value;`.
pub fn constant(code: &str, name: &str) -> Option<(String, String)> {
    let m = Re::new(&format!(
        r"\b(?:const|static)\s+(?:mut\s+)?{}\s*:",
        crate::re::escape(name)
    ))
    .search(code)?;
    let bytes = code.as_bytes();
    let mut depth = 0i32;
    let mut equals = None;
    let mut index = end(&m);
    while index < bytes.len() {
        match bytes[index] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 && equals.is_none() && bytes.get(index + 1) != Some(&b'>') => {
                equals = Some(index);
            }
            b';' if depth == 0 => {
                return equals
                    .map(|e| (code[end(&m)..e].to_owned(), code[e + 1..index].to_owned()));
            }
            _ => (),
        }
        index += 1;
    }
    None
}

/// Capitalized words of `text` that are neither constants nor known types.
pub fn type_words(text: &str, types: &BTreeSet<String>) -> BTreeSet<String> {
    let text = Re::new(r"\b(?:std|core|alloc)(?:\s*::\s*\w+)*\s*::\s*").sub(text, "");
    words(&text)
        .into_iter()
        .filter(|w| first_upper(w) && !is_upper(w) && !types.contains(w))
        .collect()
}

#[derive(Debug)]
struct Parsed {
    bodies: BTreeMap<String, Vec<(String, String, String)>>,
    aliases: BTreeSet<String>,
    types: BTreeSet<String>,
    modules: BTreeSet<String>,
    code: String,
    own: BTreeSet<String>,
}

/// Fail-closed call closure inside the loaded sources.
pub struct FixtureFlow<'a> {
    texts: &'a BTreeMap<String, String>,
    defined: BTreeMap<String, BTreeSet<String>>,
    files: RefCell<BTreeMap<String, Option<Rc<Parsed>>>>,
    paramless: RefCell<bool>,
    codes: RefCell<Option<Rc<Vec<(String, String)>>>>,
    trait_methods: RefCell<Option<Rc<BTreeSet<String>>>>,
    workspace: RefCell<Option<Rc<BTreeSet<String>>>>,
}

impl<'a> FixtureFlow<'a> {
    pub fn new(texts: &'a BTreeMap<String, String>) -> Self {
        let mut defined: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (path, text) in texts {
            for name in capture_all(&FN_NAMES, text) {
                defined.entry(name).or_default().insert(path.clone());
            }
        }
        Self {
            texts,
            defined,
            files: RefCell::new(BTreeMap::new()),
            paramless: RefCell::new(false),
            codes: RefCell::new(None),
            trait_methods: RefCell::new(None),
            workspace: RefCell::new(None),
        }
    }

    fn file(&self, path: &str) -> Option<Rc<Parsed>> {
        if let Some(found) = self.files.borrow().get(path) {
            return found.clone();
        }
        let parsed = self.parse(path);
        self.files
            .borrow_mut()
            .insert(path.to_owned(), parsed.clone());
        parsed
    }

    fn parse(&self, path: &str) -> Option<Rc<Parsed>> {
        let text = self.texts.get(path)?;
        let code = code_text(text).ok()?;
        if code.len() != text.len() {
            return None;
        }
        let lines: Vec<_> = code.lines().collect();
        let raw_lines: Vec<_> = text.lines().collect();
        let mut bodies: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
        for f in rust_functions_ungated(text) {
            let body = lines.get(f.start..f.end)?.join("\n");
            if !Re::new(&format!(r"\bfn\s+{}\b", crate::re::escape(&f.name))).is_match(&body) {
                continue;
            }
            let signature = body.split('{').next().unwrap_or("").to_owned();
            bodies.entry(f.name).or_default().push((
                signature,
                body,
                raw_lines.get(f.start..f.end)?.join("\n"),
            ));
        }
        let mut aliases = BTreeSet::new();
        for u in Re::new(r"\buse\b[^;]*;").find_all(&code) {
            aliases.extend(Re::new(r"\bas\s+(\w+)").findall1(group(&u, 0)));
        }
        let own = capture_all(&TYPE_NAMES, &code);
        let mut types = own.clone();
        types.extend(variants(&code));
        types.extend(STD_TYPES.iter().cloned());
        let modules = capture_all(&MOD_NAMES, &code);
        Some(Rc::new(Parsed {
            bodies,
            aliases,
            types,
            modules,
            code,
            own,
        }))
    }

    fn signature_ok(signature: &str, types: &BTreeSet<String>) -> bool {
        let Some(declared) = Re::new(r"\bfn\s+\w+").search(signature) else {
            return false;
        };
        let rest = &signature[end(&declared)..];
        let rest = Re::new(r"^\s*<\s*(?:'\w+\s*,?\s*)*>")
            .sub(rest, "")
            .into_owned();
        if Re::new(r"^\s*<").is_match(&rest) || UNSAFE_SIGNATURE.is_match(&rest) {
            return false;
        }
        words(&rest)
            .into_iter()
            .filter(|w| first_upper(w))
            .all(|w| types.contains(&w))
    }

    /// Names `body` binds itself: let/for patterns, parameters, fields, closure
    /// parameters and match-arm patterns.
    fn local_names(body: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for pattern in [
            r"\b(?:let|for)\b([^=;{}]*?)(?:=|\bin\b)",
            r"(?:[(,=]|\bmove)\s*\|([^|;{}]*)\|",
        ] {
            for found in Re::new(pattern).findall1(body) {
                names.extend(words(&found));
            }
        }
        names.extend(Re::new(r"\b([A-Za-z_]\w*)\s*:(?!:)").findall1(body));
        let bytes = body.as_bytes();
        for arrow in Re::new(r"=>").find_all(body) {
            let mut depth = 0i32;
            let mut index = start(&arrow) as isize - 1;
            while index >= 0 {
                let ch = bytes[index as usize];
                if matches!(ch, b')' | b']') {
                    depth += 1;
                } else if matches!(ch, b'(' | b'[') {
                    depth -= 1;
                    if depth < 0 {
                        break;
                    }
                } else if depth == 0
                    && (matches!(ch, b'{' | b'}' | b';' | b',')
                        || (index >= 1 && &bytes[index as usize - 1..=index as usize] == b"=>"))
                {
                    break;
                }
                index -= 1;
            }
            names.extend(words(&body[(index + 1) as usize..start(&arrow)]));
        }
        names
    }

    /// `[(file, name)]` a call or callback named `name` reaches from `path`.
    fn reach(&self, path: &str, name: &str) -> Option<Vec<(String, String)>> {
        if let Some(parsed) = self.file(path) {
            if parsed.bodies.contains_key(name) {
                return Some(vec![(path.to_owned(), name.to_owned())]);
            }
        }
        let places = self.defined.get(name)?;
        if places.len() == 1 {
            return Some(vec![(places.iter().next()?.clone(), name.to_owned())]);
        }
        None
    }

    fn call_ok(
        &self,
        path: &str,
        parsed: &Parsed,
        body: &str,
        name: &str,
        kind: &CallKind,
    ) -> Option<Vec<(String, String)>> {
        let (types, modules) = (&parsed.types, &parsed.modules);
        match kind {
            CallKind::Macro => SAFE_MACROS.contains(name).then(Vec::new),
            CallKind::Bare => {
                if name == "drop" || closure_binding(name).is_match(body) {
                    return Some(Vec::new());
                }
                self.reach(path, name)
            }
            CallKind::Path(qualifier) => {
                if (STD_MODULES.contains(qualifier) && !modules.contains(qualifier))
                    || PURE_PATHS.contains(&(qualifier.as_str(), name))
                {
                    return Some(Vec::new());
                }
                if !types.contains(qualifier)
                    && !modules.contains(qualifier)
                    && !ROOT_MODULES.contains(qualifier)
                {
                    return None;
                }
                if parsed.bodies.contains_key(name) {
                    return Some(vec![(path.to_owned(), name.to_owned())]);
                }
                if STD_NAMES.contains(name) && self.trait_methods().contains(name) {
                    return None;
                }
                if (STD_NAMES.contains(name) && STD_TYPES.contains(qualifier))
                    || (name == "default" && types.contains(qualifier))
                {
                    return Some(Vec::new());
                }
                self.reach(path, name)
            }
            CallKind::Method => {
                if parsed.bodies.contains_key(name) {
                    return Some(vec![(path.to_owned(), name.to_owned())]);
                }
                if STD_NAMES.contains(name) {
                    return if self.trait_methods().contains(name) || self.foreign_imports(path) {
                        None
                    } else {
                        Some(Vec::new())
                    };
                }
                self.reach(path, name)
            }
        }
    }

    /// Functions `body` names without calling them (a callback value).
    fn callbacks(
        &self,
        path: &str,
        body: &str,
        raw: &str,
        name: &str,
        word_set: &BTreeSet<String>,
    ) -> Option<Vec<(String, String)>> {
        let called: BTreeSet<_> = calls(body).into_iter().map(|(n, _)| n).collect();
        let local = Self::local_names(body);
        let mut named = Vec::new();
        for word in word_set {
            if STD_NAMES.contains(word)
                || KEYWORDS.contains(word)
                || called.contains(word)
                || local.contains(word)
            {
                continue;
            }
            if word == name || !self.defined.contains_key(word) {
                continue;
            }
            let pattern = Re::new(&format!(r"\b{}\b", crate::re::escape(word)));
            let used = pattern.find_all(body).iter().any(|found| {
                let after = raw.as_bytes().get(end(found)).copied();
                after != Some(b'\'')
                    && after != Some(b'"')
                    && !Re::new(r"(?:\.\s*|')$").is_match(&body[..start(found)])
            });
            if !used {
                continue;
            }
            named.extend(self.reach(path, word)?);
        }
        Some(named)
    }

    /// Whether `type_name` has a trait impl anywhere in its crate, or an
    /// inherent impl in another source: both can run code the walk cannot see.
    fn implemented_elsewhere(&self, whereabouts: &str, type_name: &str) -> bool {
        let krate = format!(
            "{}/",
            whereabouts.split("/src/").next().unwrap_or(whereabouts)
        );
        let escaped = crate::re::escape(type_name);
        let trait_impl = Re::new(&format!(
            r"\bimpl\b[^{{;]*\bfor\s+(?:&\s*(?:mut\s+)?)?{escaped}\b"
        ));
        if self
            .codes()
            .iter()
            .any(|(path, code)| path.starts_with(&krate) && trait_impl.is_match(code))
        {
            return true;
        }
        let inherent = Re::new(&format!(
            r"\bimpl\b(?:\s*<[^{{;>]*>)?\s*(?:[^{{;]*\bfor\s+)?(?:&\s*)?{escaped}\b"
        ));
        self.texts.iter().any(|(path, text)| {
            path != whereabouts && path.starts_with(&krate) && inherent.is_match(text)
        })
    }

    fn trait_methods(&self) -> Rc<BTreeSet<String>> {
        if let Some(found) = self.trait_methods.borrow().as_ref() {
            return found.clone();
        }
        let mut names = BTreeSet::new();
        let traits = Re::new(r"\btrait\s+(\w+)[^{;]*\{");
        let impls = Re::new(r"\bimpl\b([^{;]*)\{");
        for (_, code) in self.codes().iter() {
            for (_, body) in blocks(code, &traits) {
                names.extend(capture_all(&FN_NAMES, &body));
            }
            for (header, body) in blocks(code, &impls) {
                if Re::new(r"\bfor\b").is_match(&header) {
                    names.extend(capture_all(&FN_NAMES, &body));
                }
            }
        }
        let names = Rc::new(names);
        *self.trait_methods.borrow_mut() = Some(names.clone());
        names
    }

    /// Whether the file imports from a crate outside the loaded workspace.
    fn foreign_imports(&self, path: &str) -> bool {
        let workspace = {
            let mut slot = self.workspace.borrow_mut();
            slot.get_or_insert_with(|| {
                Rc::new(
                    self.texts
                        .keys()
                        .map(|p| {
                            p.split("/src/")
                                .next()
                                .unwrap_or(p)
                                .strip_prefix("crates/")
                                .unwrap_or(p)
                                .replace('-', "_")
                        })
                        .collect(),
                )
            })
            .clone()
        };
        let codes = self.codes();
        let code = codes
            .iter()
            .find(|(p, _)| p == path)
            .map_or("", |(_, c)| c.as_str());
        let mut roots = capture_all(&Re::new(r"\buse\s+(?:::)?(\w+)"), code);
        for grouped in Re::new(r"\buse\s*\{([^;]*)\}").findall1(code) {
            roots.extend(Re::new(r"(?:^|,)\s*(?:::)?(\w+)").findall1(&grouped));
        }
        let mut known: BTreeSet<String> = set(&["std", "core", "alloc", "crate", "self", "super"]);
        known.extend(workspace.iter().cloned());
        known.extend(capture_all(&MOD_NAMES, code));
        !roots.is_subset(&known)
    }

    fn codes(&self) -> Rc<Vec<(String, String)>> {
        if let Some(found) = self.codes.borrow().as_ref() {
            return found.clone();
        }
        let codes = Rc::new(
            self.texts
                .iter()
                .map(|(p, t)| (p.clone(), code_text(t).unwrap_or_else(|_| t.clone())))
                .collect::<Vec<_>>(),
        );
        *self.codes.borrow_mut() = Some(codes.clone());
        codes
    }

    /// Whether every type a body names is std, or declared here with vetted
    /// fields and no impl elsewhere.
    fn types_ok(&self, whereabouts: &str, parsed: &Parsed, body: &str) -> bool {
        let (types, code, own) = (&parsed.types, &parsed.code, &parsed.own);
        let mut pending = vec![Re::new(r"(?<=::)\s*[A-Z]\w*").sub(body, "").into_owned()];
        let mut vetted = BTreeSet::new();
        while let Some(text) = pending.pop() {
            if !type_words(&text, types).is_empty() {
                return false;
            }
            for word in words(&text) {
                if !own.contains(&word) || vetted.contains(&word) {
                    continue;
                }
                vetted.insert(word.clone());
                let declared = declaration(code, &word);
                if self.implemented_elsewhere(whereabouts, &word) {
                    return false;
                }
                let Some(declared) = declared else {
                    continue;
                };
                let Some((fields, plain)) = callback_fields(&declared) else {
                    return false;
                };
                if !fields.is_empty() && !self.callbacks_built_ok(&word, &fields, body) {
                    return false;
                }
                pending.push(plain);
            }
        }
        true
    }

    /// Whether a type with callback fields is built in `body` only from
    /// closures bound there, never received.
    fn callbacks_built_ok(
        &self,
        type_name: &str,
        fields: &BTreeMap<String, String>,
        body: &str,
    ) -> bool {
        if !*self.paramless.borrow() {
            return false;
        }
        let bytes = body.as_bytes();
        let literal = Re::new(&format!(
            r"(?<!struct )(?<!impl )\b{}\s*\{{",
            crate::re::escape(type_name)
        ));
        for lit in literal.find_all(body) {
            let mut depth = 1i32;
            let mut index = end(&lit);
            while index < bytes.len() && depth != 0 {
                if matches!(bytes[index], b'{' | b'(' | b'[') {
                    depth += 1;
                } else if matches!(bytes[index], b'}' | b')' | b']') {
                    depth -= 1;
                }
                index += 1;
            }
            let inner = &body[end(&lit)..index.saturating_sub(1).max(end(&lit))];
            for piece in split(inner) {
                if piece.starts_with("..") {
                    return false;
                }
                let (name, colon, value) = match piece.split_once(':') {
                    Some((n, v)) => (n.trim(), true, v.trim()),
                    None => (piece.trim(), false, piece.trim()),
                };
                let value = if colon { value } else { name };
                if !fields.contains_key(name) {
                    continue;
                }
                let value = value.strip_prefix('&').unwrap_or(value).trim();
                let value = value.strip_prefix("mut ").unwrap_or(value).trim();
                let closure =
                    Re::new(r"^\w+$").is_match(value) && closure_binding(value).is_match(body);
                if !(closure || value.starts_with('|') || value.starts_with("move |")) {
                    return false;
                }
            }
        }
        true
    }

    /// Whether every constant or static a body names is declared here and holds
    /// plain data, not a callback.
    fn constants_ok(parsed: &Parsed, body: &str) -> bool {
        let (code, types) = (&parsed.code, &parsed.types);
        let stripped = Re::new(r"::\s*[A-Z_0-9]+").sub(body, "").into_owned();
        for word in word_set(&stripped) {
            if !is_upper(&word) || types.contains(&word) {
                continue;
            }
            let Some((kind, value)) = constant(code, &word) else {
                return false;
            };
            let joined = format!("{kind}={value}");
            if Re::new(r"\b(?:fn|Fn|FnMut|FnOnce|dyn|impl)\b|\||\b[a-z_]\w*\s*\(").is_match(&joined)
            {
                return false;
            }
            if !type_words(&format!("{kind} {value}"), types).is_empty() {
                return false;
            }
        }
        true
    }

    /// Whether every lowercase path named but not called stays inside this
    /// crate's std or modules.
    fn path_values_ok(parsed: &Parsed, body: &str) -> bool {
        let mut modules = parsed.modules.clone();
        modules.extend(STD_MODULES.iter().cloned());
        modules.extend(ROOT_MODULES.iter().cloned());
        for found in Re::new(r"\b([a-z_]\w*)\s*::\s*([a-z_]\w*)\b(?!\s*[(!:<])").find_all(body) {
            let head = group(&found, 1);
            if !modules.contains(head) && !parsed.types.contains(head) {
                return false;
            }
        }
        true
    }

    /// Whether `start_name` and every function it reaches stays in memory.
    pub fn pure(&self, path: &str, start_name: &str) -> bool {
        let mut pending = vec![(path.to_owned(), start_name.to_owned())];
        let mut seen = BTreeSet::new();
        let parsed = self.file(path);
        *self.paramless.borrow_mut() = parsed.as_ref().is_some_and(|p| {
            p.bodies.get(start_name).is_some_and(|all| {
                all.iter()
                    .all(|(sig, _, _)| Re::new(r"\bfn\s+\w+\s*\(\s*\)").is_match(sig))
            })
        });
        while let Some((whereabouts, name)) = pending.pop() {
            if seen.len() > 200 {
                return false;
            }
            if !seen.insert((whereabouts.clone(), name.clone())) {
                continue;
            }
            let Some(parsed) = self.file(&whereabouts) else {
                return false;
            };
            for (signature, body, raw) in parsed.bodies.get(&name).into_iter().flatten() {
                let word_set = word_set(body);
                if !word_set.is_disjoint(&OBSERVER_WORDS)
                    || !word_set.is_disjoint(&parsed.aliases)
                    || !Self::signature_ok(signature, &parsed.types)
                {
                    return false;
                }
                if !(self.types_ok(&whereabouts, &parsed, body)
                    && Self::constants_ok(&parsed, body)
                    && Self::path_values_ok(&parsed, body))
                {
                    return false;
                }
                for (callee, kind) in calls(body) {
                    let Some(resolved) = self.call_ok(&whereabouts, &parsed, body, &callee, &kind)
                    else {
                        return false;
                    };
                    pending.extend(resolved);
                }
                let Some(named) = self.callbacks(&whereabouts, body, raw, &name, &word_set) else {
                    return false;
                };
                pending.extend(named);
            }
        }
        true
    }

    /// Whether the string literal at `[match_start, match_end)` of `source` only
    /// ever feeds in-memory fixture code (#199).
    pub fn synthetic_path_list_member(
        &self,
        source: &str,
        match_start: usize,
        match_end: usize,
    ) -> bool {
        if self.file(source).is_none() {
            return false;
        }
        let Some(text) = self.texts.get(source) else {
            return false;
        };
        let Ok(code) = code_text(text) else {
            return false;
        };
        if !list_consumer_ok(&code, match_start, match_end, &|name| {
            self.reach(source, name).is_some()
        }) {
            return false;
        }
        let line = text[..match_start].bytes().filter(|b| *b == b'\n').count();
        let owner = rust_functions_ungated(text)
            .into_iter()
            .filter(|f| f.start <= line && line < f.end)
            .map(|f| (f.end - f.start, f.name))
            .min();
        owner.is_some_and(|(_, name)| self.pure(source, &name))
    }
}
