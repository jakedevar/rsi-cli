//! Types and constants for session management.

use crate::agy::AgyProcess;
use crate::claude::ClaudeProcess;
use crate::codex::CodexProcess;
use crate::codex_app_server::CodexAppServerProcess;
use crate::error::Result;
use crate::openai::OpenAiProcess;
use crate::provider_capabilities;
use crate::store::successor_reservations::AgentSuccessorReservation;
use regex::Regex;
use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, IdeaControllerLaunchConfirmationV1,
    IdeaControllerReservationV1, NewSessionDiagnosticV1, Session, SessionProvider, SessionStatus,
    TurnMetric, WorkflowStage,
};
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Regex for discovering pipeline or handoff path candidates in assistant text.
/// Structural helpers classify each match before mutating session state.
pub(super) static PIPELINE_PATH_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"thoughts/shared/(research|plans|handoffs)/[^\s"'`)\]>]+\.md"#)
        .expect("valid regex")
});

/// Regex for detecting `<docregblock>/spawn_child …</docregblock>` directives
/// in assistant text. Matches the full block from open tag through close tag,
/// anchored at line start to avoid false positives in pasted code blocks.
///
/// `(?ms)` enables multi-line mode (`^` / `$` per line) and dot-matches-newline,
/// so the lazy `.*?` spans across the directive body until the close tag.
pub(super) static SPAWN_DIRECTIVE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    regex::RegexBuilder::new(r"(?ms)^<docregblock>\s*\n?/spawn_child\b.*?\n?</docregblock>")
        .multi_line(true)
        .dot_matches_new_line(true)
        .build()
        .expect("SPAWN_DIRECTIVE_RE compiles")
});

/// Regex for detecting `<docregblock>/halt</docregblock>` directives in assistant
/// text. Emitted by lead sessions to signal the loop executor to stop iterating
/// (`UntilCondition::LeadHalt`). Anchored at line start; tolerates optional
/// whitespace after `/halt` before the closing tag.
pub(crate) static HALT_DIRECTIVE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    regex::RegexBuilder::new(r"(?ms)^<docregblock>\s*\n?/halt\s*\n?</docregblock>")
        .multi_line(true)
        .dot_matches_new_line(true)
        .build()
        .expect("HALT_DIRECTIVE_RE compiles")
});

/// Data preserved for completed sessions.
pub struct CompletedSession {
    pub(crate) session: Session,
    pub(crate) events: Vec<ConversationEvent>,
    pub(crate) turn_metrics: Vec<TurnMetric>,
    /// Cancel sender for a pending retry timer. When set, a retry is scheduled.
    /// Dropping or sending cancels the retry.
    pub(crate) retry_cancel: Option<tokio::sync::oneshot::Sender<()>>,
    /// Set when a retry timer has fired into the daemon retry queue but has not
    /// yet been consumed by `launch_retry`.
    pub(crate) retry_fired_at: Option<std::time::Instant>,
    /// Retry child spawned from this failed session. Used to resolve a later
    /// user continue against the original without running both.
    pub(crate) superseded_by_retry: Option<Uuid>,
    /// C7 Phase 1: whether `events` reflects the session's real transcript.
    /// Restore inserts a `Vec::new()` placeholder with this `false` to avoid
    /// loading every session's full history into RAM at daemon startup; the
    /// transcript is durable in SQLite and is hydrated on first actual use
    /// (`SessionManager::hydrate_completed_events`). `true` means `events` is
    /// already authoritative (freshly finalized, explicitly loaded, or a
    /// container kind that never has events) and needs no further loading.
    pub(crate) events_hydrated: bool,
}

/// Internal retry-only admission data.  A retry child is not an ordinary
/// continued session: its row, retry counters, and the parent's pending C5
/// marker must become durable in one Store transaction before it is visible.
#[derive(Clone)]
pub(super) struct RetryAdmission {
    pub(super) parent_session_id: Uuid,
    pub(super) retry_attempt: u8,
    pub(super) max_retries: u8,
    /// Exact durable C5 marker observed when retry eligibility was claimed.
    /// Store admission compares this witness before it can create a child.
    pub(super) pending_marker: crate::store::daemon_settings::C5AutofilePending,
    source: Arc<Session>,
    custody_candidate: Arc<crate::sandbox::custody::RetryCustodyCandidate>,
    attempted: Arc<AtomicBool>,
    committed: Arc<AtomicBool>,
    retryable_failure: Arc<AtomicBool>,
    reclaim_prepared_failure: Arc<AtomicBool>,
}

impl RetryAdmission {
    pub(super) fn new(
        parent_session_id: Uuid,
        retry_attempt: u8,
        max_retries: u8,
        pending_marker: crate::store::daemon_settings::C5AutofilePending,
        source: Session,
        custody_candidate: crate::sandbox::custody::RetryCustodyCandidate,
    ) -> Self {
        Self {
            parent_session_id,
            retry_attempt,
            max_retries,
            pending_marker,
            source: Arc::new(source),
            custody_candidate: Arc::new(custody_candidate),
            attempted: Arc::new(AtomicBool::new(false)),
            committed: Arc::new(AtomicBool::new(false)),
            retryable_failure: Arc::new(AtomicBool::new(false)),
            reclaim_prepared_failure: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn source(&self) -> &Session {
        &self.source
    }

    pub(super) fn custody_candidate(&self) -> &crate::sandbox::custody::RetryCustodyCandidate {
        &self.custody_candidate
    }

    pub(super) fn mark_attempted(&self) {
        self.attempted.store(true, Ordering::Release);
    }

    pub(super) fn mark_committed(&self) {
        self.committed.store(true, Ordering::Release);
    }

    pub(super) fn mark_failed(&self, retryable: bool) {
        self.retryable_failure.store(retryable, Ordering::Release);
    }

    pub(super) fn mark_reclaim_prepared(&self) {
        self.reclaim_prepared_failure.store(true, Ordering::Release);
    }

    pub(super) fn reclaim_prepared_failure(&self) -> bool {
        self.reclaim_prepared_failure.load(Ordering::Acquire)
    }

    pub(super) fn failed_before_commit(&self) -> bool {
        self.attempted.load(Ordering::Acquire) && !self.committed.load(Ordering::Acquire)
    }

    pub(super) fn retryable_failure_before_commit(&self) -> bool {
        self.failed_before_commit() && self.retryable_failure.load(Ordering::Acquire)
    }
}

/// Redacted prospective A6 witness shared between the launch core and the
/// transfer coordinator. The token value is never formatted, serialized, or
/// persisted.
#[derive(Clone, Default)]
pub(super) struct ProspectiveAgentTokenWitness {
    token: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl std::fmt::Debug for ProspectiveAgentTokenWitness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProspectiveAgentTokenWitness")
            .field("present", &"<redacted>")
            .finish()
    }
}

impl ProspectiveAgentTokenWitness {
    pub(super) async fn install(&self, token: String) {
        *self.token.lock().await = Some(token);
    }

