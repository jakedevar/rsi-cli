//! Row structs, enum converters, and timestamp parsing for store persistence.

use crate::error::{DaemonError, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rsi_common::provider_capabilities::{
    CapabilityConfidence, CapabilityEvidence, CapabilitySource, ContextCapacity,
    ResolvedContextBudget,
};
use rsi_common::types::{
    ApprovalStatus, AutonomyPolicy, CapabilityClass, Capture, CaptureSourceKind,
    ContentAddressedRef, ContextUsageConfidence, EspGame, EventType, Idea, IdeaActorKind,
    IdeaCollection, IdeaCollectionMembership, IdeaCompatibilityMapping, IdeaCompatibilityStatus,
    IdeaEvent, IdeaEventType, IdeaLifecycle, IdeaRelationship, IdeaRelationshipKind, IdeaStage,
    LegacyIdeaSourceKind, ModelSegment, PendingQuestion, Project, Role, SandboxCleanupState,
    SandboxKind, Session, SessionKind, SessionProvider, SessionStatus, Sha256Digest, TurnMetric,
    Workflow, WorkflowStage,
};
use uuid::Uuid;

/// Parse a timestamp string leniently: tries RFC 3339 first, then the legacy
/// comma-fraction RFC 3339 form, and finally SQLite's `datetime()` format
/// (`YYYY-MM-DD HH:MM:SS`). This prevents a malformed persisted value from
/// bricking session-list loading.
pub fn parse_timestamp(s: &str) -> std::result::Result<DateTime<Utc>, String> {
    // Fast path: well-formed RFC 3339 (what the daemon writes)
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Some legacy rows use a comma instead of a period before RFC 3339 fractional
    // seconds (for example `2026-08-02T16:15:11,769711900-07:00`). Normalize
    // only that exact date-time separator shape, never arbitrary commas.
    if let Some((datetime, fraction_and_offset)) = s.split_once(',')
        && NaiveDateTime::parse_from_str(datetime, "%Y-%m-%dT%H:%M:%S").is_ok()
        && fraction_and_offset
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_digit)
    {
        let normalized = format!("{datetime}.{fraction_and_offset}");
        if let Ok(dt) = DateTime::parse_from_rfc3339(&normalized) {
            return Ok(dt.with_timezone(&Utc));
        }
    }
    // Fallback: SQLite datetime() format "YYYY-MM-DD HH:MM:SS"
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(naive.and_utc());
    }
    Err(format!("Invalid timestamp: {s}"))
}

pub(crate) const SESSION_COLUMNS: &str = "id, claude_session_id, query, working_dir, status, project_id, \
    pinned_at, created_at, updated_at, cost_usd, duration_ms, num_turns, model, input_tokens, \
    output_tokens, context_window, total_input_tokens, total_output_tokens, \
    total_cache_creation_tokens, total_cache_read_tokens, stop_reason, session_kind, \
    continued_from, provider, handoff_filepath, rotation_depth, daemon_input_tokens, \
    daemon_output_tokens, title, description, pipeline_artifact, workflow_id, git_branch, active_task, group_id, \
    pending_archive, testing_needed_at, rotation_disabled_at, effort, retry_attempt, max_retries, \
    issue_identifier, issue_url, issue_tracker_id, scheduled_job_id, \
    rating, harness_version_hash, test_passed, clippy_passed, turn_count, retry_count, \
    sandbox_kind, sandbox_root, sandbox_branch, sandbox_cleanup_state, parent_id, \
    approval_wait_ms, lead_session_id, is_eval, capability_class, \
    topology_node_id, topology_iteration, pending_question_json, work_time_ms, \
    provider_cli_version, provider_capabilities, thinking_tokens, service_tier, \
    cache_creation_1h_tokens, cache_creation_5m_tokens, permission_denial_count, \
    subagent_stats_json, queued_turn_count, terminal_reason, \
    context_window_source, context_window_source_version, context_window_source_digest, \
    context_window_observed_at, context_window_configured_tokens, agent_role, epic_spawn_ordinal";

/// Number of columns `SESSION_COLUMNS` selects, and therefore the number
/// `map_session_row` consumes.
///
/// A query that appends its own columns after `SESSION_COLUMNS` must index them
/// from this constant, never from a hardcoded literal. V99 added ten session
/// columns and silently shifted every such trailing index by ten; the resulting
/// failure surfaced far away as `sandbox_custody:persistence_transition_failed`,
/// because a `String` read landed on a nullable INTEGER column. Deriving the
/// offset makes the next column addition a compile-time-stable change instead
/// of a latent one.
pub(crate) const SESSION_COLUMN_COUNT: usize = 81;

pub(crate) fn map_session_row(row: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id_str: row.get(0)?,
        claude_session_id: row.get(1)?,
        query: row.get(2)?,
        working_dir_str: row.get(3)?,
        status_str: row.get(4)?,
        project_id_str: row.get(5)?,
        pinned_at_str: row.get(6)?,
        created_at_str: row.get(7)?,
        updated_at_str: row.get(8)?,
        cost_usd: row.get(9)?,
        duration_ms: row.get(10)?,
        num_turns: row.get(11)?,
        model: row.get(12)?,
        input_tokens: row.get(13)?,
        output_tokens: row.get(14)?,
        context_window: row.get(15)?,
        total_input_tokens: row.get(16)?,
        total_output_tokens: row.get(17)?,
        total_cache_creation_tokens: row.get(18)?,
        total_cache_read_tokens: row.get(19)?,
        stop_reason: row.get(20)?,
        session_kind_str: row.get(21)?,
        continued_from_str: row.get(22)?,
        provider_str: row.get(23)?,
        handoff_filepath: row.get(24)?,
        rotation_depth: row.get(25)?,
        daemon_input_tokens: row.get(26)?,
        daemon_output_tokens: row.get(27)?,
        title: row.get(28)?,
        description: row.get(29)?,
        pipeline_artifact: row.get(30)?,
        workflow_id_str: row.get(31)?,
        git_branch: row.get(32)?,
        active_task: row.get(33)?,
        group_id_str: row.get(34)?,
        pending_archive: row.get(35)?,
        testing_needed_at_str: row.get(36)?,
        rotation_disabled_at_str: row.get(37)?,
        effort: row.get(38)?,
        retry_attempt: row.get(39)?,
        max_retries: row.get(40)?,
        issue_identifier: row.get(41)?,
        issue_url: row.get(42)?,
        issue_tracker_id: row.get(43)?,
        scheduled_job_id_str: row.get(44)?,
        rating: row.get(45)?,
        harness_version_hash: row.get(46)?,
        test_passed: row.get(47)?,
        clippy_passed: row.get(48)?,
        turn_count: row.get(49)?,
        retry_count: row.get(50)?,
        sandbox_kind_str: row.get(51)?,
        sandbox_root_str: row.get(52)?,
        sandbox_branch: row.get(53)?,
        sandbox_cleanup_state_str: row.get(54)?,
        parent_id_str: row.get(55)?,
        approval_wait_ms: row.get(56)?,
        lead_session_id_str: row.get(57)?,
        is_eval: row.get(58)?,
        capability_class_str: row.get(59)?,
        topology_node_id: row.get(60)?,
        topology_iteration: row.get(61)?,
        pending_question_json: row.get(62)?,
        work_time_ms: row.get(63)?,
        provider_cli_version: row.get(64)?,
        provider_capabilities_json: row.get(65)?,
        thinking_tokens: row.get(66)?,
        service_tier: row.get(67)?,
        cache_creation_1h_tokens: row.get(68)?,
        cache_creation_5m_tokens: row.get(69)?,
        permission_denial_count: row.get(70)?,
        subagent_stats_json: row.get(71)?,
        queued_turn_count: row.get(72)?,
        terminal_reason: row.get(73)?,
        context_window_source: row.get(74)?,
        context_window_source_version: row.get(75)?,
        context_window_source_digest: row.get(76)?,
        context_window_observed_at: row.get(77)?,
        context_window_configured_tokens: row.get(78)?,
        agent_role: row.get(79)?,
        epic_spawn_ordinal: row.get(80)?,
    })
}

