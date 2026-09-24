//! `AgentArchiveChild` (#670 R2 design (b)): authority and the one IMMEDIATE
//! archive transaction.
//!
//! Every archive outcome, including a deduplicated replay, is decided only
//! after the caller's authority passes inside the transaction. The archive is
//! logical only: it never enters the operator cleanup saga and never removes
//! a sandbox.

use super::{continuation_cursor_tx, lineage_chain};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::issues::AgentIssueActor;
use crate::store::manager_actions::RecoveryOwnerMode;
use crate::store::row_mappers::{SESSION_COLUMNS, SessionRow, map_session_row};
use crate::store::sessions::resolve_owning_epic_topology_tx;
use rsi_common::agent_coordination::{
    AgentArchiveChildRequestV1, AgentArchiveChildResultV1, AgentArchiveErrorCodeV1 as Code,
    AgentArchiveRefusalDetailV1 as Detail,
};
use rsi_common::types::{Session, SessionStatus, is_leaf_kind};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

fn refuse(code: Code) -> DaemonError {
    crate::error::agent_archive_error(code, None, None)
}

fn refuse_with(code: Code, detail: Detail) -> DaemonError {
    crate::error::agent_archive_error(code, Some(detail), None)
}

fn load_session_tx(tx: &Transaction<'_>, id: Uuid) -> Result<Option<Session>> {
    tx.query_row(
        &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id=?1"),
        [id.to_string()],
        map_session_row,
    )
    .optional()?
    .map(SessionRow::into_session)
    .transpose()
}

fn owning_epic_id(tx: &Transaction<'_>, session: &Session) -> Option<Uuid> {
    resolve_owning_epic_topology_tx(tx, session)
        .ok()
        .map(|epic| epic.id)
}

/// The authority decision, evaluated over durable rows only: the caller must
/// be the CURRENT persisted lead of the Epic that owns both the logical child
/// and its tip (manager decision A narrows plan §3.1 to its first clause).
///
/// Returns the lineage `L` (logical child first, tip last) and the owning
/// Epic's current lead generation. A missing target is `target_unknown`;
/// every other failure is a flat `target_not_authorized` that discloses
/// nothing about the child.
fn authorize_archive_tx(
    tx: &Transaction<'_>,
    caller_session_id: Uuid,
    target_session_id: Uuid,
) -> Result<(Vec<Uuid>, i64)> {
    let child =
        load_session_tx(tx, target_session_id)?.ok_or_else(|| refuse(Code::TargetUnknown))?;
    let lineage = lineage_chain(tx, target_session_id)?;
    // A caller inside the lineage it names would archive itself.
    if lineage.contains(&caller_session_id) {
        return Err(refuse(Code::SelfArchiveDenied));
    }
    let tip_id = *lineage.last().unwrap_or(&target_session_id);
    let tip = load_session_tx(tx, tip_id)?.ok_or_else(|| refuse(Code::TargetNotAuthorized))?;
    let child_epic = owning_epic_id(tx, &child);
    let tip_epic = owning_epic_id(tx, &tip);

    // The current persisted lead of the child's owning Epic.
    if let Ok(authority) = Store::resolve_agent_issue_authority_tx(tx, caller_session_id)
        && let AgentIssueActor::Lead {
            epic_id,
            lead_generation,
        } = authority.actor
        && child_epic == Some(epic_id)
        && tip_epic == Some(epic_id)
    {
        return Ok((lineage, lead_generation));
    }

    // Decision A (review a755d660): there is no spawn-owner fallback. A
    // former lead, a spawn owner, a direct parent and a manager are all
    // refused here, including on the dedup-replay path.
    Err(refuse(Code::TargetNotAuthorized))
}

/// JSON array of the lineage ids, bound once and expanded with `json_each`.
fn lineage_json(lineage: &[Uuid]) -> String {
    serde_json::Value::from(lineage.iter().map(ToString::to_string).collect::<Vec<_>>()).to_string()
}

fn exists(tx: &Transaction<'_>, sql: &str, lineage: &str) -> Result<bool> {
    Ok(tx.query_row(sql, [lineage], |row| row.get(0))?)
}

const TERMINAL: &str = "('Completed','Failed','Interrupted')";

