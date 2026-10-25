//! Durable, attributed manager lifecycle commands. V104 is the only storage
//! surface: admission reserves identity, execution claims before effects, and
//! lost execution ownership becomes uncertain rather than being replayed.

use super::Store;
use super::harness_manager_v2::{ManagerAuthorityV2, fingerprint, now, refused, session_changed};
use super::sessions::historical_session_restore_blocked_on;
use crate::error::{DaemonError, Result};
use chrono::Utc;
use rsi_common::harness_manager_v2::*;
use rsi_common::manager_budget::{BudgetNodeKindV1, CreationCapRefusalV1, CreationCapUsageV1};
use rsi_common::manager_operator_delegation::DelegatedOperatorCallV1;
use rsi_common::types::{SandboxCleanupState, SandboxKind, Session, SessionKind, SessionStatus};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

/// #1301: the charged creations `head` pays for: those since its epoch
/// began whose origin lineage includes it (or that no node originated).
pub(crate) fn head_created_count(
    charges: &[(String, Option<Vec<Uuid>>)],
    head: &super::harness_manager_v2::PortfolioHead,
) -> i64 {
    charges
        .iter()
        .filter(|(created, lineage)| {
            created.as_str() >= head.started.as_str()
                && lineage
                    .as_ref()
                    .is_none_or(|lineage| lineage.contains(&head.node_id))
        })
        .count() as i64
}

/// #3416: a session creation count over a rolling window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreationWindowUsage {
    /// Creations inside the window.
    pub used: i64,
    /// Every charged creation, window or not (informational).
    pub lifetime: i64,
    pub window_hours: u16,
    /// RFC3339: when the next creation leaves the window and a slot frees.
    pub next_free_at: Option<String>,
}

/// The `created_at` floor of the window ending at `now`.
pub(crate) fn window_floor(now: chrono::DateTime<Utc>, window_hours: u16) -> String {
    (now - chrono::Duration::hours(i64::from(window_hours)))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// [`CreationWindowUsage`] over `times` (RFC3339, ascending, all charged
/// creations of one counter).
pub(crate) fn window_usage(
    times: &[String],
    window_hours: u16,
    limit: i64,
    now: chrono::DateTime<Utc>,
) -> CreationWindowUsage {
    let floor = window_floor(now, window_hours);
    let first = times.partition_point(|t| t.as_str() < floor.as_str());
    let inside = &times[first..];
    let used = inside.len() as i64;
    // At or over the limit, admission resumes when enough of the oldest have
    // left to put the count back under it; under it, the count next drops
    // when the oldest leaves.
    let index = if used >= limit {
        usize::try_from(used.saturating_sub(limit)).unwrap_or(0)
    } else {
        0
    };
    let next_free_at = inside.get(index).and_then(|t| {
        chrono::DateTime::parse_from_rfc3339(t).ok().map(|t| {
            (t.with_timezone(&Utc) + chrono::Duration::hours(i64::from(window_hours)))
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        })
    });
    CreationWindowUsage {
        used,
        lifetime: times.len() as i64,
        window_hours,
        next_free_at,
    }
}

/// The `limit` [`window_usage`] takes so that `next_free_at` is when `needed`
/// creations (of which `counted` are already charged) fit under `cap`: the
/// count is at or over it exactly when `used - counted + needed > cap`.
pub(crate) fn creation_free_limit(cap: u16, needed: i64, counted: i64) -> i64 {
    i64::from(cap)
        .saturating_sub(needed)
        .saturating_add(counted)
        .saturating_add(1)
}

/// [`head_created_count`] as the ascending `created_at` of each charge, for
/// the rolling creation window (#3416).
pub(crate) fn head_created_times(
    charges: &[(String, Option<Vec<Uuid>>)],
    head: &super::harness_manager_v2::PortfolioHead,
) -> Vec<String> {
    let mut times: Vec<String> = charges
        .iter()
        .filter(|(created, lineage)| {
            created.as_str() >= head.started.as_str()
                && lineage
                    .as_ref()
                    .is_none_or(|lineage| lineage.contains(&head.node_id))
        })
        .map(|(created, _)| created.clone())
        .collect();
    times.sort();
    times
}

/// One of the caller's own creation counts with its #3416 window facts.
struct OwnCreationUsage {
    used: i64,
    lifetime: Option<i64>,
    window_hours: Option<u16>,
    next_free_at: Option<String>,
}

/// The policy field a creation budget charges (#2502).
fn creation_cap_field(container: bool) -> &'static str {
    if container {
        "max_created_containers"
    } else {
        "max_created_sessions"
    }
}

#[cfg(test)]
mod eligibility_tests;
pub mod fence;
mod issue_worker;
pub mod issue_worker_capacity;
mod issue_worker_evidence;
mod issue_worker_jobs;
mod operator_delegation;
mod queued_create;
mod recovery;
mod sandbox_source;
mod superseded_wakes;

pub(crate) const ACTION_KIND: &str = "lifecycle_action";
/// #1118: independent `create_session` launches in one project may be
/// `running` at once up to this many. Build-cache reclaim may bypass running
/// creates (#434); every other action keeps the one-active-command fence.
pub const MANAGER_CREATE_PARALLELISM: i64 = 3;
/// #1143: receipt outcome (state `uncertain`) of a launch whose deadline
/// could not prove the candidate's process and task exited.
pub const MANAGER_LAUNCH_DRAIN_PENDING: &str = "manager_v2_launch_drain_pending";
/// #1143: the same receipt once the sweep proved the drain and settled the rows.
pub const MANAGER_LAUNCH_DRAIN_VERIFIED: &str = "manager_v2_launch_drain_verified";
const CONTEXT_KIND: &str = "lifecycle_context";
const EXECUTION_KIND: &str = "lifecycle_execution";
const MAX_PENDING: i64 = 64;
/// Durable witness that a terminal/idle lead's exact agent-declared program
/// outcome was superseded by an explicit manager recovery (K2, #390).
pub const LEAD_RETIREMENT_KIND: &str = "lead_continuation_retirement";
pub const RETIREMENT_PAUSE_REASON: &str = "retire_lead_continuations";
/// Settlement outcome codes. The original receipt and evidence are retained;
/// settlement is appended and never re-executes an effect.
pub const SETTLED_EFFECT_ABSENT: &str = "manager_v2_recovered_effect_absent";
pub const SETTLED_PAUSE_CONFIRMED: &str = "lead_paused_reconciled";
const UNCERTAIN_RECOVERY_BATCH: i64 = 16;
const MAX_SETTLE_NESTING: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperatorPause {
    None,
    Soft,
    Hard,
}

/// Read model behind `GetOperatorPause` (#1541).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorPauseDetail {
    pub level: OperatorPause,
    /// RFC3339 time the marker was last written (inherited markers carry the
    /// publication time).
    pub since: Option<String>,
    pub held_succession: bool,
}

impl OperatorPause {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "false",
            Self::Soft => "soft",
            Self::Hard => "hard",
        }
    }
}

/// A lead pause prevents continuations, not inert delegated cleanup or reads.
fn delegated_cleanup_allowed_during_lead_pause(action: &ManagerActionV2) -> bool {
    matches!(
        action,
        ManagerActionV2::OperatorCall { call, .. }
            if matches!(
                call.typed(),
                Ok(DelegatedOperatorCallV1::ArchiveSession(_)
                    | DelegatedOperatorCallV1::UnarchiveSession(_)
                    | DelegatedOperatorCallV1::ListSessions(_)
                    | DelegatedOperatorCallV1::GetArchiveCleanupStatus(_))
            )
    )
}
/// Idempotency-key namespace owned by the DB-native review allocator (#674
/// K15A-1). Only the allocator journals keys in it, so an operation linked by
/// `manager_review_assignments.action_operation_id` is a genuine review launch.
pub(super) const REVIEW_ALLOCATION_KEY_PREFIX: &str = "manager-review-allocation:";
/// Alias `o` is an operation row. A blocked or revoked operation whose target
/// session was never created consumed nothing and is not charged; queued,
/// running, succeeded, failed and uncertain operations stay charged. This
/// includes `succeed_manager` (#1390): a succession refused before its
/// candidate existed releases its charge, so a retried succession does not
/// burn `max_created_sessions`.
const CREATION_CHARGED: &str = "NOT (o.state IN ('blocked','revoked') AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=o.target_session_id))";

/// #3495: the instant a charged operation `o` counts from in the rolling
/// creation window, as a SQL expression: the `now` parameter (for example
/// `?7`) goes between [`CREATION_AT_HEAD`] and [`CREATION_AT_TAIL`]. A queued
/// or running operation has created nothing yet: its reservation stays charged as of `now`, however long it was held
/// back, until it settles. Once created, the charge starts at the actual
/// creation, once: the bound entity (`create_session`, lead replacement), or
/// a succession's candidate session. Any other settled operation counts from
/// the moment it settled.
const CREATION_AT_HEAD: &str = "(CASE WHEN o.state IN ('queued','running') THEN ";
/// See [`CREATION_AT_HEAD`].
const CREATION_AT_TAIL: &str = " ELSE COALESCE((SELECT e.created_at FROM harness_manager_v2_entities e WHERE e.operation_id=o.id),(SELECT cs.created_at FROM manager_root_successions r JOIN sessions cs ON cs.id=r.candidate_session_id WHERE r.operation_id=o.id),o.updated_at) END)";

/// Which caller runs [`Store::recovery_owner_gate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryOwnerMode {
    /// A manager lifecycle action. Byte-identical to the pre-extraction
    /// `manager_action_human_gate_with_interrupted_resume`.
    ManagerAction {
        allow_interrupted_resume: bool,
        allow_soft_operator_pause: bool,
    },
    /// A terminal manager archive leaves an operator interruption marker in
    /// place for a later restore or continuation. It may settle C5 and resume
    /// owners without restarting the session.
    ManagerArchive,
    /// `AgentArchiveChild`: the strictest program mode, with no
    /// interrupted-resume exception, and no hold on the C5 marker that the
    /// archive commit settles itself.
    AgentArchive,
    /// #953: an explicit manager `replace_lead`/`retry_lead` of the current
    /// lead. The outgoing lead's own ordinary resume wakes are superseded (the
    /// lead CAS disables them atomically), so they do not hold the handover.
    /// Every genuine human/operator owner (question, approval, hard operator
    /// pause, C5, capacity incident) and the program gate still refuse, as
    /// for [`Self::ManagerAction`] with a soft-pause exception.
    ManagerLeadHandover,
    /// #1042: a manager `pause_lead`. The lead's own ordinary resume wakes do
    /// not hold the pause: the action suspends them (recorded, restored by
    /// `resume_lead`). Every genuine human/operator owner still refuses.
    ManagerLeadPause,
    /// #1408: admission of a manager's own `succeed_manager` handoff. The
    /// operator pause permits a reservation, which stays queued until cleared.
    /// Effect and publication gates still enforce the operator pause: provider
    /// establishment is productive work, not just a metadata handoff.
    /// Every other human/operator owner still refuses.
    ///
    /// The seat's own ordinary `resume` wakes (backstops, job-outcome and
    /// predicate waits) do not hold it: the publication commit hands them to the
    /// successor and retires what cannot follow, so a manager no longer cancels
    /// its own wakes before a baton pass. Wakes owned by the daemon (`keepalive-*`
    /// child valves, `background-process-*` wakes, the wakes of an open capacity
    /// incident) still refuse.
    ManagerSuccession,
}

// Alias `a` is an operation row. Lost effects fence their still-live lead
// authority (or an installed candidate), not an Epic forever after explicit
// lead repair. Keep the old operation and its evidence intact for inspection.
pub(super) const UNCERTAINTY_CURRENT_AUTHORITY: &str = "(
    json_extract(a.payload_json,'$.request.operation.epic_id') IS NULL OR
    EXISTS(SELECT 1 FROM sessions e JOIN epic_lead_generations g ON g.epic_id=e.id
      WHERE e.id=json_extract(a.payload_json,'$.request.operation.epic_id') AND
      (e.lead_session_id=a.target_session_id OR
       (e.lead_session_id IS json_extract(a.payload_json,'$.request.operation.expected.lead_session_id')
        AND g.generation=json_extract(a.payload_json,'$.request.operation.expected.lead_generation')))))";

// #554: materialize only active lifecycle operations through the state-leading
// due index. Each correlated fence then reads this set instead of project history.
const CLAIM_ELIGIBLE_SQL: &str = "WITH active AS MATERIALIZED (
             SELECT * FROM harness_manager_v2_operations
              WHERE kind=?1 AND state IN ('running','uncertain')
             ), eligible AS (
             SELECT o.id,o.project_id,o.not_before,
                    json_extract(o.payload_json,'$.origin.origin') AS origin,
                    json_extract(o.payload_json,'$.request.operation.action') AS action
               FROM harness_manager_v2_operations o WHERE o.kind=?1 AND o.state='queued' AND o.not_before<=?2
             AND (?6=0 OR (json_extract(o.payload_json,'$.request.operation.action')='operator_call' AND json_extract(o.payload_json,'$.request.operation.call.method')='RunSandboxBuildCacheReclaim'))
             AND (?3=0 OR json_extract(o.payload_json,'$.request.operation.action') NOT IN ('create_session','replace_lead','retry_lead','resume_lead'))
             AND (?5=0 OR json_extract(o.payload_json,'$.request.operation.action')<>'create_session'
                  OR json_extract(o.payload_json,'$.issue_binding.continue_from') IS NOT NULL)
             AND NOT EXISTS(SELECT 1 FROM active a WHERE a.project_id=o.project_id AND a.kind=?1
                AND a.id IS NOT json_extract(o.payload_json,'$.request.operation.operation_id') AND ((a.state='running' AND NOT (((json_extract(o.payload_json,'$.request.operation.action')='operator_call' AND json_extract(o.payload_json,'$.request.operation.call.method')='RunSandboxBuildCacheReclaim') OR (json_extract(o.payload_json,'$.request.operation.action')='create_session' AND json_extract(a.payload_json,'$.request.operation.parent_id') IS NOT json_extract(o.payload_json,'$.request.operation.parent_id'))) AND json_extract(a.payload_json,'$.request.operation.action')='create_session')) OR (a.state='uncertain' AND (a.target_session_id=o.target_session_id OR (json_extract(a.payload_json,'$.request.operation.epic_id')=json_extract(o.payload_json,'$.request.operation.epic_id') AND (
    json_extract(a.payload_json,'$.request.operation.epic_id') IS NULL OR
    EXISTS(SELECT 1 FROM sessions e JOIN epic_lead_generations g ON g.epic_id=e.id
      WHERE e.id=json_extract(a.payload_json,'$.request.operation.epic_id') AND
      (e.lead_session_id=a.target_session_id OR
       (e.lead_session_id IS json_extract(a.payload_json,'$.request.operation.expected.lead_session_id')
        AND g.generation=json_extract(a.payload_json,'$.request.operation.expected.lead_generation'))))))))))
             AND (json_extract(o.payload_json,'$.request.operation.action')<>'create_session' OR (
                  (SELECT COUNT(*) FROM active a WHERE a.project_id=o.project_id AND a.kind=?1 AND a.state='running')<?4
              AND (NOT EXISTS(SELECT 1 FROM active a WHERE a.project_id=o.project_id AND a.kind=?1 AND a.state='running')
                   OR NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations q WHERE q.project_id=o.project_id AND q.kind=?1 AND q.state='queued'
                        AND json_extract(q.payload_json,'$.request.operation.action')<>'create_session' AND (q.not_before,q.id)<(o.not_before,o.id)))))
             AND NOT EXISTS(SELECT 1 FROM manager_root_successions r JOIN sessions p ON p.id=r.predecessor_session_id
                WHERE r.operation_id=o.id AND r.state='reserved'
                  AND (p.status IN ('Starting','Running','WaitingApproval')
                       OR EXISTS(SELECT 1 FROM daemon_settings d WHERE d.key='manager_operator_pause:'||p.id AND d.value<>'false'))
                  AND r.authority_epoch=(SELECT epoch FROM manager_authority_epochs WHERE project_id=r.project_id)
                  AND EXISTS(SELECT 1 FROM harness_manager_scopes h WHERE h.project_id=r.project_id AND h.row_version=r.scope_version))
             ), oldest AS (SELECT * FROM eligible ORDER BY not_before,id LIMIT 1)
             SELECT e.id,e.not_before FROM eligible e CROSS JOIN oldest first
              WHERE e.id=first.id OR (
                    first.origin='operating_intent'
                AND first.action IN ('resume_lead','retry_lead','replace_lead')
                AND e.project_id=first.project_id
                AND e.origin='agent' AND e.action='create_session')
              ORDER BY CASE WHEN e.id=first.id THEN 1 ELSE 0 END,e.not_before,e.id LIMIT 1";

/// Coordinator-owned holds are persisted through the shared record helpers.
/// Keys are disjoint so releasing resources cannot clear a human decision.
pub const MANAGER_ACTION_HOLD_KIND: &str = "lifecycle_hold";
#[derive(Debug, Clone, Copy)]
pub enum ManagerActionHoldReasonV2 {
    Resources,
    Decision,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerActionRuntimeHoldV2 {
    pub blocked: bool,
}
pub fn manager_action_hold_key(reason: ManagerActionHoldReasonV2, epic: Option<Uuid>) -> String {
    let reason = match reason {
        ManagerActionHoldReasonV2::Resources => "resources",
        ManagerActionHoldReasonV2::Decision => "decision",
    };
    format!(
        "{reason}:{}",
        epic.map(|id| id.to_string())
            .unwrap_or_else(|| "project".into())
    )
}

/// Internal attribution; no RPC accepts this enum. Intent dispatch derives the
/// appointment from the project, never borrows a feature lead's identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerActionOriginV2 {
    Agent { caller: Uuid },
    OperatingIntent { project_id: Uuid, intent_id: Uuid },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerActionSourceV2 {
    pub session_id: Uuid,
    pub working_dir: PathBuf,
    pub sandbox_root: Option<PathBuf>,
    /// #617: a container's original directory is still a lifecycle fence,
    /// even when its registered project supplies the actual fork repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_working_dir: Option<PathBuf>,
    pub commit: String,
    pub custody_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub historical_commit: bool,
    /// #1144: the symbolic ref (`refs/heads/..`) the source HEAD was attached
    /// to when frozen; `None` when detached or for rows frozen before this
    /// field existed. A moved source is accepted only on this same branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// #1195: the explicit or default sandbox source of a `create_session`.
    /// `None` is the pre-#1195 pinned fork of the source's own `HEAD` (lead
    /// replacement, retry and older rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_source: Option<ManagerFrozenSandboxSourceV1>,
}

/// #1195: how a frozen create source's `commit` was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerFrozenSandboxSourceV1 {
    /// Resolved at launch to the fetched `origin/rolling` tip; `commit` is
    /// the checkout `HEAD` used only when no published tip can be observed.
    Rolling,
    /// `commit` is the caller-named commit.
    Commit,
    /// `commit` is the named worktree's `HEAD`, pinned at admission.
    Path {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "is_false")]
        source_dirty: bool,
    },
}

impl ManagerActionSourceV2 {
    /// The launch forks from exactly the frozen `commit` (an explicit or
    /// historical source), not from the source's current `HEAD`.
    #[must_use]
    pub const fn forks_at_frozen_commit(&self) -> bool {
        self.historical_commit || self.sandbox_source.is_some()
    }