/// Intermediate row for reading sessions from the database.
/// Separates rusqlite row extraction (infallible) from domain parsing (fallible).
pub(crate) struct SessionRow {
    pub id_str: String,
    pub claude_session_id: Option<String>,
    pub query: String,
    pub working_dir_str: String,
    pub status_str: String,
    pub project_id_str: Option<String>,
    pub pinned_at_str: Option<String>,
    pub created_at_str: String,
    pub updated_at_str: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i64>,
    pub num_turns: Option<i32>,
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub context_window: Option<i64>,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub total_cache_creation_tokens: Option<i64>,
    pub total_cache_read_tokens: Option<i64>,
    pub stop_reason: Option<String>,
    pub session_kind_str: Option<String>,
    pub continued_from_str: Option<String>,
    pub provider_str: Option<String>,
    pub handoff_filepath: Option<String>,
    pub rotation_depth: i64,
    pub daemon_input_tokens: Option<i64>,
    pub daemon_output_tokens: Option<i64>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub pipeline_artifact: Option<String>,
    pub workflow_id_str: Option<String>,
    pub git_branch: Option<String>,
    pub active_task: Option<String>,
    pub group_id_str: Option<String>,
    pub pending_archive: i64,
    pub testing_needed_at_str: Option<String>,
    pub rotation_disabled_at_str: Option<String>,
    pub effort: Option<String>,
    pub retry_attempt: Option<i64>,
    pub max_retries: Option<i64>,
    pub issue_identifier: Option<String>,
    pub issue_url: Option<String>,
    pub issue_tracker_id: Option<String>,
    pub scheduled_job_id_str: Option<String>,
    pub rating: Option<i64>,
    pub harness_version_hash: Option<String>,
    pub test_passed: Option<i64>,
    pub clippy_passed: Option<i64>,
    pub turn_count: Option<i64>,
    pub retry_count: Option<i64>,
    pub sandbox_kind_str: Option<String>,
    pub sandbox_root_str: Option<String>,
    pub sandbox_branch: Option<String>,
    pub sandbox_cleanup_state_str: Option<String>,
    pub parent_id_str: Option<String>,
    pub approval_wait_ms: Option<i64>,
    pub lead_session_id_str: Option<String>,
    /// Eval-replay marker. Encoded as INTEGER in SQLite (0 or 1). See V44 / RSI-006.
    pub is_eval: i64,
    pub capability_class_str: Option<String>,
    pub topology_node_id: Option<String>,
    /// Stored as INTEGER in SQLite; converted to u32 in into_session().
    pub topology_iteration: i64,
    pub pending_question_json: Option<String>,
    /// TD1: accumulated active-work milliseconds. NULL = unmeasured (pre-V70
    /// row, or a container that never entered the monitor loop).
    pub work_time_ms: Option<i64>,
    // === V99 provider handshake (P1-A) ===
    pub provider_cli_version: Option<String>,
    /// `capabilities` from `system/init`, stored as a JSON array string.
    pub provider_capabilities_json: Option<String>,
    // === V99 richer usage capture (P1-C) ===
    pub thinking_tokens: Option<i64>,
    pub service_tier: Option<String>,
    pub cache_creation_1h_tokens: Option<i64>,
    pub cache_creation_5m_tokens: Option<i64>,
    pub permission_denial_count: Option<i64>,
    pub subagent_stats_json: Option<String>,
    pub queued_turn_count: Option<i64>,
    pub terminal_reason: Option<String>,
    pub context_window_source: Option<String>,
    pub context_window_source_version: Option<String>,
    pub context_window_source_digest: Option<String>,
    pub context_window_observed_at: Option<String>,
    pub context_window_configured_tokens: Option<i64>,
    // === V102 durable Operator Views identity convergence ===
    pub agent_role: Option<String>,
    pub epic_spawn_ordinal: Option<i64>,
}