/// The `live_continuation` and `review_source_sealed` probes, in refusal order.
fn continuation_and_review_refusal(
    tx: &Transaction<'_>,
    lineage: &[Uuid],
    lineage_ids: &str,
) -> Result<Option<(Code, Detail)>> {
    const IN_L: &str = "(SELECT value FROM json_each(?1))";
    let running_successor = format!(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE continued_from IN {IN_L}
           AND status NOT IN ('Completed','Failed','Interrupted','Archived','Deleted'))"
    );
    if exists(tx, &running_successor, lineage_ids)? {
        return Ok(Some((Code::LiveContinuation, Detail::RunningSuccessor)));
    }
    // A program-guard sentinel is program identity, not a continuation; the
    // recovery-owner gate's program half already judged it.
    for id in lineage {
        let sentinel =
            crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(*id);
        let resume: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE enabled=1 AND wake_mode='resume'
               AND wake_session_id=?1 AND id<>?2)",
            params![id.to_string(), sentinel.to_string()],
            |row| row.get(0),
        )?;
        if resume {
            return Ok(Some((Code::LiveContinuation, Detail::ResumeWake)));
        }
    }
    let probes: [(Detail, String); 3] = [
        (
            Detail::SuccessorReservation,
            format!(
                "SELECT EXISTS(SELECT 1 FROM agent_successor_reservations
                   WHERE predecessor_session_id IN {IN_L}
                     AND state IN ('reserved','launching','uncertain'))"
            ),
        ),
        (
            Detail::FreshRelaunchIntent,
            format!(
                "SELECT EXISTS(SELECT 1 FROM agent_child_relaunch_intents
                   WHERE state='intent' AND tip_session_id IN {IN_L})"
            ),
        ),
        (
            Detail::PendingMail,
            format!(
                "SELECT EXISTS(SELECT 1 FROM agent_messages WHERE target_session_id IN {IN_L}
                   AND state IN ('queued','claimed','injected','uncertain'))"
            ),
        ),
    ];
    for (detail, sql) in probes {
        if exists(tx, &sql, lineage_ids)? {
            return Ok(Some((Code::LiveContinuation, detail)));
        }
    }
    let assignment_open = format!(
        "SELECT EXISTS(SELECT 1 FROM manager_review_assignments
           WHERE author_session_id IN {IN_L} AND superseded_by_assignment_id IS NULL
             AND state IN ('reserved','allocating','active'))"
    );
    if exists(tx, &assignment_open, lineage_ids)? {
        return Ok(Some((Code::ReviewSourceSealed, Detail::AssignmentOpen)));
    }
    // An unarchived work fact sourced from L that still needs a review verdict
    // on its sealed source.
    let verdict_pending = format!(
        "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_work_facts f
           WHERE f.kind='work' AND f.archived=0
             AND json_extract(f.payload_json,'$.source_session_id') IN {IN_L}
             AND json_extract(f.payload_json,'$.source_commit') IS NOT NULL
             AND json_extract(f.payload_json,'$.integration') IS NULL
             AND EXISTS(SELECT 1 FROM json_each(f.payload_json,'$.required_gates') g
                         WHERE g.value='review')
             AND NOT EXISTS(
                 SELECT 1 FROM manager_review_assignments a
                   JOIN manager_review_receipts r ON r.assignment_id=a.assignment_id
                  WHERE a.project_id=f.project_id AND a.epic_id=f.epic_id
                    AND a.work_key=f.work_key
                    AND a.spec_revision=json_extract(f.payload_json,'$.spec_revision')
                    AND a.source_sha=json_extract(f.payload_json,'$.source_commit')
                    AND a.superseded_by_assignment_id IS NULL))"
    );
    if exists(tx, &verdict_pending, lineage_ids)? {
        return Ok(Some((Code::ReviewSourceSealed, Detail::VerdictPending)));
    }
    Ok(None)
}

