//! Shared test-only helpers for constructing minimal `App` instances.
//!
//! Extracted so siblings outside `app/` (e.g. `action_handler/session.rs`'s
//! P1.12 gR validator tests) can build a focused-session App without
//! duplicating the `make_session` / `make_test_app` boilerplate that lives
//! private inside `app/sessions.rs::tests` and `app/tests.rs`.

use std::path::PathBuf;

use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, EventType, Role, Session, SessionKind,
    SessionProvider, SessionStatus,
};
use uuid::Uuid;

use super::App;
use crate::client::DaemonClient;
use crate::types::{Pane, PaneId, SessionState, SplitNode, Tab};

/// Build a baseline `Session` row with sensible defaults. Caller customizes
/// `session_kind`, `workflow_id`, `project_id`, etc. via field updates.
pub(crate) fn baseline_session(id: Uuid, kind: SessionKind) -> Session {
    Session {
        context_fill_pct: None,
        id,
        provider: SessionProvider::Claude,
        claude_session_id: None,
        query: "test".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status: SessionStatus::Running,
        project_id: None,
        session_kind: kind,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
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
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
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

/// Build a minimal `App` with one tab and one focused session of the given
/// `kind`. When `workflow_id` is `Some`, the focused session's `workflow_id`
/// field is populated (P1.12 gR-readiness check).
///
/// The session is selected (`SessionList::selected_session`) so
/// `App::selected_session_id` returns the right value in tests.
pub(crate) fn with_focused_kind(kind: SessionKind, workflow_id: Option<Uuid>) -> App {
    let client = DaemonClient::new(PathBuf::from("/tmp/test.sock"));
    let mut app = App::new(client);
    let session_id = Uuid::new_v4();
    let mut session = baseline_session(session_id, kind);
    session.workflow_id = workflow_id;
    app.sessions.insert(session_id, SessionState::new(session));
    let pane_id = PaneId(0);
    app.tabs = vec![Tab {
        name: "[1]".to_string(),
        session_list_state: Tab::default_session_list_state(),
        layout: SplitNode::Leaf {
            pane: Pane::SessionList {
                selected_index: 0,
                selected_session: Some(session_id),
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            },
            id: pane_id,
        },
        focused_pane: pane_id,
        project_id: None,
        detail_split_offset: 0,
        layout_x_offset_adj: 0,
        session_list_width_pct: 0,
        descent_path: Vec::new(),
        bottom_focus_target: crate::types::BottomZone::Off,
        mini_dag_focus: None,
    }];
    app.active_tab = 0;
    app.next_pane_id = 1;
    app
}

/// Build a daemon-free app fixture with a populated main session list.
///
/// Rendering tests should use this instead of relying on persisted state or an
/// RPC-backed daemon. The selected session is the first row.
pub(crate) fn with_session_list(count: usize) -> App {
    let client = DaemonClient::new(PathBuf::from("/tmp/test.sock"));
    let mut app = App::new(client);
    let pane_id = PaneId(0);
    let mut order = Vec::with_capacity(count);

    for idx in 0..count {
        let id = Uuid::new_v4();
        let mut session = baseline_session(id, SessionKind::Standard);
        session.title = Some(format!("Fixture session {idx:02}"));
        session.short_summary = Some(format!("Synthetic render row {idx:02}"));
        session.model = Some(if idx % 2 == 0 {
            "claude-sonnet-5".to_string()
        } else {
            "claude-opus-4-1".to_string()
        });
        session.cost_usd = Some((idx as f64 + 1.0) / 100.0);
        session.num_turns = Some((idx + 1) as u32);
        session.updated_at = chrono::Utc::now() - chrono::Duration::minutes(idx as i64);
        if idx % 5 == 0 {
            session.status = SessionStatus::Completed;
        }
        app.sessions.insert(id, SessionState::new(session));
        order.push(id);
    }

    let selected_session = order.first().copied();
    app.filtered_session_order = order;
    app.tabs = vec![Tab {
        name: "[1]".to_string(),
        session_list_state: Tab::default_session_list_state(),
        layout: SplitNode::Leaf {
            pane: Pane::SessionList {
                selected_index: 0,
                selected_session,
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            },
            id: pane_id,
        },
        focused_pane: pane_id,
        project_id: None,
        detail_split_offset: 0,
        layout_x_offset_adj: 0,
        session_list_width_pct: 0,
        descent_path: Vec::new(),
        bottom_focus_target: crate::types::BottomZone::Off,
        mini_dag_focus: None,
    }];
    app.active_tab = 0;
    app.next_pane_id = 1;
    app
}

/// Build a daemon-free app fixture with one selected detail session and enough
/// precomputed geometry for `render_session_detail`.
pub(crate) fn with_session_detail() -> (App, Uuid) {
    let mut app = with_session_list(1);
    let session_id = app.filtered_session_order[0];
    let event = ConversationEvent {
        id: 1,
        session_id,
        sequence: 1,
        event_type: EventType::Message,
        role: Some(Role::Assistant),
        content: "Fixture assistant response for session detail rendering.".to_string(),
        tool_name: None,
        tool_input: None,
        created_at: chrono::Utc::now(),
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    };
    if let Some(state) = app.sessions.get_mut(&session_id) {
        state.events = vec![event];
        state.event_offsets = vec![0];
        state.event_heights = vec![5];
        state.total_content_height = 5;
        state.scroll_offset = 0;
    }
    (app, session_id)
}

/// T6 Decision D6 (F-001, F-017): a canonical `ToolUse` (with `tool_input`) +
/// matching `ToolResult` event pair, parameterized by `SessionProvider`.
///
/// Modeled on `theme_opaque_bg.rs`'s `push_event`/`probe_session` pattern —
/// the closest existing precedent for building a minimal provider-tagged
/// fixture session + event pair. Used to prove `content::build_event_lines_with_meta`
/// renders tool-call events identically regardless of which provider owns the
/// session: `content.rs` never reads `Session.provider` (or any provider tag)
/// at all, so this is the map's "same fixture conversation from each provider
/// renders identical component" verify bullet as an automated test.
pub(crate) fn cross_provider_tool_call_events(
    provider: SessionProvider,
) -> (Session, Vec<ConversationEvent>) {
    let session_id = Uuid::new_v4();
    let mut session = baseline_session(session_id, SessionKind::Standard);
    session.provider = provider;

    let created_at = chrono::Utc::now();
    let events = vec![
        ConversationEvent {
            id: 0,
            session_id,
            sequence: 1,
            event_type: EventType::ToolUse,
            role: Some(Role::Assistant),
            content: String::new(),
            tool_name: Some("Bash".to_string()),
            tool_input: Some(Box::new(
                serde_json::json!({"command": "echo cross-provider fixture"}),
            )),
            created_at,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        },
        ConversationEvent {
            id: 0,
            session_id,
            sequence: 2,
            event_type: EventType::ToolResult,
            role: Some(Role::Assistant),
            content: "cross-provider fixture\n".to_string(),
            tool_name: Some("Bash".to_string()),
            tool_input: None,
            created_at,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        },
    ];
    (session, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::content::{self, EventRenderContext};

    /// D6/PI-11: `content::build_event_lines_with_meta` must render the
    /// canonical ToolUse/ToolResult fixture identically regardless of
    /// `Session.provider` — operationalizes the map's cross-provider verify
    /// bullet as an automated test (F-001, F-005, F-006, F-007, F-017).
    #[test]
    fn tool_call_rendering_is_identical_across_all_session_providers() {
        let providers = [
            SessionProvider::Claude,
            SessionProvider::Codex,
            SessionProvider::Pioneer,
            SessionProvider::OpenRouter,
            SessionProvider::Local,
            SessionProvider::Antigravity,
            SessionProvider::CodexAppServer,
            SessionProvider::Harness,
        ];

        let ctx_for = |is_collapsed: bool| EventRenderContext {
            is_collapsed,
            is_expanded: !is_collapsed,
            is_cursor: false,
            is_last_event: false,
            model_name: None,
            pipeline_commands: Vec::new(),
            max_width: 80,
        };

        let mut baseline: Option<(
            Vec<ratatui::text::Line<'static>>,
            Vec<ratatui::text::Line<'static>>,
        )> = None;

        for provider in providers {
            let (_session, events) = cross_provider_tool_call_events(provider);
            let tool_use = &events[0];
            let tool_result = &events[1];

            // Both collapsed and expanded ToolUse renderings, plus the
            // ToolResult rendering — concatenated into one comparable unit
            // per provider.
            let (collapsed_lines, _) =
                content::build_event_lines_with_meta(tool_use, &ctx_for(true));
            let (expanded_lines, _) =
                content::build_event_lines_with_meta(tool_use, &ctx_for(false));
            let (result_lines, _) =
                content::build_event_lines_with_meta(tool_result, &ctx_for(false));
            let tool_use_lines: Vec<_> = [collapsed_lines, expanded_lines].concat();

            match &baseline {
                None => baseline = Some((tool_use_lines, result_lines)),
                Some((base_tool_use, base_result)) => {
                    assert_eq!(
                        &tool_use_lines, base_tool_use,
                        "ToolUse rendering differs for provider {provider:?} vs. the \
                         first (Claude) baseline — content.rs must be provider-blind"
                    );
                    assert_eq!(
                        &result_lines, base_result,
                        "ToolResult rendering differs for provider {provider:?} vs. the \
                         first (Claude) baseline — content.rs must be provider-blind"
                    );
                }
            }
        }
    }
}
