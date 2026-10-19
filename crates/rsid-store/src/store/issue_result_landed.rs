//! Hub #2911: tell the project manager, in one typed notice, that an Open or
//! InProgress Issue's named commit has landed on rolling, so it can close the
//! Issue in one call instead of rediscovering the landing by hand.
//!
//! Decisions:
//! - The signal is a project-wide manager notice (`ledger_change`, subject
//!   `issue_result_landed:<display number>`, version = the published source
//!   SHA, `record_kind` `issue_result_landed`). Nothing is auto-closed: a RESULT
//!   can be partial, so the Issue stays as it is and the manager decides.
//! - Recorded in the settle transaction of a `published` queue entry, so the
//!   notice commits or rolls back with the publication; a recording failure is
//!   logged and never blocks the settlement.
//! - Bounded by the SHAs being published, not by the Issue table: one indexed
//!   read of the project's Open/InProgress Issues (at most
//!   [`MAX_OPEN_ISSUES_SCANNED`], newest first), the *latest* handoff of each
//!   (a primary-key lookup), and at most [`MAX_NAMED_SHAS`] named SHAs from it,
//!   compared against the entry's source commit and landed tip. No Issue body is
//!   scanned.
//! - A "named commit" is a lowercase hex token of [`MIN_NAMED_SHA`] to 40
//!   characters in the latest handoff, matched as a prefix of the published SHA
//!   (hand-written RESULT lines use abbreviations; a token that is not a prefix
//!   of a published SHA never matches).
//! - Coverage: sources published through the rolling merge queue only, and only
//!   the published source SHA or landed tip itself; an Issue naming a commit that
//!   is merely an ancestor of a published source, or that landed outside the
//!   queue, gets no signal.
//! - Idempotent per `(Issue, SHA)`: a replayed settle, a restart or a second
//!   entry for the same SHA records nothing new.

use super::Store;
use crate::error::Result;
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::rolling_queue::{RollingQueueEntryV1, RollingQueueOutcome};
use rusqlite::params;
use serde_json::json;
use uuid::Uuid;

/// Subject prefix of the notice; the Issue's display number follows.
pub const ISSUE_RESULT_LANDED_SUBJECT_PREFIX: &str = "issue_result_landed:";
/// `record_kind` of the notice state.
const ISSUE_RESULT_LANDED: &str = "issue_result_landed";
/// Open/InProgress Issues of one project examined per publication.
const MAX_OPEN_ISSUES_SCANNED: i64 = 256;
/// Named SHAs read from one handoff.
const MAX_NAMED_SHAS: usize = 64;
/// Shortest abbreviation accepted as a named commit.
const MIN_NAMED_SHA: usize = 9;

/// The commits a handoff names: lowercase hex tokens of [`MIN_NAMED_SHA`] to 40
/// characters, in order of appearance, deduplicated.
fn named_shas(handoff: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    for token in handoff.split(|c: char| !c.is_ascii_alphanumeric()) {
        if found.len() == MAX_NAMED_SHAS {
            break;
        }
        let is_sha = (MIN_NAMED_SHA..=40).contains(&token.len())
            && token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if is_sha && !found.contains(&token) {
            found.push(token);
        }
    }
    found
}

impl Store {
    /// Record the settle of a `published` entry: one notice for each Open or
    /// InProgress Issue of `project_id` whose latest handoff names the entry's
    /// source commit or landed tip. Runs on the caller's open transaction.
    pub(crate) fn note_issue_result_landed_in_tx(
        &self,
        project_id: Uuid,
        entry: &RollingQueueEntryV1,
        outcome: &RollingQueueOutcome,
    ) {
        if let Err(error) = self.record_issue_result_landed(project_id, entry, outcome) {
            tracing::warn!(%error, entry = %entry.id, "issue_result_landed notice not recorded");
        }
    }