impl SessionRow {
    pub fn into_session(self) -> Result<Session> {
        let id = Uuid::parse_str(&self.id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid UUID: {}", e)))?;
        let status = str_to_session_status(&self.status_str)?;
        let project_id = self
            .project_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid project UUID: {}", e)))?;
        let created_at = parse_timestamp(&self.created_at_str).map_err(DaemonError::Store)?;
        let updated_at = parse_timestamp(&self.updated_at_str).map_err(DaemonError::Store)?;

        let pinned_at = self
            .pinned_at_str
            .as_deref()
            .map(|s| parse_timestamp(s).map_err(DaemonError::Store))
            .transpose()?;

        let testing_needed_at = self
            .testing_needed_at_str
            .as_deref()
            .map(|s| parse_timestamp(s).map_err(DaemonError::Store))
            .transpose()?;

        let rotation_disabled_at = self
            .rotation_disabled_at_str
            .as_deref()
            .map(|s| parse_timestamp(s).map_err(DaemonError::Store))
            .transpose()?;

        let session_kind = self
            .session_kind_str
            .as_deref()
            .map(str_to_session_kind)
            .transpose()?
            .unwrap_or(SessionKind::Standard);
        let provider = self
            .provider_str
            .as_deref()
            .map(str_to_session_provider)
            .transpose()?
            .unwrap_or(rsi_common::types::SessionProvider::Claude);

        let continued_from = self
            .continued_from_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid continued_from UUID: {}", e)))?;

        let workflow_id = self
            .workflow_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid workflow_id UUID: {}", e)))?;

        let group_id = self
            .group_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid group_id UUID: {}", e)))?;

        let scheduled_job_id = self
            .scheduled_job_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid scheduled_job_id UUID: {}", e)))?;

        let parent_id = self
            .parent_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid parent_id UUID: {}", e)))?;

        let lead_session_id = self
            .lead_session_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid lead_session_id UUID: {}", e)))?;

        let sandbox_kind = self
            .sandbox_kind_str
            .as_deref()
            .map(str_to_sandbox_kind)
            .transpose()?;
        let sandbox_cleanup_state = self
            .sandbox_cleanup_state_str
            .as_deref()
            .map(str_to_sandbox_cleanup_state)
            .transpose()?;
        let sandbox_root = self.sandbox_root_str.map(std::path::PathBuf::from);
        let capability_class = self
            .capability_class_str
            .as_deref()
            .map(str_to_capability_class)
            .transpose()?;
        let pending_question = self.pending_question_json.as_deref().and_then(|json| {
            match serde_json::from_str::<PendingQuestion>(json) {
                Ok(question) => Some(question),
                Err(e) => {
                    tracing::warn!(
                        session_id = %id,
                        error = %e,
                        "Ignoring malformed pending_question_json"
                    );
                    None
                }
            }
        });
        let epic_spawn_ordinal = self
            .epic_spawn_ordinal
            .map(|value| {
                u32::try_from(value)
                    .map_err(|_| DaemonError::Store(format!("Invalid Epic spawn ordinal: {value}")))
            })
            .transpose()?;
        let context_window = self
            .context_window
            .map(|value| {
                u64::try_from(value).map_err(|_| {
                    DaemonError::Store("persisted context window must not be negative".to_string())
                })
            })
            .transpose()?;
        let configured_tokens = self
            .context_window_configured_tokens
            .map(|value| {
                let value = u64::try_from(value).map_err(|_| {
                    DaemonError::Store(
                        "persisted configured context window must be positive".to_string(),
                    )
                })?;
                if value == 0 {
                    return Err(DaemonError::Store(
                        "persisted configured context window must be positive".to_string(),
                    ));
                }
                Ok(value)
            })
            .transpose()?;
        let resolved_context_budget = match (
            context_window,
            self.context_window_source.as_deref(),
            self.context_window_source_version,
            self.context_window_source_digest,
            self.context_window_observed_at,
            configured_tokens,
        ) {
            (None, None, None, None, None, None) => None,
            // V99 deliberately preserved scalar values from historical rows. A
            // legacy zero cannot construct the positive-token capability DTO,
            // but keeping the scalar readable lets the resolver replace it with
            // a safe provider-derived budget on the next incarnation.
            (Some(0), Some("legacy_unverified"), None, None, None, None) => None,
            (Some(0), _, _, _, _, _) => {
                return Err(DaemonError::Store(
                    "zero persisted context window requires scalar-only legacy provenance"
                        .to_string(),
                ));
            }
            (
                Some(active_tokens),
                Some(source),
                source_version,
                source_digest,
                observed_at,
                configured_tokens,
            ) => {
                let source = CapabilitySource::parse(source).map_err(DaemonError::Store)?;
                let observed_at = observed_at
                    .as_deref()
                    .map(|value| parse_timestamp(value).map_err(DaemonError::Store))
                    .transpose()?;
                let confidence =
                    match source {
                        CapabilitySource::RuntimeTelemetry | CapabilitySource::Configured => {
                            CapabilityConfidence::Authoritative
                        }
                        CapabilitySource::OfficialDocumentation
                        | CapabilitySource::ProviderCatalog => CapabilityConfidence::Verified,
                        CapabilitySource::RepositoryFallback
                        | CapabilitySource::LegacyUnverified => CapabilityConfidence::Degraded,
                    };
                // Raw configured intent is independent of the evidence that
                // selected the current active denominator. Discovery-only
                // provider incarnations therefore reopen with the raw value
                // intact and degraded active provenance.
                let mut capacity = ContextCapacity::default();
                capacity.configured_tokens = configured_tokens;
                Some(ResolvedContextBudget {
                    active_tokens,
                    capacity,
                    evidence: CapabilityEvidence {
                        source,
                        source_version,
                        source_digest,
                        observed_at,
                        confidence,
                    },
                })
            }
            _ => {
                return Err(DaemonError::Store(
                    "incoherent persisted context-window provenance tuple".to_string(),
                ));
            }
        };

        // NULL and a malformed blob both mean "this CLI has not told us it
        // supports anything", which is what an empty set says. A capability
        // list is used for feature gating, so degrading to "assume nothing" is
        // the safe direction; failing the whole row load would not be.
        let provider_capabilities = self
            .provider_capabilities_json
            .as_deref()
            .and_then(|json| match serde_json::from_str::<Vec<String>>(json) {
                Ok(capabilities) => Some(capabilities),
                Err(e) => {
                    tracing::warn!(
                        session_id = %id,
                        error = %e,
                        "Ignoring malformed provider_capabilities JSON"
                    );
                    None
                }
            })
            .unwrap_or_default();

        Ok(Session {
            context_fill_pct: None,
            id,
            provider,
            claude_session_id: self.claude_session_id,
            query: self.query,
            working_dir: std::path::PathBuf::from(self.working_dir_str),
            status,
            project_id,
            pinned_at,
            testing_needed_at,
            rotation_disabled_at,
            session_kind,
            created_at,
            updated_at,
            cost_usd: self.cost_usd,
            duration_ms: self.duration_ms.map(|v| v as u64),
            num_turns: self.num_turns.map(|v| v as u32),
            model: self.model,
            input_tokens: self.input_tokens.map(|v| v as u64),
            output_tokens: self.output_tokens.map(|v| v as u64),
            context_window,
            resolved_context_budget,
            total_input_tokens: self.total_input_tokens.map(|v| v as u64),
            total_output_tokens: self.total_output_tokens.map(|v| v as u64),
            total_cache_creation_tokens: self.total_cache_creation_tokens.map(|v| v as u64),
            total_cache_read_tokens: self.total_cache_read_tokens.map(|v| v as u64),
            stop_reason: self.stop_reason,
            continued_from,
            context_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: self.daemon_input_tokens.map(|v| v as u64),
            daemon_output_tokens: self.daemon_output_tokens.map(|v| v as u64),
            handoff_filepath: self.handoff_filepath,
            active_task: self.active_task,
            rotation_depth: self.rotation_depth as u32,
            title: self.title,
            agent_role: self.agent_role,
            epic_spawn_ordinal,
            description: self.description,
            short_summary: None,
            pipeline_artifact: self.pipeline_artifact,
            workflow_id,
            // P1.2: workflow_id_override is runtime-only — no DB column yet
            // (deferred to a future migration). Restored rows default to None.
            workflow_id_override: None,
            git_branch: self.git_branch,
            group_id,
            // Hydrated post-load by store/sessions.rs after the session_tags join query.
            tag: String::new(),
            tags: Vec::new(),
            pending_question,
            pending_archive: self.pending_archive != 0,
            retry_attempt: self.retry_attempt.map(|v| v as u8),
            max_retries: self.max_retries.map(|v| v as u8),
            effort: self.effort,
            issue_identifier: self.issue_identifier,
            issue_url: self.issue_url,
            issue_tracker_id: self.issue_tracker_id,
            scheduled_job_id,
            rating: self.rating.map(|v| v as i16),
            harness_version_hash: self.harness_version_hash,
            test_passed: self.test_passed.map(|v| v != 0),
            clippy_passed: self.clippy_passed.map(|v| v != 0),
            turn_count: self.turn_count.map(|v| v as u32),
            retry_count: self.retry_count.map(|v| v as u32),
            approval_wait_ms: self.approval_wait_ms.map(|v| v as u64),
            // Not persisted to SQLite — projected at runtime from
            // TrackedSession.approval_wait_start in queries.rs. Restored sessions
            // start at None and are filled by the next list_sessions projection.
            approval_started_at: None,
            work_time_ms: self.work_time_ms.map(|v| v as u64),
            sandbox_kind,
            sandbox_root,
            sandbox_branch: self.sandbox_branch,
            sandbox_cleanup_state,
            parent_id,
            lead_session_id,
            is_eval: self.is_eval != 0,
            capability_class,
            topology_node_id: self.topology_node_id,
            topology_iteration: self.topology_iteration as u32,
            provider_cli_version: self.provider_cli_version,
            provider_capabilities,
            thinking_tokens: self.thinking_tokens.map(|v| v as u64),
            service_tier: self.service_tier,
            cache_creation_1h_tokens: self.cache_creation_1h_tokens.map(|v| v as u64),
            cache_creation_5m_tokens: self.cache_creation_5m_tokens.map(|v| v as u64),
            permission_denial_count: self.permission_denial_count.map(|v| v as u64),
            subagent_stats_json: self.subagent_stats_json,
            queued_turn_count: self.queued_turn_count.map(|v| v as u64),
            terminal_reason: self.terminal_reason,
        })
    }
}

/// Intermediate row for reading turn_metrics from the database.
pub(crate) struct TurnMetricRow {
    pub id: i64,
    pub session_id_str: String,
    pub turn_number: i32,
    pub input_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
    pub stop_reason: Option<String>,
    pub tools_used_json: Option<String>,
    pub tool_count: i32,
    pub created_at_str: String,
    pub model: Option<String>,
    // === V99 richer usage capture (P1-C) ===
    pub thinking_tokens: i64,
    pub cache_creation_1h_tokens: i64,
    pub cache_creation_5m_tokens: i64,
    pub service_tier: Option<String>,
}

impl TurnMetricRow {
    pub fn into_turn_metric(self) -> Result<TurnMetric> {
        let session_id = Uuid::parse_str(&self.session_id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid UUID: {}", e)))?;
        let created_at = parse_timestamp(&self.created_at_str).map_err(DaemonError::Store)?;
        let tools_used: Option<Vec<String>> = self
            .tools_used_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid tools_used JSON: {}", e)))?;

        Ok(TurnMetric {
            id: self.id,
            session_id,
            turn_number: self.turn_number,
            input_tokens: self.input_tokens as u64,
            cache_creation_tokens: self.cache_creation_tokens as u64,
            cache_read_tokens: self.cache_read_tokens as u64,
            output_tokens: self.output_tokens as u64,
            stop_reason: self.stop_reason,
            tools_used,
            tool_count: self.tool_count as u32,
            created_at,
            model: self.model,
            thinking_tokens: self.thinking_tokens as u64,
            cache_creation_1h_tokens: self.cache_creation_1h_tokens as u64,
            cache_creation_5m_tokens: self.cache_creation_5m_tokens as u64,
            service_tier: self.service_tier,
        })
    }
}

/// Intermediate row for reading model_segments from the database.
pub(crate) struct ModelSegmentRow {
    pub id: i64,
    pub session_id_str: String,
    pub model_id: String,
    pub from_sequence: i32,
    pub to_sequence: Option<i32>,
    pub created_at_str: String,
}

impl ModelSegmentRow {
    pub fn into_model_segment(self) -> Result<ModelSegment> {
        let session_id = Uuid::parse_str(&self.session_id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid UUID: {}", e)))?;
        let created_at = parse_timestamp(&self.created_at_str).map_err(DaemonError::Store)?;

        Ok(ModelSegment {
            id: self.id,
            session_id,
            model_id: self.model_id,
            from_sequence: self.from_sequence,
            to_sequence: self.to_sequence,
            created_at,
        })
    }
}

/// Intermediate row for reading projects from the database.
pub(crate) struct ProjectRow {
    pub id_str: String,
    pub name: String,
    pub path_str: Option<String>,
    pub description: Option<String>,
    pub color: String,
    pub context_files_json: Option<String>,
    pub created_at_str: String,
    pub updated_at_str: String,
}

impl ProjectRow {
    pub fn into_project(self) -> Result<Project> {
        let id = Uuid::parse_str(&self.id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid UUID: {}", e)))?;
        let created_at = parse_timestamp(&self.created_at_str).map_err(DaemonError::Store)?;
        let updated_at = parse_timestamp(&self.updated_at_str).map_err(DaemonError::Store)?;

        let context_files: Option<Vec<std::path::PathBuf>> = self
            .context_files_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());

        Ok(Project {
            id,
            name: self.name,
            path: self.path_str.map(std::path::PathBuf::from),
            description: self.description,
            color: self.color,
            context_files,
            created_at,
            updated_at,
        })
    }
}

// -- Enum serialization helpers --

pub(crate) fn session_status_to_str(s: SessionStatus) -> &'static str {
    match s {
        SessionStatus::Starting => "Starting",
        SessionStatus::Running => "Running",
        SessionStatus::WaitingApproval => "WaitingApproval",
        SessionStatus::Completed => "Completed",
        SessionStatus::Failed => "Failed",
        SessionStatus::Interrupted => "Interrupted",
        SessionStatus::Archived => "Archived",
        SessionStatus::Deleted => "Deleted",
        _ => "Failed",
    }
}

pub(crate) fn session_kind_to_str(k: SessionKind) -> &'static str {
    match k {
        SessionKind::Standard => "Standard",
        SessionKind::TaskRabbit => "TaskRabbit",
        SessionKind::Bug => "Bug",
        SessionKind::Group => "Group",
        SessionKind::Epic => "Epic",
        SessionKind::Story => "Story",
        SessionKind::Task => "Task",
        SessionKind::Feature => "Feature",
        SessionKind::Refactor => "Refactor",
        SessionKind::Research => "Research",
        _ => "Standard",
    }
}

pub(crate) fn session_provider_to_str(p: SessionProvider) -> &'static str {
    match p {
        SessionProvider::Claude => "Claude",
        SessionProvider::Codex => "Codex",
        SessionProvider::Pioneer => "Pioneer",
        SessionProvider::OpenRouter => "OpenRouter",
        SessionProvider::Bedrock => "Bedrock",
        SessionProvider::Local => "Local",
        SessionProvider::Antigravity => "Antigravity",
        SessionProvider::CodexAppServer => "CodexAppServer",
        SessionProvider::Harness => "Harness",
        _ => "Claude",
    }
}

