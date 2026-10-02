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
//! - The Work binding is still reported (`source_binding=`) when the ledger is
//!   readable; it never refuses a landing.

use super::{AcceptedPair, git_output};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

const STORE: &str = "crates/rsid/src/store/mod.rs";
/// Each schema version is one `vNNN.rs` here; the head is the highest number.
const MIGRATION_DIR: &str = "crates/rsid/src/store/migrations";
const MAX_CHANGED_PATHS: usize = 1024;
const MAX_WORK_PAGES: usize = 16;
const RELEASED_MANIFEST: &str = "tools/released-migrations.json";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBinding {
    pub source: String,
    pub state: &'static str,
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
    let name = path.strip_prefix(MIGRATION_DIR)?.strip_prefix('/')?;
    let number = name.strip_prefix('v')?.strip_suffix(".rs")?;
    if number.contains('/') {
        None
    } else {
        number.parse().ok()
    }
}

fn is_migration_path(path: &str) -> bool {
    path == STORE || migration_file_version(path).is_some()
}

/// The highest `migrations/vNNN.rs` at `revision`, if the layout is per-file.
fn migration_file_head(repo: &Path, revision: &str) -> Result<Option<u32>, String> {
    let output = git_output(
        repo,
        &[
            "ls-tree",
            "--name-only",
            revision,
            &format!("{MIGRATION_DIR}/"),
        ],
    )?;
    if !output.status.success() {
        return Err("cannot inspect landing migration files".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(migration_file_version)
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
    let path = format!("{revision}:{STORE}");
    let output = git_output(repo, &["show", &path])?;
    if !output.status.success() {
        return Err("cannot inspect landing migration source".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "landing migration source is not UTF-8".into())
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
        let output = git_output(
            repo,
            &[
                "show",
                &format!("{revision}:{MIGRATION_DIR}/v{version:03}.rs"),
            ],
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

fn inspect_work() -> Result<Vec<Value>, String> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("RSI_LANDER_TEST_WORK_ROWS") {
        let data = std::fs::read(path).map_err(|error| error.to_string())?;
        return serde_json::from_slice(&data).map_err(|error| error.to_string());
    }
    if std::env::var_os("RSI_SESSION_TOKEN").is_none() {
        return Err("protected landing requires an rsi-managed Epic lead".into());
    }
    inspect_work_with_page(|cursor| {
        let response = rsi_common::agent_rpc_client::dispatch_from_env(
            "AgentManagerInspect",
            json!({"section":"work","cursor":cursor,"limit":64}),
        )
        .map_err(|error| format!("cannot read landing ownership ledger: {error}"))?;
        if let Some(error) = response.error {
            return Err(format!(
                "landing ownership ledger refused: {}",
                error.message
            ));
        }
        response
            .result
            .ok_or_else(|| "landing ownership ledger returned no result".into())
    })
}

fn inspect_work_with_page(
    mut page_source: impl FnMut(Value) -> Result<Value, String>,
) -> Result<Vec<Value>, String> {
    let mut cursor = Value::Null;
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for _ in 0..MAX_WORK_PAGES {
        let result = page_source(cursor.clone())?;
        if result["section"] != "work" {
            return Err("landing ownership ledger returned the wrong section".into());
        }
        let page = result["rows"]
            .as_array()
            .ok_or("landing ownership ledger returned invalid work rows")?;
        if page.len() > 64 {
            return Err("landing ownership ledger exceeded its page bound".into());
        }
        rows.extend(page.iter().cloned());
        cursor = result["next_cursor"].clone();
        if cursor.is_null() {
            if result["complete"] != true {
                return Err("landing ownership ledger traversal is incomplete".into());
            }
            return Ok(rows);
        }
        let serialized = cursor.to_string();
        if !seen.insert(serialized) {
            return Err("landing ownership ledger repeated a page cursor".into());
        }
    }
    Err("landing ownership ledger exceeded its traversal bound".into())
}

fn live_unintegrated(work: &Value) -> bool {
    if work["integrated"] == true || work["archived"] == true {
        return false;
    }
    let Some(updated_at) = work["updated_at"].as_str() else {
        return true;
    };
    let Ok(updated_at) = DateTime::parse_from_rfc3339(updated_at) else {
        return true;
    };
    updated_at.with_timezone(&Utc) >= Utc::now() - Duration::hours(24)
}

fn source_bindings(
    accepted: &[AcceptedPair],
    rows: &[Value],
    lookup_unknown: bool,
) -> Vec<SourceBinding> {
    accepted
        .iter()
        .map(|pair| {
            let bound = rows.iter().any(|row| {
                row["source_commit"] == pair.source
                    && row["source_accepted"] == true
                    && live_unintegrated(row)
            });
            SourceBinding {
                source: pair.source.clone(),
                state: if bound {
                    "bound"
                } else if lookup_unknown {
                    "unknown"
                } else {
                    "unbound"
                },
            }
        })
        .collect()
}

pub fn check_with_proof(
    repo: &Path,
    source_repo: &Path,
    target: &str,
    candidate: &str,
    accepted: &[AcceptedPair],
    proved_versions: &[u32],
) -> Result<Vec<SourceBinding>, PolicyRefusal> {
    check_with_work_and_proof(
        repo,
        source_repo,
        target,
        candidate,
        accepted,
        proved_versions,
        inspect_work,
    )
}

#[cfg(test)]
pub fn check_with_work(
    repo: &Path,
    source_repo: &Path,
    target: &str,
    candidate: &str,
    accepted: &[AcceptedPair],
    work_rows: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Result<Vec<SourceBinding>, PolicyRefusal> {
    check_with_work_and_proof(
        repo,
        source_repo,
        target,
        candidate,
        accepted,
        &[],
        work_rows,
    )
}

pub fn check_with_work_and_proof(
    repo: &Path,
    source_repo: &Path,
    target: &str,
    candidate: &str,
    accepted: &[AcceptedPair],
    proved_versions: &[u32],
    work_rows: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Result<Vec<SourceBinding>, PolicyRefusal> {
    let paths = changed_paths(repo, target, candidate)
        .map_err(|message| PolicyRefusal::new(PolicyFence::PathInspection, message))?;
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
    Ok(report_bindings(accepted, work_rows))
}

/// Report each source's Work binding. The lookup is informational: an
/// unreadable ledger (no lead token, daemon down) reports `unknown` and never
/// refuses the landing.
fn report_bindings(
    accepted: &[AcceptedPair],
    work_rows: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Vec<SourceBinding> {
    work_rows().map_or_else(
        |_| source_bindings(accepted, &[], true),
        |rows| source_bindings(accepted, &rows, false),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "1111111111111111111111111111111111111111";

    fn pair() -> AcceptedPair {
        AcceptedPair {
            base: "0".repeat(40),
            source: SOURCE.into(),
        }
    }

    fn row(integrated: bool) -> Value {
        json!({
            "key":"landing-work", "epic_id":"epic-j", "source_commit":SOURCE,
            "source_accepted":true, "integrated":integrated, "archived":false,
            "updated_at":Utc::now().to_rfc3339(),
        })
    }

    #[test]
    fn binding_report_is_bound_unbound_or_unknown_and_never_refuses() {
        let bound = report_bindings(&[pair()], || Ok(vec![row(false)]));
        assert_eq!(bound[0].source, SOURCE);
        assert_eq!(bound[0].state, "bound");
        let unbound = report_bindings(&[pair()], || Ok(vec![]));
        assert_eq!(unbound[0].state, "unbound");
        let integrated = report_bindings(&[pair()], || Ok(vec![row(true)]));
        assert_eq!(integrated[0].state, "unbound");
        let unknown = report_bindings(&[pair()], || Err("no lead token".into()));
        assert_eq!(unknown[0].state, "unknown");
    }

    #[test]
    fn work_traversal_requires_complete_distinct_bounded_pages() {
        let mut calls = Vec::new();
        let rows = inspect_work_with_page(|cursor| {
            calls.push(cursor.clone());
            if cursor.is_null() {
                Ok(json!({"section":"work","rows":[{"key":"first"}],"next_cursor":"next","complete":false}))
            } else {
                Ok(json!({"section":"work","rows":[{"key":"second"}],"next_cursor":null,"complete":true}))
            }
        })
        .unwrap();
        assert_eq!(calls, [Value::Null, json!("next")]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["key"], "first");
        assert_eq!(rows[1]["key"], "second");

        assert!(
            inspect_work_with_page(|_| Ok(
                json!({"section":"work","rows":[],"next_cursor":null,"complete":false})
            ))
            .unwrap_err()
            .contains("incomplete")
        );
        assert!(
            inspect_work_with_page(|_| Ok(
                json!({"section":"work","rows":[],"next_cursor":"repeat","complete":false})
            ))
            .unwrap_err()
            .contains("repeated")
        );
        let mut page = 0;
        assert!(
            inspect_work_with_page(|_| {
                page += 1;
                Ok(json!({"section":"work","rows":[],"next_cursor":page,"complete":false}))
            })
            .unwrap_err()
            .contains("traversal bound")
        );
    }
}
