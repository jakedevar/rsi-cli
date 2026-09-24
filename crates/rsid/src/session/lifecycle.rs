//! Session lifecycle operations: continue, interrupt, delete, archive, model switch, shutdown.

use crate::bus::{DaemonEvent, EventBus};
use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::model_control::{
    AdmissionDecision, CapacityAdmissionDecision, InvocationCompletion, admit_capacity_invocation,
    admit_invocation, complete_invocation, complete_invocation_by_id, hash_request_fingerprint,
    resume_unexecuted_capacity_delivery_admission,
};
use crate::sandbox::cleanup::{
    self, CleanupBlockedReason, CleanupCandidate, CleanupDecision, OwnershipObservation,
};
use crate::sandbox::custody::{CustodyClassification, CustodyService, EffectKind, PreparedLaunch};
use crate::store::agent_child_relaunch_intents::{
    InsertRelaunchIntent, RelaunchIntentRow, RelaunchState,
};
use crate::store::daemon_settings::{AutofileCause, RecoveryDisposition};
use crate::store::manager_actions::fence::{
    CONTINUATION_TARGET_BUSY, CONTINUATION_TIP_CHANGED, ContinuationAuthorityV1,
    ContinuationFenceV1,
};
use rsi_common::agent_coordination::{
    AgentContinuationCursorV1, AgentContinueChildResultV1, AgentContinueErrorCodeV1,
    AgentContinueRelaunchV1,
};
use rsi_common::archive_cleanup::ArchiveSessionResultV1;
use rsi_common::model_control::{InvocationOwner, ModelUsageConfidence};
use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, EventType, ReserveIdeaControllerRequestV1, Role,
    Session, SessionKind, SessionProvider, SessionStatus, WorkflowStage,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

#[cfg(test)]
type FinalizerPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
fn finalizer_pauses() -> &'static std::sync::Mutex<HashMap<Uuid, FinalizerPause>> {
    static PAUSES: std::sync::OnceLock<std::sync::Mutex<HashMap<Uuid, FinalizerPause>>> =
        std::sync::OnceLock::new();
    PAUSES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn pause_finalizer_after_active_removal(
    session_id: Uuid,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    finalizer_pauses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id, (reached_tx, release_rx));
    (reached_rx, release_tx)
}

mod manager_question_cleanup;

use super::SessionManager;
use super::types::{
    CompletedSession, PersistenceHandle, RetryAdmission, TerminalFinalizeDecision, TrackedSession,
    install_context_budget, sanitize_codex_restored_context_usage,
};

#[derive(Debug, Clone, Copy)]
struct CapacityDeliveryContext {
    wake_job_id: Uuid,
    due_slot: chrono::DateTime<chrono::Utc>,
}

enum ContinuationIntent {
    Operator,
    ExistingAuthority,
    AgentChild(FreshRelaunchIntent),
    Capacity(CapacityDeliveryContext),
    ManagerNotice {
        job_ids: Vec<Uuid>,
    },
    ManagerAction(Box<crate::store::manager_actions::ManagerActionClaimV2>),
    ManagerDecision(Box<crate::store::manager_coordinator::ManagerDecisionDeliveryV2>),
    /// Scheduled Resume wake or ordinary child-watch delivery. The exact job
    /// rows are revalidated under the target spawn guard (K2 review (a)).
    ScheduledWake {
        job_ids: Vec<Uuid>,
    },
    /// #669: daemon-owned in-place resume of a Failed appointed manager seat.
    ManagerSeat(Box<crate::store::manager_intent::manager_seat::ManagerSeatClaimV1>),
}

struct FreshRelaunchIntent {
    row: RelaunchIntentRow,
    observed: AgentContinuationCursorV1,
    manager_scope: Option<crate::store::harness_manager::ManagerSessionScope>,
}

/// Drop an archived row's completed entry, cancelling any pending retry.
fn evict_archived_completed(
    completed: &mut std::collections::HashMap<Uuid, CompletedSession>,
    session_id: Uuid,
) {
    if let Some(mut cs) = completed.remove(&session_id)
        && let Some(cancel) = cs.retry_cancel.take()
    {
        let _ = cancel.send(());
    }
}

fn unavailable_resume_error(provider: SessionProvider) -> DaemonError {
    let text = match provider {
        SessionProvider::Claude
        | SessionProvider::Codex
        | SessionProvider::Pioneer
        | SessionProvider::OpenRouter
        | SessionProvider::Bedrock => {
            let provider_name = match provider {
                SessionProvider::Claude => "Claude",
                SessionProvider::Codex => "Codex",
                SessionProvider::Pioneer => "Pioneer",
                SessionProvider::OpenRouter => "OpenRouter",
                SessionProvider::Bedrock => "Bedrock",
                _ => unreachable!(),
            };
            format!(
                "Cannot resume: no {provider_name} session ID was captured during the original run. This session predates session ID tracking or failed before the first event."
            )
        }
        SessionProvider::Antigravity => {
            "Cannot resume: no Antigravity session ID was captured during the original run.".into()
        }
        _ => "Cannot continue: no provider session ID captured".into(),
    };
    DaemonError::Rpc(text)
}

fn child_relaunch_row_outcome(row: &RelaunchIntentRow) -> Result<ContinueSessionOutcome> {
    match row.state {
        RelaunchState::Intent => Err(DaemonError::Store(
            "child relaunch intent remained open after recovery".into(),
        )),
        RelaunchState::Abandoned => Err(crate::error::agent_continue_error_with_receipt(
            AgentContinueErrorCodeV1::RelaunchAbandoned,
            row.abandon_reason.clone(),
            None,
            row.receipt_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
        )),
        RelaunchState::Launched => {
            let receipt: AgentContinueChildResultV1 =
                serde_json::from_str(row.receipt_json.as_deref().ok_or_else(|| {
                    DaemonError::Store("launched child relaunch has no receipt".into())
                })?)?;
            let mut receipt = receipt;
            if let Some(relaunch) = receipt.relaunch.as_mut() {
                relaunch.deduplicated = true;
            }
            Ok(ContinueSessionOutcome::AgentFresh(receipt))
        }
    }
}

/// Test barrier between a fenced continuation's dispatch-time capture and its
/// spawn-guard acquisition (K2 finding c): `reached` fires at the barrier and
/// the continuation proceeds only after `resume` is sent, so a race test can
/// order capture -> mutation -> guarded check with the real guards.
#[cfg(test)]
type ContinuationPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

/// Where a continuation test barrier sits.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContinuationPauseSeam {
    /// After the fence capture, before guard(tip).
    BeforeGuard,
    /// After the guarded fence check, before the effect claim (review round 3).
    AfterFenceCheck,
    /// `AgentContinueChild`: after every authorization await, immediately
    /// before the fence capture (review round 2
    /// `agent_continue_lead_authority_race`).
    AgentContinueAuthorized,
}

#[cfg(test)]
fn continuation_pauses()
-> &'static std::sync::Mutex<HashMap<(ContinuationPauseSeam, Uuid), ContinuationPause>> {
    type Pauses = std::sync::Mutex<HashMap<(ContinuationPauseSeam, Uuid), ContinuationPause>>;
    static PAUSES: std::sync::OnceLock<Pauses> = std::sync::OnceLock::new();
    PAUSES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub fn install_continuation_pause_for_test(
    session_id: Uuid,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    install_continuation_seam_pause_for_test(ContinuationPauseSeam::BeforeGuard, session_id)
}

#[cfg(test)]
pub fn install_continuation_seam_pause_for_test(
    seam: ContinuationPauseSeam,
    session_id: Uuid,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    continuation_pauses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((seam, session_id), (reached_tx, resume_rx));
    (reached_rx, resume_tx)
}

#[cfg(test)]
async fn pause_continuation_before_guard_for_test(session_id: Uuid) {
    pause_continuation_seam_for_test(ContinuationPauseSeam::BeforeGuard, session_id).await;
}

#[cfg(test)]
pub(super) async fn pause_continuation_seam_for_test(
    seam: ContinuationPauseSeam,
    session_id: Uuid,
) {
    let pause = continuation_pauses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(seam, session_id));
    if let Some((reached, resume)) = pause {
        let _ = reached.send(());
        let _ = resume.await;
    }
}

fn manager_notice_deferred() -> DaemonError {
    DaemonError::InvalidParam("manager_notice_deferred".into())
}

fn manager_notice_scope_revoked() -> DaemonError {
    DaemonError::InvalidParam("manager_notice_scope_revoked".into())
}

/// The provider session id a same-session continuation resumes, or `None`
/// when the original run never captured one. This is the continuation path's
/// single resumability predicate: `Local` reconstructs context from history
/// under a synthetic id; every other provider needs its captured id.
pub(crate) fn resumable_provider_session_id(session: &Session) -> Option<String> {
    match session.provider {
        SessionProvider::Local => Some(
            session
                .claude_session_id
                .clone()
                .unwrap_or_else(|| session.id.to_string()),
        ),
        _ => session.claude_session_id.clone(),
    }
}

/// The single manager resumability predicate. `resume_lead` admission,
/// `retry_lead` admission and the manager continuation gate all call it, so no
/// lead is queued for a resume the gate refuses. `CodexAppServer` leads are never
/// manager-resumable; other providers need a resumable provider session.
pub(crate) fn manager_lead_provider_resumable(session: &Session) -> bool {
    session.provider != SessionProvider::CodexAppServer
        && resumable_provider_session_id(session).is_some()
}

/// Manager `resume_lead` target check, shared by admission (for settled leads)
/// and the continuation gate. A lead that is not a settled leaf is not
/// resumable now; a settled lead failing [`manager_lead_provider_resumable`]
/// gets typed `manager_v2_resume_unavailable` naming `retry_lead`.
pub(crate) fn check_manager_resume_target(session: &Session) -> Result<()> {
    if !matches!(
        session.status,
        SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
    ) || !rsi_common::is_leaf_kind(session.session_kind)
    {
        return Err(DaemonError::InvalidParam(
            "manager_v2_lead_not_resumable".into(),
        ));
    }
    if !manager_lead_provider_resumable(session) {
        return Err(manager_resume_unavailable());
    }
    Ok(())
}

/// Typed manager refusal naming the lifecycle action that can proceed. The
/// message is the bare code, so `safe_action_error` still passes it through.
pub(crate) fn manager_refusal_with_next_action(
    code: &'static str,
    next_action: &'static str,
) -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: rsi_common::rpc::INVALID_PARAMS,
        message: code.into(),
        data: serde_json::json!({ "code": code, "next_action": next_action }),
    }
}

/// A lead whose provider session cannot be resumed; a fresh-successor
/// `retry_lead` is the continuity-preserving route (Issue #670).
pub(crate) fn manager_resume_unavailable() -> DaemonError {
    manager_refusal_with_next_action("manager_v2_resume_unavailable", "retry_lead")
}

/// Reconstruct the monitor's current-turn assistant accumulator from durable
/// events. Refuse on budget exhaustion rather than overlook a split gate report.
/// The caller holds the Store lock; lifecycle dispatch repeats this proof
/// under the session's spawn guard before provider effects.
pub(crate) fn check_manager_action_program_gate(
    store: &crate::store::Store,
    target: Uuid,
    allow_interrupted_resume: bool,
) -> Result<()> {
    // K2 (#390): an explicit manager retirement supersedes exactly the
    // evidence it witnessed; any later output or re-registration is evaluated.
    if store.manager_lead_program_outcome_superseded(target)? {
        return Ok(());
    }
    let interrupted_resume = allow_interrupted_resume
        && store
            .get_session(target)?
            .is_some_and(|s| s.status == SessionStatus::Interrupted);
    check_manager_program_gate(store, target, false, interrupted_resume)
}

/// Classify the lead's current program evidence for a retirement witness.
/// Store errors propagate; only the two typed gate classes are recorded.
pub(crate) fn classify_manager_program_evidence(
    store: &crate::store::Store,
    target: Uuid,
) -> Result<&'static str> {
    match check_manager_program_gate(store, target, false, false) {
        Ok(()) => Ok("none"),
        Err(error) => {
            let message = error.to_string();
            if message.contains("manager_v2_program_evidence_unknown") {
                Ok("program_evidence_unknown")
            } else if message.contains("manager_v2_human_or_recovery_owner") {
                Ok("human_or_recovery_owner")
            } else {
                Err(error)
            }
        }
    }
}

pub(crate) fn check_manager_notice_program_gate(
    store: &crate::store::Store,
    target: Uuid,
) -> Result<()> {
    check_manager_program_gate(store, target, true, false)
}

fn check_manager_program_gate(
    store: &crate::store::Store,
    target: Uuid,
    allow_child_watch: bool,
    allow_interrupted_resume: bool,
) -> Result<()> {
    use super::harness::tools::schedule_wake::{
        deterministic_program_guard_job_id, is_program_guard_sentinel,
    };
    use rsi_common::agent_contract::{
        ContractError, OrchestrationContinuationStateV1, ProgramContinuationIntentV1,
        parse_orchestration_outcome_v1, program_continuation_intent_v1_with_registration,
    };

    const MAX_EVENTS: usize = 256;
    const MAX_BYTES: usize = 256 * 1024;
    let evidence_unknown = || {
        if allow_child_watch {
            manager_notice_deferred()
        } else {
            DaemonError::InvalidParam("manager_v2_program_evidence_unknown".into())
        }
    };
    let recovery_owned = || {
        if allow_child_watch {
            manager_notice_deferred()
        } else {
            DaemonError::InvalidParam("manager_v2_human_or_recovery_owner".into())
        }
    };
    let sentinel_id = deterministic_program_guard_job_id(target);
    let sentinel = store.get_scheduled_job(&sentinel_id)?;
    if sentinel
        .as_ref()
        .is_some_and(|job| !is_program_guard_sentinel(job, target))
        || (sentinel.is_none() && store.scheduled_job_exists(&sentinel_id)?)
    {
        return Err(evidence_unknown());
    }
    let registered = sentinel.as_ref().is_some_and(|job| job.enabled);
    let last_user: Option<i64> = store.conn.query_row(
        "SELECT MAX(sequence) FROM conversation_events WHERE session_id=?1 AND role='User'",
        [target.to_string()],
        |row| row.get(0),
    )?;
    let mut stmt = store.conn.prepare(
        "SELECT substr(content,1,?2), length(CAST(content AS BLOB))
         FROM conversation_events
         WHERE session_id=?1 AND role='Assistant'
           AND sequence > ?4
         ORDER BY sequence LIMIT ?3",
    )?;
    let mut rows = stmt.query(rusqlite::params![
        target.to_string(),
        MAX_BYTES + 1,
        MAX_EVENTS + 1,
        last_user.unwrap_or(-1),
    ])?;
    let mut output = String::new();
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        let bytes: usize = row.get(1)?;
        if count > MAX_EVENTS || bytes > MAX_BYTES.saturating_sub(output.len()) {
            return Err(evidence_unknown());
        }
        output.push_str(&row.get::<_, String>(0)?);
    }
    let outcome = parse_orchestration_outcome_v1(&output);
    if outcome.as_ref().is_ok_and(|outcome| {
        outcome.continuation_state == OrchestrationContinuationStateV1::HumanGate
    }) {
        return Err(recovery_owned());
    }
    // An Interrupted provider need not have emitted a terminal report. An
    // explicit manager Resume may continue its ordinary partial text, after a
    // known User boundary, without pretending malformed report fragments are
    // a valid outcome. No prose is interpreted as completion or permission.
    if registered
        && allow_interrupted_resume
        && last_user.is_some()
        && matches!(outcome, Err(ContractError::MissingField { ref field }) if field == "orchestration_outcome_v1")
        && program_continuation_intent_v1_with_registration(&output, false)
            == ProgramContinuationIntentV1::NotProgram
        && ordinary_interrupted_program_text(&output)
    {
        return if manager_program_continuation_job_enabled(store, target, sentinel_id)? {
            Err(recovery_owned())
        } else {
            Ok(())
        };
    }
    match program_continuation_intent_v1_with_registration(&output, registered) {
        // All remaining missing or malformed evidence stays unknown. Terminal
        // settlement retains responsibility for invalid terminal output.
        ProgramContinuationIntentV1::InvalidProgram(_)
        | ProgramContinuationIntentV1::RequireAnyGuard => Err(evidence_unknown()),
        intent @ (ProgramContinuationIntentV1::RequireChildWatch { job_id }
        | ProgramContinuationIntentV1::RequireResumeWake { job_id })
            if !allow_child_watch =>
        {
            if !registered || job_id == sentinel_id || last_user.is_none() {
                return Err(evidence_unknown());
            }
            // Use the terminal-settlement validator for the declared owner.
            // Interrupted turns do not run that settlement, so a declaration
            // alone cannot prove they have a recovery path.
            if super::agent_verbs::exact_master_continuation_guard_present(
                store,
                target,
                sentinel_id,
                &intent,
            )? {
                return Err(recovery_owned());
            }
            if !allow_interrupted_resume {
                return Err(recovery_owned());
            }
            // A different enabled job (including a pending child-watch
            // delivery) still owns continuation. Inspect raw rows so an
            // unreadable job cannot disappear through tolerant hydration.
            if manager_program_continuation_job_enabled(store, target, sentinel_id)? {
                return Err(recovery_owned());
            }
            // Only proven absence or a well-formed disabled continuation is
            // stranded. Wrong mode/recipient or unreadable rows stay unknown.
            match store.get_scheduled_job(&job_id)? {
                Some(job) => {
                    use rsi_common::types::{Recurrence, WakeMode};
                    let expected_shape = match intent {
                        ProgramContinuationIntentV1::RequireChildWatch { .. } => {
                            matches!(job.wake_mode, WakeMode::OnTerminal(watched) if watched != target)
                        }
                        ProgramContinuationIntentV1::RequireResumeWake { .. } => {
                            job.wake_mode == WakeMode::Resume
                                && matches!(job.schedule.recurrence, Recurrence::Once)
                        }
                        _ => unreachable!("only exact continuation intents enter this gate"),
                    };
                    if job.enabled || job.wake_session_id != Some(target) || !expected_shape {
                        return Err(evidence_unknown());
                    }
                }
                None if store.scheduled_job_exists(&job_id)? => return Err(evidence_unknown()),
                None => {}
            }
            // Existing action claims, runtime retry/capacity/human gates and
            // the spawn guard own the subsequent same-session Resume. This
            // read-only proof neither schedules a wake nor resets a budget.
            Ok(())
        }
        ProgramContinuationIntentV1::RequireResumeWake { .. } => Err(manager_notice_deferred()),
        ProgramContinuationIntentV1::RequireChildWatch { .. }
        | ProgramContinuationIntentV1::NotProgram
        | ProgramContinuationIntentV1::TerminalAllowed => Ok(()),
    }
}

fn manager_program_continuation_job_enabled(
    store: &crate::store::Store,
    target: Uuid,
    sentinel: Uuid,
) -> Result<bool> {
    Ok(store.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
         WHERE enabled=1 AND wake_session_id=?1 AND id<>?2)",
        rusqlite::params![target.to_string(), sentinel.to_string()],
        |row| row.get(0),
    )?)
}

/// Conservatively distinguish ordinary interrupted prose from an attempted
/// report or question. Ambiguous syntax remains unknown; this is not a parser
/// fallback and never manufactures an orchestration outcome.
fn ordinary_interrupted_program_text(output: &str) -> bool {
    let text = output.trim();
    let lower = text.to_ascii_lowercase();
    !text.is_empty()
        && !text.contains(['{', '}', '[', ']', '`', ':', '?'])
        // The legacy ORCHESTRATION COMPLETE heading and Mode field can be
        // interrupted before any colon is emitted. Look through Markdown
        // presentation only to refuse known report tokens, never to parse an
        // outcome. Unbalanced delimiters still leave the evidence unknown.
        && !lower.lines().any(|line| {
            let line = line.replace(['*', '_', '~'], "");
            let mut words = line
                .split(|c: char| c.is_whitespace() || c == '|')
                .map(|word| word.trim_start_matches(['#', '>']))
                .filter(|word| !word.is_empty());
            // Skip nested quote/heading/list prefixes and table cell borders.
            let first = words.find(|word| {
                !matches!(*word, "-" | "+")
                    && !word.strip_suffix(['.', ')']).is_some_and(|number| {
                        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                    })
            });
            match first {
                Some("mode") => true,
                Some("orchestration") => words
                    .next()
                    .is_none_or(|word| "complete".starts_with(word)),
                _ => false,
            }
        })
        && ![
            "orchestration_outcome",
            "continuation_",
            "continuation state",
            "blocker_",
            "human_gate",
            "human gate",
            "queue_exhausted",
            "next_slice",
            "next-slice",
            "next slice",
            "pipeline handoff",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContinueSessionOutcome {
    Started,
    AgentFresh(AgentContinueChildResultV1),
    CapacityLaunchConfirmed {
        invocation_id: Uuid,
        recovered_admission: bool,
    },
    CapacityAlreadyLaunchConfirmed(Uuid),
}

/// Keeps [`SessionManager::CONTINUE_INTERRUPT_WAIT`] honest against the teardown
/// path it is budgeting for. A grace widened in `reaper.rs` without revisiting
/// the RPC deadline is exactly how the original race was introduced — this turns
/// that into a build failure instead of an intermittent user-visible one.
const _: () = assert!(
    SessionManager::CONTINUE_INTERRUPT_WAIT.as_millis()
        >= 2 * super::reaper::TEARDOWN_TERMINAL_WORST_CASE.as_millis(),
    "continue_session's interrupt wait must keep >=100% margin over the teardown worst case",
);

#[cfg(test)]
static CONTINUE_CUSTODY_MUTATION_TEST_KEYS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static CONTINUE_CUSTODY_CONFIG_OBSERVATIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (PathBuf, Option<PathBuf>)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static CONTINUE_CUSTODY_CONFIG_OBSERVATION_KEYS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static CONTINUE_EXECUTION_SCRATCH_FAILURE_KEYS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// The continuation admission dedup identity is available before model
/// admission. Keep the deterministic custody race control keyed to that exact
/// future identity, rather than a process-global one-shot flag.
#[cfg(test)]
fn continue_custody_test_key(session_id: Uuid, initial_sequence: i32) -> String {
    format!(
        "{}:{session_id}:{initial_sequence}",
        rsi_common::model_control::ModelInvocationPurpose::SessionContinueResume.as_str()
    )
}

#[cfg(test)]
fn install_continue_custody_root_mutation_for_test(session_id: Uuid, initial_sequence: i32) {
    CONTINUE_CUSTODY_MUTATION_TEST_KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(continue_custody_test_key(session_id, initial_sequence));
}

#[cfg(test)]
fn install_continue_custody_config_observation_for_test(session_id: Uuid, initial_sequence: i32) {
    CONTINUE_CUSTODY_CONFIG_OBSERVATION_KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(continue_custody_test_key(session_id, initial_sequence));
}

#[cfg(test)]
fn install_continue_execution_scratch_failure_for_test(session_id: Uuid, initial_sequence: i32) {
    CONTINUE_EXECUTION_SCRATCH_FAILURE_KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(continue_custody_test_key(session_id, initial_sequence));
}

#[cfg(test)]
fn apply_continue_execution_scratch_failure_for_test(
    session: &Session,
    initial_sequence: i32,
) -> Result<()> {
    let key = continue_custody_test_key(session.id, initial_sequence);
    if !CONTINUE_EXECUTION_SCRATCH_FAILURE_KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key)
    {
        return Ok(());
    }
    let root = session.sandbox_root.as_ref().ok_or_else(|| {
        DaemonError::Store("continue scratch mutation test needs a sandbox root".to_string())
    })?;
    std::os::unix::fs::symlink(root, root.join("target")).map_err(DaemonError::Io)
}

/// Mutate the real authenticated worktree after preflight and immediately
/// before `begin_effect(ContextRead)`. The revalidation must reject it before
/// context, token/admission, orphan reaping, provider dispatch, or active-map
/// publication.
#[cfg(test)]
fn apply_continue_custody_root_mutation_for_test(
    session: &Session,
    initial_sequence: i32,
) -> Result<()> {
    let key = continue_custody_test_key(session.id, initial_sequence);
    if !CONTINUE_CUSTODY_MUTATION_TEST_KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key)
    {
        return Ok(());
    }
    let root = session.sandbox_root.as_ref().ok_or_else(|| {
        DaemonError::Store("continue custody mutation test needs a sandbox root".to_string())
    })?;
    let moved = root.with_file_name(format!("{}-continue-raced", session.id));
    std::fs::rename(root, moved).map_err(DaemonError::Io)
}

#[cfg(test)]
fn observe_continue_custody_config_for_test(
    session_id: Uuid,
    initial_sequence: i32,
    config: &LaunchConfig,
) {
    let key = continue_custody_test_key(session_id, initial_sequence);
    if !CONTINUE_CUSTODY_CONFIG_OBSERVATION_KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key)
    {
        return;
    }
    let working_dir = config
        .working_dir
        .clone()
        .expect("continue config must carry permit-derived cwd");
    CONTINUE_CUSTODY_CONFIG_OBSERVATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, (working_dir, config.cargo_target_dir.clone()));
}

#[cfg(test)]
fn take_continue_custody_config_for_test(
    session_id: Uuid,
    initial_sequence: i32,
) -> Option<(PathBuf, Option<PathBuf>)> {
    let key = continue_custody_test_key(session_id, initial_sequence);
    CONTINUE_CUSTODY_CONFIG_OBSERVATION_KEYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key);
    CONTINUE_CUSTODY_CONFIG_OBSERVATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key)
}

fn exhaust_retry_budget(session: &mut Session) -> Option<u8> {
    let max_retries = session.max_retries.unwrap_or(0);
    if max_retries == 0 {
        return None;
    }
    session.retry_attempt = Some(max_retries);
    session.max_retries = Some(max_retries);
    Some(max_retries)
}

fn invocation_confidence_from_session(confidence: ContextUsageConfidence) -> ModelUsageConfidence {
    match confidence {
        ContextUsageConfidence::Counted | ContextUsageConfidence::Full => {
            ModelUsageConfidence::Measured
        }
        ContextUsageConfidence::Partial => ModelUsageConfidence::Partial,
        ContextUsageConfidence::Stale => ModelUsageConfidence::Stale,
        ContextUsageConfidence::Missing => ModelUsageConfidence::Unavailable,
        _ => ModelUsageConfidence::Unavailable,
    }
}

fn owner_from_session(session: &Session) -> InvocationOwner {
    InvocationOwner {
        session_id: Some(session.id),
        project_id: session.project_id,
        workflow_id: session.workflow_id,
        scheduled_job_id: session.scheduled_job_id,
        issue_tracker_id: session.issue_tracker_id.clone(),
        issue_identifier: session.issue_identifier.clone(),
        topology_node_id: session.topology_node_id.clone(),
        recursive_graph_id: None,
        recursive_task_id: None,
        recursive_attempt_id: None,
        operator: None,
    }
}

fn cleanup_policy_denied(reason: CleanupBlockedReason) -> DaemonError {
    DaemonError::PolicyDenied(format!("sandbox cleanup blocked: {}", reason.as_str()))
}

fn effective_provider_replacement_config(source: &Session, query: String) -> LaunchConfig {
    let provider = super::provider_spawn::effective_sync_provider(source.provider);
    let sandbox = source
        .sandbox_kind
        .filter(|kind| !matches!(kind, rsi_common::types::SandboxKind::None))
        .map(|kind| rsi_common::types::SandboxSpec {
            kind: Some(kind),
            branch: None,
        });
    LaunchConfig {
        query,
        title: None,
        agent_role: source.agent_role.clone(),
        epic_spawn_ordinal: source.epic_spawn_ordinal,
        working_dir: Some(source.working_dir.clone()),
        provider: Some(provider),
        model: source.model.clone(),
        configured_context_window: source
            .resolved_context_budget
            .as_ref()
            .and_then(|budget| budget.capacity.configured_tokens),
        max_turns: None,
        system_prompt: None,
        resume_session_id: None,
        session_kind: Some(source.session_kind),
        project_id: source.project_id,
        rsi_session_id: None,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: Some(source.id),
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: source.workflow_id,
        workflow_id_override: source.workflow_id_override,
        max_retries: None,
        group_id: source.group_id,
        skip_project_model_default: false,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::SessionContinueResume,
        parent_id: source.parent_id,
        effort: source.effort.clone(),
        issue_identifier: source.issue_identifier.clone(),
        issue_url: source.issue_url.clone(),
        issue_tracker_id: source.issue_tracker_id.clone(),
        scheduled_job_id: None,
        model_invocation_owner: None,
        model_invocation_dedup_key: None,
        model_invocation_request_fingerprint: None,
        sandbox,
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: source.is_eval,
        skip_context_pipeline: source.is_eval,
        capability_class: source.capability_class,
        tags: source.tags.clone(),
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
    }
}

pub(super) async fn observe_cleanup_ownership(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    session_id: Uuid,
    root: &std::path::Path,
) -> OwnershipObservation {
    if active.read().await.iter().any(|(id, tracked)| {
        *id != session_id && tracked.session.sandbox_root.as_deref() == Some(root)
    }) {
        return OwnershipObservation::Shared;
    }
    if completed.read().await.iter().any(|(id, completed)| {
        *id != session_id && completed.session.sandbox_root.as_deref() == Some(root)
    }) {
        return OwnershipObservation::Shared;
    }

    let store = Arc::clone(store);
    let root = root.to_path_buf();
    let owners = tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.list_live_sandbox_owners()
    })
    .await;
    match owners {
        Ok(Ok(owners)) => {
            if owners
                .into_iter()
                .any(|(id, _, owner_root)| id != session_id && owner_root == root)
            {
                OwnershipObservation::Shared
            } else {
                OwnershipObservation::Exclusive
            }
        }
        _ => OwnershipObservation::Unreadable,
    }
}

pub(super) async fn classify_cleanup_candidate_runtime(
    candidate: CleanupCandidate,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
) -> CleanupDecision {
    let ownership_key = match &candidate {
        CleanupCandidate::Row(row) => Some((row.session_id, row.root.clone())),
        _ => None,
    };
    let (initial_ownership, final_ownership) = if let Some((session_id, root)) = ownership_key {
        let initial = observe_cleanup_ownership(active, completed, store, session_id, &root).await;
        tokio::task::yield_now().await;
        let final_observation =
            observe_cleanup_ownership(active, completed, store, session_id, &root).await;
        (initial, final_observation)
    } else {
        (
            OwnershipObservation::Exclusive,
            OwnershipObservation::Exclusive,
        )
    };

    let decision_candidate = candidate.clone();
    tokio::task::spawn_blocking(move || {
        cleanup::classify_candidate(&decision_candidate, initial_ownership, final_ownership)
    })
    .await
    .unwrap_or(CleanupDecision::Blocked(
        CleanupBlockedReason::UnreadableWorktree,
    ))
}