pub(crate) fn str_to_session_kind(s: &str) -> Result<SessionKind> {
    match s {
        "Standard" => Ok(SessionKind::Standard),
        "TaskRabbit" => Ok(SessionKind::TaskRabbit),
        "Bug" => Ok(SessionKind::Bug),
        "Group" => Ok(SessionKind::Group),
        "Epic" => Ok(SessionKind::Epic),
        "Story" => Ok(SessionKind::Story),
        "Task" => Ok(SessionKind::Task),
        "Feature" => Ok(SessionKind::Feature),
        "Refactor" => Ok(SessionKind::Refactor),
        "Research" => Ok(SessionKind::Research),
        other => Err(DaemonError::Store(format!(
            "Unknown session kind: {}",
            other
        ))),
    }
}

pub(crate) fn str_to_session_provider(s: &str) -> Result<SessionProvider> {
    match s {
        "Claude" => Ok(SessionProvider::Claude),
        "Codex" => Ok(SessionProvider::Codex),
        "Pioneer" => Ok(SessionProvider::Pioneer),
        "OpenRouter" => Ok(SessionProvider::OpenRouter),
        "Bedrock" => Ok(SessionProvider::Bedrock),
        "Local" => Ok(SessionProvider::Local),
        // Backwards compat: old sessions stored as "OpenAi" map to Local
        "OpenAi" => Ok(SessionProvider::Local),
        "Antigravity" => Ok(SessionProvider::Antigravity),
        "Gemini" => Ok(SessionProvider::Antigravity),
        "CodexAppServer" => Ok(SessionProvider::CodexAppServer),
        "Harness" => Ok(SessionProvider::Harness),
        // Backwards compat: removed providers fall back to Claude so old sessions load cleanly
        "Nullclaw" | "OpenCode" => Ok(SessionProvider::Claude),
        other => Err(DaemonError::Store(format!(
            "Unknown session provider: {}",
            other
        ))),
    }
}

