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
//!   (a primary-key lookup), and one typed result SHA from it, compared against
//!   the entry's source commit and landed tip.
//! - Hub #3794: only typed sources raise the notice. A "named commit" is the
//!   SHA of the `RESULT <sha> issue=#<n>` line of the Issue's latest handoff,
//!   and only when that handoff is a `PIPELINE HANDOFF — IMPLEMENTATION:` (or
//!   `FIX`) report on its first nonblank line; it is matched as a prefix of the
//!   published SHA (a token that is not a prefix of a published SHA never
//!   matches). A hex token in any other handoff (a PM evidence note citing a
//!   SHA) is prose and names nothing. The other typed source is the queue
//!   entry's own `issue` field (`matched_by` `queue_entry_issue`). A
//!   `fixes_issue` claim is not persisted on the entry, so it raises no notice
//!   here. No Issue body is scanned.
//! - Coverage: sources published through the rolling merge queue only, and only
//!   the published source SHA or landed tip itself; an Issue naming a commit that
//!   is merely an ancestor of a published source, or that landed outside the
//!   queue, gets no signal.
//! - Idempotent per `(Issue, published source SHA)`: a replayed settle, a
//!   restart or a second entry for the same SHA records nothing new.
//! - Hub #3369, the reverse direction: the commit names the Issue and the Issue
//!   names no commit. The daemon reads the subjects of the commits the entry
//!   newly published (`RollingQueueOutcome::published_commits`, not stored) and
//!   each `hub #N`, `hub#N`, `(#N)`, `fixes #N` (also `fix`, `closes`,
//!   `resolves`, and a `,`/`&`/`and`-chained list after one of them) that names an
//!   Open or InProgress Issue of the project raises the same notice, naming the
//!   commit. At most [`MAX_SUBJECT_REFS`] distinct numbers are looked up, each
//!   by its display number. An Issue the handoff scan already noticed for this
//!   entry gets no second notice. Never auto-closed: a commit can carry one
//!   slice of a multi-slice Issue.
//! - #600 (hub #3604): a source a satellite reported (a `satellite_enqueue_intakes`
//!   row names the entry or the source commit) carries that project's Issue
//!   numbers, so only the explicit `hub #N` / `hub#N` forms name a hub Issue;
//!   an unqualified `(#N)` / `fixes #N` names nothing.
//! - Hub #3794 (the satellite marker, decided from the code): a commit is
//!   satellite-origin when its entry is satellite-sourced (an intake row names the
//!   entry or its source commit) or when the commit itself is the source of a
//!   `satellite_enqueue_intakes` row, i.e. a satellite's commit merged into a hub
//!   entry's range. Only intake rows mark it; the daemon has no commit author or
//!   remote to read. A satellite-sourced entry's own `issue` names no hub Issue.

use super::Store;
use crate::error::Result;
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::rolling_queue::{PublishedCommit, RollingQueueEntryV1, RollingQueueOutcome};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use uuid::Uuid;

/// Subject prefix of the notice; the Issue's display number follows.
pub const ISSUE_RESULT_LANDED_SUBJECT_PREFIX: &str = "issue_result_landed:";
/// `record_kind` of the notice state.
const ISSUE_RESULT_LANDED: &str = "issue_result_landed";
/// Open/InProgress Issues of one project examined per publication.
const MAX_OPEN_ISSUES_SCANNED: i64 = 256;
/// Shortest abbreviation accepted as a typed result commit.
const MIN_NAMED_SHA: usize = 9;
/// Distinct Issue numbers read from the subjects of one publication.
const MAX_SUBJECT_REFS: usize = 64;