    pub(super) async fn current_token(&self) -> Option<String> {
        self.token.lock().await.clone()
    }

    pub(super) async fn clear(&self) {
        self.token.lock().await.take();
    }
}

/// Server-owned context for one deterministic controller candidate launch.
pub(super) struct ControllerTransferLaunchContext {
    pub(super) project_id: Uuid,
    pub(super) idea_id: Uuid,
    pub(super) reservation: IdeaControllerReservationV1,
    pub(super) prospective_a6: ProspectiveAgentTokenWitness,
    pub(super) confirmation_tx:
        Option<tokio::sync::oneshot::Sender<IdeaControllerLaunchConfirmationV1>>,
}

/// Server-owned context for one stable master-successor candidate launch.
/// The reservation is the frozen authority and launch witness; the prospective
/// token is never persisted and is held only until the final fenced commit.
pub(super) struct AgentSuccessorLaunchContext {
    pub(super) reservation: AgentSuccessorReservation,
    pub(super) prospective_a6: ProspectiveAgentTokenWitness,
    pub(super) confirmation_tx:
        Option<tokio::sync::oneshot::Sender<AgentSuccessorLaunchConfirmation>>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AgentSuccessorLaunchConfirmation {
    pub(super) candidate_session_id: Uuid,
    pub(super) provider: SessionProvider,
    pub(super) admission_invocation_id: Uuid,
    pub(super) confirmation_kind: rsi_common::types::ControllerConfirmationKindV1,
}

/// Server-owned durable identity for one agent child launch.
#[derive(Debug, Clone)]
pub(super) struct AgentChildLaunchContext {
    pub(super) spawn_request_id: Uuid,
    pub(super) child_session_id: Uuid,
    pub(super) owner_session_id: Uuid,
    /// H1-04 (F-011): authenticated fork source captured by
    /// `launch_agent_child` before any child-side effect. `Some` whenever the
    /// child requests a sandbox; the allocation path refuses to fall back to
    /// canonical resolution when it is absent.
    pub(super) fork: Option<crate::sandbox::custody::SpawnForkCandidate>,
}

/// Server-owned identities reserved by the Closure launch ledger before any
/// sandbox or provider effect.
#[derive(Debug, Clone, Copy)]
pub(super) struct ClosureSourceLaunchContext {
    pub(super) session_id: Uuid,
    pub(super) custody_id: Uuid,
}

impl std::fmt::Debug for ControllerTransferLaunchContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControllerTransferLaunchContext")
            .field("project_id", &self.project_id)
            .field("idea_id", &self.idea_id)
            .field("reservation_id", &self.reservation.reservation_id)
            .field(
                "candidate_session_id",
                &self.reservation.candidate_session_id,
            )
            .field("prospective_a6", &self.prospective_a6)
            .field("confirmation_tx_present", &self.confirmation_tx.is_some())
            .finish()
    }
}

impl std::fmt::Debug for AgentSuccessorLaunchContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSuccessorLaunchContext")
            .field("reservation_id", &self.reservation.reservation_id)
            .field(
                "candidate_session_id",
                &self.reservation.candidate_session_id,
            )
            .field("prospective_a6", &self.prospective_a6)
            .field("confirmation_tx_present", &self.confirmation_tx.is_some())
            .finish()
    }
}

/// Private launch purpose. Only a controller transfer may preallocate the
/// session UUID or await establishment confirmation.
#[derive(Debug, Clone)]
pub(super) struct ManagerActionLaunchContext {
    pub(super) claim: crate::store::manager_actions::ManagerActionClaimV2,
    pub(super) fork: crate::sandbox::custody::SpawnForkCandidate,
    /// Exact runtime incarnation produced by this launch, not a later operator
    /// continuation of the same logical session. Never a request/DB identity.
    pub(super) generation: Arc<AtomicU64>,
}

#[derive(Debug)]
pub(super) struct TopologyNodeLaunchContext {
    pub(super) session_id: Uuid,
    pub(super) fork: crate::topology::custody::TopologyForkSource,
}

#[derive(Debug)]
pub(super) enum LaunchPurpose {
    Interactive,
    ManagerSuccessor(Box<super::manager_succession::ManagerSuccessionLaunchContext>),
    ManagerAction(Box<ManagerActionLaunchContext>),
    TopologyNode(TopologyNodeLaunchContext),
    ControllerCandidate(Box<ControllerTransferLaunchContext>),
    AgentSuccessor(Box<AgentSuccessorLaunchContext>),
    AgentChild(AgentChildLaunchContext),
    ClosureSource(ClosureSourceLaunchContext),
}

