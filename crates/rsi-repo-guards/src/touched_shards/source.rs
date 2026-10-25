//! Source ownership and lexical inventories from the pinned candidate.
//! This is the derivation planner's deliberately small Rust grammar, not the
//! richer shard-audit cfg evaluator owned by #362.
use super::git::Texts;
use crate::re::{Re, group};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::LazyLock;

pub const RSID_SRC: &str = "crates/rsid/src/";
pub const STORE_SRC: &str = "crates/rsid-store/src/";
pub const HOST_GATED_OFF: &str =
    "\n// rsi-host-gated-off: mounted only behind a cfg that excludes this host\n";
static SOURCE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^crates/([A-Za-z0-9_-]+)/src/(.+)\.rs$"));
static MOD: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"^([ \t]*)(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z_0-9]*)\s*(;|\{)")
});
static PATH: LazyLock<Re> = LazyLock::new(|| Re::new(r#"#\[\s*path\s*=\s*"([^"]+)"\s*\]"#));
static INCLUDE: LazyLock<Re> = LazyLock::new(|| Re::new(r#"^\s*include!\(\s*"([^"]+)"\s*\)\s*;"#));
static CFG_ROW: LazyLock<Re> = LazyLock::new(|| Re::new(r"(?s)^#\[\s*cfg\s*\((.*?)\)\s*\]$"));
static CFG_ANY: LazyLock<Re> = LazyLock::new(|| Re::new(r"(?s)#\[\s*cfg\s*\((.*?)\)\s*\]"));
static ATTR_LINE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^\s*#\["));
static LITERAL: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r###"//[^\n]*|/\*|(?P<raw>[bc]?r(?P<hashes>\#*)")|[bc]?"|'(?:\\.[^'\n]*|[^'\\\n])'"###)
});
static GATE: LazyLock<Re> = LazyLock::new(|| Re::new(r"test-shard-([a-z]+-[0-9]+)"));
static GATE_FULL: LazyLock<Re> = LazyLock::new(|| Re::new(r"^test-shard-([a-z]+-[0-9]+)$"));
static FEATURE: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bfeature\s*=\s*$"));
static TEST: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[(?:[a-z_]+::)?test\b"));
static FN: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^([ \t]*)(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)\b"));
static ITEM: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"^([ \t]*)(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(mod|impl|trait)\b\s*(\w*)")
});
static TEST_GATE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r#"^\s*#\[cfg\(any\(not\(feature = "test-shard-mode"\), feature = "test-shard-([a-z]+-[0-9]+)"\)\)\]\s*$"#,
    )
});

