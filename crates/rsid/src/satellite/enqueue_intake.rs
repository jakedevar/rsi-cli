//! Hub #3345: the daemon takes in satellite ENQUEUE reports itself.
//!
//! A satellite manager reports `ENQUEUE <sha> ...`; the report is recorded with
//! one `satellite_enqueue_intakes` row (`Store::record_satellite_report`). This
//! loop then does what the hub manager used to relay by hand:
//!
//! 1. fetch the commit from `origin` (not fetchable: refuse, tell the satellite);
//! 2. a source already on rolling is settled at once;
//! 3. probe it against the rolling tip and every queued entry. A conflict is
//!    answered to the satellite as a typed `conflicts_with` (entry, branch,
//!    paths) and nothing is enqueued; a conflict that only touches schema
//!    migration bookkeeping of a source declaring a provisional migration is
//!    renumbered by the lander and does not count (#3336);
//! 4. a source AGENTS.md reserves a pre-merge review for (a migration, a
//!    credential, IAM or network-exposure path, or anything git cannot show) is
//!    held for the hub manager (`needs_review`), never enqueued by the daemon;
//! 5. otherwise it is enqueued with `derive_filters`, as the project's current
//!    manager seat (the same admission as the manager's own call), so the
//!    queue's gates are unchanged;
//! 6. when the entry settles, the satellite is told (published, refused, or
//!    withdrawn), each reply once.
//!
//! The hub manager sees one digest notice per intake state and is woken only
//! for what needs a decision: a held review, a refusal, a stall, or a source
//! that wants a post-land review. Override: the operator setting
//! `rolling_queue_enabled` off holds every source (`needs_review`,
//! `queue_disabled`), and `AgentCancelRollingQueueEntry` withdraws a queued one
//! (the satellite is told it was withdrawn). A report the grammar does not
//! parse is never touched: it stays a plain manager notice.