    /// The launch resolves the published `rolling` tip (#1195).
    #[must_use]
    pub const fn resolves_rolling_at_launch(&self) -> bool {
        matches!(
            self.sandbox_source,
            Some(ManagerFrozenSandboxSourceV1::Rolling)
        )
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The Issue an `AgentManagerLaunchIssueWorker` action is bound to (#1100).
/// It rides in the action's journalled payload and context, never in the
/// public action wire type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerIssueBindingV1 {
    /// Manager-supplied bounded QA delegation; old bindings default to false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub qa_lane: bool,
    pub issue_id: Uuid,
    pub display_number: i64,
    /// #1254: the terminal worker this launch continues; the launch sets the
    /// new worker's `continued_from` and inherits its display identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_from: Option<Uuid>,
    /// #1590: the implementer Issue (display number) whose text and handoff
    /// the daemon copied into this reviewer's brief. Journalled so a replay
    /// reuses the original brief.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_of: Option<i64>,
    /// Review default inferred from the latest handoff, frozen for exact replay.
    /// The caller's operation keeps its original sandbox_source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_source_commit: Option<String>,
    /// #218: the same-project Issues the manager attached as read-only context
    /// (`related`). Their texts are copied into the brief as a snapshot; the
    /// worker may also read each live. Old bindings default to none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<i64>,
    /// #218: the reference selectors (`#N`, `#A-#B`) the manager's own brief
    /// named, snapshotted at launch (and unioned with a predecessor's on
    /// `continue_from`). The worker may read each selected Issue read-only. Only
    /// the manager's brief is scanned, never the Issue texts the daemon copies
    /// into the query, so references do not chain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub brief_refs: Vec<(i64, i64)>,
    /// #275: the `related` Issues of this Issue's earlier launches that a plain
    /// relaunch carried over (read-only, also folded into `brief_refs`). The
    /// manager's own `related` stays in `related`, so a replay compares equal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carried_related: Vec<i64>,
    /// #275: earlier related Issues that were not carried (the Issue no longer
    /// exists, is archived, or the bound is full); named in the brief and the
    /// launch result. Never refuses the launch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_related: Vec<i64>,
    /// #1930: set on a daemon-issued relaunch after the predecessor worker
    /// failed on provider capacity before its first turn. Journalled so the
    /// substitution (if any) is visible on the action record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_retry: Option<issue_worker_capacity::CapacityRetryV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerActionContextV2 {
    pub origin: ManagerActionOriginV2,
    pub request: AgentManagerControlRequestV2,
    pub target_session_id: Option<Uuid>,
    pub source: Option<ManagerActionSourceV2>,
    pub launch: Option<ManagerLaunchChoiceV2>,
    /// Admission witness: an older explicit resume cannot clear a newer pause.
    #[serde(default)]
    pub manager_pause_version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_binding: Option<ManagerIssueBindingV1>,
}

#[derive(Debug, Clone)]
pub struct ManagerActionOperationV2 {
    pub project_id: Uuid,
    pub manager_session_id: Uuid,
    pub scope_version: i64,
    pub context: ManagerActionContextV2,
    pub receipt: ManagerActionReceiptV2,
    pub effect_started: bool,
    pub settled_fence: Option<ManagerLeadFenceV2>,
    pub claim_boot_id: Option<Uuid>,
}

impl ManagerActionOperationV2 {
    /// Continuations resume work already admitted, even though their journal
    /// uses the same CreateSession action as a new Issue worker.
    pub fn is_new_worker_create(&self) -> bool {
        matches!(
            self.context.request.operation,
            ManagerActionV2::CreateSession { .. }
        ) && self
            .context
            .issue_binding
            .as_ref()
            .and_then(|binding| binding.continue_from)
            .is_none()
    }
}

#[derive(Debug, Clone)]
pub struct ManagerActionClaimV2 {
    pub operation: ManagerActionOperationV2,
    pub boot_id: Uuid,
}

#[derive(Debug, Clone)]
pub(super) struct ManagerActionAdmissionV2 {
    pub target: Option<Session>,
    pub target_session_id: Option<Uuid>,
    pub source: Option<ManagerActionSourceV2>,
    pub launch: Option<ManagerLaunchChoiceV2>,
    pub delay_seconds: u32,
}

#[derive(Debug, Clone)]
pub struct ManagerCascadeArchiveRecord {
    pub ids: Vec<Uuid>,
    pub completed_at: chrono::DateTime<chrono::FixedOffset>,
}

impl ManagerActionClaimV2 {
    pub fn id(&self) -> Uuid {
        self.operation.receipt.operation_id
    }
    pub fn action(&self) -> &ManagerActionV2 {
        &self.operation.context.request.operation
    }
}

pub fn action_epic(action: &ManagerActionV2) -> Option<Uuid> {
    match action {
        ManagerActionV2::ResumeLead { epic_id, .. }
        | ManagerActionV2::PauseLead { epic_id, .. }
        | ManagerActionV2::RetryLead { epic_id, .. }
        | ManagerActionV2::ReplaceLead { epic_id, .. }
        | ManagerActionV2::AssignLead { epic_id, .. }
        | ManagerActionV2::RetireLeadContinuations { epic_id, .. } => Some(*epic_id),
        _ => None,
    }
}

pub fn action_fence(action: &ManagerActionV2) -> Option<&ManagerLeadFenceV2> {
    match action {
        ManagerActionV2::ResumeLead { expected, .. }
        | ManagerActionV2::PauseLead { expected, .. }
        | ManagerActionV2::RetryLead { expected, .. }
        | ManagerActionV2::ReplaceLead { expected, .. }
        | ManagerActionV2::AssignLead { expected, .. }
        | ManagerActionV2::RetireLeadContinuations { expected, .. } => Some(expected),
        _ => None,
    }
}

/// `resume_lead` admission for a settled lead applies the same target check as
/// the manager continuation gate (`lifecycle::check_manager_resume_target`).
/// An unsettled lead's status can still change, so the gate decides it at
/// execution, as before.
pub fn manager_resume_available(lead: &Session) -> Result<()> {
    let settled = matches!(
        lead.status,
        SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
    );
    if settled {
        crate::store_support::manager_gates::check_manager_resume_target(lead)?;
    }
    Ok(())
}

/// `retry_lead` admits a `Failed` or `Interrupted` lead, and a `Completed` lead
/// that fails `lifecycle::manager_lead_provider_resumable` (the predicate that
/// resume admission and the continuation gate use). Applied at admission and
/// again at execution.
pub fn manager_retry_admissible(lead: &Session) -> Result<()> {
    match lead.status {
        SessionStatus::Failed | SessionStatus::Interrupted => Ok(()),
        SessionStatus::Completed
            if !crate::store_support::manager_gates::manager_lead_provider_resumable(lead) =>
        {
            Ok(())
        }
        SessionStatus::Completed => Err(
            crate::store_support::manager_gates::manager_refusal_with_next_action(
                "manager_v2_retry_lead_resumable",
                "resume_lead",
            ),
        ),
        _ => Err(refused("manager_v2_retry_requires_terminal")),
    }
}

impl Store {
    /// The latest failed resume invocation carries the transcript refusal.
    /// A later successful attempt clears that evidence even if the provider
    /// thread ID has not changed.
    fn manager_lead_transcript_refusal_class(&self, lead: &Session) -> Result<Option<String>> {
        if lead.provider != rsi_common::types::SessionProvider::Codex {
            return Ok(None);
        }
        let latest: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT status,error_class FROM model_invocations
             WHERE session_id=?1 AND purpose='session.continue.resume'
             ORDER BY created_at DESC,rowid DESC LIMIT 1",
                [lead.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match latest {
            Some((status, Some(class)))
                if status == "failed"
                    && matches!(
                        class.as_str(),
                        "codex_resume_rollout_torn_tail" | "codex_resume_tool_history_invalid"
                    ) =>
            {
                Some(class)
            }
            _ => None,
        })
    }

    pub fn manager_lead_transcript_unresumable(&self, lead: &Session) -> Result<bool> {
        Ok(self.manager_lead_transcript_refusal_class(lead)?.is_some())
    }

    pub(crate) fn manager_lead_torn_tail(&self, lead: &Session) -> Result<bool> {
        Ok(self.manager_lead_transcript_refusal_class(lead)?.as_deref()
            == Some("codex_resume_rollout_torn_tail"))
    }

    pub(crate) fn manager_lead_invalid_tool_history(&self, lead: &Session) -> Result<bool> {
        Ok(self.manager_lead_transcript_refusal_class(lead)?.as_deref()
            == Some("codex_resume_tool_history_invalid"))
    }

    pub fn manager_resume_available_for_lead(&self, lead: &Session) -> Result<()> {
        manager_resume_available(lead)?;
        if matches!(
            lead.status,
            SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
        ) && self.manager_lead_transcript_unresumable(lead)?
        {
            return Err(crate::store_support::manager_gates::manager_resume_unavailable());
        }
        Ok(())
    }

    pub fn manager_retry_admissible_for_lead(&self, lead: &Session) -> Result<()> {
        if lead.status == SessionStatus::Completed
            && self.manager_lead_transcript_unresumable(lead)?
        {
            return Ok(());
        }
        manager_retry_admissible(lead)
    }
}

/// #3540: the `manager_v2_container_out_of_scope` refusal of `create_container`
/// keeps its code (receipts classify on it) and says which rule refused, so a
/// manager with no Group in scope reads the remedy instead of guessing. An Epic
/// is only legal under a Group; a top-level Group needs the operator's
/// `allow_create_groups` grant.
fn create_container_refusal(
    kind: SessionKind,
    parent: Option<SessionKind>,
    allow_create_groups: bool,
) -> String {
    const CODE: &str = "manager_v2_container_out_of_scope";
    match (kind, parent) {
        (SessionKind::Epic, None) => format!(
            "{CODE}: an Epic is created under a Group; set parent_id to a Group in your scope{}",
            if allow_create_groups {
                ", or create a top-level Group first"
            } else {
                " (none is in scope and allow_create_groups is off: create_container for a top-level Group is refused until the operator or the global manager adds a Group to your scope or grants allow_create_groups)"
            }
        ),
        (SessionKind::Group, None) if !allow_create_groups => format!(
            "{CODE}: a top-level Group needs allow_create_groups, which your policy does not grant; ask the operator or the global manager to grant it or to add a Group to your scope"
        ),
        _ => CODE.to_string(),
    }
}

fn state_name(state: ManagerActionStateV2) -> &'static str {
    match state {
        ManagerActionStateV2::Queued => "queued",
        ManagerActionStateV2::Running => "running",
        ManagerActionStateV2::Succeeded => "succeeded",
        ManagerActionStateV2::Failed => "failed",
        ManagerActionStateV2::Blocked => "blocked",
        ManagerActionStateV2::Uncertain => "uncertain",
        ManagerActionStateV2::Revoked => "revoked",
    }
}

impl Store {
    /// Archive is a terminal operator decision. Settle queued lead recovery
    /// before it can be claimed; a later manager poll must not relaunch it.
    /// The caller includes this in the same transaction as the Archived row.
    pub(crate) fn cancel_queued_recovery_for_archive_on(
        tx: &rusqlite::Connection,
        session_id: Uuid,
    ) -> Result<()> {
        let key = session_id.to_string();
        let mut stmt = tx.prepare(
            "SELECT id,outcome_json FROM harness_manager_v2_operations
             WHERE kind='lifecycle_action' AND state='queued'
               AND json_extract(payload_json,'$.request.operation.action')
                   IN ('resume_lead','retry_lead','replace_lead')
               AND (json_extract(payload_json,'$.request.operation.epic_id')=?1
                    OR json_extract(payload_json,'$.request.operation.expected.lead_session_id')=?1)
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map([&key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let stamp = now();
        for (id, raw) in rows {
            let mut receipt: ManagerActionReceiptV2 = serde_json::from_str(&raw)?;
            receipt.state = ManagerActionStateV2::Blocked;
            receipt.row_version += 1;
            receipt.outcome = Some("cancelled_by_archive".into());
            tx.execute(
                "UPDATE harness_manager_v2_operations
                 SET state='blocked',row_version=?2,outcome_json=?3,updated_at=?4
                 WHERE id=?1 AND state='queued'",
                params![
                    id,
                    receipt.row_version,
                    serde_json::to_string(&receipt)?,
                    stamp
                ],
            )?;
        }
        tx.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?2
             WHERE wake_session_id=?1 AND enabled=1 AND wake_mode='resume'",
            params![key, stamp],
        )?;
        Ok(())
    }

    pub(super) fn manager_action_authority(
        &self,
        origin: &ManagerActionOriginV2,
        request: &AgentManagerControlRequestV2,
    ) -> Result<ManagerAuthorityV2> {
        if matches!(request.operation, ManagerActionV2::SucceedManager { .. })
            && !matches!(origin, ManagerActionOriginV2::Agent { .. })
        {
            return Err(refused("manager_succession_agent_origin_required"));
        }
        match origin {
            // #1235: the journalled request carries its target project, so
            // every effect-time recheck re-resolves the same arm.
            ManagerActionOriginV2::Agent { caller } => self.manager_v2_authorize_in(
                *caller,
                request.project_id,
                &request.fence,
                Some(request.operation.capability()),
            ),
            ManagerActionOriginV2::OperatingIntent {
                project_id,
                intent_id,
            } => {
                if intent_id.is_nil() {
                    return Err(refused("manager_v2_invalid_intent"));
                }
                let config = self
                    .get_harness_manager(*project_id)?
                    .ok_or_else(|| refused("manager_v2_grant_required"))?;
                // This resolves the live appointment using the shared scope validator.
                // Attribution remains daemon-owned (actor_session_id=NULL).
                let authority = self.manager_v2_authorize(
                    config
                        .current_session_id
                        .ok_or_else(|| refused("manager_v2_scope_changed"))?,
                    &request.fence,
                    Some(request.operation.capability()),
                )?;
                if authority.grant.policy.mode != ManagerOperatingModeV2::Execute {
                    return Err(refused("manager_v2_execute_required"));
                }
                Ok(authority)
            }
        }
    }

    /// The authority a JOURNALLED action re-proves at every execution gate.
    ///
    /// #1335: an agent's action can wait in the queue (a deploy drain holds a
    /// create for up to an hour) while its seat rotates: the caller is then
    /// an archived predecessor and its own authorization fails with
    /// `manager_current_session_required`. A rotation keeps the appointment
    /// (same anchor, scope and policy versions, which every caller of this
    /// re-checks against the journalled row), so the action is re-proved as
    /// the caller's lineage tip: the seat that holds the caller's authority
    /// now. A caller with no live successor is refused with
    /// `manager_v2_actor_seat_retired`, a clear class the receipt and the
    /// manager notice carry.
    ///
    /// #1716: a live caller that another session was appointed over (the
    /// operator replaced the project manager while the create was held) is
    /// denied `manager_scope_denied`, not `manager_current_session_required`.
    /// It has no successor either, so the stored action is refused with
    /// `manager_v2_actor_seat_replaced` (revoked, nothing launched; the
    /// current manager relaunches) instead of the generic blocked
    /// `manager_v2_lifecycle_unconfirmed` no verb could settle.
    /// Admission ([`Self::manager_action_authority`]) never takes this path:
    /// only a stored request is inherited.
    pub(super) fn manager_stored_action_authority(
        &self,
        origin: &ManagerActionOriginV2,
        request: &AgentManagerControlRequestV2,
    ) -> Result<ManagerAuthorityV2> {
        let denial = match self.manager_action_authority(origin, request) {
            Ok(authority) => return Ok(authority),
            Err(denial) => denial,
        };
        let ManagerActionOriginV2::Agent { caller } = origin else {
            return Err(denial);
        };
        let caller_retired = self.get_session(*caller)?.is_none_or(|session| {
            matches!(
                session.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        });
        let tip = self.manager_lineage_tip(*caller).ok();
        match tip.filter(|tip| tip != caller) {
            Some(tip) => self
                .manager_action_authority(&ManagerActionOriginV2::Agent { caller: tip }, request),
            None if caller_retired
                && denial
                    .to_string()
                    .contains("manager_current_session_required") =>
            {
                Err(refused("manager_v2_actor_seat_retired"))
            }
            None if denial.to_string().contains("manager_scope_denied") => {
                Err(refused("manager_v2_actor_seat_replaced"))
            }
            None => Err(denial),
        }
    }

    /// Read-only exact fence for progress/decision/coordinator observations.
    pub fn manager_action_lead_fence(&self, epic_id: Uuid) -> Result<ManagerLeadFenceV2> {
        let epic = self
            .get_session(epic_id)?
            .ok_or_else(|| refused("manager_v2_epic_unavailable"))?;
        if epic.session_kind != SessionKind::Epic {
            return Err(refused("manager_v2_epic_unavailable"));
        }
        let generation: i64 = self.conn.query_row(
            "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
            [epic_id.to_string()],
            |r| r.get(0),
        )?;
        let (event_sequence, custody_generation) = if let Some(lead) = epic.lead_session_id {
            let sequence = self.conn.query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
                [lead.to_string()],
                |r| r.get(0),
            )?;
            let custody = self.manager_action_custody_generation(lead)?;
            (sequence, custody)
        } else {
            (0, None)
        };
        Ok(ManagerLeadFenceV2 {
            lead_session_id: epic.lead_session_id,
            lead_generation: generation,
            event_sequence,
            custody_generation,
        })
    }

    pub(super) fn manager_action_custody_generation(&self, session: Uuid) -> Result<Option<i64>> {
        // The persisted projection is a witness, never authority to execute.
        Ok(self.conn.query_row("SELECT r.generation FROM sessions s LEFT JOIN sandbox_custody_roots r ON r.custody_id=s.sandbox_custody_id WHERE s.id=?1", [session.to_string()], |r| r.get(0))?)
    }

    fn manager_action_check_fence(&self, epic: Uuid, expected: &ManagerLeadFenceV2) -> Result<()> {
        let current = self.manager_action_lead_fence(epic)?;
        let mismatched = lead_fence_mismatch(expected, &current);
        if !mismatched.is_empty() {
            return Err(refused(&format!(
                "manager_v2_lead_changed: {mismatched}; read the current fence from AgentManagerInspect (lead_control row, `expected`)"
            )));
        }
        Ok(())
    }

    pub(super) fn manager_action_target(
        &self,
        authority: &ManagerAuthorityV2,
        action: &ManagerActionV2,
        check_version: bool,
    ) -> Result<Option<Session>> {
        use ManagerActionV2::*;
        if let WithdrawQueuedCreate {
            operation_id,
            expected_row_version,
        } = action
        {
            self.manager_withdraw_create_target(
                authority,
                *operation_id,
                *expected_row_version,
                check_version,
            )?;
            return Ok(None);
        }
        if let SettleUncertainAction {
            operation_id,
            expected_row_version,
        } = action
        {
            self.manager_settle_target(
                authority,
                *operation_id,
                *expected_row_version,
                check_version,
            )?;
            return Ok(None);
        }
        if let Some(epic) = action_epic(action) {
            let container = self.manager_v2_require_epic(authority, epic)?;
            if check_version {
                self.manager_action_check_fence(
                    epic,
                    action_fence(action).expect("lead action fence"),
                )?;
                self.reject_nonterminal_agent_successor_epic_lead_mutation(epic)?;
            }
            if let AssignLead {
                session_id: Some(id),
                ..
            } = action
            {
                let candidate = self
                    .get_session(*id)?
                    .ok_or_else(|| refused("manager_v2_candidate_unavailable"))?;
                if candidate.parent_id != Some(epic)
                    || candidate.project_id != Some(authority.config.project_id)
                    || !rsi_common::is_leaf_kind(candidate.session_kind)
                    || !rsi_common::legal_children(Some(SessionKind::Epic))
                        .contains(&candidate.session_kind)
                    || matches!(
                        candidate.status,
                        SessionStatus::Archived | SessionStatus::Deleted
                    )
                {
                    return Err(refused("manager_v2_candidate_out_of_scope"));
                }
                // #1276 rule (c): assigning a session an ancestor owns as the
                // lead is a mutation of it.
                self.manager_action_refuse_ancestor_owned(authority, candidate.id)?;
            }
            // #1276 rule (c): every lead action mutates the current lead, so it
            // flows down only, at admission and again at effect.
            if let Some(lead) = container.lead_session_id {
                self.manager_action_refuse_ancestor_owned(authority, lead)?;
            }
            return container
                .lead_session_id
                .map(|id| {
                    let lead = self
                        .get_session(id)?
                        .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                    if lead.parent_id != Some(epic)
                        || lead.project_id != Some(authority.config.project_id)
                        || !rsi_common::is_leaf_kind(lead.session_kind)
                        || !rsi_common::legal_children(Some(SessionKind::Epic))
                            .contains(&lead.session_kind)
                        || (!matches!(action, ReplaceLead { .. })
                            && matches!(
                                lead.status,
                                SessionStatus::Archived | SessionStatus::Deleted
                            ))
                    {
                        return Err(refused("manager_v2_lead_out_of_scope"));
                    }
                    Ok(lead)
                })
                .transpose();
        }
        match action {
            SucceedManager { expected, .. } => {
                let observed = self.manager_succession_observation(authority.caller)?;
                if check_version && observed.expected != *expected {
                    return Err(refused("manager_succession_authority_changed"));
                }
                self.get_session(authority.caller)
            }
            CreateContainer {
                parent_id, kind, ..
            } => {
                let parent = parent_id
                    .map(|id| self.manager_v2_require_container(authority, id, false))
                    .transpose()?;
                if !rsi_common::is_container_kind(*kind)
                    || !rsi_common::legal_children(parent.as_ref().map(|s| s.session_kind))
                        .contains(kind)
                    || (parent.is_none()
                        && (!authority.grant.policy.allow_create_groups
                            || *kind != SessionKind::Group))
                {
                    return Err(refused(&create_container_refusal(
                        *kind,
                        parent.as_ref().map(|s| s.session_kind),
                        authority.grant.policy.allow_create_groups,
                    )));
                }
                Ok(parent)
            }
            CreateSession {
                parent_id, kind, ..
            } => {
                let parent = self.manager_v2_require_container(authority, *parent_id, false)?;
                if !rsi_common::is_leaf_kind(*kind)
                    || !rsi_common::legal_children(Some(parent.session_kind)).contains(kind)
                {
                    return Err(refused("manager_v2_illegal_child"));
                }
                Ok(Some(parent))
            }
            UpdateContainer {
                container_id,
                expected_updated_at,
                ..
            }
            | ArchiveContainer {
                container_id,
                expected_updated_at,
            }
            | DeleteContainer {
                container_id,
                expected_updated_at,
            }
            | RestoreContainer {
                container_id,
                expected_updated_at,
            } => {
                // Replay may inspect a retained row after its successful retirement.
                let container =
                    self.manager_v2_require_container(authority, *container_id, true)?;
                if check_version {
                    if container.updated_at != *expected_updated_at {
                        return Err(refused("manager_v2_container_changed"));
                    }
                    let restore = matches!(action, RestoreContainer { .. });
                    if restore
                        != matches!(
                            container.status,
                            SessionStatus::Archived | SessionStatus::Deleted
                        )
                    {
                        return Err(refused("manager_v2_container_state_changed"));
                    }
                    if matches!(action, DeleteContainer { .. }) {
                        let nonempty: bool = self.conn.query_row(
                            "SELECT EXISTS(SELECT 1 FROM sessions WHERE parent_id=?1)",
                            [container_id.to_string()],
                            |r| r.get(0),
                        )?;
                        if nonempty || container.lead_session_id.is_some() {
                            return Err(refused("container_not_empty"));
                        }
                    }
                    if matches!(action, ArchiveContainer { .. }) {
                        self.manager_action_validate_archive_cascade(*container_id)?;
                    }
                    if matches!(action, RestoreContainer { .. }) {
                        self.manager_action_validate_restore_container(&container, false)?;
                    }
                }
                Ok(Some(container))
            }
            ArchiveSession {
                session_id,
                expected_updated_at,
            }
            | RestoreSession {
                session_id,
                expected_updated_at,
            }
            | UpdateSession {
                session_id,
                expected_updated_at,
                ..
            } => {
                // Replay may inspect a retained row after its archive/restore.
                let (session, epic) = self.manager_action_session_target(authority, *session_id)?;
                if check_version {
                    let policy = &authority.grant.policy;
                    if policy.mode != ManagerOperatingModeV2::Execute {
                        return Err(refused("manager_v2_execute_required"));
                    }
                    if policy.paused || policy.paused_epic_ids.contains(&epic) {
                        return Err(refused("manager_v2_policy_paused"));
                    }
                    if session.updated_at != *expected_updated_at {
                        return Err(session_changed(session.updated_at));
                    }
                    match action {
                        ArchiveSession { .. } => {
                            self.manager_action_validate_archive_session(&session)?;
                        }
                        RestoreSession { .. } => {
                            if session.status != SessionStatus::Archived {
                                return Err(refused("manager_v2_session_state_changed"));
                            }
                            if historical_session_restore_blocked_on(&self.conn, session.id)? {
                                return Err(refused("manager_v2_historical_restore_refused"));
                            }
                        }
                        UpdateSession { patch, .. } => {
                            if matches!(
                                session.status,
                                SessionStatus::Archived | SessionStatus::Deleted
                            ) {
                                return Err(refused("manager_v2_session_state_changed"));
                            }
                            self.manager_action_session_patch_tags(authority, patch)?;
                        }
                        _ => unreachable!("session housekeeping arm"),
                    }
                }
                Ok(Some(session))
            }
            OperatorCall { call, expected } => self.manager_action_operator_target(
                authority,
                call,
                expected.as_ref(),
                check_version,
            ),
            _ => unreachable!("lead actions handled above"),
        }
    }

    /// One scoped leaf reached through the shared manager reach definition
    /// (`manager_session_scope`). Containers stay under Topology's actions.
    fn manager_action_session_target(
        &self,
        authority: &ManagerAuthorityV2,
        id: Uuid,
    ) -> Result<(Session, Uuid)> {
        let scope = self
            .manager_session_scope(authority.caller, id)?
            .filter(|scope| scope.config.project_id == authority.config.project_id)
            .ok_or_else(|| refused("manager_v2_session_out_of_scope"))?;
        if !rsi_common::is_leaf_kind(scope.target.session_kind) {
            return Err(refused("manager_v2_leaf_required"));
        }
        // #1235 rule (c): housekeeping is a mutation and flows down only.
        self.manager_action_refuse_ancestor_owned(authority, scope.target.id)?;
        Ok((scope.target, scope.epic_id))
    }

    /// #1235 rule (c), mutations flow down: refuse a target owned by a
    /// principal above the acting one.
    fn manager_action_refuse_ancestor_owned(
        &self,
        authority: &ManagerAuthorityV2,
        target: Uuid,
    ) -> Result<()> {
        if self.manager_target_owned_by_ancestor(&authority.config, target)? {
            return Err(refused(
                rsi_common::global_manager::MANAGER_TARGET_OWNED_BY_ANCESTOR,
            ));
        }
        Ok(())
    }

    /// Nearest Epic at or above a session, for notice and gate attribution.
    pub(crate) fn manager_action_session_epic(&self, id: Uuid) -> Result<Option<Uuid>> {
        let mut cursor = Some(id);
        for _ in 0..32 {
            let Some(current) = cursor else { break };
            let Some(row) = self.get_session(current)? else {
                break;
            };
            if row.session_kind == SessionKind::Epic {
                return Ok(Some(row.id));
            }
            cursor = row.parent_id;
        }
        Ok(None)
    }

    /// A single terminal leaf only: trees use the container cascade, and a
    /// current lead must be replaced/unassigned first. The operator archive
    /// path clears lead pointers; the manager path refuses instead.
    pub(crate) fn manager_action_validate_archive_session(&self, session: &Session) -> Result<()> {
        match session.status {
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted => {}
            SessionStatus::Archived | SessionStatus::Deleted => {
                return Err(refused("manager_v2_session_state_changed"));
            }
            _ => return Err(refused("manager_v2_session_not_terminal")),
        }
        for id in self
            .manager_action_descendant_ids(session.id)?
            .into_iter()
            .skip(1)
        {
            let status: Option<String> = self
                .conn
                .query_row(
                    "SELECT status FROM sessions WHERE id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if !matches!(status.as_deref(), Some("Archived" | "Deleted")) {
                return Err(refused("manager_v2_session_has_descendants"));
            }
        }
        let lead: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE lead_session_id=?1
               AND status NOT IN ('Archived','Deleted'))",
            [session.id.to_string()],
            |row| row.get(0),
        )?;
        if lead {
            return Err(refused("manager_v2_session_is_lead"));
        }
        self.manager_action_archive_gate(session.id)
    }