impl LaunchPurpose {
    pub(super) fn session_id(&self) -> Uuid {
        match self {
            Self::Interactive => Uuid::new_v4(),
            Self::ManagerSuccessor(context) => context.candidate_id(),
            Self::ManagerAction(context) => context
                .claim
                .operation
                .context
                .target_session_id
                .expect("reserved manager identity"),
            Self::TopologyNode(context) => context.session_id,
            Self::ControllerCandidate(context) => context.reservation.candidate_session_id,
            Self::AgentSuccessor(context) => context.reservation.candidate_session_id,
            Self::AgentChild(context) => context.child_session_id,
            Self::ClosureSource(context) => context.session_id,
        }
    }

    pub(super) const fn is_controller_candidate(&self) -> bool {
        matches!(self, Self::ControllerCandidate(_))
    }

    pub(super) const fn is_agent_successor(&self) -> bool {
        matches!(self, Self::AgentSuccessor(_))
    }

    pub(super) fn manager_successor(
        &self,
    ) -> Option<&super::manager_succession::ManagerSuccessionLaunchContext> {
        match self {
            Self::ManagerSuccessor(context) => Some(context),
            _ => None,
        }
    }

    pub(super) fn manager_action(&self) -> Option<&ManagerActionLaunchContext> {
        match self {
            Self::ManagerAction(context) => Some(context),
            _ => None,
        }
    }

    pub(super) const fn topology_node(&self) -> Option<&TopologyNodeLaunchContext> {
        match self {
            Self::TopologyNode(context) => Some(context),
            _ => None,
        }
    }

    pub(super) const fn agent_child(&self) -> Option<&AgentChildLaunchContext> {
        match self {
            Self::AgentChild(context) => Some(context),
            _ => None,
        }
    }

    pub(super) const fn closure_source(&self) -> Option<&ClosureSourceLaunchContext> {
        match self {
            Self::ClosureSource(context) => Some(context),
            _ => None,
        }
    }

    pub(super) const fn uses_direct_establishment(&self) -> bool {
        matches!(
            self,
            Self::Interactive
                | Self::ClosureSource(_)
                | Self::ManagerAction(_)
                | Self::TopologyNode(_)
        )
    }

    pub(super) const fn prospective_a6(&self) -> Option<&ProspectiveAgentTokenWitness> {
        match self {
            Self::ManagerSuccessor(context) => Some(&context.prospective_a6),
            Self::Interactive | Self::ManagerAction(_) | Self::TopologyNode(_) => None,
            Self::ControllerCandidate(context) => Some(&context.prospective_a6),
            Self::AgentSuccessor(context) => Some(&context.prospective_a6),
            Self::AgentChild(_) | Self::ClosureSource(_) => None,
        }
    }
}

impl CompletedSession {
    /// Public constructor for test helpers that need to inject a completed
    /// session into the coordinator's view without going through SessionManager.
    pub fn for_test(session: Session) -> Self {
        Self {
            session,
            events: vec![],
            turn_metrics: vec![],
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        }
    }
}