use crate::config::RuntimeConfig;
use crate::session::agent_verbs::AgentControlHandle;
use crate::store::Store;
use crate::topology::review_policy::{Candidate, ReviewPolicy, classify_paths};
use rsi_common::rolling_queue::{
    AgentEnqueueLandingSourceRequestV1, QUEUE_CONFLICT_SCAN_INCOMPLETE, QUEUE_CONFLICTS_WITH_ENTRY,
    QUEUE_CONFLICTS_WITH_ROLLING, RollingQueueEntryState,
};
use rsi_common::satellite::SatelliteUuidV1;
use rsi_common::satellite_dispatch::AgentSendSatelliteMessageRequestV1;
use rsi_common::satellite_intake::{
    INTAKE_MAX_ATTEMPTS, IntakeConflictV1, IntakeStateV1, conflict_is_renumberable,
    conflicts_reply, intake_key, needs_review_reply, refused_reply, settled_reply,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use uuid::Uuid;

/// Seconds between passes.
const PASS_EVERY: Duration = Duration::from_secs(10);
/// Intakes decided or watched per pass, and replies sent per pass.
const PASS_BATCH: usize = 8;
/// The fetch of one source, whole.
const FETCH_BUDGET: Duration = Duration::from_secs(60);
/// The conflict scan of one source, whole.
const CONFLICT_BUDGET: Duration = Duration::from_secs(30);
/// Live queue entries read for the conflict scan. Above the scanner's own probe
/// cap on purpose: entries it cannot probe are reported as an incomplete scan,
/// which the intake records with the entry (#3499) instead of retrying it.
const LIVE_QUEUE_READ_LIMIT: usize = 1024;

type SharedStore = Arc<rsid_core::store_lock::Mutex<Store>>;

/// Run the intake for the life of the daemon.
pub(crate) async fn run_enqueue_intake_loop(
    store: SharedStore,
    control: AgentControlHandle,
    runtime_config: Arc<RuntimeConfig>,
) {
    let mut ticker = tokio::time::interval(PASS_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let queue_enabled = runtime_config.rolling_queue_enabled.load(Ordering::Relaxed);
        pass(&store, &control, queue_enabled).await;
    }
}

/// One pass: decide new intakes, watch enqueued ones, send owed replies.
pub(crate) async fn pass(store: &SharedStore, control: &AgentControlHandle, queue_enabled: bool) {
    let open = match store.lock().await.open_enqueue_intakes(PASS_BATCH) {
        Ok(open) => open,
        Err(error) => {
            tracing::warn!("satellite enqueue intake read failed: {error}");
            return;
        }
    };
    for row in open {
        let result = match row.state {
            IntakeStateV1::Received => decide(store, control, &row, queue_enabled).await,
            IntakeStateV1::Enqueued => watch(store, &row).await,
            _ => Ok(()),
        };
        if let Err(reason) = result {
            note_transient(store, &row, &reason).await;
        }
    }
    send_replies(store).await;
}

/// A step that could not finish: count it, and after
/// [`INTAKE_MAX_ATTEMPTS`] hand the source to the hub manager.
async fn note_transient(store: &SharedStore, row: &Row, reason: &str) {
    tracing::warn!(
        peer = %row.peer_id,
        report = %row.report_id,
        reason,
        "satellite enqueue intake step did not finish"
    );
    let guard = store.lock().await;
    let attempts = guard.bump_enqueue_intake_attempts(row).unwrap_or(0);
    if attempts >= INTAKE_MAX_ATTEMPTS && row.state == IntakeStateV1::Received {
        let _ = move_to(
            &guard,
            row,
            IntakeStateV1::NeedsReview,
            None,
            "intake_stalled",
            json!({ "reason": reason }),
            true,
            &format!(
                "the hub could not take in {} after {attempts} attempts ({reason}); enqueue it by hand or ask the satellite to resend",
                row.source_commit
            ),
        );
    }
}

type Row = crate::store::satellite_enqueue_intakes::EnqueueIntakeRow;

/// Move `row` to `to` and record the digest notice, in one critical section.
#[allow(clippy::too_many_arguments)]
fn move_to(
    store: &Store,
    row: &Row,
    to: IntakeStateV1,
    entry: Option<Uuid>,
    code: &str,
    outcome: Value,
    needs_attention: bool,
    detail: &str,
) -> crate::error::Result<bool> {
    if !store.transition_enqueue_intake(row, row.state, to, entry, Some(code), Some(&outcome))? {
        return Ok(false);
    }
    if let Some(updated) = store.get_enqueue_intake(row.peer_id, row.report_id)? {
        if let Err(error) = store.record_enqueue_intake_notice(&updated, needs_attention, detail) {
            tracing::warn!("satellite intake digest notice not recorded: {error}");
        }
    }
    Ok(true)
}

/// Strict, checked prefix of an error message: its stable code.
fn refusal_code(message: &str) -> String {
    message
        .split([':', ' ', '\n'])
        .next()
        .unwrap_or("queue_refused")
        .chars()
        .take(64)
        .collect()
}

fn is_transient_code(code: &str) -> bool {
    matches!(code, "queue_probe_timeout" | "queue_tip_read_timeout")
}

/// The branch of a queued entry the conflict names: what the satellite said
/// for an intake entry, else the entry's source session branch.
fn entry_branch(store: &Store, entry: Uuid) -> Option<(Option<String>, Option<String>)> {
    let queued = store.get_rolling_queue_entry(entry).ok().flatten()?;
    let branch = store
        .enqueue_intake_branch_for_entry(entry)
        .ok()
        .flatten()
        .or_else(|| {
            store
                .get_session(queued.source_session_id)
                .ok()
                .flatten()
                .and_then(|session| session.sandbox_branch)
        });
    Some((branch, Some(queued.source_commit)))
}

/// The advisory note when the conflict scan stopped short of the rolling tip
/// or a queued entry (budget spent, a probe that did not finish, or more live
/// entries than the scanner probes). A definite conflict is decided before this
/// is asked. The scan is advisory (#3499): the lander still merges the entry
/// against rolling and refuses a real conflict, so an incomplete scan is
/// recorded with the entry, never retried as a failure (under a long queue the
/// 30 s budget cannot finish, which stalled every source).
fn incomplete_scan_note(
    warnings: &[rsi_common::rolling_queue::QueueConflictWarningV1],
) -> Option<String> {
    warnings
        .iter()
        .find(|warning| warning.code == QUEUE_CONFLICT_SCAN_INCOMPLETE)
        .map(|warning| warning.detail.clone())
}

/// What a source needs before the queue may take it, from its changed paths.
enum Review {
    /// Verification only.
    Ordinary,
    /// Enqueue, and the manager arranges a post-land review.
    PostLand(&'static str),
    /// Hold for the hub manager.
    PreMerge(String),
}

fn review_of(
    facts: &crate::rolling_queue::SourceFacts,
    declarations: Option<usize>,
    delta: crate::rolling_queue::DeltaRead,
) -> Review {
    use crate::rolling_queue::DeltaRead;
    if facts.migration_version.is_some() {
        return Review::PreMerge("the source carries a schema migration".into());
    }
    match (declarations, delta) {
        (Some(0), DeltaRead::Paths(paths)) => match classify_paths(&paths) {
            Candidate::Ordinary => Review::Ordinary,
            Candidate::Requires {
                policy: ReviewPolicy::PostLand,
                why,
            } => Review::PostLand(why),
            Candidate::Requires { why, .. } => Review::PreMerge(format!("the source {why}")),
            Candidate::Unknown(why) => Review::PreMerge(why.into()),
        },
        (Some(count), _) if count > 0 => {
            Review::PreMerge("the source declares a provisional schema migration".into())
        }
        _ => Review::PreMerge(
            "git could not show what the source changes (migrations or changed paths)".into(),
        ),
    }
}

/// Decide one `received` intake. `Err` is a transient failure (retried).
async fn decide(
    store: &SharedStore,
    control: &AgentControlHandle,
    row: &Row,
    queue_enabled: bool,
) -> Result<(), String> {
    let (manager, repo) = {
        let guard = store.lock().await;
        let manager = guard
            .get_harness_manager(row.project_id)
            .map_err(|error| error.to_string())?
            .and_then(|config| config.current_session_id)
            .ok_or_else(|| "no_live_manager".to_owned())?;
        let session = guard
            .get_session(manager)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "manager_session_missing".to_owned())?;
        let repo: PathBuf = session
            .sandbox_root
            .clone()
            .unwrap_or_else(|| session.working_dir.clone());
        (manager, repo)
    };
    let source = row.source_commit.clone();
    if !queue_enabled {
        let guard = store.lock().await;
        move_to(
            &guard,
            row,
            IntakeStateV1::NeedsReview,
            None,
            "queue_disabled",
            json!({}),
            true,
            "the rolling queue is disabled, so the hub holds every satellite source for the manager",
        )
        .map_err(|error| error.to_string())?;
        return Ok(());
    }

    // 1. fetch.
    let (fetch_repo, fetch_source) = (repo.clone(), source.clone());
    let fetched = tokio::task::spawn_blocking(move || {
        crate::rolling_queue::fetch_origin_source(&fetch_repo, &fetch_source, FETCH_BUDGET)
    })
    .await
    .map_err(|error| error.to_string())?;
    match fetched {
        crate::rolling_queue::OriginFetch::Unknown => return Err("origin_fetch_unknown".into()),
        crate::rolling_queue::OriginFetch::NotOnOrigin => {
            let guard = store.lock().await;
            move_to(
                &guard,
                row,
                IntakeStateV1::Refused,
                None,
                "source_not_on_origin",
                json!({}),
                false,
                "the commit is not on origin; the satellite was told to push it and resend",
            )
            .map_err(|error| error.to_string())?;
            return Ok(());
        }
        crate::rolling_queue::OriginFetch::Fetched => {}
    }

    // 2-4. already landed, conflicts, review class: all git, off the store lock.
    let ahead = {
        let guard = store.lock().await;
        guard
            .live_rolling_queue_sources(Some(row.project_id), LIVE_QUEUE_READ_LIMIT)
            .map_err(|error| error.to_string())?
    };
    let (probe_repo, probe_source) = (repo.clone(), source.clone());
    let probed = tokio::task::spawn_blocking(move || {
        let (_, on_rolling) = crate::rolling_queue::land_position(&probe_repo, &probe_source);
        let warnings = crate::rolling_queue::queue_conflict_warnings_within(
            &probe_repo,
            &probe_source,
            &ahead,
            CONFLICT_BUDGET,
        );
        let provisional =
            crate::rolling_queue::source_declares_provisional(&probe_repo, &probe_source);
        let facts = crate::rolling_queue::derive_source_facts(&probe_repo, &probe_source);
        let declarations =
            crate::rolling_queue::source_provisional_declaration_count(&probe_repo, &probe_source);
        let delta = crate::rolling_queue::source_changed_paths(&probe_repo, &probe_source);
        (
            on_rolling,
            warnings,
            provisional,
            facts,
            declarations,
            delta,
        )
    })
    .await
    .map_err(|error| error.to_string())?;
    let (on_rolling, warnings, provisional, facts, declarations, delta) = probed;
    if on_rolling {
        let guard = store.lock().await;
        move_to(
            &guard,
            row,
            IntakeStateV1::Settled,
            None,
            "already_on_rolling",
            json!({ "landed_sha": source }),
            false,
            "the source is already on rolling",
        )
        .map_err(|error| error.to_string())?;
        return Ok(());
    }
    let mut conflicts: Vec<IntakeConflictV1> = Vec::new();
    {
        let guard = store.lock().await;
        for warning in &warnings {
            let with = match warning.code.as_str() {
                QUEUE_CONFLICTS_WITH_ROLLING => "rolling",
                QUEUE_CONFLICTS_WITH_ENTRY => "entry",
                _ => continue,
            };
            let (branch, entry_commit) = warning
                .entry_id
                .and_then(|entry| entry_branch(&guard, entry))
                .unwrap_or((None, None));
            conflicts.push(IntakeConflictV1 {
                with: with.into(),
                entry_id: warning.entry_id,
                branch,
                entry_commit,
                renumberable: conflict_is_renumberable(&warning.files, provisional),
                paths: warning.files.clone(),
            });
        }
    }
    let blocking: Vec<IntakeConflictV1> = conflicts
        .iter()
        .filter(|conflict| !conflict.renumberable)
        .cloned()
        .collect();
    if !blocking.is_empty() {
        let guard = store.lock().await;
        move_to(
            &guard,
            row,
            IntakeStateV1::Conflicts,
            None,
            "conflicts_with",
            json!({ "conflicts_with": blocking }),
            false,
            "the source conflicts with rolling or a queued entry; the satellite was told to merge and resend",
        )
        .map_err(|error| error.to_string())?;
        return Ok(());
    }
    // A scan that did not reach every queued entry proves nothing about the
    // ones it skipped, but it is advisory (#3499): enqueue anyway and record
    // the incomplete scan on the entry's notice and outcome.
    let scan_note = incomplete_scan_note(&warnings);
    let review = review_of(&facts, declarations, delta);
    let post_land = match &review {
        Review::PreMerge(why) => {
            let guard = store.lock().await;
            move_to(
                &guard,
                row,
                IntakeStateV1::NeedsReview,
                None,
                "pre_merge_review",
                json!({ "why": why, "renumberable_conflicts": conflicts }),
                true,
                &format!(
                    "{why}: AGENTS.md reserves a pre-merge review for it, so it was not enqueued; review it and enqueue it by hand"
                ),
            )
            .map_err(|error| error.to_string())?;
            return Ok(());
        }
        Review::PostLand(why) => Some(*why),
        Review::Ordinary => None,
    };

    // 5. enqueue as the project's manager seat, derived filters.
    let request = AgentEnqueueLandingSourceRequestV1 {
        issue: row.issue,
        supersedes: None,
        source_commit: source.clone(),
        test_filters: Vec::new(),
        derive_filters: true,
        fixes_issue: row.fixes_issue,
        fixes_tests: row.fixes_tests.clone(),
        idempotency_key: intake_key(row.peer_id, row.report_id, "enqueue"),
        project_id: None,
        source_session_id: None,
        expedite: row
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.get("expedite"))
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| format!("satellite_intake_expedite_invalid: {error}"))?,
    };
    match control
        .agent_enqueue_landing_source_within(
            manager,
            request,
            true,
            crate::rolling_queue::ENQUEUE_DERIVE_WAIT,
        )
        .await
    {
        Ok(receipt) => {
            let guard = store.lock().await;
            let mut detail = match post_land {
                Some(why) => format!(
                    "enqueued as queue entry {}; the source {why}, so arrange the one post-land review",
                    receipt.entry.id
                ),
                None => format!("enqueued as queue entry {}", receipt.entry.id),
            };
            if scan_note.is_some() {
                detail.push_str(
                    "; the conflict scan was incomplete (the queue was long), so the lander is the conflict check",
                );
            }
            if let Some(warning) = &receipt.expedite_warning {
                detail.push_str(&format!("; {warning}"));
            }
            move_to(
                &guard,
                row,
                IntakeStateV1::Enqueued,
                Some(receipt.entry.id),
                "enqueued",
                json!({
                    "sequence": receipt.entry.sequence,
                    "post_land_review": post_land,
                    "renumberable_conflicts": conflicts,
                    "conflict_scan_incomplete": scan_note,
                    "expedite_warning": receipt.expedite_warning,
                }),
                post_land.is_some() || receipt.expedite_warning.is_some(),
                &detail,
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        }
        Err(error) => {
            let message = error.to_string();
            // Display adds `Invalid parameter:` / `Policy denied:`. Extract
            // the queue's stable code from the typed payload, not that label.
            let code = refusal_code(match &error {
                crate::error::DaemonError::InvalidParam(detail)
                | crate::error::DaemonError::PolicyDenied(detail) => detail,
                _ => &message,
            });
            let transient = is_transient_code(&code)
                || !matches!(
                    error,
                    crate::error::DaemonError::InvalidParam(_)
                        | crate::error::DaemonError::PolicyDenied(_)
                );
            if transient {
                return Err(format!("enqueue: {message}"));
            }
            let guard = store.lock().await;
            move_to(
                &guard,
                row,
                IntakeStateV1::Refused,
                None,
                &code,
                json!({ "message": message.chars().take(400).collect::<String>() }),
                true,
                &format!("the queue refused the source: {code}"),
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        }
    }
}

/// Watch one `enqueued` intake until its queue entry settles.
async fn watch(store: &SharedStore, row: &Row) -> Result<(), String> {
    let Some(entry_id) = row.queue_entry_id else {
        return Err("enqueued_without_entry".into());
    };
    let guard = store.lock().await;
    let entry = guard
        .get_rolling_queue_entry(entry_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "entry_missing".to_owned())?;
    if !entry.state.is_terminal() {
        return Ok(());
    }
    let outcome = entry.outcome.clone().unwrap_or_default();
    let (code, attention, detail) = match entry.state {
        RollingQueueEntryState::Published => (
            "published",
            false,
            format!(
                "published; rolling tip {}",
                outcome.landed_sha.as_deref().unwrap_or("unknown")
            ),
        ),
        RollingQueueEntryState::Superseded => (
            "withdrawn",
            false,
            "the queue entry was withdrawn; the satellite was told".to_owned(),
        ),
        state => (
            state.as_str(),
            true,
            format!(
                "the queue entry ended {} ({}); the satellite was told",
                state.as_str(),
                outcome.refusal.as_deref().unwrap_or("no code")
            ),
        ),
    };
    move_to(
        &guard,
        row,
        IntakeStateV1::Settled,
        None,
        code,
        json!({
            "landed_sha": outcome.landed_sha,
            "refusal": outcome.refusal,
            "failing_tests": outcome.failing_tests,
            "detail": outcome.detail,
        }),
        attention,
        &detail,
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// The text the satellite is told for an intake in `row.state`.
fn reply_text(row: &Row) -> String {
    let source = row.source_commit.as_str();
    let outcome = row.outcome.clone().unwrap_or(Value::Null);
    let code = row.outcome_code.as_deref().unwrap_or("");
    match row.state {
        IntakeStateV1::Conflicts => {
            let conflicts: Vec<IntakeConflictV1> = outcome
                .get("conflicts_with")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            conflicts_reply(source, row.issue, &conflicts)
        }
        IntakeStateV1::Refused => refused_reply(
            source,
            row.issue,
            code,
            if code == "source_not_on_origin" {
                "push the commit to origin and send a new ENQUEUE report"
            } else {
                "fix the cause named by the code and send a new ENQUEUE report"
            },
        ),
        IntakeStateV1::NeedsReview => needs_review_reply(
            source,
            row.issue,
            outcome
                .get("why")
                .and_then(Value::as_str)
                .unwrap_or(match code {
                    "queue_disabled" => "the hub merge queue is disabled",
                    "intake_stalled" => "the hub could not finish the intake",
                    _ => "the hub manager must decide",
                }),
        ),
        IntakeStateV1::Settled => {
            let failing: Vec<String> = outcome
                .get("failing_tests")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            let text = |key: &str| outcome.get(key).and_then(Value::as_str);
            let (verb, landed) = match code {
                "published" => ("published", text("landed_sha")),
                "already_on_rolling" => ("published", Some(source)),
                "withdrawn" => ("withdrawn", None),
                other => (if other.is_empty() { "ended" } else { other }, None),
            };
            settled_reply(
                source,
                row.issue,
                verb,
                landed,
                text("refusal"),
                &failing,
                text("detail"),
            )
        }
        IntakeStateV1::Received | IntakeStateV1::Enqueued => String::new(),
    }
}

/// Tell each satellite what became of its report, once.
async fn send_replies(store: &SharedStore) {
    let owed = match store.lock().await.enqueue_intakes_owing_reply(PASS_BATCH) {
        Ok(owed) => owed,
        Err(error) => {
            tracing::warn!("satellite enqueue intake replies read failed: {error}");
            return;
        }
    };
    for row in owed {
        let guard = store.lock().await;
        let sent = (|| -> crate::error::Result<bool> {
            let Some(manager) = guard
                .get_harness_manager(row.project_id)?
                .and_then(|config| config.current_session_id)
            else {
                return Ok(false);
            };
            let Some(address) = guard.satellite_reply_address(row.peer_id)? else {
                // The hub never guesses a recipient: the manager relays it.
                guard.mark_enqueue_intake_notified(&row, row.state)?;
                let _ = guard.record_enqueue_intake_notice(
                    &row,
                    true,
                    "the satellite's declared scope names no single recipient, so the hub could not tell it; relay the outcome",
                );
                return Ok(true);
            };
            let request = AgentSendSatelliteMessageRequestV1 {
                peer_id: SatelliteUuidV1(row.peer_id),
                remote_session_id: SatelliteUuidV1(address),
                scope_project_id: None,
                message: reply_text(&row),
                idempotency_key: intake_key(row.peer_id, row.report_id, row.state.as_str()),
                expires_at: None,
                issue_refs: Vec::new(),
            };
            guard.queue_satellite_message(manager, &request, chrono::Utc::now())?;
            guard.mark_enqueue_intake_notified(&row, row.state)?;
            Ok(true)
        })();
        match sent {
            Ok(true) => {}
            Ok(false) => {}
            Err(error) => tracing::warn!(
                peer = %row.peer_id,
                report = %row.report_id,
                "satellite enqueue intake reply not queued: {error}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn warning(code: &str) -> rsi_common::rolling_queue::QueueConflictWarningV1 {
        rsi_common::rolling_queue::QueueConflictWarningV1 {
            code: code.into(),
            entry_id: None,
            entry_sequence: None,
            files: Vec::new(),
            detail: "scan stopped".into(),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_incomplete_conflict_scan_is_advisory_and_never_retried_3499() {
        assert_eq!(incomplete_scan_note(&[]), None);
        assert_eq!(
            incomplete_scan_note(&[warning(QUEUE_CONFLICTS_WITH_ENTRY)]),
            None
        );
        let note = incomplete_scan_note(&[
            warning(QUEUE_CONFLICTS_WITH_ENTRY),
            warning(QUEUE_CONFLICT_SCAN_INCOMPLETE),
        ]);
        assert_eq!(note.as_deref(), Some("scan stopped"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn review_for(
        migration_version: Option<u32>,
        declarations: Option<usize>,
        delta: crate::rolling_queue::DeltaRead,
    ) -> Review {
        let facts = crate::rolling_queue::SourceFacts {
            migration_version,
            hot_files: Vec::new(),
        };
        review_of(&facts, declarations, delta)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn changed(paths: &[&str]) -> crate::rolling_queue::DeltaRead {
        crate::rolling_queue::DeltaRead::Paths(
            paths.iter().map(|path| (*path).to_owned()).collect(),
        )
    }

    /// Authority and custody sources are enqueued and flagged for the manager's
    /// post-land review (the flag drives `needs_attention` on the digest notice).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn authority_and_custody_sources_get_the_post_land_review_3345() {
        let authority = review_for(
            None,
            Some(0),
            changed(&["crates/rsid/src/session/agent_authority.rs"]),
        );
        assert!(
            matches!(
                authority,
                Review::PostLand("changes agent or manager authority")
            ),
            "authority path"
        );
        let custody = review_for(
            None,
            Some(0),
            changed(&["docs/a.md", "crates/rsid/src/session/branch_custody.rs"]),
        );
        assert!(
            matches!(
                custody,
                Review::PostLand("changes sandbox or session custody")
            ),
            "custody path"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_ordinary_source_is_enqueued_with_no_review_flag_3345() {
        let ordinary = review_for(None, Some(0), changed(&["a.txt", "docs/agents/notes.md"]));
        assert!(matches!(ordinary, Review::Ordinary));
        let empty = review_for(None, Some(0), changed(&[]));
        assert!(matches!(empty, Review::Ordinary));
    }

    /// Credential, migration and unreadable sources are held for a pre-merge
    /// review, never enqueued.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn migration_credential_and_unknown_sources_are_held_for_pre_merge_review_3345() {
        use crate::rolling_queue::DeltaRead;
        let vault = review_for(
            None,
            Some(0),
            changed(&["crates/rsid-store/src/vault/secret.rs"]),
        );
        assert!(matches!(vault, Review::PreMerge(_)), "credential path");
        let migration = review_for(Some(191), Some(0), changed(&["a.txt"]));
        assert!(matches!(migration, Review::PreMerge(_)), "migration source");
        let authority_with_migration = review_for(
            Some(191),
            Some(0),
            changed(&["crates/rsid/src/session/agent_authority.rs"]),
        );
        assert!(
            matches!(authority_with_migration, Review::PreMerge(_)),
            "a migration outranks the post-land flag"
        );
        let declared = review_for(None, Some(1), changed(&["a.txt"]));
        assert!(
            matches!(declared, Review::PreMerge(_)),
            "provisional declaration"
        );
        let unknown_declarations = review_for(None, None, changed(&["a.txt"]));
        assert!(matches!(unknown_declarations, Review::PreMerge(_)));
        let unreadable = review_for(None, Some(0), DeltaRead::Unreadable);
        assert!(
            matches!(unreadable, Review::PreMerge(_)),
            "unreadable delta"
        );
        let too_large = review_for(None, Some(0), DeltaRead::TooLarge("too many paths"));
        assert!(matches!(too_large, Review::PreMerge(_)), "oversized delta");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_intake_reads_more_live_entries_than_the_scanner_probes_3345() {
        assert!(LIVE_QUEUE_READ_LIMIT > crate::rolling_queue::CONFLICT_PROBE_MAX_ENTRIES);
    }
}