    fn manager_action_live_worktree_blocked(session: &Session) -> bool {
        if session.sandbox_kind != Some(SandboxKind::GitWorktree)
            || session.sandbox_cleanup_state != Some(SandboxCleanupState::Live)
        {
            return false;
        }
        session.sandbox_root.as_deref().is_none_or(|root| {
            !matches!(
                crate::sandbox::git_worktree::observe_tracked_clean_idle(root),
                Ok(true)
            )
        })
    }

    /// Shape, tag normalization (exactly `update_session_tags`: normalize,
    /// sort, dedup) and label existence. Returns the normalized tag set.
    fn manager_action_session_patch_tags(
        &self,
        authority: &ManagerAuthorityV2,
        patch: &ManagerSessionPatchV2,
    ) -> Result<Option<Vec<String>>> {
        patch.validate().map_err(refused)?;
        if let Some(ManagerFieldPatchV2::Set(label)) = &patch.label {
            let label = self
                .get_label(*label)?
                .ok_or_else(|| refused("manager_v2_label_unavailable"))?;
            if label
                .project_id
                .is_some_and(|project| project != authority.config.project_id)
            {
                return Err(refused("manager_v2_label_unavailable"));
            }
        }
        patch
            .tags
            .as_ref()
            .map(|tags| {
                let mut normalized = tags
                    .iter()
                    .map(|tag| {
                        rsi_common::normalize_tag(tag)
                            .map_err(|_| refused("manager_v2_invalid_tags"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                normalized.sort();
                normalized.dedup();
                Ok(normalized)
            })
            .transpose()
    }

    /// Legacy callers create a HARD pause; only the operator or an admitted
    /// manager restart can clear an operator marker.
    pub fn record_manager_operator_pause(&self, session: Uuid, paused: bool) -> Result<()> {
        self.set_operator_pause(
            session,
            if paused {
                OperatorPause::Hard
            } else {
                OperatorPause::None
            },
        )?;
        Ok(())
    }

    pub fn set_operator_pause(&self, session: Uuid, pause: OperatorPause) -> Result<OperatorPause> {
        if self.get_session(session)?.is_none() {
            return Err(crate::error::DaemonError::SessionNotFound(session));
        }
        self.conn.execute("INSERT INTO daemon_settings(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",params![format!("manager_operator_pause:{session}"),pause.as_str(),now()])?;
        Ok(pause)
    }

    pub fn get_operator_pause(&self, session: Uuid) -> Result<OperatorPause> {
        if self.get_session(session)?.is_none() {
            return Err(crate::error::DaemonError::SessionNotFound(session));
        }
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM daemon_settings WHERE key=?1",
                [format!("manager_operator_pause:{session}")],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match value.as_deref() {
            None | Some("false") => OperatorPause::None,
            Some("soft") => OperatorPause::Soft,
            _ => OperatorPause::Hard,
        })
    }

    /// #1541: the pause level plus what the operator needs to find a stale
    /// marker: when it was last written and whether a manager succession for
    /// this seat is queued behind it (#1539 holds, never drops, it).
    pub fn get_operator_pause_detail(&self, session: Uuid) -> Result<OperatorPauseDetail> {
        let level = self.get_operator_pause(session)?;
        if level == OperatorPause::None {
            return Ok(OperatorPauseDetail {
                level,
                since: None,
                held_succession: false,
            });
        }
        let since: Option<String> = self
            .conn
            .query_row(
                "SELECT updated_at FROM daemon_settings WHERE key=?1",
                [format!("manager_operator_pause:{session}")],
                |r| r.get(0),
            )
            .optional()?;
        let held_succession = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_root_successions WHERE predecessor_session_id=?1 AND state IN ('reserved','executing'))",
            [session.to_string()],
            |r| r.get(0),
        )?;
        Ok(OperatorPauseDetail {
            level,
            since,
            held_succession,
        })
    }

    pub(crate) fn clear_soft_operator_pause(
        &self,
        session: Uuid,
        actor: Uuid,
        action: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE daemon_settings SET value='false',updated_at=?2 WHERE key=?1 AND value='soft'",
            params![format!("manager_operator_pause:{session}"), now()],
        )?;
        if changed != 0 {
            tracing::info!(%session, %actor, action, "manager cleared soft operator pause");
        }
        Ok(changed != 0)
    }