/// Internal tracking for an active session.
/// Note: event_rx is NOT stored here - it's passed to the monitor task.
pub struct TrackedSession {
    pub(crate) session: Session,
    /// Per-process incarnation stamp assigned at provider establishment.
    /// Stale monitor tasks must not finalize a newer incarnation.
    pub(crate) spawn_generation: u64,
    pub(super) events: Vec<ConversationEvent>,
    pub(super) turn_metrics: Vec<TurnMetric>,
    /// Provider process handle. `pub(crate)` for reconciliation liveness checks.
    pub(crate) process: Option<ProviderProcess>,
    /// Exact-incarnation gate for a deferred agent-successor provider effect.
    ///
    /// The deferred CodexAppServer launch task holds this gate from its final
    /// live-cancellation check through process installation. Interrupt paths
    /// take the same gate before acknowledging cancellation, so a process-less
    /// active row cannot be interrupted successfully and then launch later.
    /// `None` for every synchronous provider and non-successor launch.
    pub(crate) deferred_successor_start_gate: Option<Arc<tokio::sync::Mutex<()>>>,
    /// Stop signal sender. `pub(crate)` for reconciliation to signal finalization.
    pub(crate) stop_tx: mpsc::Sender<()>,
    pub(super) interrupt_requested: bool,
    pub(super) pending_archive: bool,
    /// Context rotation state machine. Owns all rotation lifecycle state.
    /// `pub(crate)` for reconciliation to check rotation state before intervention.
    pub(crate) rotation: super::rotation_coordinator::RotationCoordinator,
    /// Live accumulator: provider-normalized input tokens across all turns.
    /// Claude includes additive cache fields; Codex stores cumulative
    /// `turn.completed` telemetry here for metrics only. Codex live context
    /// fill uses `codex_context_tokens` instead.
    pub(super) live_input_tokens: u64,
    /// Live accumulator: total output tokens across all turns.
    pub(super) live_output_tokens: u64,
    /// Confidence of the latest usage data from the stream.
    pub(super) live_usage_confidence: ContextUsageConfidence,
    /// Daemon-counted input tokens (query + tool results), accumulated from raw text.
    /// Monotonically increasing -- never jumps. Primary source for context % when nonzero.
    pub(super) daemon_input_tokens: u64,
    /// Daemon-counted output tokens (assistant text content), accumulated from raw text.
    pub(super) daemon_output_tokens: u64,
    /// Snapshot of `daemon_input_tokens + daemon_output_tokens` at the time of the last
    /// API-reported token update. Used to compute the daemon delta for real-time context
    /// tracking between turns (tool calls grow the context but the API total only updates
    /// on assistant events).
    pub(super) daemon_tokens_at_last_api_update: u64,
    /// Latest Codex current-window token-count total (from either CLI transcript
    /// telemetry or app-server `thread/tokenUsage/updated`). This is the active
    /// context size Codex uses for its own indicator, unlike `turn.completed`
    /// usage, which is cumulative across internal model calls and can exceed
    /// the context window.
    /// Runtime-only; reset on provider subprocess spawn. `last_usage_update`
    /// records presence (including true zero) and provider observation age.
    pub(super) codex_context_tokens: u64,
    /// Pipeline artifact path detected from Write tool events.
    /// Set when model writes to thoughts/shared/{research,plans}/.
    /// Consumed by TUI for button rendering and Space+x execution.
    pub(super) pipeline_artifact: Option<String>,
    /// Compaction count (`rotation_depth`) at which the last memory-flush turn
    /// was accepted by the provider. Guards against a second flush at the same
    /// depth.
    pub(super) memory_flush_compaction_count: Option<u32>,
    pub(super) pending_question: Option<rsi_common::types::PendingQuestion>,
    /// Timestamp of the current open WaitingApproval interval, if any. Set when
    /// `pending_question` becomes `Some(...)` (AskUserQuestion detected) and
    /// consumed when the question is answered or the session terminalizes.
    /// Monotonic tokio `Instant` so wallclock jumps don't produce negative deltas.
    pub(super) approval_wait_start: Option<std::time::Instant>,
    /// Accumulated WaitingApproval duration in milliseconds across all
    /// open-close cycles in this session's lifetime. Snapshotted to
    /// `Session.approval_wait_ms` at finalize.
    pub(super) approval_wait_total_ms: u64,
    /// TD1: monotonic anchor for the current open Running interval (set at
    /// monitor Running-entry, cleared at finalize). `None` outside a Running
    /// interval.
    pub(super) work_run_start: Option<std::time::Instant>,
    /// TD1: accumulated `work_time_ms` floor captured at Running-entry (=
    /// the persisted prior value for this session id). The live total is
    /// always `work_time_base_ms + (run_elapsed − approval_elapsed)`.
    pub(super) work_time_base_ms: u64,
    /// Set to true when the current invocation emits at least one nonempty
    /// assistant message. This remains retry-classifier evidence; terminal
    /// status truth is carried by `TerminalEvidence`.
    pub(super) received_meaningful_output: bool,
    /// Process exit code captured after the provider subprocess exits.
    /// `None` until the process is checked, `Some(code)` after `try_exit_status()`.
    pub(super) exit_code: Option<i32>,
    /// Current retry attempt (0-based). 0 on first run.
    pub(super) retry_attempt: u8,
    /// Maximum retry attempts configured for this session.
    pub(super) max_retries: u8,
    /// Timestamp of the last StreamEvent received by the monitor loop.
    /// Used by the stall detector to compute idle duration. Daemon-internal only.
    pub(crate) last_event_at: chrono::DateTime<chrono::Utc>,
    /// Set to true when the stall detector interrupts this session.
    /// Changes the break reason classification so stall-interrupted sessions
    /// are eligible for retry (unlike user-initiated interrupts).
    pub(crate) stall_interrupted: bool,
    /// Monotonic clock of the last API-reported usage update (i.e., the last
    /// assistant chunk that carried a `usage` block with non-Missing
    /// confidence). Driven by Tokio's monotonic `Instant` — never subject to
    /// wallclock jumps.
    ///
    /// Codex sets this clock only for current context observations, never billing.
    /// Staleness: if `(Instant::now() - last_usage_update) > 60s` on a Claude or Codex
    /// session with status `Running`, `apply_staleness` downgrades
    /// `Full`/`Partial` → `Stale` while preserving the last API-reported
    /// numerator. `None` before the first usage block arrives (cold-start), in
    /// which case the existing `Missing` → `daemon_total` path covers the
    /// initial fallback and `Stale` is not produced.
    pub(super) last_usage_update: Option<tokio::time::Instant>,
    /// RSI-010 validator de-dupe state. Stores the last (declared, actual_model)
    /// pair that was warned about for this session. Prevents warn amplification
    /// when `system/init` is re-emitted for the same session. Reset on process
    /// spawn (no serialization).
    pub(super) last_mismatch_warn: Option<(rsi_common::types::CapabilityClass, String)>,
    /// Timestamp of the most recent stall-classifier verdict, or `None` if
    /// the session has never been classified. Read by the detector tick to
    /// enforce `classifier_cooldown_secs`; written by the classifier
    /// scheduler in Phase 4 after a successful verdict. Reset on daemon
    /// restart (not persisted).
    pub(crate) last_classified_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Lifetime count of classifier verdicts emitted on this session. Read
    /// by the detector tick to enforce `classifier_max_per_session`;
    /// incremented by the classifier scheduler. Reset on daemon restart.
    pub(crate) classification_count: u32,
    /// Most recent verdict (for the `gV` TUI keybinding). Daemon-internal;
    /// surfaced to the TUI via `GetClassificationStatus` RPC in Phase 5.
    pub(crate) last_verdict: Option<crate::stall_classifier::types::Verdict>,
}

impl TrackedSession {
    /// The typed session budget is the sole denominator and rotation authority.
    pub(super) fn context_budget(&self) -> rsi_common::ResolvedContextBudget {
        provider_capabilities::resolved_context_budget_for_session(&self.session)
    }

    pub(super) fn authorizes_threshold_rotation(&self) -> bool {
        self.context_budget().authorizes_threshold_rotation()
    }

    /// Effective working directory for provider/tool invocation. Returns
    /// `sandbox_root` when the session was allocated a sandbox, else the
    /// canonical `working_dir`. This is the single substitution point used
    /// by every `current_dir` / `resolve_sandboxed_path` site.
    #[allow(dead_code)]
    pub fn effective_working_dir(&self) -> &Path {
        self.session
            .sandbox_root
            .as_deref()
            .unwrap_or(self.session.working_dir.as_path())
    }

