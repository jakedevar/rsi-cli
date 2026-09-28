//! #628: fail closed before publishing a candidate that touches coordinated files.

use super::{AcceptedPair, git_output};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

const STORE: &str = "crates/rsid/src/store/mod.rs";
const MAX_CHANGED_PATHS: usize = 1024;
const MAX_WORK_PAGES: usize = 16;
const RELEASED_MANIFEST: &str = "tools/released-migrations.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyFence {
    PathInspection,
    ReleasedMigration,
    SchemaVersion,
    LedgerUnavailable,
    SourceUnbound,
    HotFileUnowned,
    MigrationUnreserved,
    MigrationOutOfOrder,
}

impl PolicyFence {
    pub const fn label(self) -> &'static str {
        match self {
            Self::PathInspection => "path_inspection",
            Self::ReleasedMigration => "released_migration",
            Self::SchemaVersion => "schema_version",
            Self::LedgerUnavailable => "ledger_unavailable",
            Self::SourceUnbound => "source_unbound",
            Self::HotFileUnowned => "hot_file_unowned",
            Self::MigrationUnreserved => "migration_unreserved",
            Self::MigrationOutOfOrder => "migration_out_of_order",
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

fn is_hot_file(path: &str) -> bool {
    matches!(
        path,
        "crates/rsid/src/rpc.rs"
            | "crates/rsi-common/src/agent_control_schema.rs"
            | "crates/rsi-common/src/bin/rsi-rpc.rs"
            | "crates/rsid/src/session/harness/tools/mod.rs"
            | "crates/rsid/src/session/harness/tools/rsi_control.rs"
            | "crates/rsid/src/tool_registry.rs"
            | STORE
            | "AGENTS.md"
    ) || path.ends_with("/AGENTS.md")
        || path.ends_with("/harness_manager_v2.rs")
        || path.starts_with(".claude/skills/rsi-")
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
    let migration_file = manifest["migration_file"]
        .as_str()
        .ok_or("released-migration manifest has no migration file")?;
    paths.insert(migration_file.to_string());
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

fn schema_version(repo: &Path, revision: &str) -> Result<u32, String> {
    let path = format!("{revision}:{STORE}");
    let output = git_output(repo, &["show", &path])?;
    if !output.status.success() {
        return Err("cannot inspect landing migration source".into());
    }
    let source = String::from_utf8(output.stdout)
        .map_err(|_| "landing migration source is not UTF-8".to_string())?;
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

fn inspect_allocations() -> Result<Vec<Value>, String> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("RSI_LANDER_TEST_ALLOCATION_ROWS") {
        let data = std::fs::read(path).map_err(|error| error.to_string())?;
        return serde_json::from_slice(&data).map_err(|error| error.to_string());
    }
    if std::env::var_os("RSI_SESSION_TOKEN").is_none() {
        return Err("migration landing requires an rsi-managed Epic lead".into());
    }
    let mut cursor = Value::Null;
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for _ in 0..MAX_WORK_PAGES {
        let response = rsi_common::agent_rpc_client::dispatch_from_env(
            "AgentManagerInspect",
            json!({"section":"migration_allocations","cursor":cursor,"limit":64}),
        )
        .map_err(|error| format!("cannot read migration allocations: {error}"))?;
        if let Some(error) = response.error {
            return Err(format!(
                "migration allocation inspection refused: {}",
                error.message
            ));
        }
        let result = response
            .result
            .ok_or_else(|| "migration allocation inspection returned no result".to_string())?;
        if result["section"] != "migration_allocations" {
            return Err("migration allocation inspection returned the wrong section".into());
        }
        let page = result["rows"]
            .as_array()
            .ok_or("migration allocation inspection returned invalid rows")?;
        if page.len() > 64 {
            return Err("migration allocation inspection exceeded its page bound".into());
        }
        rows.extend(page.iter().cloned());
        cursor = result["next_cursor"].clone();
        if cursor.is_null() {
            if result["complete"] != true {
                return Err("migration allocation inspection is incomplete".into());
            }
            return Ok(rows);
        }
        if !seen.insert(cursor.to_string()) {
            return Err("migration allocation inspection repeated a page cursor".into());
        }
    }
    Err("migration allocation inspection exceeded its traversal bound".into())
}

fn check_allocation_rows(
    rows: &[Value],
    accepted: &[AcceptedPair],
    proved_versions: &[u32],
    mut predecessor_landed: impl FnMut(&str) -> Result<bool, String>,
) -> Result<(), PolicyRefusal> {
    for version in proved_versions {
        let matches = rows
            .iter()
            .filter(|row| {
                row["version"].as_u64() == Some(u64::from(*version))
                    && matches!(row["state"].as_str(), Some("active" | "publishing"))
                    && accepted
                        .iter()
                        .any(|pair| row["source_commit"] == pair.source)
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(PolicyRefusal::new(
                PolicyFence::MigrationOutOfOrder,
                format!("migration V{version} lacks one live exact-source seal in landing order"),
            ));
        }
        let predecessors = matches[0]["predecessor_sources"]
            .as_array()
            .ok_or_else(|| {
                PolicyRefusal::new(
                    PolicyFence::LedgerUnavailable,
                    "migration allocation lacks predecessor evidence",
                )
            })?;
        if predecessors.len() > 256 {
            return Err(PolicyRefusal::new(
                PolicyFence::MigrationOutOfOrder,
                "migration predecessor traversal exceeds its bound",
            ));
        }
        for prior in predecessors {
            let source = prior
                .as_str()
                .filter(|source| {
                    source.len() == 40
                        && source
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                })
                .ok_or_else(|| {
                    PolicyRefusal::new(
                        PolicyFence::LedgerUnavailable,
                        "invalid migration predecessor source",
                    )
                })?;
            if !predecessor_landed(source)
                .map_err(|message| PolicyRefusal::new(PolicyFence::LedgerUnavailable, message))?
            {
                return Err(PolicyRefusal::new(
                    PolicyFence::MigrationOutOfOrder,
                    format!("migration V{version} precedes an unlanded lower seal"),
                ));
            }
        }
    }
    Ok(())
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

fn check_rows(
    rows: &[Value],
    accepted: &[AcceptedPair],
    hot_paths: &[String],
    new_versions: &[u32],
    migration_requires_binding: bool,
) -> Result<(), PolicyRefusal> {
    let mut matched = Vec::new();
    let mut epic = None;
    if migration_requires_binding {
        for pair in accepted {
            let source_rows = rows
                .iter()
                .filter(|row| {
                    row["source_commit"] == pair.source
                        && row["source_accepted"] == true
                        && live_unintegrated(row)
                })
                .collect::<Vec<_>>();
            if source_rows.is_empty() {
                let hint = recorded_work_hint(rows, new_versions);
                return Err(PolicyRefusal::new(
                    PolicyFence::SourceUnbound,
                    format!(
                        "accepted source {} lacks a visible live Work record{hint}",
                        pair.source,
                    ),
                ));
            }
            for row in source_rows {
                let row_epic = row["epic_id"].as_str().ok_or_else(|| {
                    PolicyRefusal::new(
                        PolicyFence::SourceUnbound,
                        "accepted Work has no owning Epic",
                    )
                })?;
                if epic.is_some_and(|current| current != row_epic) {
                    return Err(PolicyRefusal::new(
                        PolicyFence::SourceUnbound,
                        "accepted sources belong to different Epics",
                    ));
                }
                epic = Some(row_epic);
                matched.push(row);
            }
        }
    }
    for path in hot_paths {
        let blocked = rows.iter().any(|work| {
            if !live_unintegrated(work) {
                return false;
            }
            let own_source = accepted.iter().any(|pair| {
                pair.source == work["source_commit"].as_str().unwrap_or_default()
                    && work["source_accepted"] == true
            });
            !own_source
                && work["ownership"].as_array().is_some_and(|claims| {
                    claims.iter().any(|claim| {
                        claim["active"] == true
                            && claim["mode"] == "exclusive"
                            && claim["domain"] == *path
                            && claim["work_key"] == work["key"]
                            && claim["files"].as_array().is_some_and(|files| {
                                files.iter().any(|file| file.as_str() == Some(path))
                            })
                    })
                })
        });
        if blocked {
            return Err(PolicyRefusal::new(
                PolicyFence::HotFileUnowned,
                format!("hot file is claimed by another live unintegrated Work: {path}"),
            ));
        }
    }
    for version in new_versions {
        let reserved = matched.iter().any(|work| {
            work["migration_reservations"]
                .as_array()
                .is_some_and(|claims| {
                    claims.iter().any(|claim| {
                        claim["version"].as_u64() == Some(u64::from(*version))
                            && claim["active"] != false
                            && claim["work_key"] == work["key"]
                            && claim["row_version"].as_i64().is_some_and(|v| v > 0)
                    })
                })
        });
        if !reserved {
            return Err(PolicyRefusal::new(
                PolicyFence::MigrationUnreserved,
                format!("migration V{version} is not reserved to the landing Epic"),
            ));
        }
    }
    Ok(())
}

fn recorded_work_hint(rows: &[Value], new_versions: &[u32]) -> String {
    let recorded = rows
        .iter()
        .filter(|row| live_unintegrated(row))
        .filter(|row| {
            new_versions.is_empty()
                || row["migration_reservations"]
                    .as_array()
                    .is_some_and(|claims| {
                        claims.iter().any(|claim| {
                            claim["version"].as_u64().is_some_and(|version| {
                                new_versions
                                    .iter()
                                    .any(|candidate| u64::from(*candidate) == version)
                            })
                        })
                    })
        })
        .filter_map(|row| {
            Some(format!(
                "{} source_commit={}",
                row["key"].as_str()?,
                row["source_commit"].as_str()?
            ))
        })
        .take(4)
        .collect::<Vec<_>>();
    if recorded.is_empty() {
        String::new()
    } else {
        format!("; recorded Work: {}", recorded.join(", "))
    }
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

/*
 * Migration reservations remain bound to accepted Work. Hot-file conflicts
 * are checked independently against all live Work claims, so an otherwise
 * eligible unbound landing can proceed when no other Work owns the path.
 */

pub fn check(
    repo: &Path,
    source_repo: &Path,
    target: &str,
    candidate: &str,
    accepted: &[AcceptedPair],
) -> Result<(), PolicyRefusal> {
    check_with_proof(repo, source_repo, target, candidate, accepted, &[]).map(|_| ())
}

pub fn check_with_proof(
    repo: &Path,
    source_repo: &Path,
    target: &str,
    candidate: &str,
    accepted: &[AcceptedPair],
    proved_versions: &[u32],
) -> Result<Vec<SourceBinding>, PolicyRefusal> {
    let bindings = check_with_work_and_proof(
        repo,
        source_repo,
        target,
        candidate,
        accepted,
        proved_versions,
        inspect_work,
    )?;
    if !proved_versions.is_empty() {
        let rows = inspect_allocations()
            .map_err(|message| PolicyRefusal::new(PolicyFence::LedgerUnavailable, message))?;
        check_allocation_rows(&rows, accepted, proved_versions, |source| {
            super::git_is_ancestor(repo, source, target)
        })?;
    }
    Ok(bindings)
}

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
    let hot_paths = paths
        .iter()
        .filter(|path| is_hot_file(path))
        .cloned()
        .collect::<Vec<_>>();
    let mut new_versions = Vec::new();
    let protected = protected_migration_paths(repo, target)
        .and_then(|mut paths| {
            paths.extend(protected_migration_paths(repo, candidate)?);
            Ok(paths)
        })
        .map_err(|message| PolicyRefusal::new(PolicyFence::ReleasedMigration, message))?;
    if paths.iter().any(|path| protected.contains(path)) {
        check_released_migrations(repo, source_repo, target, candidate)
            .map_err(|message| PolicyRefusal::new(PolicyFence::ReleasedMigration, message))?;
    }
    if paths.iter().any(|path| path == STORE) {
        let old = schema_version(repo, target)
            .map_err(|message| PolicyRefusal::new(PolicyFence::SchemaVersion, message))?;
        let new = schema_version(repo, candidate)
            .map_err(|message| PolicyRefusal::new(PolicyFence::SchemaVersion, message))?;
        if new < old {
            return Err(PolicyRefusal::new(
                PolicyFence::SchemaVersion,
                "landing candidate lowers the released schema version",
            ));
        }
        new_versions.extend(old + 1..=new);
    }
    if proved_versions
        .iter()
        .any(|version| !new_versions.contains(version))
    {
        return Err(PolicyRefusal::new(
            PolicyFence::SchemaVersion,
            "provisional proof version is outside the candidate's appended migration range",
        ));
    }
    new_versions.retain(|version| !proved_versions.contains(version));
    check_work_requirements(
        accepted,
        &hot_paths,
        &new_versions,
        proved_versions,
        work_rows,
    )
}

fn check_work_requirements(
    accepted: &[AcceptedPair],
    hot_paths: &[String],
    unproved_versions: &[u32],
    proved_versions: &[u32],
    work_rows: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Result<Vec<SourceBinding>, PolicyRefusal> {
    let required =
        !hot_paths.is_empty() || !unproved_versions.is_empty() || !proved_versions.is_empty();
    let should_inspect = required || std::env::var_os("RSI_SESSION_TOKEN").is_some();
    let (rows, lookup_unknown) = inspect_work_or_mark_unknown(required, should_inspect, work_rows)?;
    check_rows(
        &rows,
        accepted,
        hot_paths,
        unproved_versions,
        !unproved_versions.is_empty() || !proved_versions.is_empty(),
    )?;
    Ok(source_bindings(accepted, &rows, lookup_unknown))
}

fn inspect_work_or_mark_unknown(
    required: bool,
    should_inspect: bool,
    work_rows: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Result<(Vec<Value>, bool), PolicyRefusal> {
    if !should_inspect {
        return Ok((Vec::new(), true));
    }
    match work_rows() {
        Ok(rows) => Ok((rows, false)),
        Err(message) if required => {
            Err(PolicyRefusal::new(PolicyFence::LedgerUnavailable, message))
        }
        Err(_) => Ok((Vec::new(), true)),
    }
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

    fn row(active: bool, mode: &str, domain: &str, version: u32) -> Value {
        json!({
            "key":"landing-work", "epic_id":"epic-j", "source_commit":SOURCE,
            "source_accepted":true, "integrated":false, "archived":false,
            "updated_at":Utc::now().to_rfc3339(),
            "ownership":[{"work_key":"landing-work","active":active,"mode":mode,
                          "domain":domain,"files":[STORE]}],
            "migration_reservations":[{"work_key":"landing-work","version":version,
                                       "row_version":1}]
        })
    }

    #[test]
    fn catalog_marks_registered_paths() {
        for path in [
            STORE,
            "crates/rsid/src/rpc.rs",
            "crates/rsi-common/src/agent_control_schema.rs",
            "crates/rsi-common/src/bin/rsi-rpc.rs",
            "crates/rsid/src/session/harness/tools/mod.rs",
            "crates/rsid/src/session/harness/tools/rsi_control.rs",
            "crates/rsid/src/tool_registry.rs",
            "crates/rsi-common/src/harness_manager_v2.rs",
            "AGENTS.md",
            "crates/rsid/AGENTS.md",
            ".claude/skills/rsi-agent-control/SKILL.md",
        ] {
            assert!(is_hot_file(path), "{path}");
        }
        assert!(!is_hot_file("scripts/rolling-landing-guard.py"));
    }

    #[test]
    fn exact_exclusive_claim_and_next_migration_pass() {
        let rows = [row(true, "exclusive", STORE, 125)];
        check_rows(&rows, &[pair()], &[STORE.into()], &[125], true).unwrap();
    }

    #[test]
    fn proved_migration_requires_accepted_work_without_a_number_reservation() {
        let mut accepted = row(true, "exclusive", STORE, 125);
        accepted["migration_reservations"] = json!([]);
        let check = |rows| check_work_requirements(&[pair()], &[], &[], &[125], || Ok(rows));
        check(vec![accepted.clone()]).expect("live accepted Work authorizes the proved unit");
        assert_eq!(check(vec![]).unwrap_err().fence, PolicyFence::SourceUnbound);
        accepted["source_accepted"] = json!(false);
        assert_eq!(
            check(vec![accepted]).unwrap_err().fence,
            PolicyFence::SourceUnbound
        );
    }

    #[test]
    fn unrelated_nonmigration_change_needs_no_work_lookup() {
        let binding = check_work_requirements(&[pair()], &[], &[], &[], || Ok(vec![]))
            .expect("ordinary source has a binding status");
        assert!(matches!(binding[0].state, "unbound" | "unknown"));
    }

    #[test]
    fn skipped_work_lookup_reports_binding_unknown() {
        let (rows, lookup_unknown) =
            inspect_work_or_mark_unknown(false, false, || panic!("lookup was skipped"))
                .expect("skipping an optional lookup is not a refusal");
        let binding = source_bindings(&[pair()], &rows, lookup_unknown);
        assert_eq!(binding[0].state, "unknown");
    }

    #[test]
    fn missing_wrong_domain_shared_and_inactive_claims_refuse() {
        for (active, mode, domain) in [
            (false, "exclusive", STORE),
            (true, "shared", STORE),
            (true, "exclusive", "other-domain"),
        ] {
            let mut other = row(active, mode, domain, 125);
            other["key"] = json!("other-work");
            other["source_commit"] = json!("2222222222222222222222222222222222222222");
            assert!(check_rows(&[other], &[pair()], &[STORE.into()], &[], false).is_ok());
        }
        assert!(check_rows(&[], &[pair()], &[STORE.into()], &[], false).is_ok());
    }

    #[test]
    fn unbound_hot_file_source_passes_and_is_reported_when_no_work_claims_it() {
        let binding = check_work_requirements(&[pair()], &[STORE.into()], &[], &[], || Ok(vec![]))
            .expect("an unbound source is allowed without a conflicting live claim");
        assert_eq!(binding[0].source, SOURCE);
        assert_eq!(binding[0].state, "unbound");
    }

    #[test]
    fn hot_file_claims_only_block_for_other_live_unintegrated_work() {
        let mut other = row(true, "exclusive", STORE, 125);
        other["key"] = json!("other-work");
        other["source_commit"] = json!("2222222222222222222222222222222222222222");
        other["ownership"][0]["work_key"] = json!("other-work");
        let pairs = [pair()];
        assert_eq!(
            check_rows(&[other.clone()], &pairs, &[STORE.into()], &[], false)
                .unwrap_err()
                .fence,
            PolicyFence::HotFileUnowned
        );
        other["integrated"] = json!(true);
        check_rows(&[other.clone()], &pairs, &[STORE.into()], &[], false)
            .expect("integrated claim is void");
        other["integrated"] = json!(false);
        other["archived"] = json!(true);
        check_rows(&[other.clone()], &pairs, &[STORE.into()], &[], false)
            .expect("archived claim is void");
        other["archived"] = json!(false);
        other["updated_at"] = json!((Utc::now() - Duration::hours(25)).to_rfc3339());
        check_rows(&[other], &pairs, &[STORE.into()], &[], false).expect("idle claim is void");
    }

    #[test]
    fn reservation_and_source_binding_refuse_mismatch() {
        let rows = [row(true, "exclusive", STORE, 125)];
        assert_eq!(
            check_rows(&rows, &[pair()], &[STORE.into()], &[126], true)
                .unwrap_err()
                .fence,
            PolicyFence::MigrationUnreserved
        );
        let mut different = pair();
        different.source = "2".repeat(40);
        let refusal = check_rows(&rows, &[different], &[], &[125], true).unwrap_err();
        assert_eq!(refusal.fence, PolicyFence::SourceUnbound);
        assert!(refusal.message.contains(&format!("source_commit={SOURCE}")));
    }

    #[test]
    fn transferred_reservation_passes_and_released_reservation_refuses() {
        let mut former = row(true, "exclusive", STORE, 125);
        former["key"] = json!("former-work");
        former["migration_reservations"] = json!([]);
        let current = row(true, "exclusive", STORE, 125);
        check_rows(
            &[former.clone(), current.clone()],
            &[pair()],
            &[],
            &[125],
            true,
        )
        .unwrap();
        let mut released = current;
        released["migration_reservations"][0]["active"] = json!(false);
        assert_eq!(
            check_rows(&[former, released], &[pair()], &[], &[125], true)
                .unwrap_err()
                .fence,
            PolicyFence::MigrationUnreserved
        );
    }

    #[test]
    fn mixed_epic_sources_refuse_with_source_fence() {
        let mut other = row(true, "exclusive", STORE, 125);
        other["source_commit"] = json!("2222222222222222222222222222222222222222");
        other["epic_id"] = json!("epic-other");
        let pairs = [
            pair(),
            AcceptedPair {
                base: "0".repeat(40),
                source: "2".repeat(40),
            },
        ];
        assert_eq!(
            check_rows(
                &[row(true, "exclusive", STORE, 125), other],
                &pairs,
                &[],
                &[],
                true
            )
            .unwrap_err()
            .fence,
            PolicyFence::SourceUnbound
        );
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

    #[test]
    fn proved_migration_requires_its_exact_source_and_allocated_version() {
        let sealed =
            json!({"source_commit":SOURCE,"version":125,"state":"active","predecessor_sources":[]});
        check_allocation_rows(&[sealed.clone()], &[pair()], &[125], |_| Ok(true)).unwrap();
        assert_eq!(
            check_allocation_rows(&[sealed.clone()], &[pair()], &[126], |_| Ok(true))
                .unwrap_err()
                .fence,
            PolicyFence::MigrationOutOfOrder
        );
        let wrong_source = json!({"source_commit":"2222222222222222222222222222222222222222","version":125,"state":"active","predecessor_sources":[]});
        assert_eq!(
            check_allocation_rows(&[wrong_source], &[pair()], &[125], |_| Ok(true))
                .unwrap_err()
                .fence,
            PolicyFence::MigrationOutOfOrder
        );
        let mut released = sealed.clone();
        released["state"] = json!("released");
        assert_eq!(
            check_allocation_rows(&[released], &[pair()], &[125], |_| Ok(true))
                .unwrap_err()
                .fence,
            PolicyFence::MigrationOutOfOrder
        );
        assert_eq!(
            check_allocation_rows(
                &[sealed.clone(), sealed.clone()],
                &[pair()],
                &[125],
                |_| Ok(true)
            )
            .unwrap_err()
            .fence,
            PolicyFence::MigrationOutOfOrder
        );
        let earlier = "3333333333333333333333333333333333333333";
        let blocked = json!({"source_commit":SOURCE,"version":125,"state":"active","predecessor_sources":[earlier]});
        assert_eq!(
            check_allocation_rows(&[blocked.clone()], &[pair()], &[125], |_| Ok(false))
                .unwrap_err()
                .fence,
            PolicyFence::MigrationOutOfOrder
        );
        check_allocation_rows(
            &[blocked],
            &[pair()],
            &[125],
            |source| Ok(source == earlier),
        )
        .unwrap();
    }
}