pub(crate) fn str_to_session_status(s: &str) -> Result<SessionStatus> {
    match s {
        "Starting" => Ok(SessionStatus::Starting),
        "Running" => Ok(SessionStatus::Running),
        "WaitingApproval" => Ok(SessionStatus::WaitingApproval),
        "Completed" => Ok(SessionStatus::Completed),
        "Failed" => Ok(SessionStatus::Failed),
        "Interrupted" => Ok(SessionStatus::Interrupted),
        "Archived" => Ok(SessionStatus::Archived),
        "Rotated" => Ok(SessionStatus::Archived),
        "Deleted" => Ok(SessionStatus::Deleted),
        other => Err(DaemonError::Store(format!(
            "Unknown session status: {}",
            other
        ))),
    }
}

pub(crate) fn event_type_to_str(e: EventType) -> &'static str {
    match e {
        EventType::Message => "Message",
        EventType::ToolUse => "ToolUse",
        EventType::ToolResult => "ToolResult",
        EventType::System => "System",
        EventType::Thinking => "Thinking",
        EventType::Compressed => "Compressed",
        _ => "System",
    }
}

pub(crate) fn str_to_event_type(s: &str) -> Result<EventType> {
    match s.to_ascii_lowercase().as_str() {
        "message" => Ok(EventType::Message),
        "tooluse" => Ok(EventType::ToolUse),
        "toolresult" => Ok(EventType::ToolResult),
        "system" => Ok(EventType::System),
        "thinking" => Ok(EventType::Thinking),
        "compressed" => Ok(EventType::Compressed),
        _ => Err(DaemonError::Store(format!("Unknown event type: {}", s))),
    }
}

pub(crate) fn role_to_str(r: Role) -> &'static str {
    match r {
        Role::User => "User",
        Role::Assistant => "Assistant",
        _ => "User",
    }
}

pub(crate) fn str_to_role(s: &str) -> Result<Role> {
    match s.to_ascii_lowercase().as_str() {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        _ => Err(DaemonError::Store(format!("Unknown role: {}", s))),
    }
}

pub(crate) fn approval_status_to_str(s: ApprovalStatus) -> &'static str {
    match s {
        ApprovalStatus::Pending => "Pending",
        ApprovalStatus::Approved => "Approved",
        ApprovalStatus::Denied => "Denied",
        _ => "Pending",
    }
}

pub(crate) fn str_to_approval_status(s: &str) -> Result<ApprovalStatus> {
    match s {
        "Pending" => Ok(ApprovalStatus::Pending),
        "Approved" => Ok(ApprovalStatus::Approved),
        "Denied" => Ok(ApprovalStatus::Denied),
        other => Err(DaemonError::Store(format!(
            "Unknown approval status: {}",
            other
        ))),
    }
}

pub(crate) fn sandbox_kind_to_str(k: SandboxKind) -> &'static str {
    match k {
        SandboxKind::None => "None",
        SandboxKind::GitWorktree => "GitWorktree",
        _ => "None",
    }
}

pub(crate) fn str_to_sandbox_kind(s: &str) -> Result<SandboxKind> {
    match s {
        "None" => Ok(SandboxKind::None),
        "GitWorktree" => Ok(SandboxKind::GitWorktree),
        other => Err(DaemonError::Store(format!(
            "Unknown sandbox kind: {}",
            other
        ))),
    }
}

pub(crate) fn sandbox_cleanup_state_to_str(s: SandboxCleanupState) -> &'static str {
    match s {
        SandboxCleanupState::Live => "Live",
        SandboxCleanupState::Purged => "Purged",
        SandboxCleanupState::Failed => "Failed",
        _ => "Live",
    }
}

pub(crate) fn str_to_sandbox_cleanup_state(s: &str) -> Result<SandboxCleanupState> {
    match s {
        "Live" => Ok(SandboxCleanupState::Live),
        "Purged" => Ok(SandboxCleanupState::Purged),
        "Failed" => Ok(SandboxCleanupState::Failed),
        other => Err(DaemonError::Store(format!(
            "Unknown sandbox cleanup state: {}",
            other
        ))),
    }
}

pub(crate) fn capability_class_to_str(c: CapabilityClass) -> &'static str {
    match c {
        CapabilityClass::Architect => "architect",
        CapabilityClass::Implementer => "implementer",
        CapabilityClass::LookupFast => "lookup_fast",
    }
}

pub(crate) fn str_to_capability_class(s: &str) -> Result<CapabilityClass> {
    match s {
        "architect" => Ok(CapabilityClass::Architect),
        "implementer" => Ok(CapabilityClass::Implementer),
        "lookup_fast" => Ok(CapabilityClass::LookupFast),
        other => Err(DaemonError::Store(format!(
            "Unknown capability class: {}",
            other
        ))),
    }
}

// -- D01 Idea-kernel row mappers --

pub(super) const CAPTURE_COLUMNS: &str = "id, project_id, creator_kind, creator_id, \
    captured_at, source_kind, raw_content_digest, storage_policy_id, content_ref";
pub(super) const IDEA_COLUMNS: &str = "id, project_id, slug, sigil, genesis_capture_id, \
    genesis_span_start, genesis_span_end, genesis_span_digest, title, description, \
    portfolio_summary, lifecycle, stage, priority, autonomy_policy, integration_target_ref, \
    program_template_policy_id, current_controller_session_id, controller_epoch, row_version, \
    next_event_sequence, created_at, updated_at, terminal_at, superseded_at";
pub(super) const IDEA_EVENT_COLUMNS: &str = "id, project_id, idea_id, sequence, event_type, \
    actor_kind, actor_id, controller_session_id, controller_epoch, expected_row_version, \
    resulting_row_version, idempotency_key, occurred_at, payload_json, \
    artifact_digests_json, evidence_digests_json";
pub(super) const IDEA_RELATIONSHIP_COLUMNS: &str = "id, project_id, source_idea_id, \
    target_idea_id, kind, created_event_id, created_at, removed_event_id, removed_at";
#[allow(dead_code)] // Collection reads are intentionally outside D01.
pub(super) const IDEA_COLLECTION_COLUMNS: &str = "id, project_id, slug, name, description, \
    created_at, updated_at, retired_at";
#[allow(dead_code)] // Collection reads are intentionally outside D01.
pub(super) const IDEA_MEMBERSHIP_COLUMNS: &str = "id, project_id, collection_id, idea_id, \
    added_at, removed_at";
#[allow(dead_code)] // D15 consumes this mapper; D01 verifies it through fixtures.
pub(super) const IDEA_COMPATIBILITY_COLUMNS: &str = "id, project_id, legacy_source_kind, \
    legacy_source_id, idea_id, collection_id, status, provenance_json, disposition, created_at, \
    updated_at, mapped_at";

fn d01_store_error(label: &str, error: impl std::fmt::Display) -> DaemonError {
    DaemonError::Store(format!("invalid D01 {label}: {error}"))
}

fn parse_d01_uuid(label: &str, value: &str) -> Result<Uuid> {
    let parsed = Uuid::parse_str(value).map_err(|error| d01_store_error(label, error))?;
    if parsed.to_string() != value {
        return Err(d01_store_error(label, "UUID is not lowercase canonical"));
    }
    Ok(parsed)
}