    /// Test-only constructor: build a minimal `TrackedSession` from a
    /// `Session`. Mirrors the test helper in `hierarchy_ops::tests::tracked`
    /// but is available outside the `session` module so the stall_classifier
    /// input tests can reach it without duplicating the field laundry.
    #[cfg(test)]
    pub(crate) fn new_for_test(session: Session) -> Self {
        let session_id = session.id;
        let (stop_tx, _stop_rx) = tokio::sync::mpsc::channel(1);
        TrackedSession {
            session,
            spawn_generation: 0,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            process: None,
            deferred_successor_start_gate: None,
            stop_tx,
            interrupt_requested: false,
            pending_archive: false,
            rotation: super::rotation_coordinator::RotationCoordinator::new(session_id, 0, false),
            live_input_tokens: 0,
            live_output_tokens: 0,
            live_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: 0,
            daemon_output_tokens: 0,
            daemon_tokens_at_last_api_update: 0,
            codex_context_tokens: 0,
            pipeline_artifact: None,
            memory_flush_compaction_count: None,
            pending_question: None,
            approval_wait_start: None,
            approval_wait_total_ms: 0,
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
        }
    }

    /// Test-only: override `last_event_at` to simulate an idle session.
    #[cfg(test)]
    pub(crate) fn set_last_event_at_for_test(&mut self, ts: chrono::DateTime<chrono::Utc>) {
        self.last_event_at = ts;
    }

    /// Flatten the active `AskUserQuestion` (if any) to a single-string
    /// summary the stall classifier prompt can ingest. Daemon-internal
    /// helper for `stall_classifier::input::build_classification_input`.
    /// Returns `None` when no question is open.
    pub(crate) fn pending_question_text(&self) -> Option<String> {
        let pq = self.pending_question.as_ref()?;
        if pq.questions.is_empty() {
            return None;
        }
        let mut s = String::new();
        for (i, q) in pq.questions.iter().enumerate() {
            if i > 0 {
                s.push('\n');
            }
            // Header is a short tag (≤12 chars), question is the prompt.
            // Combine compactly so the classifier sees both signals.
            if !q.header.is_empty() {
                s.push_str(&q.header);
                s.push_str(": ");
            }
            s.push_str(q.question.trim());
        }
        Some(s)
    }

    /// TD1: recompute the accumulated active-work floor from the current open
    /// Running interval, subtracting approval-wait. Idempotent recompute (not a
    /// delta), so ticks are safe to run any number of times. Monotonic: run_elapsed
    /// grows at wall rate; approval grows only while a question is open and is always
    /// a sub-interval of the run, so (run − approval) is non-decreasing. No-op when
    /// `work_run_start` is `None` (outside a Running interval) — keeps the session's
    /// last-known floor untouched.
    pub(super) fn recompute_work_time(&mut self) {
        if let Some(run_start) = self.work_run_start {
            let run_ms = run_start.elapsed().as_millis().min(u64::MAX as u128) as u64;
            let open_approval = self
                .approval_wait_start
                .map(|s| s.elapsed().as_millis().min(u64::MAX as u128) as u64)
                .unwrap_or(0);
            let approval_ms = self.approval_wait_total_ms.saturating_add(open_approval);
            let work = run_ms.saturating_sub(approval_ms);
            self.session.work_time_ms = Some(self.work_time_base_ms.saturating_add(work));
        }
    }
}

/// Process handle for a harness session (task-based, like OpenAiProcess).
pub struct HarnessProcess {
    pub(crate) task_handle: tokio::task::JoinHandle<()>,
    pub(crate) cancel: tokio_util::sync::CancellationToken,
}

impl HarnessProcess {
    pub(crate) fn interrupt(&self) -> Result<()> {
        self.cancel.cancel();
        Ok(())
    }

    pub(crate) async fn kill(&mut self) -> Result<()> {
        self.cancel.cancel();
        self.task_handle.abort();
        Ok(())
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task_handle.is_finished()
    }
}

/// Deterministic process seam for session lifecycle tests.
#[cfg(test)]
pub struct ScriptedProcess {
    pub(crate) alive: Arc<AtomicBool>,
    pub(crate) exit_code: Arc<AtomicI32>,
    pub(crate) interrupt_count: Arc<AtomicUsize>,
    pub(crate) kill_count: Arc<AtomicUsize>,
    pub(crate) exit_on_interrupt: bool,
    pub(crate) interrupt_fails: bool,
    pub(crate) kill_fails: bool,
}

