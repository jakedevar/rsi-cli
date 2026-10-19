//! Refresh the shared v1 diagnostic export before a daemon-owned landing (#1646).
//! This uses the same live Issue source as AgentQueryFailureSignatures; no
//! Issue bodies are written to the snapshot, only parsed signature records.

use crate::store::Store;
use chrono::{DateTime, Utc};
use rsi_common::failure_signature::{Snapshot, build_snapshot};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(super) fn default_path() -> PathBuf {
    std::env::var_os("RSI_KNOWN_FAILURES_SNAPSHOT").map_or_else(
        || {
            let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
            home.join(".rsi/qa/known-failures.v1.json")
        },
        PathBuf::from,
    )
}

/// Return a diagnostic for the gate log. Failure remains advisory: retain the
/// old export and let the lander report its staleness as well. Missing or
/// malformed exports are repaired by the same path as expired ones.
pub(super) fn refresh_if_needed(path: &Path, store: &Store, now: DateTime<Utc>) -> String {
    let trigger = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Snapshot>(&text) {
            Ok(snapshot) => match snapshot.stale_warning(now) {
                None => return String::new(),
                Some(warning) => warning,
            },
            Err(error) => format!("invalid snapshot: {error}"),
        },
        Err(error) => format!("cannot read snapshot: {error}"),
    };
    match refresh(path, store, now) {
        Ok((count, errors)) => {
            let mut diagnostic = format!(
                "known_failures=refreshed:{}: records={count}\n",
                path.display()
            );
            for error in errors {
                diagnostic.push_str(&format!("known_failures=malformed:{error}\n"));
            }
            diagnostic
        }
        Err(error) => format!(
            "known_failures=refresh_failed:{}: trigger={trigger}; reason={error}\n",
            path.display()
        ),
    }
}

fn refresh(path: &Path, store: &Store, now: DateTime<Utc>) -> Result<(usize, Vec<String>), String> {
    // The legacy shared v1 export has no project field. Export live signature
    // Issues across the store, with the same status/archive filtering as the
    // project-scoped live query. The caller holds the store lock throughout.
    let mut issues = Vec::new();
    for project in store.load_projects().map_err(|error| error.to_string())? {
        for issue in store
            .open_failure_signature_issues(project.id)
            .map_err(|error| error.to_string())?
        {
            issues.push((issue.display_number, issue.status, issue.body));
        }
    }
    let mut built = build_snapshot(&issues, None);
    built.snapshot.exported_at = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let count = built.snapshot.records.len();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    // Same-directory persist replaces atomically, so classifiers cannot read
    // half an export and a failed write cannot destroy the previous snapshot.
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("cannot create snapshot temporary file: {error}"))?;
    serde_json::to_writer_pretty(&mut temp, &built.snapshot)
        .map_err(|error| format!("cannot serialize snapshot: {error}"))?;
    temp.write_all(b"\n")
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|error| format!("cannot write snapshot: {error}"))?;
    temp.persist(path)
        .map_err(|error| format!("cannot replace snapshot: {error}"))?;
    Ok((count, built.errors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::failure_signature::{FENCE_INFO, SNAPSHOT_STALE_HOURS};
    use rsi_common::types::{IssueStatus, IssueUpdate, NewIssue, Project};
    use uuid::Uuid;

    fn store_with_signature() -> (Store, Uuid, u64) {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "snapshot-fixture".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let issue = store
            .create_issue(&NewIssue {
                project_id: project,
                title: "signature-owner".into(),
                body: String::new(),
                priority: None,
                labels: Vec::new(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap();
        let number = u64::try_from(issue.display_number).unwrap();
        let record = serde_json::json!({
            "test_id": "demo::red", "matcher": {"contains": ["boom"]},
            "issue": number, "class": "regression",
        });
        store
            .update_issue(
                issue.id,
                &IssueUpdate {
                    body: Some(format!("```{FENCE_INFO}\n{record}\n```\n")),
                    ..Default::default()
                },
            )
            .unwrap();
        (store, issue.id, number)
    }

    fn read(path: &Path) -> Snapshot {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn refresh_at_age_limit_drops_closed_owner_and_keeps_fresh_export() {
        let (store, owner, number) = store_with_signature();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qa/known-failures.v1.json");
        let now = Utc::now();
        assert!(refresh_if_needed(&path, &store, now).contains("known_failures=refreshed:"));
        let first = read(&path);
        assert_eq!(first.records.len(), 1);
        assert_eq!(first.records[0].record.issue, number);
        assert_eq!(first.stale_warning(now), None);
        store
            .update_issue_status(owner, IssueStatus::Closed)
            .unwrap();
        // Below the age limit the cached export stays byte-for-byte intact.
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            refresh_if_needed(
                &path,
                &store,
                now + chrono::Duration::hours(SNAPSHOT_STALE_HOURS) - chrono::Duration::seconds(1)
            ),
            ""
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        // At the limit, use current owner state rather than the old snapshot.
        let later = now + chrono::Duration::hours(SNAPSHOT_STALE_HOURS);
        assert!(refresh_if_needed(&path, &store, later).contains("records=0"));
        let refreshed = read(&path);
        assert!(refreshed.records.is_empty());
        assert_eq!(refreshed.stale_warning(later), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn failed_refresh_reports_trigger_and_reason_and_preserves_stale_export() {
        let (store, _, _) = store_with_signature();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        let now = Utc::now();
        refresh_if_needed(&path, &store, now);
        let original = std::fs::read(&path).unwrap();
        // A source read error, independent of filesystem permissions/root.
        store
            .conn
            .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE issues")
            .unwrap();
        let diagnostic = refresh_if_needed(
            &path,
            &store,
            now + chrono::Duration::hours(SNAPSHOT_STALE_HOURS),
        );
        assert!(diagnostic.contains("known_failures=refresh_failed:"));
        assert!(diagnostic.contains("trigger=known-failure snapshot is 24h old"));
        assert!(diagnostic.contains("reason="));
        assert!(diagnostic.contains("no such table: issues"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn malformed_export_is_repaired_and_write_failure_is_reported() {
        let (store, _, _) = store_with_signature();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        std::fs::write(&path, "invalid json").unwrap();
        assert!(refresh_if_needed(&path, &store, Utc::now()).contains("records=1"));
        assert_eq!(read(&path).records.len(), 1);
        let blocked = path.join("snapshot.json");
        let diagnostic = refresh_if_needed(&blocked, &store, Utc::now());
        assert!(diagnostic.contains("known_failures=refresh_failed:"));
        assert!(diagnostic.contains("reason=cannot create"));
    }
}
