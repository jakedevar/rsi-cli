//! Conservative test selection for a landing candidate.
//!
//! The selector prefers a fresh rsi-codegraph snapshot. When that map is not
//! available or is not provably current, it falls back to Rust module paths and
//! widens packages by Cargo workspace reverse dependencies.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use blake3::Hasher;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    CodegraphStore, Result,
    query::{QueryFilter, QueryLimits, SearchMode, SnapshotSelector},
};

pub const MAX_CHANGED_FILES: usize = 128;
/// Upper bound on changed files before the reader map is skipped outright.
pub const MAX_READER_LOOKUP_FILES: usize = 2000;

/// The single source of truth for the rsid test shard inventory. The landing
/// tool (`rsi-rolling-land`) imports this list; `rsid_shards_match_cargo_features`
/// keeps it equal to the `test-shard-*` features in `crates/rsid/Cargo.toml`.
pub const RSID_SHARDS: &[&str] = &[
    "memory-01",
    "memory-02",
    "other-01",
    "other-02",
    "other-03",
    "other-04",
    "other-05",
    "session-01",
    "session-02",
    "session-03",
    "session-04",
    "session-05",
    "store-01",
    "store-02",
    "store-03",
    "store-04",
];

const FIXED_SMOKE_TESTS: &[(&str, &str)] = &[
    (
        "store-01",
        "store::tests::rewind_tears_down_the_non_idempotent_migration_tail",
    ),
    (
        "store-01",
        "store::tests::every_recovered_migration_step_actually_executes",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub content: Option<Vec<u8>>,
}

impl ChangedFile {
    #[must_use]
    pub fn new(path: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            content: Some(content.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceImpactPackage {
    pub id: String,
    pub name: String,
    pub dependencies: Vec<WorkspaceImpactDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceImpactDependency {
    pub name: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceImpactMetadata {
    pub workspace_members: Vec<String>,
    pub packages: Vec<WorkspaceImpactPackage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionKind {
    ChangedModule,
    GraphImpact,
    /// A test or module that reads a changed non-Rust file (`include_str!`,
    /// a fixture or doc path literal).
    ReaderMap,
    FullPackage,
    FullShardSet,
    FixedSmoke,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSelection {
    pub package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard: Option<String>,
    pub kind: SelectionKind,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphStatus {
    Unused,
    Ready,
    Missing,
    Stale,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactSelection {
    pub graph_status: GraphStatus,
    pub fallback: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub selections: Vec<TestSelection>,
}

impl ImpactSelection {
    #[must_use]
    pub fn lander_arguments(&self) -> Vec<Vec<String>> {
        self.selections
            .iter()
            .filter_map(|selection| {
                let value = match (selection.shard.as_deref(), selection.filter.as_deref()) {
                    (Some(shard), Some(filter)) => format!("rsid=shard:{shard}:test({filter})"),
                    (Some(shard), None) => format!("rsid=shard:{shard}"),
                    (None, Some(filter)) => format!("{}={filter}", selection.package),
                    (None, None) => return None,
                };
                Some(vec!["--test-filter".into(), value])
            })
            .collect()
    }

    #[must_use]
    pub fn cargo_test_commands(&self) -> Vec<Vec<String>> {
        self.selections
            .iter()
            .map(|selection| {
                if let Some((flag, name)) = selection.filter.as_deref().and_then(target_filter) {
                    vec![
                        "cargo".into(),
                        "test".into(),
                        "-p".into(),
                        selection.package.clone(),
                        flag.into(),
                        name.into(),
                    ]
                } else if selection.package == "rsid" {
                    match (selection.shard.as_deref(), selection.filter.as_deref()) {
                        (Some(shard), Some(filter)) => vec![
                            "scripts/run-rsid-test-shards.sh".into(),
                            "shard".into(),
                            shard.into(),
                            "--jobs".into(),
                            "4".into(),
                            "--filterset".into(),
                            format!("test({filter})"),
                        ],
                        (Some(shard), None) => vec![
                            "scripts/run-rsid-test-shards.sh".into(),
                            "shard".into(),
                            shard.into(),
                            "--jobs".into(),
                            "4".into(),
                        ],
                        (None, Some(filter)) => vec![
                            "cargo".into(),
                            "test".into(),
                            "-p".into(),
                            "rsid".into(),
                            "--lib".into(),
                            "--".into(),
                            filter.into(),
                        ],
                        (None, None) => vec![
                            "cargo".into(),
                            "test".into(),
                            "-p".into(),
                            "rsid".into(),
                            "--lib".into(),
                        ],
                    }
                } else {
                    let mut command = vec![
                        "cargo".into(),
                        "test".into(),
                        "-p".into(),
                        selection.package.clone(),
                        "--lib".into(),
                    ];
                    if let Some(filter) = &selection.filter {
                        command.push("--".into());
                        command.push(filter.clone());
                    }
                    command
                }
            })
            .collect()
    }
}

pub struct GraphImpactSource<'a> {
    pub store: &'a CodegraphStore,
    pub workspace_id: Uuid,
}

#[must_use]
pub fn reverse_workspace_dependents(
    metadata: &WorkspaceImpactMetadata,
    affected: &[String],
) -> BTreeSet<String> {
    let members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let packages: BTreeMap<&str, &WorkspaceImpactPackage> = metadata
        .packages
        .iter()
        .filter(|package| members.contains(package.id.as_str()))
        .map(|package| (package.name.as_str(), package))
        .collect();
    let mut reached: BTreeSet<&str> = affected
        .iter()
        .filter_map(|name| {
            packages
                .contains_key(name.as_str())
                .then_some(name.as_str())
        })
        .collect();
    loop {
        let newly_reached: Vec<&str> = packages
            .iter()
            .filter(|(name, package)| {
                !reached.contains(*name)
                    && package.dependencies.iter().any(|dependency| {
                        dependency.path.is_some() && reached.contains(dependency.name.as_str())
                    })
            })
            .map(|(name, _)| *name)
            .collect();
        if newly_reached.is_empty() {
            break;
        }
        reached.extend(newly_reached);
    }
    reached
        .into_iter()
        .filter(|name| !affected.iter().any(|affected| affected == name))
        .map(str::to_owned)
        .collect()
}

pub fn select_tests(
    changed_files: &[ChangedFile],
    metadata: &WorkspaceImpactMetadata,
    graph: Option<GraphImpactSource<'_>>,
) -> Result<ImpactSelection> {
    select_tests_with_readers(changed_files, metadata, graph, None)
}

/// Select tests for a change. With a [`ReaderIndex`], non-Rust paths (docs,
/// skills, scripts, tools, fixtures, assets) map to the tests that read them
/// and fall back to the full gate only when no reader map is supplied.
pub fn select_tests_with_readers(
    changed_files: &[ChangedFile],
    metadata: &WorkspaceImpactMetadata,
    graph: Option<GraphImpactSource<'_>>,
    readers: Option<&ReaderIndex>,
) -> Result<ImpactSelection> {
    if changed_files.is_empty() {
        return Ok(ImpactSelection {
            graph_status: GraphStatus::Unused,
            fallback: Some("no changed files".into()),
            notes: Vec::new(),
            selections: smoke_selections(),
        });
    }
    if changed_files.len() > MAX_READER_LOOKUP_FILES {
        return Ok(full_workspace(
            GraphStatus::Uncertain,
            format!(
                "changed-file count {} exceeds the reader-lookup bound {MAX_READER_LOOKUP_FILES}",
                changed_files.len()
            ),
            metadata,
        ));
    }
    for file in changed_files {
        if is_global_build_path(&file.path) {
            return Ok(full_workspace(
                GraphStatus::Unused,
                format!("{} changes affect the whole workspace", file.path),
                metadata,
            ));
        }
    }
    // Non-Rust paths resolve to their readers up front, so the changed-file
    // bound counts only files that can move a test.
    let mut reader_hits: BTreeMap<&str, Vec<FileReader>> = BTreeMap::new();
    if let Some(index) = readers {
        for file in changed_files {
            if !file.path.ends_with(".rs") {
                reader_hits.insert(file.path.as_str(), index.readers_of(&file.path));
            }
        }
    }
    let relevant = changed_files
        .iter()
        .filter(|file| {
            reader_hits
                .get(file.path.as_str())
                .is_none_or(|hits| !hits.is_empty() || file.path.starts_with("crates/"))
        })
        .count();
    if relevant > MAX_CHANGED_FILES {
        return Ok(full_workspace(
            GraphStatus::Uncertain,
            format!(
                "changed-file count {relevant} exceeds the conservative bound {MAX_CHANGED_FILES}"
            ),
            metadata,
        ));
    }

    let mut affected: BTreeMap<String, Vec<TestSelection>> = BTreeMap::new();
    // Test-only readers of a changed non-Rust file: they run, but they do not
    // widen to reverse workspace dependents.
    let mut reader_only: BTreeMap<String, Vec<TestSelection>> = BTreeMap::new();
    let mut notes: Vec<String> = Vec::new();
    let mut fallback: Option<String> = None;
    let mut graph_status = GraphStatus::Missing;

    for file in changed_files {
        let hits = reader_hits.get(file.path.as_str());
        let Some(package) = package_for_path(&file.path, metadata) else {
            if let Some(hits) = hits {
                apply_readers(
                    &file.path,
                    hits,
                    metadata,
                    &mut affected,
                    &mut reader_only,
                    &mut notes,
                );
                continue;
            }
            return Ok(full_workspace(
                GraphStatus::Unused,
                format!("unknown workspace path {}", file.path),
                metadata,
            ));
        };
        if is_package_build_path(&file.path) {
            fallback = Some(format!(
                "{} requires the full {} package",
                file.path, package
            ));
            add_full_package(&mut affected, &package, &file.path, "package build input");
            continue;
        }
        if let Some(hits) = hits {
            if !hits.is_empty() {
                apply_readers(
                    &file.path,
                    hits,
                    metadata,
                    &mut affected,
                    &mut reader_only,
                    &mut notes,
                );
                continue;
            }
            if is_inert_doc_path(&file.path) {
                notes.push(format!("{} is documentation nothing reads", file.path));
                continue;
            }
        }
        if package == "rsid"
            && let Some(bin) = rsid_bin_target(&file.path)
        {
            affected
                .entry(package.clone())
                .or_default()
                .push(TestSelection {
                    package: package.clone(),
                    filter: Some(format!("bin:{bin}")),
                    shard: None,
                    kind: SelectionKind::ChangedModule,
                    reason: format!("changed rsid binary source {}", file.path),
                });
            continue;
        }
        if is_shared_test_support(&file.path) {
            fallback = Some(format!("{} is shared test support", file.path));
            add_full_package(&mut affected, &package, &file.path, "shared test support");
            continue;
        }
        if file.path == "crates/rsid/src/store/mod.rs" {
            fallback = Some(format!("{} may contain a schema migration", file.path));
            add_full_rsids(&mut affected, &file.path);
            continue;
        }
        if package == "rsi-common" && file.path.ends_with(".rs") {
            fallback = Some(format!(
                "{} is a public shared-crate source file",
                file.path
            ));
            add_full_package(&mut affected, &package, &file.path, "shared public API");
            continue;
        }
        if is_ordinary_rust_path(&file.path) && is_macro_source(file) {
            fallback = Some(format!("{} defines a macro", file.path));
            add_full_package(&mut affected, &package, &file.path, "macro definition");
            continue;
        }
        if !is_ordinary_rust_path(&file.path) {
            fallback = Some(format!("{} cannot be mapped conservatively", file.path));
            add_full_package(&mut affected, &package, &file.path, "non-library source");
            continue;
        }
        let Some(module) = rust_module_for_path(&file.path) else {
            fallback = Some(format!("{} maps to a package root", file.path));
            add_full_package(&mut affected, &package, &file.path, "package root");
            continue;
        };
        let entry = affected.entry(package.clone()).or_default();
        entry.push(TestSelection {
            package: package.clone(),
            filter: Some(module),
            shard: None,
            kind: SelectionKind::ChangedModule,
            reason: format!("changed Rust module {}", file.path),
        });
    }

    let graph_changed = affected.iter().all(|(_, selections)| {
        selections
            .iter()
            .all(|selection| selection.filter.is_some())
    });
    if let (Some(source), true) = (graph, graph_changed) {
        match graph_modules(changed_files, &source) {
            Ok((status, modules)) => {
                graph_status = status;
                if status == GraphStatus::Ready {
                    for (path, module) in modules {
                        if changed_files.iter().any(|file| file.path == path) {
                            continue;
                        }
                        let Some(package) = package_for_path(&path, metadata) else {
                            graph_status = GraphStatus::Uncertain;
                            break;
                        };
                        let entry = affected.entry(package).or_default();
                        entry.push(TestSelection {
                            package: package_for_path(&path, metadata).unwrap_or_default(),
                            filter: Some(module),
                            shard: None,
                            kind: SelectionKind::GraphImpact,
                            reason: format!("reverse dependency reached through {path}"),
                        });
                    }
                } else {
                    fallback.get_or_insert(match status {
                        GraphStatus::Stale => "codegraph snapshot is stale".into(),
                        GraphStatus::Missing => "codegraph snapshot is missing".into(),
                        _ => "codegraph impact map is uncertain".into(),
                    });
                }
            }
            Err(_) => {
                graph_status = GraphStatus::Uncertain;
                fallback.get_or_insert_with(|| "codegraph query failed".into());
            }
        }
    }
    if graph_status == GraphStatus::Uncertain {
        return Ok(full_workspace(
            graph_status,
            fallback.unwrap_or_else(|| "codegraph impact map is uncertain".into()),
            metadata,
        ));
    }

    let directly_affected: Vec<String> = affected.keys().cloned().collect();
    for dependent in reverse_workspace_dependents(metadata, &directly_affected) {
        let has_focused = affected.get(&dependent).is_some_and(|selections| {
            selections
                .iter()
                .any(|selection| selection.filter.is_some())
        });
        if !has_focused {
            add_full_package(
                &mut affected,
                &dependent,
                "",
                "workspace reverse dependency without a graph-impacted module",
            );
        }
    }
    for (package, selections) in reader_only {
        affected.entry(package).or_default().extend(selections);
    }
    Ok(dedup_and_finalize(graph_status, fallback, notes, affected))
}

/// Map a changed non-Rust file to the tests that read it.
fn apply_readers(
    path: &str,
    hits: &[FileReader],
    metadata: &WorkspaceImpactMetadata,
    affected: &mut BTreeMap<String, Vec<TestSelection>>,
    reader_only: &mut BTreeMap<String, Vec<TestSelection>>,
    notes: &mut Vec<String>,
) {
    if hits.is_empty() {
        let extra = if path.starts_with("scripts/") || path.starts_with("tools/") {
            "; run its Python/shell tests (scripts/tests, tools/test_*.py) separately"
        } else {
            ""
        };
        notes.push(format!("{path} has no Rust test reader{extra}"));
        return;
    }
    for hit in hits {
        let Some(package) = package_for_path(&hit.path, metadata) else {
            continue;
        };
        let reason = format!("{} reads {path} ({})", hit.path, hit.evidence);
        let target = match reader_target(&hit.path, &package) {
            Some(filter) => filter,
            None => {
                let map = if hit.test_code {
                    &mut *reader_only
                } else {
                    &mut *affected
                };
                add_full_package(map, &package, path, &reason);
                continue;
            }
        };
        let map = if hit.test_code {
            &mut *reader_only
        } else {
            &mut *affected
        };
        map.entry(package.clone()).or_default().push(TestSelection {
            package,
            filter: Some(target),
            shard: None,
            kind: SelectionKind::ReaderMap,
            reason,
        });
    }
}

/// The focused test target for a Rust file that reads a changed path.
/// `None` means the reader has no narrower unit than its whole package.
fn reader_target(reader_path: &str, package: &str) -> Option<String> {
    let rest = reader_path
        .strip_prefix("crates/")?
        .strip_prefix(package)?
        .strip_prefix('/')?;
    if package == "rsid"
        && let Some(bin) = rsid_bin_target(reader_path)
    {
        return Some(format!("bin:{bin}"));
    }
    if let Some(name) = rest
        .strip_prefix("tests/")
        .filter(|name| !name.contains('/'))
        .and_then(|name| name.strip_suffix(".rs"))
    {
        return Some(format!("test:{name}"));
    }
    if rest.starts_with("src/") {
        return rust_module_for_path(reader_path);
    }
    None
}

/// The rsid binary target a `crates/rsid/src` path belongs to.
fn rsid_bin_target(path: &str) -> Option<String> {
    let rest = path.strip_prefix("crates/rsid/src/")?;
    if rest == "main.rs" {
        return Some("rsid".into());
    }
    let after = rest.strip_prefix("bin/")?;
    let name = after.split('/').next()?;
    let name = name.strip_suffix(".rs").unwrap_or(name);
    (!name.is_empty()).then(|| name.to_owned())
}

/// `bin:NAME` / `test:NAME` filters name a cargo target, not a test filter.
fn target_filter(filter: &str) -> Option<(&'static str, &str)> {
    filter
        .strip_prefix("bin:")
        .map(|name| ("--bin", name))
        .or_else(|| filter.strip_prefix("test:").map(|name| ("--test", name)))
}

fn is_inert_doc_path(path: &str) -> bool {
    [
        ".md", ".txt", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".pdf",
    ]
    .iter()
    .any(|suffix| path.ends_with(suffix))
}

/// One Rust source file the reader map can search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderSource {
    pub path: String,
    pub content: String,
}

/// A Rust file that reads a changed non-Rust path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReader {
    pub path: String,
    /// Every reference sits in test code (a tests file or after `#[cfg(test)]`).
    pub test_code: bool,
    pub evidence: String,
}

/// Maps a non-Rust path to the Rust files that read it: `include_str!` and
/// `include_bytes!` edges (resolved), plus path literals, directory literals
/// (two or more segments) and file-name literals for paths of at most two
/// segments.
pub struct ReaderIndex {
    sources: Vec<ReaderSource>,
    cfg_test: Vec<usize>,
    includes: Vec<(usize, String, usize)>,
    first_hits: RefCell<BTreeMap<String, Vec<(usize, usize)>>>,
}

impl ReaderIndex {
    #[must_use]
    pub fn new(sources: Vec<ReaderSource>) -> Self {
        let cfg_test = sources
            .iter()
            .map(|source| source.content.find("#[cfg(test)]").unwrap_or(usize::MAX))
            .collect();
        let mut includes = Vec::new();
        for (index, source) in sources.iter().enumerate() {
            for marker in ["include_str!(", "include_bytes!(", "include!("] {
                let mut from = 0;
                while let Some(found) = source.content[from..].find(marker) {
                    let start = from + found + marker.len();
                    from = start;
                    let rest = source.content[start..].trim_start();
                    let Some(rest) = rest.strip_prefix('"') else {
                        continue;
                    };
                    let Some(end) = rest.find('"') else { continue };
                    if let Some(resolved) = resolve_relative(&source.path, &rest[..end]) {
                        includes.push((index, resolved, start));
                    }
                }
            }
        }
        Self {
            sources,
            cfg_test,
            includes,
            first_hits: RefCell::new(BTreeMap::new()),
        }
    }

    fn occurrences(&self, needle: &str) -> Vec<(usize, usize)> {
        if let Some(hits) = self.first_hits.borrow().get(needle) {
            return hits.clone();
        }
        let hits: Vec<(usize, usize)> = self
            .sources
            .iter()
            .enumerate()
            .filter_map(|(index, source)| source.content.find(needle).map(|pos| (index, pos)))
            .collect();
        self.first_hits
            .borrow_mut()
            .insert(needle.to_owned(), hits.clone());
        hits
    }

    /// The Rust files that read `target`, with the earliest evidence of each.
    #[must_use]
    pub fn readers_of(&self, target: &str) -> Vec<FileReader> {
        let mut best: BTreeMap<usize, (usize, String)> = BTreeMap::new();
        let mut note = |index: usize, pos: usize, evidence: String| {
            let slot = best.entry(index).or_insert((pos, evidence.clone()));
            if pos < slot.0 {
                *slot = (pos, evidence);
            }
        };
        for (index, resolved, pos) in &self.includes {
            if resolved == target {
                note(*index, *pos, "include macro".into());
            }
        }
        for (index, pos) in self.occurrences(target) {
            note(index, pos, "path literal".into());
        }
        let segments: Vec<&str> = target.split('/').collect();
        for depth in 2..segments.len() {
            let dir = segments[..depth].join("/");
            for needle in [format!("{dir}\""), format!("{dir}/\"")] {
                for (index, pos) in self.occurrences(&needle) {
                    note(index, pos, format!("directory literal {dir}"));
                }
            }
        }
        // A bare file-name literal is only evidence for shallow paths; for a
        // deeper path it collides with same-named files elsewhere (two
        // `worker-contract.md` files), and its directory literal covers the
        // `join(dir).join(name)` shape.
        if let Some(base) = segments
            .last()
            .filter(|base| !base.is_empty() && segments.len() <= 2)
        {
            for needle in [format!("\"{base}\""), format!("/{base}\"")] {
                for (index, pos) in self.occurrences(&needle) {
                    note(index, pos, format!("file-name literal {base}"));
                }
            }
        }
        best.into_iter()
            .filter(|(index, _)| self.sources[*index].path != target)
            .map(|(index, (pos, evidence))| {
                let path = self.sources[index].path.clone();
                let test_code = is_test_file_path(&path) || pos >= self.cfg_test[index];
                FileReader {
                    path,
                    test_code,
                    evidence,
                }
            })
            .collect()
    }
}

fn is_test_file_path(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/benches/")
        || path.ends_with("tests.rs")
        || path.ends_with("test_utils.rs")
        || path.ends_with("test_support.rs")
}

/// Resolve `relative` against the directory of `source_path`, collapsing `..`.
fn resolve_relative(source_path: &str, relative: &str) -> Option<String> {
    let mut parts: Vec<&str> = source_path.split('/').collect();
    parts.pop();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn graph_modules(
    changed_files: &[ChangedFile],
    source: &GraphImpactSource<'_>,
) -> Result<(GraphStatus, BTreeMap<String, String>)> {
    let session = source
        .store
        .query(source.workspace_id, SnapshotSelector::CurrentReady)?;
    for file in changed_files {
        if !is_ordinary_rust_path(&file.path) {
            continue;
        }
        let Some(content) = &file.content else {
            return Ok((GraphStatus::Stale, BTreeMap::new()));
        };
        let mut hasher = Hasher::new();
        hasher.update(content);
        let expected = hasher.finalize().to_hex().to_string();
        if session.file_source_digest(&file.path)?.as_deref() != Some(expected.as_str()) {
            return Ok((GraphStatus::Stale, BTreeMap::new()));
        }
    }
    let mut seeds = Vec::new();
    let search_limits = QueryLimits {
        max_results: 128,
        ..QueryLimits::default()
    };
    for file in changed_files {
        if !is_ordinary_rust_path(&file.path) {
            continue;
        }
        let matches = session.search(
            SearchMode::ExactPath,
            &file.path,
            &QueryFilter::default(),
            search_limits,
        )?;
        if !matches.meta.complete {
            return Ok((GraphStatus::Uncertain, BTreeMap::new()));
        }
        seeds.extend(matches.value.iter().map(|node| node.id));
    }
    if seeds.is_empty() {
        return Ok((GraphStatus::Missing, BTreeMap::new()));
    }
    let impact_limits = QueryLimits {
        max_results: 128,
        max_depth: 8,
        max_nodes: 512,
        max_relations: 1024,
        max_frontier: 256,
        ..QueryLimits::default()
    };
    let impact = session.subgraph(
        &seeds,
        crate::query::Direction::Incoming,
        &QueryFilter::default(),
        impact_limits,
    )?;
    if !impact.meta.complete {
        return Ok((GraphStatus::Uncertain, BTreeMap::new()));
    }
    let mut modules = BTreeMap::new();
    for node in &impact.value.nodes {
        if let Some(module) = rust_module_for_path(&node.span.path) {
            modules.insert(node.span.path.clone(), module);
        }
    }
    if modules.is_empty() {
        return Ok((GraphStatus::Missing, BTreeMap::new()));
    }
    Ok((GraphStatus::Ready, modules))
}

fn smoke_selections() -> Vec<TestSelection> {
    FIXED_SMOKE_TESTS
        .iter()
        .map(|(shard, filter)| TestSelection {
            package: "rsid".into(),
            filter: Some((*filter).into()),
            shard: Some((*shard).into()),
            kind: SelectionKind::FixedSmoke,
            reason: "fixed landing smoke set".into(),
        })
        .collect()
}

fn add_full_package(
    affected: &mut BTreeMap<String, Vec<TestSelection>>,
    package: &str,
    path: &str,
    reason: &str,
) {
    if package == "rsid" {
        add_full_rsids(affected, path);
        return;
    }
    let entry = affected.entry(package.to_owned()).or_default();
    entry.push(TestSelection {
        package: package.to_owned(),
        filter: None,
        shard: None,
        kind: SelectionKind::FullPackage,
        reason: format!("{path} requires the full package test gate: {reason}"),
    });
}

fn add_full_rsids(affected: &mut BTreeMap<String, Vec<TestSelection>>, path: &str) {
    let entry = affected.entry("rsid".into()).or_default();
    for shard in RSID_SHARDS {
        entry.push(TestSelection {
            package: "rsid".into(),
            filter: None,
            shard: Some((*shard).into()),
            kind: SelectionKind::FullShardSet,
            reason: format!("{path} requires the full rsid shard gate {shard}"),
        });
    }
}

fn full_workspace(
    graph_status: GraphStatus,
    reason: String,
    metadata: &WorkspaceImpactMetadata,
) -> ImpactSelection {
    let mut affected = BTreeMap::new();
    for package in workspace_packages(metadata) {
        if package == "rsid" {
            add_full_rsids(&mut affected, "workspace-wide fallback");
        } else {
            add_full_package(
                &mut affected,
                &package,
                "workspace-wide fallback",
                "workspace-wide fallback",
            );
        }
    }
    dedup_and_finalize(graph_status, Some(reason), Vec::new(), affected)
}

fn workspace_packages(metadata: &WorkspaceImpactMetadata) -> Vec<String> {
    let members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    metadata
        .packages
        .iter()
        .filter(|package| members.contains(package.id.as_str()))
        .map(|package| package.name.clone())
        .collect()
}

fn dedup_and_finalize(
    graph_status: GraphStatus,
    fallback: Option<String>,
    notes: Vec<String>,
    affected: BTreeMap<String, Vec<TestSelection>>,
) -> ImpactSelection {
    let mut seen = BTreeSet::new();
    let mut selections = Vec::new();
    for (_package, package_selections) in affected {
        for selection in package_selections {
            let key = (
                selection.package.clone(),
                selection.filter.clone(),
                selection.shard.clone(),
            );
            if seen.insert(key) {
                selections.push(selection);
            }
        }
    }
    for smoke in smoke_selections() {
        let key = (
            smoke.package.clone(),
            smoke.filter.clone(),
            smoke.shard.clone(),
        );
        if seen.insert(key) {
            selections.push(smoke);
        }
    }
    selections.sort_by(|left, right| {
        left.package
            .cmp(&right.package)
            .then_with(|| left.filter.cmp(&right.filter))
            .then_with(|| left.shard.cmp(&right.shard))
            .then_with(|| kind_order(left.kind).cmp(&kind_order(right.kind)))
    });
    ImpactSelection {
        graph_status,
        fallback,
        notes,
        selections,
    }
}

fn kind_order(kind: SelectionKind) -> u8 {
    match kind {
        SelectionKind::ChangedModule => 0,
        SelectionKind::GraphImpact => 1,
        SelectionKind::ReaderMap => 2,
        SelectionKind::FullPackage => 3,
        SelectionKind::FullShardSet => 4,
        SelectionKind::FixedSmoke => 5,
    }
}

fn is_global_build_path(path: &str) -> bool {
    path.starts_with(".cargo/")
        || path.starts_with(".config/")
        || path == "Cargo.lock"
        || path == "Cargo.toml"
        || path == "rust-toolchain"
        || path == "rust-toolchain.toml"
        || path == "build.rs"
}

fn is_package_build_path(path: &str) -> bool {
    path == "build.rs"
        || path.ends_with("/build.rs")
        || path.ends_with("Cargo.toml")
        || path.ends_with("Cargo.lock")
        || path.ends_with("rust-toolchain")
        || path.ends_with("rust-toolchain.toml")
}

fn is_shared_test_support(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/fixtures/")
        || path.contains("/testing/")
        || path.ends_with("test_utils.rs")
        || path.ends_with("test_support.rs")
}

fn is_ordinary_rust_path(path: &str) -> bool {
    path.starts_with("crates/") && path.ends_with(".rs")
}

fn is_macro_source(file: &ChangedFile) -> bool {
    file.content.as_ref().is_some_and(|content| {
        let source = String::from_utf8_lossy(content);
        source.contains("macro_rules!") || source.contains("proc_macro")
    })
}

fn package_for_path(path: &str, metadata: &WorkspaceImpactMetadata) -> Option<String> {
    let rest = path.strip_prefix("crates/")?;
    let package = rest.split('/').next()?;
    metadata
        .packages
        .iter()
        .any(|candidate| candidate.name == package)
        .then(|| package.to_owned())
}

fn rust_module_for_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("crates/")?;
    let mut parts = rest.split('/').collect::<Vec<_>>();
    if parts.len() < 3 || parts[0].is_empty() || parts[1] != "src" {
        return None;
    }
    let mut module = parts.drain(2..).collect::<Vec<_>>();
    let file_name = module.pop()?;
    if file_name == "lib.rs" || file_name == "main.rs" {
        return None;
    }
    if file_name != "mod.rs" {
        module.push(file_name.strip_suffix(".rs")?);
    }
    (!module.is_empty()).then(|| module.join("::"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> WorkspaceImpactMetadata {
        WorkspaceImpactMetadata {
            workspace_members: vec!["demo-id".into(), "app-id".into(), "rsid-id".into()],
            packages: vec![
                WorkspaceImpactPackage {
                    id: "demo-id".into(),
                    name: "demo".into(),
                    dependencies: Vec::new(),
                },
                WorkspaceImpactPackage {
                    id: "app-id".into(),
                    name: "app".into(),
                    dependencies: vec![WorkspaceImpactDependency {
                        name: "demo".into(),
                        path: Some("../demo".into()),
                    }],
                },
                WorkspaceImpactPackage {
                    id: "rsid-id".into(),
                    name: "rsid".into(),
                    dependencies: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn leaf_module_selects_module_tests_smoke_and_dependents() {
        let selection = select_tests(
            &[ChangedFile::new(
                "crates/demo/src/widget.rs",
                "pub struct Widget;\n",
            )],
            &metadata(),
            None,
        )
        .unwrap();
        assert_eq!(selection.graph_status, GraphStatus::Missing);
        assert!(
            selection
                .selections
                .iter()
                .any(|item| item.package == "demo" && item.filter.as_deref() == Some("widget"))
        );
        assert!(
            selection
                .selections
                .iter()
                .any(|item| item.package == "app" && item.filter.is_none())
        );
        assert_eq!(
            selection
                .selections
                .iter()
                .filter(|item| item.kind == SelectionKind::FixedSmoke)
                .count(),
            FIXED_SMOKE_TESTS.len()
        );
        assert!(
            selection
                .selections
                .iter()
                .all(|item| !item.reason.is_empty())
        );
    }

    #[test]
    fn shared_helper_and_cargo_manifest_fall_back_to_full() {
        for path in ["crates/demo/tests/common/mod.rs", "crates/demo/Cargo.toml"] {
            let selection =
                select_tests(&[ChangedFile::new(path, "contents\n")], &metadata(), None).unwrap();
            assert!(selection.fallback.is_some(), "{path}");
            assert!(
                selection
                    .selections
                    .iter()
                    .any(|item| item.package == "demo" && item.filter.is_none()),
                "{path}"
            );
            assert!(
                selection
                    .selections
                    .iter()
                    .any(|item| item.package == "app" && item.filter.is_none()),
                "{path}"
            );
        }
    }

    #[test]
    fn rsid_full_shards_render_as_lander_arguments() {
        let selection = select_tests(
            &[ChangedFile::new(
                "crates/rsid/src/store/mod.rs",
                "if version < 1000 {}\n",
            )],
            &metadata(),
            None,
        )
        .unwrap();
        assert_eq!(
            selection
                .lander_arguments()
                .iter()
                .filter(|argument| argument[1].starts_with("rsid=shard:")
                    && !argument[1].contains("test("))
                .count(),
            RSID_SHARDS.len()
        );
    }

    #[test]
    fn unknown_path_falls_back_to_full_workspace() {
        let selection = select_tests(
            &[ChangedFile::new("docs/unknown.md", "text\n")],
            &metadata(),
            None,
        )
        .unwrap();
        assert_eq!(selection.graph_status, GraphStatus::Unused);
        assert!(selection.fallback.is_some());
        assert!(
            selection
                .selections
                .iter()
                .any(|item| item.package == "demo" && item.filter.is_none())
        );
        assert!(
            selection
                .selections
                .iter()
                .any(|item| item.package == "app" && item.filter.is_none())
        );
    }

    fn index() -> ReaderIndex {
        ReaderIndex::new(vec![
            ReaderSource {
                path: "crates/demo/src/skills.rs".into(),
                content: "pub fn prod() {}\n#[cfg(test)]\nmod tests {\n    const S: &str = include_str!(\"../../../.claude/skills/x/SKILL.md\");\n}\n".into(),
            },
            ReaderSource {
                path: "crates/demo/src/embed.rs".into(),
                content: "const E: &[u8] = include_bytes!(\"../assets/logo.bin\");\n".into(),
            },
            ReaderSource {
                path: "crates/rsid/src/main.rs".into(),
                content: "fn main() {}\n#[cfg(test)]\nmod tests { fn t() { let _ = \"scripts/run-thing.sh\"; } }\n".into(),
            },
            ReaderSource {
                path: "crates/demo/tests/corpus.rs".into(),
                content: "fn t() { root.join(\"thoughts/shared/research\"); }\n".into(),
            },
        ])
    }

    fn select(paths: &[&str]) -> ImpactSelection {
        let files: Vec<ChangedFile> = paths
            .iter()
            .map(|path| ChangedFile::new(*path, "x\n"))
            .collect();
        select_tests_with_readers(&files, &metadata(), None, Some(&index())).unwrap()
    }

    fn non_smoke(selection: &ImpactSelection) -> Vec<&TestSelection> {
        selection
            .selections
            .iter()
            .filter(|item| item.kind != SelectionKind::FixedSmoke)
            .collect()
    }

    #[test]
    fn docs_under_thoughts_select_only_the_smoke_set() {
        let selection = select(&["thoughts/shared/manager/notes.md", "docs/agents/x.md"]);
        assert!(non_smoke(&selection).is_empty(), "{selection:?}");
        assert!(selection.fallback.is_none());
        assert_eq!(selection.selections.len(), FIXED_SMOKE_TESTS.len());
    }

    #[test]
    fn many_unread_docs_do_not_hit_the_changed_file_bound() {
        let paths: Vec<String> = (0..400)
            .map(|index| format!("thoughts/shared/notes/{index}.md"))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let selection = select(&refs);
        assert!(non_smoke(&selection).is_empty());
        assert!(selection.fallback.is_none());
    }

    #[test]
    fn a_skill_file_selects_the_test_that_include_strs_it() {
        let selection = select(&[".claude/skills/x/SKILL.md"]);
        let picked = non_smoke(&selection);
        assert_eq!(picked.len(), 1, "{selection:?}");
        assert_eq!(picked[0].package, "demo");
        assert_eq!(picked[0].filter.as_deref(), Some("skills"));
        assert_eq!(picked[0].kind, SelectionKind::ReaderMap);
        // A test-only reader does not widen to reverse dependents (`app`).
        assert!(
            selection
                .selections
                .iter()
                .all(|item| item.package != "app")
        );
    }

    #[test]
    fn a_production_include_widens_to_dependents() {
        let selection = select(&["crates/demo/assets/logo.bin"]);
        assert!(
            selection
                .selections
                .iter()
                .any(|item| { item.package == "demo" && item.filter.as_deref() == Some("embed") })
        );
        assert!(
            selection
                .selections
                .iter()
                .any(|item| item.package == "app" && item.filter.is_none())
        );
    }

    #[test]
    fn a_directory_literal_selects_the_corpus_test_binary() {
        let selection = select(&["thoughts/shared/research/2026-01-01-x.md"]);
        let picked = non_smoke(&selection);
        assert_eq!(picked.len(), 1, "{selection:?}");
        assert_eq!(picked[0].filter.as_deref(), Some("test:corpus"));
        let commands = selection.cargo_test_commands();
        assert!(
            commands
                .iter()
                .any(|command| command == &["cargo", "test", "-p", "demo", "--test", "corpus"])
        );
    }

    #[test]
    fn a_script_read_by_a_bin_test_selects_the_bin_tests() {
        let selection = select(&["scripts/run-thing.sh"]);
        let picked = non_smoke(&selection);
        assert_eq!(picked.len(), 1, "{selection:?}");
        assert_eq!(picked[0].package, "rsid");
        assert_eq!(picked[0].filter.as_deref(), Some("bin:rsid"));
        assert!(
            selection
                .lander_arguments()
                .iter()
                .any(|argument| argument[1] == "rsid=bin:rsid")
        );
    }

    #[test]
    fn rsid_bin_source_selects_bin_tests_not_lib_shards() {
        for (path, bin) in [
            ("crates/rsid/src/main.rs", "rsid"),
            (
                "crates/rsid/src/bin/rsi-rolling-land.rs",
                "rsi-rolling-land",
            ),
            (
                "crates/rsid/src/bin/rsi-rolling-land/gate.rs",
                "rsi-rolling-land",
            ),
        ] {
            let selection = select_tests(
                &[ChangedFile::new(path, "fn main() {}\n")],
                &metadata(),
                None,
            )
            .unwrap();
            let picked = non_smoke(&selection);
            assert_eq!(picked.len(), 1, "{path}: {selection:?}");
            assert_eq!(
                picked[0].filter.as_deref(),
                Some(format!("bin:{bin}").as_str())
            );
            assert!(picked[0].shard.is_none());
            assert!(
                selection
                    .cargo_test_commands()
                    .iter()
                    .any(|command| command == &["cargo", "test", "-p", "rsid", "--bin", bin])
            );
        }
    }

    #[test]
    fn global_inputs_still_force_the_full_workspace() {
        for path in [
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            ".config/nextest.toml",
        ] {
            let selection = select(&[path]);
            assert!(selection.fallback.is_some(), "{path}");
            assert!(
                selection
                    .selections
                    .iter()
                    .any(|item| item.package == "app" && item.filter.is_none()),
                "{path}"
            );
        }
    }

    #[test]
    fn a_crate_manifest_stays_that_crates_full_set() {
        let selection = select(&["crates/demo/build.rs"]);
        assert!(
            selection
                .selections
                .iter()
                .any(|item| item.package == "demo" && item.filter.is_none())
        );
        assert!(
            selection
                .selections
                .iter()
                .all(|item| item.package != "rsid" || item.kind == SelectionKind::FixedSmoke)
        );
    }

    #[test]
    fn rsid_shards_match_cargo_features() {
        let manifest =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../rsid/Cargo.toml"))
                .expect("rsid manifest");
        let mut features: Vec<&str> = manifest
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name.trim()))
            .filter_map(|name| name.strip_prefix("test-shard-"))
            .filter(|name| *name != "mode")
            .collect();
        features.sort_unstable();
        let mut shards: Vec<&str> = RSID_SHARDS.to_vec();
        shards.sort_unstable();
        assert_eq!(
            features, shards,
            "RSID_SHARDS must list every rsid test-shard-* feature"
        );
    }

    #[test]
    fn the_lander_imports_the_shared_shard_list() {
        let lander = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../rsid/src/bin/rsi-rolling-land.rs"
        ))
        .expect("lander source");
        assert!(lander.contains("use rsi_codegraph::impact::RSID_SHARDS;"));
        assert!(!lander.contains("const RSID_SHARDS"));
    }
}