#[cfg(test)]
impl ScriptedProcess {
    fn interrupt(&self) -> Result<()> {
        self.interrupt_count.fetch_add(1, Ordering::SeqCst);
        if self.interrupt_fails {
            return Err(crate::error::DaemonError::Process(
                "scripted interrupt failure".to_string(),
            ));
        }
        if self.exit_on_interrupt {
            self.exit_code.store(0, Ordering::SeqCst);
            self.alive.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    fn kill(&self) -> Result<()> {
        self.kill_count.fetch_add(1, Ordering::SeqCst);
        if self.kill_fails {
            return Err(crate::error::DaemonError::Process(
                "scripted kill failure".to_string(),
            ));
        }
        self.exit_code.store(-1, Ordering::SeqCst);
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn try_exit_status(&self) -> Option<i32> {
        (!self.is_alive()).then(|| self.exit_code.load(Ordering::SeqCst))
    }
}

pub enum ProviderProcess {
    Claude(ClaudeProcess),
    Codex(CodexProcess),
    Local(OpenAiProcess),
    Antigravity(AgyProcess),
    /// Codex running in app-server bidirectional JSON-RPC mode.
    CodexAppServer(CodexAppServerProcess),
    /// Direct API harness — task-based, no subprocess.
    Harness(HarnessProcess),
    #[cfg(test)]
    Scripted(ScriptedProcess),
}

impl ProviderProcess {
    /// Request graceful interruption of the process/task.
    /// `pub(crate)` for stall detector and reconciliation module access.
    pub(crate) fn interrupt(&self) -> Result<()> {
        match self {
            Self::Claude(p) => p.interrupt(),
            Self::Codex(p) => p.interrupt(),
            Self::Local(p) => p.interrupt(),
            Self::Antigravity(p) => p.interrupt(),
            Self::CodexAppServer(p) => p.interrupt(),
            Self::Harness(p) => p.interrupt(),
            #[cfg(test)]
            Self::Scripted(p) => p.interrupt(),
        }
    }

    pub(super) async fn kill(&mut self) -> Result<()> {
        match self {
            Self::Claude(p) => p.kill().await,
            Self::Codex(p) => p.kill().await,
            Self::Local(p) => p.kill().await,
            Self::Antigravity(p) => p.kill().await,
            Self::CodexAppServer(p) => p.kill().await,
            Self::Harness(p) => p.kill().await,
            #[cfg(test)]
            Self::Scripted(p) => p.kill(),
        }
    }

    /// Non-blocking liveness check. Returns `true` if the process/task is still running.
    /// For subprocess providers: calls `try_wait()` and returns `true` if no exit status yet.
    /// For task providers (Local, Harness): calls `is_finished()` on the JoinHandle.
    pub(crate) fn is_alive(&mut self) -> bool {
        match self {
            Self::Claude(p) => p.try_wait().ok().flatten().is_none(),
            Self::Codex(p) => p.try_wait().ok().flatten().is_none(),
            Self::Antigravity(p) => p.try_wait().ok().flatten().is_none(),
            Self::Local(p) => !p.is_finished(),
            Self::CodexAppServer(p) => p.try_wait().ok().flatten().is_none(),
            Self::Harness(p) => !p.is_finished(),
            #[cfg(test)]
            Self::Scripted(p) => p.is_alive(),
        }
    }

    /// Non-blocking check for process exit status.
    /// Returns `Some(exit_code)` if the process has exited, `None` if still running
    /// or if the provider doesn't use a subprocess (e.g., OpenAI API task).
    pub(super) fn try_exit_status(&mut self) -> Option<i32> {
        let status = match self {
            Self::Claude(p) => p.try_wait().ok()?,
            Self::Codex(p) => p.try_wait().ok()?,
            Self::Antigravity(p) => p.try_wait().ok()?,
            Self::CodexAppServer(p) => p.try_wait().ok()?,
            // Task-based providers use tokio tasks, not subprocesses
            Self::Local(_) | Self::Harness(_) => return None,
            #[cfg(test)]
            Self::Scripted(p) => return p.try_exit_status(),
        };
        status.map(|s| s.code().unwrap_or(-1))
    }
}

pub(super) fn install_context_budget(
    session: &mut Session,
    budget: rsi_common::ResolvedContextBudget,
) {
    session.context_window = Some(budget.active_tokens);
    session.resolved_context_budget = Some(budget);
}

pub(super) fn sanitize_codex_restored_context_usage(
    session: &mut Session,
    turn_metrics: &[TurnMetric],
) {
    if !matches!(
        session.provider,
        SessionProvider::Codex
            | SessionProvider::Pioneer
            | SessionProvider::OpenRouter
            | SessionProvider::Bedrock
            | SessionProvider::CodexAppServer
    ) {
        return;
    }

    let Some(stored_total) = session.total_input_tokens else {
        return;
    };
    if stored_total == 0 {
        return;
    }

    let has_legacy_cache = session.total_cache_read_tokens.unwrap_or(0) > 0
        || turn_metrics
            .iter()
            .any(|metric| metric.cache_read_tokens > 0);
    if !has_legacy_cache {
        return;
    }

    let max_uncached_input = turn_metrics
        .iter()
        .map(|metric| metric.input_tokens)
        .max()
        .unwrap_or(0);
    if max_uncached_input == 0 || max_uncached_input >= stored_total {
        return;
    }

    // Older Codex mapping stored cumulative cached_input_tokens as additive
    // cache-read context, inflating total_input_tokens into the millions. Keep
    // completed-session hydration and list output on the same normalized basis
    // as new turn.completed events without rewriting historical turn metrics.
    session.input_tokens = Some(max_uncached_input);
    session.total_input_tokens = Some(max_uncached_input);
    session.total_cache_read_tokens = Some(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MonitorBreakReason {
    Result,
    StreamClosed,
    Interrupted,
    Rotation,
    /// Session was interrupted due to stall detection (eligible for retry).
    StallTimeout,
}

/// Invocation-local semantic evidence carried by a normalized result event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalResult {
    None,
    Success,
    ProviderError,
}

/// Multi-turn disposition at the terminal candidate boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalTurnOutcome {
    NotMultiTurn,
    Continued,
    Terminal,
}

/// Whether the rotation coordinator has a post-finalization action to own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalRotationAction {
    None,
    PostFinalize,
}

/// How the process settlement helper should treat the provider before its
/// interrupt grace window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessSettlementMode {
    /// Permit a bounded natural-exit window, then request graceful interrupt.
    AwaitNatural,
    /// Request graceful interrupt immediately.
    InterruptNow,
    /// A lifecycle owner already requested graceful interrupt.
    AlreadyInterrupted,
}

/// Exhaustive outcome of settling the tracked provider handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessSettlementOutcome {
    NoHandle,
    AlreadyExited,
    ExitedNaturally,
    ExitedAfterInterrupt,
    Escalated,
    EscalatedAfterInterruptFailure,
    GenerationChanged,
    EscalationFailed,
}

impl ProcessSettlementOutcome {
    pub(super) const fn is_settled(self) -> bool {
        matches!(
            self,
            Self::NoHandle
                | Self::AlreadyExited
                | Self::ExitedNaturally
                | Self::ExitedAfterInterrupt
                | Self::Escalated
                | Self::EscalatedAfterInterruptFailure
        )
    }
}