fn parse_d01_optional_uuid(label: &str, value: Option<&str>) -> Result<Option<Uuid>> {
    value.map(|value| parse_d01_uuid(label, value)).transpose()
}

fn parse_d01_timestamp(label: &str, value: &str) -> Result<DateTime<Utc>> {
    let timestamp = DateTime::parse_from_rfc3339(value)
        .map_err(|error| d01_store_error(label, error))?
        .with_timezone(&Utc);
    if timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true) != value {
        return Err(d01_store_error(
            label,
            "timestamp is not canonical UTC RFC3339 nanosecond text",
        ));
    }
    Ok(timestamp)
}

fn parse_d01_optional_timestamp(label: &str, value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    value
        .map(|value| parse_d01_timestamp(label, value))
        .transpose()
}

#[derive(Clone)]
pub(super) struct CaptureRow {
    pub id: String,
    pub project_id: String,
    pub creator_kind: String,
    pub creator_id: String,
    pub captured_at: String,
    pub source_kind: String,
    pub raw_content_digest: String,
    pub storage_policy_id: String,
    pub content_ref: String,
}

impl CaptureRow {
    pub(super) fn into_capture(self) -> Result<Capture> {
        let capture = Capture {
            id: parse_d01_uuid("capture id", &self.id)?,
            project_id: parse_d01_uuid("capture project_id", &self.project_id)?,
            creator_kind: IdeaActorKind::parse(&self.creator_kind)
                .map_err(|error| d01_store_error("capture creator_kind", error))?,
            creator_id: self.creator_id,
            captured_at: parse_d01_timestamp("capture captured_at", &self.captured_at)?,
            source_kind: CaptureSourceKind::parse(&self.source_kind)
                .map_err(|error| d01_store_error("capture source_kind", error))?,
            raw_content_digest: Sha256Digest::parse(self.raw_content_digest)
                .map_err(|error| d01_store_error("capture raw_content_digest", error))?,
            storage_policy_id: self.storage_policy_id,
            content_ref: ContentAddressedRef::parse(self.content_ref)
                .map_err(|error| d01_store_error("capture content_ref", error))?,
        };
        capture
            .validate()
            .map_err(|error| d01_store_error("capture", error))?;
        Ok(capture)
    }
}

pub(super) fn map_capture_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CaptureRow> {
    Ok(CaptureRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        creator_kind: row.get(2)?,
        creator_id: row.get(3)?,
        captured_at: row.get(4)?,
        source_kind: row.get(5)?,
        raw_content_digest: row.get(6)?,
        storage_policy_id: row.get(7)?,
        content_ref: row.get(8)?,
    })
}

#[derive(Clone)]
pub(super) struct IdeaRow {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub sigil: Option<String>,
    pub genesis_capture_id: String,
    pub genesis_span_start: Option<i64>,
    pub genesis_span_end: Option<i64>,
    pub genesis_span_digest: Option<String>,
    pub title: String,
    pub description: String,
    pub portfolio_summary: String,
    pub lifecycle: String,
    pub stage: String,
    pub priority: i64,
    pub autonomy_policy: String,
    pub integration_target_ref: String,
    pub program_template_policy_id: Option<String>,
    pub current_controller_session_id: Option<String>,
    pub controller_epoch: i64,
    pub row_version: i64,
    pub next_event_sequence: i64,
    pub created_at: String,
    pub updated_at: String,
    pub terminal_at: Option<String>,
    pub superseded_at: Option<String>,
}

impl IdeaRow {
    pub(super) fn into_idea(self) -> Result<Idea> {
        let idea = Idea {
            id: parse_d01_uuid("idea id", &self.id)?,
            project_id: parse_d01_uuid("idea project_id", &self.project_id)?,
            slug: self.slug,
            sigil: self.sigil,
            genesis_capture_id: parse_d01_uuid(
                "idea genesis_capture_id",
                &self.genesis_capture_id,
            )?,
            genesis_span_start: self.genesis_span_start,
            genesis_span_end: self.genesis_span_end,
            genesis_span_digest: self
                .genesis_span_digest
                .map(Sha256Digest::parse)
                .transpose()
                .map_err(|error| d01_store_error("idea genesis_span_digest", error))?,
            title: self.title,
            description: self.description,
            portfolio_summary: self.portfolio_summary,
            lifecycle: IdeaLifecycle::parse(&self.lifecycle)
                .map_err(|error| d01_store_error("idea lifecycle", error))?,
            stage: IdeaStage::parse(&self.stage)
                .map_err(|error| d01_store_error("idea stage", error))?,
            priority: self.priority,
            autonomy_policy: AutonomyPolicy::parse(&self.autonomy_policy)
                .map_err(|error| d01_store_error("idea autonomy_policy", error))?,
            integration_target_ref: self.integration_target_ref,
            program_template_policy_id: self.program_template_policy_id,
            current_controller_session_id: parse_d01_optional_uuid(
                "idea current_controller_session_id",
                self.current_controller_session_id.as_deref(),
            )?,
            controller_epoch: self.controller_epoch,
            row_version: self.row_version,
            next_event_sequence: self.next_event_sequence,
            created_at: parse_d01_timestamp("idea created_at", &self.created_at)?,
            updated_at: parse_d01_timestamp("idea updated_at", &self.updated_at)?,
            terminal_at: parse_d01_optional_timestamp(
                "idea terminal_at",
                self.terminal_at.as_deref(),
            )?,
            superseded_at: parse_d01_optional_timestamp(
                "idea superseded_at",
                self.superseded_at.as_deref(),
            )?,
        };
        idea.validate()
            .map_err(|error| d01_store_error("idea", error))?;
        Ok(idea)
    }
}

pub(super) fn map_idea_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IdeaRow> {
    Ok(IdeaRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        slug: row.get(2)?,
        sigil: row.get(3)?,
        genesis_capture_id: row.get(4)?,
        genesis_span_start: row.get(5)?,
        genesis_span_end: row.get(6)?,
        genesis_span_digest: row.get(7)?,
        title: row.get(8)?,
        description: row.get(9)?,
        portfolio_summary: row.get(10)?,
        lifecycle: row.get(11)?,
        stage: row.get(12)?,
        priority: row.get(13)?,
        autonomy_policy: row.get(14)?,
        integration_target_ref: row.get(15)?,
        program_template_policy_id: row.get(16)?,
        current_controller_session_id: row.get(17)?,
        controller_epoch: row.get(18)?,
        row_version: row.get(19)?,
        next_event_sequence: row.get(20)?,
        created_at: row.get(21)?,
        updated_at: row.get(22)?,
        terminal_at: row.get(23)?,
        superseded_at: row.get(24)?,
    })
}

#[derive(Clone)]
pub(super) struct IdeaEventRow {
    pub id: String,
    pub project_id: String,
    pub idea_id: String,
    pub sequence: i64,
    pub event_type: String,
    pub actor_kind: String,
    pub actor_id: String,
    pub controller_session_id: Option<String>,
    pub controller_epoch: Option<i64>,
    pub expected_row_version: i64,
    pub resulting_row_version: i64,
    pub idempotency_key: String,
    pub occurred_at: String,
    pub payload_json: String,
    pub artifact_digests_json: String,
    pub evidence_digests_json: String,
}

