//! Readers of changed files that are not in the diff: sources that include a
//! changed file, command/skill/prompt asset readers, fixture and data readers,
//! source-tree scanners and the registered scanned-token tests (#1769, #1926,
//! #2034, #2165, #2495). Every function reads the pinned text snapshot only.
use super::{
    filters::Observer,
    fixture::{self, FixtureFlow},
    git::Hunk,
    identity::ItemReferences,
    readers::code_includes,
    selection, slow,
    source::{self, SourceTree, holds_runnable_test, lib_source, module_of, package_for},
};
use crate::re::{Re, end, group, start};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

pub const ASSET_ROOTS: &[&str] = &[
    ".claude/commands",
    ".claude/skills",
    ".agents",
    ".codex/prompts",
    ".gemini/commands",
];
pub const SCANNED_TOKENS: &str = "scripts/scanned-tokens.json";
const GENERIC_DIRECTORIES: &[&str] = &["tests", "src", "crates", "test", "data"];

static STRING_LITERAL: LazyLock<Re> = LazyLock::new(|| Re::new(r#""((?:[^"\\\n]|\\.)*)""#));
static SIBLING_CRATE: LazyLock<Re> = LazyLock::new(|| Re::new(r"\.\./([A-Za-z0-9_-]+)/"));
static TEST_ATTR: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[(?:[a-z_]+::)?test\b"));
static IGNORE_ATTR: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[ignore\b"));

fn dirname(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}
fn include_target(path: &str, target: &str) -> String {
    source::normalize(&format!("{}/{target}", dirname(path)))
}
fn word_set(text: &str) -> BTreeSet<String> {
    fixture::words(text).into_iter().collect()
}
fn string_literals(text: &str) -> Vec<String> {
    STRING_LITERAL
        .find_all(text)
        .iter()
        .map(|c| group(c, 1).to_owned())
        .collect()
}

/// Rust sources under `crates/` whose code `include_str!`s or `include_bytes!`s
/// a changed file: the reader sees a sibling source or data file as text.
pub fn include_readers(
    sources: &SourceTree,
    changed: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    for (path, text) in &sources.texts {
        if !path.ends_with(".rs") || !path.starts_with("crates/") || !text.contains("include_") {
            continue;
        }
        if code_includes(text)?
            .iter()
            .any(|i| changed.contains(&include_target(path, &i.target)))
        {
            found.push(path.clone());
        }
    }
    Ok(found)
}

/// A regex for a test source that names `root` as a literal path or chained
/// `.join` calls.
pub fn asset_pattern(root: &str) -> Re {
    let parts: Vec<_> = root.split('/').map(crate::re::escape).collect();
    Re::new(&parts.join(r#"(?:/|"\)\s*\.join\(")"#))
}

/// Target sources (integration tests, bins and modules they mount) that read a
/// changed command/skill/prompt asset.
pub fn asset_reader_targets(sources: &SourceTree, changed: &BTreeSet<String>) -> Vec<String> {
    let roots: Vec<_> = ASSET_ROOTS
        .iter()
        .filter(|r| {
            changed
                .iter()
                .any(|c| c == *r || c.starts_with(&format!("{r}/")))
        })
        .collect();
    if roots.is_empty() {
        return Vec::new();
    }
    let patterns: Vec<_> = roots.iter().map(|r| asset_pattern(r)).collect();
    let candidate = Re::new(r"^crates/[a-z0-9_-]+/(?:tests/.+|src/main\.rs|src/bin/.+)$");
    sources
        .texts
        .iter()
        .filter(|(f, _)| f.ends_with(".rs") && candidate.is_match(f))
        .filter(|(_, text)| patterns.iter().any(|rx| rx.is_match(text)))
        .map(|(f, _)| f.clone())
        .collect()
}

/// Whether a string literal names `relative`: by file name or path suffix, or
/// (`directories`) a directory above it.
pub fn literal_names_file(literal: &str, relative: &str, directories: bool) -> bool {
    let literal = literal.trim_matches('/');
    let parts: Vec<_> = relative.split('/').collect();
    if literal.is_empty() {
        return false;
    }
    if Some(&literal) == parts.last()
        || relative == literal
        || relative.ends_with(&format!("/{literal}"))
    {
        return true;
    }
    directories
        && literal.chars().count() >= 4
        && !literal.contains('/')
        && !GENERIC_DIRECTORIES.contains(&literal)
        && parts[..parts.len() - 1].contains(&literal)
}

/// Rust sources that name a changed non-Rust file in a string literal.
pub fn data_readers(sources: &SourceTree, changed: &BTreeSet<String>) -> Vec<String> {
    let data: Vec<_> = changed
        .iter()
        .filter(|p| !p.ends_with(".rs") && !p.ends_with("Cargo.toml") && !p.ends_with("Cargo.lock"))
        .cloned()
        .collect();
    if data.is_empty() {
        return Vec::new();
    }
    let location = Re::new(r"^crates/([a-z0-9_-]+)/((?!src/)[^/].*)$");
    let needles: Vec<_> = data
        .iter()
        .map(|p| p.rsplit('/').next().unwrap_or(p).to_owned())
        .collect();
    let mut found: BTreeSet<&String> = sources
        .texts
        .iter()
        .filter(|(p, text)| {
            p.ends_with(".rs")
                && p.starts_with("crates/")
                && needles.iter().any(|n| text.contains(n))
        })
        .map(|(p, _)| p)
        .collect();
    for path in &data {
        if let Some(c) = location.search(path) {
            let prefix = format!("crates/{}/", group(&c, 1));
            found.extend(sources.texts.keys().filter(|f| f.starts_with(&prefix)));
        }
    }
    let flow = FixtureFlow::new(&sources.texts);
    let mut readers = Vec::new();
    for source in found {
        let text = &sources.texts[source];
        let matches = STRING_LITERAL.find_all(text);
        for path in &data {
            let located = location.search(path);
            let same_package = located
                .as_ref()
                .is_some_and(|c| source.starts_with(&format!("crates/{}/", group(c, 1))));
            let naming: Vec<_> = matches
                .iter()
                .filter(|m| {
                    let literal = group(m, 1);
                    (literal.trim_matches('/').contains('/')
                        && literal_names_file(literal, path, false))
                        || (same_package
                            && located
                                .as_ref()
                                .is_some_and(|c| literal_names_file(literal, group(c, 2), true)))
                })
                .collect();
            if naming.is_empty() {
                continue;
            }
            if naming
                .iter()
                .all(|m| flow.synthetic_path_list_member(source, start(m), end(m)))
            {
                continue;
            }
            readers.push(source.clone());
            break;
        }
    }
    readers
}

/// Landing filters of a source that reads a changed file: its targets, or its
/// library module's tests. A sharded module is selected by source scanners.
pub fn reader_filters(sources: &SourceTree, path: &str) -> Result<Vec<String>, String> {
    let mut filters = sources.target_filters(path)?;
    let module = sources.module(path);
    if let (Some(package), Some(module)) = (package_for(path), module) {
        if !path.starts_with(source::RSID_SRC)
            && !path.starts_with(source::STORE_SRC)
            && sources.lib_mounted(path)
            && holds_runnable_test(
                sources.texts.get(path).map_or("", String::as_str),
                &sources.host,
            )?
        {
            filters.push(format!("{package}={module}"));
        }
    }
    Ok(filters)
}

/// A string literal naming a source tree or a Rust source. Mirrors
/// `source_tree_path` in the lander's test_impact.
pub fn source_tree_literal(literal: &str) -> bool {
    let literal = literal.trim_end_matches('/');
    literal == "src"
        || literal.ends_with("/src")
        || literal.starts_with("src/")
        || literal.contains("/src/")
        || literal.ends_with(".rs")
}

fn is_test_module_path(path: &str) -> bool {
    module_of(path).is_some_and(|m| source::is_test_module(&m))
}

/// Library tests that scan crate source trees when a production source they
/// read changed (#2034). A test resolving the crate root (directly or through a
/// helper) and naming a source-tree literal reads its own crate's `src/` plus
/// every `../<crate>/` sibling it names.
pub fn source_reader_tests(
    sources: &SourceTree,
    index: &ItemReferences,
    changed: &BTreeSet<String>,
    comment_only: &BTreeSet<String>,
) -> Vec<Observer> {
    let touched: BTreeSet<_> = changed
        .iter()
        .filter(|p| !comment_only.contains(*p) && !is_test_module_path(p))
        .filter_map(|p| package_for(p))
        .collect();
    if touched.is_empty() {
        return Vec::new();
    }
    struct Row {
        name: String,
        module: String,
        package: String,
        test: bool,
        shards: BTreeSet<String>,
        words: BTreeSet<String>,
        literals: Vec<String>,
        root: bool,
    }
    let mut rows = Vec::new();
    for f in &index.functions {
        if !lib_source(&f.path) {
            continue;
        }
        let Some(module) = sources.module(&f.path) else {
            continue;
        };
        let test = TEST_ATTR.is_match(&f.function.attrs);
        if !(test || source::is_test_module(&module)) {
            continue;
        }
        let literals = string_literals(&f.function.body);
        let words = f.words.clone();
        rows.push(Row {
            name: f.function.name.clone(),
            module,
            package: package_for(&f.path).unwrap_or_default(),
            test,
            shards: source::shards_in(&f.function.attrs),
            root: literals.iter().any(|l| l == "CARGO_MANIFEST_DIR")
                || f.function.body.contains("CARGO_MANIFEST_DIR")
                || words.contains("manifest_dir"),
            words,
            literals,
        });
    }
    let mut helpers: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for row in &rows {
        if row.root && !row.test {
            helpers.entry(&row.name).or_default().push(row);
        }
    }
    let mut picked = BTreeSet::new();
    for row in &rows {
        if !row.test || !row.literals.iter().any(|l| source_tree_literal(l)) {
            continue;
        }
        let used: Vec<&Row> = row
            .words
            .iter()
            .filter_map(|w| helpers.get(w.as_str()))
            .flatten()
            .copied()
            .collect();
        if !(row.root || !used.is_empty()) {
            continue;
        }
        let mut crates = BTreeSet::from([row.package.clone()]);
        for source in std::iter::once(row).chain(used) {
            for literal in &source.literals {
                crates.extend(SIBLING_CRATE.findall1(literal));
            }
        }
        if !crates.is_disjoint(&touched) {
            for shard in &row.shards {
                picked.insert(Observer {
                    module: row.module.clone(),
                    name: row.name.clone(),
                    shard: shard.clone(),
                });
            }
        }
    }
    picked.into_iter().collect()
}

/// Runnable bin/integration tests that inspect source trees (#2165).
pub fn source_reader_targets(
    sources: &SourceTree,
    changed: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let touched: BTreeSet<_> = changed
        .iter()
        .filter(|p| p.ends_with(".rs") && !is_test_module_path(p))
        .filter_map(|p| {
            let package = package_for(p)?;
            p.starts_with(&format!("crates/{package}/src/"))
                .then_some(package)
        })
        .collect();
    if touched.is_empty() {
        return Ok(Vec::new());
    }
    struct Row {
        name: String,
        test: bool,
        words: BTreeSet<String>,
        literals: Vec<String>,
        root: bool,
    }
    let mut picked = BTreeSet::new();
    for target in sources.targets() {
        if !sources.target_declares_tests(&target)? {
            continue;
        }
        let mut rows = Vec::new();
        let mounted = source::mount_closure(&sources.texts, &target.root, &sources.host).modules;
        for path in mounted.keys() {
            let text = sources.texts.get(path).map_or("", String::as_str);
            for f in source::rust_functions(text, &sources.host) {
                let words: BTreeSet<_> = word_set(&f.body)
                    .into_iter()
                    .filter(|w| *w != f.name)
                    .collect();
                rows.push(Row {
                    test: TEST_ATTR.is_match(&f.attrs),
                    literals: string_literals(&f.body),
                    root: f.body.contains("CARGO_MANIFEST_DIR") || f.body.contains("manifest_dir"),
                    words,
                    name: f.name,
                });
            }
        }
        let helper_names: BTreeSet<_> = rows
            .iter()
            .filter(|r| !r.test && r.root)
            .map(|r| r.name.clone())
            .collect();
        for row in rows.iter().filter(|r| r.test) {
            let used: Vec<_> = rows
                .iter()
                .filter(|h| row.words.contains(&h.name) && helper_names.contains(&h.name))
                .collect();
            if !(row.root || !used.is_empty()) {
                continue;
            }
            let mut crates = BTreeSet::new();
            for literal in row
                .literals
                .iter()
                .chain(used.iter().flat_map(|h| h.literals.iter()))
            {
                if let Some(c) = Re::new(r"(?:^|/)crates/([A-Za-z0-9_-]+)/src/").search(literal) {
                    crates.insert(group(&c, 1).to_owned());
                } else if source_tree_literal(literal) {
                    crates.insert(target.package.clone());
                }
                crates.extend(SIBLING_CRATE.findall1(literal));
            }
            if !crates.is_disjoint(&touched) {
                picked.insert(format!(
                    "{}={}:{}",
                    target.package, target.kind, target.name
                ));
                break;
            }
        }
    }
    Ok(picked.into_iter().collect())
}

/// The registered source readers shared with the lander's test_impact (#2495).
/// `registry` is the pinned `scripts/scanned-tokens.json` of the candidate, or
/// `None` for an older tree without one (selection is kept). A triggered test
/// that cannot resolve is an error rather than a dropped mandatory observer.
pub fn scanned_token_tests(
    sources: &SourceTree,
    old: &SourceTree,
    registry: Option<&str>,
    changed: &BTreeSet<String>,
    hunks: &BTreeMap<String, Vec<Hunk>>,
    comment_only: &BTreeSet<String>,
) -> Result<Vec<Observer>, String> {
    let Some(registry) = registry else {
        return Ok(Vec::new());
    };
    let registry: serde_json::Value =
        serde_json::from_str(registry).map_err(|e| format!("scanned-token registry: {e}"))?;
    if registry.get("version").and_then(serde_json::Value::as_i64) != Some(1) {
        return Err("unsupported scanned-token registry version".into());
    }
    let tokens = registry
        .get("tokens")
        .and_then(serde_json::Value::as_object)
        .ok_or("scanned-token registry has no tokens")?;
    let mut triggered: BTreeSet<String> = if changed.contains(SCANNED_TOKENS) {
        tokens.keys().cloned().collect()
    } else {
        BTreeSet::new()
    };
    let src_dir = Re::new(r"^crates/[^/]+/src/");
    for path in changed {
        if comment_only.contains(path)
            || !path.ends_with(".rs")
            || !src_dir.is_match(path)
            || sources
                .module(path)
                .is_some_and(|m| source::is_test_module(&m))
        {
            continue;
        }
        let before = old.texts.get(path).map_or("", String::as_str);
        let after = sources.texts.get(path).map_or("", String::as_str);
        let empty = Vec::new();
        let path_hunks = hunks.get(path).unwrap_or(&empty);
        if slow::only_test_lines(
            before,
            &selection::changed_lines(path_hunks, true),
            &old.host,
        ) && slow::only_test_lines(
            after,
            &selection::changed_lines(path_hunks, false),
            &sources.host,
        ) {
            continue;
        }
        // Keep literals (SQL included), discard comments. Ambiguous lexing
        // retains the whole text conservatively.
        let views: Vec<String> = [before, after]
            .into_iter()
            .map(|text| {
                let Ok(lexemes) = source::rust_lexemes(text) else {
                    return text.to_owned();
                };
                let mut bytes = text.as_bytes().to_vec();
                for lex in lexemes
                    .iter()
                    .filter(|l| l.kind == source::LexemeKind::Comment)
                {
                    for b in &mut bytes[lex.start..lex.end] {
                        *b = b' ';
                    }
                }
                String::from_utf8_lossy(&bytes).into_owned()
            })
            .collect();
        for (token, rule) in tokens {
            let packages = rule
                .get("packages")
                .and_then(serde_json::Value::as_array)
                .ok_or("scanned-token rule has no packages")?;
            if package_for(path)
                .is_some_and(|p| packages.iter().any(|v| v.as_str() == Some(p.as_str())))
                && views.iter().any(|v| v.contains(token.as_str()))
            {
                triggered.insert(token.clone());
            }
        }
    }
    let mut picked = BTreeSet::new();
    for token in &triggered {
        let tests = tokens[token]
            .get("tests")
            .and_then(serde_json::Value::as_array)
            .ok_or("scanned-token rule has no tests")?;
        for test in tests {
            let package = test
                .get("package")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let name = test
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let mut found = BTreeSet::new();
            for (path, text) in &sources.texts {
                if !lib_source(path) || package_for(path).as_deref() != Some(package) {
                    continue;
                }
                let module = sources.module(path).unwrap_or_default();
                for f in source::rust_functions(text, &sources.host) {
                    if TEST_ATTR.is_match(&f.attrs)
                        && !IGNORE_ATTR.is_match(&f.attrs)
                        && format!("{module}::{}", f.name) == name
                    {
                        for shard in source::shards_in(&f.attrs) {
                            found.insert(Observer {
                                module: module.clone(),
                                name: f.name.clone(),
                                shard,
                            });
                        }
                    }
                }
            }
            if found.is_empty() {
                return Err(format!(
                    "scanned-token test does not resolve: {package}::{name}"
                ));
            }
            picked.extend(found);
        }
    }
    Ok(picked.into_iter().collect())
}
