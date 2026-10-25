//! Test-count and target queries over the pinned candidate tree: how many tests a
//! landing filter selects, which module trees it would also reach, and which
//! bin/integration targets compile a changed path. Pure functions of the text
//! snapshot; nothing here reads Git or executes a build.
use super::seeds;
use super::source::{
    self, SourceTree, TestFile, holds_runnable_test, is_test, lib_source, package_for,
    rust_functions, test_shard_counts,
};
use crate::re::{Re, group};
use std::collections::BTreeSet;
use std::sync::LazyLock;

static RUST_TOKEN: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\w+|::|->|=>|\.\.=?|&&|\|\||[<>!=+*/%&|^-]=|<<=?|>>=?|[^\s]"));
static TEST_ATTR_ANY: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[(?:[a-z_]+::)?test\b"));

pub fn lex_tokens(text: &str) -> Vec<String> {
    RUST_TOKEN
        .find_all(text)
        .iter()
        .map(|c| group(c, 0).to_owned())
        .collect()
}

/// Shards of the tests themselves: the gate line right above a test attribute.
pub fn test_shards(text: &str) -> BTreeSet<String> {
    test_shard_counts(text).into_keys().collect()
}

/// Equal executable tokens with changed comments, keeping every literal.
/// Doc comments and nested block comments are prose for runtime selection;
/// ambiguous lexing keeps ordinary coverage.
pub fn rust_comment_only_change(before: &str, after: &str) -> bool {
    fn contents(text: &str) -> Result<(Vec<String>, Vec<String>), String> {
        let mut tokens = Vec::new();
        let mut comments = Vec::new();
        let mut previous = 0;
        for lex in source::rust_lexemes(text)? {
            tokens.extend(lex_tokens(&text[previous..lex.start]));
            let piece = text[lex.start..lex.end].to_owned();
            if lex.kind == source::LexemeKind::Comment {
                comments.push(piece);
            } else {
                tokens.push(piece);
            }
            previous = lex.end;
        }
        tokens.extend(lex_tokens(&text[previous..]));
        Ok((tokens, comments))
    }
    match (contents(before), contents(after)) {
        (Ok(old), Ok(new)) => old.0 == new.0 && old.1 != new.1,
        _ => false,
    }
}

/// `PKG=tests::` for a non-sharded library root (lib.rs) that holds its own
/// inline test module. The root has no module path (#2331).
pub fn crate_root_test_filter(sources: &SourceTree, path: &str) -> Result<Option<String>, String> {
    let Some(package) = package_for(path) else {
        return Ok(None);
    };
    if path != format!("crates/{package}/src/lib.rs")
        || path.starts_with(source::RSID_SRC)
        || path.starts_with(source::STORE_SRC)
        || !sources.texts.contains_key(path)
    {
        return Ok(None);
    }
    let text = &sources.texts[path];
    let rows: Vec<_> = text.lines().collect();
    for (start, end) in seeds::test_module_regions(text)? {
        let block = rows
            .get(start..end.min(rows.len()))
            .unwrap_or_default()
            .join("\n");
        if holds_runnable_test(&block, &sources.host)? {
            return Ok(Some(format!("{package}=tests::")));
        }
    }
    Ok(None)
}

impl SourceTree {
    pub fn has_tests(&self, path: &str) -> Result<bool, String> {
        for child in self.child_sources(path) {
            if holds_runnable_test(
                self.texts.get(&child).map_or("", String::as_str),
                &self.host,
            )? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Landing filters of every bin/integration target that compiles `path` and holds a test.
    pub fn target_filters(&self, path: &str) -> Result<Vec<String>, String> {
        let mut filters = Vec::new();
        for owner in self.target_owners(path) {
            if self.target_declares_tests(&owner)? {
                filters.push(format!("{}={}:{}", owner.package, owner.kind, owner.name));
            }
        }
        Ok(filters)
    }

    pub fn test_files(&self) -> &[TestFile] {
        self.caches.test_files.get_or_init(|| {
            self.texts
                .iter()
                .filter(|(p, t)| lib_source(p) && TEST_ATTR_ANY.is_match(t))
                .map(|(p, t)| {
                    (
                        self.module(p),
                        test_shards(t),
                        rust_functions(t, &self.host)
                            .into_iter()
                            .filter(|f| is_test(&f.attrs))
                            .map(|f| f.name)
                            .collect(),
                    )
                })
                .collect()
        })
    }

    /// Whether `test(module)` selects a test of `shard`: some test-bearing library
    /// source gated for the shard has `module` in its module path (nextest's
    /// `test()` is a substring match on `module::path::fn`).
    pub fn filter_matches(&self, shard: &str, module: &str) -> bool {
        self.test_files().iter().any(|(m, shards, names)| {
            m.as_ref().is_some_and(|m| {
                !m.is_empty()
                    && shards.contains(shard)
                    && (m.contains(module)
                        || names.iter().any(|n| format!("{m}::{n}").contains(module)))
            })
        })
    }

    /// Tests the substring filter `test(module)` selects outside `module`'s own tree (#1653).
    pub fn foreign_tests(&self, module: &str) -> Vec<String> {
        let prefix = format!("{module}::");
        let mut found: Vec<String> = self
            .test_files()
            .iter()
            .filter_map(|(m, _, names)| m.as_ref().filter(|m| !m.is_empty()).map(|m| (m, names)))
            .flat_map(|(m, names)| names.iter().map(move |n| format!("{m}::{n}")))
            .filter(|full| full.contains(module) && !full.starts_with(&prefix))
            .collect();
        found.sort();
        found
    }

    fn counts(&self) -> &source::ShardCounts {
        self.shard_test_counts()
    }
    /// Tests of every shard in `module` and the modules mounted beneath it.
    pub fn subtree_tests_everywhere(&self, module: &str) -> usize {
        let prefix = format!("{module}::");
        self.counts()
            .iter()
            .filter(|(m, _)| m == module || m.starts_with(&prefix))
            .map(|(_, c)| c.values().sum::<usize>())
            .sum()
    }
    /// How many tests of `shard` the nextest filter `test(module)` selects (substring match).
    pub fn tests_selected(&self, shard: &str, module: &str) -> usize {
        self.counts()
            .iter()
            .filter(|(m, _)| m.contains(module))
            .map(|(_, c)| c.get(shard).copied().unwrap_or(0))
            .sum()
    }
    /// Tests of `shard` declared directly in `module`, not in a module beneath it.
    pub fn direct_tests(&self, shard: &str, module: &str) -> usize {
        self.counts()
            .iter()
            .filter(|(m, _)| m == module)
            .map(|(_, c)| c.get(shard).copied().unwrap_or(0))
            .sum()
    }
    /// Tests of every shard the filter `test(module)` selects: the `shard:S:`
    /// label only routes it, the run selects the tests of every shard (#1698).
    pub fn tests_selected_everywhere(&self, module: &str) -> usize {
        self.counts()
            .iter()
            .filter(|(m, _)| m.contains(module))
            .map(|(_, c)| c.values().sum::<usize>())
            .sum()
    }
    pub fn direct_tests_everywhere(&self, module: &str) -> usize {
        self.counts()
            .iter()
            .filter(|(m, _)| m == module)
            .map(|(_, c)| c.values().sum::<usize>())
            .sum()
    }
    /// Modules mounted beneath `module` whose sources hold sharded tests.
    pub fn test_children(&self, module: &str) -> Vec<String> {
        let prefix = format!("{module}::");
        let mut found: Vec<String> = self
            .counts()
            .iter()
            .map(|(m, _)| m.clone())
            .filter(|m| m.starts_with(&prefix))
            .collect();
        found.sort();
        found
    }
}