/// Complete monitor-owned evidence for one terminal decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TerminalEvidence {
    pub(super) expected_generation: u64,
    pub(super) active_generation: Option<u64>,
    pub(super) break_reason: MonitorBreakReason,
    pub(super) received_any_event: bool,
    pub(super) received_meaningful_output: bool,
    pub(super) prior_meaningful_output: bool,
    pub(super) current_result: TerminalResult,
    pub(super) process_handle_present: bool,
    pub(super) process_alive: bool,
    pub(super) exit_code: Option<i32>,
    pub(super) stream_drained: bool,
    pub(super) settlement: ProcessSettlementOutcome,
    pub(super) supports_multi_turn: bool,
    pub(super) turn_outcome: TerminalTurnOutcome,
    pub(super) pending_archive: bool,
    pub(super) stall_interrupted: bool,
    pub(super) interrupt_requested: bool,
    pub(super) pending_question: bool,
    pub(super) rotation_action: TerminalRotationAction,
}

/// Settled terminal truth accepted by `finalize_session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TerminalFinalizeDecision {
    pub(super) status: SessionStatus,
    pub(super) c5_failure_cause: Option<crate::store::daemon_settings::AutofileCause>,
}

impl TerminalFinalizeDecision {
    pub(super) const fn completed() -> Self {
        Self {
            status: SessionStatus::Completed,
            c5_failure_cause: None,
        }
    }

    pub(super) const fn interrupted() -> Self {
        Self {
            status: SessionStatus::Interrupted,
            c5_failure_cause: None,
        }
    }

    pub(super) const fn waiting_approval() -> Self {
        Self {
            status: SessionStatus::WaitingApproval,
            c5_failure_cause: None,
        }
    }

    pub(super) const fn failed(cause: crate::store::daemon_settings::AutofileCause) -> Self {
        Self {
            status: SessionStatus::Failed,
            c5_failure_cause: Some(cause),
        }
    }

