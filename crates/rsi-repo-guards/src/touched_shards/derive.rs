//! The complete derivation: pinned diff to landing filters, receipt fields and
//! verdict, assembled from the independently proven components. Pure planning:
//! it reads Git objects and the working-tree shard manifests and never executes
//! Cargo, a build or a helper (compile and run modes live in `modes`).
use super::{
    data, downstream_tests, e2e,
    filters::{
        self, BROAD_TEST_LIMIT, GATE_FILTER_LENGTH, GATE_FILTER_LIMIT, MergedFilter, Observer,
        WORKER_BOUNDED_STOPS,
    },
    git::{self, DiffContext, Hunk, Repository, Texts, changed_line_ranges},
    guards,
    identity::ItemReferences,
    migration::{self, MigrationReport},
    modes,
    narrow::{self, Direct},
    readers, seeds, selection, slow,
    source::{self, SourceTree, lib_source, package_for},
    tree,
};
use crate::re::{Re, group};
use crate::{
    Ctx, desktop_selection,
    rsid_test_shards::{self, Shard},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::time::Instant;

pub const RPC: &str = "crates/rsid/src/rpc.rs";
pub const DESKTOP_PATH_LIMIT: usize = 50;
const LEAF_FACADES: &[(&str, &[&str])] = &[
    ("rsid-remote-read", &["remote_read"]),
    ("rsid-harness", &["session::harness"]),
    ("rsi-graph", &["topology", "session::graph_runner"]),
];
const LEAF_FACADES_FALLBACK_ONLY: &[&str] = &["rsi-graph"];

static SHARD_FILTER: LazyLock<Re> = LazyLock::new(|| Re::new(r"^rsid=shard:([a-z]+-\d+):(.+)$"));
static SIMPLE_MODULE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^test\(([A-Za-z0-9_:]+)\)$"));
static ANCHORED_MODULE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^test\(/\^([A-Za-z0-9_:]+)::/\)$"));
static TARGET_FILE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"^crates/([a-z0-9_-]+)/(?:src/main\.rs|src/bin/([A-Za-z0-9_-]+)(?:\.rs|/.+)|tests/([A-Za-z0-9_-]+)\.rs)$",
    )
});
static SQL_WORDS: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"\b(SELECT|INSERT\s+INTO|UPDATE|DELETE\s+FROM|CREATE\s+(?:TABLE|INDEX)|ALTER\s+TABLE|DROP\s+(?:TABLE|INDEX))\b",
    )
});
static TMPFS_FIXTURE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\b(?:TempDir::new|tempfile::tempdir|tempdir)\(\)"));
static FIELD_ROW: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?\w+\s*:\s*[^{};]+,$"));
static VERB_MENTION: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b(AGENT_VERBS|READ_VERBS)\b"));
static ENUM_NAME: LazyLock<Re> = LazyLock::new(|| Re::new(r"\benum\s+(\w+)"));

/// Why no usable derivation exists. A refusal never emits a partial receipt: the
/// queue must refuse the derivation rather than accept a narrowed list.
#[derive(Debug)]
pub enum Failure {
    Refusal(String),
    Fatal(String),
    /// The run was terminated by a signal; the partial receipt names what finished.
    Interrupted {
        code: i32,
        partial: Value,
        message: String,
    },
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Fatal(message)
    }
}
fn io<T>(result: std::io::Result<T>) -> Result<T, Failure> {
    result.map_err(|e| Failure::Fatal(e.to_string()))
}

pub fn host_os() -> &'static str {
    std::env::consts::OS
}

/// Everything read from Git (and the working-tree shard manifests) once.
pub struct Loaded {
    pub repo: Repository,
    pub ctx: DiffContext,
    pub sources: SourceTree,
    pub old: SourceTree,
    pub tree_files: BTreeSet<String>,
    pub base_files: Vec<String>,
    pub hunks: BTreeMap<String, Vec<Hunk>>,
    pub added_lines: BTreeMap<String, Vec<(usize, String)>>,
    pub shards: Vec<Shard>,
}

fn crate_sources(repo: &Repository, revision: &str) -> Result<(Vec<String>, Texts), Failure> {
    let files = io(repo.files(revision))?;
    let rust: Vec<_> = files
        .iter()
        .filter(|f| f.starts_with("crates/") && f.ends_with(".rs"))
        .cloned()
        .collect();
    let texts = io(repo.blobs(revision, &rust))?;
    Ok((files, texts))
}

type FileDiffs = (
    BTreeMap<String, Vec<Hunk>>,
    BTreeMap<String, Vec<(usize, String)>>,
);

/// Per-file hunks and added lines of every changed Rust file (#456). Each file is
/// diffed on its own with a literal pathspec, as the legacy selector did, so a path
/// with a space, tab, quote or non-ASCII character keeps its hunk coverage: nothing
/// is recovered from `+++ b/...` headers, which Git quotes or tab-pads. A file whose
/// diff cannot carry line coverage (binary) refuses instead of reading as an empty edit.
fn file_diffs(
    repo: &Repository,
    base: &str,
    head: &str,
    changed: &[String],
) -> Result<FileDiffs, Failure> {
    let mut paths = io(repo.rust_diff_names(base, head))?;
    paths.extend(changed.iter().filter(|p| p.ends_with(".rs")).cloned());
    let mut hunks = BTreeMap::new();
    let mut added = BTreeMap::new();
    for path in paths {
        let diff = io(repo.file_diff(base, head, &path))?;
        if diff.contains("\nBinary files ") || diff.contains("\nGIT binary patch") {
            return Err(Failure::Refusal(format!(
                "{path}: the diff carries no line coverage (binary); refusing to derive from it"
            )));
        }
        let lines = git::added_lines_of(&diff);
        if !lines.is_empty() {
            added.insert(path.clone(), lines);
        }
        hunks.insert(path, changed_line_ranges(&diff));
    }
    Ok((hunks, added))
}

impl Loaded {
    pub fn load(repo_path: &std::path::Path, base: &str, head: &str) -> Result<Self, Failure> {
        Self::load_with(repo_path, base, head, None)
    }

    /// `shards` overrides the working-tree manifests (fixtures without crate manifests).
    pub fn load_with(
        repo_path: &std::path::Path,
        base: &str,
        head: &str,
        shards: Option<Vec<Shard>>,
    ) -> Result<Self, Failure> {
        Self::load_on(repo_path, base, head, shards, host_os())
    }