    pub fn manager_action_operator_paused(&self, target: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM daemon_settings WHERE key=?1 AND value<>'false')",
            [format!("manager_operator_pause:{target}")],
            |r| r.get(0),
        )?)
    }

    pub fn manager_action_human_gate(&self, target: Uuid) -> Result<()> {
        self.manager_action_human_gate_with_interrupted_resume(target, false)
    }

    fn manager_action_archive_gate(&self, target: Uuid) -> Result<()> {
        self.recovery_owner_gate(target, RecoveryOwnerMode::ManagerArchive)
    }

    pub(crate) fn manager_action_descendant_ids(&self, root: Uuid) -> Result<Vec<Uuid>> {
        let mut ids = vec![root];
        let mut pending = vec![root];
        let mut seen = std::collections::HashSet::from([root]);
        while let Some(parent) = pending.pop() {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM sessions WHERE parent_id=?1 ORDER BY id LIMIT 513")?;
            let children = stmt
                .query_map(params![parent.to_string()], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if children.len() > 512 || ids.len().saturating_add(children.len()) > 512 {
                return Err(refused("manager_v2_cascade_too_large"));
            }
            for raw in children {
                let child = Uuid::parse_str(&raw)
                    .map_err(|_| refused("manager_v2_invalid_cascade_tree"))?;
                if !seen.insert(child) {
                    return Err(refused("manager_v2_invalid_cascade_tree"));
                }
                ids.push(child);
                pending.push(child);
            }
        }
        Ok(ids)
    }

    pub fn manager_action_container_tree_ids(&self, root: Uuid) -> Result<Vec<Uuid>> {
        self.manager_action_descendant_ids(root)?
            .into_iter()
            .filter_map(|id| match self.get_session(id) {
                Ok(Some(session)) if session.status != SessionStatus::Deleted => Some(Ok(id)),
                Ok(Some(_)) => None,
                Ok(None) => Some(Err(refused("manager_v2_container_changed"))),
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    fn manager_action_validate_archive_cascade(&self, root: Uuid) -> Result<Vec<(Uuid, bool)>> {
        let ids = self.manager_action_descendant_ids(root)?;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let session = self
                .get_session(id)?
                .ok_or_else(|| refused("manager_v2_container_changed"))?;
            if session.status == SessionStatus::Deleted {
                continue;
            }
            let archived = session.status == SessionStatus::Archived;
            if !matches!(
                session.status,
                SessionStatus::Completed
                    | SessionStatus::Failed
                    | SessionStatus::Interrupted
                    | SessionStatus::Archived
            ) {
                return Err(refused("container_not_terminal"));
            }
            if !archived && rsi_common::is_leaf_kind(session.session_kind) {
                self.manager_action_archive_gate(id)?;
                if Self::manager_action_live_worktree_blocked(&session) {
                    return Err(refused("manager_v2_retention_live_worktree"));
                }
            }
            result.push((id, archived));
        }
        Ok(result)
    }

    /// IDs newly archived by the current successful cascade archive for this
    /// container. A later manager restore consumes the record; an external row
    /// update also invalidates it by advancing `sessions.updated_at`.
    pub fn manager_action_cascade_archive_ids(
        &self,
        root: Uuid,
    ) -> Result<Option<ManagerCascadeArchiveRecord>> {
        let latest: Option<(String, Option<String>, String, String)> = self
            .conn
            .query_row(
                "SELECT json_extract(o.payload_json,'$.request.operation.action'),
                        json_extract(o.outcome_json,'$.execution.cascade_archived_ids'),
                        s.updated_at,o.updated_at
             FROM harness_manager_v2_operations o
             JOIN sessions s ON s.id=o.target_session_id
             WHERE o.kind='lifecycle_action' AND o.state='succeeded'
               AND o.target_session_id=?1
               AND json_extract(o.payload_json,'$.request.operation.action')
                   IN ('archive_container','restore_container')
             ORDER BY o.updated_at DESC,o.rowid DESC LIMIT 1",
                [root.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((latest_action, evidence, container_updated_at, operation_completed_at)) = latest
        else {
            return Ok(None);
        };
        if latest_action != "archive_container" {
            return Ok(None);
        }
        let updated_at = chrono::DateTime::parse_from_rfc3339(&container_updated_at)
            .map_err(|_| refused("manager_v2_invalid_cascade_timestamp"))?;
        let completed_at = chrono::DateTime::parse_from_rfc3339(&operation_completed_at)
            .map_err(|_| refused("manager_v2_invalid_cascade_timestamp"))?;
        if updated_at > completed_at {
            return Ok(None);
        }
        let ids = evidence
            .map(|json| {
                let values: Vec<String> = serde_json::from_str(&json)?;
                values
                    .into_iter()
                    .map(|value| {
                        Uuid::parse_str(&value)
                            .map_err(|_| refused("manager_v2_invalid_cascade_evidence"))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        Ok(ids.map(|ids| ManagerCascadeArchiveRecord { ids, completed_at }))
    }

    /// Shared restore check. Admission checks container occupancy; inspection
    /// and execution also check recorded cascade members. Existing actions keep
    /// stale-member failures at execution, after the queued receipt is durable.
    pub(super) fn manager_action_validate_restore_container(
        &self,
        container: &Session,
        check_cascade_members: bool,
    ) -> Result<Option<ManagerCascadeArchiveRecord>> {
        let recorded = self.manager_action_cascade_archive_ids(container.id)?;
        if recorded.is_none() {
            let nonempty: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE parent_id=?1)",
                [container.id.to_string()],
                |row| row.get(0),
            )?;
            if nonempty || container.lead_session_id.is_some() {
                return Err(refused("container_not_empty"));
            }
        }
        if let Some(record) = recorded.as_ref().filter(|_| check_cascade_members) {
            let mut members = record.ids.clone();
            if !members.contains(&container.id) {
                members.push(container.id);
            }
            if members.len() > 512 {
                return Err(refused("manager_v2_cascade_too_large"));
            }
            for member in members {
                let row: Option<(String, String)> = self
                    .conn
                    .query_row(
                        "SELECT status,updated_at FROM sessions WHERE id=?1",
                        [member.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((status, updated_at)) = row else {
                    return Err(refused("manager_v2_cascade_record_stale"));
                };
                if status == "Deleted" {
                    continue;
                }
                let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at)
                    .map_err(|_| refused("manager_v2_invalid_cascade_timestamp"))?;
                if updated_at > record.completed_at {
                    return Err(refused("manager_v2_cascade_record_stale"));
                }
                if status == "Archived"
                    && historical_session_restore_blocked_on(&self.conn, member)?
                {
                    return Err(refused("manager_v2_historical_restore_refused"));
                }
            }
        }
        Ok(recorded)
    }

    /// Genuine operator/daemon ownership that no manager recovery may clear:
    /// questions, approvals, operator pause, C5 autofile and capacity owners.
    /// Continuation wakes and program declarations are checked separately.
    pub(crate) fn manager_action_operator_gate(&self, target: Uuid) -> Result<()> {
        let held: bool = self.conn.query_row(
            "SELECT pending_question_json IS NOT NULL OR pending_archive=1 OR status='WaitingApproval'
             OR EXISTS(SELECT 1 FROM approvals a WHERE session_id=?1 AND status='Pending' AND NOT EXISTS(SELECT 1 FROM appserver_approval_publications p WHERE p.approval_id=a.id AND (p.closure_state='closed' OR p.state='superseded')))
             OR EXISTS(SELECT 1 FROM appserver_approval_publications WHERE session_id=?1 AND closure_state<>'closed' AND state<>'superseded')
             OR EXISTS(SELECT 1 FROM daemon_settings WHERE key=?3 OR (key=?2 AND value<>'false'))
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents WHERE state='open' AND controller_session_id=?1)
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents i JOIN model_invocations m ON m.id=i.last_capacity_model_invocation_id WHERE i.state='open' AND m.session_id=?1)
             FROM sessions WHERE id=?1", params![target.to_string(), format!("manager_operator_pause:{target}"), super::daemon_settings::c5_autofile_pending_key(target)], |r| r.get(0))?;
        if held {
            return Err(refused("manager_v2_human_or_recovery_owner"));
        }
        Ok(())
    }

    /// The exception is derived only by the live action gate below. Default
    /// callers (automatic intent, assignment and approval checks) retain it off.
    pub fn manager_action_human_gate_with_interrupted_resume(
        &self,
        target: Uuid,
        allow_interrupted_resume: bool,
    ) -> Result<()> {
        self.recovery_owner_gate(
            target,
            RecoveryOwnerMode::ManagerAction {
                allow_interrupted_resume,
                allow_soft_operator_pause: false,
            },
        )
    }

    /// #1408: gate for a manager `succeed_manager`; see
    /// [`RecoveryOwnerMode::ManagerSuccession`].
    pub(crate) fn manager_succession_human_gate(&self, target: Uuid) -> Result<()> {
        self.recovery_owner_gate(target, RecoveryOwnerMode::ManagerSuccession)
    }

    pub fn manager_restart_human_gate(
        &self,
        target: Uuid,
        allow_interrupted_resume: bool,
    ) -> Result<()> {
        self.recovery_owner_gate(
            target,
            RecoveryOwnerMode::ManagerAction {
                allow_interrupted_resume,
                allow_soft_operator_pause: true,
            },
        )
    }

    /// #953: gate for an explicit manager `replace_lead`/`retry_lead` of the
    /// current lead; see [`RecoveryOwnerMode::ManagerLeadHandover`].
    pub fn manager_lead_handover_human_gate(&self, target: Uuid) -> Result<()> {
        self.recovery_owner_gate(target, RecoveryOwnerMode::ManagerLeadHandover)
    }

    /// #1042: gate for a manager `pause_lead`; see
    /// [`RecoveryOwnerMode::ManagerLeadPause`].
    pub(crate) fn manager_lead_pause_human_gate(&self, target: Uuid) -> Result<()> {
        self.recovery_owner_gate(target, RecoveryOwnerMode::ManagerLeadPause)
    }

    /// The one recovery-owner check shared by manager lifecycle actions and
    /// `AgentArchiveChild`: the held-row SQL plus the program gate.
    ///
    /// Manager archive tolerates operator pause, C5, and resume owners without
    /// clearing the pause marker. Agent archive settles C5 in its own commit.
    /// Other holds and the program gate still apply. Reads through `self.conn` also
    /// observe writes in an open transaction on that connection.
    pub(crate) fn recovery_owner_gate(&self, target: Uuid, mode: RecoveryOwnerMode) -> Result<()> {
        let (
            hold_on_c5_marker,
            hold_on_operator_pause,
            hold_on_resume,
            allow_interrupted_resume,
            allow_soft_operator_pause,
        ) = match mode {
            RecoveryOwnerMode::ManagerAction {
                allow_interrupted_resume,
                allow_soft_operator_pause,
            } => (
                true,
                true,
                true,
                allow_interrupted_resume,
                allow_soft_operator_pause,
            ),
            RecoveryOwnerMode::ManagerArchive => (false, false, false, false, false),
            RecoveryOwnerMode::AgentArchive => (false, true, true, false, false),
            RecoveryOwnerMode::ManagerLeadHandover => (true, true, false, false, true),
            RecoveryOwnerMode::ManagerLeadPause => (true, true, false, false, false),
            RecoveryOwnerMode::ManagerSuccession => (true, false, false, false, false),
        };
        // The seat's own ordinary resume wakes move with a succession, but a
        // resume wake owned by the daemon never does.
        let hold_on_daemon_resume = matches!(mode, RecoveryOwnerMode::ManagerSuccession);
        let held: bool = self.conn.query_row(
            "SELECT pending_question_json IS NOT NULL OR pending_archive=1 OR status='WaitingApproval'
             OR EXISTS(SELECT 1 FROM approvals a WHERE session_id=?1 AND status='Pending' AND NOT EXISTS(SELECT 1 FROM appserver_approval_publications p WHERE p.approval_id=a.id AND (p.closure_state='closed' OR p.state='superseded')))
             OR EXISTS(SELECT 1 FROM appserver_approval_publications WHERE session_id=?1 AND closure_state<>'closed' AND state<>'superseded')
             OR EXISTS(SELECT 1 FROM daemon_settings WHERE (?5 AND key=?3) OR (?6 AND key=?2 AND value<>'false' AND (NOT ?7 OR value<>'soft')))
             OR EXISTS(SELECT 1 FROM scheduled_jobs WHERE ?8 AND enabled=1 AND wake_mode='resume' AND wake_session_id=?1 AND id<>?4)
             OR EXISTS(SELECT 1 FROM scheduled_jobs j WHERE ?9 AND j.enabled=1 AND j.wake_mode='resume' AND j.wake_session_id=?1 AND j.id<>?4
                 AND (j.name LIKE 'keepalive-%' OR j.name LIKE 'background-process-%'
                      OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents i WHERE i.state='open' AND (i.wake_job_id=j.id OR i.program_guard_job_id=j.id))))
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents WHERE state='open' AND controller_session_id=?1)
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents i JOIN model_invocations m ON m.id=i.last_capacity_model_invocation_id WHERE i.state='open' AND m.session_id=?1)
             FROM sessions WHERE id=?1", params![target.to_string(), format!("manager_operator_pause:{target}"), super::daemon_settings::c5_autofile_pending_key(target), crate::store_support::schedule_wake_job::deterministic_program_guard_job_id(target).to_string(), hold_on_c5_marker, hold_on_operator_pause, allow_soft_operator_pause, hold_on_resume, hold_on_daemon_resume], |r| r.get(0))?;
        if held {
            return Err(refused("manager_v2_human_or_recovery_owner"));
        }
        crate::store_support::manager_gates::check_manager_action_program_gate(
            self,
            target,
            allow_interrupted_resume,
        )?;
        Ok(())
    }

    pub(super) fn manager_action_resources(
        &self,
        authority: &ManagerAuthorityV2,
        launch: &ManagerLaunchChoiceV2,
        exclude: Option<Uuid>,
    ) -> Result<()> {
        let policy = &authority.grant.policy;
        launch.validate().map_err(refused)?;
        self.manager_launch_policy_gate(&authority.config, &policy.allowed_launches, launch)?;
        let epic = exclude
            .map(|id| self.get_session(id))
            .transpose()?
            .flatten()
            .and_then(|s| s.parent_id)
            .filter(|id| authority.config.epic_ids.contains(id));
        // #1274: the acting principal's own resolved policy (a global seat's
        // granted project policy), then every ancestor's caps.
        self.manager_v2_resource_gate_with(
            &authority.config,
            (!authority.grant.revoked).then_some(&authority.grant.policy),
            epic,
            launch.provider,
            exclude,
            true,
            // #1309: the acting principal originates this launch.
            &[],
        )
    }

    /// Creation usage charged against `max_created_{containers,sessions}`
    /// (#674). Reservations count as well as committed creations, and policy
    /// edits do not reset it. DB-native review launches are not charged, and
    /// a blocked or revoked operation that never created its target is free.
    ///
    /// #3416: the session count is the creations of the last
    /// `created_sessions_window_hours` of the governing policy (24 when none
    /// resolves), so it refills as the oldest creations leave the window. The
    /// container count stays a lifetime-within-scope count. Use
    /// [`Self::manager_v2_created_lifetime`] for the informational total.
    pub fn manager_v2_created_usage(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
    ) -> Result<i64> {
        if container {
            return self.manager_v2_created_lifetime(config, true);
        }
        let hours = self.manager_creation_window_hours(config);
        Ok(self
            .manager_v2_created_window_usage_at(config, hours, i64::MAX, Utc::now())?
            .used)
    }

    /// The rolling window (hours) of the policy that governs `config`
    /// (#3416), or the default when none resolves.
    pub fn manager_creation_window_hours(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
    ) -> u16 {
        self.manager_policy_for_config(config)
            .ok()
            .flatten()
            .map_or(DEFAULT_CREATED_SESSIONS_WINDOW_HOURS, |grant| {
                grant.policy.created_sessions_window_hours
            })
    }

    /// Every charged creation of the seat lineage over the whole scope, with
    /// no window (#3416: shown beside the windowed count, never compared with
    /// the cap).
    pub fn manager_v2_created_lifetime(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
    ) -> Result<i64> {
        if container {
            return self.manager_v2_created_container_count(config);
        }
        Ok(self
            .manager_v2_created_session_times(config, "", Utc::now())?
            .len() as i64)
    }

    fn manager_v2_created_container_count(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
    ) -> Result<i64> {
        Ok(self.conn.query_row(&format!("SELECT count(*) FROM harness_manager_v2_operations o WHERE o.project_id=?1 AND o.manager_session_id=?2 AND o.scope_version=?3 AND o.kind=?4 AND json_extract(o.payload_json,'$.request.operation.action') IN (SELECT value FROM json_each(?5)) AND NOT EXISTS(SELECT 1 FROM manager_review_assignments r WHERE r.action_operation_id=o.id) AND {CREATION_CHARGED}"), params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,ACTION_KIND,"[\"create_container\"]"], |r|r.get(0))?)
    }

    /// The `created_at` of each charged session creation at or after `since`
    /// (`""`: all), oldest first. #3416: the one source both the windowed
    /// count and the lifetime total read.
    fn manager_v2_created_session_times(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        since: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<String>> {
        // #3495: a reservation that has not created its session yet is
        // charged as of `now`; see [`CREATION_AT_HEAD`].
        let now_stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let mut times: Vec<String> = {
            let mut statement = self.conn.prepare(&format!("SELECT {CREATION_AT_HEAD}?7{CREATION_AT_TAIL} FROM harness_manager_v2_operations o WHERE o.project_id=?1 AND o.manager_session_id=?2 AND o.scope_version=?3 AND o.kind=?4 AND json_extract(o.payload_json,'$.request.operation.action') IN (SELECT value FROM json_each(?5)) AND NOT EXISTS(SELECT 1 FROM manager_review_assignments r WHERE r.action_operation_id=o.id) AND {CREATION_CHARGED} AND {CREATION_AT_HEAD}?7{CREATION_AT_TAIL}>=?6"))?; // sql-dynamic-ok: static CREATION_CHARGED clause
            statement
                .query_map(
                    params![
                        config.project_id.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version,
                        ACTION_KIND,
                        "[\"create_session\",\"replace_lead\",\"retry_lead\",\"succeed_manager\"]",
                        since,
                        now_stamp
                    ],
                    |r| r.get(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        // Root occurrences retain their creation charge across scope/appointment
        // changes. They are not a recovery and are counted exactly once. A
        // blocked or revoked occurrence never created its candidate and is
        // free, as its operation is (#1390).
        // #3495: a reserved or executing occurrence is charged as of `now`;
        // a created one counts from its candidate session's creation.
        let mut retained = self.conn.prepare(
            concat!(
                "SELECT (CASE WHEN r.state IN ('reserved','executing') THEN ?5 ELSE COALESCE((SELECT cs.created_at FROM sessions cs WHERE cs.id=r.candidate_session_id),r.updated_at) END) AS charged_at FROM manager_root_successions r WHERE r.project_id=?1 AND NOT (r.manager_session_id=?2 AND r.scope_version=?3) AND NOT (r.state IN ('blocked','revoked') AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=r.candidate_session_id)) AND charged_at>=?4",
            ),
        )?;
        times.extend(
            retained
                .query_map(
                    params![
                        config.project_id.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version,
                        since,
                        now_stamp
                    ],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        // #633: sessions launched by topology executions this manager
        // requested in the current scope are charged to the same quota.
        times.extend(
            crate::store_support::topology_usage::topology_created_times(
                &self.conn,
                config.project_id,
                config.manager_session_id,
                config.row_version,
                since,
                now,
            )?,
        );
        times.sort();
        Ok(times)
    }

    /// #3416: the session creations of `config` inside the `window_hours`
    /// before `now`, the lifetime total, and when a slot next frees against
    /// `limit` (a count at or over it frees one when enough of the oldest
    /// creations have left the window; under it, when the oldest leaves).
    /// `now` is a parameter so a test injects the clock.
    pub fn manager_v2_created_window_usage_at(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        window_hours: u16,
        limit: i64,
        now: chrono::DateTime<Utc>,
    ) -> Result<CreationWindowUsage> {
        let all = self.manager_v2_created_session_times(config, "", now)?;
        Ok(window_usage(&all, window_hours, limit, now))
    }

    /// Test fixture: journal a lifecycle operation in an exact state for the
    /// current seat and scope without running admission or execution.
    ///
    /// Writes only the operation row, as the older tests expect. A reader that
    /// needs the durable target (the `lifecycle_context` record) must use
    /// [`Self::seed_manager_action_with_context_for_test`].
    #[cfg(any(test, feature = "test-seam"))]
    pub fn seed_manager_action_for_test(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        operation: ManagerActionV2,
        state: ManagerActionStateV2,
        target_session_id: Option<Uuid>,
    ) -> Result<Uuid> {
        self.seed_manager_action_inner(
            config,
            operation,
            state,
            target_session_id,
            config.manager_session_id,
            false,
        )
    }

    /// Test fixture: [`Self::seed_manager_action_for_test`] plus the
    /// `lifecycle_context` record that `enqueue_manager_action` journals beside
    /// the operation row, written the same way (`ManagerActionContextV2`
    /// through `manager_v2_put_record`). The operation row's `payload_json`
    /// stays `{origin, request}`; the target lives only in the context record.
    /// `caller` is the acting agent in the origin and `actor_session_id`.
    #[cfg(any(test, feature = "test-seam"))]
    pub fn seed_manager_action_with_context_for_test(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        operation: ManagerActionV2,
        state: ManagerActionStateV2,
        target_session_id: Option<Uuid>,
        caller: Uuid,
    ) -> Result<Uuid> {
        self.seed_manager_action_inner(config, operation, state, target_session_id, caller, true)
    }

    #[cfg(any(test, feature = "test-seam"))]
    fn seed_manager_action_inner(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        operation: ManagerActionV2,
        state: ManagerActionStateV2,
        target_session_id: Option<Uuid>,
        caller: Uuid,
        with_context: bool,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let receipt = ManagerActionReceiptV2 {
            operation_id: id,
            state,
            action_kind: operation.action_kind(),
            target_type: operation.target_type(),
            row_version: 1,
            target_session_id,
            outcome: None,
            result: None,
            deduplicated: false,
            operator_result: None,
            sandbox_source: None,
            cancelled_wakes: None,
            outcome_detail: None,
        };
        let request = AgentManagerControlRequestV2::new(
            ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: 1,
            },
            format!("seeded:{id}"),
            operation,
        );
        let origin = ManagerActionOriginV2::Agent { caller };
        let payload = json!({"origin":origin,"request":request});
        let stamp = now();
        self.conn.execute("INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,scope_version,policy_version,actor_session_id,idempotency_key,fingerprint,kind,payload_json,state,row_version,target_session_id,outcome_json,not_before,created_at,updated_at) VALUES(?1,?2,?3,?4,1,?13,?5,?6,?7,?8,?9,1,?10,?11,?12,?12,?12)", params![id.to_string(),config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,request.idempotency_key,fingerprint(&payload)?,ACTION_KIND,serde_json::to_string(&payload)?,state_name(state),target_session_id.map(|v|v.to_string()),serde_json::to_string(&receipt)?,stamp,caller.to_string()])?;
        if with_context {
            let context = ManagerActionContextV2 {
                origin,
                request,
                target_session_id,
                source: None,
                launch: None,
                manager_pause_version: 0,
                issue_binding: None,
            };
            self.manager_v2_put_record(
                config,
                CONTEXT_KIND,
                &id.to_string(),
                action_epic(&context.request.operation),
                0,
                &serde_json::to_value(context)?,
            )?;
        }
        Ok(id)
    }

    pub(super) fn manager_action_creation_budget(
        &self,
        authority: &ManagerAuthorityV2,
        container: bool,
        epic: bool,
    ) -> Result<()> {
        self.manager_action_creation_budget_at(authority, container, epic, Utc::now())
    }

    /// [`Self::manager_action_creation_budget`] at `now` (#3416: the rolling
    /// window is judged against an injectable clock).
    pub(super) fn manager_action_creation_budget_at(
        &self,
        authority: &ManagerAuthorityV2,
        container: bool,
        epic: bool,
        now: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let cap = if container {
            authority.grant.policy.max_created_containers
        } else {
            authority.grant.policy.max_created_sessions
        };
        // #3416: sessions count over the policy's rolling window.
        let window = (!container).then_some(authority.grant.policy.created_sessions_window_hours);
        let usage = self.own_creation_usage(&authority.config, container, window, cap, now)?;
        if usage.used >= i64::from(cap) {
            return Err(self.own_creation_refusal_with(
                &authority.config,
                container,
                &usage,
                cap,
            )?);
        }
        self.manager_ancestor_creation_budget(authority, container)?;
        if epic && !authority.config.has_dynamic_scope() {
            let pending: i64 = self.conn.query_row("SELECT count(*) FROM harness_manager_v2_operations WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND state IN ('queued','running','uncertain') AND json_extract(payload_json,'$.request.operation.action')='create_container' AND json_extract(payload_json,'$.request.operation.kind')='Epic'", params![authority.config.project_id.to_string(),authority.config.manager_session_id.to_string(),authority.config.row_version,ACTION_KIND], |r|r.get(0))?;
            if authority.config.epic_ids.len() as i64 + pending >= 32 {
                return Err(refused("manager_v2_scope_limit"));
            }
        }
        Ok(())
    }

    /// #2502: the caller's own created-sessions (or containers) cap is
    /// spent. The refusal names the principal's node (its portfolio node,
    /// its project, or the delegate seat), the cap, used/limit and the TUI
    /// path that raises it. The count runs over the current scope.
    #[cfg(test)]
    pub(super) fn own_creation_refusal(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
        used: i64,
        cap: u16,
    ) -> Result<DaemonError> {
        let window = (!container).then(|| self.manager_creation_window_hours(config));
        let mut usage = self.own_creation_usage(config, container, window, cap, Utc::now())?;
        usage.used = used;
        self.own_creation_refusal_with(config, container, &usage, cap)
    }

    /// [`Self::own_creation_refusal`] over a usage already read (#3416: the
    /// refusal names the window, the count and when a slot frees).
    fn own_creation_refusal_with(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
        usage: &OwnCreationUsage,
        cap: u16,
    ) -> Result<DaemonError> {
        self.own_creation_refusal_for(config, container, usage, cap, 1, 0)
    }

    /// [`Self::own_creation_refusal_with`] for `needed` creations of which
    /// `counted` are already charged (#3497: a topology launch asks for one
    /// per session node).
    fn own_creation_refusal_for(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
        usage: &OwnCreationUsage,
        cap: u16,
        needed: i64,
        counted: i64,
    ) -> Result<DaemonError> {
        let cap_field = creation_cap_field(container);
        let (node_kind, node_id, window_start, raise_in) =
            self.own_creation_raise(config, cap_field, usage.used, cap)?;
        Ok(DaemonError::InvalidParam(
            CreationCapRefusalV1 {
                code: rsi_common::manager_budget::MANAGER_V2_CREATION_LIMIT.to_string(),
                node_kind,
                node_id,
                project_id: config.project_id,
                cap_field: cap_field.to_string(),
                used: usage.used,
                limit: i64::from(cap),
                needed,
                counted,
                window_start,
                raise_in,
                window_hours: usage.window_hours,
                next_free_at: usage.next_free_at.clone(),
            }
            .message(),
        ))
    }

    /// The caller's own usage against one creation cap: the rolling window
    /// for sessions (`window` hours), the scope lifetime for containers
    /// (#3416).
    fn own_creation_usage(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
        window: Option<u16>,
        cap: u16,
        now: chrono::DateTime<Utc>,
    ) -> Result<OwnCreationUsage> {
        match window {
            Some(hours) if !container => {
                let usage =
                    self.manager_v2_created_window_usage_at(config, hours, i64::from(cap), now)?;
                Ok(OwnCreationUsage {
                    used: usage.used,
                    lifetime: Some(usage.lifetime),
                    window_hours: Some(hours),
                    next_free_at: usage.next_free_at,
                })
            }
            _ => Ok(OwnCreationUsage {
                used: self.manager_v2_created_usage(config, container)?,
                lifetime: None,
                window_hours: None,
                next_free_at: None,
            }),
        }
    }

    /// #206: the caller's own creation caps (sessions, then containers),
    /// each counted ([`Self::manager_v2_created_usage`]) and limited (the
    /// policy that governs `config`) exactly as
    /// [`Self::manager_action_creation_budget`] refuses on, with the raise
    /// path that refusal prints. Empty when no live policy governs `config`
    /// (absent, revoked, or an area node authority that no longer resolves),
    /// so a read of it never fails the snapshot it is part of.
    ///
    /// # Errors
    /// A persistence error.
    pub fn manager_own_creation_caps(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
    ) -> Result<Vec<CreationCapUsageV1>> {
        let grant = match self.manager_policy_for_config(config) {
            Ok(grant) => grant,
            Err(DaemonError::InvalidParam(_)) => None,
            Err(error) => return Err(error),
        };
        let Some(grant) = grant.filter(|grant| !grant.revoked) else {
            return Ok(Vec::new());
        };
        let mut caps = Vec::with_capacity(2);
        for container in [false, true] {
            let cap = if container {
                grant.policy.max_created_containers
            } else {
                grant.policy.max_created_sessions
            };
            let window = (!container).then_some(grant.policy.created_sessions_window_hours);
            let usage = self.own_creation_usage(config, container, window, cap, Utc::now())?;
            let cap_field = creation_cap_field(container);
            let (_, _, _, raise_in) =
                self.own_creation_raise(config, cap_field, usage.used, cap)?;
            caps.push(CreationCapUsageV1 {
                cap_field: cap_field.to_string(),
                used: usage.used,
                limit: i64::from(cap),
                raise_in,
                window_hours: usage.window_hours,
                next_free_at: usage.next_free_at,
                lifetime_total: usage.lifetime,
            });
        }
        Ok(caps)
    }

    /// The node a caller's own creation cap belongs to, its counting window
    /// and the exact path that raises it (#2502, #3028), shared by the
    /// refusal and the #206 early warning so the two never disagree.
    fn own_creation_raise(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        cap_field: &str,
        used: i64,
        cap: u16,
    ) -> Result<(BudgetNodeKindV1, Uuid, Option<String>, String)> {
        let (_, own) = self.portfolio_ancestors_of(config)?;
        let (node_kind, node_id, window_start) = match own {
            Some(head) => (
                BudgetNodeKindV1::Portfolio,
                head.node_id,
                Some(head.started),
            ),
            None if self
                .get_harness_manager(config.project_id)?
                .is_some_and(|root| root.manager_session_id == config.manager_session_id) =>
            {
                (BudgetNodeKindV1::Project, config.project_id, None)
            }
            None => (BudgetNodeKindV1::Delegate, config.manager_session_id, None),
        };
        let mut raise_in = CreationCapRefusalV1::raise_path(node_kind, node_id, cap_field);
        // #3028: a satellite seat whose policy a hub pushed is raised by the
        // hub's PM, not the operator; the operator path stays as the fallback.
        if node_kind == BudgetNodeKindV1::Project
            && CreationCapRefusalV1::hub_can_raise(cap_field)
            && self.project_policy_pushed_by_hub(config.project_id)?
        {
            raise_in =
                CreationCapRefusalV1::hub_report_path(cap_field, used, i64::from(cap), &raise_in);
        }
        Ok((node_kind, node_id, window_start, raise_in))
    }

    /// #3497: whether a topology launch of `needed` session creations (of
    /// which `counted` are already charged, a relaunch) fits the caller's own
    /// rolling-window allowance (`cap` creations per `window_hours`) and then
    /// every ancestor's, judged at `now`. A refusal is the shared typed
    /// [`CreationCapRefusalV1`] message the lifecycle creation path returns:
    /// the governing window, used/limit, the slots this launch needs and when
    /// enough of the oldest creations have left the window to admit it.
    ///
    /// # Errors
    /// The creation refusal, or a persistence error.
    pub fn manager_topology_creation_allowance_at(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        cap: u16,
        window_hours: u16,
        needed: i64,
        counted: i64,
        now: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let usage = self.manager_v2_created_window_usage_at(
            config,
            window_hours,
            creation_free_limit(cap, needed, counted),
            now,
        )?;
        if (usage.used - counted).saturating_add(needed) > i64::from(cap) {
            let own = OwnCreationUsage {
                used: usage.used,
                lifetime: Some(usage.lifetime),
                window_hours: Some(window_hours),
                next_free_at: usage.next_free_at,
            };
            return Err(self.own_creation_refusal_for(config, false, &own, cap, needed, counted)?);
        }
        self.manager_ancestor_creation_allowance_at(config, false, needed, counted, now)
    }

    /// #1235 rule (d), budgets are charged up the chain: an admission in a
    /// project counts against the project-policy caps of the acting node and
    /// of every ancestor covering the project (#1237: every depth). #1301,
    /// plan §2.3(d): work is charged once, to the originating node and its
    /// ancestors. A node's count is the charged creations in the project
    /// since its authority epoch began whose origin is the node itself, a
    /// node below it, or a principal under the whole chain (the project
    /// manager, an area node, an Epic lead); never an ancestor's or a
    /// sibling's. So a descendant's cap never limits an ancestor's own
    /// admission, and an ancestor's own work never starves a descendant.
    fn manager_ancestor_creation_budget(
        &self,
        authority: &ManagerAuthorityV2,
        container: bool,
    ) -> Result<()> {
        self.manager_ancestor_creation_allowance(&authority.config, container, 1, 0)
    }

    /// [`Self::manager_ancestor_creation_budget`] for `needed` new sessions
    /// (or containers), of which `counted` are already charged (a relaunch of
    /// a charged topology attempt). #1275: a session count includes the
    /// session attempts of manager-requested topology executions in the
    /// project (by the same origin rule), so no session-creating path escapes
    /// an ancestor's allowance.
    pub fn manager_ancestor_creation_allowance(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
        needed: i64,
        counted: i64,
    ) -> Result<()> {
        self.manager_ancestor_creation_allowance_at(config, container, needed, counted, Utc::now())
    }

    /// [`Self::manager_ancestor_creation_allowance`] judged at `now` (#3497:
    /// the rolling window reads an injectable clock).
    pub fn manager_ancestor_creation_allowance_at(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
        needed: i64,
        counted: i64,
        now: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let (ancestors, own) = self.portfolio_ancestors_of(config)?;
        let charged: Vec<_> = ancestors
            .iter()
            .map(|head| (head, false))
            .chain(own.iter().map(|head| (head, true)))
            .collect();
        let Some(since) = charged.iter().map(|(head, _)| head.started.as_str()).min() else {
            return Ok(());
        };
        let charges = self.portfolio_creation_charges(config.project_id, container, since, now)?;
        for (head, is_own) in charged {
            let policy = &head.record.grant.project_policy;
            let cap = if container {
                policy.max_created_containers
            } else {
                policy.max_created_sessions
            };
            // #3416: sessions count over the head's rolling window.
            let (count, window_hours, next_free_at) = if container {
                (head_created_count(&charges, head), None, None)
            } else {
                let hours = policy.created_sessions_window_hours;
                let usage = window_usage(
                    &head_created_times(&charges, head),
                    hours,
                    creation_free_limit(cap, needed, counted),
                    now,
                );
                (usage.used, Some(hours), usage.next_free_at)
            };
            if (count - counted).saturating_add(needed) > i64::from(cap) {
                // #2502: name the node, the cap, used/limit and where to
                // raise it, whether it is the caller's own node or above it.
                let code = if is_own {
                    rsi_common::manager_budget::MANAGER_V2_CREATION_LIMIT
                } else {
                    rsi_common::global_manager::MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED
                };
                let cap_field = creation_cap_field(container);
                return Err(DaemonError::InvalidParam(
                    CreationCapRefusalV1 {
                        code: code.to_string(),
                        node_kind: BudgetNodeKindV1::Portfolio,
                        node_id: head.node_id,
                        project_id: config.project_id,
                        cap_field: cap_field.to_string(),
                        used: count,
                        limit: i64::from(cap),
                        needed,
                        counted,
                        window_start: Some(head.started.clone()),
                        raise_in: CreationCapRefusalV1::raise_path(
                            BudgetNodeKindV1::Portfolio,
                            head.node_id,
                            cap_field,
                        ),
                        window_hours,
                        next_free_at,
                    }
                    .message(),
                ));
            }
        }
        Ok(())
    }

    /// #1301: every charged creation in `project` since `since`, as its
    /// `created_at` and its origin's lineage (the originating node and the
    /// nodes above it), or `None` when no portfolio node originated it.
    /// Lifecycle operations are attributed by their ledger principal, topology
    /// session attempts and delegated appointments (sessions only) by their
    /// requesting seat and scope, or their grantor node (#1314).
    pub(crate) fn portfolio_creation_charges(
        &self,
        project: Uuid,
        container: bool,
        since: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<(String, Option<Vec<Uuid>>)>> {
        let actions = if container {
            "[\"create_container\"]"
        } else {
            "[\"create_session\",\"replace_lead\",\"retry_lead\",\"succeed_manager\"]"
        };
        let mut statement = self.conn.prepare(&format!("SELECT {CREATION_AT_HEAD}?5{CREATION_AT_TAIL},o.manager_session_id,o.scope_version FROM harness_manager_v2_operations o WHERE o.project_id=?1 AND o.kind=?2 AND {CREATION_AT_HEAD}?5{CREATION_AT_TAIL}>=?3 AND json_extract(o.payload_json,'$.request.operation.action') IN (SELECT value FROM json_each(?4)) AND NOT EXISTS(SELECT 1 FROM manager_review_assignments r WHERE r.action_operation_id=o.id) AND {CREATION_CHARGED}"))?; // sql-dynamic-ok: static CREATION_CHARGED clause
        let mut rows: Vec<(String, Option<String>, Option<i64>)> = statement
            .query_map(
                params![
                    project.to_string(),
                    ACTION_KIND,
                    since,
                    actions,
                    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !container {
            rows.extend(
                crate::store_support::topology_usage::topology_created_charges_since(
                    &self.conn, project, since, now,
                )?,
            );
        }
        let mut origins: std::collections::HashMap<(String, i64), Option<Vec<Uuid>>> =
            std::collections::HashMap::new();
        let mut charges = Vec::with_capacity(rows.len());
        for (created, session, version) in rows {
            let lineage = match (session, version) {
                (Some(session), Some(version)) => {
                    let key = (session, version);
                    if let Some(known) = origins.get(&key) {
                        known.clone()
                    } else {
                        let lineage = self
                            .portfolio_origin_node(&key.0, key.1)?
                            .map(|node| self.portfolio_lineage(node))
                            .transpose()?;
                        origins.insert(key, lineage.clone());
                        lineage
                    }
                }
                _ => None,
            };
            charges.push((created, lineage));
        }
        if !container {
            // #1314: a delegated appointment's seat is a session its
            // grantor created, charged to the grantor and its ancestors.
            for (created, node) in self.appointment_creation_charges(project, since)? {
                charges.push((created, Some(self.portfolio_lineage(node)?)));
            }
        }
        Ok(charges)
    }

    /// The authenticated directory a manager action forks `source` from.
    fn manager_action_source_root(&self, source: &Session) -> Result<PathBuf> {
        use crate::sandbox::custody::{CustodyClassification, CustodyService};
        Ok(match CustodyService::classify(source)? {
            CustodyClassification::OrdinaryUnsandboxed => {
                CustodyService::authorize_ordinary(source)?;
                source.working_dir.clone()
            }
            CustodyClassification::RequiresPersistedAuthentication => {
                // Execution authenticates this projection against the daemon's
                // allocator and the filesystem before consuming the frozen OID.
                self.live_custody_for_session(source.id)?;
                source
                    .sandbox_root
                    .clone()
                    .ok_or_else(|| refused("manager_v2_source_custody_changed"))?
            }
        })
    }

    /// #1640: the fork source of a lead handover (`replace_lead`,
    /// `retry_lead`). The predecessor's own worktree and branch when its
    /// custody is live. When it holds none (cleanup failed or purged, so every
    /// fork of it is refused `sandbox_custody:*`), the successor forks from the
    /// Epic's published `rolling` exactly like a `create_session`: the
    /// predecessor's branch stays in the repository, only its dead sandbox is
    /// not needed. Every other classification refusal (an incomplete tuple, a
    /// live tuple that fails authentication) still refuses.
    fn manager_lead_handover_source(
        &self,
        authority: &ManagerAuthorityV2,
        lead: &Session,
        epic: Uuid,
    ) -> Result<ManagerActionSourceV2> {
        match crate::sandbox::custody::CustodyService::classify(lead) {
            Ok(_) => return self.manager_action_freeze_source(lead),
            // Only these two establish that the predecessor holds no custody.
            // Any other refusal (notably `tuple_incomplete`, a partial tuple
            // whose sandbox may still exist) must not abandon its work.
            Err(DaemonError::StructuredRpc { message, .. })
                if matches!(
                    message.strip_prefix("sandbox_custody:"),
                    Some("cleanup_failed" | "historical_purged")
                ) => {}
            Err(error) => return Err(error),
        }
        let container = self.manager_v2_require_epic(authority, epic)?;
        self.manager_action_selected_source(&container, None)
    }

    pub(super) fn manager_action_freeze_source(
        &self,
        source: &Session,
    ) -> Result<ManagerActionSourceV2> {
        let root = self.manager_action_source_root(source)?;
        let (clean, commit) = crate::sandbox::git_worktree::observe_clean_head_bounded(&root)?;
        if !clean {
            return Err(refused("manager_v2_source_worktree_dirty"));
        }
        let branch = crate::sandbox::git_worktree::observe_head_branch_bounded(&root)?;
        Ok(ManagerActionSourceV2 {
            session_id: source.id,
            working_dir: source.working_dir.clone(),
            sandbox_root: source.sandbox_root.clone(),
            parent_working_dir: None,
            commit,
            custody_generation: self.manager_action_custody_generation(source.id)?,
            historical_commit: false,
            branch,
            sandbox_source: None,
        })
    }

    fn manager_action_historical_source(
        &self,
        source: &Session,
        expected_commit: &str,
    ) -> Result<ManagerActionSourceV2> {
        use crate::sandbox::custody::{CustodyClassification, CustodyService};
        if expected_commit.len() != 40
            || expected_commit
                .bytes()
                .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
        {
            return Err(refused("manager_review_source_changed"));
        }
        if let Some(reclaimed) = self.manager_review_reclaimed_source(source)? {
            return Ok(ManagerActionSourceV2 {
                session_id: source.id,
                working_dir: reclaimed.repository,
                sandbox_root: None,
                parent_working_dir: None,
                commit: expected_commit.to_string(),
                custody_generation: Some(reclaimed.generation as i64),
                historical_commit: true,
                branch: None,
                sandbox_source: None,
            });
        }
        match CustodyService::classify(source)
            .map_err(|_| refused("manager_review_author_unavailable"))?
        {
            CustodyClassification::OrdinaryUnsandboxed => {
                CustodyService::authorize_ordinary(source)
                    .map_err(|_| refused("manager_review_author_unavailable"))?;
            }
            CustodyClassification::RequiresPersistedAuthentication => {
                self.live_custody_for_session(source.id)
                    .map_err(|_| refused("manager_review_author_unavailable"))?;
            }
        }
        Ok(ManagerActionSourceV2 {
            session_id: source.id,
            working_dir: source.working_dir.clone(),
            sandbox_root: source.sandbox_root.clone(),
            parent_working_dir: None,
            commit: expected_commit.to_string(),
            custody_generation: self
                .manager_action_custody_generation(source.id)
                .map_err(|_| refused("manager_review_author_unavailable"))?,
            historical_commit: true,
            branch: None,
            sandbox_source: None,
        })
    }

    pub(super) fn manager_action_admission(
        &self,
        authority: &ManagerAuthorityV2,
        origin: &ManagerActionOriginV2,
        request: &AgentManagerControlRequestV2,
        allocate_target_identity: bool,
        check_resources: bool,
        review_source: Option<&ManagerActionSourceV2>,
    ) -> Result<ManagerActionAdmissionV2> {
        use ManagerActionV2::*;

        let target = self.manager_action_target(authority, &request.operation, true)?;
        let pending: i64 = self.conn.query_row(&format!("SELECT count(*) FROM harness_manager_v2_operations a WHERE project_id=?1 AND (state IN ('queued','running') OR (state='uncertain' AND {UNCERTAINTY_CURRENT_AUTHORITY})) AND kind=?2"), params![authority.config.project_id.to_string(),ACTION_KIND], |r|r.get(0))?;
        if pending >= MAX_PENDING {
            return Err(refused("manager_v2_action_queue_full"));
        }
        let mut launch = None;
        let mut source = None;
        let mut id = target.as_ref().map(|session| session.id);
        let mut delay_seconds = if matches!(origin, ManagerActionOriginV2::OperatingIntent { .. }) {
            authority.grant.policy.retry_delay_seconds
        } else {
            0
        };
        match &request.operation {
            CreateContainer {
                name, tags, kind, ..
            } => {
                text(name, 512).map_err(refused)?;
                if tags.is_empty() || tags.len() > 32 {
                    return Err(refused("manager_v2_invalid_tags"));
                }
                for tag in tags {
                    rsi_common::normalize_tag(tag)
                        .map_err(|_| refused("manager_v2_invalid_tags"))?;
                }
                self.manager_action_creation_budget(authority, true, *kind == SessionKind::Epic)?;
                id = allocate_target_identity.then(Uuid::new_v4);
            }
            UpdateContainer {
                name, description, ..
            } => {
                text(name, 512).map_err(refused)?;
                if let Some(description) = description {
                    if description.len() > 8192 || description.contains('\0') {
                        return Err(refused("manager_v2_invalid_description"));
                    }
                }
            }
            CreateSession {
                query,
                launch: choice,
                ..
            }
            | ReplaceLead {
                query,
                launch: choice,
                ..
            } => {
                text(query, 32768).map_err(refused)?;
                // Only the DB-native review allocation supplies a review
                // source. Reviewer launches are bounded by per-work review
                // rounds and the active-session gate, not the lifetime
                // creation budget (#674, K15a).
                if review_source.is_none() || !matches!(request.operation, CreateSession { .. }) {
                    self.manager_action_creation_budget(authority, false, false)?;
                }
                source = Some(if let Some(source) = review_source {
                    source.clone()
                } else {
                    match &request.operation {
                        // #1195: a new worker branches from published rolling
                        // (or the named commit/worktree), never from whatever
                        // the shared checkout happens to have checked out.
                        CreateSession { sandbox_source, .. } => self
                            .manager_action_selected_project_source(
                                target
                                    .as_ref()
                                    .ok_or_else(|| refused("manager_v2_source_unavailable"))?,
                                sandbox_source.as_ref(),
                            )?,
                        ReplaceLead { epic_id, .. } => match target.as_ref() {
                            Some(lead) => {
                                self.manager_lead_handover_source(authority, lead, *epic_id)?
                            }
                            None => {
                                let epic = self.manager_v2_require_epic(authority, *epic_id)?;
                                self.manager_action_selected_source(&epic, None).map_err(|_| refused(
                                    "manager_v2_replace_lead_source_unavailable: restore the Epic repository and published origin/rolling, then retry replace_lead"
                                ))?
                            }
                        },
                        _ => unreachable!("create or replace"),
                    }
                });
                launch = Some(choice.clone());
                id = allocate_target_identity.then(Uuid::new_v4);
            }
            ResumeLead { message, .. } | RetryLead { message, .. } => {
                text(message, 32768).map_err(refused)?;
                let lead = target
                    .as_ref()
                    .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                launch = Some(ManagerLaunchChoiceV2 {
                    provider: lead.provider,
                    model: lead
                        .model
                        .clone()
                        .ok_or_else(|| refused("manager_v2_model_unavailable"))?,
                    effort: lead.effort.clone(),
                });
                if matches!(request.operation, ResumeLead { .. }) {
                    self.manager_resume_available_for_lead(lead)?;
                }
                if let RetryLead { launch: choice, .. } = &request.operation {
                    self.manager_retry_admissible_for_lead(lead)?;
                    if let Some(choice) = choice {
                        launch = Some(choice.clone());
                    }
                    self.manager_action_creation_budget(authority, false, false)?;
                    let epic = action_epic(&request.operation).expect("retry epic");
                    source = Some(self.manager_lead_handover_source(authority, lead, epic)?);
                    id = allocate_target_identity.then(Uuid::new_v4);
                    delay_seconds = authority.grant.policy.retry_delay_seconds;
                    let attempts:i64=self.conn.query_row("SELECT count(*) FROM harness_manager_v2_operations WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND json_extract(payload_json,'$.request.operation.action')='retry_lead' AND json_extract(payload_json,'$.request.operation.epic_id')=?5",params![authority.config.project_id.to_string(),authority.config.manager_session_id.to_string(),authority.config.row_version,ACTION_KIND,epic.to_string()],|r|r.get(0))?;
                    if attempts >= i64::from(authority.grant.policy.max_recovery_attempts) {
                        return Err(refused("manager_v2_retry_budget_exhausted"));
                    }
                }
            }
            PauseLead { reason, .. } => {
                text(reason, 8192).map_err(refused)?;
                if target.is_none() {
                    return Err(refused("manager_v2_lead_unavailable"));
                }
            }
            AssignLead { session_id, .. } => {
                id = *session_id;
            }
            OperatorCall { call, .. } => {
                id = call.typed().map_err(refused)?.session_id();
            }
            RetireLeadContinuations { .. } => {
                // Refuse before queueing: genuine operator gates are never
                // superseded by a manager recovery.
                let lead = target
                    .as_ref()
                    .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                self.manager_action_operator_gate(lead.id)?;
            }
            _ => {}
        }
        if check_resources && let Some(choice) = &launch {
            self.manager_action_resources(
                authority,
                choice,
                target
                    .as_ref()
                    .filter(|_| {
                        matches!(
                            request.operation,
                            ResumeLead { .. } | ReplaceLead { .. } | RetryLead { .. }
                        )
                    })
                    .map(|session| session.id),
            )?;
        }
        Ok(ManagerActionAdmissionV2 {
            target,
            target_session_id: id,
            source,
            launch,
            delay_seconds,
        })
    }

    pub fn enqueue_manager_action(
        &self,
        origin: ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let receipt = self.enqueue_manager_action_on(origin, request)?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(super) fn enqueue_manager_action_on(
        &self,
        origin: ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        self.enqueue_manager_action_with_source_on(origin, request, None, None)
    }

    /// Review allocation uses the existing lifecycle journal and launch path,
    /// but forks from the work author's exact stored source instead of the
    /// Epic container's mutable working directory.
    pub(crate) fn enqueue_manager_review_session_on(
        &self,
        origin: ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
        source_session: &Session,
        expected_source: &str,
    ) -> Result<ManagerActionReceiptV2> {
        if !matches!(request.operation, ManagerActionV2::CreateSession { .. }) {
            return Err(refused("manager_review_launch_action_required"));
        }
        let source = self.manager_action_historical_source(source_session, expected_source)?;
        self.enqueue_manager_action_with_source_on(origin, request, Some(&source), None)
    }

    fn enqueue_manager_action_with_source_on(
        &self,
        origin: ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
        review_source: Option<&ManagerActionSourceV2>,
        issue_binding: Option<&ManagerIssueBindingV1>,
    ) -> Result<ManagerActionReceiptV2> {
        use ManagerActionV2::*;
        if matches!(request.operation, SucceedManager { .. }) {
            // The runtime routes this variant through enqueue_manager_succession
            // with a private authenticated Git/custody proof.
            return Err(refused("manager_succession_authenticated_source_required"));
        }
        text(&request.idempotency_key, 128).map_err(refused)?;
        // K15A-1: the review allocator's key namespace is reserved. Manager
        // control and prepared commits (review_source None) cannot journal an
        // operation the allocator would later adopt and exclude from the
        // creation budget.
        if review_source.is_none()
            && request
                .idempotency_key
                .starts_with(REVIEW_ALLOCATION_KEY_PREFIX)
        {
            return Err(refused("manager_v2_reserved_idempotency_key"));
        }
        let authority = self.manager_action_authority(&origin, &request)?;
        self.manager_action_target(&authority, &request.operation, false)?;
        // The allocator journals and links its operation in one IMMEDIATE
        // transaction, so a legitimate retry always journals fresh. Any
        // existing operation under its key was not created by it: never adopt.
        if review_source.is_some() {
            let existing: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND idempotency_key=?4)",
                params![
                    authority.config.project_id.to_string(),
                    authority.config.manager_session_id.to_string(),
                    authority.config.row_version,
                    request.idempotency_key
                ],
                |r| r.get(0),
            )?;
            if existing {
                return Err(refused("manager_review_allocation_conflict"));
            }
        }
        let mut payload = json!({"origin":origin,"request":request});
        if let Some(binding) = issue_binding {
            payload["issue_binding"] = serde_json::to_value(binding)?;
        }
        if let Some(replay) =
            self.manager_v2_replay(&authority.config, &request.idempotency_key, &payload)?
        {
            let mut receipt: ManagerActionReceiptV2 = serde_json::from_value(replay)?;
            receipt.refresh_action_metadata(&request.operation);
            if let Some(id) = receipt.target_session_id {
                if let Some(target) = self.get_session(id)? {
                    if rsi_common::is_container_kind(target.session_kind) {
                        self.manager_v2_require_container(&authority, id, true)?;
                    } else if request.operation.housekeeping_session().is_some()
                        || matches!(request.operation, OperatorCall { .. })
                    {
                        // Reach was re-derived by manager_action_target above.
                    } else {
                        let parent =
                            action_epic(&request.operation).or_else(|| match request.operation {
                                CreateSession { parent_id, .. } => Some(parent_id),
                                _ => None,
                            });
                        if target.project_id != Some(authority.config.project_id)
                            || target.parent_id != parent
                        {
                            return Err(refused("manager_v2_target_out_of_scope"));
                        }
                    }
                }
            }
            return Ok(receipt);
        }
        if matches!(
            &request.operation,
            ResumeLead { .. } | RetryLead { .. } | ReplaceLead { .. }
        ) {
            let epic = action_epic(&request.operation).expect("lead action");
            self.manager_v2_inherit_lead_pause(&authority.config, epic)?;
        }
        let mut admission = self.manager_action_admission(
            &authority,
            &origin,
            &request,
            true,
            true,
            review_source,
        )?;
        // A review_of default changes only the frozen fork, not the caller's
        // operation or the ordinary worker admission/capacity policy.
        if let Some(commit) =
            issue_binding.and_then(|binding| binding.review_source_commit.as_ref())
            && matches!(
                &request.operation,
                CreateSession {
                    sandbox_source: None,
                    ..
                }
            )
        {
            let source = admission
                .source
                .as_ref()
                .ok_or_else(|| refused("manager_v2_source_unavailable"))?;
            let session = self
                .get_session(source.session_id)?
                .ok_or_else(|| refused("manager_v2_source_unavailable"))?;
            admission.source = Some(self.manager_action_selected_project_source(
                &session,
                Some(&ManagerSandboxSourceV1::Commit(commit.clone())),
            )?);
        }
        let id = admission.target_session_id;
        let operation_id = Uuid::new_v4();
        let receipt = ManagerActionReceiptV2 {
            operation_id,
            state: ManagerActionStateV2::Queued,
            action_kind: request.operation.action_kind(),
            target_type: request.operation.target_type(),
            row_version: 1,
            target_session_id: id,
            outcome: None,
            result: None,
            deduplicated: false,
            operator_result: None,
            sandbox_source: admission
                .source
                .as_ref()
                .and_then(sandbox_source::sandbox_source_receipt),
            cancelled_wakes: None,
            outcome_detail: None,
        };
        let stamp = now();
        let due = (Utc::now() + chrono::Duration::seconds(i64::from(admission.delay_seconds)))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let actor = match origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        self.conn.execute("INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,scope_version,policy_version,actor_session_id,idempotency_key,fingerprint,kind,payload_json,state,row_version,target_session_id,outcome_json,not_before,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'queued',1,?11,?12,?13,?14,?14)", params![operation_id.to_string(),authority.config.project_id.to_string(),authority.config.manager_session_id.to_string(),authority.config.row_version,authority.grant.row_version,actor.map(|v|v.to_string()),request.idempotency_key,fingerprint(&payload)?,ACTION_KIND,serde_json::to_string(&payload)?,id.map(|v|v.to_string()),serde_json::to_string(&receipt)?,due,stamp])?;
        // Publish stop intent at admission, before waiting on an earlier
        // action or a process guard. Retirement must close the gap before its
        // wake-disabling effect; automatic recovery re-reads this pause.
        let pause = match &request.operation {
            PauseLead {
                epic_id,
                expected,
                reason,
            } => Some((*epic_id, expected.lead_session_id, reason.as_str())),
            RetireLeadContinuations { epic_id, expected } => {
                Some((*epic_id, expected.lead_session_id, RETIREMENT_PAUSE_REASON))
            }
            _ => None,
        };
        if let Some((epic_id, lead_session_id, reason)) = pause {
            self.manager_v2_set_lead_pause(
                &authority.config,
                epic_id,
                operation_id,
                actor,
                lead_session_id,
                reason,
            )?;
        }
        let manager_pause_version = action_epic(&request.operation)
            .map(|epic| {
                self.manager_v2_lead_pause(&authority.config, epic)
                    .map(|p| p.0)
            })
            .transpose()?
            .unwrap_or(0);
        let context = ManagerActionContextV2 {
            origin,
            request,
            target_session_id: id,
            source: admission.source,
            launch: admission.launch,
            manager_pause_version,
            issue_binding: issue_binding.cloned(),
        };
        self.manager_v2_put_record(
            &authority.config,
            CONTEXT_KIND,
            &operation_id.to_string(),
            action_epic(&context.request.operation),
            0,
            &serde_json::to_value(context)?,
        )?;
        self.manager_v2_event(
            &authority.config,
            actor,
            "action_queued",
            &operation_id.to_string(),
            1,
            &serde_json::to_value(&receipt)?,
        )?;
        Ok(receipt)
    }

    /// #1084: launch targets of terminal manager actions whose failed-launch
    /// cleanup is still owed: the row is still Starting, or already Failed with
    /// its model invocation still open. Read-only; the settlement itself runs
    /// under the session manager's fences.
    pub fn manager_launch_cleanup_candidates(&self, limit: i64) -> Result<Vec<Uuid>> {
        // #1143: an action carrying drain-pending cleanup debt is excluded
        // here: its owed process/task exit must be proved before any row or
        // invocation settles (`manager_launch_drain_pending_targets`).
        let mut stmt = self.conn.prepare(
            "SELECT o.target_session_id
             FROM harness_manager_v2_operations o
             JOIN sessions s ON s.id = o.target_session_id
             LEFT JOIN model_invocations mi ON mi.id = s.model_invocation_id
             WHERE o.kind = ?1
               AND o.state IN ('failed','blocked','revoked','uncertain')
               AND COALESCE(json_extract(o.outcome_json,'$.outcome'),'') <> ?3
               AND json_extract(o.payload_json,'$.request.operation.action')
                   IN ('create_session','replace_lead','retry_lead')
               AND (s.status = 'Starting'
                    OR (s.status = 'Failed'
                        AND mi.status IN ('running','cancellation_requested')))
             ORDER BY o.updated_at
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![ACTION_KIND, limit, MANAGER_LAUNCH_DRAIN_PENDING],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| parse_id(&row?)).collect()
    }

    /// #1143: candidates of `uncertain` actions that still owe a verified
    /// drain (the launch deadline could not prove the provider process and
    /// its task gone). The debt lasts while the row is `Starting` or its
    /// invocation is open; settling both ends it.
    pub fn manager_launch_drain_pending_targets(&self, limit: i64) -> Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT o.target_session_id
             FROM harness_manager_v2_operations o
             JOIN sessions s ON s.id = o.target_session_id
             LEFT JOIN model_invocations mi ON mi.id = s.model_invocation_id
             WHERE o.kind = ?1
               AND o.state = 'uncertain'
               AND json_extract(o.outcome_json,'$.outcome') = ?3
               AND json_extract(o.payload_json,'$.request.operation.action')
                   IN ('create_session','replace_lead','retry_lead')
               AND (s.status = 'Starting'
                    OR mi.status IN ('running','cancellation_requested'))
             ORDER BY o.updated_at
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![ACTION_KIND, limit, MANAGER_LAUNCH_DRAIN_PENDING],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| parse_id(&row?)).collect()
    }

    /// #1143: record that the owed drain was proved and settled: the receipt
    /// keeps its `uncertain` state but names the timeout class instead of the
    /// debt, so the sweep stops revisiting it.
    pub fn clear_manager_launch_drain_pending(&self, target: Uuid) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE harness_manager_v2_operations
             SET outcome_json = json_set(outcome_json,'$.outcome',?4), updated_at = ?5
             WHERE kind = ?1 AND target_session_id = ?2 AND state = 'uncertain'
               AND json_extract(outcome_json,'$.outcome') = ?3",
            params![
                ACTION_KIND,
                target.to_string(),
                MANAGER_LAUNCH_DRAIN_PENDING,
                MANAGER_LAUNCH_DRAIN_VERIFIED,
                now()
            ],
        )?)
    }

    pub fn manager_action_operation(&self, id: Uuid) -> Result<Option<ManagerActionOperationV2>> {
        let raw:Option<(String,String,i64,String,String,Option<String>,Option<String>)>=self.conn.query_row(
            "SELECT o.project_id,o.manager_session_id,o.scope_version,o.outcome_json,c.payload_json,o.claim_boot_id,e.payload_json
             FROM harness_manager_v2_operations o JOIN harness_manager_v2_records c ON c.project_id=o.project_id AND c.manager_session_id=o.manager_session_id AND c.scope_version=o.scope_version AND c.kind=?2 AND c.record_key=o.id
             LEFT JOIN harness_manager_v2_records e ON e.project_id=o.project_id AND e.manager_session_id=o.manager_session_id AND e.scope_version=o.scope_version AND e.kind=?3 AND e.record_key=o.id
             WHERE o.id=?1 AND o.kind=?4",params![id.to_string(),CONTEXT_KIND,EXECUTION_KIND,ACTION_KIND],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        raw.map(
            |(project, manager, scope, receipt, context, boot, execution)| {
                let execution: Value = execution
                    .map(|s| serde_json::from_str(&s))
                    .transpose()?
                    .unwrap_or(json!({}));
                let context: ManagerActionContextV2 = serde_json::from_str(&context)?;
                let mut receipt: ManagerActionReceiptV2 = serde_json::from_str(&receipt)?;
                receipt.refresh_action_metadata(&context.request.operation);
                Ok(ManagerActionOperationV2 {
                    project_id: parse_id(&project)?,
                    manager_session_id: parse_id(&manager)?,
                    scope_version: scope,
                    context,
                    receipt,
                    claim_boot_id: boot.map(|s| parse_id(&s)).transpose()?,
                    effect_started: execution["effect_started"].as_bool().unwrap_or(false),
                    settled_fence: execution
                        .get("settled_fence")
                        .filter(|v| !v.is_null())
                        .map(|v| serde_json::from_value(v.clone()))
                        .transpose()?,
                })
            },
        )
        .transpose()
    }

    /// Queued `create_session` actions that are due, oldest first (the order
    /// the claim query releases them in): `(target session id, not_before)`.
    /// Read-only; `AgentGetDaemonInfo` host-load admission lists them (#1417).
    pub fn queued_manager_create_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<(Option<Uuid>, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT o.target_session_id,o.not_before FROM harness_manager_v2_operations o
             WHERE o.kind=?1 AND o.state='queued' AND o.not_before<=?3
               AND json_extract(o.payload_json,'$.request.operation.action')='create_session'
               AND json_extract(o.payload_json,'$.issue_binding.continue_from') IS NULL
             ORDER BY o.not_before,o.id LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![ACTION_KIND, i64::try_from(limit).unwrap_or(i64::MAX), now()],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
        )?;
        let mut queued = Vec::new();
        for row in rows {
            let (target, not_before) = row?;
            queued.push((target.map(|id| parse_id(&id)).transpose()?, not_before));
        }
        Ok(queued)
    }

    pub fn claim_manager_action(&self, boot_id: Uuid) -> Result<Option<ManagerActionClaimV2>> {
        self.claim_manager_action_holding(boot_id, false)
    }

    /// `hold_worker_starts` (#1073 deploy drain) leaves every action that
    /// would start a worker turn queued and unclaimed, so a restart re-admits
    /// it instead of finding a `running` claim it must mark uncertain.
    pub fn claim_manager_action_holding(
        &self,
        boot_id: Uuid,
        hold_worker_starts: bool,
    ) -> Result<Option<ManagerActionClaimV2>> {
        self.claim_manager_action_holding_creates(boot_id, hold_worker_starts, false)
    }

    /// [`Self::claim_manager_action_holding`] plus `hold_creates` (#1417 host
    /// load): leaves new queued `create_session` actions (Issue-worker launches and
    /// reviews included) unclaimed while the host is too loaded for a new
    /// worker. Lead recovery (`replace_lead`, `retry_lead`, `resume_lead`)
    /// Issue worker continuations, and every other action still claim, so only
    /// new work waits. The held
    /// creates stay queued, so they keep their `(not_before, id)` place and
    /// the claim query releases them oldest-first across projects.
    pub fn claim_manager_action_holding_creates(
        &self,
        boot_id: Uuid,
        hold_worker_starts: bool,
        hold_creates: bool,
    ) -> Result<Option<ManagerActionClaimV2>> {
        self.claim_manager_action_with_admission(
            boot_id,
            hold_worker_starts,
            hold_creates,
            None,
            false,
        )
    }

    /// Consult admission only for the oldest eligible new create. The callback
    /// must be synchronous and must not access the store. A held create stays
    /// queued; other actions, including continuations, can still claim.
    pub fn claim_manager_action_with_create_admission(
        &self,
        boot_id: Uuid,
        hold_worker_starts: bool,
        admit: &dyn Fn(Uuid, chrono::DateTime<chrono::Utc>, Option<Uuid>) -> bool,
    ) -> Result<Option<ManagerActionClaimV2>> {
        self.claim_manager_action_with_admission(
            boot_id,
            hold_worker_starts,
            false,
            Some(admit),
            false,
        )
    }

    /// #434: the coordinator reserves an extra slot for disk-freeing work when
    /// its ordinary slots (or batch budget) are occupied by slow launches.
    /// This uses the same claim and authority fences, restricted to reclaim.
    pub fn claim_manager_build_cache_reclaim(
        &self,
        boot_id: Uuid,
    ) -> Result<Option<ManagerActionClaimV2>> {
        self.claim_manager_action_with_admission(boot_id, false, false, None, true)
    }

    fn claim_manager_action_with_admission(
        &self,
        boot_id: Uuid,
        hold_worker_starts: bool,
        hold_creates: bool,
        admit: Option<&dyn Fn(Uuid, chrono::DateTime<chrono::Utc>, Option<Uuid>) -> bool>,
        reclaim_only: bool,
    ) -> Result<Option<ManagerActionClaimV2>> {
        // Avoid an immediate write transaction and the full conflict/admission
        // query on idle ticks. The due index excludes settled history and
        // future retries; a later enqueue is picked up by the next event/tick.
        // A positive probe still goes through every existing live claim fence.
        let due: bool = self
            .conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations
             WHERE state='queued' AND not_before<=?1 AND kind=?2)",
            )?
            .query_row(params![now(), ACTION_KIND], |row| row.get(0))?;
        if !due {
            return Ok(None);
        }
        // #2494: a backlog of creates the host gate holds is otherwise
        // rescanned in full every tick. When nothing was written since a scan
        // found only a refused create to claim, ask the gate about that create
        // again and skip the scans. Every other input of the scans is a table
        // row (covered by the version) or a queued `not_before` (covered by the
        // memo's expiry), so the skipped scans would repeat their answer.
        let version = if admit.is_some() && !hold_creates {
            self.manager_intent_observation_version()?
        } else {
            None
        };
        let mut admitted_create: Option<String> = None;
        if let (Some(version), Some(admit)) = (version, admit) {
            let held = self.manager_intent_idle.borrow().held_claim(
                version,
                hold_worker_starts,
                chrono::Utc::now(),
            );
            if let Some(held) = held {
                let since = chrono::DateTime::parse_from_rfc3339(&held.not_before)
                    .map_err(|_| refused("manager_v2_action_invalid_not_before"))?
                    .with_timezone(&chrono::Utc);
                if !admit(parse_id(&held.operation_id)?, since, held.target) {
                    self.manager_intent_idle.borrow_mut().held_claim_reuses += 1;
                    return Ok(None);
                }
                // Admitted now (and recorded by the gate): the full scan
                // below must not ask again for the same create.
                self.manager_intent_idle.borrow_mut().forget_held_claim();
                admitted_create = Some(held.operation_id);
            }
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // One active command per project except independent creates and
        // reclaim bypassing creates, even across independent daemon handles.
        // An uncertain command fences only conflicting targets, not all future work.
        // Preserve the oldest eligible claim except when that claim is
        // automatic lead recovery and a due agent-created child in the same
        // project is ready. Review allocations use that same CreateSession
        // journal; the original conflict and due predicates apply to both.
        // #1143: running creates overlap only across distinct parents. A
        // same-parent (or unreadable-parent: `IS NOT` fails closed) running
        // create fences the candidate; the runtime keeps a matching
        // per-parent guard (`manager_create_parent_guard_key`).
        // Only reclaim may pass a running create. Reclaims still serialize
        // with each other and all other actions, and creates never pass a
        // running reclaim. Custody checks remain in the reclaim worker.
        let selected: Option<(String, String)> = self
            .conn
            .query_row(
                CLAIM_ELIGIBLE_SQL,
                params![
                    ACTION_KIND,
                    now(),
                    i64::from(hold_worker_starts),
                    MANAGER_CREATE_PARALLELISM,
                    i64::from(hold_creates),
                    i64::from(reclaim_only)
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, not_before)) = selected else {
            return Ok(None);
        };
        let id = parse_id(&id)?;
        let mut operation = self
            .manager_action_operation(id)?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        if operation.is_new_worker_create()
            && let Some(admit) = admit
        {
            let since = chrono::DateTime::parse_from_rfc3339(&not_before)
                .map_err(|_| refused("manager_v2_action_invalid_not_before"))?
                .with_timezone(&chrono::Utc);
            let asked = admitted_create.as_deref() == Some(id.to_string().as_str());
            if !asked && !admit(id, since, operation.receipt.target_session_id) {
                drop(tx);
                let other =
                    self.claim_manager_action_holding_creates(boot_id, hold_worker_starts, true)?;
                if other.is_none()
                    && let Some(version) = version
                {
                    let valid_until = self.next_manager_action_due(chrono::Utc::now())?;
                    self.manager_intent_idle.borrow_mut().remember_held_claim(
                        version,
                        hold_worker_starts,
                        valid_until,
                        id.to_string(),
                        not_before,
                        operation.receipt.target_session_id,
                    );
                }
                return Ok(other);
            }
        }
        operation.receipt.state = ManagerActionStateV2::Running;
        operation.receipt.outcome = None;
        operation.receipt.row_version += 1;
        operation
            .receipt
            .refresh_action_metadata(&operation.context.request.operation);
        self.conn.execute("UPDATE harness_manager_v2_operations SET state='running',row_version=?2,outcome_json=?3,attempts=attempts+1,claim_boot_id=?4,updated_at=?5 WHERE id=?1 AND state='queued'",params![id.to_string(),operation.receipt.row_version,serde_json::to_string(&operation.receipt)?,boot_id.to_string(),now()])?;
        operation.claim_boot_id = Some(boot_id);
        self.claim_manager_succession_on(id, boot_id)?;
        tx.commit()?;
        Ok(Some(ManagerActionClaimV2 { operation, boot_id }))
    }

    /// When a scan of the action queue can next give a different answer
    /// without a write: the earliest queued `not_before` still in the future,
    /// or the memo backstop.
    fn next_manager_action_due(
        &self,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<chrono::DateTime<chrono::Utc>> {
        let mut next = at
            + chrono::Duration::seconds(super::manager_intent::idle::HELD_CLAIM_BACKSTOP_SECONDS);
        let later: Option<String> = self
            .conn
            .prepare_cached(
                "SELECT MIN(not_before) FROM harness_manager_v2_operations
                 WHERE state='queued' AND kind=?1 AND not_before>?2",
            )?
            .query_row(params![ACTION_KIND, now()], |row| row.get(0))?;
        if let Some(later) = later.and_then(|v| chrono::DateTime::parse_from_rfc3339(&v).ok()) {
            next = next.min(later.with_timezone(&chrono::Utc));
        }
        Ok(next)
    }

    /// #1073: hand a claimed action back to the queue when the deploy drain
    /// engaged after the claim and before any provider effect. Returns `false`
    /// (leaving the claim untouched) once an effect started: that outcome is
    /// no longer safe to replay.
    pub fn requeue_held_manager_action(&self, claim: &ManagerActionClaimV2) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let op = self.manager_action_assert_claim(claim)?;
        if op.effect_started {
            return Ok(false);
        }
        let mut receipt = op.receipt;
        receipt.state = ManagerActionStateV2::Queued;
        receipt.outcome = None;
        receipt.row_version += 1;
        receipt.refresh_action_metadata(&op.context.request.operation);
        let changed = self.conn.execute(
            "UPDATE harness_manager_v2_operations SET state='queued',row_version=?2,outcome_json=?3,attempts=MAX(attempts-1,0),claim_boot_id=NULL,updated_at=?4 WHERE id=?1 AND state='running' AND row_version=?5 AND claim_boot_id=?6",
            params![
                claim.id().to_string(),
                receipt.row_version,
                serde_json::to_string(&receipt)?,
                now(),
                claim.operation.receipt.row_version,
                claim.boot_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(refused("manager_v2_claim_changed"));
        }
        tx.commit()?;
        Ok(true)
    }

    /// #495: only a pre-effect create whose allocator cleaned up may retry.
    /// Keep the charged attempt count across restarts and other admission holds.
    pub fn requeue_sandbox_git_timeout(&self, claim: &ManagerActionClaimV2) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let op = self.manager_action_assert_claim(claim)?;
        let attempts: i64 = self.conn.query_row(
            "SELECT attempts FROM harness_manager_v2_operations WHERE id=?1",
            [claim.id().to_string()],
            |row| row.get(0),
        )?;
        if op.effect_started || !op.is_new_worker_create() || attempts >= 3 {
            return Ok(false);
        }
        let mut receipt = op.receipt;
        receipt.state = ManagerActionStateV2::Queued;
        receipt.outcome = Some("sandbox_git_timeout".into());
        receipt.row_version += 1;
        receipt.refresh_action_metadata(&op.context.request.operation);
        let retry_at = (chrono::Utc::now() + chrono::Duration::seconds(30))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        self.conn.execute(
            "UPDATE harness_manager_v2_operations SET state='queued',row_version=?2,outcome_json=?3,claim_boot_id=NULL,not_before=?4,updated_at=?5 WHERE id=?1",
            params![claim.id().to_string(), receipt.row_version, serde_json::to_string(&receipt)?, retry_at, now()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Durable cooldown, also available after a daemon restart. Once due, the
    /// ordinary host/deploy admission view explains any remaining hold.
    pub fn sandbox_git_timeout_retry_at(&self, id: Uuid) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT not_before FROM harness_manager_v2_operations
             WHERE id=?1 AND state='queued' AND not_before>?2
             AND json_extract(outcome_json,'$.outcome')='sandbox_git_timeout'",
                params![id.to_string(), now()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn recover_manager_actions_startup(&self, boot_id: Uuid) -> Result<usize> {
        self.recover_manager_action_claims(Some(boot_id))
    }

    /// Caller must own the runtime reconciliation single-flight guard. No
    /// previous execution future can then own a running claim, including an
    /// aborted future from this same boot. Never retry its provider effect.
    pub fn recover_abandoned_manager_action_claims(&self) -> Result<usize> {
        self.recover_manager_action_claims(None)
    }

    fn recover_manager_action_claims(&self, boot_id: Option<Uuid>) -> Result<usize> {
        let ids: Vec<String> = {
            let mut s=self.conn.prepare("SELECT id FROM harness_manager_v2_operations WHERE kind=?1 AND state='running' AND (?2 IS NULL OR claim_boot_id IS NOT ?2) ORDER BY id LIMIT 64")?;
            s.query_map(
                params![ACTION_KIND, boot_id.map(|id| id.to_string())],
                |r| r.get(0),
            )?
            .collect::<std::result::Result<_, _>>()?
        };
        let mut count = 0;
        for id in ids {
            let Some(operation) = self.manager_action_operation(parse_id(&id)?)? else {
                continue;
            };
            let old_boot = operation
                .claim_boot_id
                .ok_or_else(|| refused("manager_v2_claim_corrupt"))?;
            let claim = ManagerActionClaimV2 {
                operation,
                boot_id: old_boot,
            };
            self.finish_manager_action(
                &claim,
                ManagerActionStateV2::Uncertain,
                "execution_owner_lost_unconfirmed",
            )?;
            count += 1;
        }
        Ok(count)
    }

    pub(super) fn manager_action_assert_claim(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<ManagerActionOperationV2> {
        let op = self
            .manager_action_operation(claim.id())?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        if op.receipt.state != ManagerActionStateV2::Running
            || op.receipt.row_version != claim.operation.receipt.row_version
            || op.claim_boot_id != Some(claim.boot_id)
        {
            return Err(refused("manager_v2_claim_changed"));
        }
        Ok(op)
    }

    /// Check at the actual spawn/effect boundary. A preflight result is never a
    /// portable capability; every effect must recheck the persisted claim.
    pub fn manager_action_runtime_gate(
        &self,
        claim: &ManagerActionClaimV2,
        effect: bool,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        if effect {
            let op = self.manager_action_assert_claim(claim)?;
            let authority =
                self.manager_stored_action_authority(&op.context.origin, &op.context.request)?;
            let key = claim.id().to_string();
            let prior = self.manager_v2_record(&authority.config, EXECUTION_KIND, &key)?;
            let mut payload = prior
                .as_ref()
                .map(|r| r.payload.clone())
                .unwrap_or(json!({}));
            payload["effect_started"] = json!(true);
            self.manager_v2_put_record(
                &authority.config,
                EXECUTION_KIND,
                &key,
                action_epic(claim.action()),
                prior.map_or(0, |r| r.row_version),
                &payload,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn manager_action_runtime_gate_on(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<ManagerAuthorityV2> {
        let op = self.manager_action_assert_claim(claim)?;
        // #967: a DB review launch whose assignment already ended must not
        // start (or continue toward) a reviewer provider turn.
        self.require_manager_review_launch_live(&op)?;
        self.require_issue_worker_launch_live(&op)?;
        let authority =
            self.manager_stored_action_authority(&op.context.origin, &op.context.request)?;
        // An appointment rotation cannot make a stored daemon intent act under a
        // newer principal even if its numeric scope/policy versions coincide.
        if authority.config.project_id != op.project_id
            || authority.config.manager_session_id != op.manager_session_id
            || authority.config.row_version != op.scope_version
        {
            return Err(refused("manager_v2_scope_changed"));
        }
        let mut action = op.context.request.operation.clone();
        let review_evidence_action = self.manager_review_evidence_action(&op)?;
        if let Some(mut settled) = op.settled_fence {
            // A lost-lead replacement publishes its candidate before dispatch.
            // Its own first turn may append events; only the lead identity,
            // generation and custody remain assignment fences in this phase.
            if matches!(action, ManagerActionV2::ReplaceLead { .. })
                && settled.lead_session_id == op.context.target_session_id
            {
                settled.event_sequence = self
                    .manager_action_lead_fence(action_epic(&action).expect("replacement epic"))?
                    .event_sequence;
            }
            match &mut action {
                ManagerActionV2::PauseLead { expected, .. }
                | ManagerActionV2::ReplaceLead { expected, .. }
                | ManagerActionV2::RetryLead { expected, .. }
                | ManagerActionV2::AssignLead { expected, .. } => *expected = settled,
                _ => return Err(refused("manager_v2_invalid_settlement_witness")),
            }
        }
        let target = self.manager_action_target(&authority, &action, true)?;
        // Retry admission is re-proved at every execution gate: a lead that an
        // operator resumed after admission must not be settled by the retry.
        if let (ManagerActionV2::RetryLead { .. }, Some(lead)) = (&action, target.as_ref()) {
            self.manager_retry_admissible_for_lead(lead)?;
        }
        let effect_epic = match action
            .housekeeping_session()
            .or_else(|| operator_delegation::delegated_effect_session(&action))
        {
            // A leaf's nearest Epic owns its decision, pause and hold gates.
            Some(session) => self.manager_action_session_epic(session)?,
            None => action_epic(&action).or_else(|| {
                target
                    .as_ref()
                    .filter(|s| s.session_kind == SessionKind::Epic)
                    .map(|s| s.id)
            }),
        };
        // Check live decisions here, not only the coordinator's last hold
        // projection. Operator answer delivery has a separate continuation
        // intent and does not call this action gate on its own pending answer.
        if !matches!(action, ManagerActionV2::PauseLead { .. }) {
            if let Some(epic) = effect_epic {
                if !review_evidence_action {
                    self.manager_v2_decision_gate(&authority.config, epic)?;
                }
                let (version, paused) = self.manager_v2_action_pause(&authority.config, epic)?;
                // Retirement stops automatic intent recovery at admission,
                // including an intent action queued before the retirement.
                if matches!(
                    op.context.origin,
                    ManagerActionOriginV2::OperatingIntent { .. }
                ) && self.manager_v2_lead_pause(&authority.config, epic)?.1
                {
                    return Err(refused("manager_v2_manager_paused"));
                }
                let explicit_resume =
                    matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
                        && matches!(
                            action,
                            ManagerActionV2::ResumeLead { .. }
                                | ManagerActionV2::RetryLead { .. }
                                | ManagerActionV2::ReplaceLead { .. }
                        );
                if paused
                    && (!explicit_resume || version != op.context.manager_pause_version)
                    && !delegated_cleanup_allowed_during_lead_pause(&action)
                    && !matches!(
                        action,
                        ManagerActionV2::AssignLead { .. }
                            | ManagerActionV2::RetireLeadContinuations { .. }
                            | ManagerActionV2::ArchiveContainer { .. }
                    )
                {
                    return Err(refused("manager_v2_manager_paused"));
                }
                if matches!(
                    op.context.origin,
                    ManagerActionOriginV2::OperatingIntent { .. }
                ) {
                    self.manager_v2_intent_work_gate(&authority.config, epic)?;
                }
            }
        }
        for reason in [
            ManagerActionHoldReasonV2::Resources,
            ManagerActionHoldReasonV2::Decision,
        ] {
            if matches!(action, ManagerActionV2::PauseLead { .. })
                || review_evidence_action && matches!(reason, ManagerActionHoldReasonV2::Decision)
                || matches!(reason, ManagerActionHoldReasonV2::Resources)
                    && op.context.launch.is_none()
            {
                continue;
            }
            for epic in [None, effect_epic] {
                let key = manager_action_hold_key(reason, epic);
                if let Some(record) =
                    self.manager_v2_record(&authority.config, MANAGER_ACTION_HOLD_KIND, &key)?
                {
                    let hold: ManagerActionRuntimeHoldV2 =
                        serde_json::from_value(record.payload)
                            .map_err(|_| refused("manager_v2_runtime_hold_malformed"))?;
                    if hold.blocked {
                        return Err(refused(match reason {
                            ManagerActionHoldReasonV2::Resources => "manager_v2_resource_hold",
                            ManagerActionHoldReasonV2::Decision => "manager_v2_decision_hold",
                        }));
                    }
                }
            }
        }

        if !matches!(action, ManagerActionV2::PauseLead { .. }) {
            if authority.grant.policy.paused
                || effect_epic.is_some_and(|e| authority.grant.policy.paused_epic_ids.contains(&e))
            {
                return Err(refused("manager_v2_policy_paused"));
            }
            // Metadata edits neither answer nor cancel a gate; archive and
            // restore keep the full human/recovery-owner gate.
            if let Some(target) = target.as_ref().filter(|s| {
                rsi_common::is_leaf_kind(s.session_kind)
                    && !matches!(action, ManagerActionV2::UpdateSession { .. })
            }) {
                let allow_interrupted_resume =
                    matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
                        && matches!(action, ManagerActionV2::ResumeLead { .. })
                        && target.status == SessionStatus::Interrupted;
                if matches!(action, ManagerActionV2::RetireLeadContinuations { .. }) {
                    // Retirement supersedes continuation owners and program
                    // declarations only; operator ownership still refuses.
                    self.manager_action_operator_gate(target.id)?;
                } else if matches!(action, ManagerActionV2::ArchiveSession { .. })
                    || operator_delegation::delegated_archive_effect(&action)
                {
                    self.manager_action_archive_gate(target.id)?;
                } else {
                    if matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
                        && matches!(
                            action,
                            ManagerActionV2::RetryLead { .. } | ManagerActionV2::ReplaceLead { .. }
                        )
                    {
                        self.manager_lead_handover_human_gate(target.id)?;
                    } else if matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
                        && matches!(
                            action,
                            ManagerActionV2::ResumeLead { .. }
                                | ManagerActionV2::AssignLead {
                                    session_id: Some(_),
                                    ..
                                }
                        )
                    {
                        self.manager_restart_human_gate(target.id, allow_interrupted_resume)?;
                    } else {
                        self.manager_action_human_gate_with_interrupted_resume(
                            target.id,
                            allow_interrupted_resume,
                        )?;
                    }
                }
            }
        } else if let Some(target) = target.as_ref() {
            // Pausing may stop a running turn, but never answers or cancels a
            // pending approval/question or takes an independent recovery owner.
            self.manager_lead_pause_human_gate(target.id)?;
        }
        if let Some(choice) = &op.context.launch {
            let exclude = if matches!(
                action,
                ManagerActionV2::ReplaceLead { .. } | ManagerActionV2::RetryLead { .. }
            ) && op
                .context
                .target_session_id
                .map(|id| self.get_session(id))
                .transpose()?
                .flatten()
                .is_none()
            {
                action_fence(&action).and_then(|f| f.lead_session_id)
            } else {
                op.context.target_session_id
            };
            self.manager_action_resources(&authority, choice, exclude)?;
            if matches!(action, ManagerActionV2::ResumeLead { .. }) {
                let target = target.ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                if target.provider != choice.provider
                    || target.model.as_deref() != Some(choice.model.as_str())
                    || target.effort != choice.effort
                {
                    return Err(refused("manager_v2_launch_changed"));
                }
            }
        }
        Ok(authority)
    }

    /// Called only with the predecessor spawn guard held, after the runtime has
    /// proved its observed process incarnation settled. Only the event component
    /// may advance; hierarchy and custody authority may never be refreshed away.
    pub fn manager_action_accept_settled_fence(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let op = self.manager_action_assert_claim(claim)?;
        let authority =
            self.manager_stored_action_authority(&op.context.origin, &op.context.request)?;
        let epic = action_epic(claim.action())
            .ok_or_else(|| refused("manager_v2_invalid_settlement_witness"))?;
        self.manager_v2_require_epic(&authority, epic)?;
        let old = action_fence(claim.action()).expect("lead fence");
        let current = self.manager_action_lead_fence(epic)?;
        if old.lead_session_id != current.lead_session_id
            || old.lead_generation != current.lead_generation
            || old.custody_generation != current.custody_generation
        {
            return Err(refused("manager_v2_lead_changed"));
        }
        if let Some(id) = current.lead_session_id {
            let session = self
                .get_session(id)?
                .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
            if matches!(
                session.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
            ) {
                return Err(refused("manager_v2_predecessor_unsettled"));
            }
        }
        let key = claim.id().to_string();
        let prior = self.manager_v2_record(&authority.config, EXECUTION_KIND, &key)?;
        let mut payload = prior
            .as_ref()
            .map(|r| r.payload.clone())
            .unwrap_or(json!({}));
        payload["settled_fence"] = serde_json::to_value(current)?;
        self.manager_v2_put_record(
            &authority.config,
            EXECUTION_KIND,
            &key,
            Some(epic),
            prior.map_or(0, |r| r.row_version),
            &payload,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Seat a lost lead's replacement after durable custody admission, before
    /// token mint or provider dispatch. A failed launch retains this recoverable
    /// lead; it never restores an archived predecessor or overwrites a new lead.
    pub fn publish_manager_lost_lead_replacement(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<bool> {
        let ManagerActionV2::ReplaceLead {
            epic_id, expected, ..
        } = claim.action()
        else {
            return Ok(false);
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_runtime_gate_on(claim)?;
        if let Some(old) = expected.lead_session_id {
            let old = self
                .get_session(old)?
                .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
            if !matches!(old.status, SessionStatus::Archived | SessionStatus::Deleted) {
                return Ok(false);
            }
        }
        let target = claim
            .operation
            .context
            .target_session_id
            .expect("replacement candidate");
        let candidate = self
            .get_session(target)?
            .ok_or_else(|| refused("manager_v2_candidate_unavailable"))?;
        if candidate.status != SessionStatus::Starting
            || candidate.parent_id != Some(*epic_id)
            || candidate.project_id != Some(claim.operation.project_id)
            || !rsi_common::is_leaf_kind(candidate.session_kind)
            || self.is_non_lead_worker_session(target)?
            || self.session_model_invocation_id(target)?.is_none()
        {
            return Err(refused("manager_v2_candidate_changed"));
        }
        self.set_lead_session(*epic_id, Some(target))?;
        let key = claim.id().to_string();
        let prior = self.manager_v2_record(&authority.config, EXECUTION_KIND, &key)?;
        let mut payload = prior
            .as_ref()
            .map(|r| r.payload.clone())
            .unwrap_or(json!({}));
        payload["settled_fence"] = serde_json::to_value(self.manager_action_lead_fence(*epic_id)?)?;
        self.manager_v2_put_record(
            &authority.config,
            EXECUTION_KIND,
            &key,
            Some(*epic_id),
            prior.map_or(0, |r| r.row_version),
            &payload,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// A manager continuation publishes its admitted invocation before the
    /// backend effect, rather than relying on the generic asynchronous mirror.
    pub fn bind_manager_resume_invocation(
        &self,
        claim: &ManagerActionClaimV2,
        invocation: Uuid,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        if !matches!(claim.action(), ManagerActionV2::ResumeLead { .. }) {
            return Err(refused("manager_v2_not_resume_action"));
        }
        let target = claim
            .operation
            .context
            .target_session_id
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        let bound:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1 AND session_id=?2 AND project_id=?3 AND dedup_key=?4)",params![invocation.to_string(),target.to_string(),claim.operation.project_id.to_string(),format!("manager.action:{}",claim.id())],|r|r.get(0))?;
        if !bound {
            return Err(refused("manager_v2_invocation_changed"));
        }
        self.set_session_model_invocation(target, Some(invocation))?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish_manager_action(
        &self,
        claim: &ManagerActionClaimV2,
        state: ManagerActionStateV2,
        outcome: &str,
    ) -> Result<ManagerActionReceiptV2> {
        self.finish_manager_action_detailed(claim, state, outcome, None)
    }

    /// `finish_manager_action` plus the bounded `outcome_detail` (#3640): the
    /// redacted error behind a catch-all outcome, kept on the receipt.
    pub fn finish_manager_action_detailed(
        &self,
        claim: &ManagerActionClaimV2,
        state: ManagerActionStateV2,
        outcome: &str,
        detail: Option<&str>,
    ) -> Result<ManagerActionReceiptV2> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let receipt = self.finish_manager_action_result_on(claim, state, outcome, None, detail)?;
        tx.commit()?;
        // #1553: a launch that ends without its worker tells its Issue. Best
        // effort: the receipt is already durable and is the record.
        if let Err(error) =
            self.note_issue_worker_launch_ended(&claim.operation, receipt.state, outcome)
        {
            tracing::warn!(operation_id = %claim.id(), %error, "issue worker launch note not appended");
        }
        Ok(receipt)
    }

    /// #1042: a succeeded `pause_lead` suspends the fenced lead's resume
    /// wakes and a succeeded `resume_lead` restores exactly those. Shared by the
    /// normal finish and the settlement of an uncertain pause.
    pub(super) fn apply_lead_wake_effect(
        &self,
        action: &ManagerActionV2,
        action_id: Uuid,
    ) -> Result<()> {
        let lead = action_fence(action).and_then(|f| f.lead_session_id);
        match (action, lead) {
            (ManagerActionV2::PauseLead { .. }, Some(lead)) => {
                self.suspend_lead_resume_wakes(lead, action_id)?;
            }
            (ManagerActionV2::ResumeLead { .. }, Some(lead)) => {
                self.restore_lead_suspended_wakes(lead)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn finish_manager_action_on(
        &self,
        claim: &ManagerActionClaimV2,
        state: ManagerActionStateV2,
        outcome: &str,
    ) -> Result<ManagerActionReceiptV2> {
        self.finish_manager_action_result_on(claim, state, outcome, None, None)
    }

    /// `finish_manager_action_on` plus the bounded K14 `operator_result`.
    pub(super) fn finish_manager_action_result_on(
        &self,
        claim: &ManagerActionClaimV2,
        state: ManagerActionStateV2,
        outcome: &str,
        operator_result: Option<rsi_common::manager_operator_delegation::OperatorCallResultV1>,
        detail: Option<&str>,
    ) -> Result<ManagerActionReceiptV2> {
        if let Some(result) = &operator_result {
            result.validate().map_err(refused)?;
        }
        if matches!(
            state,
            ManagerActionStateV2::Queued | ManagerActionStateV2::Running
        ) {
            return Err(refused("manager_v2_invalid_result"));
        }
        text(outcome, 256).map_err(refused)?;
        let op = self.manager_action_assert_claim(claim)?;
        let state = self.finish_manager_succession_on(claim.id(), state)?;
        let mut receipt = op.receipt.clone();
        receipt.state = state;
        receipt.row_version += 1;
        receipt.outcome = Some(outcome.into());
        receipt.outcome_detail = detail.map(bounded_outcome_detail);
        receipt.refresh_action_metadata(&op.context.request.operation);
        receipt.operator_result = operator_result.map(Box::new);
        // #2652: a succeeded `succeed_manager` names the wakes its publication
        // cancelled, read back from that transaction's audit event, so the
        // stored receipt (every later read and replay) carries the same ids.
        receipt.cancelled_wakes = match (&op.context.request.operation, state) {
            (ManagerActionV2::SucceedManager { .. }, ManagerActionStateV2::Succeeded) => {
                self.succession_cancelled_wakes(claim.id())?
            }
            _ => None,
        };
        let changed=self.conn.execute("UPDATE harness_manager_v2_operations SET state=?2,row_version=?3,outcome_json=?4,updated_at=?5 WHERE id=?1 AND state='running' AND row_version=?6 AND claim_boot_id=?7",params![claim.id().to_string(),state_name(state),receipt.row_version,serde_json::to_string(&receipt)?,now(),claim.operation.receipt.row_version,claim.boot_id.to_string()])?;
        if changed != 1 {
            return Err(refused("manager_v2_claim_changed"));
        }
        if state == ManagerActionStateV2::Succeeded
            && op
                .context
                .issue_binding
                .as_ref()
                .is_some_and(|b| b.continue_from.is_some())
            && let Some(successor) = op.context.target_session_id
        {
            self.carry_worker_job_wakes_on(successor)?;
        }
        if matches!(
            state,
            ManagerActionStateV2::Failed
                | ManagerActionStateV2::Blocked
                | ManagerActionStateV2::Revoked
        ) {
            self.retire_unstarted_issue_worker_watch_on(&op)?;
        }
        // #2455: a failure that finishes after its scope changed is retired
        // by the queue trigger; hand it to the current owner in this commit.
        self.escalate_manager_action_failure_if_retired(
            op.project_id,
            op.manager_session_id,
            op.scope_version,
            &receipt,
        )?;
        // #1042: the pause's success and the suspension of the lead's resume
        // wakes commit together; `resume_lead` restores exactly that record.
        if state == ManagerActionStateV2::Succeeded {
            self.apply_lead_wake_effect(&op.context.request.operation, claim.id())?;
        }
        if state == ManagerActionStateV2::Succeeded
            && matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
            && matches!(
                op.context.request.operation,
                ManagerActionV2::ResumeLead { .. }
                    | ManagerActionV2::RetryLead { .. }
                    | ManagerActionV2::ReplaceLead { .. }
                    | ManagerActionV2::AssignLead {
                        session_id: Some(_),
                        ..
                    }
            )
        {
            let actor = match op.context.origin {
                ManagerActionOriginV2::Agent { caller } => caller,
                _ => unreachable!(),
            };
            let mut cleared_targets = action_fence(&op.context.request.operation)
                .and_then(|f| f.lead_session_id)
                .into_iter()
                .collect::<Vec<_>>();
            if let ManagerActionV2::AssignLead {
                session_id: Some(target),
                ..
            } = &op.context.request.operation
            {
                if !cleared_targets.contains(target) {
                    cleared_targets.push(*target);
                }
            }
            for old in cleared_targets {
                if self.clear_soft_operator_pause(old, actor, "manager_restart")? {
                    if let Some(config) = self.get_harness_manager(op.project_id)? {
                        self.manager_v2_event(&config, Some(actor), "operator_pause_cleared", &old.to_string(), receipt.row_version, &json!({"action_id":claim.id(),"action":op.context.request.operation.action_kind(),"level":"soft","session_id":old}))?;
                    }
                }
            }
            if !matches!(
                op.context.request.operation,
                ManagerActionV2::AssignLead { .. }
            ) {
                if let Some(config) = self.get_harness_manager(op.project_id)? {
                    if config.manager_session_id == op.manager_session_id
                        && config.row_version == op.scope_version
                    {
                        let epic = action_epic(&op.context.request.operation).expect("lead action");
                        self.manager_v2_clear_lead_pause(
                            &config,
                            epic,
                            op.context.manager_pause_version,
                            &format!("manager_action:{}", claim.id()),
                        )?;
                    }
                }
            }
        }
        let actor = match op.context.origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller.to_string()),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        self.conn.execute("INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,actor_session_id,kind,record_key,row_version,payload_json,created_at) VALUES(?1,?2,?3,?4,'action_result',?5,?6,?7,?8)",params![op.project_id.to_string(),op.manager_session_id.to_string(),op.scope_version,actor,claim.id().to_string(),receipt.row_version,serde_json::to_string(&receipt)?,now()])?;
        self.conn.execute(
            "UPDATE harness_manager_v2_records SET archived=1,updated_at=?5
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND record_key=?4 AND kind IN ('lifecycle_context','lifecycle_execution')",
            params![
                op.project_id.to_string(),
                op.manager_session_id.to_string(),
                op.scope_version,
                claim.id().to_string(),
                now()
            ],
        )?;
        Ok(receipt)
    }

    /// One transaction owns container identity, normalized tags, provenance,
    /// effective v1 enrollment, result and audit. Never call cascading operators.
    pub fn apply_manager_container_action(
        &self,
        claim: &ManagerActionClaimV2,
        created: Option<&Session>,
    ) -> Result<(Session, Vec<Uuid>)> {
        use ManagerActionV2::*;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_runtime_gate_on(claim)?;
        let id = claim
            .operation
            .context
            .target_session_id
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        let mut transitioned = vec![id];
        match claim.action() {
            CreateContainer {
                kind, parent_id, ..
            } => {
                let session = created.ok_or_else(|| refused("manager_v2_container_unavailable"))?;
                if session.id != id
                    || session.parent_id != *parent_id
                    || session.session_kind != *kind
                    || session.project_id != Some(authority.config.project_id)
                    || session.lead_session_id.is_some()
                    || session.sandbox_root.is_some()
                {
                    return Err(refused("manager_v2_container_identity_changed"));
                }
                // Pending reservations have already been counted at admission.
                if *kind == SessionKind::Epic
                    && !authority.config.has_dynamic_scope()
                    && authority.config.epic_ids.len() >= 32
                {
                    return Err(refused("manager_v2_scope_limit"));
                }
                self.insert_session(session)?;
                for tag in &session.tags {
                    self.conn.execute(
                        "INSERT INTO session_tags(session_id,tag) VALUES(?1,?2)",
                        params![id.to_string(), tag],
                    )?;
                }
                self.manager_action_bind_entity(
                    claim,
                    if *kind == SessionKind::Epic {
                        "Epic"
                    } else {
                        "Group"
                    },
                )?;
            }
            UpdateContainer {
                name, description, ..
            } => {
                self.conn.execute("UPDATE sessions SET title=?2,query=?2,description=?3,updated_at=?4 WHERE id=?1",params![id.to_string(),name,description,now()])?;
            }
            ArchiveContainer { .. } => {
                transitioned = self.archive_manager_container_rows(id)?;
            }
            DeleteContainer { .. } => {
                self.conn.execute("UPDATE sessions SET status='Deleted',lead_session_id=NULL,updated_at=?2 WHERE id=?1", params![id.to_string(), now()])?;
            }
            RestoreContainer { .. } => {
                transitioned = self.restore_manager_container_rows(id)?;
            }
            _ => return Err(refused("manager_v2_not_container_action")),
        }
        self.finish_manager_action_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "container_committed",
        )?;
        if matches!(claim.action(), ManagerActionV2::ArchiveContainer { .. }) {
            let archived_ids = transitioned
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            self.conn.execute(
                "UPDATE harness_manager_v2_operations SET outcome_json=json_set(outcome_json,'$.execution.cascade_archived_ids',json(?2)) WHERE id=?1",
                params![claim.id().to_string(), serde_json::to_string(&archived_ids)?],
            )?;
        }
        let session = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        tx.commit()?;
        Ok((session, transitioned))
    }

    /// One transaction re-runs the runtime gate (scope, grant, fence, state,
    /// human gate), applies the single-row effect and records the result.
    /// Archive never runs archive cleanup or a sandbox purge.
    pub fn apply_manager_session_action(&self, claim: &ManagerActionClaimV2) -> Result<Session> {
        use ManagerActionV2::{ArchiveSession, RestoreSession, UpdateSession};
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_runtime_gate_on(claim)?;
        let id = claim
            .action()
            .housekeeping_session()
            .ok_or_else(|| refused("manager_v2_not_session_action"))?;
        let stamp = now();
        let outcome = match claim.action() {
            ArchiveSession { .. } => {
                let changed = self.conn.execute(
                    "UPDATE sessions SET status='Archived',pending_archive=0,
                     retry_attempt=COALESCE(max_retries,retry_attempt),updated_at=?2
                     WHERE id=?1 AND status IN ('Completed','Failed','Interrupted')",
                    params![id.to_string(), stamp],
                )?;
                if changed != 1 {
                    return Err(refused("manager_v2_session_state_changed"));
                }
                Self::resolve_c5_autofile_pending_tx(&tx, id)?;
                Self::cancel_queued_recovery_for_archive_on(&tx, id)?;
                "session_archived"
            }
            RestoreSession { .. } => {
                let changed = self.conn.execute(
                    "UPDATE sessions SET status='Completed',pending_archive=0,
                     stop_reason=COALESCE(NULLIF(TRIM(stop_reason),''),'completed:restored'),updated_at=?2
                     WHERE id=?1 AND status='Archived'",
                    params![id.to_string(), stamp],
                )?;
                if changed != 1 {
                    return Err(refused("manager_v2_session_state_changed"));
                }
                "session_restored"
            }
            UpdateSession { patch, .. } => {
                let tags = self.manager_action_session_patch_tags(&authority, patch)?;
                let key = id.to_string();
                if let Some(title) = &patch.title {
                    self.conn.execute(
                        "UPDATE sessions SET title=?2 WHERE id=?1",
                        params![key, title],
                    )?;
                }
                if let Some(description) = &patch.description {
                    self.conn.execute(
                        "UPDATE sessions SET description=?2 WHERE id=?1",
                        params![key, description.value()],
                    )?;
                }
                if let Some(rating) = &patch.rating {
                    self.conn.execute(
                        "UPDATE sessions SET rating=?2 WHERE id=?1",
                        params![key, rating.value().map(|v| i64::from(*v))],
                    )?;
                }
                if let Some(task) = &patch.active_task {
                    self.conn.execute(
                        "UPDATE sessions SET active_task=?2 WHERE id=?1",
                        params![key, task.value()],
                    )?;
                }
                if let Some(label) = &patch.label {
                    self.conn.execute(
                        "UPDATE sessions SET group_id=?2 WHERE id=?1",
                        params![key, label.value().map(ToString::to_string)],
                    )?;
                }
                if let Some(tags) = tags {
                    self.conn
                        .execute("DELETE FROM session_tags WHERE session_id=?1", [&key])?;
                    for tag in &tags {
                        self.conn.execute(
                            "INSERT INTO session_tags(session_id,tag) VALUES(?1,?2)",
                            params![key, tag],
                        )?;
                    }
                    // Same legacy `sessions.tag` projection as tag_ops: the
                    // lexicographically first tag (the set is non-empty).
                    self.conn.execute(
                        "UPDATE sessions SET tag=(SELECT tag FROM session_tags
                           WHERE session_id=?1 ORDER BY tag ASC LIMIT 1) WHERE id=?1",
                        [&key],
                    )?;
                }
                self.conn.execute(
                    "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                    params![key, stamp],
                )?;
                "session_updated"
            }
            _ => return Err(refused("manager_v2_not_session_action")),
        };
        self.finish_manager_action_on(claim, ManagerActionStateV2::Succeeded, outcome)?;
        let session = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        tx.commit()?;
        Ok(session)
    }

    /// Archive the validated cascade rows without touching already-Archived rows.
    fn archive_manager_container_rows(&self, id: Uuid) -> Result<Vec<Uuid>> {
        let members = self.manager_action_validate_archive_cascade(id)?;
        let transitioned = members
            .iter()
            .filter(|(_, was_archived)| !was_archived)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let stamp = now();
        for (member, was_archived) in &members {
            if !*was_archived {
                self.conn.execute(
                    "UPDATE sessions SET status='Archived',lead_session_id=NULL,
                     retry_attempt=COALESCE(max_retries,retry_attempt),updated_at=?2
                     WHERE id=?1 AND status IN ('Completed','Failed','Interrupted')",
                    params![member.to_string(), stamp],
                )?;
                self.conn.execute(
                    "DELETE FROM daemon_settings WHERE key=?1",
                    [super::daemon_settings::c5_autofile_pending_key(*member)],
                )?;
                Self::cancel_queued_recovery_for_archive_on(&self.conn, *member)?;
            }
        }
        Ok(transitioned)
    }

    /// Apply restore gates and row transitions inside the caller's transaction.
    fn restore_manager_container_rows(&self, id: Uuid) -> Result<Vec<Uuid>> {
        let container = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_v2_container_changed"))?;
        let recorded = self.manager_action_validate_restore_container(&container, true)?;
        let mut members = recorded
            .as_ref()
            .map(|record| record.ids.clone())
            .unwrap_or_default();
        if recorded.is_some() && !members.contains(&id) {
            members.push(id);
        }
        // A Deleted container restore retains the original manager behavior.
        // Cascade evidence applies only to the exact Archived rows it recorded.
        if recorded.is_none() {
            members = vec![id];
        }
        let stamp = now();
        let mut transitioned = Vec::new();
        for member in members {
            let status: Option<String> = self
                .conn
                .query_row(
                    "SELECT status FROM sessions WHERE id=?1",
                    [member.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if status.as_deref() == Some("Archived")
                || (member == id && recorded.is_none() && status.as_deref() == Some("Deleted"))
            {
                let previous_status = status.as_deref().unwrap_or("Archived");
                let changed = self.conn.execute(
                    "UPDATE sessions SET status='Completed',pending_archive=0,lead_session_id=NULL,
                     stop_reason=COALESCE(NULLIF(TRIM(stop_reason),''),'completed:restored'),updated_at=?2 WHERE id=?1 AND status=?3",
                    params![member.to_string(), stamp, previous_status],
                )?;
                if changed == 1 {
                    transitioned.push(member);
                }
            }
        }
        Ok(transitioned)
    }

    pub(crate) fn manager_action_bind_entity(
        &self,
        claim: &ManagerActionClaimV2,
        kind: &str,
    ) -> Result<()> {
        self.conn.execute("INSERT INTO harness_manager_v2_entities(session_id,operation_id,project_id,manager_session_id,scope_version,policy_version,kind,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![claim.operation.context.target_session_id.map(|v|v.to_string()),claim.id().to_string(),claim.operation.project_id.to_string(),claim.operation.manager_session_id.to_string(),claim.operation.scope_version,claim.operation.context.request.fence.policy_version,kind,now()])?;
        Ok(())
    }

    /// The runtime proves provider establishment and quiescent old ownership
    /// while holding both spawn guards and active-map custody before this CAS.
    pub fn commit_manager_lead_action(&self, claim: &ManagerActionClaimV2) -> Result<Vec<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        let epic =
            action_epic(claim.action()).ok_or_else(|| refused("manager_v2_epic_unavailable"))?;
        let target = claim.operation.context.target_session_id;
        if let Some(target) = target {
            let candidate = self
                .get_session(target)?
                .ok_or_else(|| refused("manager_v2_candidate_unavailable"))?;
            if candidate.parent_id != Some(epic)
                || candidate.project_id != Some(claim.operation.project_id)
                || !rsi_common::legal_children(Some(SessionKind::Epic))
                    .contains(&candidate.session_kind)
                || !rsi_common::is_leaf_kind(candidate.session_kind)
                || !matches!(
                    candidate.status,
                    SessionStatus::Running
                        | SessionStatus::Starting
                        | SessionStatus::Completed
                        | SessionStatus::Interrupted
                        | SessionStatus::Failed
                )
            {
                return Err(refused("manager_v2_candidate_changed"));
            }
            // #2815: an Issue-bound or topology worker (or a rotation child of
            // one) is never an Epic lead, whichever path writes the pointer.
            if self.is_non_lead_worker_session(target)? {
                return Err(refused("manager_v2_candidate_changed"));
            }
        }
        let op = self.manager_action_assert_claim(claim)?;
        let preassigned = matches!(claim.action(), ManagerActionV2::ReplaceLead { .. })
            && op
                .settled_fence
                .as_ref()
                .is_some_and(|f| f.lead_session_id == target);
        let expected = action_fence(claim.action()).expect("lead fence");
        self.reject_nonterminal_agent_successor_epic_lead_mutation(epic)?;
        if !preassigned {
            let changed=self.conn.execute("UPDATE sessions SET lead_session_id=?2,updated_at=?3 WHERE id=?1 AND lead_session_id IS ?4 AND EXISTS(SELECT 1 FROM epic_lead_generations WHERE epic_id=?1 AND generation=?5)",params![epic.to_string(),target.map(|v|v.to_string()),now(),expected.lead_session_id.map(|v|v.to_string()),expected.lead_generation])?;
            if changed != 1 {
                return Err(refused("manager_v2_lead_changed"));
            }
        }
        let jobs = match (expected.lead_session_id, target) {
            (Some(old), Some(new)) if old != new => {
                self.readdress_open_manager_requests(epic, old, new)?
            }
            _ => Vec::new(),
        };
        if matches!(
            claim.action(),
            ManagerActionV2::RetryLead { .. } | ManagerActionV2::ReplaceLead { .. }
        ) {
            self.manager_action_bind_entity(claim, "session")?;
            // #953: the superseded lineage never keeps a live resume wake,
            // atomically with the lead CAS above.
            if let Some(old) = expected.lead_session_id.filter(|old| Some(*old) != target) {
                self.retire_handover_lineage_resume_wakes_on(claim, old)?;
            }
        }
        self.finish_manager_action_on(claim, ManagerActionStateV2::Succeeded, "lead_committed")?;
        tx.commit()?;
        Ok(jobs)
    }

    #[cfg(test)]
    pub(crate) fn commit_manager_created_session(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<()> {
        self.commit_manager_created_session_with_watch(claim, None)
    }

    /// #1115: an Issue-bound launch persists its caller's terminal watch in the
    /// same transaction as the success, so no crash window leaves a launched
    /// worker unwatched. An enabled watch for the same (owner, worker) is kept.
    pub fn commit_manager_created_session_with_watch(
        &self,
        claim: &ManagerActionClaimV2,
        watch: Option<&rsi_common::types::ScheduledJob>,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        self.manager_action_bind_entity(claim, "session")?;
        self.finish_manager_action_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "session_established",
        )?;
        if let Some(watch) = watch {
            self.insert_on_terminal_watch_if_absent_conn(&tx, watch)?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// #4011: the fields of `expected` that differ from the `current` fence, each as
/// `field expected X, current Y` (the values are not secret to the authorized
/// manager); empty when the fence holds. An out-of-range `expected` is named too.
fn lead_fence_mismatch(expected: &ManagerLeadFenceV2, current: &ManagerLeadFenceV2) -> String {
    fn show<T: std::fmt::Display>(value: Option<T>) -> String {
        value.map_or_else(|| "null".into(), |v| v.to_string())
    }
    let mut fields = Vec::new();
    if current.lead_session_id != expected.lead_session_id {
        fields.push(format!(
            "lead_session_id expected {}, current {}",
            show(expected.lead_session_id),
            show(current.lead_session_id)
        ));
    }
    if current.lead_generation != expected.lead_generation || expected.lead_generation <= 0 {
        fields.push(format!(
            "lead_generation expected {}, current {}",
            expected.lead_generation, current.lead_generation
        ));
    }
    if current.event_sequence != expected.event_sequence || expected.event_sequence < 0 {
        fields.push(format!(
            "event_sequence expected {}, current {}",
            expected.event_sequence, current.event_sequence
        ));
    }
    if current.custody_generation != expected.custody_generation {
        fields.push(format!(
            "custody_generation expected {}, current {}",
            show(expected.custody_generation),
            show(current.custody_generation)
        ));
    }
    fields.join("; ")
}

fn parse_id(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| refused("manager_v2_invalid_stored_identity"))
}

#[cfg(test)]
mod create_container_refusal_tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn create_container_refusal_names_the_remedy_and_keeps_the_code() {
        use super::{SessionKind, create_container_refusal};
        let off = create_container_refusal(SessionKind::Epic, None, false);
        assert!(
            off.starts_with("manager_v2_container_out_of_scope: "),
            "{off}"
        );
        assert!(off.contains("under a Group"), "{off}");
        assert!(off.contains("allow_create_groups"), "{off}");
        let on = create_container_refusal(SessionKind::Epic, None, true);
        assert!(on.contains("create a top-level Group first"), "{on}");
        let group = create_container_refusal(SessionKind::Group, None, false);
        assert!(group.contains("needs allow_create_groups"), "{group}");
        assert_eq!(
            create_container_refusal(SessionKind::Epic, Some(SessionKind::Group), true),
            "manager_v2_container_out_of_scope"
        );
    }
}

/// Longest `outcome_detail` a receipt keeps, in characters (#3640).
pub const OUTCOME_DETAIL_MAX_CHARS: usize = 300;

/// Bound a receipt's `outcome_detail`: control characters become spaces and the
/// text is cut at [`OUTCOME_DETAIL_MAX_CHARS`] on a character boundary. The
/// caller redacts secrets; this keeps the stored text one short line.
fn bounded_outcome_detail(detail: &str) -> String {
    detail
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(OUTCOME_DETAIL_MAX_CHARS)
        .collect()
}