    /// Revalidate lifecycle intent at the final active-map removal boundary.
    /// The monitor's settled provider evidence is stable, but archive, stall,
    /// interrupt, and question writers can legitimately queue behind its
    /// earlier snapshot. Their established precedence must therefore be
    /// applied while the matching generation is still atomically removed.
    pub(super) const fn with_current_lifecycle_intent(
        self,
        pending_archive: bool,
        stall_interrupted: bool,
        interrupt_requested: bool,
        pending_question: bool,
    ) -> Self {
        if pending_archive {
            Self::completed()
        } else if stall_interrupted {
            Self::failed(crate::store::daemon_settings::AutofileCause::StallTimeout)
        } else if interrupt_requested {
            Self::interrupted()
        } else if pending_question {
            Self::waiting_approval()
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalRetentionReason {
    OwnershipUnsettled,
    StreamNotDrained,
    ProviderStillAlive,
    TurnContinues,
    /// The provider is known dead, but its event producer still owns a sender.
    /// The monitor remains the sole receiver and retries bounded recovery
    /// rather than fabricating terminal persistence.
    ProducerNotClosed,
}

/// One authoritative outcome from monitor evidence. Retry policy is
/// intentionally absent: it consumes only a durable `Failed` finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalDecision {
    StaleGeneration,
    RetainRunning(TerminalRetentionReason),
    Finalize(TerminalFinalizeDecision),
}

pub(super) const ROTATION_HANDOFF_PROMPT: &str = r#"/create_handoff"#;

pub(super) const PERSISTENCE_QUEUE_CAPACITY: usize = 256;
pub(super) const PERSISTENCE_WARN_THRESHOLD: f32 = 0.8;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MetadataWriteCaller {
    Finalizer,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MetadataBarrierScope {
    pub session_id: Uuid,
    pub caller: MetadataWriteCaller,
}

impl MetadataBarrierScope {
    pub(super) const fn finalizer(session_id: Uuid) -> Self {
        Self {
            session_id,
            caller: MetadataWriteCaller::Finalizer,
        }
    }
}

#[derive(Debug)]
pub enum StoreCommand {
    InsertSession {
        session: Session,
    },
    /// Response-bearing FIFO fence. The worker performs no Store mutation;
    /// a scoped response reports that caller's failed metadata writes.
    Barrier {
        metadata_scope: Option<MetadataBarrierScope>,
        respond_to: tokio::sync::oneshot::Sender<crate::error::Result<()>>,
    },
    InsertEvent {
        event: ConversationEvent,
        provenance: Option<rsi_common::closure_kernel::ConversationEventProvenanceV1>,
        respond_to: tokio::sync::oneshot::Sender<Result<i64>>,
    },
    InsertSessionDiagnostic {
        diagnostic: NewSessionDiagnosticV1,
        respond_to: tokio::sync::oneshot::Sender<Result<i64>>,
    },
    ResolveAppServerApproval {
        event: ConversationEvent,
        target: serde_json::Value,
        resolution: serde_json::Value,
        respond_to: tokio::sync::oneshot::Sender<crate::error::Result<(bool, bool)>>,
    },
    PublishAppServerApproval {
        event: ConversationEvent,
        target: serde_json::Value,
        respond_to: tokio::sync::oneshot::Sender<crate::error::Result<i64>>,
    },
    PublishQuestionEvent {
        event: ConversationEvent,
        provenance: Option<rsi_common::closure_kernel::ConversationEventProvenanceV1>,
        question: rsi_common::types::PendingQuestion,
        respond_to: tokio::sync::oneshot::Sender<Result<i64>>,
    },
    UpdateSessionStatus {
        session_id: Uuid,
        status: rsi_common::types::SessionStatus,
    },
    /// Acknowledged C5 terminal failure write: status and the durable pending
    /// auto-file marker commit together before a post-disposition feed runs.
    UpdateFailedAndStageAutofile {
        session_id: Uuid,
        cause: crate::store::daemon_settings::AutofileCause,
        respond_to: tokio::sync::oneshot::Sender<
            crate::store::daemon_settings::C5TransitionResult<
                crate::store::daemon_settings::C5StageOutcome,
            >,
        >,
    },
    /// Acknowledged archival transition: status and C5 marker resolution
    /// commit in the same Store transaction.
    ArchiveAndResolveAutofile {
        session_id: Uuid,
        respond_to: tokio::sync::oneshot::Sender<
            crate::store::daemon_settings::C5TransitionResult<
                crate::store::daemon_settings::C5SuppressionOutcome,
            >,
        >,
    },
    UpdatePendingQuestion {
        session_id: Uuid,
        pending_question_json: Option<String>,
    },
    UpdateSessionKind {
        session_id: Uuid,
        kind: rsi_common::types::SessionKind,
    },
    InsertContextSnapshot {
        session_id: Uuid,
        tokens: u64,
    },
    InsertTurnMetric {
        metric: TurnMetric,
    },
    UpdateClaudeSessionId {
        session_id: Uuid,
        claude_session_id: String,
    },
    UpdateSessionMetadata {
        session: Session,
        barrier_scope: Option<MetadataBarrierScope>,
        respond_to: tokio::sync::oneshot::Sender<crate::error::Result<()>>,
    },
    UpdateSessionProject {
        session_id: Uuid,
        project_id: Option<Uuid>,
    },
    UpdateSessionTitle {
        session_id: Uuid,
        title: String,
    },
    /// Generated-title enrichment: fill the title only while it is absent so
    /// asynchronous generation cannot overwrite an explicit title.
    FillSessionTitleIfAbsent {
        session_id: Uuid,
        title: String,
        respond_to: tokio::sync::oneshot::Sender<Result<bool>>,
    },
    UpdateSessionDescription {
        session_id: Uuid,
        description: String,
    },
    UpdateSessionRating {
        session_id: Uuid,
        rating: Option<i16>,
    },
    UpdateSessionActiveTask {
        session_id: Uuid,
        active_task: Option<String>,
    },
    DeleteSession {
        session_id: Uuid,
        respond_to: tokio::sync::oneshot::Sender<Result<()>>,
    },
    SoftDeleteSession {
        session_id: Uuid,
        respond_to: tokio::sync::oneshot::Sender<Result<()>>,
    },
    PurgeSession {
        session_id: Uuid,
        respond_to: tokio::sync::oneshot::Sender<Result<()>>,
    },
    UndeleteSession {
        session_id: Uuid,
        respond_to: tokio::sync::oneshot::Sender<Result<Option<Session>>>,
    },
    TogglePin {
        session_id: Uuid,
        respond_to: tokio::sync::oneshot::Sender<Result<Option<String>>>,
    },
    ToggleTestingNeeded {
        session_id: Uuid,
        respond_to: tokio::sync::oneshot::Sender<Result<Option<String>>>,
    },
    ToggleRotationDisabled {
        session_id: Uuid,
        respond_to: tokio::sync::oneshot::Sender<Result<Option<String>>>,
    },
    InsertProject {
        project: rsi_common::types::Project,
    },
    UpdateProjectRow {
        project: rsi_common::types::Project,
    },
    DeleteProject {
        project_id: Uuid,
    },
    UnarchiveSession {
        session_id: Uuid,
        respond_to: tokio::sync::oneshot::Sender<Result<Option<Session>>>,
    },
    UpdateWorkflowStage {
        workflow_id: Uuid,
        stage: WorkflowStage,
        artifact_path: Option<String>,
    },
    UpdateSessionWorkflow {
        session_id: Uuid,
        workflow_id: Option<Uuid>,
    },
    InsertRotationEvent {
        session_id: Uuid,
        rotation_id: String,
        phase: String,
        event_type: String,
        metadata: Option<String>,
    },
    InsertLabel {
        label: rsi_common::types::SessionLabel,
    },
    UpdateLabelRow {
        label: rsi_common::types::SessionLabel,
    },
    DeleteLabel {
        label_id: Uuid,
    },
    InsertTopology {
        topology: rsi_common::types::Topology,
    },
    UpdateTopologyRow {
        topology: rsi_common::types::Topology,
    },
    DeleteTopology {
        topology_id: Uuid,
    },
    UpdateSessionLabel {
        session_id: Uuid,
        group_id: Option<Uuid>,
    },
    UpdatePendingArchive {
        session_id: Uuid,
        pending_archive: bool,
    },
    InsertSessionSummary {
        summary: rsi_common::types::SessionSummary,
    },
    UpsertEntityCard {
        card: rsi_common::types::EntityCard,
    },
    UpdateRetryState {
        session_id: Uuid,
        retry_attempt: Option<u8>,
        max_retries: Option<u8>,
    },
    OffloadEventContent {
        session_id: Uuid,
        event_sequence: i32,
        content_hash: String,
        original_content: String,
    },
    UpdateEventContent {
        event_id: i64,
        new_content: String,
    },
    UpdateSandboxCleanupState {
        session_id: Uuid,
        state: Option<rsi_common::types::SandboxCleanupState>,
    },
    /// Atomic tombstone after successful sandbox teardown: clears
    /// `sandbox_root` and `sandbox_branch`, and stamps
    /// `sandbox_cleanup_state = 'Purged'` in one SQL statement.
    MarkSandboxPurged {
        session_id: Uuid,
    },
}

#[derive(Clone)]
pub struct PersistenceHandle {
    pub(super) tx: mpsc::Sender<StoreCommand>,
    pub(super) pending: Arc<AtomicUsize>,
    pub(super) last_command_duration_ms: Arc<AtomicU64>,
    pub(super) capacity: usize,
}

// Note: StreamEvent re-export for submodules that need it
// Suppress unused import warning — StreamEvent is used directly via crate::claude::StreamEvent