/// Re-read the durable row before deciding whether a lifecycle transition has
/// no destructive target. An in-memory `NoTarget` snapshot is not sufficient:
/// stale or partially restored maps must never bypass the D00 clamp.
pub(super) async fn classify_cleanup_session_runtime(
    session_id: Uuid,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
) -> CleanupDecision {
    let store_for_read = Arc::clone(store);
    let row = tokio::task::spawn_blocking(move || {
        let store = store_for_read.blocking_lock();
        store.get_session(session_id)
    })
    .await;
    let candidate = match row {
        Ok(Ok(Some(session))) => cleanup::candidate_from_session(&session),
        _ => CleanupCandidate::RowReadFailure {
            session_id,
            root: PathBuf::new(),
        },
    };
    classify_cleanup_candidate_runtime(candidate, active, completed, store).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InterruptOutcome {
    ActiveInterrupted,
    PendingRetryCancelled,
}

/// Interrupt a session by acting directly on the `active`/`completed` maps.
///
/// Extracted from [`SessionManager::interrupt_session`] so collaborators that
/// hold only the underlying `Arc`s (rather than a `&SessionManager`) — notably
/// the P2 [`super::agent_verbs::AgentControlHandle`] backing the native
/// `rsi_control halt` tool — drive the exact same interrupt path the
/// `InterruptSession`/`AgentHalt` RPC verbs use. Single source of truth.
pub(super) async fn interrupt_in_maps(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    session_id: Uuid,
) -> Result<InterruptOutcome> {
    if interrupt_active_in_maps(active, session_id).await? {
        return Ok(InterruptOutcome::ActiveInterrupted);
    }
    // Check for pending retry in completed map.
    {
        let mut completed = completed.write().await;
        if let Some(cs) = completed.get_mut(&session_id) {
            if let Some(cancel) = cs.retry_cancel.take() {
                let _ = cancel.send(());
                cs.retry_fired_at = None;
                exhaust_retry_budget(&mut cs.session);
                tracing::info!(session_id = %session_id, "Cancelled pending retry via interrupt");
                return Ok(InterruptOutcome::PendingRetryCancelled);
            }
        }
    }
    Err(DaemonError::SessionNotFound(session_id))
}

/// Attempt only the active-session half of interruption.  Lifecycle callers
/// use this before retry suppression so an active→completed transition that
/// races the call is revalidated by the durable pending-retry path instead of
/// consuming a newly armed timer without recording user intent.
pub(super) async fn interrupt_active_in_maps(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    session_id: Uuid,
) -> Result<bool> {
    loop {
        // A deferred successor has a per-incarnation effect gate. Snapshot it
        // under Active, then release Active before awaiting the gate. Both the
        // interrupt path and deferred launch path subsequently use gate ->
        // Active; neither ever awaits the gate while holding Active, so the
        // slow app-server launch cannot block or invert the global map lock.
        let (spawn_generation, gate) = {
            let mut active = active.write().await;
            let Some(tracked) = active.get_mut(&session_id) else {
                return Ok(false);
            };
            match tracked.deferred_successor_start_gate.as_ref() {
                Some(gate) => (tracked.spawn_generation, Arc::clone(gate)),
                None => {
                    tracked.interrupt_requested = true;
                    if let Some(ref process) = tracked.process {
                        process.interrupt()?;
                    }
                    let _ = tracked.stop_tx.try_send(());
                    return Ok(true);
                }
            }
        };
        let _effect_guard = Arc::clone(&gate).lock_owned().await;
        let mut active = active.write().await;
        let Some(tracked) = active.get_mut(&session_id) else {
            return Ok(false);
        };
        let exact_incarnation = tracked.spawn_generation == spawn_generation
            && tracked
                .deferred_successor_start_gate
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &gate));
        if !exact_incarnation {
            // The UUID was replaced while this caller waited. Retry against
            // the currently published incarnation rather than acknowledging
            // cancellation of a stale latch.
            drop(active);
            drop(_effect_guard);
            continue;
        }
        tracked.interrupt_requested = true;
        if let Some(ref process) = tracked.process {
            process.interrupt()?;
        }
        let _ = tracked.stop_tx.try_send(());
        return Ok(true);
    }
}

/// Commit durable user suppression while the completed-map entry is still the
/// sole retry owner, then consume that owner.  Holding the map write lock over
/// the small synchronous Store transition is intentional: a timer that has
/// not fired cannot disappear between validation and commit, and a timer that
/// already fired is not reported as a cancellation.  The Store transaction is
/// the truth point, so a caller that receives `Some` must report success even
/// if a concurrently queued retry observes the exhausted row immediately
/// afterwards.
pub(super) async fn suppress_pending_retry_in_maps(
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    session_id: Uuid,
) -> Result<Option<u8>> {
    let mut completed = completed.write().await;
    let Some(completed_session) = completed.get_mut(&session_id) else {
        return Ok(None);
    };
    let Some(max_retries) = completed_session.session.max_retries.filter(|value| {
        (completed_session.retry_cancel.is_some() || completed_session.retry_fired_at.is_some())
            && *value > 0
    }) else {
        return Ok(None);
    };

    store
        .lock()
        .await
        .suppress_c5_autofile_pending_and_exhaust_retry(session_id, max_retries)?;

    // The write guard makes this transition non-racy with timer consumption.
    if let Some(cancel) = completed_session.retry_cancel.take() {
        let _ = cancel.send(());
    }
    completed_session.retry_fired_at = None;
    exhaust_retry_budget(&mut completed_session.session);
    Ok(Some(max_retries))
}

impl SessionManager {
    /// Convert stored ConversationEvents into OpenAI-compatible messages array.
    /// Used to reconstruct conversation history for API provider resume/continue.
    pub(super) fn events_to_openai_messages(
        events: &[ConversationEvent],
    ) -> Vec<serde_json::Value> {
        let mut messages = Vec::new();
        for event in events {
            match event.event_type {
                EventType::Message => {
                    let role = match event.role {
                        Some(Role::User) => "user",
                        Some(Role::Assistant) => "assistant",
                        None => continue,
                        Some(_) => continue,
                    };
                    if !event.content.is_empty() {
                        messages.push(serde_json::json!({
                            "role": role,
                            "content": event.content,
                        }));
                    }
                }
                // Skip tool events, system events, and thinking
                _ => continue,
            }
        }
        messages
    }

    /// Convert stored ConversationEvents into Harness ChatMessages for context replay.
    pub(super) fn events_to_harness_messages(
        events: &[ConversationEvent],
    ) -> Vec<crate::session::harness::types::ChatMessage> {
        use crate::session::harness::types::{ChatMessage, MessageRole};
        let mut messages = Vec::new();
        for event in events {
            match event.event_type {
                EventType::Message => {
                    let role = match event.role {
                        Some(Role::User) => MessageRole::User,
                        Some(Role::Assistant) | Some(_) | None => MessageRole::Assistant,
                    };
                    if !event.content.is_empty() {
                        messages.push(ChatMessage {
                            role,
                            content: event.content.clone(),
                            tool_call_id: None,
                            tool_calls: Vec::new(),
                        });
                    }
                }
                EventType::ToolResult => {
                    messages.push(ChatMessage {
                        role: MessageRole::Tool,
                        content: event.content.clone(),
                        tool_call_id: event.tool_name.clone(),
                        tool_calls: Vec::new(),
                    });
                }
                _ => {} // Skip ToolUse, System, Thinking, Compressed
            }
        }
        messages
    }

    /// Fence the active-map to durable-terminal gap from orphan reconciliation.
    /// The guard is installed while the active write lock is held, before the
    /// session disappears from that map, and clears on every return path.
    /// Reconciliation takes the same locks in active -> settling order.
    /// This marker is process-local; startup reconciliation has no settling
    /// turn from an earlier daemon incarnation.
    ///
    /// The marker is intentionally distinct from `completed`: publishing a
    /// resumable session before its terminal status persists would create a
    /// different continuation race.
    ///
    /// Move session from active to completed and publish final status.
    pub(super) async fn finalize_session(
        session_id: Uuid,
        expected_generation: u64,
        decision: TerminalFinalizeDecision,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: Arc<EventBus>,
        store: Arc<tokio::sync::Mutex<crate::store::Store>>,
        persistence: PersistenceHandle,
        memory_handle: Option<crate::memory::worker::MemoryHandle>,
        runtime_config: Arc<crate::config::RuntimeConfig>,
    ) -> Option<TerminalFinalizeDecision> {
        let (
            session_data,
            should_auto_archive,
            c5_failure_cause,
            status_transition,
            effective_decision,
            _settlement_guard,
        ) = {
            let mut active_guard = active.write().await;
            if let Some(tracked) = active_guard.get(&session_id)
                && tracked.spawn_generation != expected_generation
            {
                tracing::warn!(
                    session_id = %session_id,
                    expected_generation,
                    actual_generation = tracked.spawn_generation,
                    "Skipping stale session finalizer for newer active incarnation"
                );
                return None;
            }
            let settlement_guard = active_guard
                .contains_key(&session_id)
                .then(|| crate::reconciliation::TerminalSettlementGuard::new(session_id));
            if let Some(mut tracked) = active_guard.remove(&session_id) {
                let old_status = tracked.session.status;
                let pending_archive = tracked.pending_archive;

                if let super::rotation_coordinator::RotationState::WritingHandoff {
                    handoff_filepath: Some(ref filepath),
                    ..
                } = *tracked.rotation.state()
                {
                    tracing::info!(
                        session_id = %session_id,
                        filepath = %filepath,
                        "Persisting handoff filepath to session"
                    );
                    tracked.session.handoff_filepath = Some(filepath.clone());
                }

                if let Some(ref artifact) = tracked.pipeline_artifact {
                    tracing::info!(
                        session_id = %session_id,
                        artifact = %artifact,
                        "Persisting pipeline artifact to session"
                    );
                    tracked.session.pipeline_artifact = Some(artifact.clone());
                }

                tracked.session.pending_question = tracked.pending_question.clone();

                // Status truth was decided by the monitor while this exact
                // generation still owned a drained, settled provider. The
                // finalizer applies that truth and never reclassifies silence.
                // Re-read all higher-priority lifecycle intent while the
                // exact generation is still protected by this removal lock.
                // Writers queued behind the monitor's evidence snapshot win
                // here instead of being lost to a stale Completed decision.
                let effective_decision = decision.with_current_lifecycle_intent(
                    tracked.pending_archive,
                    tracked.stall_interrupted,
                    tracked.interrupt_requested,
                    tracked.pending_question.is_some(),
                );
                if effective_decision != decision
                    && tracked.session.stop_reason.as_deref()
                        == Some("terminal_handoff_superseded_by_tool")
                {
                    // A newer lifecycle intent won after the handoff-order
                    // check. Its terminal reason must win as well.
                    tracked.session.stop_reason = None;
                }
                let final_status = effective_decision.status;
                let c5_failure_cause = effective_decision.c5_failure_cause;

                // Provider-specific terminal events set a more precise
                // stop_reason in the monitor. For every other failed terminal
                // path, persist the closed C5 cause so a failure never lands
                // as an unexplained NULL. The raw diagnostic remains in the
                // conversation events where operators can inspect it.
                if final_status == SessionStatus::Failed && tracked.session.stop_reason.is_none() {
                    let cause = c5_failure_cause
                        .map(crate::store::daemon_settings::AutofileCause::as_str)
                        .unwrap_or("unknown");
                    tracked.session.stop_reason = Some(format!("terminal_failure:{cause}"));
                }

                tracked.session.status = final_status;
                tracked.session.updated_at = chrono::Utc::now();

                // Close any still-open WaitingApproval interval at terminalization
                // (covers sessions that reach a terminal status while a question
                // is still pending, e.g. user interrupts during approval).
                if let Some(started) = tracked.approval_wait_start.take() {
                    let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    tracked.approval_wait_total_ms =
                        tracked.approval_wait_total_ms.saturating_add(elapsed_ms);
                }
                tracked.session.approval_wait_ms = Some(tracked.approval_wait_total_ms);

                // TD1: fold the final work interval (Running − approval-wait) into the
                // floor. Approval is fully closed just above, so open_approval == 0 here.
                // A session that never reached Running (work_run_start == None) is a
                // safe no-op that keeps its launch value.
                tracked.recompute_work_time();
                tracked.work_run_start = None;

                // Populate outcome telemetry. Canonical sources:
                // - `turn_count` = Claude stream-json `num_turns` (authoritative).
                //   Falls back to None when the provider did not emit a result
                //   event — prefer None over `events.len()` which counts every
                //   ToolUse/ToolResult and drifts from the real turn count.
                // - `retry_count` = snapshot of `retry_attempt` at finalize
                //   (Q2 resolution: this is a cold mirror of the hot retry
                //   counter; per-message / tool-call retries are out of scope).
                // - `test_passed` / `clippy_passed` = final-invocation-wins
                //   parse of `cargo test` / `cargo clippy` ToolResult events.
                //   None = no matching invocation; Some(bool) = measured outcome.
                tracked.session.turn_count = tracked.session.num_turns;
                tracked.session.retry_count =
                    Some(tracked.session.retry_attempt.unwrap_or(0) as u32);
                let probe = super::outcome::probe_outcomes(&tracked.events);
                tracked.session.test_passed = probe.test_passed;
                tracked.session.clippy_passed = probe.clippy_passed;

                if matches!(
                    tracked.session.session_kind,
                    SessionKind::TaskRabbit | SessionKind::Bug
                ) && final_status == SessionStatus::Completed
                {
                    let should_escalate = tracked
                        .events
                        .iter()
                        .rev()
                        .find(|e| e.role == Some(Role::Assistant))
                        .map(|e| e.content.trim().ends_with("[TASKRABBIT_ESCALATE]"))
                        .unwrap_or(false);

                    if should_escalate {
                        tracked.session.session_kind = SessionKind::Standard;
                    }
                }

                (
                    Some(CompletedSession {
                        session: tracked.session,
                        events: tracked.events,
                        turn_metrics: tracked.turn_metrics,
                        retry_cancel: None,
                        retry_fired_at: None,
                        superseded_by_retry: None,
                        events_hydrated: true,
                    }),
                    pending_archive,
                    c5_failure_cause,
                    Some((old_status, final_status)),
                    Some(effective_decision),
                    settlement_guard,
                )
            } else {
                (None, false, None, None, None, None)
            }
        };
        #[cfg(test)]
        let pause = finalizer_pauses()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session_id);
        #[cfg(test)]
        if let Some((reached, release)) = pause {
            let _ = reached.send(());
            let _ = release.await;
        }
        let mut durable_terminal = false;
        let mut finalizer_persistence_ordered = false;