impl IdeaEventRow {
    pub(super) fn into_idea_event(self) -> Result<IdeaEvent> {
        let event = IdeaEvent {
            id: parse_d01_uuid("idea event id", &self.id)?,
            project_id: parse_d01_uuid("idea event project_id", &self.project_id)?,
            idea_id: parse_d01_uuid("idea event idea_id", &self.idea_id)?,
            sequence: self.sequence,
            event_type: IdeaEventType::parse(&self.event_type)
                .map_err(|error| d01_store_error("idea event type", error))?,
            actor_kind: IdeaActorKind::parse(&self.actor_kind)
                .map_err(|error| d01_store_error("idea event actor_kind", error))?,
            actor_id: self.actor_id,
            controller_session_id: parse_d01_optional_uuid(
                "idea event controller_session_id",
                self.controller_session_id.as_deref(),
            )?,
            controller_epoch: self.controller_epoch,
            expected_row_version: self.expected_row_version,
            resulting_row_version: self.resulting_row_version,
            idempotency_key: self.idempotency_key,
            occurred_at: parse_d01_timestamp("idea event occurred_at", &self.occurred_at)?,
            payload: serde_json::from_str(&self.payload_json)
                .map_err(|error| d01_store_error("idea event payload_json", error))?,
            artifact_digests: serde_json::from_str(&self.artifact_digests_json)
                .map_err(|error| d01_store_error("idea event artifact_digests_json", error))?,
            evidence_digests: serde_json::from_str(&self.evidence_digests_json)
                .map_err(|error| d01_store_error("idea event evidence_digests_json", error))?,
        };
        event
            .validate()
            .map_err(|error| d01_store_error("idea event", error))?;
        Ok(event)
    }
}

pub(super) fn map_idea_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IdeaEventRow> {
    Ok(IdeaEventRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        idea_id: row.get(2)?,
        sequence: row.get(3)?,
        event_type: row.get(4)?,
        actor_kind: row.get(5)?,
        actor_id: row.get(6)?,
        controller_session_id: row.get(7)?,
        controller_epoch: row.get(8)?,
        expected_row_version: row.get(9)?,
        resulting_row_version: row.get(10)?,
        idempotency_key: row.get(11)?,
        occurred_at: row.get(12)?,
        payload_json: row.get(13)?,
        artifact_digests_json: row.get(14)?,
        evidence_digests_json: row.get(15)?,
    })
}

#[derive(Clone)]
pub(super) struct IdeaRelationshipRow {
    pub id: String,
    pub project_id: String,
    pub source_idea_id: String,
    pub target_idea_id: String,
    pub kind: String,
    pub created_event_id: String,
    pub created_at: String,
    pub removed_event_id: Option<String>,
    pub removed_at: Option<String>,
}

impl IdeaRelationshipRow {
    pub(super) fn into_idea_relationship(self) -> Result<IdeaRelationship> {
        let relationship = IdeaRelationship {
            id: parse_d01_uuid("relationship id", &self.id)?,
            project_id: parse_d01_uuid("relationship project_id", &self.project_id)?,
            source_idea_id: parse_d01_uuid("relationship source_idea_id", &self.source_idea_id)?,
            target_idea_id: parse_d01_uuid("relationship target_idea_id", &self.target_idea_id)?,
            kind: IdeaRelationshipKind::parse(&self.kind)
                .map_err(|error| d01_store_error("relationship kind", error))?,
            created_event_id: parse_d01_uuid(
                "relationship created_event_id",
                &self.created_event_id,
            )?,
            created_at: parse_d01_timestamp("relationship created_at", &self.created_at)?,
            removed_event_id: parse_d01_optional_uuid(
                "relationship removed_event_id",
                self.removed_event_id.as_deref(),
            )?,
            removed_at: parse_d01_optional_timestamp(
                "relationship removed_at",
                self.removed_at.as_deref(),
            )?,
        };
        if relationship.source_idea_id == relationship.target_idea_id
            || relationship.removed_event_id.is_some() != relationship.removed_at.is_some()
        {
            return Err(d01_store_error(
                "relationship",
                "self-edge or unpaired removal fields",
            ));
        }
        Ok(relationship)
    }
}

pub(super) fn map_idea_relationship_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<IdeaRelationshipRow> {
    Ok(IdeaRelationshipRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        source_idea_id: row.get(2)?,
        target_idea_id: row.get(3)?,
        kind: row.get(4)?,
        created_event_id: row.get(5)?,
        created_at: row.get(6)?,
        removed_event_id: row.get(7)?,
        removed_at: row.get(8)?,
    })
}

#[derive(Clone)]
#[allow(dead_code)]
pub(super) struct IdeaCollectionRow {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub retired_at: Option<String>,
}

#[allow(dead_code)]
impl IdeaCollectionRow {
    pub(super) fn into_idea_collection(self) -> Result<IdeaCollection> {
        if self.slug.trim().is_empty()
            || self.slug.len() > 128
            || self.name.trim().is_empty()
            || self.name.len() > 512
            || self
                .description
                .as_deref()
                .is_some_and(|description| description.len() > 65_536)
        {
            return Err(d01_store_error(
                "collection",
                "invalid slug, name, or description",
            ));
        }
        Ok(IdeaCollection {
            id: parse_d01_uuid("collection id", &self.id)?,
            project_id: parse_d01_uuid("collection project_id", &self.project_id)?,
            slug: self.slug,
            name: self.name,
            description: self.description,
            created_at: parse_d01_timestamp("collection created_at", &self.created_at)?,
            updated_at: parse_d01_timestamp("collection updated_at", &self.updated_at)?,
            retired_at: parse_d01_optional_timestamp(
                "collection retired_at",
                self.retired_at.as_deref(),
            )?,
        })
    }
}

#[allow(dead_code)]
pub(super) fn map_idea_collection_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<IdeaCollectionRow> {
    Ok(IdeaCollectionRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        slug: row.get(2)?,
        name: row.get(3)?,
        description: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        retired_at: row.get(7)?,
    })
}

#[derive(Clone)]
#[allow(dead_code)]
pub(super) struct IdeaMembershipRow {
    pub id: String,
    pub project_id: String,
    pub collection_id: String,
    pub idea_id: String,
    pub added_at: String,
    pub removed_at: Option<String>,
}

#[allow(dead_code)]
impl IdeaMembershipRow {
    pub(super) fn into_idea_collection_membership(self) -> Result<IdeaCollectionMembership> {
        Ok(IdeaCollectionMembership {
            id: parse_d01_uuid("membership id", &self.id)?,
            project_id: parse_d01_uuid("membership project_id", &self.project_id)?,
            collection_id: parse_d01_uuid("membership collection_id", &self.collection_id)?,
            idea_id: parse_d01_uuid("membership idea_id", &self.idea_id)?,
            added_at: parse_d01_timestamp("membership added_at", &self.added_at)?,
            removed_at: parse_d01_optional_timestamp(
                "membership removed_at",
                self.removed_at.as_deref(),
            )?,
        })
    }
}

#[allow(dead_code)]
pub(super) fn map_idea_membership_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<IdeaMembershipRow> {
    Ok(IdeaMembershipRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        collection_id: row.get(2)?,
        idea_id: row.get(3)?,
        added_at: row.get(4)?,
        removed_at: row.get(5)?,
    })
}

#[derive(Clone)]
#[allow(dead_code)]
pub(super) struct IdeaCompatibilityRow {
    pub id: String,
    pub project_id: String,
    pub legacy_source_kind: String,
    pub legacy_source_id: String,
    pub idea_id: Option<String>,
    pub collection_id: Option<String>,
    pub status: String,
    pub provenance_json: String,
    pub disposition: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub mapped_at: Option<String>,
}