/// Retire the caller's own enabled ordinary child watches on `L` with
/// witness `consumed`, so the `SessionArchived` publish sends no second
/// notice to the caller. Manager watches and other owners' watches are left
/// untouched.
fn consume_caller_watches_tx(
    store: &Store,
    tx: &Transaction<'_>,
    caller_session_id: Uuid,
    lineage_ids: &str,
) -> Result<u32> {
    let ids: Vec<String> = tx
        .prepare(
            "SELECT id FROM scheduled_jobs
              WHERE enabled=1 AND wake_session_id=?2
                AND wake_mode IN (SELECT 'on_terminal:' || value FROM json_each(?1))
                AND NOT EXISTS (SELECT 1 FROM harness_manager_watches w WHERE w.job_id=scheduled_jobs.id)
              ORDER BY id",
        )?
        .query_map(params![lineage_ids, caller_session_id.to_string()], |row| {
            row.get(0)
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut consumed = 0_u32;
    for id in ids {
        let id = Uuid::parse_str(&id)
            .map_err(|error| DaemonError::Store(format!("invalid scheduled job id: {error}")))?;
        let Some(job) = store.get_scheduled_job(&id)? else {
            continue;
        };
        if Store::retire_unchanged_child_watch_in(tx, &job, "consumed")? {
            consumed += 1;
        }
    }
    Ok(consumed)
}

impl Store {
    /// Refusal-ordering pre-check outside the archive transaction. It returns
    /// the lineage `L` observed now; it never returns success for the verb.
    pub(crate) fn agent_archive_child_precheck(
        &self,
        caller_session_id: Uuid,
        target_session_id: Uuid,
    ) -> Result<Vec<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let (lineage, _) = authorize_archive_tx(&tx, caller_session_id, target_session_id)?;
        tx.commit()?;
        Ok(lineage)
    }

    /// The single IMMEDIATE archive transaction of §3.3 step 3.
    ///
    /// Order: re-derive `L`, authority, cursor fence, dedup, checks, mutate,
    /// commit. A refusal rolls the whole transaction back, so status, the C5
    /// marker and watches are unchanged.
    pub(crate) fn agent_archive_child_tx(
        &self,
        caller_session_id: Uuid,
        request: &AgentArchiveChildRequestV1,
    ) -> Result<AgentArchiveChildResultV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (lineage, lead_generation) =
            authorize_archive_tx(&tx, caller_session_id, request.target_session_id)?;
        let observed = continuation_cursor_tx(&tx, request.target_session_id)?;
        if !request.admits(&observed) {
            return Err(crate::error::agent_archive_error(
                Code::StaleArchive,
                None,
                Some(observed),
            ));
        }
        let mut rows = Vec::with_capacity(lineage.len());
        for id in &lineage {
            rows.push(load_session_tx(&tx, *id)?.ok_or_else(|| refuse(Code::TargetUnknown))?);
        }
        let prior_status = rows
            .last()
            .map_or(SessionStatus::Archived, |row| row.status);
        let sandbox_retained = rows.iter().any(|row| row.sandbox_root.is_some());
        let lineage_ids = lineage_json(&lineage);

        if rows.iter().all(|row| row.status == SessionStatus::Archived) {
            tx.commit()?;
            return Ok(AgentArchiveChildResultV1 {
                target_session_id: request.target_session_id,
                archived_session_ids: Vec::new(),
                prior_status,
                deduplicated: true,
                watches_consumed: 0,
                c5_marker_settled: false,
                lead_generation,
                sandbox_retained,
            });
        }

        if rows.iter().any(|row| !is_leaf_kind(row.session_kind)) {
            return Err(refuse(Code::TargetNotLeaf));
        }
        if !matches!(
            rows[0].status,
            SessionStatus::Completed
                | SessionStatus::Failed
                | SessionStatus::Interrupted
                | SessionStatus::Archived
        ) {
            return Err(refuse(Code::TargetNotTerminal));
        }
        let is_lead = exists(
            &tx,
            "SELECT EXISTS(SELECT 1 FROM sessions
               WHERE lead_session_id IN (SELECT value FROM json_each(?1)))",
            &lineage_ids,
        )?;
        if is_lead {
            return Err(refuse(Code::TargetIsLead));
        }
        for id in &lineage {
            self.recovery_owner_gate(*id, RecoveryOwnerMode::AgentArchive)
                .map_err(|error| match error {
                    DaemonError::InvalidParam(ref code)
                        if code == "manager_v2_human_or_recovery_owner" =>
                    {
                        refuse_with(Code::RecoveryOwnerHeld, Detail::RecoveryOwner)
                    }
                    DaemonError::InvalidParam(ref code)
                        if code == "manager_v2_program_evidence_unknown" =>
                    {
                        refuse_with(Code::RecoveryOwnerHeld, Detail::ProgramEvidenceUnknown)
                    }
                    other => other,
                })?;
        }
        if let Some((code, detail)) = continuation_and_review_refusal(&tx, &lineage, &lineage_ids)?
        {
            return Err(refuse_with(code, detail));
        }

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let mut archived_session_ids = Vec::new();
        let mut c5_marker_settled = false;
        for id in &lineage {
            let changed = tx.execute(
                &format!(
                    "UPDATE sessions SET status='Archived', pending_archive=0, updated_at=?1
                      WHERE id=?2 AND status IN {TERMINAL}"
                ),
                params![now, id.to_string()],
            )?;
            if changed == 1 {
                archived_session_ids.push(*id);
                c5_marker_settled |= Self::resolve_c5_autofile_pending_tx(&tx, *id)? > 0;
            }
        }
        let watches_consumed =
            consume_caller_watches_tx(self, &tx, caller_session_id, &lineage_ids)?;
        tx.commit()?;
        Ok(AgentArchiveChildResultV1 {
            target_session_id: request.target_session_id,
            archived_session_ids,
            prior_status,
            deduplicated: false,
            watches_consumed,
            c5_marker_settled,
            lead_generation,
            sandbox_retained,
        })
    }
}
