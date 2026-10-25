//! Registration expansion and changed-test priority, over pinned old/new trees.
use super::{
    filters::{self, Observer},
    git::{self, Hunk, Repository},
    source::{self, SourceTree},
};
use crate::re::{Re, group};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

static MOD: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z_0-9]*)\s*;$"));
static PATH: LazyLock<Re> = LazyLock::new(|| Re::new(r#"^#\[\s*path\s*=\s*"([^"]+)"\s*\]$"#));
static USE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"^pub(?:\([^)]*\))?\s+use\s+([A-Za-z_][A-Za-z_0-9]*(?:::[A-Za-z_][A-Za-z_0-9]*)*)(?:::(?:\*|\{[^{};]+\}))?(?:\s+as\s+[A-Za-z_][A-Za-z_0-9]*)?\s*;$",
    )
});

/// Hunk lines are one based; empty ranges represent insertions/deletions and
/// must not accidentally include the neighboring test or production line.
pub fn changed_lines(hunks: &[Hunk], old: bool) -> BTreeSet<usize> {
    hunks
        .iter()
        .flat_map(|h| {
            let (start, count) = if old {
                (h.old_first, h.old_count)
            } else {
                (h.new_first, h.new_count)
            };
            start..start + count
        })
        .collect()
}

/// Narrow only a proven registration-only edit. Both sides are inspected, so
/// removing a registration still selects its old module. An unresolved use,
/// attribute, inline module or executable line preserves the source coverage.
/// `hunks` must describe the supplied pinned snapshots (Repository::diff).
pub fn registration_sources(
    path: &str,
    sources: &SourceTree,
    old: &SourceTree,
    hunks: &[Hunk],
) -> Vec<String> {
    let fallback = || vec![path.to_owned()];
    if !source::lib_source(path) {
        return fallback();
    }
    let Some(parent) = sources.module(path) else {
        return fallback();
    };
    let mut modules: BTreeMap<String, String> = old.mounts.modules.clone();
    modules.extend(sources.mounts.modules.clone());
    for p in old.texts.keys().chain(sources.texts.keys()) {
        if let Some(module) = source::module_of(p) {
            modules.entry(p.clone()).or_insert(module);
        }
    }
    let mut targets = Vec::new();
    for (tree, is_old) in [(old, true), (sources, false)] {
        let rows: Vec<_> = tree
            .texts
            .get(path)
            .map_or("", String::as_str)
            .lines()
            .collect();
        for number in changed_lines(hunks, is_old) {
            let Some(row) = rows.get(number.saturating_sub(1)) else {
                return fallback();
            };
            let mut declaration = row.split("//").next().unwrap_or("").trim();
            if PATH.is_match(declaration) {
                declaration = rows[number..]
                    .iter()
                    .map(|r| r.trim())
                    .find(|r| !r.is_empty())
                    .unwrap_or("");
                if !MOD.is_match(declaration) {
                    return fallback();
                }
            }
            if declaration.is_empty() {
                continue;
            }
            let (module, is_use) = if let Some(c) = MOD.search(declaration) {
                (format!("{parent}::{}", group(&c, 1)), false)
            } else if let Some(c) = USE.search(declaration) {
                let mut parts: Vec<_> = group(&c, 1).split("::").collect();
                let mut prefix: Vec<_> = parent.split("::").collect();
                if parts.first() == Some(&"crate") {
                    prefix.clear();
                    parts.remove(0);
                } else if parts.first() == Some(&"self") {
                    parts.remove(0);
                }
                while parts.first() == Some(&"super") {
                    prefix.pop();
                    parts.remove(0);
                }
                prefix.extend(parts);
                (prefix.join("::"), true)
            } else {
                return fallback();
            };
            let candidates: Vec<_> = modules
                .iter()
                .filter(|(p, m)| {
                    p.as_str() != path
                        && !m.is_empty()
                        && source::package_for(p) == source::package_for(path)
                        && (module == **m || is_use && module.starts_with(&format!("{m}::")))
                })
                .collect();
            let Some(longest) = candidates.iter().map(|(_, m)| m.len()).max() else {
                return fallback();
            };
            for (p, m) in candidates {
                if m.len() == longest && !targets.contains(p) {
                    targets.push(p.clone());
                }
            }
        }
    }
    if targets.is_empty() {
        fallback()
    } else {
        targets
    }
}

/// Keep edited/added tests even when ordinary coverage merging subsumes their
/// namespace filters. This is a priority list, never replacement coverage.
pub fn changed_test_observers(sources: &SourceTree, path: &str, hunks: &[Hunk]) -> Vec<Observer> {
    if !source::lib_source(path) {
        return Vec::new();
    }
    let Some(module) = sources.module(path) else {
        return Vec::new();
    };
    let lines = changed_lines(hunks, false);
    source::rust_functions(
        sources.texts.get(path).map_or("", String::as_str),
        &sources.host,
    )
    .into_iter()
    .filter(|f| source::is_test(&f.attrs) && lines.range(f.attr_start + 1..=f.end).next().is_some())
    .flat_map(|f| {
        source::shards_in(&f.attrs).into_iter().map({
            let module = module.clone();
            move |shard| Observer {
                module: module.clone(),
                name: f.name.clone(),
                shard,
            }
        })
    })
    .collect()
}

pub fn changed_test_filters(
    sources: &SourceTree,
    repo: &Repository,
    base: &str,
    head: &str,
    changed: &[String],
) -> Result<Vec<String>, String> {
    let mut picked = Vec::new();
    for path in changed.iter().filter(|p| source::lib_source(p)) {
        let diff = repo.diff(base, head, path).map_err(|e| e.to_string())?;
        picked.extend(changed_test_observers(
            sources,
            path,
            &git::changed_line_ranges(&diff),
        ));
    }
    Ok(filters::traced_filters(&picked))
}