    /// As `load_with`, evaluating platform cfgs for `host` (`linux`, `macos`, `windows`).
    pub fn load_on(
        repo_path: &std::path::Path,
        base: &str,
        head: &str,
        shards: Option<Vec<Shard>>,
        host: &str,
    ) -> Result<Self, Failure> {
        let repo = io(Repository::new(repo_path))?;
        let ctx = io(DiffContext::load(&repo, base, head))?;
        let (files, texts) = crate_sources(&repo, &ctx.head)?;
        let tree_files: BTreeSet<_> = files
            .into_iter()
            .filter(|f| f.starts_with("crates/"))
            .collect();
        let sources = SourceTree::new(texts, host);
        let any_rust = ctx
            .changed
            .iter()
            .any(|p| p.starts_with("crates/") && p.ends_with(".rs"));
        let (base_files, old) = if any_rust {
            let (files, texts) = crate_sources(&repo, &ctx.merge_base)?;
            (files, SourceTree::new(texts, host))
        } else {
            (
                io(repo.files(&ctx.base))?,
                SourceTree::new(Texts::new(), host),
            )
        };
        let base_files = if any_rust {
            io(repo.files(&ctx.base))?
        } else {
            base_files
        };
        let (hunks, added_lines) = file_diffs(&repo, &ctx.merge_base, &ctx.head, &ctx.changed)?;
        let shards = match shards {
            Some(shards) => shards,
            None => rsid_test_shards::read_shard_packages(&repo.root).map_err(|e| {
                Failure::Refusal(format!("shard package inventory is unavailable: {e}"))
            })?,
        };
        Ok(Self {
            repo,
            ctx,
            sources,
            old,
            tree_files,
            base_files,
            hunks,
            added_lines,
            shards,
        })
    }

    pub fn head_blob(&self, path: &str) -> Option<String> {
        self.repo
            .blobs(&self.ctx.head, &[path.to_owned()])
            .ok()?
            .remove(path)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub filters_only: bool,
    pub run_filters: bool,
    pub compile: bool,
    pub no_compile: bool,
    pub extra_filters: Vec<String>,
    pub receipt: Option<std::path::PathBuf>,
    pub receipt_line: bool,
    /// Replaces the process environment for the cargo helpers (tests, supervised jobs).
    pub env: Option<Vec<(String, String)>>,
    /// Replaces the SIGTERM flag (supervised jobs register their own; tests set it).
    pub interrupt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

pub struct Derived {
    pub receipt: Value,
    pub failed: bool,
    pub summary: String,
    /// The shard map and compile order, for the compile and run modes.
    pub shard_map: BTreeMap<String, Vec<String>>,
    pub stderr: Vec<String>,
}

fn shard_package_map(shards: &[Shard]) -> BTreeMap<String, Vec<String>> {
    shards
        .iter()
        .map(|s| (s.name.clone(), s.packages.clone()))
        .collect()
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}
fn tests_of(picked: &[Observer]) -> usize {
    picked
        .iter()
        .map(|o| (&o.module, &o.name))
        .collect::<BTreeSet<_>>()
        .len()
}

struct Run<'a> {
    loaded: &'a Loaded,
    changed: BTreeSet<String>,
    index: Option<ItemReferences>,
    narrowed: Vec<Value>,
    broad: Vec<Value>,
    direct_found: BTreeMap<Observer, usize>,
    leaf_notes: Vec<Value>,
    unprovable: Vec<String>,
    stderr: Vec<String>,
}

impl<'a> Run<'a> {
    fn index(&mut self) -> Result<&mut ItemReferences, Failure> {
        if self.index.is_none() {
            self.index = Some(ItemReferences::new(&self.loaded.sources)?);
        }
        Ok(self.index.as_mut().expect("built above"))
    }
    fn hunks(&self, path: &str) -> &'a [Hunk] {
        self.loaded.hunks.get(path).map_or(&[], Vec::as_slice)
    }

    fn narrow(&mut self, path: &str) -> Result<narrow::Narrow, Failure> {
        let loaded = self.loaded;
        let hunks = self.hunks(path);
        let index = self.index()?;
        Ok(narrow::narrow_tests(
            &loaded.sources,
            &loaded.old,
            index,
            path,
            hunks,
        ))
    }