#[allow(dead_code)]
impl IdeaCompatibilityRow {
    pub(super) fn into_idea_compatibility_mapping(self) -> Result<IdeaCompatibilityMapping> {
        let mapping = IdeaCompatibilityMapping {
            id: parse_d01_uuid("compatibility id", &self.id)?,
            project_id: parse_d01_uuid("compatibility project_id", &self.project_id)?,
            legacy_source_kind: LegacyIdeaSourceKind::parse(&self.legacy_source_kind)
                .map_err(|error| d01_store_error("compatibility legacy_source_kind", error))?,
            legacy_source_id: parse_d01_uuid(
                "compatibility legacy_source_id",
                &self.legacy_source_id,
            )?,
            idea_id: parse_d01_optional_uuid("compatibility idea_id", self.idea_id.as_deref())?,
            collection_id: parse_d01_optional_uuid(
                "compatibility collection_id",
                self.collection_id.as_deref(),
            )?,
            status: IdeaCompatibilityStatus::parse(&self.status)
                .map_err(|error| d01_store_error("compatibility status", error))?,
            provenance: serde_json::from_str(&self.provenance_json)
                .map_err(|error| d01_store_error("compatibility provenance_json", error))?,
            disposition: self.disposition,
            created_at: parse_d01_timestamp("compatibility created_at", &self.created_at)?,
            updated_at: parse_d01_timestamp("compatibility updated_at", &self.updated_at)?,
            mapped_at: parse_d01_optional_timestamp(
                "compatibility mapped_at",
                self.mapped_at.as_deref(),
            )?,
        };
        mapping
            .validate()
            .map_err(|error| d01_store_error("compatibility", error))?;
        Ok(mapping)
    }
}

#[allow(dead_code)]
pub(super) fn map_idea_compatibility_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<IdeaCompatibilityRow> {
    Ok(IdeaCompatibilityRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        legacy_source_kind: row.get(2)?,
        legacy_source_id: row.get(3)?,
        idea_id: row.get(4)?,
        collection_id: row.get(5)?,
        status: row.get(6)?,
        provenance_json: row.get(7)?,
        disposition: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        mapped_at: row.get(11)?,
    })
}

// -- Workflow row mapper --

/// Intermediate row for reading workflows from the database.
pub(crate) struct WorkflowRow {
    pub id_str: String,
    pub title: String,
    pub stage_str: String,
    pub artifact_path: Option<String>,
    pub project_id_str: Option<String>,
    pub created_at_str: String,
    pub updated_at_str: String,
}

impl WorkflowRow {
    pub fn into_workflow(self) -> Result<Workflow> {
        let id = Uuid::parse_str(&self.id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid workflow UUID: {}", e)))?;
        let stage = str_to_workflow_stage(&self.stage_str)?;
        let project_id = self
            .project_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid workflow project UUID: {}", e)))?;
        let created_at = parse_timestamp(&self.created_at_str).map_err(DaemonError::Store)?;
        let updated_at = parse_timestamp(&self.updated_at_str).map_err(DaemonError::Store)?;

        Ok(Workflow {
            id,
            title: self.title,
            stage,
            artifact_path: self.artifact_path,
            project_id,
            created_at,
            updated_at,
        })
    }
}

pub(crate) fn map_workflow_row(row: &rusqlite::Row) -> rusqlite::Result<WorkflowRow> {
    Ok(WorkflowRow {
        id_str: row.get(0)?,
        title: row.get(1)?,
        stage_str: row.get(2)?,
        artifact_path: row.get(3)?,
        project_id_str: row.get(4)?,
        created_at_str: row.get(5)?,
        updated_at_str: row.get(6)?,
    })
}

pub(crate) fn workflow_stage_to_str(s: WorkflowStage) -> &'static str {
    match s {
        WorkflowStage::Research => "Research",
        WorkflowStage::ResearchComplete => "ResearchComplete",
        WorkflowStage::Planning => "Planning",
        WorkflowStage::PlanComplete => "PlanComplete",
        WorkflowStage::Implementing => "Implementing",
        WorkflowStage::ImplementComplete => "ImplementComplete",
        WorkflowStage::Complete => "Complete",
        _ => "Research",
    }
}

pub(crate) fn str_to_workflow_stage(s: &str) -> Result<WorkflowStage> {
    match s {
        "Research" => Ok(WorkflowStage::Research),
        "ResearchComplete" => Ok(WorkflowStage::ResearchComplete),
        "Planning" => Ok(WorkflowStage::Planning),
        "PlanComplete" => Ok(WorkflowStage::PlanComplete),
        "Implementing" => Ok(WorkflowStage::Implementing),
        "ImplementComplete" => Ok(WorkflowStage::ImplementComplete),
        "Complete" => Ok(WorkflowStage::Complete),
        other => Err(DaemonError::Store(format!(
            "Unknown workflow stage: {}",
            other
        ))),
    }
}

// ── ESP Games ──

pub(crate) struct EspGameRow {
    pub id: String,
    pub played_at: String,
    pub score: i64,
    pub rounds_played: i64,
    pub total_rounds: i64,
    pub p_value: f64,
    pub round_details: String,
}

pub(crate) fn map_esp_game_row(row: &rusqlite::Row) -> rusqlite::Result<EspGameRow> {
    Ok(EspGameRow {
        id: row.get(0)?,
        played_at: row.get(1)?,
        score: row.get(2)?,
        rounds_played: row.get(3)?,
        total_rounds: row.get(4)?,
        p_value: row.get(5)?,
        round_details: row.get(6)?,
    })
}

impl EspGameRow {
    pub fn into_esp_game(self) -> anyhow::Result<EspGame> {
        Ok(EspGame {
            id: Uuid::parse_str(&self.id)?,
            played_at: parse_timestamp(&self.played_at)
                .map_err(|e| anyhow::anyhow!("Invalid ESP game timestamp: {}", e))?,
            score: self.score as u8,
            rounds_played: self.rounds_played as u8,
            total_rounds: self.total_rounds as u8,
            p_value: self.p_value,
            round_details: self.round_details,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{session_provider_to_str, str_to_session_provider};
    use rsi_common::types::SessionProvider;

    #[test]
    fn test_session_provider_antigravity_string_roundtrip_and_alias() {
        assert_eq!(
            session_provider_to_str(SessionProvider::Antigravity),
            "Antigravity"
        );
        assert_eq!(
            str_to_session_provider("Antigravity").unwrap(),
            SessionProvider::Antigravity
        );
        assert_eq!(
            str_to_session_provider("Gemini").unwrap(),
            SessionProvider::Antigravity
        );
    }

    #[test]
    fn test_session_provider_bedrock_string_roundtrip() {
        assert_eq!(session_provider_to_str(SessionProvider::Bedrock), "Bedrock");
        assert_eq!(
            str_to_session_provider("Bedrock").unwrap(),
            SessionProvider::Bedrock
        );
    }

    #[test]
    fn test_session_provider_pioneer_string_roundtrip() {
        assert_eq!(session_provider_to_str(SessionProvider::Pioneer), "Pioneer");
        assert_eq!(
            str_to_session_provider("Pioneer").unwrap(),
            SessionProvider::Pioneer
        );
    }
}
