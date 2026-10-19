//! #628, revised by the operator P0 directive of 2026-09-29 04:20Z: the
//! landing policy is mechanical and runs before any test gate, so a refusal
//! costs seconds instead of a full gate.
//!
//! - No ledger admission: no Work binding, seal, hot-file claim or migration
//!   reservation is required. Fast-forward-only publication serializes
//!   landings; a conflicting tip fails the merge in seconds.
//! - Released migrations stay immutable (`tools/check-released-migrations.py`).
//! - A candidate that raises `LATEST_SCHEMA_VERSION` must append a contiguous
//!   run of migrations starting at exactly the rolling tip's version + 1, each
//!   version carrying its `if version < N` block. The refusal names the number
//!   the source needs.
//! - No ledger lookup: the lander never calls `AgentManagerInspect`, a manager
//!   verb a worker's token is refused (#1927).

use super::git_output;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

const STORE: &str = "crates/rsid-store/src/store/mod.rs";
/// Each schema version is one `vNNN.rs` here; the head is the highest number.
const MIGRATION_DIR: &str = "crates/rsid-store/src/store/migrations";
/// The layout before the #1021 S4 crate split (the store lived in `crates/rsid`).
/// A target or a stale source cut before the split still lands through it.
const PRE_SPLIT_STORE: &str = "crates/rsid/src/store/mod.rs";
const PRE_SPLIT_MIGRATION_DIR: &str = "crates/rsid/src/store/migrations";
const STORES: [&str; 2] = [STORE, PRE_SPLIT_STORE];
const MIGRATION_DIRS: [&str; 2] = [MIGRATION_DIR, PRE_SPLIT_MIGRATION_DIR];
const MAX_CHANGED_PATHS: usize = 1024;
const RELEASED_MANIFEST: &str = "tools/released-migrations.json";
/// The store directories whose sources the pin file describes: a change under
/// one can add a migration block or a marked protected section (#1949).
const STORE_ROOTS: [&str; 2] = ["crates/rsid-store/src/store/", "crates/rsid/src/store/"];
/// Identity and subject of the commit that records a derived pin (#1949). The
/// stale retry recognises it by these to merge the source without it.
const PIN_COMMIT_NAME: &str = "rsi rolling landing";
const PIN_COMMIT_EMAIL: &str = "rsi-rolling-land@rsi.invalid";
const PIN_COMMIT_SUBJECT: &str = "rsi rolling landing: derive released-migration pin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyFence {
    PathInspection,
    ReleasedMigration,
    SchemaVersion,
    MigrationNumber,
}

impl PolicyFence {
    pub const fn label(self) -> &'static str {
        match self {
            Self::PathInspection => "path_inspection",
            Self::ReleasedMigration => "released_migration",
            Self::SchemaVersion => "schema_version",
            Self::MigrationNumber => "migration_number",
        }
    }
}

#[derive(Debug)]
pub struct PolicyRefusal {
    pub fence: PolicyFence,
    pub message: String,
}

impl PolicyRefusal {
    fn new(fence: PolicyFence, message: impl Into<String>) -> Self {
        Self {
            fence,
            message: message.into(),
        }
    }
}

fn protected_migration_paths(repo: &Path, revision: &str) -> Result<HashSet<String>, String> {
    let output = git_output(repo, &["show", &format!("{revision}:{RELEASED_MANIFEST}")])?;
    if !output.status.success() {
        return Err(format!(
            "released-migration manifest is missing at {revision}"
        ));
    }
    let manifest: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| format!("released-migration manifest is invalid at {revision}"))?;
    let mut paths = HashSet::from([RELEASED_MANIFEST.to_string()]);
    match (
        manifest["migration_dir"].as_str(),
        manifest["migration_file"].as_str(),
    ) {
        // A trailing slash marks a directory prefix (see `is_protected`).
        (Some(directory), _) => paths.insert(format!("{}/", directory.trim_end_matches('/'))),
        (None, Some(file)) => paths.insert(file.to_string()),
        (None, None) => {
            return Err("released-migration manifest has no migration file or directory".into());
        }
    };
    let sections = manifest["protected_sections"]
        .as_object()
        .ok_or("released-migration manifest has no protected sections")?;
    for section in sections.values() {
        let path = section["path"]
            .as_str()
            .ok_or("released-migration manifest has an invalid protected path")?;
        paths.insert(path.to_string());
    }
    Ok(paths)
}

fn is_protected(protected: &HashSet<String>, path: &str) -> bool {
    protected.contains(path)
        || protected
            .iter()
            .any(|entry| entry.ends_with('/') && path.starts_with(entry.as_str()))
}