    fn record_issue_result_landed(
        &self,
        project_id: Uuid,
        entry: &RollingQueueEntryV1,
        outcome: &RollingQueueOutcome,
    ) -> Result<()> {
        let Some(config) = self.get_harness_manager_notice_config(project_id)? else {
            return Ok(());
        };
        if config.current_session_id.is_none()
            || config.is_revoked()
            || self
                .get_harness_manager_policy(project_id)?
                .is_none_or(|policy| policy.revoked)
        {
            return Ok(());
        }
        let published: Vec<&str> = [
            Some(entry.source_commit.as_str()),
            outcome.landed_sha.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut stmt = self.conn.prepare(
            "SELECT i.display_number, i.title, i.status,
                    (SELECT h.body FROM issue_handoffs h
                     WHERE h.issue_id=i.id ORDER BY h.sequence DESC LIMIT 1)
             FROM issues i
             WHERE i.project_id=?1 AND i.status IN ('Open','InProgress')
               AND i.archived_at IS NULL
             ORDER BY i.display_number DESC LIMIT ?2",
        )?;
        let candidates = stmt
            .query_map(
                params![project_id.to_string(), MAX_OPEN_ISSUES_SCANNED],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for (number, title, status, handoff) in candidates {
            let Some(handoff) = handoff else {
                continue;
            };
            let named = named_shas(&handoff)
                .into_iter()
                .find(|named| published.iter().any(|sha| sha.starts_with(named)));
            let Some(named) = named else {
                continue;
            };
            self.insert_issue_result_landed_notice(
                &config, number, &title, &status, named, entry, outcome,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_issue_result_landed_notice(
        &self,
        config: &HarnessManagerConfigV1,
        number: i64,
        title: &str,
        status: &str,
        named: &str,
        entry: &RollingQueueEntryV1,
        outcome: &RollingQueueOutcome,
    ) -> Result<()> {
        let (job_id, _, _) = self.manager_action_watch_identity(config);
        let subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{number}");
        let version = entry.source_commit.as_str();
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE job_id=?1 AND kind='ledger_change' AND subject_id=?2 AND subject_version=?3)",
            params![job_id.to_string(), subject, version],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(());
        }
        self.ensure_manager_action_watch(config, version)?;
        let recorded_at = super::harness_manager_v2::now();
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?4,'ledger_change',?6,?7,?8,?9,?9)",
            params![
                Uuid::new_v4().to_string(),
                job_id.to_string(),
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                subject,
                version,
                serde_json::to_string(&json!({
                    "record_kind": ISSUE_RESULT_LANDED,
                    "issue_number": number,
                    "title": title,
                    "status": status,
                    "named_sha": named,
                    "source_commit": entry.source_commit,
                    "landed_sha": outcome.landed_sha,
                    "queue_entry_id": entry.id,
                    "detail": format!(
                        "Issue #{number} ({status}) names commit {named}, which the rolling \
                         merge queue just published (source {source}, rolling tip {tip}). The \
                         Issue was NOT closed: its RESULT can be partial. Check the handoff and \
                         close #{number} in one call if the work is complete.",
                        source = entry.source_commit,
                        tip = outcome.landed_sha.as_deref().unwrap_or("unknown"),
                    ),
                }))?,
                recorded_at,
            ],
        )?;
        self.refresh_manager_notice_job(job_id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::manager_coordinator::tests::fixture;
    use chrono::Utc;
    use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerPolicyV2};
    use rsi_common::rolling_queue::{RollingQueueBinding, RollingQueueEntryState};
    use rsi_common::types::{IssueStatus, NewIssue};

    use crate::store::rolling_queue::NewQueueEntry;

    fn policy() -> ManagerPolicyV2 {
        ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::WorkPlan],
            ..Default::default()
        }
    }