/// How a published commit relates to the Issue a notice names.
enum Named<'a> {
    /// The Issue's latest handoff is a typed `RESULT` report naming this
    /// (abbreviated) commit.
    Handoff(&'a str),
    /// The queue entry was enqueued with this Issue's number (`issue` field);
    /// the string is the entry's source commit.
    Entry(&'a str),
    /// The subject of a commit the entry published names the Issue.
    Subject(&'a PublishedCommit),
}

/// The Issue numbers a commit subject names, in order of appearance and
/// deduplicated: `hub #N`, `hub#N`, `(#N)`, `fixes #N` (also `fix`, `fixed`,
/// `closes`, `resolves`), and a list continued after one of those with `,`,
/// `/`, `&` or `and` (`hub#3086,#3087`). A bare `#N` elsewhere names nothing.
/// `hub_only` (#600, hub #3604): the commit comes from a satellite, whose
/// unqualified `(#N)` / `fixes #N` are that project's numbers, so only the
/// explicit `hub #N` / `hub#N` forms (and lists chained after them) count.
fn subject_issue_refs(subject: &str, hub_only: bool) -> Vec<i64> {
    const ALL_LEADS: [&str; 7] = [
        "hub ",
        "hub",
        "fixes ",
        "fix ",
        "fixed ",
        "closes ",
        "resolves ",
    ];
    const HUB_LEADS: [&str; 2] = ["hub ", "hub"];
    let leads: &[&str] = if hub_only { &HUB_LEADS } else { &ALL_LEADS };
    let lower = subject.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut found: Vec<i64> = Vec::new();
    // End offset of the previous accepted reference, for a chained list.
    let mut previous_end: Option<usize> = None;
    let mut at = 0;
    while let Some(offset) = lower[at..].find('#') {
        let hash = at + offset;
        let digits_start = hash + 1;
        let digits_end = digits_start
            + bytes[digits_start..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
        at = digits_start;
        if digits_end == digits_start || digits_end - digits_start > 9 {
            continue;
        }
        let before = &lower[..hash];
        let led = (!hub_only && before.ends_with('('))
            || leads.iter().any(|lead| {
                before.ends_with(lead)
                    && before[..before.len() - lead.len()]
                        .chars()
                        .next_back()
                        .is_none_or(|c| !c.is_ascii_alphanumeric())
            });
        let chained = previous_end.is_some_and(|end| {
            let between = lower[end..hash].trim();
            matches!(between, "," | "/" | "&" | "and" | ", and" | ",and")
        });
        if !(led || chained) {
            continue;
        }
        if let Ok(number) = lower[digits_start..digits_end].parse::<i64>() {
            if !found.contains(&number) {
                found.push(number);
            }
        }
        previous_end = Some(digits_end);
        at = digits_end;
    }
    found
}

/// Hub #3794: the one commit a worker's typed final report names. Only a
/// `PIPELINE HANDOFF — IMPLEMENTATION:` (or `FIX`) report counts, on its first
/// nonblank line, and only the SHA of its `RESULT <sha> issue=#<n> ...` line; an
/// `issue=#<n>` there must be `number`, the Issue the report was published to.
/// A hex token anywhere else (a PM evidence note, a review, a quoted log) is
/// prose, not a typed result, and names nothing.
fn typed_result_sha(handoff: &str, number: i64) -> Option<&str> {
    let mut lines = handoff.lines().filter(|line| !line.trim().is_empty());
    let first = lines
        .next()?
        .trim_start_matches(|c: char| c == '*' || c == '`' || c.is_whitespace());
    let stage = first
        .strip_prefix("PIPELINE HANDOFF")?
        .trim_start_matches(|c: char| c == '—' || c == '-' || c == ':' || c.is_whitespace())
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()?;
    if !matches!(stage, "IMPLEMENTATION" | "FIX") {
        return None;
    }
    let result = lines.find_map(|line| line.trim().strip_prefix("RESULT "))?;
    let mut words = result.split_whitespace();
    let sha = words.next()?;
    let is_sha = (MIN_NAMED_SHA..=40).contains(&sha.len())
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !is_sha {
        return None;
    }
    let other_issue = words
        .filter_map(|word| word.strip_prefix("issue=#"))
        .any(|n| n.parse::<i64>().ok() != Some(number));
    (!other_issue).then_some(sha)
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
        let mut noticed: Vec<i64> = Vec::new();
        for (number, title, status, handoff) in candidates {
            let Some(handoff) = handoff else {
                continue;
            };
            let named = typed_result_sha(&handoff, number)
                .filter(|named| published.iter().any(|sha| sha.starts_with(named)));
            let Some(named) = named else {
                continue;
            };
            self.insert_issue_result_landed_notice(
                &config,
                number,
                &title,
                &status,
                Named::Handoff(named),
                entry,
                outcome,
            )?;
            noticed.push(number);
        }
        if let Some(number) = self.record_entry_issue(&config, entry, outcome, &noticed)? {
            noticed.push(number);
        }
        self.record_subject_named_issues(&config, entry, outcome, &noticed)
    }

    /// Hub #3794: the typed link of a queue entry. An entry enqueued with an
    /// `issue` that is an Open or InProgress Issue of the project signals that
    /// Issue, naming the entry's source commit. A satellite-reported source
    /// carries its origin project's number there, so it names no hub Issue.
    fn record_entry_issue(
        &self,
        config: &HarnessManagerConfigV1,
        entry: &RollingQueueEntryV1,
        outcome: &RollingQueueOutcome,
        noticed: &[i64],
    ) -> Result<Option<i64>> {
        let Some(number) = entry.issue else {
            return Ok(None);
        };
        if noticed.contains(&number)
            || self.entry_is_satellite_sourced(entry.id, &entry.source_commit)?
        {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                "SELECT title, status FROM issues
                 WHERE project_id=?1 AND display_number=?2
                   AND status IN ('Open','InProgress') AND archived_at IS NULL",
                params![config.project_id.to_string(), number],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((title, status)) = row else {
            return Ok(None);
        };
        self.insert_issue_result_landed_notice(
            config,
            number,
            &title,
            &status,
            Named::Entry(&entry.source_commit),
            entry,
            outcome,
        )?;
        Ok(Some(number))
    }

    /// #3369: one notice per Open or InProgress Issue that a published commit's
    /// subject names and the handoff scan did not already signal.
    fn record_subject_named_issues(
        &self,
        config: &HarnessManagerConfigV1,
        entry: &RollingQueueEntryV1,
        outcome: &RollingQueueOutcome,
        noticed: &[i64],
    ) -> Result<()> {
        // Issue number -> the commits whose subjects name it (newest first).
        let mut named: Vec<(i64, Vec<&PublishedCommit>)> = Vec::new();
        let entry_hub_only = self.entry_is_satellite_sourced(entry.id, &entry.source_commit)?;
        for commit in &outcome.published_commits {
            // A satellite's commit merged by a hub entry is satellite-origin too.
            let hub_only = entry_hub_only || self.commit_is_satellite_reported(&commit.sha)?;
            for number in subject_issue_refs(&commit.subject, hub_only) {
                if noticed.contains(&number) {
                    continue;
                }
                if let Some(position) = named.iter().position(|(known, _)| *known == number) {
                    named[position].1.push(commit);
                } else if named.len() < MAX_SUBJECT_REFS {
                    named.push((number, vec![commit]));
                }
            }
        }
        for (number, commits) in named {
            let row = self
                .conn
                .query_row(
                    "SELECT title, status FROM issues
                     WHERE project_id=?1 AND display_number=?2
                       AND status IN ('Open','InProgress') AND archived_at IS NULL",
                    params![config.project_id.to_string(), number],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            let Some((title, status)) = row else {
                continue;
            };
            self.insert_issue_result_landed_notice(
                config,
                number,
                &title,
                &status,
                Named::Subject(commits[0]),
                entry,
                outcome,
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
        named: Named<'_>,
        entry: &RollingQueueEntryV1,
        outcome: &RollingQueueOutcome,
    ) -> Result<()> {
        let (job_id, _, _) = self.manager_action_watch_identity(config);
        let subject_id = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{number}");
        let version = entry.source_commit.as_str();
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE job_id=?1 AND kind='ledger_change' AND subject_id=?2 AND subject_version=?3)",
            params![job_id.to_string(), subject_id, version],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(());
        }
        self.ensure_manager_action_watch(config, version)?;
        let recorded_at = chrono::Utc::now();
        let (named_sha, matched_by, subject, how) = match named {
            Named::Handoff(sha) => (
                sha,
                "handoff",
                None,
                format!("has a typed RESULT report naming commit {sha}"),
            ),
            Named::Entry(sha) => (
                sha,
                "queue_entry_issue",
                None,
                format!("is the linked Issue of the queue entry for commit {sha}"),
            ),
            Named::Subject(commit) => (
                commit.sha.as_str(),
                "commit_subject",
                Some(commit.subject.as_str()),
                format!("is named by the subject of commit {}", commit.sha),
            ),
        };
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
                subject_id,
                version,
                serde_json::to_string(&json!({
                    "record_kind": ISSUE_RESULT_LANDED,
                    "digest_until_unix": recorded_at.timestamp()
                        + super::manager_notices::MANAGER_NOTICE_DIGEST_SECS,
                    "issue_number": number,
                    "title": title,
                    "status": status,
                    "named_sha": named_sha,
                    "matched_by": matched_by,
                    "commit_subject": subject,
                    "source_commit": entry.source_commit,
                    "landed_sha": outcome.landed_sha,
                    "queue_entry_id": entry.id,
                    "detail": format!(
                        "Issue #{number} ({status}) {how}, which the rolling merge queue just \
                         published (source {source}, rolling tip {tip}). The Issue was NOT \
                         closed: a commit or RESULT can be partial. Check it and close \
                         #{number} in one call if the work is complete.",
                        source = entry.source_commit,
                        tip = outcome.landed_sha.as_deref().unwrap_or("unknown"),
                    ),
                }))?,
                recorded_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
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
        publish_with(store, project, owner, commit, landed, Vec::new());
    }

    fn publish_with(
        store: &Store,
        project: Uuid,
        owner: Uuid,
        commit: char,
        landed: &str,
        published_commits: Vec<PublishedCommit>,
    ) {
        publish_linked(
            store,
            project,
            owner,
            commit,
            landed,
            published_commits,
            None,
        );
    }

    fn publish_linked(
        store: &Store,
        project: Uuid,
        owner: Uuid,
        commit: char,
        landed: &str,
        published_commits: Vec<PublishedCommit>,
        issue: Option<i64>,
    ) {
        let key = format!("k-{commit}");
        store
            .enqueue_rolling_queue_source_linked(
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
                None,
                None,
                false,
                issue,
                Utc::now(),
            )
            .unwrap();
        let batch = store
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        let outcome = RollingQueueOutcome {
            landed_sha: Some(landed.into()),
            published_commits,
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
    fn typed_result_sha_reads_only_the_result_line_of_a_pipeline_handoff_3794() {
        let report = |first: &str, result: &str| format!("{first}\n\n{result}\nFriction: none");
        let full = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            typed_result_sha(
                &report(
                    "PIPELINE HANDOFF — IMPLEMENTATION:",
                    "RESULT 6f957021b issue=#12 tests=\"x\" status=green"
                ),
                12
            ),
            Some("6f957021b")
        );
        assert_eq!(
            typed_result_sha(
                &report("**PIPELINE HANDOFF — FIX:**", &format!("RESULT {full}")),
                12
            ),
            Some(full)
        );
        // The report's own issue= must be the Issue it was published to.
        assert_eq!(
            typed_result_sha(
                &report(
                    "PIPELINE HANDOFF — IMPLEMENTATION:",
                    "RESULT 6f957021b issue=#13"
                ),
                12
            ),
            None
        );
        // Prose that merely cites a SHA is no typed result, whatever it says.
        assert_eq!(
            typed_result_sha(
                "PM evidence: laptop #578 landed 29c70df01 (RESULT 29c70df01)",
                12
            ),
            None
        );
        assert_eq!(
            typed_result_sha(
                "RESULT 6f957021b ok; full 0123456789abcdef0123456789abcdef01234567",
                12
            ),
            None
        );
        // A review report, a short or non-hex token, and a body SHA are not it.
        assert_eq!(
            typed_result_sha(
                &report("PIPELINE HANDOFF — REVIEW:", "RESULT 6f957021b issue=#12"),
                12
            ),
            None
        );
        assert_eq!(
            typed_result_sha(
                &report(
                    "PIPELINE HANDOFF — IMPLEMENTATION:",
                    "RESULT ab12 issue=#12"
                ),
                12
            ),
            None
        );
        assert_eq!(
            typed_result_sha(
                "PIPELINE HANDOFF — IMPLEMENTATION:\nSee deadbeefcafe in the log.",
                12
            ),
            None
        );
    }

    fn typed_handoff(sha: &str) -> String {
        format!("PIPELINE HANDOFF — IMPLEMENTATION:\nRESULT {sha} tests=\"x\" status=green")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_open_issue_naming_a_landed_sha_gets_one_notice_and_stays_open_2911() {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let project = config.project_id;
        let owner = config.manager_session_id;
        let named = "a".repeat(40);
        let (issue_id, number) = issue_with_handoff(&store, project, &typed_handoff(&named[..12]));
        let (_, unrelated) = issue_with_handoff(&store, project, &typed_handoff("0123456789ab"));
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
        let (recorded_at, deadline): (String, i64) = store
            .conn
            .query_row(
                "SELECT recorded_at,json_extract(state_json,'$.digest_until_unix')
             FROM harness_manager_notices WHERE subject_id=?1",
                [&subject],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            deadline
                - chrono::DateTime::parse_from_rfc3339(&recorded_at)
                    .unwrap()
                    .timestamp(),
            1800
        );
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn subject_issue_refs_reads_the_conventional_forms_only_3369() {
        assert_eq!(
            subject_issue_refs("feat(hub #3011): add x", false),
            vec![3011]
        );
        assert_eq!(subject_issue_refs("Hub#3015 follow-up", false), vec![3015]);
        assert_eq!(
            subject_issue_refs("fix(sandbox reclaim hub#3086,#3087)", false),
            vec![3086, 3087]
        );
        assert_eq!(subject_issue_refs("fix: crash (#12)", false), vec![12]);
        assert_eq!(
            subject_issue_refs("fixes #7 and #8, closes #9", false),
            vec![7, 8, 9]
        );
        assert_eq!(subject_issue_refs("hub #5 hub #5", false), vec![5]);
        // A bare number, a word ending in "hub", and a long number name nothing.
        assert!(subject_issue_refs("see #44 for context", false).is_empty());
        assert!(subject_issue_refs("github #44", false).is_empty());
        assert!(subject_issue_refs("hub #12345678901", false).is_empty());
        assert!(subject_issue_refs("rsi integration: merge abc into rolling", false).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_satellite_subject_names_only_explicit_hub_issues_600() {
        // Unqualified forms are the satellite project's own numbers.
        assert!(subject_issue_refs("fix(#557): redact values", true).is_empty());
        assert!(subject_issue_refs("fixes #7 and #8", true).is_empty());
        assert!(subject_issue_refs("fix: crash (#12)", true).is_empty());
        // The explicit hub forms, and a list chained after one, still name hub Issues.
        assert_eq!(subject_issue_refs("feat(hub #3011): x", true), vec![3011]);
        assert_eq!(subject_issue_refs("Hub#3015 follow-up", true), vec![3015]);
        assert_eq!(
            subject_issue_refs("fix (hub#3086,#3087)", true),
            vec![3086, 3087]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_satellite_sourced_commit_does_not_notice_the_hub_issue_with_its_local_number_600() {
        use rsi_common::satellite_intake::parse_enqueue_claim;

        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let project = config.project_id;
        let owner = config.manager_session_id;
        let (_, local_number) = issue_with_handoff(&store, project, "no commit named here");
        let (_, mapped_number) = issue_with_handoff(&store, project, "no commit named here");
        let local_subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{local_number}");
        let mapped_subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{mapped_number}");
        let commit = |sha: char, subject: String| PublishedCommit {
            sha: sha.to_string().repeat(40),
            subject,
        };

        // The satellite reported source 'a'; its foreign `(#N)` is not a hub number.
        let claim =
            parse_enqueue_claim(&format!("ENQUEUE {} branch=rsi/x", "a".repeat(40))).unwrap();
        assert!(
            store
                .insert_enqueue_intake(Uuid::new_v4(), Uuid::new_v4(), project, &claim)
                .unwrap()
        );
        publish_with(
            &store,
            project,
            owner,
            'a',
            &"c".repeat(40),
            vec![
                commit('1', format!("fix(#{local_number}): redact values")),
                commit('2', format!("feat(hub #{mapped_number}): mapped work")),
            ],
        );
        assert!(notices(&store, &local_subject).is_empty());
        let rows = notices(&store, &mapped_subject);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1["matched_by"], "commit_subject");

        // A source no satellite reported keeps the unqualified forms.
        publish_with(
            &store,
            project,
            owner,
            'b',
            &"d".repeat(40),
            vec![commit('3', format!("fix(#{local_number}): hub-local work"))],
        );
        assert_eq!(notices(&store, &local_subject).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_sha_cited_in_a_note_does_not_notice_the_issue_but_a_typed_result_does_3794() {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let project = config.project_id;
        let owner = config.manager_session_id;
        // hub #3775: a PM evidence handoff cited the laptop's landed SHA.
        let (_, cited) = issue_with_handoff(
            &store,
            project,
            &format!(
                "PM evidence: laptop #578 landed {} (not this work)",
                "a".repeat(12)
            ),
        );
        let (_, typed) = issue_with_handoff(&store, project, &typed_handoff(&"a".repeat(12)));
        publish(&store, project, owner, 'a', &"c".repeat(40));
        let cited_subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{cited}");
        let typed_subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{typed}");
        assert!(notices(&store, &cited_subject).is_empty());
        let rows = notices(&store, &typed_subject);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1["matched_by"], "handoff");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_queue_entry_linked_to_an_issue_notices_it_unless_a_satellite_reported_it_3794() {
        use rsi_common::satellite_intake::parse_enqueue_claim;

        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let project = config.project_id;
        let owner = config.manager_session_id;
        let (_, hub) = issue_with_handoff(&store, project, "no commit named here");
        let (_, local) = issue_with_handoff(&store, project, "no commit named here");
        let hub_subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{hub}");
        let local_subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{local}");

        // The typed `issue` field of a hub entry names the Issue.
        publish_linked(
            &store,
            project,
            owner,
            'a',
            &"c".repeat(40),
            Vec::new(),
            Some(hub),
        );
        let rows = notices(&store, &hub_subject);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1["matched_by"], "queue_entry_issue");
        assert_eq!(rows[0].1["named_sha"], "a".repeat(40));

        // A satellite-reported source carries its own project's number there.
        let claim =
            parse_enqueue_claim(&format!("ENQUEUE {} branch=rsi/x", "b".repeat(40))).unwrap();
        assert!(
            store
                .insert_enqueue_intake(Uuid::new_v4(), Uuid::new_v4(), project, &claim)
                .unwrap()
        );
        publish_linked(
            &store,
            project,
            owner,
            'b',
            &"d".repeat(40),
            Vec::new(),
            Some(local),
        );
        assert!(notices(&store, &local_subject).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_satellite_commit_merged_by_a_hub_entry_names_no_hub_issue_3794() {
        use rsi_common::satellite_intake::parse_enqueue_claim;

        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let project = config.project_id;
        let owner = config.manager_session_id;
        // hub #579: the laptop's own #579 landed in a hub entry's range.
        let (_, hub_579) = issue_with_handoff(&store, project, "no commit named here");
        let subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{hub_579}");
        let claim =
            parse_enqueue_claim(&format!("ENQUEUE {} branch=rsi/x", "e".repeat(40))).unwrap();
        assert!(
            store
                .insert_enqueue_intake(Uuid::new_v4(), Uuid::new_v4(), project, &claim)
                .unwrap()
        );
        let satellite = |sha: char| PublishedCommit {
            sha: sha.to_string().repeat(40),
            subject: format!("fix: satellite work (#{hub_579})"),
        };
        // The entry itself is a hub source; only the published 'e' commit is the satellite's.
        publish_with(
            &store,
            project,
            owner,
            'a',
            &"c".repeat(40),
            vec![satellite('e')],
        );
        assert!(notices(&store, &subject).is_empty());
        // The same subject on a commit no satellite reported keeps its meaning.
        publish_with(
            &store,
            project,
            owner,
            'b',
            &"d".repeat(40),
            vec![satellite('f')],
        );
        assert_eq!(notices(&store, &subject).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_published_commit_subject_naming_an_open_issue_gets_one_notice_3369() {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let project = config.project_id;
        let owner = config.manager_session_id;
        let (open_id, open_number) = issue_with_handoff(&store, project, "no commit named here");
        let (_, other_number) = issue_with_handoff(&store, project, "no commit named here");
        let (closed_id, closed_number) = issue_with_handoff(&store, project, "done");
        store
            .conn
            .execute(
                "UPDATE issues SET status='Closed' WHERE id=?1",
                [closed_id.to_string()],
            )
            .unwrap();
        let open_subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{open_number}");
        let other_subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{other_number}");
        let closed_subject = format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{closed_number}");
        let commit = |sha: char, subject: String| PublishedCommit {
            sha: sha.to_string().repeat(40),
            subject,
        };

        publish_with(
            &store,
            project,
            owner,
            'a',
            &"c".repeat(40),
            vec![
                commit('1', format!("feat(hub #{open_number}): second slice")),
                commit('2', format!("feat(hub #{open_number}): first slice")),
                commit('3', format!("fix (hub#{closed_number}) already closed")),
                commit('4', "fix(hub #999999): no such Issue".into()),
                commit('5', format!("chore: mention #{other_number} in passing")),
            ],
        );

        let rows = notices(&store, &open_subject);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let state = &rows[0].1;
        assert_eq!(state["record_kind"], ISSUE_RESULT_LANDED);
        assert_eq!(state["issue_number"], open_number);
        assert_eq!(state["matched_by"], "commit_subject");
        assert_eq!(state["named_sha"], "1".repeat(40));
        assert_eq!(state["source_commit"], "a".repeat(40));
        assert!(
            state["detail"].as_str().unwrap().contains(&"1".repeat(40)),
            "{state}"
        );
        // A closed Issue, an unknown number and a bare mention signal nothing.
        assert!(notices(&store, &closed_subject).is_empty());
        assert!(notices(&store, &other_subject).is_empty());

        // A replay of the same landing records nothing more, and the Issue
        // is not closed.
        let entry = store
            .get_rolling_queue_entry_by_key(owner, "k-a")
            .unwrap()
            .unwrap();
        let mut again = entry.outcome.clone().unwrap();
        again.published_commits = vec![commit('1', format!("feat(hub #{open_number}): x"))];
        store.note_issue_result_landed_in_tx(project, &entry, &again);
        assert_eq!(notices(&store, &open_subject).len(), 1);
        let issue = store.get_issue(open_id).unwrap().unwrap();
        assert_eq!(issue.status, IssueStatus::Open);

        // A later publication of another slice signals again.
        publish_with(
            &store,
            project,
            owner,
            'b',
            &"d".repeat(40),
            vec![commit('6', format!("fix(hub #{open_number}): third slice"))],
        );
        assert_eq!(notices(&store, &open_subject).len(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_issue_the_handoff_scan_noticed_gets_no_second_subject_notice_3369() {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let project = config.project_id;
        let owner = config.manager_session_id;
        let named = "a".repeat(12);
        let (_, number) = issue_with_handoff(&store, project, &typed_handoff(&named));
        publish_with(
            &store,
            project,
            owner,
            'a',
            &"c".repeat(40),
            vec![PublishedCommit {
                sha: "a".repeat(40),
                subject: format!("feat(hub #{number}): the work"),
            }],
        );
        let rows = notices(
            &store,
            &format!("{ISSUE_RESULT_LANDED_SUBJECT_PREFIX}{number}"),
        );
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1["matched_by"], "handoff");
    }
}