pub fn package_for(path: &str) -> Option<String> {
    SOURCE.search(path).map(|c| group(&c, 1).into())
}
pub fn module_of(path: &str) -> Option<String> {
    let captures = SOURCE.search(path)?;
    let mut parts: Vec<_> = group(&captures, 2).split('/').collect();
    if parts.last() == Some(&"mod") {
        parts.pop();
    }
    if parts.is_empty() || parts == ["lib"] || parts == ["main"] {
        None
    } else {
        Some(parts.join("::"))
    }
}
pub fn lib_source(path: &str) -> bool {
    path.ends_with(".rs")
        && (path.starts_with(STORE_SRC)
            || (path.starts_with(RSID_SRC)
                && !path.starts_with("crates/rsid/src/bin/")
                && path != "crates/rsid/src/main.rs"))
}
pub fn package_of(path: &str) -> &str {
    if path.starts_with(STORE_SRC) {
        "rsid-store"
    } else {
        "rsid"
    }
}
pub fn leaf_package(path: &str) -> Option<String> {
    let package = package_for(path)?;
    match package.as_str() {
        "rsid-remote-read" | "rsid-harness" => Some(package),
        "rsi-graph" if path.starts_with("crates/rsi-graph/src/planning/") => Some(package),
        _ => None,
    }
}
pub fn is_test_module(module: &str) -> bool {
    module
        .split("::")
        .any(|s| s == "tests" || s.ends_with("_tests"))
}
/// The host platform used for cfg evaluation, from the legacy `HOST_OS` spelling.
fn host_platform(host: &str) -> crate::rsid_test_shards::Host {
    crate::rsid_test_shards::Host {
        os: host.into(),
        family: match host {
            "linux" | "macos" => Some("unix".into()),
            "windows" => Some("windows".into()),
            _ => None,
        },
    }
}
/// Exclude only a cfg proven false on this host; malformed or unknown cfgs
/// stay selected so a runnable test is never hidden.
pub fn other_os_gate(row: &str, host: &str) -> bool {
    CFG_ROW.search(row.trim()).is_some_and(|c| {
        crate::rsid_test_shards::cfg::parse_cfg(group(&c, 1))
            .is_ok_and(|p| p.host_value(&host_platform(host)) == Some(false))
    })
}
/// Whether a cfg among the item's attributes proves exclusion on this host.
pub fn cfg_gated_off(attrs: &str, host: &str) -> bool {
    CFG_ANY
        .find_all(attrs)
        .iter()
        .any(|c| other_os_gate(group(c, 0), host))
}
/// Contiguous outer attributes above `index`, including multiline cfgs (nearest first).
pub fn attrs_above(rows: &[&str], index: usize) -> Vec<String> {
    let mut found = Vec::new();
    let mut back = index as isize - 1;
    while back >= 0 {
        if ATTR_LINE.is_match(rows[back as usize]) {
            found.push(rows[back as usize].to_owned());
            back -= 1;
            continue;
        }
        if !rows[back as usize].trim_end().ends_with(']') {
            break;
        }
        let last = back as usize;
        while back >= 0 && !ATTR_LINE.is_match(rows[back as usize]) {
            if rows[back as usize].trim().is_empty() {
                return found;
            }
            back -= 1;
        }
        if back < 0 {
            break;
        }
        found.push(rows[back as usize..=last].join("\n"));
        back -= 1;
    }
    found
}
fn attr_row_count(blocks: &[String]) -> usize {
    blocks.iter().map(|b| b.split('\n').count()).sum()
}
pub fn file_gated_off(text: &str, host: &str) -> bool {
    if text.ends_with(HOST_GATED_OFF) {
        return true;
    }
    for row in text.lines().map(str::trim) {
        if row.is_empty() || row.starts_with("//") {
            continue;
        }
        if !row.starts_with("#![") {
            return false;
        }
        if other_os_gate(&row.replacen("#![", "#[", 1), host) {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub name: Option<String>,
    pub path: Option<String>,
    pub inline: Vec<String>,
    pub off: bool,
}

pub fn mod_declarations(text: &str, host: &str) -> Vec<Declaration> {
    let rows: Vec<_> = text.split('\n').collect();
    let mut stack: Vec<(String, String, bool)> = Vec::new();
    let mut found = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if stack.last().is_some_and(|(indent, _, _)| {
            row.trim_end() == format!("{indent}}}") || row.trim_end() == format!("{indent}}};")
        }) {
            stack.pop();
            continue;
        }
        if let Some(c) = INCLUDE.search(row) {
            found.push(Declaration {
                name: None,
                path: Some(group(&c, 1).into()),
                inline: stack.iter().map(|(_, n, _)| n.clone()).collect(),
                off: stack.iter().any(|(_, _, off)| *off),
            });
            continue;
        }
        if !row.contains("mod ") {
            continue;
        }
        let indent = &row[..row.len() - row.trim_start().len()];
        let mut rest = *row;
        let mut path = None;
        let mut peeled = Vec::new();
        while rest.trim_start().starts_with("#[") {
            let Some((head, tail)) = rest.split_once(']') else {
                break;
            };
            if let Some(c) = PATH.search(rest) {
                path = Some(group(&c, 1).into());
            }
            peeled.push(format!("{}]", head.trim()));
            rest = tail;
        }
        let declaration = format!("{indent}{}", rest.trim_start());
        let Some(c) = MOD.search(&declaration) else {
            continue;
        };
        let own_off = peeled.iter().any(|s| other_os_gate(s, host))
            || cfg_gated_off(&attrs_above(&rows, index).join("\n"), host);
        if group(&c, 3) == "{" {
            stack.push((indent.into(), group(&c, 2).into(), own_off));
            continue;
        }
        if path.is_none() {
            path = attrs_above(&rows, index)
                .iter()
                .find_map(|row| PATH.search(row).map(|c| group(&c, 1).into()));
        }
        found.push(Declaration {
            name: Some(group(&c, 2).into()),
            path,
            inline: stack.iter().map(|(_, n, _)| n.clone()).collect(),
            off: own_off || stack.iter().any(|(_, _, off)| *off),
        });
    }
    found
}

/// POSIX repository paths on every host; never let Windows path rules change
/// `#[path]`, Cargo path dependency or Git tree ownership.
pub fn normalize(path: &str) -> String {
    let mut parts = Vec::new();
    let absolute = path.starts_with('/');
    for part in path.split('/') {
        match part {
            "" | "." => (),
            ".." if parts.last().is_some_and(|s| *s != "..") => {
                parts.pop();
            }
            ".." if !absolute => parts.push(part),
            ".." => (),
            _ => parts.push(part),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}
fn join(prefix: &str, tail: &str) -> String {
    if tail.starts_with('/') {
        normalize(tail)
    } else if prefix.is_empty() {
        normalize(tail)
    } else {
        normalize(&format!("{prefix}/{tail}"))
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Mounts {
    pub modules: BTreeMap<String, String>,
    pub gated_off: BTreeSet<String>,
}
pub fn mount_closure(files: &Texts, root: &str, host: &str) -> Mounts {
    let mut mapped = BTreeMap::from([(root.to_owned(), String::new())]);
    let mut queue = VecDeque::from([root.to_owned()]);
    let mut path_mounted = BTreeSet::new();
    let mut edges = Vec::new();
    while let Some(source) = queue.pop_front() {
        let Some(text) = files.get(&source) else {
            continue;
        };
        let directory = parent(&source);
        let base = source.rsplit('/').next().unwrap_or("");
        let own = if source == root
            || path_mounted.contains(&source)
            || ["mod.rs", "lib.rs", "main.rs"].contains(&base)
        {
            directory.into()
        } else {
            join(directory, base.trim_end_matches(".rs"))
        };
        for decl in mod_declarations(text, host) {
            let inline_path = decl.inline.join("/");
            let candidates = if decl.name.is_none() {
                vec![join(directory, decl.path.as_deref().unwrap_or(""))]
            } else if let Some(attr) = &decl.path {
                vec![join(
                    &if decl.inline.is_empty() {
                        directory.into()
                    } else {
                        join(&own, &inline_path)
                    },
                    attr,
                )]
            } else {
                let anchor = join(&own, &inline_path);
                let name = decl.name.as_deref().unwrap_or("");
                vec![
                    join(&anchor, &format!("{name}.rs")),
                    join(&anchor, &format!("{name}/mod.rs")),
                ]
            };
            let Some(target) = candidates.into_iter().find(|p| files.contains_key(p)) else {
                continue;
            };
            edges.push((source.clone(), target.clone(), decl.off));
            if mapped.contains_key(&target) {
                continue;
            }
            let mut parts = vec![mapped[&source].clone()];
            parts.extend(decl.inline);
            parts.extend(decl.name.clone());
            mapped.insert(
                target.clone(),
                parts
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("::"),
            );
            if decl.name.is_some() && decl.path.is_some() {
                path_mounted.insert(target.clone());
            }
            queue.push_back(target);
        }
    }
    let mut live = BTreeSet::from([root.to_owned()]);
    let mut frontier = vec![root.to_owned()];
    while let Some(source) = frontier.pop() {
        for (_, target, _) in edges.iter().filter(|(p, _, off)| p == &source && !off) {
            if live.insert(target.clone()) {
                frontier.push(target.clone());
            }
        }
    }
    let gated_off = mapped
        .keys()
        .filter(|p| !live.contains(*p))
        .cloned()
        .collect();
    Mounts {
        modules: mapped,
        gated_off,
    }
}

pub fn module_map(files: &Texts, host: &str) -> Mounts {
    let mut roots = BTreeMap::<String, String>::new();
    for path in files.keys() {
        let Some(c) = SOURCE.search(path) else {
            continue;
        };
        if ["lib", "main"].contains(&group(&c, 2)) {
            roots
                .entry(group(&c, 1).into())
                .and_modify(|p| {
                    if path.ends_with("/lib.rs") {
                        *p = path.clone();
                    }
                })
                .or_insert_with(|| path.clone());
        }
    }
    let mut result = Mounts::default();
    for root in roots.values() {
        let mounts = mount_closure(files, root, host);
        for (p, m) in mounts.modules {
            result.modules.entry(p).or_insert(m);
        }
        result.gated_off.extend(mounts.gated_off);
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexemeKind {
    Comment,
    Literal,
}
#[derive(Debug, Clone)]
pub struct Lexeme {
    pub kind: LexemeKind,
    pub start: usize,
    pub end: usize,
    pub contents: Option<(usize, usize)>,
}

pub fn rust_lexemes(text: &str) -> Result<Vec<Lexeme>, String> {
    let mut found = Vec::new();
    let mut position = 0;
    while let Some(c) = LITERAL.search_at(text, position) {
        let start = crate::re::start(&c);
        let mut end = crate::re::end(&c);
        let token = group(&c, 0);
        let mut contents = None;
        if token == "/*" {
            let mut depth = 1;
            while depth > 0 {
                let closer = text[end..]
                    .find("*/")
                    .map(|p| p + end)
                    .ok_or("unterminated Rust comment")?;
                if let Some(opener) = text[end..closer].find("/*") {
                    depth += 1;
                    end += opener + 2;
                } else {
                    depth -= 1;
                    end = closer + 2;
                }
            }
        } else if c.name("raw").is_some() {
            let hashes = c.name("hashes").map_or("", |m| m.as_str());
            let closer = text[end..]
                .find(&format!("\"{hashes}"))
                .ok_or("unterminated Rust raw string")?
                + end;
            contents = Some((end, closer));
            end = closer + 1 + hashes.len();
        } else if token.ends_with('"') && !token.starts_with("//") {
            let first = end;
            while end < text.len() && text.as_bytes()[end] != b'"' {
                end += if text.as_bytes()[end] == b'\\' { 2 } else { 1 };
            }
            if end >= text.len() {
                return Err("unterminated Rust string".into());
            }
            contents = Some((first, end));
            end += 1;
        }
        found.push(Lexeme {
            kind: if token.starts_with("//") || token.starts_with("/*") {
                LexemeKind::Comment
            } else {
                LexemeKind::Literal
            },
            start,
            end,
            contents,
        });
        position = end;
    }
    Ok(found)
}

pub fn code_text(text: &str) -> Result<String, String> {
    let mut code = text.as_bytes().to_vec();
    for span in rust_lexemes(text)? {
        for b in &mut code[span.start..span.end] {
            if *b != b'\n' && *b != b'\r' {
                *b = b' ';
            }
        }
    }
    String::from_utf8(code).map_err(|e| e.to_string())
}

pub fn code_shards(text: &str) -> Result<BTreeSet<String>, String> {
    let mut found = BTreeSet::new();
    for lex in rust_lexemes(text)? {
        let Some((start, end)) = lex.contents else {
            continue;
        };
        if let Some(c) = GATE_FULL.search(&text[start..end]) {
            // Slice by bytes only at character boundaries; 40 is the legacy
            // lookbehind window, and all grammar characters are ASCII.
            let mut from = lex.start.saturating_sub(40);
            while !text.is_char_boundary(from) {
                from += 1;
            }
            if FEATURE.is_match(&text[from..lex.start]) {
                found.insert(group(&c, 1).into());
            }
        }
    }
    Ok(found)
}
pub fn shards_in(text: &str) -> BTreeSet<String> {
    GATE.findall1(text).into_iter().collect()
}
pub fn test_shard_counts(text: &str) -> BTreeMap<String, usize> {
    let rows: Vec<_> = text.split('\n').collect();
    let mut counts = BTreeMap::new();
    for pair in rows.windows(2) {
        if TEST
            .search(pair[1].trim_start())
            .is_some_and(|c| crate::re::start(&c) == 0)
        {
            if let Some(c) = TEST_GATE.search(pair[0]) {
                *counts.entry(group(&c, 1).into()).or_insert(0) += 1;
            }
        }
    }
    counts
}

#[derive(Debug, Clone, Serialize)]
pub struct Function {
    pub name: String,
    /// Zero based start, exclusive end (the original planner convention).
    pub start: usize,
    pub end: usize,
    pub attr_start: usize,
    pub attrs: String,
    pub body: String,
}
pub fn rust_functions(text: &str, host: &str) -> Vec<Function> {
    functions_of(text, Some(host))
}
/// Every function span regardless of cfg gates (the legacy span scan).
pub fn rust_functions_ungated(text: &str) -> Vec<Function> {
    functions_of(text, None)
}
fn functions_of(text: &str, host: Option<&str>) -> Vec<Function> {
    if host.is_some_and(|host| file_gated_off(text, host)) {
        return Vec::new();
    }
    let rows: Vec<_> = text.lines().collect();
    // Preserve original bodies/attributes, but never interpret a declaration
    // or closing brace quoted in a source-scanner fixture as real syntax.
    // Inventory/derivation callers separately refuse lexical errors.
    let lexical = code_text(text).unwrap_or_else(|_| text.to_owned());
    let code_rows: Vec<_> = lexical.lines().collect();
    let mut off = Vec::new();
    for (index, row) in code_rows.iter().enumerate() {
        if let Some(c) = MOD.search(row) {
            if group(&c, 3) == "{"
                && row.trim_end().ends_with('{')
                && host
                    .is_some_and(|host| cfg_gated_off(&attrs_above(&rows, index).join("\n"), host))
            {
                if let Some(end) = (index + 1..rows.len())
                    .find(|i| code_rows[*i].trim_end() == format!("{}}}", group(&c, 1)))
                {
                    off.push((index, end));
                }
            }
        }
    }
    let mut functions = Vec::new();
    for (start, row) in code_rows.iter().enumerate() {
        let Some(c) = FN.search(row) else {
            continue;
        };
        let end = if row.contains('{') && row.trim_end().ends_with('}') {
            Some(start + 1)
        } else {
            (start + 1..rows.len())
                .find(|i| code_rows[*i] == format!("{}}}", group(&c, 1)))
                .map(|i| i + 1)
        };
        let Some(end) = end else {
            continue;
        };
        let attr_start = start - attr_row_count(&attrs_above(&rows, start));
        let attrs = rows[attr_start..start].join("\n");
        if host.is_some_and(|host| cfg_gated_off(&attrs, host))
            || off.iter().any(|(a, b)| *a <= start && start <= *b)
        {
            continue;
        }
        functions.push(Function {
            name: group(&c, 2).into(),
            start,
            end,
            attr_start,
            attrs,
            body: rows[start..end].join("\n"),
        });
    }
    functions
}
pub fn is_test(attrs: &str) -> bool {
    TEST.is_match(attrs)
}

/// Inline module identity for each line; impl/trait blocks don't add a module.
/// Reference resolution (imports, aliases, reexports, receiver types) is a
/// separate continuation; this API makes no claim to resolve call identities.
pub fn inline_scopes(module: &str, text: &str) -> Result<Vec<(String, bool)>, String> {
    let code = code_text(text)?;
    let mut stack: Vec<(String, String, String)> = Vec::new();
    let mut result = Vec::new();
    for row in code.split('\n') {
        if let Some(c) = ITEM.search(row) {
            if !row.trim_end().ends_with([';', '}'])
                && (group(&c, 2) != "mod" || row.trim_end().ends_with('{'))
            {
                stack.push((
                    group(&c, 1).into(),
                    group(&c, 2).into(),
                    group(&c, 3).into(),
                ));
            }
        }
        let mut parts = vec![module];
        parts.extend(
            stack
                .iter()
                .filter(|(_, k, _)| k == "mod")
                .map(|(_, _, n)| n.as_str()),
        );
        result.push((
            parts
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("::"),
            stack.iter().any(|(_, k, _)| k != "mod"),
        ));
        if stack
            .last()
            .is_some_and(|(i, _, _)| row.trim_end() == format!("{i}}}"))
        {
            stack.pop();
        }
    }
    Ok(result)
}

/// `(module, shards, test function names)` of a test-bearing library source.
pub type TestFile = (Option<String>, BTreeSet<String>, Vec<String>);
pub type ShardCounts = Vec<(String, BTreeMap<String, usize>)>;

/// Per-tree scans computed once: a tree is immutable after construction, and the
/// filter-size queries run for every changed module.
#[derive(Clone, Debug, Default)]
pub struct Caches {
    pub test_files: std::sync::OnceLock<Vec<TestFile>>,
    pub counts: std::sync::OnceLock<ShardCounts>,
    /// How often the target inventory was scanned and a target's module closure built
    /// (call-count coverage of #458: each must happen once per immutable tree).
    pub target_scans: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub mount_builds: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub targets: std::sync::OnceLock<Vec<Target>>,
    pub target_mounts: KeyedCache<std::sync::Arc<Mounts>>,
    pub target_tests: KeyedCache<Result<bool, String>>,
}

/// A small keyed memo behind a lock; cloning a tree copies the entries computed so far.
#[derive(Debug)]
pub struct KeyedCache<T>(std::sync::Mutex<BTreeMap<String, T>>);
impl<T> Default for KeyedCache<T> {
    fn default() -> Self {
        Self(std::sync::Mutex::new(BTreeMap::new()))
    }
}
impl<T: Clone> Clone for KeyedCache<T> {
    fn clone(&self) -> Self {
        let entries = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Self(std::sync::Mutex::new(entries))
    }
}
impl<T: Clone> KeyedCache<T> {
    pub fn get_or_insert_with(&self, key: &str, make: impl FnOnce() -> T) -> T {
        let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = entries.get(key) {
            return hit.clone();
        }
        let value = make();
        entries.insert(key.to_owned(), value.clone());
        value
    }
}

#[derive(Clone, Debug)]
pub struct SourceTree {
    pub texts: Texts,
    pub mounts: Mounts,
    pub host: String,
    pub caches: Caches,
}
impl SourceTree {
    pub fn new(mut texts: Texts, host: &str) -> Self {
        let mounts = module_map(&texts, host);
        for path in &mounts.gated_off {
            if let Some(text) = texts.get_mut(path) {
                if !text.ends_with(HOST_GATED_OFF) {
                    text.push_str(HOST_GATED_OFF);
                }
            }
        }
        Self {
            texts,
            mounts,
            host: host.into(),
            caches: Caches::default(),
        }
    }
    pub fn module(&self, path: &str) -> Option<String> {
        self.mounts
            .modules
            .get(path)
            .filter(|s| !s.is_empty())
            .cloned()
            .or_else(|| module_of(path))
    }
    /// A leaf's crate prefix prevents root and sibling modules from colliding
    /// with sharded daemon/store modules during the observer walk.
    pub fn trace_module(&self, path: &str) -> Option<String> {
        if lib_source(path) {
            return self.module(path);
        }
        let package = leaf_package(path)?.replace('-', "_");
        Some(
            self.module(path)
                .map_or(package.clone(), |m| format!("{package}::{m}")),
        )
    }
    pub fn lib_mounted(&self, path: &str) -> bool {
        package_for(path).is_some_and(|p| {
            self.texts.contains_key(&format!("crates/{p}/src/lib.rs"))
                && self.mounts.modules.contains_key(path)
        })
    }
    pub fn child_sources(&self, path: &str) -> Vec<String> {
        let stem = path
            .strip_suffix("/mod.rs")
            .unwrap_or_else(|| path.strip_suffix(".rs").unwrap_or(path));
        let prefix = format!("{stem}/");
        let mut found = vec![path.to_owned()];
        found.extend(
            self.texts
                .keys()
                .filter(|p| {
                    p.starts_with(&prefix)
                        && (!self.lib_mounted(path)
                            || !p.ends_with(".rs")
                            || self.mounts.modules.contains_key(*p)
                            || !self.target_owners(p).is_empty())
                })
                .cloned(),
        );
        if let Some(mounted) = self.mounts.modules.get(path).filter(|s| !s.is_empty()) {
            let prefix = format!("{mounted}::");
            for (p, m) in &self.mounts.modules {
                if m.starts_with(&prefix) && !found.contains(p) {
                    found.push(p.clone());
                }
            }
        }
        found
    }
    pub fn shards_for(&self, path: &str) -> Result<BTreeSet<String>, String> {
        let mut found = BTreeSet::new();
        for p in self.child_sources(path) {
            if let Some(text) = self.texts.get(&p) {
                found.extend(code_shards(text)?);
            }
        }
        Ok(found)
    }
    pub fn shard_test_counts(&self) -> &ShardCounts {
        self.caches.counts.get_or_init(|| {
            self.texts
                .iter()
                .filter(|(p, _)| lib_source(p))
                .filter_map(|(p, text)| {
                    let counts = test_shard_counts(text);
                    self.module(p)
                        .filter(|_| !counts.is_empty())
                        .map(|m| (m, counts))
                })
                .collect()
        })
    }
    pub fn feature_test_inventory(
        &self,
        package: &str,
    ) -> Result<Vec<(String, Vec<String>)>, String> {
        if package != "rsid" {
            return Ok(Vec::new());
        }
        let path = "crates/rsid/src/recursive_dag/smoke_fixture.rs";
        let Some(text) = self.texts.get(path) else {
            return Ok(Vec::new());
        };
        let scopes = inline_scopes("recursive_dag::smoke_fixture", text)?;
        Ok(rust_functions(text, &self.host)
            .into_iter()
            .filter(|f| is_test(&f.attrs))
            .map(|f| {
                (
                    format!("{}::{}", scopes[f.start].0, f.name),
                    vec!["dev-fixtures".into()],
                )
            })
            .collect())
    }
    pub fn changed_modules(
        &self,
        changed: &BTreeSet<String>,
        comment_only: &BTreeSet<String>,
    ) -> Vec<serde_json::Value> {
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        let counts = self.shard_test_counts();
        for path in changed
            .iter()
            .filter(|p| p.ends_with(".rs") && !comment_only.contains(*p))
        {
            let library = package_for(path).and_then(|package| {
                self.mounts
                    .modules
                    .get(path)
                    .filter(|s| !s.is_empty())
                    .map(|module| (package, module))
            });
            let Some((package, module)) = library else {
                for owner in self.target_owners(path) {
                    result.push(serde_json::json!({"package":owner.package,"kind":owner.kind,"target":owner.name,"path":path}));
                }
                continue;
            };
            if !seen.insert((package.clone(), module.clone())) {
                continue;
            }
            let prefix = format!("{module}::");
            let descendants: BTreeSet<_> = self
                .mounts
                .modules
                .iter()
                .filter(|(p, m)| {
                    package_for(p).as_deref() == Some(&package)
                        && m.starts_with(&prefix)
                        && !is_test_module(&m[prefix.len()..])
                        && !changed.contains(*p)
                })
                .map(|(_, m)| m.clone())
                .collect();
            let excluded: Vec<_> = descendants
                .iter()
                .filter(|m| {
                    !descendants
                        .iter()
                        .any(|o| o != *m && m.starts_with(&format!("{o}::")))
                })
                .cloned()
                .collect();
            let tests: usize = counts
                .iter()
                .filter(|(m, _)| m == module)
                .map(|(_, c)| c.values().sum::<usize>())
                .sum();
            result.push(serde_json::json!({"package":package,"module":module,"path":path,"excluded":excluded,"tests":tests}));
        }
        result
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Target {
    pub package: String,
    pub kind: String,
    pub name: String,
    pub root: String,
}
static TARGET: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"^crates/([a-z0-9_-]+)/(?:src/(main\.rs)|src/bin/([A-Za-z0-9_-]+)(?:\.rs|/main\.rs)|tests/([A-Za-z0-9_-]+)(?:\.rs|/main\.rs))$",
    )
});
impl SourceTree {
    /// The target inventory, scanned once per immutable tree.
    pub fn targets(&self) -> Vec<Target> {
        self.caches
            .targets
            .get_or_init(|| self.scan_targets())
            .clone()
    }
    fn scan_targets(&self) -> Vec<Target> {
        self.caches
            .target_scans
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.texts
            .keys()
            .filter_map(|p| {
                let c = TARGET.search(p)?;
                let package = group(&c, 1).to_owned();
                let (kind, name) = if !group(&c, 2).is_empty() {
                    ("bin", package.clone())
                } else if !group(&c, 3).is_empty() {
                    ("bin", group(&c, 3).into())
                } else {
                    ("test", group(&c, 4).into())
                };
                Some(Target {
                    package,
                    kind: kind.into(),
                    name,
                    root: p.clone(),
                })
            })
            .collect()
    }
    /// A target's module closure, built once per target root.
    fn target_mounts(&self, root: &str) -> std::sync::Arc<Mounts> {
        self.caches.target_mounts.get_or_insert_with(root, || {
            self.caches
                .mount_builds
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::sync::Arc::new(mount_closure(&self.texts, root, &self.host))
        })
    }
    pub fn target_owners(&self, path: &str) -> Vec<Target> {
        if !path.ends_with(".rs") {
            return Vec::new();
        }
        let targets = self.targets();
        let owners: Vec<_> = targets
            .iter()
            .filter(|t| {
                path.starts_with(&format!("crates/{}/", t.package))
                    && self.target_mounts(&t.root).modules.contains_key(path)
            })
            .cloned()
            .collect();
        if !owners.is_empty() {
            return owners;
        }
        let layout =
            Re::new(r"^crates/([a-z0-9_-]+)/(?:src/bin/([A-Za-z0-9_-]+)|tests/([A-Za-z0-9_-]+))/");
        let Some(c) = layout.search(path) else {
            return Vec::new();
        };
        let (kind, name) = if !group(&c, 2).is_empty() {
            ("bin", group(&c, 2))
        } else {
            ("test", group(&c, 3))
        };
        targets
            .into_iter()
            .filter(|t| t.package == group(&c, 1) && t.kind == kind && t.name == name)
            .collect()
    }
    pub fn target_declares_tests(&self, target: &Target) -> Result<bool, String> {
        self.caches
            .target_tests
            .get_or_insert_with(&target.root, || self.scan_target_tests(target))
    }
    fn scan_target_tests(&self, target: &Target) -> Result<bool, String> {
        let mut members = self.target_mounts(&target.root).modules.clone();
        let stem = if target.root.ends_with("/main.rs") && !target.root.ends_with("/src/main.rs") {
            target.root.trim_end_matches("main.rs").to_owned()
        } else {
            format!("{}/", target.root.trim_end_matches(".rs"))
        };
        for p in self.texts.keys().filter(|p| p.starts_with(&stem)) {
            members.entry(p.clone()).or_default();
        }
        for path in members.keys() {
            if let Some(text) = self.texts.get(path) {
                if holds_runnable_test(text, &self.host)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

pub fn holds_runnable_test(text: &str, host: &str) -> Result<bool, String> {
    if file_gated_off(text, host) {
        return Ok(false);
    }
    let code = code_text(text)?;
    let unmodelled = Re::new(
        r"#\[\s*(?:(?:\w+::)*(?:rstest|test_case|proptest|quickcheck|wasm_bindgen_test)\b|(?:\w+::){2,}test\b|cfg_attr\([^\]]*?,\s*(?:\w+::)*(?:test|rstest|test_case|proptest|quickcheck)\b)|\b(?:proptest|quickcheck)!\s*[{(]",
    );
    if unmodelled.is_match(&code) {
        return Ok(true);
    }
    let declared = TEST.find_all(&code).len();
    if declared == 0 {
        return Ok(false);
    }
    // Parse code, not fixture strings, but retain attributes' original literals
    // for platform checks. Unknown layouts conservatively keep the filter.
    let rows: Vec<_> = text.lines().collect();
    let ignore = Re::new(r"#\[ignore\b");
    let all = rust_functions_ungated(&code);
    let runnable = rust_functions(text, host);
    let inert = all
        .iter()
        .filter(|f| {
            let attrs = rows[f.attr_start..f.start].join("\n");
            is_test(&attrs)
                && (ignore.is_match(&attrs) || !runnable.iter().any(|r| r.start == f.start))
        })
        .count();
    Ok(declared > inert)
}