    fn issue_with_handoff(store: &Store, project: Uuid, handoff: &str) -> (Uuid, i64) {
        let issue = store
            .create_issue(&NewIssue {
                project_id: project,
                title: "landed work".into(),
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
        let author = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO issue_handoffs
                 (issue_id,sequence,project_id,author_session_id,idempotency_key,body,created_at)
                 VALUES(?1,1,?2,?3,'k',?4,?5)",
                params![
                    issue.id.to_string(),
                    project.to_string(),
                    author.to_string(),
                    handoff,
                    super::super::harness_manager_v2::now(),
                ],
            )
            .unwrap();
        (issue.id, issue.display_number)
    }

    /// Enqueue one source, claim it, and settle it published at `landed`.
    fn publish(store: &Store, project: Uuid, owner: Uuid, commit: char, landed: &str) {
        let key = format!("k-{commit}");
        store
            .enqueue_rolling_queue_source(
                &NewQueueEntry {
                    project_id: Some(project),
                    repo_path: "/tmp/repo".into(),
                    source_commit: commit.to_string().repeat(40),
                    source_session_id: owner,
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: Vec::new(),
                    test_filters: vec!["rsid=x".into()],
                    fixes_tests: Vec::new(),
                    idempotency_key: key,
                },
                Utc::now(),
            )
            .unwrap();
        let batch = store
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        let outcome = RollingQueueOutcome {
            landed_sha: Some(landed.into()),
            ..RollingQueueOutcome::default()
        };
        for entry in &batch.entries {
            store
                .settle_rolling_queue_entry(
                    entry.id,
                    RollingQueueEntryState::Published,
                    &outcome,
                    Utc::now(),
                )
                .unwrap();
        }
    }

    fn notices(store: &Store, subject: &str) -> Vec<(String, serde_json::Value)> {
        let mut stmt = store
            .conn
            .prepare(
                "SELECT subject_version,state_json FROM harness_manager_notices
                 WHERE kind='ledger_change' AND subject_id=?1 ORDER BY sequence",
            )
            .unwrap();
        stmt.query_map([subject], |row| {
            Ok((
                row.get::<_, String>(0)?,
                serde_json::from_str(&row.get::<_, String>(1)?).unwrap(),
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn named_shas_reads_full_and_abbreviated_commits_only() {
        let text = "RESULT 6f957021b ok; full 0123456789abcdef0123456789abcdef01234567 \
                    word defaced1 ab12 deadbeefcafe 6f957021b";
        assert_eq!(
            named_shas(text),
            vec![
                "6f957021b",
                "0123456789abcdef0123456789abcdef01234567",
                "deadbeefcafe"
            ]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_open_issue_naming_a_landed_sha_gets_one_notice_and_stays_open_2911() {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let project = config.project_id;
        let owner = config.manager_session_id;
        let named = "a".repeat(40);
        let (issue_id, number) =
            issue_with_handoff(&store, project, &format!("RESULT {} green", &named[..12]));
        let (_, unrelated) = issue_with_handoff(&store, project, "RESULT 0123456789ab green");
        let subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{number}");
        let other = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{unrelated}");

        // An unrelated SHA landing signals nothing.
        publish(&store, project, owner, 'b', &"b".repeat(40));
        assert!(notices(&store, &subject).is_empty());
        assert!(notices(&store, &other).is_empty());

        // The named SHA landing signals exactly the naming Issue, once.
        publish(&store, project, owner, 'a', &"c".repeat(40));
        let rows = notices(&store, &subject);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, named);
        let state = &rows[0].1;
        assert_eq!(state["record_kind"], ISSUE_RESULT_LANDED);
        assert_eq!(state["issue_number"], number);
        assert_eq!(state["named_sha"], named[..12]);
        assert_eq!(state["source_commit"], named);
        assert_eq!(state["landed_sha"], "c".repeat(40));
        assert!(notices(&store, &other).is_empty());

        // A replay of the same landing records nothing more.
        let entry = store
            .get_rolling_queue_entry_by_key(owner, "k-a")
            .unwrap()
            .unwrap();
        store.note_issue_result_landed_in_tx(project, &entry, &entry.outcome.clone().unwrap());
        assert_eq!(notices(&store, &subject).len(), 1);

        // The Issue itself was never touched.
        let issue = store.get_issue(issue_id).unwrap().unwrap();
        assert_eq!(issue.status, IssueStatus::Open);
    }
}