/// `NNN` of a `migrations/vNNN.rs` path, else `None`.
fn migration_file_version(path: &str) -> Option<u32> {
    let name = MIGRATION_DIRS
        .iter()
        .find_map(|dir| path.strip_prefix(dir)?.strip_prefix('/'))?;
    let number = name.strip_prefix('v')?.strip_suffix(".rs")?;
    if number.contains('/') {
        None
    } else {
        number.parse().ok()
    }
}

fn is_migration_path(path: &str) -> bool {
    STORES.contains(&path) || migration_file_version(path).is_some()
}

/// Which store layout a revision builds from. `rsid-store/build.rs` collects
/// ONLY `crates/rsid-store/src/store/migrations/vNNN.rs`, so a revision whose
/// migration directory is populated there is `Split` and the pre-split
/// `crates/rsid/src/store/migrations` is dead to its daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreLayout {
    Split,
    PreSplit,
}

impl StoreLayout {
    const fn migration_dir(self) -> &'static str {
        match self {
            Self::Split => MIGRATION_DIR,
            Self::PreSplit => PRE_SPLIT_MIGRATION_DIR,
        }
    }
}

fn migration_files_in(repo: &Path, revision: &str, directory: &str) -> Result<Vec<String>, String> {
    let output = git_output(
        repo,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            revision,
            &format!("{directory}/"),
        ],
    )?;
    if !output.status.success() {
        return Err("cannot inspect landing migration files".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|path| migration_file_version(path).is_some())
        .map(str::to_owned)
        .collect())
}

fn store_layout(repo: &Path, revision: &str) -> Result<StoreLayout, String> {
    Ok(
        if migration_files_in(repo, revision, MIGRATION_DIR)?.is_empty() {
            StoreLayout::PreSplit
        } else {
            StoreLayout::Split
        },
    )
}

/// A candidate may only carry migrations in the directory its build consumes.
/// A base cut before the split is still read from its own (pre-split) layout;
/// what is refused is a split candidate that leaves or adds a versioned unit
/// in the dead pre-split directory (the daemon would never run it while the
/// number and manifest checks accept it), and a candidate that falls back to
/// the pre-split layout on a split target.
fn check_migration_layout(repo: &Path, target: &str, candidate: &str) -> Result<(), PolicyRefusal> {
    let refuse = |message: String| PolicyRefusal::new(PolicyFence::MigrationNumber, message);
    let layout = |revision| {
        store_layout(repo, revision)
            .map_err(|message| PolicyRefusal::new(PolicyFence::SchemaVersion, message))
    };
    let candidate_layout = layout(candidate)?;
    if candidate_layout == StoreLayout::Split {
        let stray = migration_files_in(repo, candidate, PRE_SPLIT_MIGRATION_DIR)
            .map_err(|message| PolicyRefusal::new(PolicyFence::SchemaVersion, message))?;
        if let Some(path) = stray.first() {
            return Err(refuse(format!(
                "migration {path} is in the pre-split directory {PRE_SPLIT_MIGRATION_DIR}; \
                 the store's build only runs {MIGRATION_DIR}/vNNN.rs, so move it there \
                 (a migration outside it is never executed)"
            )));
        }
    } else if layout(target)? == StoreLayout::Split {
        return Err(refuse(format!(
            "the landing candidate has no migrations under {MIGRATION_DIR} but the \
             target does; new migrations belong in {MIGRATION_DIR}/vNNN.rs"
        )));
    }
    Ok(())
}

/// The highest `migrations/vNNN.rs` at `revision` in the directory its layout
/// builds from, if the layout is per-file.
fn migration_file_head(repo: &Path, revision: &str) -> Result<Option<u32>, String> {
    let directory = store_layout(repo, revision)?.migration_dir();
    Ok(migration_files_in(repo, revision, directory)?
        .iter()
        .filter_map(|path| migration_file_version(path))
        .max())
}

fn changed_paths(repo: &Path, target: &str, candidate: &str) -> Result<Vec<String>, String> {
    let output = git_output(
        repo,
        &[
            "diff",
            "--no-ext-diff",
            "--no-renames",
            "--name-only",
            "-z",
            target,
            candidate,
        ],
    )?;
    if !output.status.success() {
        return Err("cannot inspect landing candidate paths".into());
    }
    let paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec())
                .map_err(|_| "landing candidate has a non-UTF-8 path".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if paths.len() > MAX_CHANGED_PATHS {
        return Err("landing candidate path count exceeds policy bound".into());
    }
    Ok(paths)
}

fn store_source(repo: &Path, revision: &str) -> Result<String, String> {
    for store in STORES {
        let output = git_output(repo, &["show", &format!("{revision}:{store}")])?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .map_err(|_| "landing migration source is not UTF-8".into());
        }
    }
    Err("cannot inspect landing migration source".into())
}

