//! RPC schema-lock snapshot tests (Phase 2.2).
//!
//! These tests use `insta` to lock the JSON wire format for the most critical
//! RPC request/response pairs. A schema-breaking change (renamed key, added
//! required field, type change) fails the test suite, surfacing the diff in
//! the PR review rather than at runtime.
//!
//! When a schema change is intentional, run `cargo insta review` to accept the
//! new snapshots, then commit the updated `.snap` files alongside the schema
//! change.

use chrono::TimeZone;
use rsi_common::model_control::{
    AdmissionStatus, BudgetScopeKind, InvocationForeground, InvocationOwner, ModelBudgetHeadroom,
    ModelControlMode, ModelControlStatusReport, ModelInvocationKind, ModelInvocationPurpose,
    ModelInvocationRecord, ModelInvocationStatus, ModelInvocationUsage, ModelInvocationView,
    ModelTier, ModelUsageConfidence, PaidRisk,
};
use rsi_common::provider_capabilities::{
    CapabilityConfidence, CapabilityEvidence, CapabilitySource, ContextCapacity,
    ResolvedContextBudget,
};
use rsi_common::rpc::{
    BusEvent, CancelModelInvocationParams, ConversationBatchEntry, ConversationBatchResponse,
    ConversationFetchCursor, GetConversationsSinceParams, GetModelControlStatusParams,
    GetUsageStatsParams, HealthStatusResponse, INVALID_PARAMS, LaunchSessionParams,
    ListModelInvocationsParams, ProviderRateLimitSnapshot, ProviderRateLimitWindow, RpcError,
    RpcRequest, RpcResponse, UpdateModelControlPolicyParams,
};
use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, EventType, ModelUsage, Project, Role, Session,
    SessionKind, SessionProvider, SessionStatus, UsageBucket, UsageStats,
};
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;

/// Deterministic fixture UUID — never changes across runs so snapshots stay stable.
fn fix_uuid(byte: u8) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[15] = byte;
    Uuid::from_bytes(bytes)
}