        if let Some(completed_session) = session_data {
            let sid = completed_session.session.id;
            let status = completed_session.session.status;
            let session_kind = completed_session.session.session_kind;
            let completion = InvocationCompletion {
                input_tokens: completed_session.session.total_input_tokens,
                output_tokens: completed_session.session.total_output_tokens,
                cache_creation_tokens: completed_session.session.total_cache_creation_tokens,
                cache_read_tokens: completed_session.session.total_cache_read_tokens,
                wall_time_ms: completed_session.session.work_time_ms,
                estimated_cost_usd: completed_session.session.cost_usd,
                error_class: match status {
                    SessionStatus::Completed | SessionStatus::Archived => None,
                    SessionStatus::Interrupted => Some("interrupted".to_string()),
                    SessionStatus::WaitingApproval => Some("waiting_approval".to_string()),
                    SessionStatus::Failed => Some("failed".to_string()),
                    SessionStatus::Starting => Some("starting".to_string()),
                    SessionStatus::Running => Some("running".to_string()),
                    SessionStatus::Deleted => Some("deleted".to_string()),
                    _ => Some("unknown".to_string()),
                },
                confidence: Some(invocation_confidence_from_session(
                    completed_session.session.context_usage_confidence,
                )),
                ..InvocationCompletion::default()
            };
            let status_persisted = if status == SessionStatus::Failed {
                persistence
                    .update_failed_and_stage_autofile(
                        sid,
                        c5_failure_cause.unwrap_or(AutofileCause::OtherTerminalFailure),
                    )
                    .await
            } else {
                persistence.update_status(sid, status).await
            };
            match status_persisted {
                Ok(()) => {
                    durable_terminal = true;
                }
                Err(e) => {
                    tracing::error!(error = %e, session_id = %sid, "Failed to persist final session status");
                }
            }
            if let Err(e) = persistence.update_session_kind(sid, session_kind).await {
                tracing::warn!(error = %e, session_id = %sid, "Failed to persist session kind");
            }
            let session_clone = completed_session.session.clone();
            let metadata_persisted = match persistence
                .update_final_session_metadata(session_clone.clone())
                .await
            {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(error = %e, session_id = %sid, "Failed to persist final session metadata");
                    event_bus.publish(DaemonEvent::SystemMessage {
                        level: "error".to_string(),
                        message: format!(
                            "Final session metadata persistence failed for {sid}: {e}"
                        ),
                    });
                    false
                }
            };
            match persistence.barrier_for_final_session_metadata(sid).await {
                Ok(()) => finalizer_persistence_ordered = metadata_persisted,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        session_id = %sid,
                        "Final session persistence barrier failed; suppressing post-finalization rotation"
                    );
                }
            }
            // The provider has stopped. A failed metadata write must never
            // report a successful invocation, but it must release its running
            // admission with a durable failure classification.
            let completion = if finalizer_persistence_ordered {
                completion
            } else {
                InvocationCompletion {
                    error_class: Some("session_metadata_persistence_failed".to_string()),
                    ..completion
                }
            };
            let store_ref = store.clone();
            match tokio::task::spawn_blocking(move || {
                let store = store_ref.blocking_lock();
                store.session_model_invocation_id(sid)
            })
            .await
            {
                Ok(Ok(Some(invocation_id))) => {
                    if let Err(e) =
                        complete_invocation_by_id(&store, invocation_id, completion, &event_bus)
                            .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %sid,
                            invocation_id = %invocation_id,
                            "Failed to settle model invocation on finalization"
                        );
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, session_id = %sid, "Failed to load session model invocation id");
                }
                Err(e) => {
                    tracing::warn!(error = %e, session_id = %sid, "Failed to join session model invocation lookup");
                }
            }

            // Advance workflow to ImplementComplete when an implement session completes successfully
            if status == SessionStatus::Completed {
                if let Some(wf_id) = session_clone.workflow_id {
                    let query_clean = session_clone.query.trim_start_matches('/');
                    if query_clean.starts_with("implement") {
                        if let Err(e) = persistence
                            .update_workflow_stage(wf_id, WorkflowStage::ImplementComplete, None)
                            .await
                        {
                            tracing::warn!(error = %e, workflow_id = %wf_id, "Failed to advance workflow to ImplementComplete");
                        } else {
                            tracing::info!(workflow_id = %wf_id, "Advanced workflow to ImplementComplete on session completion");
                        }
                    }
                }
            }

            if super::title::should_enqueue_title_refinement(
                session_kind,
                super::title::is_role_titled_epic_identity(&completed_session.session),
                completed_session.events.len(),
                completed_session.session.title.is_none(),
            ) {
                let context_events: Vec<&ConversationEvent> = completed_session
                    .events
                    .iter()
                    .filter(|e| e.event_type == EventType::Message)
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let title_forced_role = super::title::forced_role_for_session(
                    &active,
                    &completed,
                    &store,
                    session_id,
                    completed_session.session.parent_id,
                )
                .await;
                let title_prompt = super::title::build_title_and_description_prompt(
                    &completed_session.session.query,
                    Some(&context_events),
                    title_forced_role,
                );
                let persistence_for_title = persistence.clone();
                let completed_for_title = completed.clone();
                let store_for_title = store.clone();
                let ollama_model = runtime_config.title_model_local.read().clone();
                let fallback_model = runtime_config.title_model_fallback.read().clone();
                let title_provider = *runtime_config.title_model_provider.read();
                let title_base_url = runtime_config.title_model_base_url.read().clone();
                let title_api_key = runtime_config.title_model_api_key.read().clone();
                let title_event_bus = Arc::clone(&event_bus);
                tokio::spawn(async move {
                    match super::title::generate_title_and_description_from_prompt(
                        &store_for_title,
                        &title_event_bus,
                        session_id,
                        &title_prompt,
                        title_forced_role,
                        &ollama_model,
                        &fallback_model,
                        title_provider,
                        title_base_url,
                        title_api_key,
                    )
                    .await
                    {
                        Ok(result) => {
                            // Refinement is generated enrichment: it may fill an
                            // absent title but must never replace an explicit one.
                            // The durable row is the arbiter, so advance the
                            // projection only when the store accepted the fill.
                            let filled = match persistence_for_title
                                .fill_session_title_if_absent(session_id, result.title.clone())
                                .await
                            {
                                Ok(filled) => filled,
                                Err(e) => {
                                    tracing::warn!(error = %e, session_id = %session_id, "Failed to persist refined title");
                                    false
                                }
                            };
                            if filled {
                                if let Some(cs) =
                                    completed_for_title.write().await.get_mut(&session_id)
                                {
                                    cs.session.title = Some(result.title.clone());
                                }
                            }
                            if !result.description.is_empty() {
                                if let Some(cs) =
                                    completed_for_title.write().await.get_mut(&session_id)
                                {
                                    cs.session.description = Some(result.description.clone());
                                }
                                if let Err(e) = persistence_for_title
                                    .update_session_description(session_id, result.description)
                                    .await
                                {
                                    tracing::warn!(error = %e, session_id = %session_id, "Failed to persist refined description");
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, session_id = %session_id, "Title+description refinement failed, keeping initial values");
                        }
                    }
                });
            }

            // Capture data for observation extraction before moving completed_session
            let obs_data = if memory_handle.is_some()
                && completed_session.events.len() >= 4
                && matches!(
                    status,
                    SessionStatus::Completed | SessionStatus::Interrupted
                ) {
                Some((
                    completed_session.session.project_id,
                    completed_session.session.query.clone(),
                    completed_session.events.clone(),
                ))
            } else {
                None
            };

            completed
                .write()
                .await
                .insert(session_id, completed_session);

            // A continuation waiting on this event removes the resumable
            // source from `completed` immediately.  Publishing before the
            // insertion above woke it into a gap where the terminal session
            // was durable but not yet resumable, so its first follow-up failed
            // and required a second submission.  Keep the durable-status
            // acknowledgement as the first gate, then publish only once both
            // durable and in-memory terminal state are ready for consumers.
            if durable_terminal {
                if let Some((old_status, new_status)) = status_transition {
                    event_bus.publish(DaemonEvent::SessionStatusChanged {
                        session_id,
                        old_status,
                        new_status,
                    });
                }
            }

            // Trigger observation extraction in background (after insert so session is queryable)
            if let (Some((obs_project_id, obs_query, obs_events)), Some(handle)) =
                (obs_data, &memory_handle)
            {
                let obs_handle = handle.clone();
                tokio::spawn(async move {
                    if let Err(e) = obs_handle
                        .extract_observations(session_id, obs_project_id, obs_query, obs_events)
                        .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Observation extraction trigger failed"
                        );
                    }
                });
            }
        }

        if should_auto_archive {
            // Archive is a terminal metadata transition. It deliberately
            // retains sandbox files, refs, and custody, so D00's physical
            // cleanup/reclamation proof must not gate it.
            tracing::info!(session_id = %session_id, "Auto-archiving session");
            // Capture the exact terminal manager obligation before clearing
            // the Epic pointer. Once archive commits, the retained notice and
            // its deterministic transport—not the live lead lookup—own retry.
            let terminal = completed
                .read()
                .await
                .get(&session_id)
                .map(|completed| completed.session.clone());
            let notice_ready = if let Some(terminal) = terminal {
                match store
                    .lock()
                    .await
                    .record_manager_terminal_notice_before_archive(&terminal)
                {
                    Ok(job) => {
                        if let Some(job_id) = job {
                            event_bus.publish(DaemonEvent::ManagerNoticeQueued { job_id });
                        }
                        true
                    }
                    Err(error) => {
                        tracing::error!(
                            error = %error,
                            session_id = %session_id,
                            "Skipping auto-archive because terminal manager notice capture failed"
                        );
                        false
                    }
                }
            } else {
                tracing::error!(
                    session_id = %session_id,
                    "Skipping auto-archive because completed session state is unavailable"
                );
                false
            };
            let can_archive = if notice_ready {
                // Never wait on a continuation's guard from the finalizer: a
                // contended gate skips this auto-archive (pending_archive
                // stays durable and restore completes it).
                match Self::clear_lead_pointers_to_runtime(
                    session_id,
                    &active,
                    &completed,
                    &store,
                    &event_bus,
                    super::hierarchy_ops::LeadGuardMode::TryAcquire,
                )
                .await
                {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            session_id = %session_id,
                            "Skipping auto-archive because Epic lead pointer cleanup failed"
                        );
                        false
                    }
                }
            } else {
                false
            };
            if can_archive {
                if let Err(e) = persistence.archive_and_resolve_autofile(session_id).await {
                    tracing::error!(error = %e, session_id = %session_id, "Failed to persist auto-archive status");
                } else {
                    completed.write().await.remove(&session_id);
                    event_bus.publish(DaemonEvent::SessionArchived {
                        session_id,
                        projection_id: None,
                    });
                }
            }
        }

        if let Some(handle) = memory_handle {
            let reason = format!("session_completed:{}", session_id);
            tokio::spawn(async move {
                if let Err(e) = handle.sync_now(false, &reason).await {
                    tracing::warn!(error = %e, session_id = %session_id, "Memory sync after session completion failed");
                }
            });
        }
        if durable_terminal && finalizer_persistence_ordered {
            effective_decision
        } else {
            None
        }
    }

    /// Answer a pending question from the session, resuming it.
    pub async fn answer_question(&self, session_id: Uuid, response_text: String) -> Result<()> {
        let is_waiting = {
            let active = self.active.read().await;
            if let Some(tracked) = active.get(&session_id) {
                tracked.session.status == SessionStatus::WaitingApproval
            } else {
                false
            }
        };

        if !is_waiting {
            // Check if it's in completed as a fallback, although WaitingApproval sessions
            // are moved to completed map in finalize_session, they still keep their status.
            let in_completed = {
                let completed = self.completed.read().await;
                completed.get(&session_id).map(|s| s.session.status)
                    == Some(SessionStatus::WaitingApproval)
            };
            if !in_completed {
                return Err(DaemonError::SessionNotFound(session_id));
            }
        }

        // Clear the pending question in the DB and memory immediately (best effort for UI responsiveness)
        {
            if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
                tracked.pending_question = None;
                tracked.session.pending_question = None;
                // Close the open WaitingApproval interval and fold its duration
                // into the running total. `saturating_add` defends against an
                // improbable u64 overflow (would require a session wait
                // measured in hundreds of millions of years).
                if let Some(started) = tracked.approval_wait_start.take() {
                    let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    tracked.approval_wait_total_ms =
                        tracked.approval_wait_total_ms.saturating_add(elapsed_ms);
                }
            }
        }
        {
            if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
                completed.session.pending_question = None;
            }
        }
        if let Err(e) = self
            .persistence
            .update_pending_question_json(session_id, None)
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "Failed to clear pending question"
            );
        }

        let provider = {
            let active = self.active.read().await;
            if let Some(tracked) = active.get(&session_id) {
                Some(tracked.session.provider)
            } else {
                let completed = self.completed.read().await;
                completed.get(&session_id).map(|s| s.session.provider)
            }
        };

        let provider = provider.ok_or_else(|| DaemonError::SessionNotFound(session_id))?;
        let answer = super::question::encode_answer(provider, &response_text);
        self.continue_session(session_id, answer).await
    }

    /// How long [`Self::continue_session`] waits for an already-active session
    /// to reach a terminal status after it SIGINTs it.
    ///
    /// This MUST comfortably exceed [`super::reaper::TEARDOWN_TERMINAL_WORST_CASE`].
    /// It previously did not: a hardcoded 10s sat barely above the ~8.4s
    /// two-attempt settlement worst case, so a *correct but slow* teardown
    /// intermittently tripped the deadline and surfaced to the user as a failed
    /// continue (worse on slower/loaded machines). The static assertion below
    /// keeps that margin honest — widening a teardown grace without revisiting
    /// this value is a compile error, not a silent regression.
    pub(super) const CONTINUE_INTERRUPT_WAIT: std::time::Duration =
        std::time::Duration::from_secs(20);

    /// Continue a completed/interrupted/failed session with a new query.
    pub async fn continue_session(&self, session_id: Uuid, query: String) -> Result<()> {
        self.continue_session_with_delivery(
            session_id,
            query,
            ContinuationIntent::ExistingAuthority,
        )
        .await
        .map(|_| ())
    }

    pub(super) async fn continue_agent_child(
        &self,
        session_id: Uuid,
        query: String,
        row: RelaunchIntentRow,
        observed: AgentContinuationCursorV1,
        fence: ContinuationFenceV1,
        manager_scope: Option<crate::store::harness_manager::ManagerSessionScope>,
    ) -> Result<Option<AgentContinueChildResultV1>> {
        match Box::pin(self.continue_session_fenced(
            session_id,
            query,
            ContinuationIntent::AgentChild(FreshRelaunchIntent {
                row,
                observed,
                manager_scope,
            }),
            Some(fence),
        ))
        .await?
        {
            ContinueSessionOutcome::AgentFresh(receipt) => Ok(Some(receipt)),
            ContinueSessionOutcome::Started => Ok(None),
            _ => Err(DaemonError::Store(
                "unexpected agent child continuation outcome".into(),
            )),
        }
    }

    pub(super) async fn recover_agent_child_relaunch(
        &self,
        row: &RelaunchIntentRow,
    ) -> Result<RelaunchIntentRow> {
        let runtime_active = self.active.read().await.contains_key(&row.tip_session_id);
        if !runtime_active {
            let tip = row.tip_session_id;
            tokio::task::spawn_blocking(move || super::reaper::reap_orphans_for_session(tip))
                .await
                .map_err(|error| {
                    DaemonError::Process(format!("child relaunch orphan reap join failed: {error}"))
                })??;
        }
        let store = self.store.lock().await;
        let task_source = super::rotation::resolve_rotation_task_query(
            &store,
            &store
                .get_session(row.tip_session_id)?
                .ok_or(DaemonError::SessionNotFound(row.tip_session_id))?,
        )?
        .map_or(row.tip_session_id, |(source, _)| source);
        store.recover_child_relaunch_intent(row.request_id, task_source, runtime_active)
    }

    /// Scheduled wake delivery bound to its exact job rows. Refuses when any
    /// row was disabled or retired after the scheduler captured it, and when
    /// the K2 continuation fence captured at dispatch no longer holds.
    pub(crate) async fn continue_scheduled_wake(
        &self,
        session_id: Uuid,
        query: String,
        job_ids: Vec<Uuid>,
        fence: ContinuationFenceV1,
    ) -> Result<()> {
        self.continue_session_fenced(
            session_id,
            query,
            ContinuationIntent::ScheduledWake { job_ids },
            Some(fence),
        )
        .await
        .map(|_| ())
    }

    /// An automated or agent continuation with no job rows (operator manual
    /// trigger, stall nudge, `AgentContinueChild`): fenced, never unfenced.
    pub(crate) async fn continue_fenced(
        &self,
        session_id: Uuid,
        query: String,
        fence: ContinuationFenceV1,
    ) -> Result<()> {
        self.continue_session_fenced(
            session_id,
            query,
            ContinuationIntent::ExistingAuthority,
            Some(fence),
        )
        .await
        .map(|_| ())
    }

    /// Capture the K2 continuation fence for `origin` in one store-lock hold:
    /// the published lineage tip (RPC-1 C2) and its Epic lead generation.
    pub(crate) async fn capture_continuation_fence(
        &self,
        origin: Uuid,
        authority: ContinuationAuthorityV1,
    ) -> Result<ContinuationFenceV1> {
        self.store
            .lock()
            .await
            .capture_continuation_fence(origin, authority)?
            .ok_or(DaemonError::SessionNotFound(origin))
    }

    /// Capture for an exact target: the target must itself be the published
    /// tip, else the continuation is refused `continuation_tip_changed`.
    pub(crate) async fn capture_exact_continuation_fence(
        &self,
        target: Uuid,
        authority: ContinuationAuthorityV1,
    ) -> Result<ContinuationFenceV1> {
        let fence = self.capture_continuation_fence(target, authority).await?;
        if fence.tip != target {
            return Err(DaemonError::InvalidParam(CONTINUATION_TIP_CHANGED.into()));
        }
        Ok(fence)
    }

    /// Stall-nudge continuation of the exact stalled session (K2 path table).
    ///
    /// # Errors
    /// A typed continuation-fence refusal (the nudge is dropped) or any
    /// continuation failure.
    pub async fn continue_stall_nudge(&self, session_id: Uuid, query: String) -> Result<()> {
        let fence = self
            .capture_exact_continuation_fence(session_id, ContinuationAuthorityV1::Automated)
            .await?;
        self.continue_fenced(session_id, query, fence).await
    }

    /// Under the tip's spawn guard: the first check of a fenced continuation.
    /// Order and codes follow the K2 design table.
    async fn check_continuation_fence(
        &self,
        _spawn_guard: &super::spawn_single_flight::SpawnGuard,
        target: Uuid,
        fence: &ContinuationFenceV1,
    ) -> Result<()> {
        if fence.tip != target {
            return Err(DaemonError::InvalidParam(CONTINUATION_TIP_CHANGED.into()));
        }
        self.store
            .lock()
            .await
            .check_continuation_fence_durable(fence)?;
        let tip_active = self.active.read().await.contains_key(&target);
        // A manager recovery keeps its own typed busy refusal below.
        if tip_active && fence.authority == ContinuationAuthorityV1::Automated {
            return Err(DaemonError::InvalidParam(format!(
                "{CONTINUATION_TARGET_BUSY}:{target}"
            )));
        }
        self.store
            .lock()
            .await
            .check_continuation_fence_owner(fence, tip_active)
    }

    /// Operator RPC entry point. Internal/agent continuations must use their
    /// existing intent and cannot erase durable operator pause ownership.
    pub async fn continue_session_operator(&self, session_id: Uuid, query: String) -> Result<()> {
        self.continue_session_with_delivery(session_id, query, ContinuationIntent::Operator)
            .await
            .map(|_| ())
    }

    pub(super) async fn continue_manager_action(
        &self,
        claim: crate::store::manager_actions::ManagerActionClaimV2,
        query: String,
    ) -> Result<()> {
        let target = claim
            .operation
            .context
            .target_session_id
            .ok_or_else(|| DaemonError::InvalidParam("manager_v2_lead_unavailable".into()))?;
        // K2 exception: a manager recovery waives only the retirement
        // witness. The exact target must still be the published tip, its
        // lead fence is rechecked under the guard, and it must be idle.
        let fence = self
            .capture_exact_continuation_fence(
                target,
                ContinuationAuthorityV1::ManagerRecovery {
                    operation_id: claim.operation.receipt.operation_id,
                },
            )
            .await?;
        self.continue_session_fenced(
            target,
            query,
            ContinuationIntent::ManagerAction(Box::new(claim)),
            Some(fence),
        )
        .await
        .map(|_| ())
    }

    pub(super) async fn continue_manager_decision(
        &self,
        delivery: crate::store::manager_coordinator::ManagerDecisionDeliveryV2,
    ) -> Result<()> {
        let target = super::manager_coordinator::decision_session(&delivery)?;
        let provider = self
            .store
            .lock()
            .await
            .get_session(target)?
            .ok_or_else(|| DaemonError::InvalidParam("manager_v2_decision_target_changed".into()))?
            .provider;
        if provider != SessionProvider::Claude {
            return Err(DaemonError::InvalidParam(
                "manager_v2_decision_provider_unsupported".into(),
            ));
        }
        let query = super::question::encode_answer(provider, &delivery.answer);
        self.continue_session_with_delivery(
            target,
            query,
            ContinuationIntent::ManagerDecision(Box::new(delivery)),
        )
        .await
        .map(|_| ())
    }

    /// #669: resume the exact claimed manager tip in place. No lineage chase:
    /// a rotated seat is refused at the gate, never followed to a successor.
    pub(super) async fn continue_manager_seat(
        &self,
        claim: crate::store::manager_intent::manager_seat::ManagerSeatClaimV1,
        query: String,
    ) -> Result<()> {
        let target = claim.tip_session_id;
        Box::pin(self.continue_session_with_delivery(
            target,
            query,
            ContinuationIntent::ManagerSeat(Box::new(claim)),
        ))
        .await
        .map(|_| ())
    }

    /// Deliver a daemon-bound manager inbox notice only to its authorized idle recipient.
    /// Recurring watches retain custody when this returns a manager notice refusal.
    pub async fn resume_manager_notice(
        &self,
        target: Uuid,
        query: String,
        job_ids: Vec<Uuid>,
    ) -> Result<Uuid> {
        let fence = self
            .capture_exact_continuation_fence(target, ContinuationAuthorityV1::Automated)
            .await?;
        self.continue_session_fenced(
            target,
            query,
            ContinuationIntent::ManagerNotice { job_ids },
            Some(fence),
        )
        .await
        .map(|_| target)
    }

    async fn check_manager_notice_resume(
        &self,
        _spawn_guard: &super::spawn_single_flight::SpawnGuard,
        target: Uuid,
        job_ids: &[Uuid],
    ) -> Result<()> {
        if job_ids.is_empty()
            || job_ids.len() > super::agent_verbs::MAX_TERMINAL_WATCHES_PER_MASTER
            || job_ids
                .iter()
                .enumerate()
                .any(|(index, id)| id.is_nil() || job_ids[..index].contains(id))
        {
            return Err(manager_notice_scope_revoked());
        }
        // Map membership is runtime custody even when the status has already
        // become terminal. The monitor must finish removing the active owner.
        if self.active.read().await.contains_key(&target) {
            return Err(manager_notice_deferred());
        }
        let completed = self.completed.read().await;
        if let Some(cached) = completed.get(&target) {
            if cached.session.status != SessionStatus::Completed
                || cached.session.pending_question.is_some()
                || cached.session.pending_archive
                || cached.retry_cancel.is_some()
                || cached.retry_fired_at.is_some()
                || cached.superseded_by_retry.is_some()
            {
                return Err(manager_notice_deferred());
            }
            if cached.session.provider == SessionProvider::CodexAppServer {
                return Err(manager_notice_scope_revoked());
            }
        }
        drop(completed);

        let store = self.store.lock().await;
        let persisted = store
            .get_session(target)?
            .ok_or_else(manager_notice_scope_revoked)?;
        if persisted.status != SessionStatus::Completed || persisted.pending_archive {
            return Err(manager_notice_deferred());
        }
        // Read the raw question marker too: Session hydration deliberately
        // tolerates malformed JSON, which must not erase a human gate here.
        let held: bool = store.conn.query_row(
            "SELECT pending_question_json IS NOT NULL
                 OR EXISTS(SELECT 1 FROM approvals WHERE session_id=?1 AND status='Pending')
                 OR EXISTS(SELECT 1 FROM daemon_settings WHERE key=?2)
                 OR EXISTS(SELECT 1 FROM scheduled_jobs
                           WHERE enabled=1 AND wake_mode='resume' AND wake_session_id=?1 AND id<>?3
                             AND CASE WHEN json_valid(schedule_json)
                                 THEN json_extract(schedule_json,'$.recurrence.type')='Once'
                                 ELSE 1 END)
                 OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents
                           WHERE state='open' AND controller_session_id=?1)
                 OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents AS incident
                           JOIN model_invocations AS invocation
                             ON invocation.id=incident.last_capacity_model_invocation_id
                           WHERE incident.state='open' AND invocation.session_id=?1)
             FROM sessions WHERE id=?1",
            rusqlite::params![
                target.to_string(),
                crate::store::daemon_settings::c5_autofile_pending_key(target),
                super::harness::tools::schedule_wake::deterministic_program_guard_job_id(target)
                    .to_string(),
            ],
            |row| row.get(0),
        )?;
        if held {
            return Err(manager_notice_deferred());
        }
        check_manager_notice_program_gate(&store, target)?;
        // AppServer continuation allocates a fresh principal. This lane cannot
        // authorize that replacement or claim the requested UUID received it.
        if persisted.provider == SessionProvider::CodexAppServer
            || !rsi_common::is_leaf_kind(persisted.session_kind)
        {
            return Err(manager_notice_scope_revoked());
        }
        for &job_id in job_ids {
            if !store.harness_manager_wake_authorized(job_id, target)? {
                return Err(manager_notice_scope_revoked());
            }
        }
        Ok(())
    }

    pub(super) async fn continue_capacity_scheduled(
        &self,
        session_id: Uuid,
        query: String,
        wake_job_id: Uuid,
        due_slot: chrono::DateTime<chrono::Utc>,
        fence: ContinuationFenceV1,
    ) -> Result<Uuid> {
        match self
            .continue_session_fenced(
                session_id,
                query,
                ContinuationIntent::Capacity(CapacityDeliveryContext {
                    wake_job_id,
                    due_slot,
                }),
                Some(fence),
            )
            .await?
        {
            ContinueSessionOutcome::CapacityLaunchConfirmed {
                invocation_id,
                recovered_admission,
            } => {
                tracing::info!(
                    session_id = %session_id,
                    invocation_id = %invocation_id,
                    recovered_admission,
                    "capacity provider launch durably confirmed"
                );
                Ok(session_id)
            }
            ContinueSessionOutcome::CapacityAlreadyLaunchConfirmed(invocation_id) => {
                tracing::info!(
                    session_id = %session_id,
                    invocation_id = %invocation_id,
                    "capacity due-slot launch confirmation replayed without a second provider"
                );
                Ok(session_id)
            }
            ContinueSessionOutcome::Started | ContinueSessionOutcome::AgentFresh(_) => Err(
                DaemonError::Store("capacity continuation returned generic start outcome".into()),
            ),
        }
    }

    async fn continue_session_with_delivery(
        &self,
        session_id: Uuid,
        query: String,
        intent: ContinuationIntent,
    ) -> Result<ContinueSessionOutcome> {
        self.continue_session_fenced(session_id, query, intent, None)
            .await
    }

    /// `fence` is `Some` for every automated or agent continuation (K2); the
    /// operator-authorized exact-id paths (operator `ContinueSession`,
    /// `answer_question`, decisions) pass `None` and still serialize on the
    /// spawn guard. The large continuation future is boxed once here.
    async fn continue_session_fenced(
        &self,
        session_id: Uuid,
        query: String,
        intent: ContinuationIntent,
        fence: Option<ContinuationFenceV1>,
    ) -> Result<ContinueSessionOutcome> {
        Box::pin(self.continue_session_fenced_inner(session_id, query, intent, fence)).await
    }

    #[allow(clippy::too_many_lines)]
    async fn continue_session_fenced_inner(
        &self,
        session_id: Uuid,
        mut query: String,
        intent: ContinuationIntent,
        fence: Option<ContinuationFenceV1>,
    ) -> Result<ContinueSessionOutcome> {
        let operator_intent = matches!(&intent, ContinuationIntent::Operator);
        let manager_seat = match &intent {
            ContinuationIntent::ManagerSeat(claim) => Some(claim.as_ref().clone()),
            _ => None,
        };
        let mut scheduled_wake_jobs = None;
        let (
            capacity_delivery,
            manager_notice_jobs,
            manager_action,
            mut manager_decision,
            agent_child,
        ) = match intent {
            ContinuationIntent::Operator
            | ContinuationIntent::ExistingAuthority
            | ContinuationIntent::ManagerSeat(_) => (None, None, None, None, None),
            ContinuationIntent::AgentChild(child) => (None, None, None, None, Some(child)),
            ContinuationIntent::ScheduledWake { job_ids } => {
                scheduled_wake_jobs = Some(job_ids);
                (None, None, None, None, None)
            }
            ContinuationIntent::Capacity(delivery) => (Some(delivery), None, None, None, None),
            ContinuationIntent::ManagerNotice { job_ids } => {
                (None, Some(job_ids), None, None, None)
            }
            ContinuationIntent::ManagerAction(claim) => (None, None, Some(claim), None, None),
            ContinuationIntent::ManagerDecision(delivery) => {
                (None, None, None, Some(delivery), None)
            }
        };
        // Single-flight spawn guard: held across this fn's check -> launch ->
        // active.insert span. A racing continue/resume for the same id blocks
        // here and, on acquiring, proceeds through the ordinary
        // interrupt-then-respawn path below.
        //
        // This deliberately does NOT adopt-and-return on contention. `query` is
        // a message that MUST reach a provider turn, and the adopt shortcut had
        // no way to deliver it: it returned `Ok(())` before the query was
        // persisted as a conversation event or handed to any child, so the
        // caller's text was silently discarded while the RPC reported success.
        // The two signals it keyed on do not mean what the shortcut assumed —
        // `contended()` only means "someone held (or was queued on) the
        // per-session lock", and `adopt_if_live` only means "a live child exists
        // in `active`", which is the normal state of EVERY running session. So
        // the shortcut fired on ordinary traffic, not just on a genuine twin
        // spawn, and when the guard holder never spawned (e.g. another continue
        // that hit the `CONTINUE_INTERRUPT_WAIT` deadline) it adopted a stale
        // wedged child and nothing ran at all. See issue #13.
        //
        // Spawn dedup is preserved by the guard itself, without dropping
        // messages: the fall-through below interrupts the live child and waits
        // for it to reach a terminal status before spawning exactly one
        // replacement, so a racing pair still resolves to a single tracked
        // child (the `dual_resume_spawns_single_child` invariant).
        #[cfg(test)]
        pause_continuation_before_guard_for_test(session_id).await;
        let spawn_guard = super::spawn_single_flight::acquire_spawn_guard(session_id).await;
        if let Some(child) = agent_child.as_ref() {
            let existing = self
                .store
                .lock()
                .await
                .child_relaunch_intent_by_key(&child.row.key_digest)?;
            if let Some(row) = existing {
                if row.request_fingerprint != child.row.request_fingerprint {
                    return Err(crate::error::agent_continue_error(
                        AgentContinueErrorCodeV1::IdempotencyConflict,
                        None,
                        None,
                    ));
                }
                let row = if row.state == RelaunchState::Intent {
                    self.recover_agent_child_relaunch(&row).await?
                } else {
                    row
                };
                return child_relaunch_row_outcome(&row);
            }
        }
        if let Some(job_ids) = scheduled_wake_jobs.as_deref() {
            // A due-list snapshot may predate a manager retirement that
            // committed under this same guard; never resume on stale rows.
            // Checked first so a scheduled wake keeps its exact-row codes
            // (`scheduled_wake_target_retired` alias, K2 design).
            self.store
                .lock()
                .await
                .check_scheduled_wake_owner(session_id, job_ids)?;
        }
        // K2: the fence check runs under the guard before any effect. A lead
        // or tip mutation either committed before this point (refused here)
        // or waits for this guard until the provider is installed below.
        if let Some(fence) = fence.as_ref() {
            self.check_continuation_fence(&spawn_guard, session_id, fence)
                .await?;
        }
        // Replays return above under this guard. Audit only a new authorized
        // attempt, before its effect, so an exact retry cannot publish a
        // second manager watch or requested event.
        if let Some(scope) = agent_child
            .as_ref()
            .and_then(|child| child.manager_scope.as_ref())
        {
            self.store.lock().await.audit_manager_session_control(
                scope,
                "AgentContinueChild",
                "requested",
            )?;
        }
        #[cfg(test)]
        pause_continuation_seam_for_test(ContinuationPauseSeam::AfterFenceCheck, session_id).await;
        let cwd_admission_guard =
            super::spawn_single_flight::acquire_provider_cwd_admission().await;
        if let Some(delivery) = manager_decision.as_ref() {
            self.check_manager_decision_runtime(delivery).await?;
        }
        if operator_intent {
            self.store
                .lock()
                .await
                .clear_manager_pause_for_operator(session_id)?;
        }
        if let Some(claim) = manager_action.as_ref() {
            self.check_manager_action_runtime(claim, false).await?;
            if self.active.read().await.contains_key(&session_id) {
                return Err(DaemonError::InvalidParam(
                    "manager_v2_lead_not_resumable".into(),
                ));
            }
            let store = self.store.lock().await;
            let session = store
                .get_session(session_id)?
                .ok_or_else(|| DaemonError::InvalidParam("manager_v2_lead_unavailable".into()))?;
            check_manager_resume_target(&session)?;
        }
        if let Some(job_ids) = manager_notice_jobs.as_deref() {
            self.check_manager_notice_resume(&spawn_guard, session_id, job_ids)
                .await?;
        }
        if let Some(claim) = manager_seat.as_ref() {
            // Under the spawn guard, before any provider effect: never
            // interrupt a live tip, and re-check the claim, the exact tip and
            // every recovery bound (#669 stale-claim guard).
            if claim.tip_session_id != session_id
                || self.active.read().await.contains_key(&session_id)
            {
                return Err(DaemonError::InvalidParam(
                    crate::store::manager_intent::manager_seat::SEAT_BUSY_OUTCOME.into(),
                ));
            }
            self.store.lock().await.manager_seat_effect_gate(
                claim,
                self.runtime_config
                    .retry_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
            )?;
        }
        if spawn_guard.contended() {
            tracing::info!(
                session_id = %session_id,
                "continue_session serialized behind an in-flight spawn for the same session; \
                 delivering the queued continuation rather than dropping it"
            );
        }
        {
            let store = self.store.lock().await;
            if store.session_or_custody_has_settlement_fence(session_id)? {
                return Err(DaemonError::PolicyDenied(
                    "source-worktree settlement journal fences this session continuation".into(),
                ));
            }
            if store.ordinary_session_path_inside_live_custody_root(session_id)? {
                return Err(DaemonError::PolicyDenied(
                    "ordinary session cwd is inside a live sandbox custody root".into(),
                ));
            }
        }

        // Review round 3: the effect claim. The guarded check above can be
        // overtaken by a lead writer that does not take this tip's guard
        // (SetEpicLead of the caller's Epic). Revalidate and commit the first
        // durable write in one IMMEDIATE transaction before any effect below.
        if let Some(fence) = fence.as_ref() {
            self.store.lock().await.claim_continuation_effect(fence)?;
        }

        // Acquire settlement-producer ownership before removing the completed
        // session, admitting an invocation, or spawning a provider. A sealed
        // dispatcher therefore rejects the resume without partial side effects.
        let model_call_settlements = self.model_call_settlements.handle()?;

        // Suppression is durable user intent and must commit before this path
        // takes the sole completed-map/timer owner.  In particular, a Store
        // failure below must leave both ownership handles intact.
        let mut completed_session = {
            let mut completed = self.completed.write().await;
            let retry_exhaustion_to_persist = completed.get(&session_id).and_then(|cs| {
                (cs.retry_cancel.is_some() || cs.retry_fired_at.is_some())
                    .then_some(cs.session.max_retries)
                    .flatten()
            });
            if let Some(max_retries) = retry_exhaustion_to_persist {
                // Do not remove the completed entry or cancel its timer until
                // durable suppression commits.  The guard makes the
                // snapshot-to-commit boundary deterministic.
                self.store
                    .lock()
                    .await
                    .suppress_c5_autofile_pending_and_exhaust_retry(session_id, max_retries)?;
            }
            match completed.remove(&session_id) {
                Some(mut cs) => {
                    // Cancel any pending retry timer
                    if let Some(cancel) = cs.retry_cancel.take() {
                        let _ = cancel.send(());
                    }
                    cs.retry_fired_at = None;
                    // The acknowledged suppression already exhausted the
                    // durable row; mirror that committed state locally.
                    exhaust_retry_budget(&mut cs.session);
                    cs
                }
                None => {
                    drop(completed);
                    let is_active = self.active.read().await.contains_key(&session_id);
                    if is_active {
                        tracing::info!(session_id = %session_id, "Session is active -- interrupting before continue");
                        let mut bus_rx = self.event_bus.subscribe();
                        self.interrupt_session(session_id).await?;
                        let deadline = tokio::time::Instant::now() + Self::CONTINUE_INTERRUPT_WAIT;
                        loop {
                            match tokio::time::timeout_at(deadline, bus_rx.recv()).await {
                                Ok(Ok(arc_event)) => match arc_event.as_ref() {
                                    DaemonEvent::SessionStatusChanged {
                                        session_id: sid,
                                        new_status,
                                        ..
                                    } if *sid == session_id
                                        && matches!(
                                            new_status,
                                            SessionStatus::Completed
                                                | SessionStatus::Interrupted
                                                | SessionStatus::Failed
                                        ) =>
                                    {
                                        break;
                                    }
                                    _ => continue,
                                },
                                Ok(Err(_)) => {
                                    continue;
                                }
                                Err(_) => {
                                    self.event_bus.unsubscribe();
                                    // Past this deadline the teardown is no
                                    // longer merely slow. The dominant cause is
                                    // unsettled provider ownership
                                    // (`ProcessSettlementOutcome::EscalationFailed`),
                                    // which never publishes a terminal status at
                                    // all, so this timeout is the only signal the
                                    // caller ever gets — say so rather than
                                    // implying a transient wait.
                                    tracing::error!(
                                        session_id = %session_id,
                                        waited_secs = Self::CONTINUE_INTERRUPT_WAIT.as_secs(),
                                        "Interrupted session did not reach a terminal status; provider ownership may be unsettled"
                                    );
                                    return Err(DaemonError::Rpc(format!(
                                        "Session did not finalize within {}s of interrupt; the provider process may be wedged (unsettled ownership) — check the daemon log for a settlement error before retrying",
                                        Self::CONTINUE_INTERRUPT_WAIT.as_secs()
                                    )));
                                }
                            }
                        }
                        self.event_bus.unsubscribe();
                        let mut completed = self.completed.write().await;
                        completed.remove(&session_id).ok_or_else(|| {
                            DaemonError::Rpc(
                                "Session finalized but not found in completed map".to_string(),
                            )
                        })?
                    } else if let Some(cs) =
                        self.load_completed_session_from_store(session_id).await?
                    {
                        // The completed map lost this entry (e.g. a daemon
                        // restart raced its restoration) even though the row
                        // is durably terminal in the store. Recover it from
                        // there rather than reporting a false not-found.
                        cs
                    } else {
                        return Err(DaemonError::SessionNotFound(session_id));
                    }
                }
            }
        };

        // C7 Phase 1: a restored session may still carry the hydration
        // placeholder (`events_hydrated == false`, `events` empty) -- see
        // `SessionManager::restore_sessions`. Below, `completed_session.events`
        // is probed for the last sequence, replayed to API providers, and
        // carried forward verbatim into the new `TrackedSession`; an
        // unhydrated placeholder here would silently truncate the resumed
        // session's history to nothing. Hydrate before any of that.
        if let Err(e) = self.hydrate_completed_events(&mut completed_session).await {
            self.completed
                .write()
                .await
                .insert(session_id, completed_session);
            return Err(e);
        }

        if let Some(retry_child_id) = completed_session.superseded_by_retry.take() {
            let interrupted = {
                let mut active = self.active.write().await;
                if let Some(child) = active.get_mut(&retry_child_id) {
                    child.interrupt_requested = true;
                    if let Some(process) = child.process.as_ref()
                        && let Err(e) = process.interrupt()
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            retry_child_id = %retry_child_id,
                            error = %e,
                            "Failed to interrupt live retry child before continue"
                        );
                    }
                    let _ = child.stop_tx.try_send(());
                    true
                } else {
                    false
                }
            };
            if interrupted {
                self.event_bus.publish(DaemonEvent::SystemMessage {
                    level: "warn".to_string(),
                    message: format!(
                        "Continuing session {session_id}; interrupted live retry child {retry_child_id}"
                    ),
                });
            } else {
                tracing::info!(
                    session_id = %session_id,
                    retry_child_id = %retry_child_id,
                    "Retry child no longer active before continue; clearing supersession marker"
                );
            }
        }

        let current_status = completed_session.session.status;
        if !matches!(
            current_status,
            SessionStatus::Completed
                | SessionStatus::Interrupted
                | SessionStatus::Failed
                | SessionStatus::WaitingApproval
        ) {
            self.completed
                .write()
                .await
                .insert(session_id, completed_session);
            return Err(DaemonError::Rpc(format!(
                "Session {} cannot be continued (status: {:?})",
                session_id, current_status
            )));
        }

        // A synchronous CodexAppServer resume is actually a Codex CLI launch.
        // Provider identity is durable security state, so represent this as a
        // fresh UUID and row instead of mutating or reusing the app-server row.
        if completed_session.session.provider == SessionProvider::CodexAppServer {
            let replacement_source = completed_session.session.clone();
            self.completed
                .write()
                .await
                .insert(session_id, completed_session);
            Box::pin(self.launch_effective_provider_replacement(
                replacement_source,
                query,
                cwd_admission_guard,
            ))
            .await?;
            return Ok(ContinueSessionOutcome::Started);
        }

        let initial_sequence = completed_session
            .events
            .last()
            .map(|event| event.sequence + 1)
            .unwrap_or(0);

        // Reuse never invents a replacement sandbox or silently downgrades
        // custody-bearing history to the canonical checkout. Authenticate the
        // completed session before *any* context/Git work, token mint,
        // admission, orphan reap, provider dispatch, or active publication.
        let custody = match CustodyService::classify(&completed_session.session) {
            Ok(CustodyClassification::OrdinaryUnsandboxed) => {
                CustodyService::authorize_ordinary(&completed_session.session)
            }
            Ok(CustodyClassification::RequiresPersistedAuthentication) => {
                let mut store = self.store.lock().await;
                CustodyService::authorize_live(
                    &completed_session.session,
                    &mut store,
                    self.sandbox_allocator.base_dir(),
                    rsi_common::types::SandboxCustodyTransitionV1::Continue,
                )
            }
            Err(error) => Err(error),
        };
        let prepared_launch = match custody {
            Ok(custody) => PreparedLaunch::new(custody),
            Err(error) => {
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return Err(error);
            }
        };

        let resumable_id = resumable_provider_session_id(&completed_session.session);
        let fresh_relaunch = agent_child.is_some() && resumable_id.is_none();
        let (provider_session_id, conversation_history) = match resumable_id {
            // API providers reconstruct context from history under the
            // synthetic id; CLI providers resume with the captured id.
            Some(id) => {
                let history = (completed_session.session.provider == SessionProvider::Local)
                    .then(|| Self::events_to_openai_messages(&completed_session.events));
                (Some(id), history)
            }
            None if fresh_relaunch => (None, None),
            None => {
                let error = unavailable_resume_error(completed_session.session.provider);
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return Err(error);
            }
        };
        let mut fresh_task_source = None;

        #[cfg(test)]
        if let Err(error) = apply_continue_custody_root_mutation_for_test(
            &completed_session.session,
            initial_sequence,
        ) {
            self.completed
                .write()
                .await
                .insert(session_id, completed_session);
            return Err(error);
        }

        // This permit is the only location authority for the continue/context
        // boundary. It revalidates immediately before the context pipeline and
        // supplies both the context cwd and the temporary raw-config paths
        // consumed by the still-unconverted common provider funnel.
        let context_permit = match CustodyService::begin_effect(
            &self.store,
            &self.custody_settlements,
            prepared_launch.custody(),
            &completed_session.session,
            self.sandbox_allocator.base_dir(),
            self.program_run_boot_id,
            EffectKind::ContextRead,
        )
        .await
        {
            Ok(permit) => permit,
            Err(error) => {
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return Err(error);
            }
        };
        let context_cwd = context_permit.effective_cwd().to_path_buf();
        let cargo_target_dir = context_permit.cargo_target_dir().map(ToOwned::to_owned);
        if fresh_relaunch {
            let task = {
                let store = self.store.lock().await;
                super::rotation::resolve_rotation_task_query(&store, &completed_session.session)
            };
            let (source, task) = match task {
                Ok(Some(task)) => task,
                Ok(None) => {
                    drop(context_permit);
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Err(crate::error::agent_continue_error(
                        AgentContinueErrorCodeV1::ResumeUnavailableTaskUnresolved,
                        None,
                        None,
                    ));
                }
                Err(error) => {
                    drop(context_permit);
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Err(error);
                }
            };
            fresh_task_source = Some(source);
            let pointer = super::rotation::rotation_task_pointer(
                &completed_session.session,
                super::rotation::PointerKind::FreshRelaunch,
            )
            .await;
            query = format!("{task}\n\n{pointer}\n\nLead instruction:\n{query}");
            let child = agent_child
                .as_ref()
                .expect("fresh relaunch has agent intent");
            let inserted = self
                .store
                .lock()
                .await
                .insert_child_relaunch_intent(&child.row);
            match inserted {
                Ok(InsertRelaunchIntent::Inserted) => {}
                Ok(InsertRelaunchIntent::TipInProgress) => {
                    drop(context_permit);
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Err(crate::error::agent_continue_error(
                        AgentContinueErrorCodeV1::RelaunchInProgress,
                        None,
                        None,
                    ));
                }
                Ok(InsertRelaunchIntent::Existing(row)) => {
                    drop(context_permit);
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    let row = if row.state == RelaunchState::Intent {
                        self.recover_agent_child_relaunch(&row).await?
                    } else {
                        *row
                    };
                    return child_relaunch_row_outcome(&row);
                }
                Err(error) => {
                    drop(context_permit);
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Err(error);
                }
            }
        }
        let query_for_event = query.clone();
        #[cfg(test)]
        if let Err(error) = apply_continue_execution_scratch_failure_for_test(
            &completed_session.session,
            initial_sequence,
        ) {
            drop(context_permit);
            self.completed
                .write()
                .await
                .insert(session_id, completed_session);
            return Err(error);
        }
        let execution_scratch =
            match crate::sandbox::execution_scratch::SandboxExecutionScratch::from_context_permit(
                &context_permit,
            ) {
                Ok(scratch) => scratch,
                Err(error) => {
                    drop(context_permit);
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Err(error);
                }
            };

        if let Err(error) = crate::provider_capabilities::refresh_installed_catalog_for_provider(
            completed_session.session.provider,
            Arc::clone(&self.runtime_config),
        )
        .await
        {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "Installed provider catalog refresh failed on continue; using degraded capability evidence"
            );
        }
        let prior_model = completed_session.session.model.clone();
        let prior_context_window = completed_session.session.context_window;
        let prior_budget = completed_session.session.resolved_context_budget.clone();
        let next_budget = crate::provider_capabilities::resolve_new_incarnation_context_budget(
            &completed_session.session,
        );
        let budget_persisted = self
            .persistence
            .compare_and_update_session_model(
                Arc::clone(&self.store),
                session_id,
                prior_model.clone(),
                prior_context_window,
                prior_budget,
                prior_model,
                Some(next_budget.active_tokens),
                Some(next_budget.clone()),
            )
            .await;
        match budget_persisted {
            Ok(true) => install_context_budget(&mut completed_session.session, next_budget),
            Ok(false) => {
                drop(context_permit);
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return Err(DaemonError::Store(
                    "session context budget changed while continue was preparing".into(),
                ));
            }
            Err(error) => {
                drop(context_permit);
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return Err(error);
            }
        }

        // Compose system prompt via context pipeline (memory + project files + git + active task).
        let system_prompt = if matches!(
            completed_session.session.provider,
            SessionProvider::Claude | SessionProvider::Local | SessionProvider::Antigravity
        ) {
            // Resolve project for context pipeline
            let project: Option<rsi_common::types::Project> =
                if let Some(pid) = completed_session.session.project_id {
                    let store = self.store.clone();
                    tokio::task::spawn_blocking(move || {
                        let store = store.blocking_lock();
                        store.get_project(pid).ok().flatten()
                    })
                    .await
                    .ok()
                    .flatten()
                } else {
                    None
                };

            let active_task = completed_session.session.active_task.as_deref();

            let (pipeline_project, pipeline_active_task) = if matches!(
                completed_session.session.session_kind,
                SessionKind::TaskRabbit | SessionKind::Bug
            ) {
                (None, active_task)
            } else {
                (project.as_ref(), active_task)
            };

            let pipeline = super::context_pipeline::ContextPipeline::new(
                Arc::clone(&self.token_counter),
                self.memory_handle.clone(),
                Arc::clone(&self.store),
            );
            let budget = super::context_pipeline::context_injection_allowance(
                completed_session
                    .session
                    .resolved_context_budget
                    .as_ref()
                    .expect("continued leaf sessions carry a resolved context budget")
                    .active_tokens,
            );

            pipeline
                .assemble(
                    &query,
                    &context_cwd,
                    pipeline_project,
                    pipeline_active_task,
                    None, // workflow content not re-injected on continue
                    Some(session_id),
                    budget,
                )
                .await
        } else {
            None
        };
        let system_prompt = super::preamble::prepend_sandbox_custody_instruction(
            system_prompt,
            super::preamble::sandbox_custody_instruction_for_session(&completed_session.session),
        );
        let system_prompt = if fresh_relaunch {
            let mut parts =
                super::launch::fresh_launch_preamble_parts(completed_session.session.session_kind);
            parts.extend(system_prompt);
            Some(parts.join("\n\n"))
        } else {
            system_prompt
        };

        // A6 (G1): a continued session is a NEW OS process for this existing
        // session id — re-mint (revoke-then-register) its authority token
        // BEFORE the config is built, so registration precedes the guarded
        // spawn below (mirrors launch_session's fast-first-callback ordering:
        // a fast first `rsi-rpc` callback from the child must resolve).
        // Scheduler resume-mode wakes (`resume_scheduled`) route through this
        // path and are covered transitively.
        self.store
            .lock()
            .await
            .remove_controller_grant_v1(session_id);
        let session_token = self.remint_session_token(session_id).await;

        let config = LaunchConfig {
            query,
            title: None,
            agent_role: completed_session.session.agent_role.clone(),
            epic_spawn_ordinal: completed_session.session.epic_spawn_ordinal,
            working_dir: Some(context_cwd),
            provider: Some(completed_session.session.provider),
            model: completed_session.session.model.clone(),
            configured_context_window: completed_session
                .session
                .resolved_context_budget
                .as_ref()
                .and_then(|budget| budget.capacity.configured_tokens),
            max_turns: None,
            system_prompt,
            resume_session_id: provider_session_id,
            session_kind: None,
            project_id: completed_session.session.project_id,
            rsi_session_id: Some(session_id),
            rsi_socket: Some(self.socket_path.clone()),
            rsi_session_token: Some(session_token.clone()),
            continued_from: completed_session.session.continued_from,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history,
            workflow_id: completed_session.session.workflow_id,
            workflow_id_override: completed_session.session.workflow_id_override,
            max_retries: None,
            group_id: completed_session.session.group_id,
            skip_project_model_default: false,
            model_invocation_purpose: if fresh_relaunch {
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh
            } else {
                rsi_common::model_control::ModelInvocationPurpose::SessionContinueResume
            },
            parent_id: completed_session.session.parent_id,
            effort: completed_session.session.effort.clone(),
            issue_identifier: completed_session.session.issue_identifier.clone(),
            issue_url: completed_session.session.issue_url.clone(),
            issue_tracker_id: completed_session.session.issue_tracker_id.clone(),
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: if fresh_relaunch {
                agent_child
                    .as_ref()
                    .map(|child| child.row.dedup_key.clone())
            } else {
                None
            },
            model_invocation_request_fingerprint: if fresh_relaunch {
                agent_child
                    .as_ref()
                    .map(|child| child.row.request_fingerprint.clone())
            } else {
                None
            },
            // Re-using an existing sandbox; no re-allocation needed.
            sandbox: None,
            cargo_target_dir,
            execution_scratch,
            // RSI-006: continue chains inherit the parent's eval status.
            is_eval: completed_session.session.is_eval,
            // Continue path reuses system_prompt verbatim; ContextPipeline is
            // not re-run by callers anyway. Default false preserves existing
            // production behavior; eval-driven continues set this via session.is_eval.
            skip_context_pipeline: completed_session.session.is_eval,
            // Continue inherits the parent's declared capability class.
            capability_class: completed_session.session.capability_class,
            // Continue inherits tags from parent session.
            tags: completed_session.session.tags.clone(),
            // P1.7: continue is a context-window event, not a new topology
            // node spawn — children do NOT inherit the parent's binding.
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        };

        #[cfg(test)]
        observe_continue_custody_config_for_test(session_id, initial_sequence, &config);

        // Dropping queues asynchronous permit settlement before the raw
        // provider funnel. It intentionally makes no synchronous SQLite
        // settlement claim.
        drop(context_permit);

        let provider = completed_session.session.provider;
        let purpose = config.model_invocation_purpose;
        let parent_invocation_id = {
            let store_ref = self.store.clone();
            tokio::task::spawn_blocking(move || {
                let store = store_ref.blocking_lock();
                store.session_model_invocation_id(session_id)
            })
            .await
            .map_err(|e| {
                DaemonError::Process(format!("failed to join resume invocation lookup: {e}"))
            })??
        };
        let provider_label = format!("{provider:?}");
        let owner = match capacity_delivery {
            Some(delivery) => InvocationOwner {
                session_id: Some(session_id),
                project_id: completed_session.session.project_id,
                scheduled_job_id: Some(delivery.wake_job_id),
                ..Default::default()
            },
            None => owner_from_session(&completed_session.session),
        };
        let admission_request = crate::model_control::ModelAdmissionRequest {
            purpose,
            provider: Some(provider_label.clone()),
            model: config.model.clone(),
            backend: Some(provider_label.clone()),
            effort: config.effort.clone(),
            trigger: if fresh_relaunch {
                "agent_child_relaunch".to_string()
            } else if capacity_delivery.is_some() {
                "scheduled_capacity_resume".to_string()
            } else {
                "continue_session".to_string()
            },
            owner,
            dedup_key: Some(match capacity_delivery {
                Some(delivery) => format!(
                    "scheduled.resume.capacity:{}:{}",
                    delivery.wake_job_id,
                    delivery
                        .due_slot
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ),
                None if fresh_relaunch => agent_child
                    .as_ref()
                    .expect("fresh relaunch intent")
                    .row
                    .dedup_key
                    .clone(),
                None => manager_decision
                    .as_ref()
                    .map(|delivery| format!("manager.answer:{}", delivery.key))
                    .or_else(|| {
                        manager_action
                            .as_ref()
                            .map(|claim| format!("manager.action:{}", claim.id()))
                    })
                    .unwrap_or_else(|| format!("{purpose}:{session_id}:{initial_sequence}")),
            }),
            request_fingerprint: Some(if fresh_relaunch {
                agent_child
                    .as_ref()
                    .expect("fresh relaunch intent")
                    .row
                    .request_fingerprint
                    .clone()
            } else {
                hash_request_fingerprint(&[
                    purpose.as_str(),
                    &format!("{provider:?}"),
                    config.model.as_deref().unwrap_or(""),
                    &query_for_event,
                ])
            }),
            parent_invocation_id,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                purpose,
                Some(provider_label.as_str()),
                Some(provider_label.as_str()),
                config.model.as_deref(),
            )),
            baseline_input_tokens: completed_session.session.total_input_tokens.unwrap_or(0),
            baseline_output_tokens: completed_session.session.total_output_tokens.unwrap_or(0),
            baseline_cache_creation_tokens: completed_session
                .session
                .total_cache_creation_tokens
                .unwrap_or(0),
            baseline_cache_read_tokens: completed_session
                .session
                .total_cache_read_tokens
                .unwrap_or(0),
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: completed_session.session.work_time_ms.unwrap_or(0),
        };
        let admission_request_for_recovery = admission_request.clone();
        let (admission_permit, recovered_capacity_admission) = if capacity_delivery.is_some() {
            match admit_capacity_invocation(&self.store, admission_request, self.event_bus()).await
            {
                Ok(CapacityAdmissionDecision::Admitted(permit)) => (permit, false),
                Ok(CapacityAdmissionDecision::CapacityDuplicate {
                    invocation_id,
                    phase: crate::store::capacity_recovery::CapacityDeliveryPhase::Admitted,
                }) => (
                    resume_unexecuted_capacity_delivery_admission(
                        &self.store,
                        &admission_request_for_recovery,
                        invocation_id,
                    )
                    .await?,
                    true,
                ),
                Ok(CapacityAdmissionDecision::CapacityDuplicate {
                    invocation_id,
                    phase: crate::store::capacity_recovery::CapacityDeliveryPhase::LaunchConfirmed,
                }) => {
                    self.revoke_agent_token_for_session(session_id).await;
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Ok(ContinueSessionOutcome::CapacityAlreadyLaunchConfirmed(
                        invocation_id,
                    ));
                }
                Err(error) => {
                    self.revoke_agent_token_for_session(session_id).await;
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Err(error);
                }
            }
        } else {
            match admit_invocation(&self.store, admission_request, self.event_bus()).await {
                Ok(AdmissionDecision::Admitted(permit)) => (permit, false),
                Ok(AdmissionDecision::Duplicate { invocation_id }) => {
                    self.revoke_agent_token_for_session(session_id).await;
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    if fresh_relaunch {
                        let child = agent_child.as_ref().expect("fresh relaunch intent");
                        let store = self.store.lock().await;
                        store
                            .bind_child_relaunch_invocation(child.row.request_id, invocation_id)?;
                        drop(store);
                        let row = self.recover_agent_child_relaunch(&child.row).await?;
                        return child_relaunch_row_outcome(&row);
                    }
                    return Err(DaemonError::PolicyDenied(format!(
                        "duplicate resume admission blocked backend execution: {invocation_id}"
                    )));
                }
                Err(error) => {
                    self.revoke_agent_token_for_session(session_id).await;
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    if fresh_relaunch {
                        let child = agent_child.as_ref().expect("fresh relaunch intent");
                        let receipt = serde_json::json!({"request_id":child.row.request_id,"reason":"admission_refused"}).to_string();
                        self.store.lock().await.settle_child_relaunch_intent(
                            child.row.request_id,
                            RelaunchState::Abandoned,
                            None,
                            &receipt,
                            Some("admission_refused"),
                        )?;
                    }
                    return Err(error);
                }
            }
        };

        if fresh_relaunch {
            let child = agent_child.as_ref().expect("fresh relaunch intent");
            let bind_result = self.store.lock().await.bind_child_relaunch_invocation(
                child.row.request_id,
                admission_permit.invocation_id(),
            );
            if let Err(error) = bind_result {
                self.revoke_agent_token_for_session(session_id).await;
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return Err(error);
            }
        }

        #[cfg(test)]
        if capacity_delivery.is_some() {
            super::launch::pause_controller_candidate_test(
                session_id,
                super::launch::ControllerCandidateTestPhase::AfterCapacityAdmissionBeforeProviderSpawn,
            )
            .await;
            if super::launch::take_after_capacity_admission_before_provider_spawn_failure(
                session_id,
            ) {
                self.revoke_agent_token_for_session(session_id).await;
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return Err(DaemonError::Process(
                    "injected failure after capacity admission before provider spawn".into(),
                ));
            }
        }

        // Launch through the single guarded spawn primitive (`&spawn_guard` is the
        // mandatory single-flight witness). The harness replay history is needed
        // ONLY on the Harness path, so compute it lazily here — non-harness
        // continues never iterate the event log.
        let conversation_history = if provider == SessionProvider::Harness {
            Some(Self::events_to_harness_messages(&completed_session.events))
        } else {
            None
        };
        let launcher = super::provider_spawn::CachedLauncher {
            mgr: self,
            harness: super::provider_spawn::HarnessLaunchCtx {
                conversation_history,
                project_id: completed_session.session.project_id,
                initial_admission_permit: admission_permit.clone(),
                model_call_settlements: model_call_settlements.clone(),
                resolved_context_budget: completed_session
                    .session
                    .resolved_context_budget
                    .clone()
                    .expect("continued sessions carry a resolved context budget"),
            },
        };
        // A5 (Change 2): reap any cross-restart orphan of THIS session before
        // spawning, so a provider that survived a prior teardown can never
        // coexist with the fresh child (F-006/F-007). Harness itself is
        // task-based, but its shell-tool subprocesses inherit the same exact
        // `RSI_SESSION_ID` stamp and therefore require the same exclusion.
        // Local has no daemon-owned subprocess boundary. The A1 single-flight
        // guard (acquired at fn entry, still held here) closes the reap->spawn
        // window; the reap runs strictly before spawn, so the new child does
        // not yet exist and can never be a scan target. Blocking `/proc` walk
        // -> `spawn_blocking`.
        if matches!(
            provider,
            SessionProvider::Claude
                | SessionProvider::Codex
                | SessionProvider::Pioneer
                | SessionProvider::OpenRouter
                | SessionProvider::Bedrock
                | SessionProvider::Antigravity
                | SessionProvider::CodexAppServer
                | SessionProvider::Harness
        ) {
            let sid = session_id;
            let reap_task = if capacity_delivery.is_some() {
                tokio::task::spawn_blocking(move || {
                    super::reaper::reap_capacity_orphans_checked(sid)
                })
                .await
                .map_err(|error| {
                    DaemonError::Process(format!(
                        "capacity orphan reap task failed before spawn: {error}"
                    ))
                })
            } else {
                tokio::task::spawn_blocking(move || super::reaper::reap_orphans_for_session(sid))
                    .await
                    .map_err(|error| {
                        DaemonError::Process(format!(
                            "runtime orphan reap task failed before spawn: {error}"
                        ))
                    })
            };
            let reaped = match reap_task.and_then(|result| result) {
                Ok(reaped) => reaped,
                Err(error) => {
                    // Capacity redelivery must retain its admission: its
                    // recovery path requires the original Admitted/Running
                    // invocation. Other resumes have no such receipt, so
                    // release their dedup slot for retry.
                    if capacity_delivery.is_none()
                        && let Err(settle_error) = complete_invocation(
                            &self.store,
                            &admission_permit,
                            InvocationCompletion {
                                error_class: Some("orphan_reap_failed".to_string()),
                                confidence: Some(ModelUsageConfidence::Unavailable),
                                ..InvocationCompletion::default()
                            },
                            self.event_bus(),
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %settle_error,
                            session_id = %session_id,
                            "Failed to settle resume admission after orphan-reap error"
                        );
                    }
                    self.revoke_agent_token_for_session(session_id).await;
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Err(error);
                }
            };
            if reaped > 0 {
                self.event_bus.publish(DaemonEvent::SystemMessage {
                    level: "warn".to_string(),
                    message: format!(
                        "Reaped {reaped} orphaned provider process(es) for session {sid} before resume"
                    ),
                });
            }
        }
        if let Some(claim) = manager_action.as_ref() {
            let fence = match self.check_manager_action_runtime(claim, true).await {
                Ok(()) => self
                    .store
                    .lock()
                    .await
                    .bind_manager_resume_invocation(claim, admission_permit.invocation_id()),
                Err(error) => Err(error),
            };
            if let Err(error) = fence {
                let _ = complete_invocation(
                    &self.store,
                    &admission_permit,
                    InvocationCompletion {
                        error_class: Some("manager_action_fence_rejected".into()),
                        confidence: Some(ModelUsageConfidence::Unavailable),
                        ..InvocationCompletion::default()
                    },
                    self.event_bus(),
                )
                .await;
                self.revoke_agent_token_for_session(session_id).await;
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return Err(error);
            }
        }
        if let Some(delivery) = manager_decision.as_mut() {
            let marked = async {
                self.check_manager_decision_runtime(delivery).await?;
                self.store
                    .lock()
                    .await
                    .manager_v2_set_decision_delivery(delivery, "running", true, None)
            }
            .await;
            match marked {
                Ok(marked) => **delivery = marked,
                Err(error) => {
                    let _ = complete_invocation(
                        &self.store,
                        &admission_permit,
                        InvocationCompletion {
                            error_class: Some("manager_decision_fence_rejected".into()),
                            confidence: Some(ModelUsageConfidence::Unavailable),
                            ..InvocationCompletion::default()
                        },
                        self.event_bus(),
                    )
                    .await;
                    self.revoke_agent_token_for_session(session_id).await;
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Err(error);
                }
            }
        }
        #[cfg(test)]
        if manager_seat.is_some() {
            super::launch::pause_controller_candidate_test(
                session_id,
                super::launch::ControllerCandidateTestPhase::ManagerSeatBeforeFinalGate,
            )
            .await;
        }
        // #669 final seat fence: after catalog/context preparation, model
        // admission and orphan reaping, re-run the exact seat predicate and
        // hold the Store guard across the synchronous spawn below, so no
        // pause, revocation, question or rotation can interleave.
        let seat_fence = if let Some(claim) = manager_seat.as_ref() {
            let store = self.store.lock().await;
            match store.manager_seat_effect_gate(
                claim,
                self.runtime_config
                    .retry_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
            ) {
                Ok(()) => Some(store),
                Err(error) => {
                    drop(store);
                    let _ = complete_invocation(
                        &self.store,
                        &admission_permit,
                        InvocationCompletion {
                            error_class: Some("manager_seat_fence_rejected".into()),
                            confidence: Some(ModelUsageConfidence::Unavailable),
                            ..InvocationCompletion::default()
                        },
                        self.event_bus(),
                    )
                    .await;
                    self.revoke_agent_token_for_session(session_id).await;
                    self.completed
                        .write()
                        .await
                        .insert(session_id, completed_session);
                    return Err(error);
                }
            }
        } else {
            None
        };
        #[cfg(test)]
        let launch_result = if super::launch::take_capacity_provider_spawn_failure(session_id) {
            Err(DaemonError::Process(
                "injected capacity provider spawn failure".into(),
            ))
        } else {
            super::launch::take_controller_candidate_test_process(session_id).map_or_else(
                || {
                    super::provider_spawn::spawn_provider_process(
                        provider,
                        &config,
                        &launcher,
                        &admission_permit,
                        &spawn_guard,
                    )
                },
                Ok,
            )
        };
        #[cfg(not(test))]
        let launch_result = super::provider_spawn::spawn_provider_process(
            provider,
            &config,
            &launcher,
            &admission_permit,
            &spawn_guard,
        );
        drop(seat_fence);
        let (mut process, event_rx) = match launch_result {
            Ok(launch) => launch,
            Err(e) => {
                if capacity_delivery.is_none() {
                    if let Err(settle_error) = complete_invocation(
                        &self.store,
                        &admission_permit,
                        InvocationCompletion {
                            error_class: Some("spawn_failed".to_string()),
                            confidence: Some(ModelUsageConfidence::Unavailable),
                            ..InvocationCompletion::default()
                        },
                        self.event_bus(),
                    )
                    .await
                    {
                        tracing::warn!(
                            error = %settle_error,
                            session_id = %session_id,
                            "Failed to settle resume admission after spawn error"
                        );
                    }
                }
                self.revoke_agent_token_for_session(session_id).await;
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                if fresh_relaunch {
                    let child = agent_child.as_ref().expect("fresh relaunch intent");
                    let receipt = serde_json::json!({"request_id":child.row.request_id,"reason":"spawn_failed","invocation_id":admission_permit.invocation_id()}).to_string();
                    self.store.lock().await.settle_child_relaunch_intent(
                        child.row.request_id,
                        RelaunchState::Abandoned,
                        Some(admission_permit.invocation_id()),
                        &receipt,
                        Some("spawn_failed"),
                    )?;
                }
                return Err(e);
            }
        };
        if let Some(delivery) = manager_decision.as_ref() {
            #[cfg(test)]
            super::launch::pause_controller_candidate_test(
                session_id,
                super::launch::ControllerCandidateTestPhase::ManagerQuestionBeforeClear,
            )
            .await;
            if let Err(error) = self.clear_delivered_manager_question(delivery).await {
                self.revoke_agent_token_for_session(session_id).await;
                self.retain_failed_manager_question_process(
                    manager_question_cleanup::FailedQuestionProcess {
                        process,
                        completed: completed_session,
                        permit: admission_permit,
                        spawn_guard,
                        cwd_guard: cwd_admission_guard,
                        settlements: model_call_settlements,
                    },
                )
                .await;
                return Err(error);
            }
            completed_session.session.pending_question = None;
        }
        if let Some(delivery) = capacity_delivery {
            let confirmation = {
                let store = self.store.lock().await;
                store.confirm_capacity_delivery_launch(
                    admission_permit.invocation_id(),
                    delivery.wake_job_id,
                    delivery.due_slot,
                    chrono::Utc::now(),
                )
            };
            if let Err(error) = confirmation {
                let kill_result = process.kill().await;
                let reap_result = tokio::task::spawn_blocking(move || {
                    super::reaper::reap_capacity_orphans_checked(session_id)
                })
                .await
                .map_err(|join_error| {
                    DaemonError::Process(format!(
                        "capacity confirmation cleanup task failed: {join_error}"
                    ))
                })
                .and_then(|result| result);
                self.revoke_agent_token_for_session(session_id).await;
                self.completed
                    .write()
                    .await
                    .insert(session_id, completed_session);
                return Err(DaemonError::Store(format!(
                    "capacity launch confirmation failed after provider spawn: {error}; kill={kill_result:?}; checked_reap={reap_result:?}"
                )));
            }
        }
        let (stop_tx, stop_rx) = mpsc::channel(1);

        let mut session = completed_session.session;
        sanitize_codex_restored_context_usage(&mut session, &completed_session.turn_metrics);

        let hydrated_input = session.total_input_tokens.unwrap_or(0);
        let hydrated_output = session.total_output_tokens.unwrap_or(0);
        let hydrated_confidence = if hydrated_input > 0 {
            ContextUsageConfidence::Partial
        } else {
            ContextUsageConfidence::Missing
        };

        let (live_input, live_output, live_confidence) = if hydrated_input > 0 {
            (hydrated_input, hydrated_output, hydrated_confidence)
        } else {
            let store_ref = self.store.clone();
            let sid = session_id;
            let snapshot_tokens = tokio::task::spawn_blocking(move || {
                let store = store_ref.blocking_lock();
                store.load_latest_context_snapshot(sid)
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten()
            .unwrap_or(0);
            if snapshot_tokens > 0 {
                (snapshot_tokens, 0, ContextUsageConfidence::Partial)
            } else {
                (0, 0, ContextUsageConfidence::Missing)
            }
        };

        let old_status = session.status;
        session.status = SessionStatus::Starting;
        // A new turn owns its own outcome. `stop_reason` describes how the
        // PREVIOUS turn ended, and nothing else clears it, so leaving it set
        // published a stale terminal reason against a live session: the
        // 2s metadata snapshot writes `stop_reason` but NOT `status`
        // (`Store::update_session_metadata`), which is how sessions ended up
        // reading `status='Running'` alongside a `stop_reason` from a turn that
        // had already failed minutes earlier. See issue #15.
        session.stop_reason = None;
        session.updated_at = chrono::Utc::now();
        let is_codex = matches!(
            session.provider,
            SessionProvider::Codex
                | SessionProvider::Pioneer
                | SessionProvider::OpenRouter
                | SessionProvider::Bedrock
        );

        let rotation_depth = session.rotation_depth;
        let rotation_enabled =
            self.context_rotation_enabled && session.rotation_disabled_at.is_none();
        let rotation = if query_for_event.trim() == "/create_handoff" {
            super::rotation_coordinator::RotationCoordinator::new_writing_handoff(
                session_id,
                rotation_depth,
                rotation_enabled,
            )
        } else {
            super::rotation_coordinator::RotationCoordinator::new(
                session_id,
                rotation_depth,
                rotation_enabled,
            )
        };

        let controller_project_id = session.project_id;
        let spawn_generation = self.next_spawn_generation();
        let tracked = TrackedSession {
            pending_archive: session.pending_archive,
            session,
            spawn_generation,
            events: completed_session.events,
            turn_metrics: completed_session.turn_metrics,
            process: Some(process),
            deferred_successor_start_gate: None,
            stop_tx,
            interrupt_requested: false,
            rotation,
            live_input_tokens: if is_codex { 0 } else { live_input },
            live_output_tokens: if is_codex { 0 } else { live_output },
            live_usage_confidence: if is_codex {
                ContextUsageConfidence::Missing
            } else {
                live_confidence
            },
            daemon_input_tokens: 0,
            daemon_output_tokens: 0,
            daemon_tokens_at_last_api_update: 0,
            codex_context_tokens: 0,
            pipeline_artifact: None,
            memory_flush_compaction_count: None,
            pending_question: None,
            // Continue reuses the session UUID but the provider subprocess is new;
            // accumulator tracks only the current subprocess lifetime. Finalize will
            // overwrite `session.approval_wait_ms` with the new accumulated total.
            approval_wait_start: None,
            approval_wait_total_ms: 0,
            // TD1: unlike approval_wait_ms, work_time_ms is lifetime-cumulative across
            // continues — do NOT reset it here. The real base is captured at the next
            // Running-entry (monitor.rs Site 1) from the reused `session.work_time_ms`
            // floor; finalize then ADDS the new interval rather than overwriting.
            work_run_start: None,
            work_time_base_ms: 0,
            received_meaningful_output: false,
            exit_code: None,
            retry_attempt: 0,
            max_retries: 0,
            last_event_at: chrono::Utc::now(),
            stall_interrupted: false,
            last_usage_update: None,
            last_mismatch_warn: None,
            last_classified_at: None,
            classification_count: 0,
            last_verdict: None,
        };

        self.active.write().await.insert(session_id, tracked);
        if let Err(error) = self
            .persistence
            .update_status(session_id, SessionStatus::Starting)
            .await
        {
            tracing::warn!(
                %session_id,
                %error,
                "Continued provider is live but durable active status was not established"
            );
        }
        drop(cwd_admission_guard);
        #[cfg(test)]
        super::launch::pause_controller_candidate_test(
            session_id,
            super::launch::ControllerCandidateTestPhase::SameIdBeforeReconstruction,
        )
        .await;
        let controller_grant = Self::reconstruct_live_same_id_controller_grant(
            &self.active,
            &self.store,
            &self.agent_tokens,
            session_id,
            controller_project_id,
            provider,
            &session_token,
        )
        .await;
        if controller_grant == super::SameIdControllerGrantOutcome::EstablishmentInvalid {
            self.revoke_agent_token_for_session(session_id).await;
        }

        let fresh_receipt = if fresh_relaunch {
            let child = agent_child.as_ref().expect("fresh relaunch intent");
            let watch_rearmed = if child.manager_scope.is_some() {
                false
            } else {
                self.agent_control()
                    .rearm_child_watch_after_continue(
                        child.row.caller_session_id,
                        child.row.target_session_id,
                    )
                    .await
            };
            let receipt = AgentContinueChildResultV1 {
                target_session_id: child.row.target_session_id,
                continued_session_id: session_id,
                observed: child.observed,
                watch_rearmed,
                relaunch: Some(AgentContinueRelaunchV1 {
                    mode: "fresh".into(),
                    reason: "no_provider_session_id".into(),
                    request_id: child.row.request_id,
                    task_source_session_id: fresh_task_source.expect("fresh relaunch task source"),
                    invocation_id: admission_permit.invocation_id(),
                    deduplicated: false,
                    recovered: false,
                }),
            };
            self.store.lock().await.settle_child_relaunch_intent(
                child.row.request_id,
                RelaunchState::Launched,
                Some(admission_permit.invocation_id()),
                &serde_json::to_string(&receipt)?,
                None,
            )?;
            Some(receipt)
        } else {
            None
        };

        let store = Arc::clone(&self.store);
        let persistence = self.persistence.clone();
        let active = Arc::clone(&self.active);
        let completed = Arc::clone(&self.completed);
        let event_bus = Arc::clone(&self.event_bus);
        let context_rotation_enabled = self.context_rotation_enabled;
        let socket_path = self.socket_path.clone();
        let counter = Arc::clone(&self.token_counter);
        let memory_handle = self.memory_handle.clone();
        let retry_tx = self.retry_tx.clone();
        let runtime_config = Arc::clone(&self.runtime_config);
        let spawn_coordinator = Arc::clone(&self.spawn_coordinator);
        let agent_tokens = Arc::clone(&self.agent_tokens);
        let spawn_epoch = Arc::clone(&self.spawn_epoch);
        let agent_message_arbiter = Arc::clone(&self.agent_message_arbiter);
        let codegraph_handle = self.codegraph_handle.clone();
        let custody_runtime = self.custody_execution_runtime();
        let invocation_id = admission_permit.invocation_id();

        tokio::spawn(async move {
            if let Err(e) = persistence
                .update_status(session_id, SessionStatus::Starting)
                .await
            {
                tracing::warn!(error = %e, session_id = %session_id, "Failed to persist continue-session status");
            }
            if !fresh_relaunch {
                let store_ref = store.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || {
                    let store = store_ref.blocking_lock();
                    store.set_session_model_invocation(session_id, Some(invocation_id))
                })
                .await
                {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "Failed to bind continued session row to active model invocation"
                    );
                }
            }

            let user_event =
                Self::create_user_event(session_id, initial_sequence, &query_for_event);
            {
                let mut active_guard = active.write().await;
                if let Some(tracked) = active_guard.get_mut(&session_id) {
                    tracked.events.push(user_event.clone());
                }
            }
            match persistence.insert_event(user_event.clone()).await {
                Ok(db_id) => {
                    let mut active_guard = active.write().await;
                    if let Some(tracked) = active_guard.get_mut(&session_id)
                        && let Some(last_event) = tracked.events.last_mut()
                    {
                        last_event.id = db_id;
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, session_id = %session_id, "Failed to persist continue-session user event");
                }
            }

            event_bus.publish(DaemonEvent::SessionStatusChanged {
                session_id,
                old_status,
                new_status: SessionStatus::Starting,
            });
            event_bus.publish(DaemonEvent::ConversationEvent {
                session_id,
                event: user_event,
            });

            {
                let query_tokens = counter.count(&query_for_event);
                let mut active_guard = active.write().await;
                if let Some(tracked) = active_guard.get_mut(&session_id) {
                    tracked.daemon_input_tokens += query_tokens;
                    tracked.session.daemon_input_tokens = Some(tracked.daemon_input_tokens);
                }
            }

            let provider_session = Box::new(crate::provider::CliProviderSession::new(event_rx));
            // CLI continue sessions always use Single policy (one turn, no multi-turn continuation).
            let turn_controller = crate::turn_controller::TurnController::new(
                crate::turn_controller::ContinuationPolicy::Single,
            );
            // CLI sessions have no tool registry (tools run natively in the CLI subprocess).
            let tool_registry = std::sync::Arc::new(crate::tool_registry::ToolRegistry::new());
            Self::monitor_session(
                session_id,
                spawn_generation,
                provider_session,
                active,
                completed,
                event_bus,
                stop_rx,
                store,
                model_call_settlements,
                persistence,
                initial_sequence,
                context_rotation_enabled,
                socket_path,
                counter,
                memory_handle,
                retry_tx,
                tool_registry,
                turn_controller,
                runtime_config,
                spawn_coordinator,
                agent_tokens,
                spawn_epoch,
                agent_message_arbiter,
                codegraph_handle,
                custody_runtime,
            )
            .await;
        });

        Ok(match (capacity_delivery, fresh_receipt) {
            (_, Some(receipt)) => ContinueSessionOutcome::AgentFresh(receipt),
            (Some(_), None) => ContinueSessionOutcome::CapacityLaunchConfirmed {
                invocation_id: admission_permit.invocation_id(),
                recovered_admission: recovered_capacity_admission,
            },
            (None, None) => ContinueSessionOutcome::Started,
        })
    }

    /// Launch a fresh-UUID replacement when the effective synchronous provider
    /// differs from the durable provider identity of the completed row.
    async fn launch_effective_provider_replacement(
        &self,
        source: Session,
        query: String,
        cwd_admission_guard: super::spawn_single_flight::ProviderCwdAdmissionGuard,
    ) -> Result<()> {
        let source_id = source.id;
        let config = effective_provider_replacement_config(&source, query);

        let controller_idea = self
            .store
            .lock()
            .await
            .assigned_idea_for_session_v1(source_id)
            .map_err(|error| DaemonError::Store(error.to_string()))?;
        if let Some(idea) = controller_idea {
            let transfer = crate::idea_control::IdeaControllerTransferHandle::for_system(
                Arc::clone(&self.store),
                idea.project_id,
                idea.id,
                Arc::new(crate::idea_control::SystemIdeaControllerClock),
            )
            .await
            .map_err(|error| DaemonError::Store(error.to_string()))?;
            let reserved = transfer
                .reserve(&ReserveIdeaControllerRequestV1 {
                    expected_row_version: idea.row_version,
                    transfer_intent_key: format!(
                        "provider-replacement:{source_id}:{}",
                        idea.row_version
                    ),
                })
                .await
                .map_err(|error| DaemonError::Store(error.to_string()))?;
            let reservation = reserved.reservation.ok_or_else(|| {
                DaemonError::Store("controller reserve did not return a reservation".to_string())
            })?;
            self.launch_confirmed_controller_candidate(
                config,
                &transfer,
                reservation,
                None,
                Some(cwd_admission_guard),
            )
            .await?;
        } else {
            self.launch_session_with_retry_admission(
                config,
                None,
                false,
                super::types::LaunchPurpose::Interactive,
                Some(cwd_admission_guard),
            )
            .await?;
        }
        Ok(())
    }

    /// A5-P2: SIGKILL any OS subprocess still byte-exact-stamped
    /// `RSI_SESSION_ID=<session_id>`. Callers MUST have already established the
    /// session is not live/tracked (delete/purge both guard on `active` first),
    /// so this can only ever hit a leaked orphan — a finalized provider that
    /// survived teardown — never a process we still manage. The match is
    /// surgical (exact env, subprocess-only, self-skip, TOCTOU re-read), so it
    /// cannot touch the daemon or another session. Blocking `/proc` walk, run on
    /// a blocking thread. Inventory or proof failure refuses the destructive
    /// transition so ownership evidence is never erased prematurely.
    async fn reap_orphaned_subprocesses(&self, session_id: Uuid, context: &str) -> Result<()> {
        let reaped = tokio::task::spawn_blocking(move || {
            super::reaper::reap_orphans_for_session(session_id)
        })
        .await
        .map_err(|error| {
            DaemonError::Process(format!(
                "{context} orphan reap task failed for session {session_id}: {error}"
            ))
        })??;
        if reaped > 0 {
            self.event_bus.publish(DaemonEvent::SystemMessage {
                level: "warn".to_string(),
                message: format!(
                    "Reaped {reaped} orphaned provider process(es) for session {session_id} on {context}"
                ),
            });
        }
        Ok(())
    }

    /// Apply a terminal metadata transition to a selected row and all of its
    /// descendants. Container actions are leaves-first so every leaf runs the
    /// same active-session, retry, lead, and event settlement used by a
    /// standalone session before its parent is hidden. Sandboxes and custody
    /// are deliberately retained; their physical reclamation has a separate
    /// D00-governed lifecycle.
    async fn lifecycle_tree_ids(&self, session_id: Uuid) -> Result<Vec<Uuid>> {
        let store = self.store.clone();
        let mut ids = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.list_descendants(session_id)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))??
        .into_iter()
        .map(|session| session.id)
        .collect::<Vec<_>>();
        ids.reverse();
        ids.push(session_id);

        // Fail before mutating any member when a live descendant makes the
        // requested whole-container lifecycle transition ineligible.
        let active = self.active.read().await;
        if let Some(active_id) = ids.iter().find(|id| active.contains_key(id)) {
            return Err(DaemonError::Rpc(format!(
                "Cannot apply lifecycle action while descendant session {active_id} is active"
            )));
        }
        drop(active);

        Ok(ids)
    }

    pub async fn delete_session(&self, session_id: Uuid) -> Result<()> {
        for id in self.lifecycle_tree_ids(session_id).await? {
            self.delete_one_session(id).await?;
        }
        Ok(())
    }

    async fn delete_one_session(&self, session_id: Uuid) -> Result<()> {
        if self.active.read().await.contains_key(&session_id) {
            return Err(DaemonError::Rpc("Cannot delete active session".to_string()));
        }
        // A5-P2: the active-session guard above proves this session is not
        // tracked/live, so any
        // process still stamped `RSI_SESSION_ID=<id>` is an orphan. Reap it
        // before we tear the row down so a delete never leaves a subprocess
        // behind. Surgical env-match — cannot touch the daemon or a live session.
        self.reap_orphaned_subprocesses(session_id, "delete")
            .await?;
        self.clear_lead_pointers_to(session_id).await?;
        self.suppress_c5_autofile_pending(session_id, None).await?;
        // Cancel any pending retry before soft-deleting
        if let Some(mut cs) = self.completed.write().await.remove(&session_id) {
            if let Some(cancel) = cs.retry_cancel.take() {
                let _ = cancel.send(());
            }
        }
        self.persistence.soft_delete_session(session_id).await?;
        self.event_bus
            .publish(DaemonEvent::SessionDeleted { session_id });
        Ok(())
    }

    /// Permanently hard-delete a session and all its data from the database.
    /// Only use this to purge a session from the trash.
    pub async fn purge_session(&self, session_id: Uuid) -> Result<()> {
        if self.active.read().await.contains_key(&session_id) {
            return Err(DaemonError::Rpc("Cannot purge active session".to_string()));
        }
        if let CleanupDecision::Blocked(reason) =
            self.cleanup_decision_for_session_id(session_id).await
        {
            return Err(cleanup_policy_denied(reason));
        }
        // A5-P2: reap any leaked orphan subprocess before the hard-delete removes
        // the row — after the purge we could no longer even identify it. Same
        // surgical, non-active-only guarantee as `delete_session`.
        self.reap_orphaned_subprocesses(session_id, "purge").await?;
        self.clear_lead_pointers_to(session_id).await?;
        self.suppress_c5_autofile_pending(session_id, None).await?;
        self.completed.write().await.remove(&session_id);
        self.persistence.purge_session(session_id).await?;
        self.event_bus
            .publish(DaemonEvent::SessionDeleted { session_id });
        Ok(())
    }

    pub async fn mark_pending_archive(&self, session_id: Uuid, pending: bool) -> Result<()> {
        if pending {
            if !self.active.read().await.contains_key(&session_id) {
                return Err(DaemonError::Rpc(format!(
                    "Session {} not found in active sessions",
                    session_id
                )));
            }
        }
        let mut active = self.active.write().await;
        let tracked = active.get_mut(&session_id).ok_or_else(|| {
            DaemonError::Rpc(format!(
                "Session {} not found in active sessions",
                session_id
            ))
        })?;
        tracked.pending_archive = pending;
        tracked.session.pending_archive = pending;
        drop(active);
        self.persistence
            .update_pending_archive(session_id, pending)
            .await?;
        Ok(())
    }

    pub async fn archive_session(&self, session_id: Uuid) -> Result<ArchiveSessionResultV1> {
        // The private proof service owns the only positive destructive path.
        // Its read-only structural classifier returns `None` for every shape
        // that must remain on the unchanged generic D00 lifecycle policy.
        if let Some(result) = self.try_archive_cleanup(session_id).await? {
            return Ok(result);
        }
        let lifecycle_ids = self.lifecycle_tree_ids(session_id).await?;
        for id in lifecycle_ids {
            self.archive_one_session(id).await?;
        }
        Ok(ArchiveSessionResultV1::no_cleanup_required())
    }

    async fn archive_one_session(&self, session_id: Uuid) -> Result<()> {
        if self.active.read().await.contains_key(&session_id) {
            return Err(DaemonError::Rpc(
                "Cannot archive active session".to_string(),
            ));
        }
        self.clear_lead_pointers_to(session_id).await?;
        // This acknowledged Store transition is the durable settlement point;
        // do not evict/cancel the completed retry until it commits.
        let mut completed = self.completed.write().await;
        self.persistence
            .archive_and_resolve_autofile(session_id)
            .await?;
        evict_archived_completed(&mut completed, session_id);
        drop(completed);
        self.publish_logical_archive_tail(session_id);
        Ok(())
    }

    /// The post-commit logical archive tail shared by operator
    /// `archive_one_session` and `AgentArchiveChild`: publish
    /// `SessionArchived` and schedule the memory sync. It never removes a
    /// sandbox and never enters the cleanup saga.
    pub(super) fn publish_logical_archive_tail(&self, session_id: Uuid) {
        self.event_bus.publish(DaemonEvent::SessionArchived {
            session_id,
            projection_id: None,
        });
        if let Some(ref handle) = self.memory_handle {
            let h = handle.clone();
            let reason = format!("session_archived:{}", session_id);
            tokio::spawn(async move {
                if let Err(e) = h.sync_now(false, &reason).await {
                    tracing::warn!(error = %e, session_id = %session_id, "Memory sync after session archive failed");
                }
            });
        }
    }

    /// Settle the runtime side of a committed `AgentArchiveChild`: evict each
    /// archived row's completed entry, then run the shared logical tail.
    pub(super) async fn finish_agent_archive(&self, archived_session_ids: &[Uuid]) {
        let mut completed = self.completed.write().await;
        for id in archived_session_ids {
            evict_archived_completed(&mut completed, *id);
        }
        drop(completed);
        for id in archived_session_ids {
            self.publish_logical_archive_tail(*id);
        }
    }

    pub async fn toggle_pin(&self, session_id: Uuid) -> Result<Option<String>> {
        let result = self.persistence.toggle_pin(session_id).await?;
        let pinned_at = result.as_ref().and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        });
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.pinned_at = pinned_at;
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.pinned_at = pinned_at;
        }
        self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
            session_id,
            model: None,
            pinned_at: Some(result.clone()),
            project_id: None,
            parent_id: None,
            lead_session_id: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            resolved_context_budget: None,
        });
        Ok(result)
    }

    pub async fn toggle_testing_needed(&self, session_id: Uuid) -> Result<Option<String>> {
        let result = self.persistence.toggle_testing_needed(session_id).await?;
        let testing_needed_at = result.as_ref().and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        });
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.testing_needed_at = testing_needed_at;
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.testing_needed_at = testing_needed_at;
        }
        self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
            session_id,
            model: None,
            pinned_at: None,
            project_id: None,
            parent_id: None,
            lead_session_id: None,
            testing_needed_at: Some(result.clone()),
            rotation_disabled_at: None,
            resolved_context_budget: None,
        });
        Ok(result)
    }

    pub async fn toggle_rotation_disabled(&self, session_id: Uuid) -> Result<Option<String>> {
        let result = self
            .persistence
            .toggle_rotation_disabled(session_id)
            .await?;
        let rotation_disabled_at = result.as_ref().and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        });
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.rotation_disabled_at = rotation_disabled_at;
            // Update the coordinator's enabled flag in real-time
            tracked
                .rotation
                .set_enabled(rotation_disabled_at.is_none() && self.context_rotation_enabled);
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.rotation_disabled_at = rotation_disabled_at;
        }
        self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
            session_id,
            model: None,
            pinned_at: None,
            project_id: None,
            parent_id: None,
            lead_session_id: None,
            testing_needed_at: None,
            rotation_disabled_at: Some(result.clone()),
            resolved_context_budget: None,
        });
        Ok(result)
    }

    pub async fn update_session_project(
        &self,
        session_id: Uuid,
        project_id: Option<Uuid>,
    ) -> Result<()> {
        self.persistence
            .update_session_project(session_id, project_id)
            .await?;
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.project_id = project_id;
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.project_id = project_id;
        }
        self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
            session_id,
            model: None,
            pinned_at: None,
            project_id: Some(project_id),
            parent_id: None,
            lead_session_id: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            resolved_context_budget: None,
        });
        super::schedule_memory_sync(
            self.memory_handle.clone(),
            format!("session_project_changed:{session_id}"),
        );
        Ok(())
    }

    pub async fn update_session_workflow(
        &self,
        session_id: Uuid,
        workflow_id: Option<Uuid>,
    ) -> Result<()> {
        self.persistence
            .update_session_workflow(session_id, workflow_id)
            .await?;
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.workflow_id = workflow_id;
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.workflow_id = workflow_id;
        }
        self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
            session_id,
            model: None,
            pinned_at: None,
            project_id: None,
            parent_id: None,
            lead_session_id: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            resolved_context_budget: None,
        });
        Ok(())
    }

    pub async fn update_session_title(&self, session_id: Uuid, title: String) -> Result<()> {
        self.persistence
            .update_session_title(session_id, title.clone())
            .await?;
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.title = Some(title.clone());
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.title = Some(title);
        }
        Ok(())
    }

    pub async fn update_session_description(
        &self,
        session_id: Uuid,
        description: String,
    ) -> Result<()> {
        self.persistence
            .update_session_description(session_id, description.clone())
            .await?;
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.description = Some(description.clone());
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.description = Some(description);
        }
        Ok(())
    }

    pub async fn update_session_rating(&self, session_id: Uuid, rating: Option<i16>) -> Result<()> {
        self.persistence
            .update_session_rating(session_id, rating)
            .await?;
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.rating = rating;
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.rating = rating;
        }
        Ok(())
    }

    pub async fn update_active_task(
        &self,
        session_id: Uuid,
        active_task: Option<String>,
    ) -> Result<()> {
        self.persistence
            .update_session_active_task(session_id, active_task.clone())
            .await?;
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.active_task = active_task.clone();
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.active_task = active_task;
        }
        Ok(())
    }

    async fn suppress_c5_autofile_pending(
        &self,
        session_id: Uuid,
        max_retries: Option<u8>,
    ) -> Result<()> {
        // The completed-map writer is also the retry-admission rollback
        // serialization point. Hold it until durable user intent commits and
        // any timer owner has been made cancellable/consumed.
        let mut completed = self.completed.write().await;
        let retry_max = max_retries
            .or_else(|| {
                completed
                    .get(&session_id)
                    .and_then(|cs| cs.session.max_retries)
            })
            .filter(|value| *value > 0);
        if let Some(max_retries) = retry_max {
            self.store
                .lock()
                .await
                .suppress_c5_autofile_pending_and_exhaust_retry(session_id, max_retries)?;
            if let Some(completed_session) = completed.get_mut(&session_id) {
                if let Some(cancel) = completed_session.retry_cancel.take() {
                    let _ = cancel.send(());
                }
                completed_session.retry_fired_at = None;
                exhaust_retry_budget(&mut completed_session.session);
            }
            Ok(())
        } else {
            self.store.lock().await.resolve_c5_autofile_pending(
                &crate::store::daemon_settings::c5_autofile_pending_key(session_id),
            )
        }
    }

    /// Operator RPC entry point; pause ownership is not inferred from internal
    /// shutdown, stall recovery, graph cancellation, or AgentHalt calls.
    pub async fn interrupt_session_operator(&self, session_id: Uuid) -> Result<()> {
        // Publish intent before waiting, so an in-flight replacement refuses
        // its final CAS. Then acquire the real spawn guard so a continuation
        // cannot install a process just after an apparently successful pause.
        self.store
            .lock()
            .await
            .record_manager_operator_pause(session_id, true)?;
        let _guard = super::spawn_single_flight::acquire_spawn_guard(session_id).await;
        self.interrupt_session(session_id).await
    }

    pub async fn interrupt_session(&self, session_id: Uuid) -> Result<()> {
        if interrupt_active_in_maps(&self.active, session_id).await? {
            return Ok(());
        }
        let Some(_max_retries) =
            suppress_pending_retry_in_maps(&self.completed, &self.store, session_id).await?
        else {
            // Active interruption has no C5 suppression; invalid and
            // non-pending targets preserve their marker and existing error.
            return interrupt_in_maps(&self.active, &self.completed, session_id)
                .await
                .map(|_| ());
        };
        Ok(())
    }

    /// Interrupt a session if it was stalled and has retries configured.
    /// Sets the stall_interrupted flag so the break reason is overridden to StallTimeout.
    /// Returns true if an interrupt was issued, false if the session has no retries or was not found.
    pub async fn interrupt_if_stall_retryable(&self, session_id: Uuid) -> bool {
        // Check if session has retries configured
        let has_retries = {
            let active = self.active.read().await;
            active.get(&session_id).map_or(false, |t| {
                super::retry_policy::session_retries_allowed(
                    &self.runtime_config,
                    t.session.session_kind,
                    Some(t.max_retries),
                )
            })
        };

        if !has_retries {
            return false;
        }

        // Set the stall_interrupted flag BEFORE issuing the interrupt
        {
            let mut active = self.active.write().await;
            if let Some(tracked) = active.get_mut(&session_id) {
                tracked.stall_interrupted = true;
            }
        }

        match self.interrupt_session(session_id).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "Stall-triggered interrupt failed"
                );
                false
            }
        }
    }

    /// Cancel a pending retry for a completed session.
    /// Returns true if a retry was actually cancelled, false if no retry was pending.
    pub async fn cancel_retry(&self, session_id: Uuid) -> Result<bool> {
        let cancelled = suppress_pending_retry_in_maps(&self.completed, &self.store, session_id)
            .await?
            .is_some();
        if cancelled {
            tracing::info!(session_id = %session_id, "Cancelled pending retry via CancelRetry RPC");
        }
        Ok(cancelled)
    }

    /// Re-launch a failed session as a fresh retry (not --resume).
    /// Called by the daemon's retry handler loop when a retry timer fires.
    pub async fn launch_retry(&self, session_id: Uuid) -> Result<()> {
        let spawn_guard = super::spawn_single_flight::acquire_spawn_guard(session_id).await;
        if spawn_guard.contended()
            && super::spawn_single_flight::adopt_if_live(&self.active, session_id).await
        {
            self.event_bus.publish(DaemonEvent::SessionSpawnDeduped {
                session_id,
                source: "launch_retry".to_string(),
            });
            return Ok(());
        }

        let retry_visibility_snapshot = {
            let mut completed = self.completed.write().await;
            match completed.get_mut(&session_id) {
                Some(cs) => {
                    cs.retry_cancel = None;
                    if let Some(fired_at) = cs.retry_fired_at.as_ref() {
                        tracing::info!(
                            session_id = %session_id,
                            fire_to_consume_ms = fired_at.elapsed().as_millis(),
                            "Retry fire consumed by launch_retry"
                        );
                    }
                    cs.session.clone()
                }
                None => {
                    tracing::info!(session_id = %session_id, "Retry: session no longer in completed map");
                    return Ok(());
                }
            }
        };
        // A fired retry retains its completed-map entry while the provider is
        // being established and Store admission is pending.  This is the
        // observable recovery owner used by replay and user-intent paths.

        if retry_visibility_snapshot.status != SessionStatus::Failed {
            tracing::info!(
                session_id = %session_id,
                status = ?retry_visibility_snapshot.status,
                "Retry skipped: session not in Failed status"
            );
            return Ok(());
        }

        let retry_source = {
            let store = self.store.lock().await;
            match store.get_session(session_id) {
                Ok(Some(row)) => {
                    let row_attempt = row.retry_attempt.unwrap_or(0);
                    let row_max = row.max_retries.unwrap_or(0);
                    if row.status == SessionStatus::Failed && row_max > 0 && row_attempt < row_max {
                        Some(row)
                    } else {
                        None
                    }
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "Retry skipped: failed to re-read durable session row"
                    );
                    None
                }
            }
        };

        let Some(retry_source) = retry_source else {
            tracing::info!(
                session_id = %session_id,
                "Retry skipped: durable session row is no longer retry-eligible"
            );
            return Ok(());
        };

        // A retry is admissible only while the exact C5 marker observed here
        // remains live.  The Store transaction compares this typed witness,
        // so a replay or user-intent settlement between this read and provider
        // spawn rejects the child rather than resurrecting the parent.
        let pending_marker = {
            let store = self.store.lock().await;
            match store.get_c5_autofile_pending(
                &crate::store::daemon_settings::c5_autofile_pending_key(session_id),
            ) {
                Ok(Some(marker)) => marker,
                Ok(None) => {
                    return Ok(());
                }
                Err(error) => {
                    return Err(error);
                }
            }
        };

        // The completed-map row owns visibility/timer serialization only. The
        // complete durable Failed row and its exact ordinary/live custody must
        // authenticate before any successor UUID, controller reservation,
        // token, model admission, context, provider, or target effect exists.
        let custody_candidate = self
            .custody_execution_runtime()
            .prepare_retry_successor(&retry_source)
            .await?;

        let retry_attempt = retry_source.retry_attempt.unwrap_or(1);
        let max_retries = retry_source.max_retries.unwrap_or(0);

        tracing::info!(
            session_id = %session_id,
            attempt = retry_attempt,
            max_retries = max_retries,
            "Executing retry — fresh launch"
        );

        // Raw retry config is deliberately non-authoritative. The private
        // RetryAdmission candidate applies custody after C5; no allocator or
        // canonical-HEAD path is reachable from this boundary.
        let config = LaunchConfig {
            query: retry_source.query.clone(),
            title: None,
            agent_role: retry_source.agent_role.clone(),
            epic_spawn_ordinal: retry_source.epic_spawn_ordinal,
            working_dir: Some(retry_source.working_dir.clone()),
            provider: Some(retry_source.provider),
            model: retry_source.model.clone(),
            configured_context_window: retry_source
                .resolved_context_budget
                .as_ref()
                .and_then(|budget| budget.capacity.configured_tokens),
            max_turns: None,
            system_prompt: None,     // Will be re-composed by launch_session
            resume_session_id: None, // Fresh start
            session_kind: Some(retry_source.session_kind),
            project_id: retry_source.project_id,
            rsi_session_id: None,
            rsi_socket: None,
            // A6: intentionally None — NOT a token gap. This retry config
            // re-enters `launch_session` below, which unconditionally
            // re-mints and registers a fresh token for the NEW retry session
            // id (fresh mint applies; plan §2.3).
            rsi_session_token: None,
            continued_from: Some(session_id), // Link to original session
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: retry_source.workflow_id,
            workflow_id_override: retry_source.workflow_id_override,
            max_retries: Some(max_retries),
            group_id: retry_source.group_id,
            skip_project_model_default: false,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::SessionRetryAuto,
            parent_id: retry_source.parent_id,
            effort: retry_source.effort.clone(),
            issue_identifier: retry_source.issue_identifier.clone(),
            issue_url: retry_source.issue_url.clone(),
            issue_tracker_id: retry_source.issue_tracker_id.clone(),
            scheduled_job_id: None,
            sandbox: None,
            cargo_target_dir: None,
            execution_scratch: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: Some(format!(
                "{}:{session_id}:{retry_attempt}",
                rsi_common::model_control::ModelInvocationPurpose::SessionRetryAuto
            )),
            model_invocation_request_fingerprint: Some(hash_request_fingerprint(&[
                rsi_common::model_control::ModelInvocationPurpose::SessionRetryAuto.as_str(),
                &format!("{:?}", retry_source.provider),
                retry_source.model.as_deref().unwrap_or(""),
                &retry_source.query,
                &retry_attempt.to_string(),
            ])),
            // RSI-006: retry inherits the parent's eval status. Eval replays
            // also keep the hermetic ContextPipeline bypass on retry so the
            // hash stays stable across the retry.
            is_eval: retry_source.is_eval,
            skip_context_pipeline: retry_source.is_eval,
            // Retry inherits the original session's declared capability class.
            capability_class: retry_source.capability_class,
            // Retry inherits tags from the original session.
            tags: retry_source.tags.clone(),
            // P1.7: retry is not a new topology node spawn — no binding inherited.
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        };

        // Launch as a new session (new UUID), linked via continued_from.  The
        // child row, its retry attempt, the exhausted parent, and the parent
        // marker are committed before `launch_retry_successor` inserts it into
        // active or starts monitor/event exposure.
        let retry_admission = RetryAdmission::new(
            session_id,
            retry_attempt,
            max_retries,
            pending_marker,
            retry_source,
            custody_candidate,
        );
        // This is a read-only hint before controller/model admission. The C5
        // transaction repeats the predicate atomically if the gate changes.
        // A persistent Prepared gate must not create an invocation per timer.
        let prepared = self.store.lock().await.prepared_reclaim_for_owner_tuple(
            session_id,
            retry_admission.source().sandbox_root.as_deref(),
            retry_admission.source().sandbox_branch.as_deref(),
        );
        match prepared {
            Ok(false) => {}
            Ok(true) => {
                retry_admission.mark_attempted();
                retry_admission.mark_failed(true);
                retry_admission.mark_reclaim_prepared();
                self.rearm_retry_after_retryable_admission(&retry_admission)
                    .await;
                return Err(crate::sandbox::custody::CustodyService::refusal(
                    rsi_common::types::SandboxCustodyErrorCodeV1::ReclaimPrepared,
                    Some(session_id),
                    rsi_common::types::SandboxCustodyTransitionV1::Retry,
                ));
            }
            Err(error) => {
                retry_admission.mark_attempted();
                retry_admission.mark_failed(true);
                self.rearm_retry_after_retryable_admission(&retry_admission)
                    .await;
                return Err(DaemonError::Store(error.to_string()));
            }
        }
        let controller_idea = {
            let store = self.store.lock().await;
            store
                .assigned_idea_for_session_v1(session_id)
                .map_err(|error| DaemonError::Store(error.to_string()))?
        };
        let launch_result = if let Some(idea) = controller_idea {
            let transfer = crate::idea_control::IdeaControllerTransferHandle::for_system(
                Arc::clone(&self.store),
                idea.project_id,
                idea.id,
                Arc::new(crate::idea_control::SystemIdeaControllerClock),
            )
            .await
            .map_err(|error| DaemonError::Store(error.to_string()))?;
            let reserved = transfer
                .reserve(&ReserveIdeaControllerRequestV1 {
                    expected_row_version: idea.row_version,
                    transfer_intent_key: format!("retry:{session_id}:{retry_attempt}"),
                })
                .await
                .map_err(|error| DaemonError::Store(error.to_string()))?;
            let reservation = reserved.reservation.ok_or_else(|| {
                DaemonError::Store("controller reserve did not return a reservation".to_string())
            })?;
            let candidate_session_id = reservation.candidate_session_id;
            self.launch_confirmed_controller_candidate(
                config,
                &transfer,
                reservation,
                Some(retry_admission.clone()),
                None,
            )
            .await
            .map(|_| candidate_session_id)
        } else {
            self.launch_retry_successor(config, retry_admission.clone())
                .await
        };
        match launch_result {
            Ok(new_session_id) => {
                tracing::info!(
                    original_session_id = %session_id,
                    new_session_id = %new_session_id,
                    attempt = retry_attempt,
                    "Retry launched as new session"
                );

                // The parent stayed observable during spawn/admission.  Only
                // after the atomic Store commit may it become superseded.
                if let Some(completed_session) = self.completed.write().await.get_mut(&session_id) {
                    completed_session.session.retry_attempt = Some(max_retries);
                    completed_session.session.max_retries = Some(max_retries);
                    completed_session.retry_cancel = None;
                    completed_session.retry_fired_at = None;
                    completed_session.superseded_by_retry = Some(new_session_id);
                }

                Ok(())
            }
            Err(e) => {
                if retry_admission.retryable_failure_before_commit() {
                    // The completed-map writer serializes every retry
                    // suppression.  Keep it through the durable re-read and
                    // installation of the cancellation owner, so a user
                    // suppression either wins before the proof or observes a
                    // cancellable timer after it -- never a stale rearm gap.
                    self.rearm_retry_after_retryable_admission(&retry_admission)
                        .await;
                    return Err(e);
                }
                if retry_admission.failed_before_commit() {
                    // A compare conflict means replay or explicit user intent
                    // already settled the parent. The spawned provider was
                    // killed by launch_retry_successor; do not rearm or file.
                    return Err(e);
                }
                tracing::error!(
                    error = %e,
                    session_id = %session_id,
                    "Retry launch failed"
                );
                // Recovery has now failed to launch and no timer is live; this
                // is a settled daemon-owned failure, not a user cancellation.
                self.agent_control()
                    .maybe_autofile_terminal_failure(
                        session_id,
                        RecoveryDisposition::RetryLaunchFailed,
                    )
                    .await;
                Err(e)
            }
        }
    }

    /// Restore a completed-map/timer owner after an attempted retry admission
    /// rolls back. The Store transaction left the parent retryable and its
    /// marker live, so this timer is again the sole in-memory owner.
    fn prepare_retry_after_failed_admission(
        &self,
        completed_session: &mut CompletedSession,
    ) -> (u64, tokio::sync::oneshot::Receiver<()>) {
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        completed_session.retry_cancel = Some(cancel_tx);
        completed_session.retry_fired_at = None;
        let attempt = completed_session.session.retry_attempt.unwrap_or(1);
        let max_backoff = self
            .runtime_config
            .retry_max_backoff_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let delay = crate::session::monitor::backoff_ms(attempt, max_backoff);
        (delay, cancel_rx)
    }

    pub(super) async fn rearm_retry_after_retryable_admission(&self, admission: &RetryAdmission) {
        let session_id = admission.parent_session_id;
        let mut completed = self.completed.write().await;
        let Some(completed_session) = completed.get_mut(&session_id) else {
            return;
        };

        // This lock order intentionally matches every pending-retry user
        // suppression: completed map first, Store second.  Do not move this
        // durable proof outside the map guard; doing so revives the old gap
        // where a suppression could commit before the new timer existed.
        let can_rearm = {
            let store = self.store.lock().await;
            matches!(
                store.get_session(session_id),
                Ok(Some(row))
                    if row.status == SessionStatus::Failed
                        && row.retry_attempt == Some(admission.retry_attempt)
                        && row.max_retries == Some(admission.max_retries)
            ) && matches!(
                store.get_c5_autofile_pending(
                    &crate::store::daemon_settings::c5_autofile_pending_key(session_id)
                ),
                Ok(Some(marker)) if marker == admission.pending_marker
            )
        };
        if !can_rearm {
            return;
        }

        let (delay, cancel_rx) = self.prepare_retry_after_failed_admission(completed_session);
        // Prepared is a transient custody gate with its own one-second
        // recovery cadence. Preserve the retry owner and marker while the
        // gate is live, then retry promptly after it releases.
        let delay = if admission.reclaim_prepared_failure() {
            1_000
        } else {
            delay
        };
        drop(completed);
        crate::session::monitor::spawn_retry_timer(
            Arc::clone(&self.completed),
            self.retry_tx.clone(),
            session_id,
            delay,
            cancel_rx,
            "Retry admission rollback timer cancelled",
        );
    }

    /// Classify a caller-supplied candidate through the shared D00 boundary.
    pub(super) async fn classify_cleanup_candidate(
        &self,
        candidate: CleanupCandidate,
    ) -> CleanupDecision {
        classify_cleanup_candidate_runtime(candidate, &self.active, &self.completed, &self.store)
            .await
    }

    /// Resolve a session through active → completed → store without collapsing
    /// missing or unreadable ownership into a no-sandbox result.
    pub(super) async fn cleanup_decision_for_session_id(
        &self,
        session_id: Uuid,
    ) -> CleanupDecision {
        classify_cleanup_session_runtime(session_id, &self.active, &self.completed, &self.store)
            .await
    }

    pub async fn shutdown(&self) -> Result<()> {
        let session_ids: Vec<Uuid> = self.active.read().await.keys().copied().collect();
        for session_id in session_ids {
            let _ = self.interrupt_session(session_id).await;
        }
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);
        while !self.active.read().await.is_empty() && start.elapsed() < timeout {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let mut active = self.active.write().await;
        for tracked in active.values_mut() {
            if let Some(ref mut process) = tracked.process {
                let _ = process.kill().await;
            }
        }
        active.clear();
        drop(active);
        let mut custody_attempt = 0_u64;
        loop {
            custody_attempt = custody_attempt.saturating_add(1);
            match self
                .custody_settlement_worker
                .shutdown(&self.custody_settlements)
                .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::error!(
                        attempt = custody_attempt,
                        error = %error,
                        "Custody effect settlement shutdown incomplete; daemon remains in shutdown recovery"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
        let mut settlement_attempt = 0_u64;
        loop {
            settlement_attempt = settlement_attempt.saturating_add(1);
            match self.model_call_settlements.shutdown().await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    // Settlement shutdown is fail-closed: a bounded worker
                    // attempt may time out, but production must not unwind the
                    // runtime and discard the still-owned recovery boundary.
                    // Remain visibly in failed-shutdown recovery and retry the
                    // same idempotent state machine until it converges.
                    tracing::error!(
                        attempt = settlement_attempt,
                        error = %error,
                        "Model-call settlement shutdown incomplete; daemon remains in shutdown recovery"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod d00_tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::sandbox::SandboxAllocator;
    use crate::store::Store;
    use crate::store::program_runs::{ProgramRunExternalReferenceV1, ProgramRunTransitionInputV1};
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
    use rsi_common::archive_cleanup::ArchivePreservationClassV1;
    use rsi_common::program_runs::{
        CreateProgramRunRequestV1, ProgramRunActionKindV1, ProgramRunBudgetLimitsV1,
        ProgramRunCursorV1, ProgramRunOperationV1, ProgramRunTemplateV1,
        ProgramRunTransitionRequestV1, ProgramRunV1,
    };
    use rsi_common::types::{
        AutonomyPolicy, Capture, CaptureSourceKind, ContentAddressedRef, Idea, IdeaActorKind,
        IdeaLifecycle, IdeaStage, Project, SandboxCleanupState, SandboxKind, Sha256Digest,
    };
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize};
    use tempfile::TempDir;

    struct TestManager {
        manager: SessionManager,
        _db: TempDir,
        sandbox_base: TempDir,
        repo: TempDir,
    }

    fn git(path: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output utf8")
    }

    fn test_manager() -> TestManager {
        let db = TempDir::new().expect("db tempdir");
        let sandbox_base = TempDir::new().expect("sandbox tempdir");
        let repo = TempDir::new().expect("repo tempdir");
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(
            repo.path(),
            &["config", "user.email", "d00@example.invalid"],
        );
        git(repo.path(), &["config", "user.name", "D00 Fixture"]);
        std::fs::write(repo.path().join("README.md"), "fixture\n").expect("write fixture");
        git(repo.path(), &["add", "README.md"]);
        git(repo.path(), &["commit", "-q", "-m", "initial"]);

        let store = Store::open(&db.path().join("rsi.db")).expect("open store");
        let manager = SessionManager::new(
            Arc::new(EventBus::new(64)),
            store,
            false,
            db.path().join("daemon.sock"),
            None,
            Vec::new(),
            RuntimeConfig::from_config(&Config::from_env()),
            sandbox_base.path().to_path_buf(),
        )
        .expect("manager");
        TestManager {
            manager,
            _db: db,
            sandbox_base,
            repo,
        }
    }

    fn session(session_id: Uuid, working_dir: &std::path::Path) -> Session {
        Session {
            context_fill_pct: None,
            id: session_id,
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "D00 lifecycle fixture".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: working_dir.to_path_buf(),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: SessionKind::Standard,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    async fn insert_completed_continue_fixture(
        test: &TestManager,
        sandboxed: bool,
    ) -> (Uuid, Option<PathBuf>) {
        let session_id = Uuid::new_v4();
        let mut completed = session(session_id, test.repo.path());
        completed.status = SessionStatus::Completed;
        completed.claude_session_id = Some(format!("continue-{session_id}"));
        let root = if sandboxed {
            let allocation = SandboxAllocator::new(test.sandbox_base.path().to_path_buf())
                .allocate(
                    session_id,
                    test.repo.path(),
                    SandboxKind::GitWorktree,
                    "HEAD",
                    None,
                )
                .expect("allocate live continue worktree");
            let branch = allocation.branch.expect("worktree branch");
            completed.sandbox_kind = Some(SandboxKind::GitWorktree);
            completed.sandbox_root = Some(allocation.root.clone());
            completed.sandbox_branch = Some(branch.clone());
            completed.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            completed.git_branch = Some(branch.clone());
            let common_dir = std::fs::canonicalize(
                git(
                    &allocation.root,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )
                .trim(),
            )
            .expect("canonical common git directory");
            let source_commit = git(&allocation.root, &["rev-parse", "HEAD"])
                .trim()
                .to_string();
            test.manager
                .store
                .lock()
                .await
                .insert_session_with_custody(
                    &completed,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id: Uuid::new_v4(),
                        canonical_repo_dir: completed.working_dir.display().to_string(),
                        sandbox_root: allocation.root.display().to_string(),
                        sandbox_branch: branch,
                        repository_identity: common_dir.display().to_string(),
                        source_commit,
                        cause: CustodyCause::FreshLaunch,
                    }),
                )
                .expect("insert completed live custody fixture");
            Some(allocation.root)
        } else {
            test.manager
                .store
                .lock()
                .await
                .insert_session(&completed)
                .expect("insert completed ordinary fixture");
            None
        };
        test.manager
            .completed
            .write()
            .await
            .insert(session_id, CompletedSession::for_test(completed));
        (session_id, root)
    }

    mod harness_manager_resume_tests {
        use super::*;
        use crate::session::harness::tools::schedule_wake::{
            ScheduleWakeRequest, build_agent_scheduled_job,
        };
        use rsi_common::types::{Approval, ApprovalStatus, PendingQuestion, QuestionItem};
        use std::future::Future;
        use std::sync::atomic::Ordering;
        use std::task::Poll;

        async fn snapshot(test: &TestManager, target: Uuid) -> serde_json::Value {
            let store = test.manager.store.lock().await;
            serde_json::json!({
                "session": store.get_session(target).unwrap(),
                "invocation": store.session_model_invocation_id(target).unwrap(),
                "events": store.load_events(target).unwrap(),
                "invocation_count": store.conn.query_row(
                    "SELECT COUNT(*) FROM model_invocations", [], |row| row.get::<_, i64>(0)
                ).unwrap(),
                "retry_marker": store.get_daemon_setting(
                    &crate::store::daemon_settings::c5_autofile_pending_key(target)
                ).unwrap(),
                "approvals": store.get_pending_approvals(target).unwrap(),
            })
        }

        fn assert_refusal(result: Result<Uuid>, code: &str) {
            assert!(
                matches!(result, Err(DaemonError::InvalidParam(ref message)) if message == code),
                "expected {code}, got {result:?}"
            );
        }

        async fn assert_preserved_refusal(
            test: &TestManager,
            target: Uuid,
            jobs: Vec<Uuid>,
            code: &str,
        ) {
            let before = snapshot(test, target).await;
            assert_refusal(
                test.manager
                    .resume_manager_notice(target, "Read the manager inbox.".into(), jobs)
                    .await,
                code,
            );
            assert_eq!(snapshot(test, target).await, before);
            assert!(test.manager.completed.read().await.contains_key(&target));
        }

        fn question() -> PendingQuestion {
            PendingQuestion {
                questions: vec![QuestionItem {
                    question: "Approve this operation?".into(),
                    header: "Approval".into(),
                    options: Vec::new(),
                    multi_select: false,
                }],
            }
        }

        fn wake(
            test: &TestManager,
            target: Uuid,
            watched: Option<Uuid>,
        ) -> rsi_common::types::ScheduledJob {
            build_agent_scheduled_job(ScheduleWakeRequest {
                message: "Read the manager inbox.".into(),
                in_seconds: watched.map(|_| 1),
                at: None,
                name: None,
                every_seconds: watched.map(|_| 60),
                mode: Some(
                    if watched.is_some() {
                        "on_terminal"
                    } else {
                        "program_guard"
                    }
                    .into(),
                ),
                working_dir: test.repo.path().to_path_buf(),
                provider: Some(SessionProvider::Claude),
                model: None,
                project_id: None,
                origin_session_id: Some(target),
                watch_session_id: watched,
            })
            .unwrap()
        }

        // Exercise the real Store authorization hook against the V102 records,
        // independently of the parent's scheduler/service fixture construction.
        async fn bind_notice(test: &TestManager, target: Uuid) -> Uuid {
            let project = d03_lifecycle_project();
            let mut group = session(Uuid::new_v4(), test.repo.path());
            group.session_kind = SessionKind::Group;
            group.project_id = Some(project.id);
            let mut epic = session(Uuid::new_v4(), test.repo.path());
            epic.session_kind = SessionKind::Epic;
            epic.parent_id = Some(group.id);
            epic.project_id = Some(project.id);
            epic.status = SessionStatus::Completed;
            let mut lead = session(Uuid::new_v4(), test.repo.path());
            lead.parent_id = Some(epic.id);
            lead.project_id = Some(project.id);
            lead.session_kind = SessionKind::Task;
            lead.status = SessionStatus::Completed;
            let mut job = wake(test, target, Some(lead.id));
            job.project_id = Some(project.id);
            // Manager notices bind the existing recipient and carry no launch
            // overrides; provider/cwd custody belongs to its continuation.
            job.working_dir = None;
            job.provider = None;
            job.model = None;
            let store = test.manager.store.lock().await;
            store.insert_project(&project).unwrap();
            store.insert_session(&group).unwrap();
            store.insert_session(&epic).unwrap();
            store.insert_session(&lead).unwrap();
            store.set_lead_session(epic.id, Some(lead.id)).unwrap();
            store
                .conn
                .execute(
                    "UPDATE sessions SET project_id=?2 WHERE id=?1",
                    rusqlite::params![target.to_string(), project.id.to_string()],
                )
                .unwrap();
            store.conn.execute(
                "INSERT INTO harness_manager_scopes(project_id,manager_session_id,epic_ids_json,row_version,updated_at)
                 VALUES(?1,?2,?3,1,?4)",
                rusqlite::params![project.id.to_string(), target.to_string(),
                    serde_json::to_string(&vec![epic.id]).unwrap(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)],
            ).unwrap();
            store.insert_scheduled_job(&job).unwrap();
            store.conn.execute(
                "INSERT INTO harness_manager_watches(job_id,project_id,epic_id,scope_version,direction,
                    source_session_id,target_session_id,attention_signature)
                 VALUES(?1,?2,?3,1,'to_manager',?4,?5,'completed')",
                rusqlite::params![job.id.to_string(), project.id.to_string(), epic.id.to_string(),
                    lead.id.to_string(), target.to_string()],
            ).unwrap();
            assert!(
                store
                    .harness_manager_wake_authorized(job.id, target)
                    .unwrap()
            );
            drop(store);
            test.manager
                .completed
                .write()
                .await
                .get_mut(&target)
                .unwrap()
                .session
                .project_id = Some(project.id);
            job.id
        }

        fn insert_text(store: &Store, target: Uuid, sequence: i32, role: Role, content: String) {
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id: target,
                    sequence,
                    event_type: EventType::Message,
                    role: Some(role),
                    content,
                    tool_name: None,
                    tool_input: None,
                    tool_use_id: None,
                    offload_id: None,
                    metadata: None,
                    created_at: chrono::Utc::now(),
                })
                .unwrap();
        }

        #[tokio::test]
        async fn manager_notice_rechecks_active_turn_after_waiting_for_spawn_guard() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let job = bind_notice(&test, target).await;
            let guard = crate::session::spawn_single_flight::acquire_spawn_guard(target).await;
            let resume =
                test.manager
                    .resume_manager_notice(target, "Inbox notice".into(), vec![job]);
            tokio::pin!(resume);
            // Poll deterministically into the held guard; no scheduler sleeps.
            std::future::poll_fn(|cx| {
                assert!(resume.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let mut row = test
                .manager
                .store
                .lock()
                .await
                .get_session(target)
                .unwrap()
                .unwrap();
            row.status = SessionStatus::Running;
            let alive = Arc::new(AtomicBool::new(true));
            let mut tracked = d03_live_tracked_session(row, alive.clone());
            tracked.spawn_generation = 41;
            test.manager.active.write().await.insert(target, tracked);
            let invocation = Uuid::new_v4();
            {
                let store = test.manager.store.lock().await;
                store.conn.execute(
                    "INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,
                        admission_status,status,trigger_source,session_id,policy_snapshot_json,
                        usage_confidence,created_at)
                     VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                        'admitted','running','manager-notice-race',?2,'{}','unavailable',?3)",
                    rusqlite::params![invocation.to_string(), target.to_string(),
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)],
                ).unwrap();
                store
                    .set_session_model_invocation(target, Some(invocation))
                    .unwrap();
            }
            let before = snapshot(&test, target).await;
            drop(guard);
            assert_refusal(resume.await, "manager_notice_deferred");
            let active = test.manager.active.read().await;
            let tracked = active.get(&target).unwrap();
            assert_eq!(tracked.spawn_generation, 41);
            assert!(!tracked.interrupt_requested);
            assert!(alive.load(Ordering::SeqCst));
            if let Some(crate::session::types::ProviderProcess::Scripted(process)) =
                &tracked.process
            {
                assert_eq!(process.interrupt_count.load(Ordering::SeqCst), 0);
                assert_eq!(process.kill_count.load(Ordering::SeqCst), 0);
            } else {
                panic!("original scripted provider must remain installed");
            }
            drop(active);
            assert_eq!(snapshot(&test, target).await, before);
            assert!(test.manager.completed.read().await.contains_key(&target));
        }

        #[tokio::test]
        async fn manager_notice_preserves_persisted_questions_even_with_stale_completed_cache() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            for raw in [
                serde_json::to_string(&question()).unwrap(),
                "malformed question".into(),
            ] {
                test.manager
                    .store
                    .lock()
                    .await
                    .update_session_pending_question_json(target, Some(&raw))
                    .unwrap();
                assert_preserved_refusal(
                    &test,
                    target,
                    vec![Uuid::new_v4()],
                    "manager_notice_deferred",
                )
                .await;
                let stored: String = test
                    .manager
                    .store
                    .lock()
                    .await
                    .conn
                    .query_row(
                        "SELECT pending_question_json FROM sessions WHERE id=?1",
                        [target.to_string()],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(stored, raw);
            }
        }

        #[tokio::test]
        async fn manager_notice_waiting_approval_in_completed_map_remains_parked() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let pending = question();
            {
                let mut completed = test.manager.completed.write().await;
                let cached = completed.get_mut(&target).unwrap();
                cached.session.status = SessionStatus::WaitingApproval;
                cached.session.pending_question = Some(pending.clone());
            }
            test.manager
                .store
                .lock()
                .await
                .update_session_status(target, SessionStatus::WaitingApproval)
                .unwrap();
            assert_preserved_refusal(
                &test,
                target,
                vec![Uuid::new_v4()],
                "manager_notice_deferred",
            )
            .await;
            let completed = test.manager.completed.read().await;
            assert_eq!(
                completed[&target].session.status,
                SessionStatus::WaitingApproval
            );
            assert_eq!(completed[&target].session.pending_question, Some(pending));
        }

        #[tokio::test]
        async fn manager_notice_preserves_unresolved_approval_on_completed_session() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let approval = Approval {
                id: Uuid::new_v4(),
                session_id: target,
                tool_name: "operator decision".into(),
                tool_input: serde_json::json!({"operation": "deploy"}),
                status: ApprovalStatus::Pending,
                created_at: chrono::Utc::now(),
                resolved_at: None,
            };
            test.manager
                .store
                .lock()
                .await
                .insert_approval(&approval)
                .unwrap();
            assert_preserved_refusal(
                &test,
                target,
                vec![Uuid::new_v4()],
                "manager_notice_deferred",
            )
            .await;
            assert_eq!(
                test.manager
                    .store
                    .lock()
                    .await
                    .get_pending_approvals(target)
                    .unwrap()[0]
                    .id,
                approval.id
            );
        }

        #[tokio::test]
        async fn manager_notice_preserves_retry_timer_queue_and_durable_recovery_owner() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let (cancel, mut cancel_rx) = tokio::sync::oneshot::channel();
            {
                let mut completed = test.manager.completed.write().await;
                let cached = completed.get_mut(&target).unwrap();
                cached.retry_cancel = Some(cancel);
                cached.retry_fired_at = Some(std::time::Instant::now());
                cached.superseded_by_retry = Some(Uuid::new_v4());
            }
            assert_preserved_refusal(
                &test,
                target,
                vec![Uuid::new_v4()],
                "manager_notice_deferred",
            )
            .await;
            assert!(matches!(
                cancel_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            {
                let mut completed = test.manager.completed.write().await;
                let cached = completed.get_mut(&target).unwrap();
                assert!(cached.retry_cancel.is_some());
                assert!(cached.retry_fired_at.is_some());
                assert!(cached.superseded_by_retry.is_some());
                cached.retry_cancel = None;
                cached.retry_fired_at = None;
                cached.superseded_by_retry = None;
            }
            test.manager
                .store
                .lock()
                .await
                .set_daemon_setting(
                    &crate::store::daemon_settings::c5_autofile_pending_key(target),
                    "recovery owns this row",
                )
                .unwrap();
            assert_preserved_refusal(
                &test,
                target,
                vec![Uuid::new_v4()],
                "manager_notice_deferred",
            )
            .await;
        }

        #[tokio::test]
        async fn manager_notice_preserves_an_existing_one_shot_resume_owner() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let mut job = wake(&test, target, Some(Uuid::new_v4()));
            job.wake_mode = rsi_common::types::WakeMode::Resume;
            job.schedule.recurrence = rsi_common::types::Recurrence::Once;
            test.manager
                .store
                .lock()
                .await
                .insert_scheduled_job(&job)
                .unwrap();
            assert_preserved_refusal(
                &test,
                target,
                vec![Uuid::new_v4()],
                "manager_notice_deferred",
            )
            .await;
            assert!(
                test.manager
                    .store
                    .lock()
                    .await
                    .get_scheduled_job(&job.id)
                    .unwrap()
                    .unwrap()
                    .enabled
            );
        }

        #[tokio::test]
        async fn manager_notice_preserves_open_capacity_incident_after_status_changes() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let mut guard = wake(&test, target, None);
            guard.provider = Some(SessionProvider::Codex);
            let invocation = Uuid::new_v4();
            {
                let store = test.manager.store.lock().await;
                store.conn.execute(
                    "UPDATE sessions SET provider='Codex',stop_reason='provider_error:codex_usage_limit' WHERE id=?1",
                    [target.to_string()],
                ).unwrap();
                store.insert_scheduled_job(&guard).unwrap();
                let now = chrono::Utc::now();
                store.conn.execute(
                    "INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,
                        admission_status,status,trigger_source,session_id,policy_snapshot_json,
                        usage_confidence,created_at,completed_at)
                     VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                        'admitted','failed','manager-notice-test',?2,'{}','unavailable',?3,?3)",
                    rusqlite::params![invocation.to_string(), target.to_string(),
                        now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)],
                ).unwrap();
                store
                    .set_session_model_invocation(target, Some(invocation))
                    .unwrap();
                store
                    .update_failed_and_stage_c5_autofile(target, AutofileCause::ProcessDied)
                    .unwrap();
                store
                    .settle_capacity_failure(target, target, guard.id, invocation, 1, now)
                    .unwrap();
                store
                    .update_session_status(target, SessionStatus::Completed)
                    .unwrap();
                // Isolate the capacity predicate: both scheduled owners are
                // disabled, no program report is present, and C5 transferred custody.
                store.conn.execute(
                    "UPDATE scheduled_jobs SET enabled=0 WHERE id=?2 OR id IN
                        (SELECT wake_job_id FROM master_no_idle_capacity_incidents WHERE controller_session_id=?1)",
                    rusqlite::params![target.to_string(), guard.id.to_string()],
                ).unwrap();
            }
            assert_preserved_refusal(
                &test,
                target,
                vec![Uuid::new_v4()],
                "manager_notice_deferred",
            )
            .await;
            let state: String = test.manager.store.lock().await.conn.query_row(
                "SELECT state FROM master_no_idle_capacity_incidents WHERE controller_session_id=?1",
                [target.to_string()], |row| row.get(0),
            ).unwrap();
            assert_eq!(state, "open");
        }

        #[tokio::test]
        async fn manager_notice_requires_completed_persisted_status() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            for status in [
                SessionStatus::Starting,
                SessionStatus::Running,
                SessionStatus::WaitingApproval,
                SessionStatus::Failed,
                SessionStatus::Interrupted,
                SessionStatus::Archived,
                SessionStatus::Deleted,
            ] {
                test.manager
                    .store
                    .lock()
                    .await
                    .update_session_status(target, status)
                    .unwrap();
                assert_preserved_refusal(
                    &test,
                    target,
                    vec![Uuid::new_v4()],
                    "manager_notice_deferred",
                )
                .await;
            }
        }

        #[tokio::test]
        async fn manager_notice_rejects_missing_empty_duplicate_and_unbound_jobs() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let ordinary = wake(&test, target, Some(Uuid::new_v4()));
            test.manager
                .store
                .lock()
                .await
                .insert_scheduled_job(&ordinary)
                .unwrap();
            for jobs in [
                Vec::new(),
                vec![Uuid::nil()],
                vec![Uuid::new_v4()],
                vec![ordinary.id],
                vec![ordinary.id, ordinary.id],
                (0..=crate::session::agent_verbs::MAX_TERMINAL_WATCHES_PER_MASTER)
                    .map(|_| Uuid::new_v4())
                    .collect(),
            ] {
                assert_preserved_refusal(&test, target, jobs, "manager_notice_scope_revoked").await;
            }
        }

        #[tokio::test]
        async fn manager_notice_rechecks_every_bound_job_after_scope_changes() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let job = bind_notice(&test, target).await;
            assert_preserved_refusal(
                &test,
                target,
                vec![job, Uuid::new_v4()],
                "manager_notice_scope_revoked",
            )
            .await;
            test.manager.store.lock().await.conn.execute(
                "UPDATE harness_manager_scopes SET row_version=row_version+1 WHERE manager_session_id=?1",
                [target.to_string()],
            ).unwrap();
            assert_preserved_refusal(&test, target, vec![job], "manager_notice_scope_revoked")
                .await;
        }

        #[tokio::test]
        async fn manager_notice_human_program_gate_survives_a_closed_sentinel_and_split_output() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let mut sentinel = wake(&test, target, None);
            sentinel.enabled = false;
            {
                let store = test.manager.store.lock().await;
                store.insert_scheduled_job(&sentinel).unwrap();
                insert_text(
                    &store,
                    target,
                    0,
                    Role::Assistant,
                    "orchestration_out".into(),
                );
                insert_text(&store, target, 1, Role::Assistant,
                    "come_v1: {\"schema_version\":1,\"mode\":\"program\",\"next_slice_ready\":false,\"continuation_state\":\"human_gate\",\"blocker_class\":\"production\",\"evidence\":\"operator deployment required\"}".into());
            }
            assert_preserved_refusal(
                &test,
                target,
                vec![Uuid::new_v4()],
                "manager_notice_deferred",
            )
            .await;
            // A later operator turn establishes a new boundary; old human gates
            // must not permanently prevent inbox delivery after operator action.
            let store = test.manager.store.lock().await;
            insert_text(&store, target, 2, Role::User, "Approved; proceed.".into());
            insert_text(
                &store,
                target,
                3,
                Role::Assistant,
                "Deployment completed.".into(),
            );
            check_manager_notice_program_gate(&store, target).unwrap();
        }

        #[tokio::test]
        async fn manager_notice_registered_recovery_and_output_budget_remain_deferred() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            test.manager
                .store
                .lock()
                .await
                .insert_scheduled_job(&wake(&test, target, None))
                .unwrap();
            assert_preserved_refusal(
                &test,
                target,
                vec![Uuid::new_v4()],
                "manager_notice_deferred",
            )
            .await;
            let store = test.manager.store.lock().await;
            insert_text(
                &store,
                target,
                0,
                Role::Assistant,
                "x".repeat(256 * 1024 + 1),
            );
            assert!(matches!(check_manager_notice_program_gate(&store, target),
                Err(DaemonError::InvalidParam(message)) if message == "manager_notice_deferred"));
        }

        #[tokio::test]
        async fn manager_notice_appserver_refusal_preserves_the_authorized_principal() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            test.manager
                .store
                .lock()
                .await
                .conn
                .execute(
                    "UPDATE sessions SET provider='CodexAppServer' WHERE id=?1",
                    [target.to_string()],
                )
                .unwrap();
            // Leave a stale Claude cache to prove persisted provider identity wins.
            assert_preserved_refusal(
                &test,
                target,
                vec![Uuid::new_v4()],
                "manager_notice_scope_revoked",
            )
            .await;
            let count: i64 = test
                .manager
                .store
                .lock()
                .await
                .conn
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 1);
        }

        async fn wait_for_notice_turn(test: &TestManager, target: Uuid, query: &str) {
            // Continuation returns after establishment; its monitor persists
            // the invocation binding and user event asynchronously.
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let store = test.manager.store.lock().await;
                    let recorded = store.load_events(target).unwrap().iter().any(|event| {
                        event.role == Some(Role::User) && event.content.contains(query)
                    });
                    if recorded && store.session_model_invocation_id(target).unwrap().is_some() {
                        return;
                    }
                    drop(store);
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("continued turn must persist the prompt and invocation");
        }

        #[tokio::test]
        async fn manager_notice_authorized_completed_recipient_uses_existing_spawn_funnel() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            let job = bind_notice(&test, target).await;
            crate::session::launch::install_controller_candidate_test_process(target);
            let recipient = test
                .manager
                .resume_manager_notice(target, "Read the manager inbox.".into(), vec![job])
                .await
                .unwrap();
            assert_eq!(recipient, target);
            assert!(test.manager.active.read().await.contains_key(&target));
            wait_for_notice_turn(&test, target, "Read the manager inbox.").await;
            crate::session::launch::drop_controller_candidate_test_stream(target);
        }

        #[tokio::test]
        async fn manager_notice_intent_preserves_ordinary_waiting_approval_continuation() {
            let test = test_manager();
            let (target, _) = insert_completed_continue_fixture(&test, false).await;
            test.manager
                .completed
                .write()
                .await
                .get_mut(&target)
                .unwrap()
                .session
                .status = SessionStatus::WaitingApproval;
            test.manager
                .store
                .lock()
                .await
                .update_session_status(target, SessionStatus::WaitingApproval)
                .unwrap();
            crate::session::launch::install_controller_candidate_test_process(target);
            test.manager
                .continue_session(target, "Operator continuation".into())
                .await
                .unwrap();
            assert!(test.manager.active.read().await.contains_key(&target));
            wait_for_notice_turn(&test, target, "Operator continuation").await;
            crate::session::launch::drop_controller_candidate_test_stream(target);
        }
    }

    #[tokio::test]
    async fn archiving_a_non_selected_sandbox_retains_its_live_worktree_and_custody() {
        let test = test_manager();
        let (session_id, root) = insert_completed_continue_fixture(&test, true).await;
        let root = root.expect("sandbox root");
        test.manager
            .store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE sessions SET pending_archive=1 WHERE id=?1",
                [session_id.to_string()],
            )
            .expect("exclude fixture from the selected cleanup route");
        test.manager
            .completed
            .write()
            .await
            .get_mut(&session_id)
            .expect("completed sandbox fixture")
            .session
            .pending_archive = true;
        let custody_before = test
            .manager
            .store
            .lock()
            .await
            .live_custody_for_session(session_id)
            .expect("live custody before archive");

        test.manager
            .archive_session(session_id)
            .await
            .expect("archive is metadata-only for a sandboxed session");

        assert!(root.exists(), "archive must keep the worktree");
        let store = test.manager.store.lock().await;
        let row = store
            .get_session(session_id)
            .expect("read retained session")
            .expect("retained session row");
        assert_eq!(row.status, SessionStatus::Archived);
        assert_eq!(row.sandbox_cleanup_state, Some(SandboxCleanupState::Live));
        assert_eq!(row.sandbox_root.as_deref(), Some(root.as_path()));
        assert_eq!(
            row.sandbox_branch.as_deref(),
            Some(custody_before.sandbox_branch.as_str())
        );
        let custody_after = store
            .live_custody_for_session(session_id)
            .expect("archive must retain live custody");
        assert_eq!(custody_after.custody_id, custody_before.custody_id);
        assert_eq!(custody_after.allocation_id, custody_before.allocation_id);
        drop(store);

        let restored = test
            .manager
            .unarchive_session(session_id)
            .await
            .expect("restore archived sandboxed session");
        assert_eq!(restored.status, SessionStatus::Completed);
        assert_eq!(restored.sandbox_root.as_deref(), Some(root.as_path()));
        assert_eq!(
            test.manager
                .store
                .lock()
                .await
                .live_custody_for_session(session_id)
                .expect("restore must preserve live custody")
                .allocation_id,
            custody_before.allocation_id
        );

        install_continue_custody_config_observation_for_test(session_id, 0);
        super::super::launch::install_controller_candidate_test_process(session_id);
        test.manager
            .continue_session(session_id, "continue after retained archive".to_string())
            .await
            .expect("retained sandbox must support continuation after unarchive");
        assert_eq!(
            take_continue_custody_config_for_test(session_id, 0),
            Some((root.clone(), Some(root.join("target")))),
            "normal unarchive must reuse the retained live recovery worktree"
        );
        assert!(
            test.manager.active.read().await.contains_key(&session_id),
            "continued restored session must become active"
        );
    }

    #[tokio::test]
    async fn terminal_metadata_transitions_retain_non_selected_sandboxes() {
        for state in ["clean", "dirty", "unmerged"] {
            for (operation, expected_status) in [
                ("archive", SessionStatus::Archived),
                ("trash", SessionStatus::Deleted),
            ] {
                let test = test_manager();
                let (session_id, root) = insert_completed_continue_fixture(&test, true).await;
                let root = root.expect("sandbox root");
                test.manager
                    .store
                    .lock()
                    .await
                    .conn
                    .execute(
                        "UPDATE sessions SET pending_archive=1 WHERE id=?1",
                        [session_id.to_string()],
                    )
                    .expect("exclude fixture from the selected cleanup route");
                test.manager
                    .completed
                    .write()
                    .await
                    .get_mut(&session_id)
                    .expect("completed sandbox fixture")
                    .session
                    .pending_archive = true;

                match state {
                    "clean" => {}
                    "dirty" => {
                        std::fs::write(root.join("dirty.txt"), "retain dirty recovery\n")
                            .expect("write dirty recovery file");
                    }
                    "unmerged" => {
                        std::fs::write(root.join("README.md"), "sandbox change\n")
                            .expect("write sandbox commit");
                        git(&root, &["add", "README.md"]);
                        git(&root, &["commit", "-q", "-m", "sandbox change"]);
                        std::fs::write(test.repo.path().join("README.md"), "main change\n")
                            .expect("write main commit");
                        git(test.repo.path(), &["add", "README.md"]);
                        git(test.repo.path(), &["commit", "-q", "-m", "main change"]);
                        let merge = Command::new("git")
                            .args(["merge", "main"])
                            .current_dir(&root)
                            .output()
                            .expect("start conflicting merge");
                        assert!(
                            !merge.status.success(),
                            "fixture merge must remain unmerged: {}",
                            String::from_utf8_lossy(&merge.stderr)
                        );
                    }
                    _ => unreachable!("known sandbox state"),
                }

                let (branch, custody_id, allocation_id) = {
                    let store = test.manager.store.lock().await;
                    let row = store
                        .get_session(session_id)
                        .expect("read sandbox session")
                        .expect("sandbox session exists");
                    let custody = store
                        .live_custody_for_session(session_id)
                        .expect("live custody before terminal transition");
                    (
                        row.sandbox_branch.expect("sandbox branch"),
                        custody.custody_id,
                        custody.allocation_id,
                    )
                };
                let status_before = git(&root, &["status", "--porcelain=v1"]);
                let worktrees_before = git(test.repo.path(), &["worktree", "list", "--porcelain"]);
                let ref_before = git(
                    test.repo.path(),
                    &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
                );

                match operation {
                    "archive" => {
                        test.manager
                            .archive_session(session_id)
                            .await
                            .expect("archive sandboxed terminal session");
                    }
                    "trash" => {
                        test.manager
                            .delete_session(session_id)
                            .await
                            .expect("trash sandboxed terminal session");
                    }
                    _ => unreachable!("known terminal metadata operation"),
                }

                let store = test.manager.store.lock().await;
                let row = store
                    .get_session(session_id)
                    .expect("read terminal session")
                    .expect("terminal session exists");
                assert_eq!(row.status, expected_status, "{state} {operation}");
                assert_eq!(row.sandbox_root.as_deref(), Some(root.as_path()));
                assert_eq!(row.sandbox_branch.as_deref(), Some(branch.as_str()));
                assert_eq!(row.sandbox_cleanup_state, Some(SandboxCleanupState::Live));
                let custody = store
                    .live_custody_for_session(session_id)
                    .expect("terminal metadata transition must retain custody");
                assert_eq!(custody.custody_id, custody_id);
                assert_eq!(custody.allocation_id, allocation_id);
                drop(store);
                assert!(root.exists(), "{state} {operation} must retain root");
                assert_eq!(
                    git(&root, &["status", "--porcelain=v1"]),
                    status_before,
                    "{state} {operation} must retain worktree state"
                );
                assert_eq!(
                    git(test.repo.path(), &["worktree", "list", "--porcelain"]),
                    worktrees_before,
                    "{state} {operation} must retain worktree registration"
                );
                assert_eq!(
                    git(
                        test.repo.path(),
                        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
                    ),
                    ref_before,
                    "{state} {operation} must retain sandbox branch"
                );

                if operation == "trash" {
                    let restored = test
                        .manager
                        .undelete_session(session_id)
                        .await
                        .expect("restore trashed sandboxed session");
                    assert_eq!(restored.status, SessionStatus::Completed);
                    assert_eq!(restored.sandbox_root.as_deref(), Some(root.as_path()));
                    assert_eq!(
                        test.manager
                            .store
                            .lock()
                            .await
                            .live_custody_for_session(session_id)
                            .expect("restored trashed session must retain live custody")
                            .allocation_id,
                        allocation_id
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn terminal_metadata_transitions_ignore_shared_sandbox_cleanup_classification() {
        for (operation, expected_status) in [
            ("archive", SessionStatus::Archived),
            ("trash", SessionStatus::Deleted),
        ] {
            let test = test_manager();
            let (session_id, root) = insert_completed_continue_fixture(&test, true).await;
            let root = root.expect("sandbox root");
            let branch = test
                .manager
                .store
                .lock()
                .await
                .get_session(session_id)
                .expect("read sandbox session")
                .expect("sandbox session exists")
                .sandbox_branch
                .expect("sandbox branch");
            let shared_id = Uuid::new_v4();
            let mut shared = session(shared_id, test.repo.path());
            shared.status = SessionStatus::Completed;
            shared.sandbox_kind = Some(SandboxKind::GitWorktree);
            shared.sandbox_root = Some(root.clone());
            shared.sandbox_branch = Some(branch);
            shared.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            test.manager
                .completed
                .write()
                .await
                .insert(shared_id, CompletedSession::for_test(shared));

            assert_eq!(
                test.manager
                    .cleanup_decision_for_session_id(session_id)
                    .await,
                CleanupDecision::Blocked(CleanupBlockedReason::SharedLiveOwnership),
                "fixture must exercise the physical cleanup boundary"
            );

            match operation {
                "archive" => {
                    test.manager
                        .archive_session(session_id)
                        .await
                        .expect("archive shared sandbox metadata");
                }
                "trash" => {
                    test.manager
                        .delete_session(session_id)
                        .await
                        .expect("trash shared sandbox metadata");
                }
                _ => unreachable!("known terminal metadata operation"),
            }

            assert_eq!(
                test.manager
                    .store
                    .lock()
                    .await
                    .get_session(session_id)
                    .expect("read terminal sandbox row")
                    .expect("terminal sandbox row")
                    .status,
                expected_status
            );
            assert!(root.exists(), "{operation} must retain shared root");
            assert!(
                test.manager.completed.read().await.contains_key(&shared_id),
                "metadata transition must leave the distinct shared owner intact"
            );
        }
    }

    #[tokio::test]
    async fn purging_a_sandboxed_session_remains_guarded_by_cleanup_proof() {
        let test = test_manager();
        let (session_id, root) = insert_completed_continue_fixture(&test, true).await;
        let root = root.expect("sandbox root");

        let error = test
            .manager
            .purge_session(session_id)
            .await
            .expect_err("purge must retain the physical cleanup proof gate");
        assert!(
            error
                .to_string()
                .contains("sandbox cleanup blocked: missing_independently_verified_proof")
        );
        assert!(root.exists(), "guarded purge must not remove the worktree");
        let store = test.manager.store.lock().await;
        assert_eq!(
            store
                .get_session(session_id)
                .expect("read guarded purge row")
                .expect("guarded purge row")
                .status,
            SessionStatus::Completed
        );
        assert!(
            store.live_custody_for_session(session_id).is_ok(),
            "guarded purge must retain custody"
        );
    }

    #[tokio::test]
    async fn public_archive_routes_an_eligible_sandbox_through_cleanup() {
        let test = test_manager();
        let (session_id, root) = insert_completed_continue_fixture(&test, true).await;
        let root = root.expect("sandbox root");
        let branch = git(&root, &["branch", "--show-current"]).trim().to_string();
        let source_ref = format!("refs/heads/{branch}");
        std::fs::write(root.join("integrated.txt"), "integrated archive output\n")
            .expect("write integrated output");
        git(&root, &["add", "integrated.txt"]);
        git(&root, &["commit", "-q", "-m", "integrated archive output"]);
        let source_oid = git(&root, &["rev-parse", "HEAD"]).trim().to_string();
        git(test.repo.path(), &["merge", "--ff-only", &branch]);
        let target_oid = git(test.repo.path(), &["rev-parse", "refs/heads/main"])
            .trim()
            .to_string();
        assert_eq!(
            target_oid, source_oid,
            "fixture integrates committed output"
        );

        let result = test
            .manager
            .archive_session(session_id)
            .await
            .expect("eligible sandbox archive must reach the private cleanup route");
        result
            .validate_wire()
            .expect("archive result wire contract");
        let receipt = result.receipt.expect("typed settled cleanup receipt");
        assert_eq!(
            receipt.preservation_class,
            ArchivePreservationClassV1::IntegratedAncestor
        );
        assert_eq!(receipt.source_branch, source_ref);
        assert_eq!(receipt.source_oid.as_str(), source_oid);
        assert_eq!(receipt.target_ref.as_deref(), Some("refs/heads/main"));
        assert_eq!(
            receipt.target_oid.as_ref().map(|oid| oid.as_str()),
            Some(target_oid.as_str())
        );

        assert!(!root.exists(), "settled cleanup removes the worktree");
        assert_eq!(
            git(test.repo.path(), &["rev-parse", &source_ref]).trim(),
            source_oid
        );
        let store = test.manager.store.lock().await;
        let row = store
            .get_session(session_id)
            .expect("read archived session")
            .expect("archived session row");
        assert_eq!(row.status, SessionStatus::Archived);
        assert_eq!(row.sandbox_cleanup_state, Some(SandboxCleanupState::Purged));
        assert!(row.sandbox_root.is_none());
        assert!(row.sandbox_branch.is_none());
    }

    #[tokio::test]
    async fn archive_restart_unarchive_retains_source_and_allocates_distinct_branch() {
        let test = test_manager();
        let (session_id, original_root) = insert_completed_continue_fixture(&test, true).await;
        let original_root = original_root.expect("original sandbox root");
        let original_branch = git(&original_root, &["branch", "--show-current"])
            .trim()
            .to_string();
        let source_ref = format!("refs/heads/{original_branch}");
        let source_oid = git(&original_root, &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        let archived = test
            .manager
            .archive_session(session_id)
            .await
            .expect("archive through the production cleanup path");
        archived.validate_wire().expect("settled archive result");
        assert!(
            !original_root.exists(),
            "archive cleanup must remove the original worktree before restart"
        );
        assert_eq!(
            git(test.repo.path(), &["rev-parse", &source_ref]).trim(),
            source_oid
        );

        let restarted = SessionManager::new(
            Arc::new(EventBus::new(64)),
            Store::open(&test._db.path().join("rsi.db")).expect("reopen archived store"),
            false,
            test._db.path().join("restarted-daemon.sock"),
            None,
            Vec::new(),
            RuntimeConfig::from_config(&Config::from_env()),
            test.sandbox_base.path().to_path_buf(),
        )
        .expect("restart manager");
        restarted
            .restore_sessions()
            .await
            .expect("restart reconciliation");

        let restored = restarted
            .unarchive_session(session_id)
            .await
            .expect("unarchive archived sandbox after restart");
        let restored_root = restored
            .sandbox_root
            .as_ref()
            .expect("unarchive must create a replacement worktree");
        assert_eq!(restored.status, SessionStatus::Completed);
        assert_eq!(
            restored.sandbox_cleanup_state,
            Some(SandboxCleanupState::Live)
        );
        assert!(restored_root.exists(), "replacement worktree must exist");
        assert_ne!(
            restored_root, &original_root,
            "unarchive must not reuse the historical sandbox path"
        );
        let restored_branch = restored
            .sandbox_branch
            .as_deref()
            .expect("replacement branch");
        assert_ne!(restored_branch, original_branch);
        assert_eq!(
            git(test.repo.path(), &["rev-parse", &source_ref]).trim(),
            source_oid,
            "ordinary unarchive retains the archived direct source ref/OID"
        );
        let custody = restarted
            .store
            .lock()
            .await
            .live_custody_for_session(session_id)
            .expect("replacement worktree must be durably bound as live custody");
        assert_ne!(
            custody.allocation_id, session_id,
            "replacement worktree must receive a distinct allocation identity"
        );
        let replacement_allocation_id = custody.allocation_id;
        drop(custody);

        install_continue_custody_config_observation_for_test(session_id, 0);
        super::super::launch::install_controller_candidate_test_process(session_id);
        restarted
            .continue_session(
                session_id,
                "continue after cleanup-backed unarchive".to_string(),
            )
            .await
            .expect("replacement custody must admit an isolated continuation");
        assert_eq!(
            take_continue_custody_config_for_test(session_id, 0),
            Some((restored_root.clone(), Some(restored_root.join("target")))),
            "continuation must use the exact replacement custody"
        );
        assert!(
            restarted.active.read().await.contains_key(&session_id),
            "continued restored session becomes active"
        );
        let replacement = restarted
            .store
            .lock()
            .await
            .live_custody_for_session(session_id)
            .expect("replacement custody remains live after continuation");
        assert_eq!(replacement.allocation_id, replacement_allocation_id);
        assert_eq!(
            git(test.repo.path(), &["rev-parse", &source_ref]).trim(),
            source_oid,
            "continuation cannot delete, rewrite, or reuse the retained source ref"
        );
        super::super::launch::drop_controller_candidate_test_stream(session_id);
    }

    #[tokio::test]
    async fn archiving_or_deleting_a_container_cascades_the_session_lifecycle() {
        let test = test_manager();

        let archive_group_id = Uuid::new_v4();
        let archive_epic_id = Uuid::new_v4();
        let (archive_leaf_id, archive_root) = insert_completed_continue_fixture(&test, true).await;
        let archive_root = archive_root.expect("archive leaf sandbox root");
        let archive_standard_id = Uuid::new_v4();
        let mut archive_group = session(archive_group_id, test.repo.path());
        archive_group.status = SessionStatus::Completed;
        archive_group.session_kind = SessionKind::Group;
        let mut archive_epic = session(archive_epic_id, test.repo.path());
        archive_epic.status = SessionStatus::Completed;
        archive_epic.session_kind = SessionKind::Epic;
        archive_epic.parent_id = Some(archive_group_id);
        let mut archive_standard = session(archive_standard_id, test.repo.path());
        archive_standard.status = SessionStatus::Completed;
        archive_standard.parent_id = Some(archive_group_id);
        {
            let store = test.manager.store.lock().await;
            for row in [&archive_group, &archive_epic, &archive_standard] {
                store.insert_session(row).expect("insert archive tree row");
            }
            store
                .update_session_parent(archive_leaf_id, Some(archive_epic_id))
                .expect("attach sandboxed archive leaf to Epic");
            store
                .set_lead_session(archive_epic_id, Some(archive_leaf_id))
                .expect("set archive tree lead");
        }

        test.manager
            .archive_session(archive_group_id)
            .await
            .expect("archive whole group");
        {
            let store = test.manager.store.lock().await;
            for id in [
                archive_group_id,
                archive_epic_id,
                archive_leaf_id,
                archive_standard_id,
            ] {
                assert_eq!(
                    store
                        .get_session(id)
                        .expect("read archived tree row")
                        .unwrap()
                        .status,
                    SessionStatus::Archived
                );
            }
            assert_eq!(
                store
                    .get_session(archive_epic_id)
                    .expect("read archived epic")
                    .unwrap()
                    .lead_session_id,
                None
            );
            let archive_leaf = store
                .get_session(archive_leaf_id)
                .expect("read archived sandbox leaf")
                .expect("archived sandbox leaf");
            assert_eq!(
                archive_leaf.sandbox_root.as_deref(),
                Some(archive_root.as_path())
            );
            assert_eq!(
                archive_leaf.sandbox_cleanup_state,
                Some(SandboxCleanupState::Live)
            );
            assert!(
                archive_root.exists(),
                "group archive must retain sandbox leaf"
            );
            assert_eq!(
                store
                    .live_custody_for_session(archive_leaf_id)
                    .expect("archive must retain leaf custody")
                    .owner_session_id,
                archive_leaf_id
            );
        }

        let delete_group_id = Uuid::new_v4();
        let delete_epic_id = Uuid::new_v4();
        let (delete_leaf_id, delete_root) = insert_completed_continue_fixture(&test, true).await;
        let delete_root = delete_root.expect("delete leaf sandbox root");
        let mut delete_group = session(delete_group_id, test.repo.path());
        delete_group.status = SessionStatus::Completed;
        delete_group.session_kind = SessionKind::Group;
        let mut delete_epic = session(delete_epic_id, test.repo.path());
        delete_epic.status = SessionStatus::Completed;
        delete_epic.session_kind = SessionKind::Epic;
        delete_epic.parent_id = Some(delete_group_id);
        {
            let store = test.manager.store.lock().await;
            for row in [&delete_group, &delete_epic] {
                store.insert_session(row).expect("insert delete tree row");
            }
            store
                .update_session_parent(delete_leaf_id, Some(delete_epic_id))
                .expect("attach sandboxed delete leaf to Epic");
            store
                .set_lead_session(delete_epic_id, Some(delete_leaf_id))
                .expect("set delete tree lead");
        }

        test.manager
            .delete_session(delete_group_id)
            .await
            .expect("delete whole group");
        let store = test.manager.store.lock().await;
        for id in [delete_group_id, delete_epic_id, delete_leaf_id] {
            assert_eq!(
                store
                    .get_session(id)
                    .expect("read deleted tree row")
                    .unwrap()
                    .status,
                SessionStatus::Deleted
            );
        }
        assert_eq!(
            store
                .get_session(delete_epic_id)
                .expect("read deleted epic")
                .unwrap()
                .lead_session_id,
            None
        );
        let delete_leaf = store
            .get_session(delete_leaf_id)
            .expect("read deleted sandbox leaf")
            .expect("deleted sandbox leaf");
        assert_eq!(
            delete_leaf.sandbox_root.as_deref(),
            Some(delete_root.as_path())
        );
        assert_eq!(
            delete_leaf.sandbox_cleanup_state,
            Some(SandboxCleanupState::Live)
        );
        assert!(
            delete_root.exists(),
            "group delete must retain sandbox leaf"
        );
        assert_eq!(
            store
                .live_custody_for_session(delete_leaf_id)
                .expect("delete must retain leaf custody")
                .owner_session_id,
            delete_leaf_id
        );
    }

    fn d03_lifecycle_project() -> Project {
        Project {
            id: Uuid::new_v4(),
            name: "D03 lifecycle".to_string(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn d03_lifecycle_capture(project_id: Uuid) -> anyhow::Result<Capture> {
        let digest = Sha256Digest::parse(format!("sha256:{}", "3".repeat(64)))
            .map_err(anyhow::Error::msg)?;
        Ok(Capture {
            id: Uuid::new_v4(),
            project_id,
            creator_kind: IdeaActorKind::Operator,
            creator_id: "d03-lifecycle".to_string(),
            captured_at: chrono::Utc::now(),
            source_kind: CaptureSourceKind::OperatorInput,
            raw_content_digest: digest.clone(),
            storage_policy_id: "cas-v1".to_string(),
            content_ref: ContentAddressedRef::for_digest(&digest),
        })
    }

    fn d03_lifecycle_idea(project_id: Uuid, capture_id: Uuid, session_id: Uuid) -> Idea {
        let now = chrono::Utc::now();
        Idea {
            id: Uuid::new_v4(),
            project_id,
            slug: "d03-lifecycle-grant".to_string(),
            sigil: Some("D03".to_string()),
            genesis_capture_id: capture_id,
            genesis_span_start: None,
            genesis_span_end: None,
            genesis_span_digest: None,
            title: "Lifecycle grant".to_string(),
            description: "same-ID reconstruction".to_string(),
            portfolio_summary: "live durable A6 facts".to_string(),
            lifecycle: IdeaLifecycle::Open,
            stage: IdeaStage::Captured,
            priority: 1,
            autonomy_policy: AutonomyPolicy::CaptureOnly,
            integration_target_ref: "refs/heads/main".to_string(),
            program_template_policy_id: None,
            current_controller_session_id: Some(session_id),
            controller_epoch: 7,
            row_version: 1,
            next_event_sequence: 2,
            created_at: now,
            updated_at: now,
            terminal_at: None,
            superseded_at: None,
        }
    }

    fn d03_live_tracked_session(session: Session, alive: Arc<AtomicBool>) -> TrackedSession {
        let mut tracked = TrackedSession::new_for_test(session);
        tracked.process = Some(super::super::types::ProviderProcess::Scripted(
            super::super::types::ScriptedProcess {
                alive,
                exit_code: Arc::new(AtomicI32::new(0)),
                interrupt_count: Arc::new(AtomicUsize::new(0)),
                kill_count: Arc::new(AtomicUsize::new(0)),
                exit_on_interrupt: false,
                interrupt_fails: false,
                kill_fails: false,
            },
        ));
        tracked
    }

    async fn d03_reconstruct(test: &TestManager, session_id: Uuid, project_id: Uuid, token: &str) {
        SessionManager::reconstruct_live_same_id_controller_grant(
            &test.manager.active,
            &test.manager.store,
            &test.manager.agent_tokens,
            session_id,
            Some(project_id),
            SessionProvider::Claude,
            token,
        )
        .await;
    }

    async fn d03_grant(
        test: &TestManager,
        session_id: Uuid,
    ) -> Option<crate::idea_control::BoundControllerWriteAuthority> {
        test.manager
            .store
            .lock()
            .await
            .controller_grant_v1(session_id)
    }

    async fn assert_d05_program_run_capabilities_revoked(
        controller: &crate::program_run_control::BoundProgramRunControllerAuthority,
        scheduler: &crate::program_run_control::BoundProgramRunSchedulerAuthority,
        run: &ProgramRunV1,
        semantic_claim: &rsi_common::program_runs::ClaimProgramRunActionRequestV1,
    ) {
        use crate::program_run_control::ProgramRunControlError;

        let transition = ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
            program_run_id: run.id,
            expected_run_version: run.row_version,
            expected_idea_version: run.idea_row_version,
            operation: ProgramRunOperationV1::LocksGranted,
            idempotency_key: format!("d05-revoked-transition:{}", Uuid::new_v4()),
            reason: None,
        });
        assert_eq!(
            controller.transition(&transition).await.unwrap_err(),
            ProgramRunControlError::Forbidden
        );
        assert_eq!(
            controller
                .heartbeat_locks(run.id, Uuid::new_v4(), 1)
                .await
                .unwrap_err(),
            ProgramRunControlError::Forbidden
        );
        assert_eq!(
            scheduler
                .claim(&[ProgramRunActionKindV1::Work], 1)
                .await
                .unwrap_err(),
            ProgramRunControlError::Forbidden
        );
        assert_eq!(
            scheduler.claim_action(semantic_claim).await,
            Err(ProgramRunControlError::Forbidden)
        );
        let action_id = Uuid::new_v4();
        assert_eq!(
            scheduler.published(action_id, 1).await.unwrap_err(),
            ProgramRunControlError::Forbidden
        );
        assert_eq!(
            scheduler
                .bind_reference(
                    action_id,
                    1,
                    ProgramRunExternalReferenceV1::Session(Uuid::new_v4()),
                )
                .await
                .unwrap_err(),
            ProgramRunControlError::Forbidden
        );
        assert_eq!(
            scheduler
                .fail(action_id, 1, "confirmed_pre_effect", "revoked")
                .await
                .unwrap_err(),
            ProgramRunControlError::Forbidden
        );
        assert_eq!(
            scheduler.acknowledge(action_id, 1).await.unwrap_err(),
            ProgramRunControlError::Forbidden
        );
    }

    #[tokio::test]
    async fn d03_d05_controller_lifecycle_reconstructs_only_from_live_durable_current_a6_facts()
    -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        let test = test_manager();
        let replacement_source_id = Uuid::new_v4();
        let mut replacement_source = session(replacement_source_id, test.repo.path());
        replacement_source.provider = SessionProvider::CodexAppServer;
        replacement_source.agent_role = Some("Planner".into());
        replacement_source.epic_spawn_ordinal = Some(5);
        let replacement_config = effective_provider_replacement_config(
            &replacement_source,
            "continue through effective provider".to_string(),
        );
        assert_eq!(replacement_config.provider, Some(SessionProvider::Codex));
        assert_eq!(replacement_config.rsi_session_id, None);
        assert_eq!(replacement_config.resume_session_id, None);
        assert_eq!(
            replacement_config.continued_from,
            Some(replacement_source_id)
        );
        assert_eq!(replacement_config.agent_role.as_deref(), Some("Planner"));
        assert_eq!(replacement_config.epic_spawn_ordinal, Some(5));

        replacement_source.provider = SessionProvider::Pioneer;
        replacement_source.model = Some(crate::pioneer::PIONEER_DEFAULT_MODEL.to_string());
        let pioneer_replacement = effective_provider_replacement_config(
            &replacement_source,
            "continue Pioneer through Codex CLI".to_string(),
        );
        assert_eq!(pioneer_replacement.provider, Some(SessionProvider::Pioneer));
        assert_eq!(pioneer_replacement.agent_role.as_deref(), Some("Planner"));
        assert_eq!(pioneer_replacement.epic_spawn_ordinal, Some(5));
        assert_eq!(
            pioneer_replacement.model.as_deref(),
            Some(crate::pioneer::PIONEER_DEFAULT_MODEL)
        );

        let project = d03_lifecycle_project();
        let session_id = Uuid::new_v4();
        let mut durable_session = session(session_id, test.repo.path());
        durable_session.project_id = Some(project.id);
        durable_session.provider = SessionProvider::Claude;
        durable_session.status = SessionStatus::Starting;
        let capture = d03_lifecycle_capture(project.id)?;
        let idea = d03_lifecycle_idea(project.id, capture.id, session_id);
        {
            let store = test.manager.store.lock().await;
            store.insert_project(&project)?;
            store.insert_session(&durable_session)?;
            store.insert_d01_idea_fixture(&capture, &idea)?;
        }

        let alive = Arc::new(AtomicBool::new(true));
        let tracked = d03_live_tracked_session(durable_session.clone(), Arc::clone(&alive));
        test.manager
            .active
            .write()
            .await
            .insert(session_id, tracked);
        let token = "d03-current-a6".to_string();
        test.manager
            .agent_tokens
            .write()
            .await
            .insert(token.clone(), session_id);

        d03_reconstruct(&test, session_id, project.id, &token).await;
        let grant = d03_grant(&test, session_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("same-ID grant was not reconstructed"))?;
        assert_eq!(grant.controller_epoch(), idea.controller_epoch);

        let operator = test
            .manager
            .program_run_control_handle()
            .bind_operator(project.id)?;
        let created = operator
            .create(&CreateProgramRunRequestV1 {
                idea_id: idea.id,
                expected_idea_row_version: u64::try_from(idea.row_version)?,
                idempotency_key: "d05-lifecycle-live-witness-run".to_string(),
                template: ProgramRunTemplateV1 {
                    template_key: "d05-lifecycle".to_string(),
                    template_version: 1,
                    cursors: vec![ProgramRunCursorV1 {
                        key: "implement".to_string(),
                        phase: "implementation".to_string(),
                        required_gates: Vec::new(),
                        revision_target_ordinal: None,
                    }],
                    budgets: ProgramRunBudgetLimitsV1 {
                        productive_transitions: 4,
                        work_attempts: 2,
                        launch_retries: 2,
                        revisions: 2,
                        wake_reservations: 2,
                        action_publication_retries: 2,
                    },
                    locks: Vec::new(),
                    max_publication_attempts: 2,
                },
            })
            .await?;
        let current_controller = test
            .manager
            .bind_program_run_controller_authority(session_id)
            .await?;
        let ready = current_controller
            .transition(&ProgramRunTransitionInputV1::Simple(
                ProgramRunTransitionRequestV1 {
                    program_run_id: created.run.id,
                    expected_run_version: created.run.row_version,
                    expected_idea_version: created.run.idea_row_version,
                    operation: ProgramRunOperationV1::LocksGranted,
                    idempotency_key: "d05-current-incarnation-ready".to_string(),
                    reason: None,
                },
            ))
            .await?;
        let stale_controller = test
            .manager
            .bind_program_run_controller_authority(session_id)
            .await?;
        let stale_scheduler = test
            .manager
            .bind_program_run_scheduler_authority(session_id, test.manager.program_run_boot_id)
            .await?;
        let Some(semantic_claim) = stale_scheduler
            .claim(&[ProgramRunActionKindV1::Work], 1)
            .await?
            .pop()
            .and_then(|claim| claim.semantic_claim)
        else {
            anyhow::bail!("work action did not carry an exact semantic claim witness");
        };

        let reminted_token = test.manager.remint_session_token(session_id).await;
        assert_ne!(reminted_token, token);
        assert_d05_program_run_capabilities_revoked(
            &stale_controller,
            &stale_scheduler,
            &ready.run,
            &semantic_claim,
        )
        .await;
        let after_a6 = operator
            .get(ready.run.id)
            .await?
            .expect("run after A6 fence");
        assert_eq!(after_a6, ready.run, "revoked A6 cannot mutate the run");

        let stale_d03_controller = test
            .manager
            .bind_program_run_controller_authority(session_id)
            .await?;
        let stale_d03_scheduler = test
            .manager
            .bind_program_run_scheduler_authority(session_id, test.manager.program_run_boot_id)
            .await?;
        {
            let store = test.manager.store.lock().await;
            let identical_grant = store
                .controller_grant_v1(session_id)
                .expect("current D03 grant");
            store.remove_controller_grant_v1(session_id);
            store.install_controller_grant_v1(identical_grant);
        }
        assert_d05_program_run_capabilities_revoked(
            &stale_d03_controller,
            &stale_d03_scheduler,
            &ready.run,
            &semantic_claim,
        )
        .await;
        let after_d03 = operator
            .get(ready.run.id)
            .await?
            .expect("run after D03 fence");
        assert_eq!(after_d03, ready.run, "D03 ABA cannot mutate the run");

        let current = test
            .manager
            .bind_program_run_scheduler_authority(session_id, test.manager.program_run_boot_id)
            .await?;
        let advanced = current.claim_action(&semantic_claim).await?;
        assert_eq!(advanced.run.row_version, ready.run.row_version + 1);

        test.manager
            .store
            .lock()
            .await
            .remove_controller_grant_v1(session_id);
        test.manager.interrupt_session(session_id).await?;
        {
            let active = test.manager.active.read().await;
            let Some(tracked) = active.get(&session_id) else {
                anyhow::bail!("interrupted provider is no longer tracked");
            };
            assert!(tracked.interrupt_requested);
            assert!(
                alive.load(Ordering::SeqCst),
                "scripted provider remains transiently live after interrupt"
            );
            drop(active);
        }
        d03_reconstruct(&test, session_id, project.id, &token).await;
        assert_eq!(
            d03_grant(&test, session_id).await,
            None,
            "interrupt_requested must invalidate transient live-process reconstruction"
        );

        test.manager.active.write().await.insert(
            session_id,
            d03_live_tracked_session(durable_session.clone(), Arc::clone(&alive)),
        );

        alive.store(false, Ordering::SeqCst);
        d03_reconstruct(&test, session_id, project.id, &token).await;
        assert_eq!(d03_grant(&test, session_id).await, None);

        alive.store(true, Ordering::SeqCst);
        test.manager
            .store
            .lock()
            .await
            .update_session_status(session_id, SessionStatus::Completed)?;
        d03_reconstruct(&test, session_id, project.id, &token).await;
        assert_eq!(d03_grant(&test, session_id).await, None);

        test.manager
            .store
            .lock()
            .await
            .update_session_status(session_id, SessionStatus::Starting)?;
        d03_reconstruct(&test, session_id, project.id, "stale-a6-token").await;
        assert_eq!(d03_grant(&test, session_id).await, None);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn d03_controller_continue_real_interrupt_blocks_same_id_reconstruction()
    -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        let test = test_manager();
        let project = d03_lifecycle_project();
        let session_id = Uuid::new_v4();
        let mut durable_session = session(session_id, test.repo.path());
        durable_session.project_id = Some(project.id);
        durable_session.provider = SessionProvider::Claude;
        durable_session.claude_session_id = Some("d03-provider-session".to_string());
        durable_session.status = SessionStatus::Completed;
        let capture = d03_lifecycle_capture(project.id)?;
        let idea = d03_lifecycle_idea(project.id, capture.id, session_id);
        {
            let store = test.manager.store.lock().await;
            store.insert_project(&project)?;
            store.insert_session(&durable_session)?;
            store.insert_d01_idea_fixture(&capture, &idea)?;
        }
        test.manager.completed.write().await.insert(
            session_id,
            CompletedSession {
                session: durable_session,
                events: Vec::new(),
                turn_metrics: Vec::new(),
                retry_cancel: None,
                retry_fired_at: None,
                superseded_by_retry: None,
                events_hydrated: true,
            },
        );
        test.manager
            .register_agent_token("d03-stale-continue-token".to_string(), session_id)
            .await;
        let scripted = super::super::launch::install_controller_candidate_test_process(session_id);
        let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
            session_id,
            super::super::launch::ControllerCandidateTestPhase::SameIdBeforeReconstruction,
        );

        let continuation = test
            .manager
            .continue_session(session_id, "D03 interrupted continuation".to_string());
        tokio::pin!(continuation);
        tokio::select! {
            result = &mut continuation => {
                panic!("continuation completed before reconstruction pause: {result:?}");
            }
            result = reached => {
                result?;
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("continuation did not reach reconstruction pause");
            }
        }
        test.manager.interrupt_session(session_id).await?;
        resume
            .send(())
            .map_err(|()| anyhow::anyhow!("resume continuation reconstruction receiver dropped"))?;
        let continued = continuation.await;
        continued?;

        assert!(!scripted.alive.load(Ordering::SeqCst));
        assert_eq!(d03_grant(&test, session_id).await, None);
        assert!(
            !test
                .manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|bound| *bound == session_id),
            "failed same-ID establishment revokes its prospective A6 binding"
        );
        let projection = test
            .manager
            .store
            .lock()
            .await
            .load_idea_controller_projection_v1(project.id, idea.id)?;
        assert_eq!(projection.current_controller_session_id, Some(session_id));
        assert_eq!(projection.controller_epoch, idea.controller_epoch);
        assert_eq!(projection.row_version, idea.row_version);
        super::super::launch::drop_controller_candidate_test_stream(session_id);
        Ok(())
    }

    async fn sandboxed_active(test: &TestManager) -> (Uuid, PathBuf, String) {
        let session_id = Uuid::new_v4();
        let allocation = SandboxAllocator::new(test.sandbox_base.path().to_path_buf())
            .allocate(
                session_id,
                test.repo.path(),
                SandboxKind::GitWorktree,
                "HEAD",
                None,
            )
            .expect("allocate sandbox");
        let mut session = session(session_id, test.repo.path());
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(allocation.root.clone());
        session.sandbox_branch = allocation.branch.clone();
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        test.manager
            .store
            .lock()
            .await
            .insert_session(&session)
            .expect("insert sandbox row");
        test.manager
            .active
            .write()
            .await
            .insert(session_id, TrackedSession::new_for_test(session));
        (
            session_id,
            allocation.root,
            allocation.branch.expect("sandbox branch"),
        )
    }

    async fn wait_for_session(
        manager: &SessionManager,
        session_id: Uuid,
        predicate: impl Fn(&Session) -> bool,
    ) -> Session {
        for _ in 0..10_000 {
            let row = manager
                .store
                .lock()
                .await
                .get_session(session_id)
                .expect("load session")
                .expect("session row");
            if predicate(&row) {
                return row;
            }
            tokio::task::yield_now().await;
        }
        panic!("persistence worker did not reach expected state");
    }

    #[tokio::test]
    async fn failed_final_metadata_write_records_invocation_failure() {
        let test = test_manager();
        let session_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let mut persisted = session(session_id, test.repo.path());
        persisted.status = SessionStatus::Running;
        persisted.context_window = Some(1);
        {
            let store = test.manager.store.lock().await;
            store.insert_session(&persisted).unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,
                    admission_status,status,trigger_source,session_id,policy_snapshot_json,
                    usage_confidence,created_at)
                 VALUES(?1,'session.continue.resume','session_lifecycle','foreground','paid_capable',
                    'admitted','running','metadata-failure-test',?2,'{}','unavailable',?3)",
                    rusqlite::params![
                        invocation_id.to_string(),
                        session_id.to_string(),
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    ],
                )
                .unwrap();
            store
                .set_session_model_invocation(session_id, Some(invocation_id))
                .unwrap();
        }
        let mut invalid = persisted.clone();
        invalid.context_window = Some(0);
        invalid.permission_denial_count = Some(3);
        invalid.total_input_tokens = Some(160);
        invalid.total_output_tokens = Some(55);
        invalid.total_cache_creation_tokens = Some(18);
        invalid.total_cache_read_tokens = Some(7);
        invalid.work_time_ms = Some(125);
        invalid.cost_usd = Some(0.42);
        invalid.context_usage_confidence = ContextUsageConfidence::Full;
        let mut tracked = TrackedSession::new_for_test(invalid);
        tracked.received_meaningful_output = true;
        test.manager
            .active
            .write()
            .await
            .insert(session_id, tracked);

        SessionManager::finalize_session(
            session_id,
            0,
            TerminalFinalizeDecision::completed(),
            Arc::clone(&test.manager.active),
            Arc::clone(&test.manager.completed),
            Arc::clone(&test.manager.event_bus),
            Arc::clone(&test.manager.store),
            test.manager.persistence.clone(),
            None,
            Arc::clone(&test.manager.runtime_config),
        )
        .await;

        let store = test.manager.store.lock().await;
        let invocation: (
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<f64>,
            String,
        ) = store
            .conn
            .query_row(
                "SELECT status,error_class,input_tokens,output_tokens,cache_creation_tokens,
                        cache_read_tokens,wall_time_ms,estimated_cost_usd,usage_confidence
                 FROM model_invocations WHERE id=?1",
                [invocation_id.to_string()],
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
                    ))
                },
            )
            .unwrap();
        assert_eq!(invocation.0, "failed");
        assert_eq!(
            invocation.1.as_deref(),
            Some("session_metadata_persistence_failed")
        );
        assert_eq!(invocation.2, Some(160));
        assert_eq!(invocation.3, Some(55));
        assert_eq!(invocation.4, Some(18));
        assert_eq!(invocation.5, Some(7));
        assert_eq!(invocation.6, Some(125));
        assert_eq!(invocation.7, Some(0.42));
        assert_eq!(invocation.8, "measured");
        let charged: (i64, i64, i64, i64, i64) = store
            .conn
            .query_row(
                "SELECT input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,
                        wall_time_ms FROM model_budget_counters
                 WHERE scope_kind='session' AND scope_id=?1 AND purpose='__all__'
                   AND model_tier='__all__' AND effort='__all__'",
                [session_id.to_string()],
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
            .unwrap();
        assert_eq!(charged, (160, 55, 18, 7, 125));
        let row = store.get_session(session_id).unwrap().unwrap();
        assert_eq!(row.permission_denial_count, None);
        drop(store);

        let valid_session_id = Uuid::new_v4();
        let valid_invocation_id = Uuid::new_v4();
        let mut valid = session(valid_session_id, test.repo.path());
        valid.status = SessionStatus::Running;
        valid.context_window = Some(1);
        valid.permission_denial_count = Some(3);
        {
            let store = test.manager.store.lock().await;
            store.insert_session(&valid).unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,
                    admission_status,status,trigger_source,session_id,policy_snapshot_json,
                    usage_confidence,created_at)
                 VALUES(?1,'session.continue.resume','session_lifecycle','foreground','paid_capable',
                    'admitted','running','metadata-success-test',?2,'{}','unavailable',?3)",
                    rusqlite::params![
                        valid_invocation_id.to_string(),
                        valid_session_id.to_string(),
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    ],
                )
                .unwrap();
            store
                .set_session_model_invocation(valid_session_id, Some(valid_invocation_id))
                .unwrap();
        }
        let mut invalid_snapshot = valid.clone();
        invalid_snapshot.context_window = Some(0);
        assert!(
            test.manager
                .persistence
                .update_session_metadata(invalid_snapshot)
                .await
                .is_err(),
            "the snapshot caller observes its own metadata failure"
        );
        let mut tracked = TrackedSession::new_for_test(valid);
        tracked.received_meaningful_output = true;
        test.manager
            .active
            .write()
            .await
            .insert(valid_session_id, tracked);
        SessionManager::finalize_session(
            valid_session_id,
            0,
            TerminalFinalizeDecision::completed(),
            Arc::clone(&test.manager.active),
            Arc::clone(&test.manager.completed),
            Arc::clone(&test.manager.event_bus),
            Arc::clone(&test.manager.store),
            test.manager.persistence.clone(),
            None,
            Arc::clone(&test.manager.runtime_config),
        )
        .await;
        let store = test.manager.store.lock().await;
        let valid_invocation_status: String = store
            .conn
            .query_row(
                "SELECT status FROM model_invocations WHERE id=?1",
                [valid_invocation_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let valid_row = store.get_session(valid_session_id).unwrap().unwrap();
        assert_eq!(valid_row.status, SessionStatus::Completed);
        assert_eq!(valid_row.permission_denial_count, Some(3));
        assert_eq!(
            valid_invocation_status,
            "completed",
            "valid session status={:?}, permission_denial_count={:?}, invocation={:?}",
            valid_row.status,
            valid_row.permission_denial_count,
            store.session_model_invocation_id(valid_session_id).unwrap()
        );
    }

    #[tokio::test]
    async fn pending_archive_admission_retains_sandbox_without_cleanup_proof() {
        let test = test_manager();
        let (session_id, root, branch) = sandboxed_active(&test).await;
        let marker = root.join("pending.txt");
        std::fs::write(&marker, "retain\n").expect("write marker");
        let worktrees_before = git(test.repo.path(), &["worktree", "list", "--porcelain"]);
        let ref_before = git(
            test.repo.path(),
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        );

        test.manager
            .mark_pending_archive(session_id, true)
            .await
            .expect("pending archive must not require physical cleanup proof");
        let active = test.manager.active.read().await;
        let tracked = active.get(&session_id).expect("active session");
        assert!(tracked.pending_archive);
        assert!(tracked.session.pending_archive);
        drop(active);
        let row = wait_for_session(&test.manager, session_id, |row| row.pending_archive).await;
        assert!(row.pending_archive);
        assert_eq!(row.sandbox_cleanup_state, Some(SandboxCleanupState::Live));
        assert_eq!(
            std::fs::read_to_string(marker).expect("read marker"),
            "retain\n"
        );
        assert_eq!(
            git(test.repo.path(), &["worktree", "list", "--porcelain"]),
            worktrees_before
        );
        assert_eq!(
            git(
                test.repo.path(),
                &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
            ),
            ref_before
        );
    }

    #[tokio::test]
    async fn clearing_pending_archive_remains_available_for_recovery() {
        let test = test_manager();
        let (session_id, _, _) = sandboxed_active(&test).await;
        {
            let mut active = test.manager.active.write().await;
            let tracked = active.get_mut(&session_id).expect("active session");
            tracked.pending_archive = true;
            tracked.session.pending_archive = true;
        }
        test.manager
            .store
            .lock()
            .await
            .update_pending_archive(session_id, true)
            .expect("set durable pending marker");

        test.manager
            .mark_pending_archive(session_id, false)
            .await
            .expect("clear pending archive");
        let row = wait_for_session(&test.manager, session_id, |row| !row.pending_archive).await;
        assert!(!row.pending_archive);
        let active = test.manager.active.read().await;
        let tracked = active.get(&session_id).expect("active session");
        assert!(!tracked.pending_archive);
        assert!(!tracked.session.pending_archive);
    }

    #[tokio::test]
    async fn terminal_pending_auto_archive_retains_sandbox_and_commits_archive() {
        let test = test_manager();
        let (session_id, root, branch) = sandboxed_active(&test).await;
        let marker = root.join("terminal.txt");
        std::fs::write(&marker, "terminal work\n").expect("write marker");
        {
            let mut active = test.manager.active.write().await;
            let tracked = active.get_mut(&session_id).expect("active session");
            tracked.pending_archive = true;
            tracked.session.pending_archive = true;
            tracked.received_meaningful_output = true;
        }
        test.manager
            .store
            .lock()
            .await
            .update_pending_archive(session_id, true)
            .expect("set durable pending marker");
        let worktrees_before = git(test.repo.path(), &["worktree", "list", "--porcelain"]);
        let ref_before = git(
            test.repo.path(),
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        );
        let mut events = test.manager.event_bus.subscribe();

        SessionManager::finalize_session(
            session_id,
            0,
            TerminalFinalizeDecision::completed(),
            Arc::clone(&test.manager.active),
            Arc::clone(&test.manager.completed),
            Arc::clone(&test.manager.event_bus),
            Arc::clone(&test.manager.store),
            test.manager.persistence.clone(),
            None,
            Arc::clone(&test.manager.runtime_config),
        )
        .await;

        let row = wait_for_session(&test.manager, session_id, |row| {
            row.status == SessionStatus::Archived
        })
        .await;
        assert_eq!(row.status, SessionStatus::Archived);
        assert!(!row.pending_archive);
        assert_eq!(row.sandbox_cleanup_state, Some(SandboxCleanupState::Live));
        assert_eq!(row.sandbox_root.as_deref(), Some(root.as_path()));
        assert!(
            !test
                .manager
                .completed
                .read()
                .await
                .contains_key(&session_id)
        );
        assert_eq!(
            std::fs::read_to_string(marker).expect("read terminal marker"),
            "terminal work\n"
        );
        assert_eq!(
            git(test.repo.path(), &["worktree", "list", "--porcelain"]),
            worktrees_before
        );
        assert_eq!(
            git(
                test.repo.path(),
                &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
            ),
            ref_before
        );
        let archived = std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event.as_ref(), DaemonEvent::SessionArchived { .. }));
        assert!(archived, "terminal auto-archive must emit success");
        test.manager.event_bus.unsubscribe();
    }

    #[tokio::test]
    async fn h1_v83_continue_context_custody_uses_exact_ordinary_and_live_paths() {
        let ordinary = test_manager();
        let (ordinary_id, ordinary_root) =
            insert_completed_continue_fixture(&ordinary, false).await;
        assert_eq!(ordinary_root, None);
        install_continue_custody_config_observation_for_test(ordinary_id, 0);
        super::super::launch::install_controller_candidate_test_process(ordinary_id);
        ordinary
            .manager
            .continue_session(ordinary_id, "ordinary custody continuation".to_string())
            .await
            .expect("ordinary continue succeeds");
        assert_eq!(
            take_continue_custody_config_for_test(ordinary_id, 0),
            Some((ordinary.repo.path().to_path_buf(), None)),
            "ordinary continuation must preserve exactly the canonical cwd with no target override"
        );
        assert!(
            ordinary
                .manager
                .active
                .read()
                .await
                .contains_key(&ordinary_id)
        );

        let sandboxed = test_manager();
        let (sandboxed_id, root) = insert_completed_continue_fixture(&sandboxed, true).await;
        let root = root.expect("live worktree root");
        install_continue_custody_config_observation_for_test(sandboxed_id, 0);
        super::super::launch::install_controller_candidate_test_process(sandboxed_id);
        sandboxed
            .manager
            .continue_session(sandboxed_id, "sandbox custody continuation".to_string())
            .await
            .expect("live Reuse continue succeeds");
        assert_eq!(
            take_continue_custody_config_for_test(sandboxed_id, 0),
            Some((root.clone(), Some(root.join("target")))),
            "live Reuse must pass only the authenticated root and its target"
        );
        let live = sandboxed
            .manager
            .store
            .lock()
            .await
            .live_custody_for_session(sandboxed_id)
            .expect("durable live custody remains bound");
        assert_eq!(live.owner_session_id, sandboxed_id);
        assert!(
            sandboxed
                .manager
                .active
                .read()
                .await
                .contains_key(&sandboxed_id)
        );
    }

    #[tokio::test]
    async fn h1_v83_continue_context_revalidation_race_refuses_before_admission_or_dispatch() {
        let test = test_manager();
        let (session_id, root) = insert_completed_continue_fixture(&test, true).await;
        let root = root.expect("live worktree root");
        install_continue_custody_config_observation_for_test(session_id, 0);
        install_continue_custody_root_mutation_for_test(session_id, 0);

        let error = test
            .manager
            .continue_session(session_id, "race custody continuation".to_string())
            .await
            .expect_err("post-authorization root mutation must refuse ContextRead");
        assert!(
            error.to_string().contains("root_missing"),
            "expected typed custody refusal, got {error}"
        );
        assert!(
            test.manager
                .completed
                .read()
                .await
                .contains_key(&session_id),
            "failed revalidation must restore completed visibility"
        );
        assert!(
            !test.manager.active.read().await.contains_key(&session_id),
            "failed revalidation must not publish an active session"
        );
        assert!(
            !test
                .manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|bound| *bound == session_id),
            "failed revalidation occurs before token mint"
        );
        assert!(
            take_continue_custody_config_for_test(session_id, 0).is_none(),
            "failed revalidation must not prepare raw provider config"
        );
        assert!(
            !root.exists(),
            "the test mutates the real authenticated root before revalidation"
        );
    }

    #[tokio::test]
    async fn continue_execution_scratch_failure_restores_exact_completed_session() {
        let test = test_manager();
        let (session_id, root) = insert_completed_continue_fixture(&test, true).await;
        let root = root.expect("live worktree root");
        install_continue_execution_scratch_failure_for_test(session_id, 0);
        super::super::launch::install_controller_candidate_test_process(session_id);

        let error = test
            .manager
            .continue_session(session_id, "descriptor failure continuation".to_string())
            .await
            .expect_err("descriptor-relative target symlink must fail continue");
        assert!(
            error.to_string().contains("execution scratch rejected"),
            "unexpected descriptor failure: {error}"
        );
        let completed = test.manager.completed.read().await;
        let restored = completed
            .get(&session_id)
            .expect("descriptor failure restores exact completed visibility");
        assert_eq!(restored.session.id, session_id);
        assert_eq!(restored.session.sandbox_root.as_ref(), Some(&root));
        drop(completed);
        assert!(!test.manager.active.read().await.contains_key(&session_id));
        assert!(
            !test
                .manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|bound| *bound == session_id),
            "descriptor failure occurs before continue token mint"
        );
        assert!(
            std::fs::symlink_metadata(root.join("target"))
                .expect("injected target symlink remains evidence")
                .file_type()
                .is_symlink()
        );
        assert!(!root.join(".rsi-tmp").exists());
        super::super::launch::drop_controller_candidate_test_process(session_id);
    }

    #[tokio::test]
    async fn h1_v83_continue_preserves_provider_session_id_failure_restoration() {
        let test = test_manager();
        let (session_id, _) = insert_completed_continue_fixture(&test, false).await;
        test.manager
            .completed
            .write()
            .await
            .get_mut(&session_id)
            .expect("completed fixture")
            .session
            .claude_session_id = None;
        install_continue_custody_config_observation_for_test(session_id, 0);

        let error = test
            .manager
            .continue_session(session_id, "missing provider session id".to_string())
            .await
            .expect_err("CLI continuation without session id must still fail");
        assert!(error.to_string().contains("Cannot resume"));
        assert!(
            test.manager
                .completed
                .read()
                .await
                .contains_key(&session_id)
        );
        assert!(!test.manager.active.read().await.contains_key(&session_id));
        assert!(take_continue_custody_config_for_test(session_id, 0).is_none());
    }

    #[tokio::test]
    async fn h1_v83_continue_historical_and_malformed_custody_never_falls_back_or_reallocates() {
        use rsi_common::types::SandboxCustodyErrorCodeV1;

        let test = test_manager();
        let cases = [
            (
                Some(SandboxCleanupState::Purged),
                None,
                "source-worktree settlement journal fences",
            ),
            (Some(SandboxCleanupState::Failed), None, "sandbox_custody"),
            (
                None,
                Some(test.repo.path().join("partial-sandbox-root")),
                "sandbox_custody",
            ),
        ];
        for (index, (cleanup_state, root, expected_error)) in cases.into_iter().enumerate() {
            let session_id = Uuid::new_v4();
            let mut completed = session(session_id, test.repo.path());
            completed.status = SessionStatus::Completed;
            completed.claude_session_id = Some(format!("historical-{index}"));
            completed.sandbox_kind = Some(SandboxKind::GitWorktree);
            completed.sandbox_root = root;
            completed.sandbox_cleanup_state = cleanup_state;
            test.manager
                .store
                .lock()
                .await
                .insert_session(&completed)
                .expect("insert non-executable custody fixture");
            test.manager
                .completed
                .write()
                .await
                .insert(session_id, CompletedSession::for_test(completed));
            install_continue_custody_config_observation_for_test(session_id, 0);

            let error = test
                .manager
                .continue_session(session_id, format!("historical custody case {index}"))
                .await
                .expect_err("historical or malformed custody must refuse");
            assert!(
                error.to_string().contains(expected_error),
                "case {index} must return the expected custody fence: {error}"
            );
            assert!(
                test.manager
                    .completed
                    .read()
                    .await
                    .contains_key(&session_id)
            );
            assert!(!test.manager.active.read().await.contains_key(&session_id));
            assert!(take_continue_custody_config_for_test(session_id, 0).is_none());
        }

        let (quarantined_id, root) = insert_completed_continue_fixture(&test, true).await;
        let root = root.expect("quarantined fixture root");
        {
            let mut store = test.manager.store.lock().await;
            let live = store
                .live_custody_for_session(quarantined_id)
                .expect("live custody before quarantine");
            store
                .record_failed_revalidation(
                    live.custody_id,
                    live.generation,
                    SandboxCustodyErrorCodeV1::RootMissing,
                    rsi_common::types::SandboxCustodyTransitionV1::Continue,
                )
                .expect("quarantine durable root");
        }
        install_continue_custody_config_observation_for_test(quarantined_id, 0);
        let error = test
            .manager
            .continue_session(quarantined_id, "quarantined custody".to_string())
            .await
            .expect_err("quarantined custody must refuse");
        assert!(error.to_string().contains("sandbox_custody"));
        assert!(
            test.manager
                .completed
                .read()
                .await
                .contains_key(&quarantined_id)
        );
        assert!(
            !test
                .manager
                .active
                .read()
                .await
                .contains_key(&quarantined_id)
        );
        assert!(take_continue_custody_config_for_test(quarantined_id, 0).is_none());
        assert!(
            root.exists(),
            "continue must not reallocate or replace history"
        );
    }

    #[tokio::test]
    async fn h1_v83_continue_reports_startup_invalid_legacy_tuple_before_missing_ownership() {
        use rsi_common::types::SandboxCustodyErrorCodeV1;

        let test = test_manager();
        let (session_id, root) = insert_completed_continue_fixture(&test, true).await;
        let root = root.expect("live worktree root");
        {
            let mut store = test.manager.store.lock().await;
            store
                .conn
                .execute(
                    "UPDATE sessions SET sandbox_custody_id=NULL WHERE id=?1",
                    [session_id.to_string()],
                )
                .expect("detach legacy fixture from custody aggregate");
            store
                .conn
                .execute(
                    "UPDATE session_execution_projections
                     SET custody_id=NULL,custody_generation=NULL
                     WHERE session_id=?1",
                    [session_id.to_string()],
                )
                .expect("detach legacy fixture projection from custody aggregate");
            store
                .invalidate_startup_session(session_id, SandboxCustodyErrorCodeV1::TupleIncomplete)
                .expect("record startup invalid legacy tuple");
        }
        install_continue_custody_config_observation_for_test(session_id, 0);

        let error = test
            .manager
            .continue_session(session_id, "legacy tuple continuation".to_string())
            .await
            .expect_err("invalid legacy tuple must remain non-executable");
        assert!(
            error
                .to_string()
                .contains("sandbox_custody:tuple_incomplete"),
            "continue must return the durable startup diagnosis: {error}"
        );
        assert!(
            !error.to_string().contains("ownership_missing"),
            "known invalid legacy state must not be relabeled: {error}"
        );
        assert!(
            test.manager
                .completed
                .read()
                .await
                .contains_key(&session_id),
            "refusal must restore completed session visibility"
        );
        assert!(!test.manager.active.read().await.contains_key(&session_id));
        assert!(take_continue_custody_config_for_test(session_id, 0).is_none());
        assert!(root.exists(), "refusal must not alter the legacy worktree");
    }

    #[tokio::test]
    async fn h1_v83_continue_transferred_predecessor_refuses_without_fallback_or_provider() {
        let test = test_manager();
        let (session_id, root) = insert_completed_continue_fixture(&test, true).await;
        let root = root.expect("transferred fixture root");
        let predecessor = test
            .manager
            .completed
            .read()
            .await
            .get(&session_id)
            .expect("completed predecessor")
            .session
            .clone();
        let live = test
            .manager
            .store
            .lock()
            .await
            .live_custody_for_session(session_id)
            .expect("predecessor owns live custody before transfer");

        let successor_id = Uuid::new_v4();
        let mut successor = session(successor_id, test.repo.path());
        successor.status = SessionStatus::Starting;
        successor.continued_from = Some(session_id);
        successor.sandbox_kind = predecessor.sandbox_kind;
        successor.sandbox_root = predecessor.sandbox_root.clone();
        successor.sandbox_branch = predecessor.sandbox_branch.clone();
        successor.sandbox_cleanup_state = predecessor.sandbox_cleanup_state;
        successor.git_branch = predecessor.git_branch.clone();
        {
            let mut store = test.manager.store.lock().await;
            store
                .insert_session(&successor)
                .expect("reserve custody successor");
            store
                .bind_reserved_session_custody(
                    successor_id,
                    SessionCustodyBinding::Transfer {
                        custody_id: live.custody_id,
                        from_session_id: session_id,
                        generation: live.generation,
                        cause: CustodyCause::Rotation,
                        origin_session_id: Some(session_id),
                        scheduled_job_id: None,
                    },
                )
                .expect("transfer custody to successor");
        }

        install_continue_custody_config_observation_for_test(session_id, 0);
        let error = test
            .manager
            .continue_session(session_id, "stale predecessor continuation".to_string())
            .await
            .expect_err("a transferred predecessor cannot reuse successor custody");
        assert!(
            error.to_string().contains("ownership_missing"),
            "transferred predecessor must return typed missing ownership: {error}"
        );
        assert!(
            test.manager
                .completed
                .read()
                .await
                .contains_key(&session_id),
            "refusal must restore completed predecessor visibility"
        );
        assert!(!test.manager.active.read().await.contains_key(&session_id));
        assert!(take_continue_custody_config_for_test(session_id, 0).is_none());
        assert!(
            !test
                .manager
                .agent_tokens
                .read()
                .await
                .values()
                .any(|bound| *bound == session_id),
            "refusal must happen before token mint"
        );
        let successor_live = test
            .manager
            .store
            .lock()
            .await
            .live_custody_for_session(successor_id)
            .expect("successor retains transferred custody");
        assert_eq!(successor_live.owner_session_id, successor_id);
        assert!(root.exists(), "refusal must preserve successor-owned root");
    }

    #[test]
    fn h1_v83_continue_source_ratchets_forbid_fallback_reallocation_and_prepermit_dispatch() {
        let source = include_str!("lifecycle.rs");
        let continue_start = source
            .find("pub async fn continue_session")
            .expect("continue implementation");
        let continue_end = source[continue_start..]
            .find("async fn launch_effective_provider_replacement")
            .map(|offset| continue_start + offset)
            .expect("continue implementation end");
        let continue_body = &source[continue_start..continue_end];
        assert!(
            !continue_body.contains("sandbox_root.unwrap_or(working_dir)"),
            "continue must not recover sandbox custody through canonical fallback"
        );
        assert!(
            !continue_body.contains("self.sandbox_allocator.allocate("),
            "continue must not reintroduce best-effort same-id worktree allocation"
        );
        let begin = continue_body
            .find("CustodyService::begin_effect(")
            .expect("continue ContextRead permit call");
        let dispatch = continue_body
            .find("super::provider_spawn::spawn_provider_process(")
            .expect("existing provider funnel dispatch");
        assert!(
            begin < dispatch,
            "continue must authorize ContextRead before provider dispatch"
        );
    }

    #[tokio::test]
    async fn explicit_legacy_sandbox_restore_rehydrates_a_purged_tuple() {
        // Low-level compatibility proof only. `continue_session` deliberately
        // does not call allocation or restore: an explicitly authorized legacy
        // recovery workflow may allocate the deterministic path and then call
        // `restore_sandbox_allocation` to re-hydrate the row to Live.
        let test = test_manager();
        let session_id = Uuid::new_v4();
        let allocator = SandboxAllocator::new(test.sandbox_base.path().to_path_buf());

        // 1. Original allocation + Live row (mirrors a fresh sandboxed launch).
        let original = allocator
            .allocate(
                session_id,
                test.repo.path(),
                SandboxKind::GitWorktree,
                "HEAD",
                None,
            )
            .expect("original allocate");
        let original_root = original.root.clone();
        let original_branch = original.branch.clone().expect("branch");
        let mut sess = session(session_id, test.repo.path());
        sess.sandbox_kind = Some(SandboxKind::GitWorktree);
        sess.sandbox_root = Some(original_root.clone());
        sess.sandbox_branch = Some(original_branch.clone());
        sess.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        sess.git_branch = Some(original_branch.clone());
        test.manager
            .store
            .lock()
            .await
            .insert_session(&sess)
            .expect("insert");

        // 2. Simulate the terminal teardown: destroy the worktree + branch and
        //    tombstone the row (exactly what maybe_destroy_sandbox does).
        git(
            test.repo.path(),
            &[
                "worktree",
                "remove",
                "--force",
                original_root.to_str().expect("root utf8"),
            ],
        );
        git(test.repo.path(), &["branch", "-D", &original_branch]);
        test.manager
            .store
            .lock()
            .await
            .mark_sandbox_purged(session_id)
            .expect("purge");
        assert!(!original_root.exists(), "worktree dir gone after purge");
        let purged = test
            .manager
            .store
            .lock()
            .await
            .get_session(session_id)
            .expect("load purged")
            .expect("purged row");
        assert_eq!(
            purged.sandbox_cleanup_state,
            Some(SandboxCleanupState::Purged)
        );
        assert_eq!(purged.sandbox_root, None);
        // sandbox_kind is preserved as historical sandbox metadata.
        assert_eq!(purged.sandbox_kind, Some(SandboxKind::GitWorktree));

        // 3. An explicit recovery allocation at the same session id lands at
        //    the same deterministic path.
        let restored = allocator
            .allocate(
                session_id,
                test.repo.path(),
                SandboxKind::GitWorktree,
                "HEAD",
                None,
            )
            .expect("re-allocate after purge");
        assert_eq!(
            restored.root, original_root,
            "fresh worktree reuses the session-id path"
        );
        assert!(restored.root.exists(), "fresh worktree present on disk");
        let restored_branch = restored.branch.clone().expect("branch");

        // 4. Re-hydrate the row and assert it is Live again with fresh metadata.
        test.manager
            .store
            .lock()
            .await
            .restore_sandbox_allocation(
                session_id,
                restored.kind,
                &restored.root,
                restored.branch.as_deref(),
                restored.branch.as_deref(),
            )
            .expect("restore metadata");
        let row = test
            .manager
            .store
            .lock()
            .await
            .get_session(session_id)
            .expect("load restored")
            .expect("restored row");
        assert_eq!(row.sandbox_cleanup_state, Some(SandboxCleanupState::Live));
        assert_eq!(row.sandbox_root.as_deref(), Some(restored.root.as_path()));
        assert_eq!(row.sandbox_kind, Some(SandboxKind::GitWorktree));
        assert_eq!(
            row.sandbox_branch.as_deref(),
            Some(restored_branch.as_str())
        );
        assert_eq!(row.git_branch.as_deref(), Some(restored_branch.as_str()));
    }
}