fn schema_version(repo: &Path, revision: &str) -> Result<u32, String> {
    if let Some(head) = migration_file_head(repo, revision)? {
        return Ok(head);
    }
    let source = store_source(repo, revision)?;
    let versions = source
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("pub const LATEST_SCHEMA_VERSION: i32 = ")
                .and_then(|value| value.strip_suffix(';'))
        })
        .collect::<Vec<_>>();
    if versions.len() != 1 {
        return Err("cannot identify one landing schema version".into());
    }
    versions[0]
        .parse::<u32>()
        .map_err(|_| "invalid landing schema version".into())
}

/// Whether `revision`'s store carries the `if version < N {` migration block.
fn has_migration_block(repo: &Path, revision: &str, version: u32) -> Result<bool, String> {
    let block = format!("if version < {version} {{");
    if migration_file_head(repo, revision)?.is_some() {
        let dir = store_layout(repo, revision)?.migration_dir();
        let output = git_output(
            repo,
            &["show", &format!("{revision}:{dir}/v{version:03}.rs")],
        )?;
        if !output.status.success() {
            return Ok(false);
        }
        return Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim() == block));
    }
    Ok(store_source(repo, revision)?
        .lines()
        .any(|line| line.trim() == block))
}

/// The mechanical migration-number rule: a candidate that raises the schema
/// version appends a contiguous run of migrations starting at exactly the
/// target tip's version + 1, and every version in the run carries its block
/// (one landing may carry several already-renumbered migrations in order).
fn check_migration_number(
    repo: &Path,
    target: &str,
    candidate: &str,
) -> Result<Vec<u32>, PolicyRefusal> {
    let schema = |revision| {
        schema_version(repo, revision)
            .map_err(|message| PolicyRefusal::new(PolicyFence::SchemaVersion, message))
    };
    let old = schema(target)?;
    let new = schema(candidate)?;
    if new < old {
        return Err(PolicyRefusal::new(
            PolicyFence::SchemaVersion,
            "landing candidate lowers the released schema version",
        ));
    }
    if new == old {
        return Ok(Vec::new());
    }
    let needed = old + 1;
    let mut appended = Vec::new();
    for version in needed..=new {
        let has_block = has_migration_block(repo, candidate, version)
            .map_err(|message| PolicyRefusal::new(PolicyFence::SchemaVersion, message))?;
        if !has_block {
            let message = if version == needed && new != needed {
                format!(
                    "migration must be V{needed}: rolling LATEST_SCHEMA_VERSION is {old}, \
                     the candidate declares {new} without a V{needed} block; renumber the \
                     new blocks to start at V{needed} with no gaps"
                )
            } else {
                format!(
                    "migration V{version} has no `if version < {version} {{` block in \
                     {MIGRATION_DIR}/v{version:03}.rs (or {STORE}); new migrations must run \
                     contiguously from V{needed} to the declared head V{new}"
                )
            };
            return Err(PolicyRefusal::new(PolicyFence::MigrationNumber, message));
        }
        appended.push(version);
    }
    Ok(appended)
}

fn is_store_path(path: &str) -> bool {
    STORE_ROOTS.iter().any(|root| path.starts_with(root))
}