/// Deterministic fixture timestamp.
fn fix_ts() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn fixture_session() -> Session {
    Session {
        context_fill_pct: None,
        id: fix_uuid(1),
        status: SessionStatus::Running,
        session_kind: SessionKind::Standard,
        provider: SessionProvider::Claude,
        context_usage_confidence: ContextUsageConfidence::Counted,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        created_at: fix_ts(),
        updated_at: fix_ts(),
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        query: "fix the regression".to_string(),
        title: Some("Fix regression".to_string()),
        description: None,
        short_summary: None,
        working_dir: PathBuf::from("/tmp/repo"),
        git_branch: Some("main".to_string()),
        model: Some("claude-sonnet-5".to_string()),
        claude_session_id: Some("sess_abc".to_string()),
        project_id: Some(fix_uuid(2)),
        continued_from: None,
        parent_id: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        tag: String::new(),
        tags: Vec::new(),
        scheduled_job_id: None,
        stop_reason: None,
        cost_usd: Some(0.0123),
        duration_ms: Some(5000),
        num_turns: Some(3),
        input_tokens: Some(1000),
        output_tokens: Some(500),
        context_window: Some(200_000),
        resolved_context_budget: None,
        total_input_tokens: Some(2000),
        total_output_tokens: Some(900),
        total_cache_creation_tokens: Some(0),
        total_cache_read_tokens: Some(0),
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        rating: None,
        harness_version_hash: None,
        test_passed: None,
        clippy_passed: None,
        turn_count: Some(3),
        retry_count: Some(0),
        approval_wait_ms: Some(0),
        approval_started_at: None,
        work_time_ms: Some(120_000),
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        lead_session_id: None,
        agent_role: None,
        epic_spawn_ordinal: None,
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

fn fixture_event() -> ConversationEvent {
    ConversationEvent {
        id: 42,
        session_id: fix_uuid(1),
        sequence: 0,
        event_type: EventType::Message,
        role: Some(Role::User),
        created_at: fix_ts(),
        content: "hello".to_string(),
        tool_name: None,
        tool_input: None,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    }
}

fn fixture_project() -> Project {
    Project {
        id: fix_uuid(2),
        name: "rsi".to_string(),
        path: Some(PathBuf::from("/tmp/rsi")),
        description: Some("Vim TUI".to_string()),
        color: Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: fix_ts(),
        updated_at: fix_ts(),
    }
}

fn fixture_model_control_status() -> ModelControlStatusReport {
    let record = ModelInvocationRecord {
        id: fix_uuid(9),
        purpose: ModelInvocationPurpose::SessionLaunchFresh,
        kind: ModelInvocationKind::SessionLifecycle,
        foreground: InvocationForeground::Foreground,
        paid_risk: PaidRisk::PaidCapable,
        status: ModelInvocationStatus::Running,
        provider: Some("Claude".to_string()),
        model: Some("claude-sonnet-5".to_string()),
        backend: Some("Claude".to_string()),
        model_tier: Some(ModelTier::Premium),
        effort: Some("medium".to_string()),
        trigger: "launch_session".to_string(),
        owner: InvocationOwner {
            session_id: Some(fix_uuid(1)),
            project_id: Some(fix_uuid(2)),
            operator: Some("operator".to_string()),
            ..Default::default()
        },
        owner_scopes: vec![
            rsi_common::model_control::BudgetScopeRef {
                kind: BudgetScopeKind::Session,
                scope_id: Some(fix_uuid(1).to_string()),
            },
            rsi_common::model_control::BudgetScopeRef {
                kind: BudgetScopeKind::Project,
                scope_id: Some(fix_uuid(2).to_string()),
            },
        ],
        dedup_key: Some("session.launch".to_string()),
        request_fingerprint: Some("sha256:deadbeef".to_string()),
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        raw_admission_status: "admitted".to_string(),
        raw_status: "running".to_string(),
        admission_status: AdmissionStatus::Admitted,
        usage: ModelInvocationUsage {
            input_tokens: Some(1200),
            output_tokens: Some(400),
            cache_creation_tokens: Some(0),
            cache_read_tokens: Some(0),
            reasoning_tokens: Some(50),
            embedding_input_count: Some(0),
            wall_time_ms: Some(35_000),
            estimated_cost_usd: Some(0.42),
            confidence: ModelUsageConfidence::Measured,
        },
        baseline_usage: ModelInvocationUsage {
            input_tokens: Some(1000),
            output_tokens: Some(300),
            cache_creation_tokens: Some(0),
            cache_read_tokens: Some(0),
            reasoning_tokens: Some(0),
            embedding_input_count: Some(0),
            wall_time_ms: Some(20_000),
            estimated_cost_usd: None,
            confidence: ModelUsageConfidence::Stale,
        },
        error_class: None,
        cancellation_requested_at: None,
        cancellation_reason: None,
        cancellation_mechanism: None,
        authorization_reason: None,
        policy_authorized: true,
        escalation_source: None,
        escalation_reason: None,
        policy_snapshot: Some(serde_json::json!({ "mode": "normal" })),
        policy_snapshot_status: "valid".to_string(),
        policy_snapshot_error: None,
        created_at: fix_ts().to_rfc3339(),
        started_at: Some(fix_ts().to_rfc3339()),
        completed_at: None,
    };
    ModelControlStatusReport {
        mode: ModelControlMode::Normal,
        mode_updated_at: Some(fix_ts().to_rfc3339()),
        restart_required_fields: Vec::new(),
        circuit_state: "closed".to_string(),
        circuit_reason:
            "interactive work remains budget-governed; paid background work is denied by default"
                .to_string(),
        circuits: Vec::new(),
        policies: Vec::new(),
        active_invocations: vec![ModelInvocationView {
            owner_summary: "session 00000001".to_string(),
            lineage_summary: "root".to_string(),
            scope_summary: "session:00000000-0000-0000-0000-000000000001 | project:00000000-0000-0000-0000-000000000002".to_string(),
            denial_reason: None,
            stop_mechanism: "interrupt_session".to_string(),
            stop_target: Some(fix_uuid(1).to_string()),
            cancellation_reason: None,
            budget: vec![ModelBudgetHeadroom {
                scope_kind: BudgetScopeKind::Session,
                scope_id: Some(fix_uuid(1).to_string()),
                purpose: Some(ModelInvocationPurpose::SessionLaunchFresh),
                model_tier: Some(ModelTier::Premium),
                effort: Some("medium".to_string()),
                source: "policy:session/test".to_string(),
                authorized: true,
                policy_status: "configured".to_string(),
                remaining_calls: Some(2),
                remaining_active: Some(0),
                remaining_total_tokens: Some(20_000),
                remaining_input_tokens: Some(15_000),
                remaining_output_tokens: Some(5_000),
                remaining_embedding_inputs: None,
                remaining_wall_time_ms: Some(120_000),
            }],
            record: record.clone(),
        }],
        recent_invocations: vec![ModelInvocationView {
            owner_summary: "session 00000001".to_string(),
            lineage_summary: "root".to_string(),
            scope_summary: "session:00000000-0000-0000-0000-000000000001 | project:00000000-0000-0000-0000-000000000002".to_string(),
            denial_reason: None,
            stop_mechanism: "interrupt_session".to_string(),
            stop_target: Some(fix_uuid(1).to_string()),
            cancellation_reason: None,
            budget: Vec::new(),
            record,
        }],
        recent_denials: Vec::new(),
        recent_budget_alerts: Vec::new(),
    }
}

// ---- Tests --------------------------------------------------------------

#[test]
fn snap_launch_session_request() {
    let params = LaunchSessionParams {
        query: "implement feature X".to_string(),
        title: None,
        working_dir: Some(PathBuf::from("/tmp/repo")),
        provider: Some(SessionProvider::Claude),
        model: Some("claude-sonnet-5".to_string()),
        configured_context_window: None,
        system_prompt: None,
        session_kind: Some(SessionKind::Standard),
        project_id: Some(fix_uuid(2)),
        continued_from: None,
        parent_id: None,
        openai_base_url: None,
        openai_api_key: None,
        workflow_id: None,
        max_retries: Some(0),
        group_id: None,
        effort: None,
        sandbox: None,
        is_eval: None,
        skip_context_pipeline: None,
        tags: vec!["ci".to_string()],
        workflow_id_override: None,
    };
    let request = RpcRequest::new("LaunchSession", serde_json::to_value(&params).unwrap());
    insta::assert_json_snapshot!(request);
}

#[test]
fn snap_list_sessions_response() {
    let sessions = vec![fixture_session()];
    let response = RpcResponse::success(
        Some(Value::Number(1.into())),
        serde_json::to_value(&sessions).unwrap(),
    );
    insta::assert_json_snapshot!(response);
}

#[test]
fn snap_get_conversations_since_request() {
    let params = GetConversationsSinceParams {
        requests: vec![
            ConversationFetchCursor {
                session_id: fix_uuid(1),
                since_sequence: Some(10),
            },
            ConversationFetchCursor {
                session_id: fix_uuid(3),
                since_sequence: None,
            },
        ],
    };
    let request = RpcRequest::new(
        "GetConversationsSince",
        serde_json::to_value(&params).unwrap(),
    );
    insta::assert_json_snapshot!(request);
}

#[test]
fn snap_get_conversations_since_response() {
    let body = ConversationBatchResponse {
        conversations: vec![ConversationBatchEntry {
            session_id: fix_uuid(1),
            events: vec![fixture_event()],
        }],
    };
    let response = RpcResponse::success(
        Some(Value::Number(1.into())),
        serde_json::to_value(&body).unwrap(),
    );
    insta::assert_json_snapshot!(response);
}

#[test]
fn snap_get_model_control_status_request() {
    let params = GetModelControlStatusParams { recent_limit: 12 };
    let request = RpcRequest::new(
        "GetModelControlStatus",
        serde_json::to_value(&params).unwrap(),
    );
    insta::assert_json_snapshot!(request);
}

#[test]
fn snap_update_model_control_policy_request() {
    let params = UpdateModelControlPolicyParams {
        mode: ModelControlMode::StopAll,
        interrupt_active: true,
        replace_policies: false,
        policies: Vec::new(),
        circuit_updates: Vec::new(),
    };
    let request = RpcRequest::new(
        "UpdateModelControlPolicy",
        serde_json::to_value(&params).unwrap(),
    );
    insta::assert_json_snapshot!(request);
}

#[test]
fn snap_list_model_invocations_request() {
    let params = ListModelInvocationsParams {
        limit: 25,
        active_only: true,
        purpose: Some(ModelInvocationPurpose::SessionLaunchFresh),
        session_id: Some(fix_uuid(1)),
    };
    let request = RpcRequest::new(
        "ListModelInvocations",
        serde_json::to_value(&params).unwrap(),
    );
    insta::assert_json_snapshot!(request);
}

#[test]
fn snap_cancel_model_invocation_request() {
    let params = CancelModelInvocationParams {
        invocation_id: fix_uuid(9),
    };
    let request = RpcRequest::new(
        "CancelModelInvocation",
        serde_json::to_value(&params).unwrap(),
    );
    insta::assert_json_snapshot!(request);
}

#[test]
fn snap_get_model_control_status_response() {
    let response = RpcResponse::success(
        Some(Value::Number(1.into())),
        serde_json::to_value(&fixture_model_control_status()).unwrap(),
    );
    insta::assert_json_snapshot!(response);
}

#[test]
fn snap_get_health_status_response() {
    let body = HealthStatusResponse {
        persistence_queue_depth: 0,
        persistence_queue_capacity: 1024,
        last_command_duration_ms: 12,
        project_cache_size: 5,
        project_cache_hits: 100,
        project_cache_misses: 5,
        last_poll_payload_bytes: 4096,
        last_poll_event_count: 10,
        provider_claude_available: true,
        provider_codex_available: true,
        provider_pioneer_available: true,
        provider_openrouter_available: true,
        provider_bedrock_available: true,
        provider_local_available: false,
        provider_antigravity_available: false,
        provider_codex_app_server_available: true,
        provider_harness_available: true,
        queue_pending: 0,
        queue_claimed: 0,
        queue_completed: 42,
        queue_failed: 0,
        latest_daemon_restart: None,
        // V99/P1-B: a populated snapshot, so the wire shape of the new field is
        // pinned rather than only its empty-vec default.
        rate_limits: vec![ProviderRateLimitSnapshot {
            provider: SessionProvider::Claude,
            status: Some("allowed".to_string()),
            rate_limit_type: Some("five_hour".to_string()),
            overage_status: Some("rejected".to_string()),
            is_using_overage: false,
            observed_at: fix_ts(),
            windows: vec![
                ProviderRateLimitWindow {
                    window_key: "five_hour".to_string(),
                    utilization: 0.27,
                    resets_at_epoch: Some(1_788_402_000),
                },
                ProviderRateLimitWindow {
                    window_key: "seven_day".to_string(),
                    utilization: 0.05,
                    resets_at_epoch: Some(1_788_883_200),
                },
            ],
        }],
    };
    let response = RpcResponse::success(
        Some(Value::Number(1.into())),
        serde_json::to_value(&body).unwrap(),
    );
    insta::assert_json_snapshot!(response);
}

#[test]
fn snap_get_session_response() {
    let response = RpcResponse::success(
        Some(Value::Number(1.into())),
        serde_json::to_value(&fixture_session()).unwrap(),
    );
    insta::assert_json_snapshot!(response);
}

#[test]
fn snap_get_session_response_with_context_budget_provenance() {
    let mut session = fixture_session();
    session.resolved_context_budget = Some(ResolvedContextBudget {
        active_tokens: 200_000,
        capacity: ContextCapacity {
            advertised_max_tokens: Some(1_050_000),
            provider_default_tokens: Some(272_000),
            provider_max_tokens: Some(872_000),
            effective_percent: Some(95),
            configured_tokens: None,
            runtime_effective_tokens: Some(200_000),
            compaction_limit_tokens: Some(180_000),
            max_output_tokens: Some(128_000),
        },
        evidence: CapabilityEvidence {
            source: CapabilitySource::RuntimeTelemetry,
            source_version: Some("codex-cli 0.155.1".to_string()),
            source_digest: Some(format!("sha256:{}", "d".repeat(64))),
            observed_at: Some(fix_ts()),
            confidence: CapabilityConfidence::Authoritative,
        },
    });
    let response = RpcResponse::success(
        Some(Value::Number(1.into())),
        serde_json::to_value(&session).unwrap(),
    );
    insta::assert_json_snapshot!(response);
}

#[test]
fn snap_create_project_request() {
    // CreateProject is dispatched without a typed params struct in rsi-common —
    // the canonical wire shape is the Project payload. Snapshot the Project
    // serialization to lock the shape.
    let project = fixture_project();
    let request = RpcRequest::new("CreateProject", serde_json::to_value(&project).unwrap());
    insta::assert_json_snapshot!(request);
}

/// T8 — pin `GetUsageStats` request wire shape. satisfies: F-008
#[test]
fn snap_get_usage_stats_request() {
    let params = GetUsageStatsParams {
        project_id: Some(fix_uuid(2)),
    };
    let request = RpcRequest::new("GetUsageStats", serde_json::to_value(&params).unwrap());
    insta::assert_json_snapshot!(request);
}

/// T8 — pin `GetUsageStats` response wire shape (mirror TD1 Session regen).
/// satisfies: F-008
#[test]
fn snap_get_usage_stats_response() {
    let body = UsageStats {
        lifetime_chats: 42,
        total_cost_usd: 12.34,
        total_input_tokens: 100_000,
        total_output_tokens: 20_000,
        total_cache_creation_tokens: 3_000,
        total_cache_read_tokens: 4_000,
        total_work_time_ms: 3_600_000,
        per_model: vec![ModelUsage {
            model: "claude-sonnet-5".to_string(),
            chats: 30,
            cost_usd: 10.0,
            input_tokens: 80_000,
            output_tokens: 15_000,
            cache_creation_tokens: 2_000,
            cache_read_tokens: 3_000,
            work_time_ms: 2_400_000,
        }],
        timeline: vec![UsageBucket {
            day: "2026-07-08".to_string(),
            cost_usd: 1.5,
            input_tokens: 500,
            output_tokens: 100,
            cache_creation_tokens: 10,
            cache_read_tokens: 20,
        }],
    };
    let response = RpcResponse::success(
        Some(Value::Number(1.into())),
        serde_json::to_value(&body).unwrap(),
    );
    insta::assert_json_snapshot!(response);
}

#[test]
fn snap_launch_session_error_response() {
    let response = RpcResponse::error(
        Some(Value::Number(1.into())),
        RpcError {
            code: INVALID_PARAMS,
            message: "missing required field: query".to_string(),
            data: None,
        },
    );
    insta::assert_json_snapshot!(response);
}

#[test]
fn snap_bus_event_wire_format() {
    let event = BusEvent {
        event_type: "session_updated".to_string(),
        timestamp: fix_ts(),
        data: serde_json::json!({
            "session_id": fix_uuid(1).to_string(),
            "status": "Running",
        }),
    };
    insta::assert_json_snapshot!(event);
}