    /// Landing filters of one changed sharded source; bounded, or flagged `broad`
    /// (#1418, #1438). A test module that mounts test submodules gets only its own
    /// tests; a filter selecting more than `BROAD_TEST_LIMIT` is narrowed to the
    /// tests that mention the change, and when that cannot be traced it is kept
    /// and flagged.
    fn shard_filters(
        &mut self,
        path: &str,
        module: &str,
        shards: &[String],
    ) -> Result<Vec<String>, Failure> {
        let loaded = self.loaded;
        let sources = &loaded.sources;
        let mut shards = shards.to_vec();
        let mut children = if source::is_test_module(module) {
            sources.test_children(module)
        } else {
            Vec::new()
        };
        if !children.is_empty() && !shards.iter().any(|s| sources.direct_tests(s, module) > 0) {
            children.clear();
        }
        let has_children = !children.is_empty();
        if has_children {
            shards.retain(|s| sources.direct_tests(s, module) > 0);
        }
        let mut scope = if has_children {
            filters::direct_filterset(module)
        } else {
            format!("test({module})")
        };
        // The real run selects the tests of every shard whichever shard labels the
        // filter (#1698), so every labelled copy is as large as the whole selection.
        let mut selected = if has_children {
            sources.direct_tests_everywhere(module)
        } else {
            sources.tests_selected_everywhere(module)
        };
        // A substring that also names tests outside the module would run them too;
        // anchor the filter to the module's own tree (#1653).
        let foreign = if has_children {
            Vec::new()
        } else {
            sources.foreign_tests(module)
        };
        if !foreign.is_empty() && !shards.is_empty() {
            scope = filters::anchored_filterset(module);
            selected = sources.subtree_tests_everywhere(module);
            self.narrowed.push(json!({
                "path": path, "module": module, "filter": scope, "tests": selected,
                "reason": format!(
                    "`test({module})` is a substring match and also selects {} tests outside the module (e.g. {}); anchored to the module's own tree",
                    foreign.len(), foreign.iter().take(3).cloned().collect::<Vec<_>>().join(", ")),
            }));
        }
        let over: BTreeMap<String, usize> = if selected > BROAD_TEST_LIMIT {
            shards.iter().map(|s| (s.clone(), selected)).collect()
        } else {
            BTreeMap::new()
        };
        let mut diagnostics: Vec<String> = Vec::new();
        let mut direct = Direct::default();
        let mut declaration = false;
        let in_diff = self.changed.contains(path);
        if in_diff {
            let hunks = self.hunks(path);
            let old_lines = selection::changed_lines(hunks, true);
            let new_lines = selection::changed_lines(hunks, false);
            for (tree, lines) in [(&loaded.old, &old_lines), (&loaded.sources, &new_lines)] {
                let text = tree.texts.get(path).map_or("", String::as_str);
                let rows: Vec<_> = text.lines().collect();
                let functions = source::rust_functions(text, &tree.host);
                let enums: BTreeSet<String> = ENUM_NAME.findall1(text).into_iter().collect();
                let owners = seeds::type_owners(text, &enums)?;
                for &line in lines {
                    let owner = owners.get(line.wrapping_sub(1)).cloned().flatten();
                    if owner.is_some_and(|o| enums.contains(&o)) {
                        declaration = true;
                    }
                    let in_function = functions
                        .iter()
                        .any(|f| f.attr_start < line && line <= f.end);
                    if rows
                        .get(line.wrapping_sub(1))
                        .is_some_and(|r| FIELD_ROW.is_match(r))
                        && !in_function
                    {
                        declaration = true;
                    }
                }
            }
        }
        if (!over.is_empty() || declaration || path == RPC) && in_diff {
            let result = self.narrow(path)?;
            diagnostics = result.diagnostics.clone();
            direct = result.direct.clone();
            if result.picked.is_none() {
                for (observer, hops) in &direct.tests {
                    let slot = self.direct_found.entry(observer.clone()).or_insert(*hops);
                    *slot = (*slot).min(*hops);
                }
            }
            if let Some(picked) = result.picked {
                self.narrowed.push(json!({
                    "path": path, "module": module, "tests": tests_of(&picked),
                    "reason": format!("`{scope}` selects {selected} tests; {}", if picked.is_empty() {
                        "no test mentions the change, so only the compile check observes it"
                    } else {
                        "narrowed to the tests that mention the change"
                    }),
                }));
                return Ok(filters::traced_filters(&picked));
            }
        }
        if has_children && !shards.is_empty() {
            self.narrowed.push(json!({
                "path": path, "module": module, "filter": scope, "excludes": children,
                "reason": "a test module that mounts test submodules selects its own tests only; add test(<submodule>) after editing a helper they share",
            }));
        }
        for (shard, tests) in &over {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "filter".into(),
                json!(format!("rsid=shard:{shard}:{scope}")),
            );
            entry.insert("tests".into(), json!(tests));
            entry.insert("path".into(), json!(path));
            entry.insert(
                "reason".into(),
                json!(if diagnostics.is_empty() {
                    "diff cannot be traced to the tests that observe it".to_owned()
                } else {
                    diagnostics.join("; ")
                }),
            );
            entry.insert("stop".into(), json!(direct.reason));
            entry.insert(
                "worker_bounded".into(),
                json!(
                    direct
                        .reason
                        .as_deref()
                        .is_some_and(|r| WORKER_BOUNDED_STOPS.contains(&r))
                ),
            );
            for (key, value) in direct.summary() {
                entry.insert(key.into(), value);
            }
            self.broad.push(Value::Object(entry));
        }
        Ok(shards
            .iter()
            .map(|s| format!("rsid=shard:{s}:{scope}"))
            .collect())
    }

    /// Landing filters of the daemon tests that observe a production edit of leaf
    /// crate `package` (#2410). The leaf cannot name its callers: the tests of the
    /// retained facade module always run, and the sharded tests that name the
    /// changed identifiers join them when the trace is complete.
    fn leaf_observers(&mut self, path: &str, package: &str) -> Result<Vec<String>, Failure> {
        let loaded = self.loaded;
        let sources = &loaded.sources;
        let hunks = self.hunks(path);
        let before = loaded.old.texts.get(path).map_or("", String::as_str);
        let after = sources.texts.get(path).map_or("", String::as_str);
        if slow::only_test_lines(
            before,
            &selection::changed_lines(hunks, true),
            &loaded.old.host,
        ) && slow::only_test_lines(
            after,
            &selection::changed_lines(hunks, false),
            &sources.host,
        ) {
            self.leaf_notes
                .push(json!({"path": path, "package": package, "filters": [],
                "reason": "only the leaf's test code changed: its own tests observe it"}));
            return Ok(Vec::new());
        }
        let facades: &[&str] = LEAF_FACADES
            .iter()
            .find(|(p, _)| *p == package)
            .map_or(&[], |(_, f)| f);
        let mut facade_filters = Vec::new();
        for facade in facades {
            let directory = format!("{}{}", source::RSID_SRC, facade.replace("::", "/"));
            let facade_path = [format!("{directory}/mod.rs"), format!("{directory}.rs")]
                .into_iter()
                .find(|p| sources.texts.contains_key(p));
            let shards: Vec<String> = match &facade_path {
                Some(p) => sources.shards_for(p)?.into_iter().collect(),
                None => Vec::new(),
            };
            if let (Some(p), false) = (&facade_path, shards.is_empty()) {
                facade_filters.extend(self.shard_filters(p, facade, &shards)?);
            }
        }
        let fallback_only = LEAF_FACADES_FALLBACK_ONLY.contains(&package);
        let mut filters = if fallback_only {
            Vec::new()
        } else {
            facade_filters
        };
        let facade_text = facades
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(" and ");
        let result = self.narrow(path)?;
        let Some(picked) = result.picked else {
            self.unprovable.push(path.to_owned());
            let why = if result.diagnostics.is_empty() {
                "unresolved".to_owned()
            } else {
                result.diagnostics.join("; ")
            };
            self.leaf_notes.push(json!({
                "path": path, "package": package, "filters": [], "unfiltered": true,
                "reason": format!("production edit: the trace to the daemon tests is not provable ({why}), so no filter is derived and the lander's complete closure over the leaf and rsid sources selects the observers"),
            }));
            return Ok(Vec::new());
        };
        if !picked.is_empty() {
            filters.extend(filters::traced_filters(&picked));
        }
        let mut seen = BTreeSet::new();
        let unique: Vec<_> = filters
            .iter()
            .filter(|f| seen.insert((*f).clone()))
            .cloned()
            .collect();
        self.leaf_notes.push(json!({
            "path": path, "package": package, "filters": unique,
            "reason": format!("{}{}",
                if fallback_only { "production edit: ".to_owned() } else { format!("production edit: the {facade_text} facade tests plus ") },
                if picked.is_empty() { "no other sharded test named the change".to_owned() }
                else { format!("{} sharded tests that name the change", tests_of(&picked)) }),
        }));
        Ok(filters)
    }
}

