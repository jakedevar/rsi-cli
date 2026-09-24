//! Durable DB-native review allocation, reviewer receipts, and source admission.

use super::{
    Store,
    harness_manager_v2::{fingerprint, now, refused},
    manager_actions::{ManagerActionOperationV2, ManagerActionOriginV2},
    manager_ledger::{Acceptance, WorkRecord, canonical_sha},
};
#[cfg(test)]
use crate::error::DaemonError;
use crate::error::Result;
use chrono::{DateTime, Utc};
use rsi_common::{
    harness_manager::HarnessManagerConfigV1,
    harness_manager_v2::*,
    review_model_family::{ReviewModelFamily, review_model_family},
    types::{SessionKind, SessionStatus},
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

mod contributors;

const MAX_ASSIGNMENTS_PER_WORK: i64 = 64;
const MAX_REVIEW_ROUNDS_PER_REVISION: usize = 3;
const REVIEW_RECONCILE_BATCH: usize = 16;
const MAX_INFRA_RELAUNCHES: usize = 2;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagerReviewFault {
    AfterReservation,
    AfterAllocationJournal,
    BeforeReceiptCommit,
    BeforeAdmissionRead,
    AfterReviewerTerminalObservation,
}

#[cfg(test)]
thread_local! {
    static MANAGER_REVIEW_FAIL_NEXT: std::cell::Cell<Option<ManagerReviewFault>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn manager_review_fail_next(point: ManagerReviewFault) {
    MANAGER_REVIEW_FAIL_NEXT.with(|slot| slot.set(Some(point)));
}

#[cfg(test)]
fn manager_review_fault(point: ManagerReviewFault) -> Result<()> {
    if MANAGER_REVIEW_FAIL_NEXT.with(|slot| slot.get() == Some(point) && slot.take().is_some()) {
        return Err(DaemonError::Store(format!(
            "injected manager review fault: {point:?}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewAllocationRequest {
    requester_session_id: Uuid,
    fence: ManagerFenceV2,
    query: String,
    launch: ManagerLaunchChoiceV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    infra_retry_of: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    contributor_session_ids: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    contributor_families: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    family_override_key: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewAssignment {
    pub assignment_id: Uuid,
    pub project_id: Uuid,
    pub epic_id: Uuid,
    pub manager_session_id: Uuid,
    pub scope_version: i64,
    pub work_key: String,
    pub spec_revision: i64,
    pub author_session_id: Uuid,
    pub source_sha: String,
    pub reviewer_session_id: Option<Uuid>,
    pub reviewer_invocation_id: Option<Uuid>,
    pub reviewer_custody_id: Option<Uuid>,
    pub reviewer_custody_generation: Option<u64>,
    pub action_operation_id: Option<Uuid>,
    pub state: String,
    request: ReviewAllocationRequest,
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewSubmissionObservation {
    pub assignment_id: Uuid,
    pub reviewer_session_id: Uuid,
    pub reviewer_invocation_id: Uuid,
    pub custody: crate::store::sandbox_custody::PersistedCustody,
    pub source_sha: String,
}

fn parse_uuid(value: String) -> Result<Uuid> {
    Uuid::parse_str(&value).map_err(|_| refused("manager_review_stored_identity"))
}

fn optional_uuid(value: Option<String>) -> Result<Option<Uuid>> {
    value.map(parse_uuid).transpose()
}

fn verdict_name(verdict: ManagerReviewVerdictV1) -> &'static str {
    match verdict {
        ManagerReviewVerdictV1::Accepted => "accepted",
        ManagerReviewVerdictV1::ChangesRequested => "changes_requested",
        ManagerReviewVerdictV1::Blocked => "blocked",
    }
}

fn severity_name(severity: ManagerReviewFindingSeverityV1) -> &'static str {
    match severity {
        ManagerReviewFindingSeverityV1::Info => "info",
        ManagerReviewFindingSeverityV1::Warning => "warning",
        ManagerReviewFindingSeverityV1::Error => "error",
    }
}

fn review_request_fingerprint(request: &AgentSubmitReviewReceiptRequestV1) -> Result<String> {
    let mut findings = request.findings.clone();
    findings.sort_by(|left, right| left.key.cmp(&right.key));
    fingerprint(&json!({"verdict":request.verdict,"findings":findings}))
}

fn terminal_allocation_failure(error: &crate::error::DaemonError) -> Option<&'static str> {
    let crate::error::DaemonError::InvalidParam(code) = error else {
        return None;
    };
    match code.as_str() {
        "manager_review_scope_changed"
        | "manager_review_work_changed"
        | "manager_review_author_unavailable"
        | "manager_review_author_out_of_scope"
        | "manager_review_source_changed"
        | "manager_review_self_review"
        | "manager_v2_scope_changed"
        | "manager_v2_policy_changed"
        | "manager_v2_capability_denied"
        | "manager_v2_launch_not_granted"
        | "manager_v2_creation_limit" => Some("manager_review_allocation_invalidated"),
        // K15A-1: an operation under the allocator's key already exists and
        // was not journaled by the allocator; it is never adopted or linked.
        "manager_review_allocation_conflict" => Some("manager_review_allocation_conflict"),
        _ => None,
    }
}

/// The exact lifecycle request the allocator journals for an assignment.
fn review_allocation_request(
    assignment_id: Uuid,
    assignment: &ReviewAssignment,
) -> AgentManagerControlRequestV2 {
    let prompt = manager_review_launch_prompt(
        assignment_id,
        &assignment.source_sha,
        &assignment.work_key,
        assignment.spec_revision,
        &assignment.request.query,
    );
    AgentManagerControlRequestV2 {
        fence: assignment.request.fence.clone(),
        idempotency_key: format!(
            "{}{assignment_id}",
            super::manager_actions::REVIEW_ALLOCATION_KEY_PREFIX
        ),
        operation: ManagerActionV2::CreateSession {
            parent_id: assignment.epic_id,
            kind: SessionKind::Research,
            query: prompt,
            launch: assignment.request.launch.clone(),
        },
    }
}

/// Review-terminal notice projection: project, Epic, work key, spec revision,
/// source, state, failure code, reviewer, receipt, verdict, finding counts.
type ReviewNoticeRow = (
    String,
    String,
    String,
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
);

/// Reviewer closeout budget stated in the launch prompt (#575 R1).
///
/// These are review-design constants, deliberately independent of the
/// model-control per-invocation accounting reservation (`reserved_wall_time_ms`),
/// which is an admission estimate and not a review duration. They are sized for
/// healthy Tier-2 reviews (20-45 minutes; builds alone take minutes) and are a
/// closeout rule, not a race: they are stated to the reviewer, not enforced.
/// #575 R2 will let the manager's `request_review` declare these per review.
const REVIEW_TARGET_MINUTES: u32 = 30;
/// Hard closeout deadline: submit a receipt (`blocked` if unresolved) by then.
const REVIEW_CLOSEOUT_DEADLINE_MINUTES: u32 = 60;
/// Tool-call budget stated to the reviewer.
const REVIEW_TOOL_CALL_BUDGET: u32 = 150;

/// Build the reviewer launch prompt: the fixed identity line, then a bounded
/// review contract, then the caller's free-form query. The contract comes first
/// so an unbounded query cannot displace the receipt obligation (#568/#575).
fn manager_review_launch_prompt(
    assignment_id: Uuid,
    source_sha: &str,
    work_key: &str,
    spec_revision: i64,
    query: &str,
) -> String {
    format!(
        "DB-native independent review assignment {assignment_id}. Review exact source {source_sha} for manager work `{work_key}` revision {spec_revision}. Do not create or commit review artifacts. Submit exactly one immutable receipt with AgentSubmitReviewReceipt (or rsi_control_submit_review_receipt) for assignment_id {assignment_id} during this model invocation.\n\n\
Review contract (it precedes the caller request below; the budget bounds optional exploration only, never required checks):\n\
1. Required checks first: the fixed minimum (review exact source {source_sha} and its diff, run the focused tests of the touched modules, and run the gates the change is subject to) plus every check the caller request below requires. Do all required checks before any optional exploration.\n\
2. Closeout budget: target {REVIEW_TARGET_MINUTES} minutes and at most {REVIEW_TOOL_CALL_BUDGET} tool calls; closeout deadline {REVIEW_CLOSEOUT_DEADLINE_MINUTES} minutes elapsed. This is a closeout rule, not a race: when you reach the target, stop optional work and submit; if required checks remain at the deadline, apply clause 5.\n\
3. Receipt first: submit your receipt as soon as all required checks (fixed minimum and caller-required) are done. Optional exploration only after the receipt is submitted, and keep it brief.\n\
4. Never run destructive cleanup (rm -rf, rm -f, git clean, forced worktree removal); the exec policy refuses it. Use cargo clean for build directories and leave scratch files in place.\n\
5. At the closeout deadline, at tool-call budget exhaustion, or on any blocker, submit verdict `blocked` listing completed checks, unchecked required checks (including caller-required ones), and the reason. Never end this invocation without a receipt.\n\n\
Caller request:\n{query}"
    )
}

/// Typed failure code for a reviewer that reached a terminal state without a
/// receipt. Every code keeps the `manager_review_receipt_missing` prefix so
/// prefix consumers keep matching; the suffix records WHY the review ended.
/// Returns `None` while neither the invocation nor the session is terminal.
fn receipt_missing_failure_code(
    invocation_status: &str,
    invocation_error_class: Option<&str>,
    session_status: SessionStatus,
) -> Option<&'static str> {
    const INTERRUPTED: &str = "manager_review_receipt_missing_interrupted";
    const PROVIDER_FAILED: &str = "manager_review_receipt_missing_provider_failed";
    const BUDGET_EXCEEDED: &str = "manager_review_receipt_missing_budget_exceeded";
    const FINAL: &str = "manager_review_receipt_missing_final_without_receipt";
    match invocation_status {
        "cancelled" => return Some(INTERRUPTED),
        "failed" => {
            return Some(match invocation_error_class {
                Some(class)
                    if super::model_control::is_cancellation_terminal_error(Some(class)) =>
                {
                    INTERRUPTED
                }
                Some(class) if class.starts_with("over_budget") => BUDGET_EXCEEDED,
                _ => PROVIDER_FAILED,
            });
        }
        "completed" => return Some(FINAL),
        _ => {}
    }
    match session_status {
        SessionStatus::Interrupted => Some(INTERRUPTED),
        SessionStatus::Failed => Some(PROVIDER_FAILED),
        SessionStatus::Completed => Some(FINAL),
        _ => None,
    }
}

fn is_infra_review_end(code: &str) -> bool {
    matches!(
        code,
        "manager_review_receipt_missing_interrupted"
            | "manager_review_receipt_missing_provider_failed"
            | "manager_review_allocation_failed"
            | "manager_review_custody_unavailable"
    )
}

/// One stored assignment row of a (work, revision), as the round budget reads it.
struct ReviewAttemptRow {
    assignment_id: Uuid,
    state: String,
    launched: bool,
    superseded_by: Option<Uuid>,
    created_at: String,
    terminal_at: Option<String>,
    has_receipt: bool,
    request: ReviewAllocationRequest,
}

/// #599 A1: group rows into attempt chains (an original request plus its
/// infra-retry successors) and return the chains that spend review budget, in
/// request order. `pending_supersede` is the current same-SHA row that the
/// request being evaluated would supersede.
///
/// A chain counts when any row has a receipt, when its end is still in
/// flight, or when it launched a reviewer and was then superseded, while in
/// flight, by a new request. Chains that ended failed (or cancelled) without a
/// receipt and chains superseded before any launch are exempt.
fn counted_review_attempts(
    rows: &[ReviewAttemptRow],
    pending_supersede: Option<Uuid>,
) -> Vec<Vec<&ReviewAttemptRow>> {
    let by_id: std::collections::HashMap<Uuid, &ReviewAttemptRow> =
        rows.iter().map(|row| (row.assignment_id, row)).collect();
    let mut counted = Vec::new();
    for root in rows
        .iter()
        .filter(|row| row.request.infra_retry_of.is_none())
    {
        let mut chain = vec![root];
        let mut end = root;
        for _ in 0..rows.len() {
            let Some(next) = end.superseded_by.and_then(|id| by_id.get(&id).copied()) else {
                break;
            };
            if next.request.infra_retry_of != Some(end.assignment_id) {
                break;
            }
            chain.push(next);
            end = next;
        }
        let launched = chain.iter().any(|row| row.launched);
        let in_flight = matches!(end.state.as_str(), "reserved" | "allocating" | "active");
        let spends = if chain.iter().any(|row| row.has_receipt) {
            true
        } else if pending_supersede == Some(end.assignment_id) {
            in_flight && launched
        } else if in_flight {
            true
        } else if end.state == "superseded" {
            // A supersession stamps `terminal_at` with the successor's
            // `created_at`; an earlier stamp means the row had already ended
            // (failed or cancelled) before it was re-requested. An unreadable
            // successor fails closed as spent.
            end.superseded_by
                .and_then(|id| by_id.get(&id))
                .is_none_or(|successor| {
                    launched
                        && end
                            .terminal_at
                            .as_deref()
                            .is_none_or(|ended| ended >= successor.created_at.as_str())
                })
        } else {
            false
        };
        if spends {
            counted.push(chain);
        }
    }
    counted
}

impl Store {
    /// Load at most `MAX_ASSIGNMENTS_PER_WORK` rows of one (work, revision).
    fn review_attempt_rows(
        &self,
        project_id: Uuid,
        epic_id: Uuid,
        work_key: &str,
        spec_revision: i64,
    ) -> Result<Vec<ReviewAttemptRow>> {
        let mut statement = self.conn.prepare(
            "SELECT a.assignment_id,a.state,a.reviewer_session_id IS NOT NULL,
                    a.superseded_by_assignment_id,a.created_at,a.terminal_at,
                    EXISTS(SELECT 1 FROM manager_review_receipts r
                            WHERE r.assignment_id=a.assignment_id),
                    a.request_json
               FROM manager_review_assignments a
              WHERE a.project_id=?1 AND a.epic_id=?2 AND a.work_key=?3 AND a.spec_revision=?4
              ORDER BY a.created_at,a.assignment_id LIMIT ?5",
        )?;
        let raw = statement
            .query_map(
                params![
                    project_id.to_string(),
                    epic_id.to_string(),
                    work_key,
                    spec_revision,
                    MAX_ASSIGNMENTS_PER_WORK
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, bool>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(
                |(id, state, launched, superseded_by, created_at, terminal_at, receipt, json)| {
                    Ok(ReviewAttemptRow {
                        assignment_id: parse_uuid(id)?,
                        state,
                        launched,
                        superseded_by: optional_uuid(superseded_by)?,
                        created_at,
                        terminal_at,
                        has_receipt: receipt,
                        request: serde_json::from_str(&json)
                            .map_err(|_| refused("manager_review_stored_request"))?,
                    })
                },
            )
            .collect()
    }

    /// #599 A1: the manager may supersede any row; anyone else only rows its
    /// own rotation lineage requested.
    fn require_review_supersede_owner(
        &self,
        config: &HarnessManagerConfigV1,
        caller: Uuid,
        requester: Uuid,
    ) -> Result<()> {
        if config.current_session_id == Some(caller) || requester == caller {
            return Ok(());
        }
        match self.manager_lineage_tip(requester) {
            Ok(tip) if tip == caller => Ok(()),
            _ => Err(refused("manager_review_manager_owned")),
        }
    }

    fn recorded_review_family(&self, session_id: Uuid) -> Result<ReviewModelFamily> {
        let session = self
            .get_session(session_id)?
            .ok_or_else(|| refused("manager_review_author_unavailable"))?;
        let model = if session
            .model
            .as_deref()
            .is_some_and(|model| !model.trim().is_empty())
        {
            session.model
        } else {
            self.conn
                .query_row(
                    "SELECT model FROM model_invocations
                      WHERE session_id=?1 AND model IS NOT NULL AND trim(model)<>''
                      ORDER BY created_at DESC,id DESC LIMIT 1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
        };
        Ok(review_model_family(session.provider, model.as_deref()))
    }

    /// Only the action journaled for a live DB review assignment may collect
    /// evidence while an operator acceptance decision is pending.
    pub(crate) fn manager_review_evidence_action(
        &self,
        action: &ManagerActionOperationV2,
    ) -> Result<bool> {
        let (
            ManagerActionV2::CreateSession {
                parent_id,
                kind: SessionKind::Research,
                ..
            },
            Some(reviewer),
            Some(source),
        ) = (
            &action.context.request.operation,
            action.context.target_session_id,
            action.context.source.as_ref(),
        )
        else {
            return Ok(false);
        };
        let assignment: Option<(String, i64, String, String)> = self
            .conn
            .query_row(
                "SELECT work_key,spec_revision,source_sha,author_session_id
               FROM manager_review_assignments
              WHERE action_operation_id=?1 AND project_id=?2 AND epic_id=?3
                AND reviewer_session_id=?4 AND state IN ('allocating','active')",
                params![
                    action.receipt.operation_id.to_string(),
                    action.project_id.to_string(),
                    parent_id.to_string(),
                    reviewer.to_string()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((key, revision, sha, author)) = assignment else {
            return Ok(false);
        };
        if source.commit != sha {
            return Ok(false);
        }
        let config = self
            .get_harness_manager(action.project_id)?
            .ok_or_else(|| refused("manager_review_scope_changed"))?;
        if source.session_id.to_string() != author
            && !self
                .manager_review_source_is_rotation_holder(&config, *parent_id, &author, source)?
        {
            return Ok(false);
        }
        let (_, work) = self.manager_v2_work(&config, &key)?;
        Ok(work.epic_id == *parent_id
            && work.spec_revision == revision
            && work.source_commit.as_deref() == Some(&sha))
    }

    /// #599 S1: allocation forks a DB review from the author's verified
    /// rotation tip when the author no longer holds its custody. The action
    /// source may name that tip only while it is still the exact holder the
    /// resolver proves (same custody root, unbroken transfer chain, same
    /// Epic) at the same sandbox root and custody generation; never any other
    /// session.
    fn manager_review_source_is_rotation_holder(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        author: &str,
        source: &super::manager_actions::ManagerActionSourceV2,
    ) -> Result<bool> {
        let Ok(author) = Uuid::parse_str(author) else {
            return Ok(false);
        };
        let Some(author) = self.get_session(author)? else {
            return Ok(false);
        };
        let Ok(holder) = self.manager_review_source_holder(config, epic, &author) else {
            return Ok(false);
        };
        // Callers reach here only when the source is not the author, so a
        // holder equal to the author can never match.
        let forked_from = source.session_id;
        Ok(holder.id == forked_from
            && holder.sandbox_root == source.sandbox_root
            && self.manager_action_custody_generation(holder.id)? == source.custody_generation)
    }

    pub(crate) fn manager_review_enrolled(
        &self,
        config: &HarnessManagerConfigV1,
        work: &WorkRecord,
        source_sha: &str,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_review_assignments
              WHERE project_id=?1 AND epic_id=?2 AND work_key=?3
                AND spec_revision=?4 AND source_sha=?5)",
            params![
                config.project_id.to_string(),
                work.epic_id.to_string(),
                work.key,
                work.spec_revision,
                source_sha
            ],
            |row| row.get(0),
        )?)
    }

    fn manager_review_assignment(&self, assignment_id: Uuid) -> Result<ReviewAssignment> {
        let row: Option<(
            String,
            String,
            String,
            i64,
            String,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
            String,
            i64,
            String,
        )> = self
            .conn
            .query_row(
                "SELECT project_id,epic_id,manager_session_id,scope_version,work_key,
                        spec_revision,author_session_id,source_sha,reviewer_session_id,
                        reviewer_invocation_id,reviewer_custody_id,
                        reviewer_custody_generation,action_operation_id,state,row_version,
                        request_json
                   FROM manager_review_assignments WHERE assignment_id=?1",
                [assignment_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                        row.get(15)?,
                    ))
                },
            )
            .optional()?;
        let Some(row) = row else {
            return Err(refused("manager_review_assignment_missing"));
        };
        Ok(ReviewAssignment {
            assignment_id,
            project_id: parse_uuid(row.0)?,
            epic_id: parse_uuid(row.1)?,
            manager_session_id: parse_uuid(row.2)?,
            scope_version: row.3,
            work_key: row.4,
            spec_revision: row.5,
            author_session_id: parse_uuid(row.6)?,
            source_sha: row.7,
            reviewer_session_id: optional_uuid(row.8)?,
            reviewer_invocation_id: optional_uuid(row.9)?,
            reviewer_custody_id: optional_uuid(row.10)?,
            reviewer_custody_generation: row.11.map(|value| value as u64),
            action_operation_id: optional_uuid(row.12)?,
            state: row.13,
            request: serde_json::from_str(&row.15)
                .map_err(|_| refused("manager_review_stored_request"))?,
        })
    }

    fn require_current_manager_review_authority(
        &self,
        assignment: &ReviewAssignment,
    ) -> Result<()> {
        let config = self
            .get_harness_manager(assignment.project_id)?
            .filter(|config| {
                config.manager_session_id == assignment.manager_session_id
                    && config.row_version == assignment.scope_version
            })
            .ok_or_else(|| refused("manager_review_scope_changed"))?;
        let policy = self
            .get_harness_manager_policy(assignment.project_id)?
            .filter(|policy| {
                !policy.revoked
                    && policy.manager_session_id == config.manager_session_id
                    && policy.scope_version == config.row_version
                    && policy.row_version == assignment.request.fence.policy_version
            })
            .ok_or_else(|| refused("manager_v2_policy_changed"))?;
        debug_assert_eq!(policy.scope_version, assignment.request.fence.scope_version);
        Ok(())
    }

    pub(crate) fn manager_review_reserve_on(
        &self,
        caller: Uuid,
        config: &HarnessManagerConfigV1,
        request: &AgentManagerUpdateRequestV2,
        work: &WorkRecord,
        work_row_version: i64,
        observed: &super::manager_ledger::LedgerObservation,
    ) -> Result<ManagerMutationReceiptV2> {
        let ManagerUpdateV2::RequestReview {
            expected_row_version,
            source_commit,
            query,
            launch,
            ..
        } = &request.change
        else {
            return Err(refused("manager_review_request_required"));
        };
        // Reservation is also called directly by retry/tests. Bind a lead to
        // the Work's Epic at the effect boundary, even without ledger prepare.
        if config.current_session_id != Some(caller) {
            let authority = self.manager_v2_authorize(caller, &request.fence, None)?;
            if authority.config.project_id != config.project_id
                || authority.config.row_version != config.row_version
            {
                return Err(refused("manager_v2_scope_changed"));
            }
            self.manager_v2_require_epic(&authority, work.epic_id)?;
        }
        if work.source_commit.as_ref() != Some(source_commit)
            || observed.source_commit.as_ref() != Some(source_commit)
            || !canonical_sha(source_commit)
        {
            return Err(refused("manager_review_source_changed"));
        }
        if *expected_row_version != work_row_version {
            return Err(refused("manager_review_work_version_changed"));
        }
        let author = work
            .source_session_id
            .ok_or_else(|| refused("manager_review_author_missing"))?;
        if self.manager_v2_descendant_epic(config, author)? != work.epic_id {
            return Err(refused("manager_review_author_out_of_scope"));
        }
        let total_count: i64 = self.conn.query_row(
            "SELECT count(*) FROM manager_review_assignments
              WHERE project_id=?1 AND epic_id=?2 AND work_key=?3 AND spec_revision=?4",
            params![
                config.project_id.to_string(),
                work.epic_id.to_string(),
                work.key,
                work.spec_revision
            ],
            |row| row.get(0),
        )?;
        if total_count >= MAX_ASSIGNMENTS_PER_WORK {
            return Err(refused("manager_review_assignment_limit"));
        }
        let assignment_id = Uuid::new_v4();
        let stamp = now();
        let prior: Option<(String, i64, String)> = self
            .conn
            .query_row(
                "SELECT assignment_id,row_version,request_json
                   FROM manager_review_assignments
                  WHERE project_id=?1 AND epic_id=?2 AND work_key=?3
                    AND spec_revision=?4 AND source_sha=?5
                    AND superseded_by_assignment_id IS NULL",
                params![
                    config.project_id.to_string(),
                    work.epic_id.to_string(),
                    work.key,
                    work.spec_revision,
                    source_commit
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let prior = match prior {
            Some((prior_id, prior_version, prior_json)) => {
                let prior_request: ReviewAllocationRequest = serde_json::from_str(&prior_json)
                    .map_err(|_| refused("manager_review_stored_request"))?;
                self.require_review_supersede_owner(
                    config,
                    caller,
                    prior_request.requester_session_id,
                )?;
                Some((parse_uuid(prior_id)?, prior_version))
            }
            None => None,
        };
        // #599 A1: the budget counts attempt chains across supersession; a
        // same-SHA re-request starts a new attempt.
        let rows = self.review_attempt_rows(
            config.project_id,
            work.epic_id,
            &work.key,
            work.spec_revision,
        )?;
        let counted = counted_review_attempts(&rows, prior.map(|(id, _)| id));
        if counted.len() >= MAX_REVIEW_ROUNDS_PER_REVISION {
            return Err(refused("manager_review_round_budget"));
        }
        if counted.len() + 1 == MAX_REVIEW_ROUNDS_PER_REVISION
            && counted
                .iter()
                .flatten()
                .any(|row| row.request.launch.model == launch.model)
        {
            return Err(refused("manager_review_closure_specialist_required"));
        }
        let contributors = self.review_contributors(
            config.project_id,
            work.epic_id,
            &work.key,
            author,
            None,
            &[],
            &[],
        )?;
        let override_key =
            self.review_family_override(config, &work.key, work.spec_revision, None)?;
        self.require_review_contributor_family(
            config.project_id,
            &work.key,
            work.spec_revision,
            &contributors,
            override_key.as_deref(),
            review_model_family(launch.provider, Some(launch.model.as_str())),
        )?;
        if let Some((prior_id, prior_version)) = prior {
            self.conn.execute(
                "UPDATE manager_review_assignments
                    SET state='superseded',row_version=?2,
                        superseded_by_assignment_id=?3,failure_code=NULL,
                        updated_at=?4,terminal_at=COALESCE(terminal_at,?4)
                  WHERE assignment_id=?1",
                params![
                    prior_id.to_string(),
                    prior_version + 1,
                    assignment_id.to_string(),
                    stamp
                ],
            )?;
        }
        let allocation = ReviewAllocationRequest {
            requester_session_id: caller,
            fence: request.fence.clone(),
            query: query.clone(),
            launch: launch.clone(),
            infra_retry_of: None,
            contributor_session_ids: contributors.sessions.iter().copied().collect(),
            contributor_families: contributors.families.iter().cloned().collect(),
            family_override_key: override_key,
        };
        let request_json = serde_json::to_value(&allocation)?;
        self.conn.execute(
            "INSERT INTO manager_review_assignments(
                assignment_id,project_id,epic_id,manager_session_id,scope_version,work_key,
                spec_revision,author_session_id,source_sha,state,row_version,request_json,
                request_fingerprint,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'reserved',1,?10,?11,?12,?12)",
            params![
                assignment_id.to_string(),
                config.project_id.to_string(),
                work.epic_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                work.key,
                work.spec_revision,
                author.to_string(),
                source_commit,
                serde_json::to_string(&request_json)?,
                fingerprint(&request_json)?,
                stamp
            ],
        )?;
        let receipt = ManagerMutationReceiptV2 {
            event_sequence: self.manager_v2_event(
                config,
                Some(caller),
                "review_assignment",
                &assignment_id.to_string(),
                1,
                &json!({"assignment_id":assignment_id,"work_key":work.key,"spec_revision":work.spec_revision,"source_commit":source_commit,"state":"reserved"}),
            )?,
            key: assignment_id.to_string(),
            row_version: 1,
            deduplicated: false,
        };
        #[cfg(test)]
        manager_review_fault(ManagerReviewFault::AfterReservation)?;
        Ok(receipt)
    }

    /// Test fixture: the exact request (key and payload) the allocator would
    /// journal, as a manager could reconstruct it from its own request.
    #[cfg(test)]
    pub(crate) fn manager_review_allocation_request_for_test(
        &self,
        assignment_id: Uuid,
    ) -> Result<AgentManagerControlRequestV2> {
        let assignment = self.manager_review_assignment(assignment_id)?;
        Ok(review_allocation_request(assignment_id, &assignment))
    }

    /// Test fixture: reserve the infra-retry successor exactly as an infra
    /// death does, without the live-custody seal probe.
    #[cfg(test)]
    pub(crate) fn manager_review_infra_retry_for_test(&self, assignment_id: Uuid) -> Result<Uuid> {
        let assignment = self.manager_review_assignment(assignment_id)?;
        let config = self
            .get_harness_manager(assignment.project_id)?
            .ok_or_else(|| refused("manager_review_scope_changed"))?;
        let tx = self.conn.unchecked_transaction()?;
        let contributors = self.review_contributors_for_assignment(&assignment)?;
        self.require_review_contributor_family(
            assignment.project_id,
            &assignment.work_key,
            assignment.spec_revision,
            &contributors,
            assignment.request.family_override_key.as_deref(),
            review_model_family(
                assignment.request.launch.provider,
                Some(&assignment.request.launch.model),
            ),
        )?;
        self.reserve_manager_review_retry_on(&assignment, &config, &contributors)?;
        tx.commit()?;
        let next: String = self.conn.query_row(
            "SELECT superseded_by_assignment_id FROM manager_review_assignments
              WHERE assignment_id=?1",
            [assignment_id.to_string()],
            |row| row.get(0),
        )?;
        parse_uuid(next)
    }

    pub(crate) fn allocate_manager_review_assignment(&self, assignment_id: Uuid) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let assignment = self.manager_review_assignment(assignment_id)?;
        if assignment.state != "reserved" {
            tx.commit()?;
            return Ok(false);
        }
        let expected_manager = assignment.manager_session_id;
        let expected_scope_version = assignment.scope_version;
        let config = self
            .get_harness_manager(assignment.project_id)?
            .filter(|config| {
                config.manager_session_id == expected_manager
                    && config.row_version == expected_scope_version
            })
            .ok_or_else(|| refused("manager_review_scope_changed"))?;
        let current_manager = config
            .current_session_id
            .ok_or_else(|| refused("manager_review_manager_unavailable"))?;
        let authority = self.manager_v2_authorize(
            current_manager,
            &assignment.request.fence,
            Some(ManagerCapabilityV2::SessionCreate),
        )?;
        if !authority
            .grant
            .policy
            .capabilities
            .contains(&ManagerCapabilityV2::WorkPlan)
        {
            return Err(refused("manager_v2_capability_denied"));
        }
        let (_, work) = self.manager_v2_work(&config, &assignment.work_key)?;
        if work.epic_id != assignment.epic_id
            || work.spec_revision != assignment.spec_revision
            || work.source_session_id != Some(assignment.author_session_id)
            || work.source_commit.as_deref() != Some(&assignment.source_sha)
        {
            return Err(refused("manager_review_work_changed"));
        }
        let author = self
            .get_session(assignment.author_session_id)?
            .filter(|session| {
                session.project_id == Some(assignment.project_id)
                    && rsi_common::is_leaf_kind(session.session_kind)
            })
            .ok_or_else(|| refused("manager_review_author_unavailable"))?;
        // #599 S1: the author remains the review identity; a rotation tip
        // may hold the live source custody used for the reviewer fork.
        let holder = self
            .manager_review_source_holder(&config, assignment.epic_id, &author)
            .map_err(|_| refused("manager_review_author_unavailable"))?;
        let control = review_allocation_request(assignment_id, &assignment);
        let action = self.enqueue_manager_review_session_on(
            ManagerActionOriginV2::Agent {
                caller: current_manager,
            },
            control,
            &holder,
            &assignment.source_sha,
        )?;
        #[cfg(test)]
        manager_review_fault(ManagerReviewFault::AfterAllocationJournal)?;
        let reviewer = action
            .target_session_id
            .ok_or_else(|| refused("manager_review_reviewer_unavailable"))?;
        if reviewer == assignment.author_session_id {
            return Err(refused("manager_review_self_review"));
        }
        self.conn.execute(
            "UPDATE manager_review_assignments
                SET reviewer_session_id=?2,action_operation_id=?3,state='allocating',
                    row_version=row_version+1,updated_at=?4
              WHERE assignment_id=?1 AND state='reserved'",
            params![
                assignment_id.to_string(),
                reviewer.to_string(),
                action.operation_id.to_string(),
                now()
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn fail_manager_review_assignment(
        &self,
        assignment: &ReviewAssignment,
        code: &str,
    ) -> Result<bool> {
        if !matches!(
            assignment.state.as_str(),
            "reserved" | "allocating" | "active"
        ) {
            return Ok(false);
        }
        let changed = self.conn.execute(
            "UPDATE manager_review_assignments
                SET state='failed',row_version=row_version+1,failure_code=?2,
                    updated_at=?3,terminal_at=?3
              WHERE assignment_id=?1 AND state=?4",
            params![
                assignment.assignment_id.to_string(),
                code,
                now(),
                assignment.state
            ],
        )?;
        if changed == 1 {
            self.record_manager_review_terminal_notice(assignment.assignment_id)?;
        }
        Ok(changed == 1)
    }

    /// Record the durable `to_manager` notice for an assignment that has just
    /// become `submitted` or `failed`, inside the caller's transition
    /// transaction. No resolvable live manager, in-scope Epic, or lead skips
    /// the notice without failing the transition.
    fn record_manager_review_terminal_notice(&self, assignment_id: Uuid) -> Result<Option<Uuid>> {
        let row: Option<ReviewNoticeRow> = self
            .conn
            .query_row(
                "SELECT a.project_id,a.epic_id,a.work_key,a.spec_revision,a.source_sha,a.state,
                        a.failure_code,a.reviewer_session_id,r.receipt_id,r.verdict,
                        COALESCE((SELECT count(*) FROM manager_review_findings f
                                  WHERE f.receipt_id=r.receipt_id),0),
                        COALESCE((SELECT count(*) FROM manager_review_findings f
                                  WHERE f.receipt_id=r.receipt_id AND f.blocking=1),0)
                   FROM manager_review_assignments a
                   LEFT JOIN manager_review_receipts r ON r.assignment_id=a.assignment_id
                  WHERE a.assignment_id=?1",
                [assignment_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            project,
            epic,
            work_key,
            spec_revision,
            source_sha,
            state,
            failure_code,
            reviewer,
            receipt_id,
            verdict,
            finding_count,
            blocking_finding_count,
        )) = row
        else {
            return Ok(None);
        };
        if !matches!(state.as_str(), "submitted" | "failed") {
            return Ok(None);
        }
        let mut notice = json!({
            "assignment_id":assignment_id,
            "work_key":work_key,
            "record_key":format!("work:{work_key}"),
            "spec_revision":spec_revision,
            "source_commit":source_sha,
            "state":state,
            "failure_code":failure_code,
            "reviewer_session_id":reviewer,
        });
        if state == "submitted" {
            notice["receipt_id"] = json!(receipt_id);
            notice["verdict"] = json!(verdict);
            notice["finding_count"] = json!(finding_count);
            notice["blocking_finding_count"] = json!(blocking_finding_count);
        }
        self.record_manager_review_notice(
            parse_uuid(project)?,
            parse_uuid(epic)?,
            assignment_id,
            &state,
            &notice,
        )
    }

    /// Reserve a fresh V121 successor for an infrastructure death. The outer
    /// refresh transaction makes supersession and reservation atomic; normal
    /// allocation then performs the same policy and resource checks as a
    /// manager-requested review.
    fn retry_manager_review_after_infra(&self, assignment: &ReviewAssignment) -> Result<bool> {
        let mut retry_count = 0;
        let mut current = assignment.clone();
        for _ in 0..MAX_ASSIGNMENTS_PER_WORK {
            let predecessor: Option<String> = self
                .conn
                .query_row(
                    "SELECT assignment_id FROM manager_review_assignments
                      WHERE superseded_by_assignment_id=?1",
                    [current.assignment_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(predecessor) = predecessor else {
                break;
            };
            let predecessor = parse_uuid(predecessor)?;
            if current.request.infra_retry_of == Some(predecessor) {
                retry_count += 1;
            }
            current = self.manager_review_assignment(predecessor)?;
        }
        if retry_count >= MAX_INFRA_RELAUNCHES {
            return self.fail_manager_review_assignment(
                assignment,
                "manager_review_infra_retry_exhausted",
            );
        }

        let Some(config) = self.get_harness_manager(assignment.project_id)? else {
            return self.fail_manager_review_assignment(
                assignment,
                "manager_review_infra_relaunch_refused",
            );
        };
        let contributors = match self.review_contributors_for_assignment(assignment) {
            Ok(set) => set,
            Err(crate::error::DaemonError::InvalidParam(code)) => {
                return self.fail_manager_review_assignment(assignment, &code);
            }
            Err(error) => return Err(error),
        };
        let family = review_model_family(
            assignment.request.launch.provider,
            Some(&assignment.request.launch.model),
        );
        match self.require_review_contributor_family(
            assignment.project_id,
            &assignment.work_key,
            assignment.spec_revision,
            &contributors,
            assignment.request.family_override_key.as_deref(),
            family,
        ) {
            Err(crate::error::DaemonError::InvalidParam(code)) => {
                return self.fail_manager_review_assignment(assignment, &code);
            }
            Err(error) => return Err(error),
            Ok(()) => {}
        }
        if let Some(code) = self.manager_review_retry_seal_failure(assignment, &config)? {
            return self.fail_manager_review_assignment(assignment, &code);
        }
        self.reserve_manager_review_retry_on(assignment, &config, &contributors)?;
        Ok(true)
    }

    fn manager_review_retry_seal_failure(
        &self,
        assignment: &ReviewAssignment,
        config: &HarnessManagerConfigV1,
    ) -> Result<Option<String>> {
        let Ok((_, work)) = self.manager_v2_work(config, &assignment.work_key) else {
            return Ok(Some("manager_review_source_changed".into()));
        };
        if work.epic_id != assignment.epic_id
            || work.spec_revision != assignment.spec_revision
            || work.source_session_id != Some(assignment.author_session_id)
            || work.source_commit.as_deref() != Some(&assignment.source_sha)
        {
            return Ok(Some("manager_review_source_changed".into()));
        }
        let Some(source) = self.get_session(assignment.author_session_id)? else {
            return Ok(Some("manager_review_infra_relaunch_refused".into()));
        };
        let Ok(holder) = self.manager_review_source_holder(config, assignment.epic_id, &source)
        else {
            return Ok(Some("manager_review_infra_relaunch_refused".into()));
        };
        let Ok(custody) = self.live_custody_for_session(holder.id) else {
            return Ok(Some("manager_review_infra_relaunch_refused".into()));
        };
        let root = std::path::Path::new(&custody.sandbox_root);
        if crate::sandbox::git_worktree::review_source_custody_holds_bounded(
            root,
            &custody.sandbox_branch,
            &custody.repository_identity,
        )
        .is_err()
        {
            return Ok(Some("manager_review_infra_relaunch_refused".into()));
        }
        // #599 A1: the reviewer forks the sealed commit object, so a dirty
        // author tree cannot change what the retry reviews.
        let Ok(head) = crate::sandbox::git_worktree::observe_head_bounded(root) else {
            return Ok(Some("manager_review_infra_relaunch_refused".into()));
        };
        if let Err(error) = crate::sandbox::git_worktree::review_sealed_source_holds_bounded(
            root,
            &assignment.source_sha,
            &head,
        ) {
            return Ok(Some(match error {
                crate::error::DaemonError::InvalidParam(code)
                    if code.starts_with("manager_review_source_changed") =>
                {
                    code
                }
                _ => "manager_review_infra_relaunch_refused".into(),
            }));
        }
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM manager_review_assignments
              WHERE project_id=?1 AND epic_id=?2 AND work_key=?3 AND spec_revision=?4",
            params![
                assignment.project_id.to_string(),
                assignment.epic_id.to_string(),
                assignment.work_key,
                assignment.spec_revision
            ],
            |row| row.get(0),
        )?;
        if count >= MAX_ASSIGNMENTS_PER_WORK {
            return Ok(Some("manager_review_infra_relaunch_refused".into()));
        }
        Ok(None)
    }

    fn reserve_manager_review_retry_on(
        &self,
        assignment: &ReviewAssignment,
        config: &HarnessManagerConfigV1,
        contributors: &contributors::ContributorSet,
    ) -> Result<()> {
        let next_id = Uuid::new_v4();
        let stamp = now();
        self.conn.execute(
            "UPDATE manager_review_assignments
                SET state='superseded',row_version=row_version+1,
                    superseded_by_assignment_id=?2,failure_code=NULL,
                    updated_at=?3,terminal_at=COALESCE(terminal_at,?3)
              WHERE assignment_id=?1 AND state=?4",
            params![
                assignment.assignment_id.to_string(),
                next_id.to_string(),
                stamp,
                assignment.state
            ],
        )?;
        let mut request = assignment.request.clone();
        request.infra_retry_of = Some(assignment.assignment_id);
        request.contributor_session_ids = contributors.sessions.iter().copied().collect();
        request.contributor_families = contributors.families.iter().cloned().collect();
        let request_json = serde_json::to_value(&request)?;
        self.conn.execute(
            "INSERT INTO manager_review_assignments(
                assignment_id,project_id,epic_id,manager_session_id,scope_version,work_key,
                spec_revision,author_session_id,source_sha,state,row_version,request_json,
                request_fingerprint,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'reserved',1,?10,?11,?12,?12)",
            params![
                next_id.to_string(),
                assignment.project_id.to_string(),
                assignment.epic_id.to_string(),
                assignment.manager_session_id.to_string(),
                assignment.scope_version,
                assignment.work_key,
                assignment.spec_revision,
                assignment.author_session_id.to_string(),
                assignment.source_sha,
                serde_json::to_string(&request_json)?,
                fingerprint(&request_json)?,
                stamp
            ],
        )?;
        self.manager_v2_event(
            config,
            None,
            "review_assignment",
            &next_id.to_string(),
            1,
            &json!({"assignment_id":next_id,"infra_retry_of":assignment.assignment_id,
                "work_key":assignment.work_key,"spec_revision":assignment.spec_revision,
                "source_commit":assignment.source_sha,"state":"reserved"}),
        )?;
        Ok(())
    }

    pub(crate) fn refresh_manager_review_assignment(&self, assignment_id: Uuid) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let assignment = self.manager_review_assignment(assignment_id)?;
        if !matches!(assignment.state.as_str(), "allocating" | "active") {
            tx.commit()?;
            return Ok(false);
        }
        self.require_current_manager_review_authority(&assignment)?;
        let reviewer = assignment
            .reviewer_session_id
            .ok_or_else(|| refused("manager_review_reviewer_unavailable"))?;
        let action_id = assignment
            .action_operation_id
            .ok_or_else(|| refused("manager_review_action_missing"))?;
        let action_state: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT state,json_extract(outcome_json,'$.outcome') FROM harness_manager_v2_operations
                  WHERE id=?1 AND target_session_id=?2 AND kind='lifecycle_action'",
                params![action_id.to_string(), reviewer.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if action_state.as_ref().is_some_and(|(state, _)| {
            matches!(
                state.as_str(),
                "failed" | "blocked" | "uncertain" | "revoked"
            )
        }) {
            let code = action_state
                .as_ref()
                .and_then(|(_, outcome)| outcome.as_deref())
                .unwrap_or("manager_review_allocation_failed");
            // A blocked launch is a policy/resource refusal, not an
            // infrastructure death. Preserve its exact #541 diagnostic.
            if action_state
                .as_ref()
                .is_some_and(|(state, _)| state == "blocked")
            {
                let changed = self.fail_manager_review_assignment(&assignment, code)?;
                tx.commit()?;
                return Ok(changed);
            }
            let changed = self.retry_manager_review_after_infra(&assignment)?;

            tx.commit()?;
            return Ok(changed);
        }
        let Some(session) = self.get_session(reviewer)? else {
            tx.commit()?;
            return Ok(false);
        };
        if session.project_id != Some(assignment.project_id)
            || session.parent_id != Some(assignment.epic_id)
            || !rsi_common::is_leaf_kind(session.session_kind)
            || reviewer == assignment.author_session_id
        {
            let changed = self
                .fail_manager_review_assignment(&assignment, "manager_review_identity_changed")?;
            tx.commit()?;
            return Ok(changed);
        }
        let Some(invocation) = self.session_model_invocation_id(reviewer)? else {
            tx.commit()?;
            return Ok(false);
        };
        let invocation_state: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT status,error_class FROM model_invocations
                  WHERE id=?1 AND session_id=?2 AND project_id=?3
                    AND admission_status='admitted' AND dedup_key=?4",
                params![
                    invocation.to_string(),
                    reviewer.to_string(),
                    assignment.project_id.to_string(),
                    format!("manager.action:{action_id}")
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((invocation_state, invocation_error_class)) = invocation_state else {
            tx.commit()?;
            return Ok(false);
        };
        let custody = match self.live_custody_for_session(reviewer) {
            Ok(custody) => custody,
            Err(_) => {
                if matches!(
                    session.status,
                    SessionStatus::Failed | SessionStatus::Interrupted
                ) {
                    let changed = self.retry_manager_review_after_infra(&assignment)?;
                    tx.commit()?;
                    return Ok(changed);
                }
                tx.commit()?;
                return Ok(false);
            }
        };
        if custody.source_commit != assignment.source_sha {
            let changed =
                self.fail_manager_review_assignment(&assignment, "manager_review_source_mismatch")?;
            tx.commit()?;
            return Ok(changed);
        }
        if assignment.state == "allocating" {
            self.conn.execute(
                "UPDATE manager_review_assignments
                    SET reviewer_invocation_id=?2,reviewer_custody_id=?3,
                        reviewer_custody_generation=?4,state='active',
                        row_version=row_version+1,updated_at=?5
                  WHERE assignment_id=?1 AND state='allocating'",
                params![
                    assignment_id.to_string(),
                    invocation.to_string(),
                    custody.custody_id.to_string(),
                    custody.generation as i64,
                    now()
                ],
            )?;
            tx.commit()?;
            return Ok(true);
        }
        if let Some(code) = receipt_missing_failure_code(
            &invocation_state,
            invocation_error_class.as_deref(),
            session.status,
        ) {
            let changed = if is_infra_review_end(code) {
                self.retry_manager_review_after_infra(&assignment)?
            } else {
                self.fail_manager_review_assignment(&assignment, code)?
            };
            tx.commit()?;
            return Ok(changed);
        }
        tx.commit()?;
        Ok(false)
    }

    /// Undelivered review-terminal notice transport for one assignment, for
    /// post-commit `ManagerNoticeQueued` publication.
    pub(crate) fn manager_review_notice_job(
        &self,
        assignment_id: Uuid,
        terminal_state: &str,
    ) -> Result<Option<Uuid>> {
        self.manager_notice_job_for_subject(
            "ledger_change",
            &format!("review:{assignment_id}"),
            terminal_state,
        )
    }

    /// One bounded reconciliation pass. Returns the changed-assignment count
    /// and every review-terminal notice transport queued by this pass, which
    /// the caller publishes after the store lock is released.
    pub(crate) fn reconcile_manager_review_assignments_once(&self) -> Result<(usize, Vec<Uuid>)> {
        let ids = {
            let mut statement = self.conn.prepare(
                "SELECT assignment_id,state FROM manager_review_assignments
                  WHERE state IN ('reserved','allocating','active')
                  ORDER BY created_at,assignment_id LIMIT ?1",
            )?;
            statement
                .query_map([REVIEW_RECONCILE_BATCH as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut changed = 0;
        let mut notice_jobs = Vec::new();
        for (id, state) in ids {
            let id = parse_uuid(id)?;
            let outcome = if state == "reserved" {
                self.allocate_manager_review_assignment(id)
            } else {
                self.refresh_manager_review_assignment(id)
            };
            match outcome {
                Ok(true) => {
                    changed += 1;
                    notice_jobs.extend(self.manager_review_notice_job(id, "failed")?);
                }
                Ok(false) => {}
                Err(error) => {
                    let assignment = self.manager_review_assignment(id)?;
                    let code = if assignment.request.infra_retry_of.is_some() {
                        match &error {
                            crate::error::DaemonError::InvalidParam(reason) => Some(
                                if reason.starts_with("manager_review_source_changed")
                                    || reason == "manager_review_work_changed"
                                {
                                    "manager_review_source_changed"
                                } else {
                                    "manager_review_infra_relaunch_refused"
                                },
                            ),
                            _ => None,
                        }
                    } else {
                        terminal_allocation_failure(&error)
                    };
                    if let Some(code) = code {
                        // The failed transition and its durable manager notice
                        // commit together.
                        let tx =
                            Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
                        let assignment = self.manager_review_assignment(id)?;
                        let failed = self.fail_manager_review_assignment(&assignment, code)?;
                        tx.commit()?;
                        if failed {
                            changed += 1;
                            notice_jobs.extend(self.manager_review_notice_job(id, "failed")?);
                        }
                        tracing::warn!(assignment_id=%id,%error,"manager review allocation failed terminally");
                    } else {
                        tracing::warn!(assignment_id=%id,%error,"manager review reconciliation deferred");
                    }
                }
            }
        }
        notice_jobs.sort_unstable();
        notice_jobs.dedup();
        Ok((changed, notice_jobs))
    }

    fn manager_review_existing_receipt(
        &self,
        caller: Uuid,
        request: &AgentSubmitReviewReceiptRequestV1,
    ) -> Result<Option<ManagerReviewReceiptV1>> {
        let fingerprint = review_request_fingerprint(request)?;
        let conflicting_assignment: Option<String> = self
            .conn
            .query_row(
                "SELECT assignment_id FROM manager_review_receipts
                  WHERE reviewer_session_id=?1 AND idempotency_key=?2",
                params![caller.to_string(), request.idempotency_key],
                |row| row.get(0),
            )
            .optional()?;
        if conflicting_assignment
            .as_deref()
            .is_some_and(|assignment| assignment != request.assignment_id.to_string())
        {
            return Err(refused("manager_review_idempotency_conflict"));
        }
        let row: Option<(String, String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT receipt_id,source_sha,verdict,request_fingerprint,created_at
                   FROM manager_review_receipts
                  WHERE assignment_id=?1 AND reviewer_session_id=?2",
                params![request.assignment_id.to_string(), caller.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, source, verdict, stored_fingerprint, created)) = row else {
            return Ok(None);
        };
        let stored_key: String = self.conn.query_row(
            "SELECT idempotency_key FROM manager_review_receipts WHERE receipt_id=?1",
            [&id],
            |row| row.get(0),
        )?;
        if stored_key != request.idempotency_key || stored_fingerprint != fingerprint {
            return Err(refused("manager_review_idempotency_conflict"));
        }
        let verdict = match verdict.as_str() {
            "accepted" => ManagerReviewVerdictV1::Accepted,
            "changes_requested" => ManagerReviewVerdictV1::ChangesRequested,
            "blocked" => ManagerReviewVerdictV1::Blocked,
            _ => return Err(refused("manager_review_stored_verdict")),
        };
        Ok(Some(ManagerReviewReceiptV1 {
            receipt_id: parse_uuid(id)?,
            assignment_id: request.assignment_id,
            source_commit: source,
            verdict,
            request_fingerprint: stored_fingerprint,
            created_at: DateTime::parse_from_rfc3339(&created)
                .map_err(|_| refused("manager_review_stored_timestamp"))?
                .with_timezone(&Utc),
            deduplicated: true,
        }))
    }

    pub(crate) fn prepare_manager_review_submission(
        &self,
        caller: Uuid,
        request: &AgentSubmitReviewReceiptRequestV1,
    ) -> Result<ReviewSubmissionObservation> {
        request.validate().map_err(refused)?;
        if self
            .manager_review_existing_receipt(caller, request)?
            .is_some()
        {
            return Err(refused("manager_review_receipt_already_submitted"));
        }
        let assignment = self.manager_review_assignment(request.assignment_id)?;
        self.require_current_manager_review_authority(&assignment)?;
        if assignment.state != "active"
            || assignment.reviewer_session_id != Some(caller)
            || caller == assignment.author_session_id
        {
            return Err(refused("manager_review_reviewer_required"));
        }
        let invocation = assignment
            .reviewer_invocation_id
            .ok_or_else(|| refused("manager_review_invocation_unavailable"))?;
        let session = self
            .get_session(caller)?
            .filter(|session| {
                session.project_id == Some(assignment.project_id)
                    && session.parent_id == Some(assignment.epic_id)
                    && matches!(
                        session.status,
                        SessionStatus::Starting | SessionStatus::Running
                    )
            })
            .ok_or_else(|| refused("manager_review_reviewer_changed"))?;
        if self.session_model_invocation_id(caller)? != Some(invocation) {
            return Err(refused("manager_review_reviewer_changed"));
        }
        let running: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_invocations
              WHERE id=?1 AND session_id=?2 AND status='running' AND admission_status='admitted')",
            params![invocation.to_string(), session.id.to_string()],
            |row| row.get(0),
        )?;
        if !running {
            return Err(refused("manager_review_invocation_unfinished"));
        }
        let custody = self.live_custody_for_session(caller)?;
        if assignment.reviewer_custody_id != Some(custody.custody_id)
            || assignment.reviewer_custody_generation != Some(custody.generation)
            || custody.source_commit != assignment.source_sha
        {
            return Err(refused("manager_review_custody_changed"));
        }
        self.review_receipt_contributor_gate(&assignment, caller)?;
        Ok(ReviewSubmissionObservation {
            assignment_id: assignment.assignment_id,
            reviewer_session_id: caller,
            reviewer_invocation_id: invocation,
            source_sha: assignment.source_sha,
            custody,
        })
    }

    pub(crate) fn manager_review_receipt_replay(
        &self,
        caller: Uuid,
        request: &AgentSubmitReviewReceiptRequestV1,
    ) -> Result<Option<ManagerReviewReceiptV1>> {
        request.validate().map_err(refused)?;
        self.manager_review_existing_receipt(caller, request)
    }

    pub(crate) fn commit_manager_review_submission(
        &self,
        caller: Uuid,
        request: &AgentSubmitReviewReceiptRequestV1,
        observed: &ReviewSubmissionObservation,
    ) -> Result<ManagerReviewReceiptV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(receipt) = self.manager_review_existing_receipt(caller, request)? {
            tx.commit()?;
            return Ok(receipt);
        }
        let assignment = self.manager_review_assignment(request.assignment_id)?;
        self.require_current_manager_review_authority(&assignment)?;
        if assignment.state != "active"
            || assignment.assignment_id != observed.assignment_id
            || assignment.reviewer_session_id != Some(caller)
            || assignment.reviewer_invocation_id != Some(observed.reviewer_invocation_id)
            || assignment.reviewer_custody_id != Some(observed.custody.custody_id)
            || assignment.reviewer_custody_generation != Some(observed.custody.generation)
            || assignment.source_sha != observed.source_sha
            || caller != observed.reviewer_session_id
            || caller == assignment.author_session_id
        {
            return Err(refused("manager_review_assignment_changed"));
        }
        let current = self.live_custody_for_session(caller)?;
        if current.custody_id != observed.custody.custody_id
            || current.generation != observed.custody.generation
            || current.source_commit != observed.source_sha
        {
            return Err(refused("manager_review_custody_changed"));
        }
        let invocation_live: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions s JOIN model_invocations m
                ON m.id=s.model_invocation_id AND m.session_id=s.id
              WHERE s.id=?1 AND s.model_invocation_id=?2 AND m.status='running'
                AND m.admission_status='admitted')",
            params![
                caller.to_string(),
                observed.reviewer_invocation_id.to_string()
            ],
            |row| row.get(0),
        )?;
        if !invocation_live {
            return Err(refused("manager_review_invocation_changed"));
        }
        self.review_receipt_contributor_gate(&assignment, caller)?;
        let fingerprint = review_request_fingerprint(request)?;
        let receipt_id = Uuid::new_v4();
        let stamp = now();
        self.conn.execute(
            "INSERT INTO manager_review_receipts(
                receipt_id,assignment_id,source_sha,reviewer_session_id,
                reviewer_invocation_id,reviewer_custody_id,
                reviewer_custody_generation,verdict,idempotency_key,
                request_fingerprint,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                receipt_id.to_string(),
                assignment.assignment_id.to_string(),
                assignment.source_sha,
                caller.to_string(),
                observed.reviewer_invocation_id.to_string(),
                observed.custody.custody_id.to_string(),
                observed.custody.generation as i64,
                verdict_name(request.verdict),
                request.idempotency_key,
                &fingerprint,
                &stamp
            ],
        )?;
        let mut findings = request.findings.clone();
        findings.sort_by(|left, right| left.key.cmp(&right.key));
        for finding in findings {
            self.conn.execute(
                "INSERT INTO manager_review_findings(
                    receipt_id,finding_key,severity,summary,location,blocking)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    receipt_id.to_string(),
                    finding.key,
                    severity_name(finding.severity),
                    finding.summary,
                    finding.location,
                    finding.blocking
                ],
            )?;
        }
        let submitted = self.conn.execute(
            "UPDATE manager_review_assignments
                SET state='submitted',row_version=row_version+1,updated_at=?2,terminal_at=?2
              WHERE assignment_id=?1 AND state='active'",
            params![assignment.assignment_id.to_string(), stamp],
        )?;
        if submitted == 1 {
            self.record_manager_review_terminal_notice(assignment.assignment_id)?;
        }
        #[cfg(test)]
        manager_review_fault(ManagerReviewFault::BeforeReceiptCommit)?;
        tx.commit()?;
        Ok(ManagerReviewReceiptV1 {
            receipt_id,
            assignment_id: assignment.assignment_id,
            source_commit: observed.source_sha.clone(),
            verdict: request.verdict,
            request_fingerprint: fingerprint,
            created_at: DateTime::parse_from_rfc3339(&stamp)
                .map_err(|_| refused("manager_review_stored_timestamp"))?
                .with_timezone(&Utc),
            deduplicated: false,
        })
    }

    /// Single representation-neutral admission seam consumed by integration.
    /// Enrollment of this exact work/spec/source makes DB state authoritative;
    /// only never-enrolled sources delegate to the retained legacy projection.
    pub(crate) fn manager_v2_accepted_source(
        &self,
        config: &HarnessManagerConfigV1,
        work: &WorkRecord,
        source_sha: &str,
    ) -> Result<Option<Acceptance>> {
        if work.source_commit.as_deref() != Some(source_sha) || !canonical_sha(source_sha) {
            return Ok(None);
        }
        let enrolled = self.manager_review_enrolled(config, work, source_sha)?;
        if !enrolled {
            return Ok(work.acceptance.clone().filter(|acceptance| {
                acceptance.spec_revision == work.spec_revision
                    && acceptance.source_commit == source_sha
            }));
        }
        #[cfg(test)]
        manager_review_fault(ManagerReviewFault::BeforeAdmissionRead)?;
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT r.request_fingerprint,r.created_at
                   FROM manager_review_assignments a
                   JOIN manager_review_receipts r ON r.assignment_id=a.assignment_id
                   JOIN sessions reviewer ON reviewer.id=a.reviewer_session_id
                   JOIN model_invocations invocation ON invocation.id=a.reviewer_invocation_id
                        AND invocation.session_id=reviewer.id
                   JOIN sandbox_custody_roots custody ON custody.custody_id=a.reviewer_custody_id
                  WHERE a.project_id=?1 AND a.epic_id=?2 AND a.work_key=?3
                    AND a.spec_revision=?4 AND a.source_sha=?5
                    AND a.superseded_by_assignment_id IS NULL AND a.state='submitted'
                    AND r.source_sha=a.source_sha AND r.verdict='accepted'
                    AND r.reviewer_session_id=a.reviewer_session_id
                    AND r.reviewer_invocation_id=a.reviewer_invocation_id
                    AND r.reviewer_custody_id=a.reviewer_custody_id
                    AND r.reviewer_custody_generation=a.reviewer_custody_generation
                    AND a.reviewer_session_id!=a.author_session_id
                    AND reviewer.project_id=a.project_id AND reviewer.parent_id=a.epic_id
                    AND reviewer.model_invocation_id=a.reviewer_invocation_id
                    AND reviewer.sandbox_custody_id=a.reviewer_custody_id
                    AND reviewer.status='Completed'
                    AND invocation.status='completed' AND invocation.completed_at IS NOT NULL
                    AND custody.custody_id=a.reviewer_custody_id
                    AND custody.generation=a.reviewer_custody_generation
                    AND custody.source_commit=a.source_sha
                    AND EXISTS(SELECT 1 FROM harness_manager_v2_operations operation
                        WHERE operation.id=a.action_operation_id
                          AND operation.target_session_id=a.reviewer_session_id
                          AND operation.state='succeeded')
                    AND NOT EXISTS(SELECT 1 FROM manager_review_findings finding
                        WHERE finding.receipt_id=r.receipt_id AND finding.blocking=1)",
                params![
                    config.project_id.to_string(),
                    work.epic_id.to_string(),
                    work.key,
                    work.spec_revision,
                    source_sha
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((fingerprint, accepted_at)) = row else {
            return Ok(None);
        };
        #[cfg(test)]
        manager_review_fault(ManagerReviewFault::AfterReviewerTerminalObservation)?;
        Ok(Some(Acceptance {
            source_commit: source_sha.to_string(),
            spec_revision: work.spec_revision,
            evidence_digest: fingerprint,
            method: "db_review_receipt".into(),
            accepted_at,
        }))
    }

    pub(crate) fn manager_review_projection(
        &self,
        config: &HarnessManagerConfigV1,
        work: &WorkRecord,
    ) -> Result<Value> {
        let Some(source) = work.source_commit.as_deref() else {
            return Ok(json!({"mode":"legacy","current":null,"historical":[]}));
        };
        let mut statement = self.conn.prepare(
            "SELECT a.assignment_id,a.source_sha,a.state,a.row_version,
                    a.reviewer_session_id,a.reviewer_invocation_id,
                    a.reviewer_custody_id,a.reviewer_custody_generation,
                    a.superseded_by_assignment_id,a.failure_code,a.created_at,a.terminal_at,
                    r.receipt_id,r.verdict,r.request_fingerprint,r.created_at,
                    COALESCE((SELECT count(*) FROM manager_review_findings f
                              WHERE f.receipt_id=r.receipt_id),0),
                    COALESCE((SELECT count(*) FROM manager_review_findings f
                              WHERE f.receipt_id=r.receipt_id AND f.blocking=1),0)
               FROM manager_review_assignments a
               LEFT JOIN manager_review_receipts r ON r.assignment_id=a.assignment_id
              WHERE a.project_id=?1 AND a.epic_id=?2 AND a.work_key=?3
                AND a.spec_revision=?4
              ORDER BY a.created_at DESC,a.assignment_id DESC LIMIT ?5",
        )?;
        let raw = statement
            .query_map(
                params![
                    config.project_id.to_string(),
                    work.epic_id.to_string(),
                    work.key,
                    work.spec_revision,
                    MAX_ASSIGNMENTS_PER_WORK
                ],
                |row| {
                    let superseded_by = row.get::<_, Option<String>>(8)?;
                    Ok(json!({
                        "assignment_id":row.get::<_,String>(0)?,
                        "source_commit":row.get::<_,String>(1)?,
                        "state":row.get::<_,String>(2)?,
                        "row_version":row.get::<_,i64>(3)?,
                        "reviewer_session_id":row.get::<_,Option<String>>(4)?,
                        "reviewer_invocation_id":row.get::<_,Option<String>>(5)?,
                        "reviewer_custody_id":row.get::<_,Option<String>>(6)?,
                        "reviewer_custody_generation":row.get::<_,Option<i64>>(7)?,
                        "current":superseded_by.is_none(),
                        "superseded_by_assignment_id":superseded_by,
                        "failure_code":row.get::<_,Option<String>>(9)?,
                        "created_at":row.get::<_,String>(10)?,
                        "terminal_at":row.get::<_,Option<String>>(11)?,
                        "receipt_id":row.get::<_,Option<String>>(12)?,
                        "verdict":row.get::<_,Option<String>>(13)?,
                        "request_fingerprint":row.get::<_,Option<String>>(14)?,
                        "receipt_created_at":row.get::<_,Option<String>>(15)?,
                        "finding_count":row.get::<_,i64>(16)?,
                        "blocking_finding_count":row.get::<_,i64>(17)?,
                    }))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let accepted = self
            .manager_v2_accepted_source(config, work, source)?
            .is_some();
        let mut current = None;
        let mut historical = Vec::new();
        for mut row in raw {
            let is_current = row["current"] == true && row["source_commit"] == source;
            row["eligible"] = json!(is_current && accepted);
            if is_current && current.is_none() {
                current = Some(row);
            } else {
                historical.push(row);
            }
        }
        Ok(json!({
            "mode":if current.is_some() || !historical.is_empty() {"db_native"} else {"legacy"},
            "source_commit":source,
            "accepted":accepted,
            "current":current,
            "historical":historical,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_missing_code_types_each_terminal_cause() {
        let cases = [
            // Interrupted (daemon restart / operator stop): invocation settles
            // failed/interrupted from the session status.
            (
                "failed",
                Some("interrupted"),
                SessionStatus::Interrupted,
                "manager_review_receipt_missing_interrupted",
            ),
            (
                "cancelled",
                Some("operator_cancelled"),
                SessionStatus::Interrupted,
                "manager_review_receipt_missing_interrupted",
            ),
            (
                "failed",
                Some("cancelled_after_restart"),
                SessionStatus::Failed,
                "manager_review_receipt_missing_interrupted",
            ),
            // Provider failure such as a 403 key-limit ends the session Failed.
            (
                "failed",
                Some("failed"),
                SessionStatus::Failed,
                "manager_review_receipt_missing_provider_failed",
            ),
            (
                "failed",
                Some("over_budget_actual_exceeded"),
                SessionStatus::Completed,
                "manager_review_receipt_missing_budget_exceeded",
            ),
            // Normal final turn that never submitted a receipt.
            (
                "completed",
                None,
                SessionStatus::Completed,
                "manager_review_receipt_missing_final_without_receipt",
            ),
            // Session observed terminal before invocation settlement.
            (
                "running",
                None,
                SessionStatus::Interrupted,
                "manager_review_receipt_missing_interrupted",
            ),
            (
                "running",
                None,
                SessionStatus::Failed,
                "manager_review_receipt_missing_provider_failed",
            ),
            (
                "running",
                None,
                SessionStatus::Completed,
                "manager_review_receipt_missing_final_without_receipt",
            ),
        ];
        for (status, class, session, expected) in cases {
            assert_eq!(
                receipt_missing_failure_code(status, class, session),
                Some(expected),
                "{status}/{class:?}/{session:?}"
            );
            assert!(expected.starts_with("manager_review_receipt_missing_"));
            assert!(expected.len() <= 128, "failure_code column bound");
        }
    }

    #[test]
    fn receipt_missing_code_keeps_final_without_receipt_even_after_tool_denials() {
        for (status, session) in [
            ("completed", SessionStatus::Completed),
            ("running", SessionStatus::Completed),
        ] {
            assert_eq!(
                receipt_missing_failure_code(status, None, session),
                Some("manager_review_receipt_missing_final_without_receipt")
            );
        }
        assert_eq!(
            receipt_missing_failure_code("failed", Some("interrupted"), SessionStatus::Interrupted),
            Some("manager_review_receipt_missing_interrupted")
        );
    }

    #[test]
    fn receipt_missing_code_waits_while_review_is_live() {
        for session in [
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
        ] {
            assert_eq!(receipt_missing_failure_code("running", None, session), None);
        }
    }

    #[test]
    fn reviewer_prompt_states_bounded_receipt_first_contract_before_query() {
        let assignment_id = Uuid::new_v4();
        let source = "a".repeat(40);
        let query = "CALLER-QUERY: also check rendering";
        let prompt = manager_review_launch_prompt(assignment_id, &source, "product", 3, query);
        for clause in [
            format!("DB-native independent review assignment {assignment_id}."),
            format!("Review exact source {source} for manager work `product` revision 3."),
            "the budget bounds optional exploration only, never required checks".to_string(),
            "Required checks first: the fixed minimum".to_string(),
            "run the focused tests of the touched modules".to_string(),
            "plus every check the caller request below requires".to_string(),
            "Do all required checks before any optional exploration.".to_string(),
            "if required checks remain at the deadline, apply clause 5".to_string(),
            "Closeout budget: target 30 minutes and at most 150 tool calls".to_string(),
            "closeout deadline 60 minutes elapsed".to_string(),
            "This is a closeout rule, not a race: when you reach the target, stop optional work and submit"
                .to_string(),
            "At the closeout deadline, at tool-call budget exhaustion, or on any blocker, submit verdict `blocked`"
                .to_string(),
            "submit your receipt as soon as all required checks (fixed minimum and caller-required) are done"
                .to_string(),
            "Optional exploration only after the receipt is submitted".to_string(),
            "Never run destructive cleanup (rm -rf".to_string(),
            "Use cargo clean for build directories and leave scratch files in place".to_string(),
            "submit verdict `blocked` listing completed checks, unchecked required checks (including caller-required ones), and the reason"
                .to_string(),
            "Never end this invocation without a receipt.".to_string(),
        ] {
            assert!(
                prompt.contains(&clause),
                "missing clause: {clause}\n{prompt}"
            );
        }
        let contract = prompt.find("Review contract");
        let caller = prompt.find(query);
        assert!(
            matches!((contract, caller), (Some(contract), Some(caller)) if contract < caller),
            "fixed contract precedes caller query"
        );
        assert!(prompt.ends_with(query));
    }
}