/// #1949: the released-migrations pin is derived where the lander publishes, so
/// no worker edits the shared manifest and concurrent migrations never conflict
/// on it. `worktree` is the candidate's detached worktree at `candidate`. When
/// the diff `target..candidate` can change a pin and `--refresh` against
/// `target` rewrites the manifest, the new manifest is committed on top of the
/// candidate and its oid returned; `None` leaves the candidate as it is.
///
/// Best effort by design: a refresh that fails (a changed released block, a
/// missing altered-objects entry, ...) derives nothing, and the released
/// migration guard that follows refuses the candidate with the same reason.
pub(super) fn derive_released_pin(
    repo: &Path,
    worktree: &Path,
    target: &str,
    candidate: &str,
) -> Result<Option<String>, String> {
    let script = repo.join("tools/check-released-migrations.py");
    if !script.is_file() {
        return Ok(None);
    }
    let paths = changed_paths(repo, target, candidate)?;
    let protected = protected_migration_paths(repo, target)
        .and_then(|mut paths| {
            paths.extend(protected_migration_paths(repo, candidate)?);
            Ok(paths)
        })
        // A tree with no readable pin file has nothing to derive on.
        .unwrap_or_default();
    if !paths
        .iter()
        .any(|path| is_protected(&protected, path) || is_store_path(path))
    {
        return Ok(None);
    }
    // A source that edited the pin file owns its pin (an older worker's
    // `--refresh`): rewriting that text would read as lost hunks.
    let manifest_blob = |revision: &str| {
        super::git_text(
            repo,
            &["rev-parse", &format!("{revision}:{RELEASED_MANIFEST}")],
        )
    };
    if manifest_blob(target).ok() != manifest_blob(candidate).ok() {
        return Ok(None);
    }
    if super::git_text(worktree, &["rev-parse", "HEAD"])? != candidate
        || !super::git_text(worktree, &["status", "--porcelain=v1"])?.is_empty()
    {
        return Err("pin derivation needs a clean worktree at the candidate".into());
    }
    let refreshed = Command::new("python3")
        .arg(&script)
        .arg("--refresh")
        .arg(target)
        .current_dir(worktree)
        .output()
        .map_err(|error| format!("cannot start released-migration refresh: {error}"))?;
    if !refreshed.status.success() {
        // `--refresh` writes only after every check passed, so the worktree is
        // untouched; the guard that follows refuses with the reason.
        println!(
            "released_pin_derive=skipped reason={}",
            String::from_utf8_lossy(&refreshed.stderr).trim()
        );
        return Ok(None);
    }
    super::git_ok(worktree, &["add", "--", RELEASED_MANIFEST])?;
    if super::git_output(worktree, &["diff", "--cached", "--quiet"])?
        .status
        .success()
    {
        return Ok(None);
    }
    let name = format!("user.name={PIN_COMMIT_NAME}");
    let email = format!("user.email={PIN_COMMIT_EMAIL}");
    super::git_ok(
        worktree,
        &[
            "-c",
            &name,
            "-c",
            &email,
            "commit",
            "--no-verify",
            "--no-gpg-sign",
            "-q",
            "-m",
            PIN_COMMIT_SUBJECT,
        ],
    )?;
    if !super::git_text(worktree, &["status", "--porcelain=v1"])?.is_empty() {
        return Err("pin derivation left the candidate worktree dirty".into());
    }
    let derived = super::git_text(worktree, &["rev-parse", "HEAD"])?;
    println!("released_pin_derived={derived} parent={candidate}");
    Ok(Some(derived))
}

/// The candidate a pin commit from [`derive_released_pin`] sits on, `None` for
/// any other commit. A stale retry merges that parent onto the advanced tip and
/// derives the pin again, so the manifest never conflicts with a sibling's pin.
pub(super) fn pin_commit_parent(repo: &Path, oid: &str) -> Result<Option<String>, String> {
    let identity = super::git_text(repo, &["log", "-1", "--format=%ce%n%s", oid])?;
    if identity != format!("{PIN_COMMIT_EMAIL}\n{PIN_COMMIT_SUBJECT}") {
        return Ok(None);
    }
    let parent = super::git_text(repo, &["rev-parse", &format!("{oid}^")])?;
    Ok(Some(parent))
}

pub(super) fn check_released_migrations(
    repo: &Path,
    source_repo: &Path,
    target: &str,
    candidate: &str,
) -> Result<(), String> {
    let script = source_repo.join("tools/check-released-migrations.py");
    if !script.is_file() {
        return Err("released-migration guard script is missing".into());
    }
    let output = Command::new("python3")
        .arg(script)
        .arg(target)
        .arg(candidate)
        .current_dir(repo)
        .output()
        .map_err(|error| format!("cannot start released-migration guard: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "released-migration guard refused landing: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

pub fn check_with_proof(
    repo: &Path,
    source_repo: &Path,
    target: &str,
    candidate: &str,
    proved_versions: &[u32],
) -> Result<(), PolicyRefusal> {
    let paths = changed_paths(repo, target, candidate)
        .map_err(|message| PolicyRefusal::new(PolicyFence::PathInspection, message))?;
    check_migration_layout(repo, target, candidate)?;
    let protected = protected_migration_paths(repo, target)
        .and_then(|mut paths| {
            paths.extend(protected_migration_paths(repo, candidate)?);
            Ok(paths)
        })
        .map_err(|message| PolicyRefusal::new(PolicyFence::ReleasedMigration, message))?;
    if paths.iter().any(|path| is_protected(&protected, path)) {
        check_released_migrations(repo, source_repo, target, candidate)
            .map_err(|message| PolicyRefusal::new(PolicyFence::ReleasedMigration, message))?;
    }
    let new_versions = if paths.iter().any(|path| is_migration_path(path)) {
        check_migration_number(repo, target, candidate)?
    } else {
        Vec::new()
    };
    if proved_versions
        .iter()
        .any(|version| !new_versions.contains(version))
    {
        return Err(PolicyRefusal::new(
            PolicyFence::SchemaVersion,
            "provisional proof version is outside the candidate's appended migration range",
        ));
    }
    Ok(())
}