fn dynamic_sql(added: &BTreeMap<String, Vec<(usize, String)>>) -> Vec<String> {
    let mut findings = Vec::new();
    for (path, lines) in added {
        for (index, (number, text)) in lines.iter().enumerate() {
            if !text.contains("format!") {
                continue;
            }
            let window = &lines[index..(index + 6).min(lines.len())];
            let joined = window
                .iter()
                .map(|i| i.1.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if SQL_WORDS.is_match(&joined) && !window.iter().any(|i| i.1.contains("sql-dynamic-ok"))
            {
                findings.push(format!("{path}:{number}"));
            }
        }
    }
    findings
}

pub fn verdict_value(ok: bool, detail: &str) -> Value {
    json!({"verdict": if ok { "ok" } else { "fail" }, "detail": detail})
}

pub fn compile_receipt(mode: Option<&str>, shard_runs: &BTreeMap<String, Value>) -> Value {
    let executed = mode == Some("compile");
    let mut receipt = json!({
        "mode": mode,
        "status": if executed { "executed" } else { "skipped" },
        "cargo_check_invoked": !shard_runs.is_empty(),
        "shards_checked": shard_runs.keys().collect::<Vec<_>>(),
    });
    if !executed {
        receipt["detail"] =
            json!("planning only: compile validation belongs to the supervised scoped_test job");
    }
    receipt
}

/// The `desktop` test lane selection (hub #3001, #3042): selected exactly when a
/// changed path is a lane input. Fails closed (#355) when the selector is missing
/// and the diff touches it or one of the lane's manifest-derived inputs.
fn desktop_lane(loaded: &Loaded, changed: &[String]) -> Value {
    let read = |path: &str| loaded.head_blob(path);
    let (inputs, error) = match desktop_selection::lane_selected_with(&read, changed) {
        Ok(_) => (desktop_selection::trigger_inputs(&read), None),
        Err(message) => (desktop_selection::static_inputs(), Some(message)),
    };
    let paths: Vec<_> = changed
        .iter()
        .filter(|p| desktop_selection::under(p, &inputs))
        .cloned()
        .collect();
    let mut lane = json!({
        "selected": error.is_some() || !paths.is_empty(),
        "total": paths.len(),
        "paths": paths.iter().take(DESKTOP_PATH_LIMIT).collect::<Vec<_>>(),
    });
    if let Some(message) = error {
        lane["error"] = json!(message);
    }
    lane
}

pub fn derive(loaded: &Loaded, opts: &Options) -> Result<Derived, Failure> {
    let started = Instant::now();
    let mut timings: Vec<(&str, f64)> = Vec::new();
    let mut spent = 0.0;
    let ctx = &loaded.ctx;
    let sources = &loaded.sources;
    let old = &loaded.old;
    let changed: Vec<String> = ctx.changed.clone();
    let changed_set: BTreeSet<String> = changed.iter().cloned().collect();
    let added_files: BTreeSet<String> = ctx.added.iter().cloned().collect();
    let slow_tier = slow::decision(&loaded.repo, &ctx.merge_base, &ctx.head, &changed);
    let mut stderr = Vec::new();
    let quiet = match &opts.env {
        Some(vars) => vars.iter().any(|(k, _)| k == "CHECK_TOUCHED_SHARDS_QUIET"),
        None => std::env::var_os("CHECK_TOUCHED_SHARDS_QUIET").is_some(),
    };
    // A derivation that takes seconds needs no log; the full check does (#1657).
    let progress_on = !opts.filters_only && !quiet;
    macro_rules! lap {
        ($name:expr) => {{
            let total = started.elapsed().as_secs_f64();
            let each = ((total - spent).max(0.0) * 100.0).round() / 100.0;
            spent += each;
            timings.push(($name, each));
            if progress_on {
                stderr.push(format!(
                    "check-touched-shards: [{total:6.1}s] {} done in {each:?}s",
                    $name
                ));
            }
        }};
    }
    if ctx.merge_bases.len() > 1 {
        stderr.push(format!(
            "check-touched-shards: {} merge bases ({}): diffing against the base tip {}, kept to files a merge base also sees changed (#1633)",
            ctx.merge_bases.len(),
            ctx.merge_bases.iter().map(|b| b[..9.min(b.len())].to_owned()).collect::<Vec<_>>().join(", "),
            &ctx.base[..9.min(ctx.base.len())],
        ));
    }
    let mut run = Run {
        loaded,
        changed: changed_set.clone(),
        index: None,
        narrowed: Vec::new(),
        broad: Vec::new(),
        direct_found: BTreeMap::new(),
        leaf_notes: Vec::new(),
        unprovable: Vec::new(),
        stderr: Vec::new(),
    };

    // Shard mapping: registration-only edits narrow to the registered modules.
    let mut affected: Vec<String> = changed.clone();
    if changed.iter().any(|p| lib_source(p)) {
        let mut seen = BTreeSet::new();
        affected = Vec::new();
        for path in &changed {
            for target in selection::registration_sources(path, sources, old, run.hunks(path)) {
                if seen.insert(target.clone()) {
                    affected.push(target);
                }
            }
        }
    }
    lap!("diff_and_sources");
    let packages_of_shard = shard_package_map(&loaded.shards);
    let guard_table = loaded.head_blob(guards::GUARD_DECLARATIONS);
    let (guard_filters, guard_observers) = match guards::declared_guard_filters(
        sources,
        guard_table.as_deref(),
        &changed_set,
        &loaded.shards,
    ) {
        Ok(found) => found,
        Err(message) => {
            return Err(Failure::Refusal(format!(
                "{}: {message}",
                guards::GUARD_DECLARATIONS
            )));
        }
    };
    let mapping = super::map_shards(sources, &affected, &loaded.shards)?;
    let shard_files = mapping.map.clone();
    let fallback = mapping.fallback.clone();
    lap!("shard_mapping");

    let mode = if opts.filters_only {
        Some("filters_only")
    } else if opts.no_compile {
        Some("no_compile")
    } else if opts.compile {
        Some("compile")
    } else {
        None
    };
    // Compile: an explicit verifier mode only; planning never reaches cargo.
    let process_ctx = match &opts.env {
        Some(vars) => {
            let pairs: Vec<(&str, &str)> =
                vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            Ctx::new(&loaded.repo.root, &pairs)
        }
        None => Ctx::from_process(),
    };
    let mut compiled = modes::CompileOutcome::default();
    if opts.compile {
        let (flag, signal) = match &opts.interrupt {
            Some(flag) => (
                flag.clone(),
                std::sync::Arc::new(std::sync::atomic::AtomicI32::new(15)),
            ),
            None => modes::termination_flag(),
        };
        let shard_list: Vec<String> = shard_files.keys().cloned().collect();
        let partial = |phase: &str,
                       outcome: &modes::CompileOutcome,
                       timings: &[(&str, f64)]|
         -> Value {
            json!({
                "partial": true, "phase": phase, "ok": false, "base": ctx.base, "head": ctx.head,
                "merge_base": ctx.merge_base, "changed_files": changed.len(),
                "compile": compile_receipt(mode, &outcome.runs),
                "shards": {
                    "compiled_ok": outcome.ok, "compiled_failed": outcome.failed,
                    "compile_pending": shard_files.keys().filter(|s| !outcome.runs.contains_key(*s)).collect::<Vec<_>>(),
                    "compile_skipped": false, "runs": outcome.runs, "map": shard_files,
                },
                "timings_sec": Value::Object(timings.iter().map(|(k, v)| ((*k).to_owned(), json!(v))).collect()),
            })
        };
        let snapshot = timings.clone();
        let mut checkpoint = |phase: &str, outcome: &modes::CompileOutcome| {
            if let Some(path) = &opts.receipt {
                let _ = modes::write_receipt(path, &partial(phase, outcome, &snapshot));
            }
        };
        compiled = modes::compile_shards(
            loaded,
            &process_ctx,
            &shard_list,
            &packages_of_shard,
            &flag,
            signal.load(std::sync::atomic::Ordering::SeqCst),
            started,
            &mut checkpoint,
        );
        for line in &compiled.log {
            match line {
                modes::Line::Progress(at, message) => {
                    if progress_on {
                        stderr.push(format!("check-touched-shards: [{at:6.1}s] {message}"));
                    }
                }
                modes::Line::Raw(text) => stderr.push(text.clone()),
            }
        }
        if let Some(signum) = compiled.interrupted {
            let partial_receipt = partial("interrupted", &compiled, &timings);
            let pending: Vec<_> = shard_files
                .keys()
                .filter(|s| !compiled.runs.contains_key(*s))
                .cloned()
                .collect();
            let list = |items: &[String]| {
                if items.is_empty() {
                    "-".to_owned()
                } else {
                    items.join(",")
                }
            };
            let message = format!(
                "check-touched-shards: [{:6.1}s] interrupted by signal {signum}; shards ok so far: {}; pending: {}",
                started.elapsed().as_secs_f64(),
                list(&compiled.ok),
                list(&pending)
            );
            return Err(Failure::Interrupted {
                code: 128 + signum,
                partial: partial_receipt,
                message,
            });
        }
    }
    lap!("compile");

    // Audits that need no execution.
    let mut audits = serde_json::Map::new();
    let heavy_skipped = json!({"verdict": "skipped", "detail": "--filters-only"});
    if opts.filters_only {
        audits.insert("shard_gates".into(), heavy_skipped.clone());
    } else {
        audits.insert("shard_gates".into(), super::modes::shard_gate_audit(loaded));
    }
    audits.insert(
        "diff_check".into(),
        super::modes::diff_check(loaded, &changed, ctx.merge_bases.len() > 1),
    );
    if opts.filters_only {
        audits.insert("released_migrations".into(), heavy_skipped.clone());
    } else {
        audits.insert(
            "released_migrations".into(),
            super::modes::released_migrations_audit(loaded),
        );
    }
    let added: BTreeMap<String, Vec<(usize, String)>> = loaded
        .added_lines
        .iter()
        .filter(|(p, _)| changed_set.contains(*p))
        .map(|(p, l)| (p.clone(), l.clone()))
        .collect();
    let sql = dynamic_sql(&added);
    audits.insert(
        "dynamic_sql".into(),
        json!({
            "verdict": if sql.is_empty() { "ok" } else { "flag" },
            "detail": sql.iter().take(10).cloned().collect::<Vec<_>>().join(", "),
        }),
    );
    // #1182: a fixture on $TMPDIR passes with a disk-backed TMPDIR and fails the
    // lander's tmpfs gate when it allocates a sandbox. Advisory.
    let mut tmpfs = Vec::new();
    for (path, lines) in &added {
        if path.starts_with("crates/rsid/src/session/") {
            for (number, text) in lines {
                if TMPFS_FIXTURE.is_match(text) && !text.contains("tmpfs-fixture-ok") {
                    tmpfs.push(format!("{path}:{number}"));
                }
            }
        }
    }
    audits.insert(
        "tmpfs_fixture".into(),
        json!({
            "verdict": if tmpfs.is_empty() { "ok" } else { "flag" },
            "detail": tmpfs.iter().take(10).cloned().collect::<Vec<_>>().join(", "),
        }),
    );
    lap!("audits");

    // Migrations: numbering and layout.
    let mut declarations = Vec::new();
    for name in added_files
        .iter()
        .filter(|n| n.starts_with("tools/provisional-migrations/") && n.ends_with(".json"))
    {
        if let Some(text) = loaded.head_blob(name) {
            declarations.push(text);
        }
    }
    let migrations = migration::migration_report(
        &loaded.base_files,
        &loaded.tree_files.iter().cloned().collect::<Vec<_>>(),
        &added_files,
        &declarations,
    );
    lap!("migrations");

    // Suggested filters. Only filters that match at least one test: a static check
    // that the file (or its child modules) holds a test (a 40-minute landing gate
    // was refused for a filter that matched nothing, #1129).
    let mut suggested: Vec<String> = Vec::new();
    let mut rewind_filters: Option<Vec<String>> = None;
    let mut declared_used: Vec<Value> = Vec::new();
    let mut derived_by_path: Vec<(String, Vec<String>)> = Vec::new();
    let mut comment_only: BTreeSet<String> = BTreeSet::new();
    for path in changed.iter().filter(|p| p.ends_with(".rs")) {
        let before = old.texts.get(path).map_or("", String::as_str);
        if tree::rust_comment_only_change(
            before,
            sources.texts.get(path).map_or("", String::as_str),
        ) {
            comment_only.insert(path.clone());
        }
    }
    let comment_scanners = if comment_only.is_empty() {
        Vec::new()
    } else {
        let index = run.index()?;
        readers::source_scanner_tests(sources, index, &comment_only)?
    };
    suggested.extend(filters::traced_filters(&comment_scanners));

    if affected.iter().any(|p| p == migration::REWIND_FIXTURE) {
        let before = old
            .texts
            .get(migration::REWIND_FIXTURE)
            .map_or("", String::as_str);
        let after = sources
            .texts
            .get(migration::REWIND_FIXTURE)
            .map_or("", String::as_str);
        if migration::rewind_only_change(before, after) {
            let units: Vec<_> = added_files
                .iter()
                .filter(|p| {
                    p.starts_with(migration::MIGRATIONS) && migration::migration_file(p).is_some()
                })
                .collect();
            let additive = !units.is_empty()
                && units.iter().all(|p| {
                    migration::additive_migration(sources.texts.get(*p).map_or("", String::as_str))
                });
            let pins = if additive {
                migration::rewind_test_filters(sources, true, false)
            } else {
                Vec::new()
            };
            let all = if pins.is_empty() {
                migration::rewind_test_filters(sources, false, false)
            } else {
                pins
            };
            if !all.is_empty() {
                let reason = if additive {
                    "proven additive/isolated migration selects only migration-chain pins"
                } else {
                    "rewind helper change has no proven additive/isolated migration; retained every caller"
                };
                run.narrowed.push(json!({
                    "path": migration::REWIND_FIXTURE, "module": sources.module(migration::REWIND_FIXTURE),
                    "tests": all.len(), "reason": reason,
                }));
                if !additive && all.len() > BROAD_TEST_LIMIT {
                    run.broad.push(json!({"path": migration::REWIND_FIXTURE, "filter": "migration rewind callers",
                        "tests": all.len(), "reason": reason}));
                }
                rewind_filters = Some(all);
            }
        }
    }
    for path in &affected {
        if comment_only.contains(path) {
            run.narrowed.push(json!({
                "path": path, "module": sources.module(path),
                "tests": comment_scanners.iter().map(|o| (&o.module, &o.name)).collect::<BTreeSet<_>>().len(),
                "reason": "only Rust comments/docs changed; compile check and source readers observe the change",
            }));
            continue;
        }
        if path == migration::REWIND_FIXTURE {
            if let Some(filters) = &rewind_filters {
                suggested.extend(filters.iter().cloned());
                continue;
            }
        }
        // Targets mounting the metadata still observe it, independently of the
        // library replay closure below.
        for filter in sources.target_filters(path)? {
            push_unique(&mut suggested, filter);
        }
        if path == &format!("{}altered_objects.rs", migration::MIGRATIONS)
            || path == &format!("{}altered_objects.rs", migration::PRE_SPLIT_MIGRATIONS)
        {
            let index = run.index()?;
            let metadata = migration::migration_metadata_tests(sources, index, path);
            let mut combined = filters::traced_filters(&metadata);
            combined.extend(migration::rewind_test_filters(sources, false, false));
            let found = filters::covered_filters(&combined, &mut Vec::new());
            if !found.is_empty() {
                derived_by_path.push((path.clone(), found.clone()));
                suggested.extend(found);
                run.narrowed.push(json!({"path": path, "module": sources.module(path),
                    "reason": "migration catalog metadata selects its consumers and all required replay callers"}));
                continue;
            }
        }
        // #1937, #1650: a bin or integration target that compiles this source runs its own tests.
        if TARGET_FILE.is_match(path)
            || (!sources.target_owners(path).is_empty() && !sources.lib_mounted(path))
        {
            continue;
        }
        if let Some(leaf) = source::leaf_package(path) {
            if changed_set.contains(path) {
                for filter in run.leaf_observers(path, &leaf)? {
                    push_unique(&mut suggested, filter);
                }
            }
        }
        let module = sources.module(path);
        if module.is_none() {
            if let Some(root) = tree::crate_root_test_filter(sources, path)? {
                // #2331: a library root has no module path, but its own inline
                // `mod tests` still observes the diff.
                suggested.push(root);
                continue;
            }
        }
        let sharded = path.starts_with(source::RSID_SRC) || path.starts_with(source::STORE_SRC);
        let Some(module) = module else { continue };
        if sharded && !lib_source(path) {
            continue;
        }
        if sharded {
            let shards: Vec<String> = shard_files
                .iter()
                .filter(|(s, files)| {
                    files.contains(path) && Some(s.as_str()) != fallback.as_deref()
                })
                .map(|(s, _)| s.clone())
                .collect();
            let declared = if changed_set.contains(path) {
                let coverage = loaded.head_blob(guards::COVERAGE_DECLARATIONS);
                let old_text = loaded
                    .repo
                    .blobs(&ctx.merge_base, &[path.clone()])
                    .ok()
                    .and_then(|mut b| b.remove(path));
                let index = run.index()?;
                guards::declared_coverage(
                    coverage.as_deref(),
                    old_text.as_deref(),
                    sources.texts.get(path).map_or("", String::as_str),
                    loaded.hunks.get(path).map_or(&[], Vec::as_slice),
                    path,
                    index,
                    &sources.host,
                )
            } else {
                None
            };
            let produced = if let Some((filters, rule)) = &declared {
                declared_used.push(json!({"path": path, "filters": filters,
                    "reason": rule.get("reason").and_then(Value::as_str).unwrap_or("")}));
                filters.clone()
            } else {
                run.shard_filters(path, &module, &shards)?
            };
            derived_by_path.push((path.clone(), produced.clone()));
            suggested.extend(produced.iter().cloned());
            // A proven observer trace already covers the edit. Fill an empty selection, never widen that proof.
            if changed_set.contains(path) && declared.is_none() && produced.is_empty() {
                suggested.extend(guards::sibling_behaviour_filters(sources, path)?);
            }
        } else if sources.has_tests(path)? {
            // The owning package: `rsid-core=process_control`, not `rsi=process_control`.
            if let Some(package) = package_for(path) {
                suggested.push(format!("{package}={module}"));
            }
        }
    }

    // A bin, integration test or library module that reads a changed file
    // (include_str!/include_bytes!, a command/skill/prompt asset, a package fixture
    // named by a path literal) observes it although it is not in the diff (#1769, #1926).
    let mut changed_files: BTreeSet<String> = changed_set.clone();
    changed_files.extend(affected.iter().cloned());
    let data_changed: BTreeSet<String> = changed_set
        .iter()
        .filter(|p| !p.ends_with(".rs"))
        .cloned()
        .collect();
    let mut readers_found = data::include_readers(sources, &changed_files)?;
    readers_found.extend(data::asset_reader_targets(sources, &changed_set));
    readers_found.extend(data::data_readers(sources, &data_changed));
    let mut seen_readers = BTreeSet::new();
    for path in readers_found {
        if !seen_readers.insert(path.clone()) {
            continue;
        }
        for filter in data::reader_filters(sources, &path)? {
            push_unique(&mut suggested, filter);
        }
    }
    // #2057: a data file under src/ is observed through the item that includes it.
    let included = {
        let index = run.index()?;
        readers::include_item_tests(sources, index, &changed_set).unwrap_or_default()
    };
    for filter in filters::traced_filters(&included) {
        push_unique(&mut suggested, filter);
    }
    // #2034: a source scanner reads production files it does not name.
    let scanner_tests = {
        let index = run.index()?;
        data::source_reader_tests(sources, index, &changed_set, &comment_only)
    };
    let registry = loaded.head_blob(data::SCANNED_TOKENS);
    let hunk_map: BTreeMap<String, Vec<Hunk>> = loaded.hunks.clone();
    let token_scanners = data::scanned_token_tests(
        sources,
        old,
        registry.as_deref(),
        &changed_set,
        &hunk_map,
        &comment_only,
    )
    .map_err(Failure::Refusal)?;
    let scanners: BTreeSet<Observer> = scanner_tests
        .iter()
        .chain(token_scanners.iter())
        .cloned()
        .collect();
    let scanners: Vec<Observer> = scanners.into_iter().collect();
    for filter in filters::traced_filters(&scanners) {
        push_unique(&mut suggested, filter);
    }
    for filter in data::source_reader_targets(sources, &changed_set)? {
        push_unique(&mut suggested, filter);
    }
    // #2321: a changed TUI surface runs the e2e_tui modules that drive it, and only those.
    let read_head = |path: &str| loaded.head_blob(path);
    let e2e = e2e::e2e_surface_filters(&read_head, &changed_set, &comment_only, &sources.host)?;
    for filter in &e2e.filters {
        let target_part = filter.split_once('=').map_or("", |(_, rest)| rest);
        let whole_target = !target_part
            .strip_prefix("test:")
            .unwrap_or("")
            .contains(':');
        if whole_target {
            suggested.retain(|f| !f.starts_with(&format!("{filter}:")));
        } else {
            let wanted = format!("test:{}", filter.split(':').nth(1).unwrap_or(""));
            if suggested.iter().any(|f| {
                f.contains("=test:") && f.split_once('=').map(|x| x.1) == Some(wanted.as_str())
            }) {
                continue;
            }
        }
        push_unique(&mut suggested, filter.clone());
    }
    // Public library item consumers share the worker and queue derivation (#3408).
    let downstream_tests =
        downstream_tests::downstream_test_plan(&loaded.repo, &ctx.merge_base, &ctx.head, sources)?;
    if let Some(list) = downstream_tests["filters"].as_array() {
        suggested.extend(list.iter().filter_map(|v| v.as_str().map(str::to_owned)));
    }
    // #2029: a new or edited migration file changes what every replay/rewind/catalog
    // pin observes, although no test names the file.
    if changed.iter().any(|p| {
        p.starts_with(migration::MIGRATIONS)
            && migration::migration_file(p).is_some()
            && !comment_only.contains(p)
    }) {
        for filter in migration::rewind_test_filters(sources, false, true) {
            push_unique(&mut suggested, filter);
        }
    }
    let mut pins: Vec<String> = Vec::new();
    let added_text: String = added
        .iter()
        .filter(|(p, _)| !comment_only.contains(*p))
        .flat_map(|(_, lines)| lines.iter().map(|l| l.1.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    let mut broad_extra = Vec::new();
    if VERB_MENTION.is_match(&added_text)
        || changed.iter().any(|p| {
            !comment_only.contains(p)
                && (p.starts_with("crates/rsid/src/rpc/agent")
                    || p.starts_with("crates/rsid/src/harness/tools"))
        })
    {
        let named = guards::verb_pin_filters(run.index()?);
        let picked: Vec<Observer> = named
            .iter()
            .flat_map(|((shard, module), names)| {
                names.iter().map(|name| Observer {
                    module: module.clone(),
                    name: name.clone(),
                    shard: shard.clone(),
                })
            })
            .collect();
        pins.extend(filters::traced_filters(&picked));
        let total: usize = named.values().map(BTreeSet::len).sum();
        if total > guards::AGENT_VERB_PIN_CAP {
            broad_extra.push(json!({"filter": "agent verb catalog pins", "tests": total,
                "reason": format!("more than {} catalog/operator_only tests", guards::AGENT_VERB_PIN_CAP)}));
        }
    }
    run.broad.extend(broad_extra);
    let (observer_tests, observer_missing) =
        guards::source_observer_pins(run.index()?, &changed_set, &comment_only);
    pins.extend(filters::traced_filters(&observer_tests));
    pins.extend(guard_filters.iter().cloned());
    let workspace_observers = guards::manifest_dir_guard_filters(&loaded.tree_files, &changed_set);
    pins.extend(workspace_observers.iter().cloned());
    let manual_docs_changed = changed_set.contains("docs/keybindings.md");
    if changed.iter().any(|p| {
        let name = p.rsplit('/').next().unwrap_or(p);
        name == "settings_registry.rs" || name == "key_tables.rs"
    }) {
        pins.push("rsi=manual::".into());
    }
    let derived_filters: BTreeSet<&String> =
        derived_by_path.iter().flat_map(|(_, f)| f.iter()).collect();
    let other_filters: Vec<String> = suggested
        .iter()
        .filter(|f| !derived_filters.contains(f))
        .cloned()
        .chain(pins.iter().cloned())
        .collect();
    let mut combined = suggested.clone();
    combined.extend(pins.iter().cloned());
    suggested = filters::covered_filters(&combined, &mut Vec::new());
    // Small per-module inventories can still exceed the worker's gate budget in
    // aggregate. Retry the observer trace, accepting only reductions after merging
    // overlapping coverage. An uncertain trace retains its original filters.
    if suggested.len() > GATE_FILTER_LIMIT {
        let originals: Vec<(String, Vec<String>)> = derived_by_path.clone();
        for (path, original) in originals {
            if !changed_set.contains(&path) || original.is_empty() {
                continue;
            }
            let result = run.narrow(&path)?;
            let Some(picked) = result.picked else {
                continue;
            };
            let replacement = filters::traced_filters(&picked);
            let mut candidate_input = other_filters.clone();
            for (source_path, filters_of) in &derived_by_path {
                if source_path == &path {
                    candidate_input.extend(replacement.iter().cloned());
                } else {
                    candidate_input.extend(filters_of.iter().cloned());
                }
            }
            let candidate = filters::covered_filters(&candidate_input, &mut Vec::new());
            if candidate.len() >= suggested.len() {
                continue;
            }
            if let Some(slot) = derived_by_path.iter_mut().find(|(p, _)| p == &path) {
                slot.1 = replacement;
            }
            suggested = candidate;
            run.narrowed.push(json!({
                "path": path, "module": sources.module(&path), "tests": tests_of(&picked),
                "reason": "aggregate gate budget: traced field, export and function consumers; merged already-covered tests",
            }));
            if suggested.len() <= GATE_FILTER_LIMIT {
                break;
            }
        }
    }
    let mut merged: Vec<MergedFilter> = Vec::new();
    let mut final_input = other_filters.clone();
    final_input.extend(derived_by_path.iter().flat_map(|(_, f)| f.iter().cloned()));
    suggested = filters::covered_filters(&final_input, &mut merged);
    // #1260/#1253: a filter that selects no test fails the gate after a long build.
    // Keep a shard filter only when a test of that shard sits under the module.
    let mut dropped: Vec<Value> = Vec::new();
    let mut kept = Vec::new();
    for flt in suggested {
        let simple = SHARD_FILTER.search(&flt).and_then(|m| {
            let expression = group(&m, 2).to_owned();
            let name = SIMPLE_MODULE
                .search(&expression)
                .or_else(|| ANCHORED_MODULE.search(&expression))
                .map(|s| group(&s, 1).to_owned())?;
            Some((group(&m, 1).to_owned(), name))
        });
        if let Some((shard, module)) = simple {
            if !sources.filter_matches(&shard, &module) {
                dropped.push(json!({"filter": flt, "reason": format!("no test of shard {shard} has `{module}` in its module path")}));
                continue;
            }
        }
        kept.push(flt);
    }
    let mut suggested = kept;
    let mut broad = run.broad.clone();
    if !opts.run_filters {
        suggested = filters::unsharded_filters(&suggested, &packages_of_shard, &mut merged);
        let duplicates: BTreeSet<&String> = merged
            .iter()
            .filter(|m| m.reason.starts_with("identical expression"))
            .map(|m| &m.filter)
            .collect();
        broad.retain(|item| {
            item["filter"]
                .as_str()
                .is_none_or(|f| !duplicates.iter().any(|d| d.as_str() == f))
        });
    }
    for item in &dropped {
        stderr.push(format!(
            "check-touched-shards: dropped {}: {}",
            item["filter"].as_str().unwrap_or(""),
            item["reason"].as_str().unwrap_or("")
        ));
    }
    // A worker still verifies the known changed modules and targets when an
    // unresolved leaf trace leaves the lander to derive its complete closure.
    let worker_filters = suggested.clone();
    if !run.unprovable.is_empty() {
        // #2441: no filters at all means the unfiltered gate; a partial list would replace it.
        suggested = Vec::new();
    }
    let oversize: Vec<String> = suggested
        .iter()
        .filter(|f| f.len() > GATE_FILTER_LENGTH)
        .cloned()
        .collect();
    for flt in &oversize {
        stderr.push(format!(
            "check-touched-shards: {}-byte filter exceeds the {GATE_FILTER_LENGTH}-byte job wire cap and cannot be split further: {}...; enqueue with derive_filters instead of copying it",
            flt.len(), flt.chars().take(120).collect::<String>()));
    }
    for item in &broad {
        stderr.push(format!(
            "check-touched-shards: BROAD {} ({} tests): {}; split the run or run it with a longer budget",
            item["filter"].as_str().unwrap_or(""), item["tests"], item["reason"].as_str().unwrap_or("")));
    }
    lap!("filters");
    let impact = if opts.filters_only {
        None
    } else {
        modes::impact_filters(loaded, &process_ctx)
    };
    lap!("impact");
    let _ = spent;

    let desktop = desktop_lane(loaded, &changed);
    if let Some(error) = desktop.get("error").and_then(Value::as_str) {
        stderr.push(format!("check-touched-shards: desktop lane: {error}"));
    }
    let compiled_failed: Vec<String> = compiled.failed.clone();
    let mut filter_runs: (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    if opts.run_filters {
        let mut all = suggested.clone();
        all.extend(opts.extra_filters.iter().cloned());
        filter_runs =
            modes::run_filters(loaded, &process_ctx, &all, &packages_of_shard, &mut stderr);
    }
    let any_audit_failed = audits.values().any(|a| a["verdict"] == "fail");
    let failed = desktop.get("error").is_some()
        || !filter_runs.1.is_empty()
        || ctx.merge_clean == Some(false)
        || migrations.numbering_conflict
        || !migrations.stray_pre_split.is_empty()
        || any_audit_failed
        || !compiled_failed.is_empty();
    let priority: Vec<Observer> = changed
        .iter()
        .filter(|p| lib_source(p))
        .flat_map(|p| selection::changed_test_observers(sources, p, run.hunks(p)))
        .collect();
    let priority_filters = filters::unsharded_filters(
        &filters::traced_filters(&priority),
        &packages_of_shard,
        &mut Vec::new(),
    );
    let direct_found = run.direct_found.clone();
    let shard_runs: BTreeMap<String, Value> = compiled.runs.clone();
    let mut receipt = serde_json::Map::new();
    let mut put = |key: &str, value: Value| {
        receipt.insert(key.to_owned(), value);
    };
    put("base", json!(ctx.base));
    put("head", json!(ctx.head));
    put("merge_base", json!(ctx.merge_base));
    put("merge_bases", json!(ctx.merge_bases));
    put("merge_clean", json!(ctx.merge_clean));
    put("changed_files", json!(changed.len()));
    put("slow_tier", slow_tier);
    put("compile", compile_receipt(mode, &shard_runs));
    put(
        "shards",
        json!({
            "compiled_ok": compiled.ok, "compiled_failed": compiled_failed, "compile_skipped": !opts.compile,
            "shard_compile_suspect_stale": compiled.suspect_stale, "fallback": fallback, "map": shard_files, "runs": shard_runs,
        }),
    );
    put("audits", Value::Object(audits));
    put("desktop", desktop);
    put(
        "filter_runs",
        json!({"ok": filter_runs.0, "failed": filter_runs.1, "skipped": !opts.run_filters}),
    );
    put(
        "migrations",
        json!(MigrationReport {
            new: migrations.new.clone(),
            base_head: migrations.base_head,
            numbering_conflict: migrations.numbering_conflict,
            pending_predecessors: migrations.pending_predecessors.clone(),
            stray_pre_split: migrations.stray_pre_split.clone(),
        }),
    );
    put("suggested_filters", json!(suggested));
    put("worker_filters", json!(worker_filters));
    put("priority_filters", json!(priority_filters));
    put(
        "changed_modules",
        Value::Array(sources.changed_modules(&changed_set, &comment_only)),
    );
    let mut dropped_all = dropped.clone();
    dropped_all.extend(e2e.dropped.iter().cloned());
    put("dropped_filters", Value::Array(dropped_all));
    put("e2e_surfaces", Value::Array(e2e.selected.clone()));
    put("oversize_filters", json!(oversize));
    put("broad", json!(!broad.is_empty()));
    put("broad_filters", Value::Array(broad.clone()));
    put(
        "direct_observer_filters",
        serde_json::to_value(filters::direct_observer_filters(&direct_found))
            .map_err(|e| Failure::Fatal(e.to_string()))?,
    );
    put(
        "direct_observers",
        filters::direct_observer_summary(&direct_found),
    );
    put("narrowed_filters", Value::Array(run.narrowed.clone()));
    let scanner_names: BTreeSet<String> = scanners
        .iter()
        .map(|o| format!("{}::{}", o.module, o.name))
        .collect();
    put("source_scanners", json!(scanner_names));
    put(
        "scanned_token_filters",
        json!(filters::traced_filters(&token_scanners)),
    );
    put("downstream_tests", downstream_tests);
    put("leaf_observers", Value::Array(run.leaf_notes.clone()));
    put("unfiltered_leaf_paths", json!(run.unprovable));
    put("declared_coverage", Value::Array(declared_used));
    put("declared_guards", Value::Array(guard_observers));
    let pins_named: BTreeSet<String> = observer_tests
        .iter()
        .map(|o| format!("{}::{}", o.module, o.name))
        .collect();
    put("source_observer_pins", json!(pins_named));
    put("workspace_source_observers", json!(workspace_observers));
    put(
        "source_observer_missing",
        json!(
            observer_missing
                .iter()
                .map(|(m, n)| format!("{m}::{n}"))
                .collect::<Vec<_>>()
        ),
    );
    put(
        "merged_filters",
        serde_json::to_value(&merged).map_err(|e| Failure::Fatal(e.to_string()))?,
    );
    put(
        "filter_budget",
        json!({"limit": GATE_FILTER_LIMIT, "over": suggested.len() > GATE_FILTER_LIMIT,
        "reason": "uncertain coverage is retained rather than truncated"}),
    );
    put("impact_filters", json!(impact));
    put("manual_docs_changed", json!(manual_docs_changed));
    put("ok", json!(!failed));
    put("filters_only", json!(opts.filters_only));
    put(
        "timings_sec",
        Value::Object(
            timings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), json!(v)))
                .collect(),
        ),
    );
    let audit_text = receipt["audits"]
        .as_object()
        .map(|a| {
            a.iter()
                .map(|(k, v)| format!("{k}={}", v["verdict"].as_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let versions = if migrations.new.is_empty() {
        "-".to_owned()
    } else {
        migrations
            .new
            .iter()
            .map(|n| format!("V{n}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let summary = format!(
        "check-touched-shards {} head={} base={} merge_clean={} shards_ok={} shards_failed={} {} filters_failed={} migrations={}{}{} filters={}{}{}",
        if failed { "FAIL" } else { "OK" },
        &ctx.head[..9.min(ctx.head.len())],
        &ctx.base[..9.min(ctx.base.len())],
        match ctx.merge_clean {
            Some(true) => "true",
            Some(false) => "false",
            None => "none",
        },
        if compiled.ok.is_empty() {
            "-".to_owned()
        } else {
            compiled.ok.join(",")
        },
        if compiled.failed.is_empty() {
            "-".to_owned()
        } else {
            compiled.failed.join(",")
        },
        audit_text,
        if filter_runs.1.is_empty() {
            "-".to_owned()
        } else {
            filter_runs.1.join(",")
        },
        versions,
        if migrations.numbering_conflict {
            " RENUMBER"
        } else {
            ""
        },
        if migrations.stray_pre_split.is_empty() {
            ""
        } else {
            " STRAY_PRE_SPLIT_MIGRATION"
        },
        suggested.len(),
        if broad.is_empty() { "" } else { " BROAD" },
        if ctx.merge_bases.len() > 1 {
            format!(" MERGE_BASES={}", ctx.merge_bases.len())
        } else {
            String::new()
        },
    );
    stderr.extend(run.stderr.clone());
    let mut receipt = Value::Object(receipt);
    receipt["partial"] = json!(false);
    Ok(Derived {
        receipt,
        failed,
        summary,
        shard_map: shard_files,
        stderr,
    })
}
