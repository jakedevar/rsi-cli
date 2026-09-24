//! Tests for event-driven navigation delay elimination.

#[cfg(test)]
mod tests {
    use super::super::App;
    use crate::client::DaemonClient;
    use crate::types::{Pane, SessionState};
    use rsi_common::types::{
        ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
    };
    use std::path::PathBuf;
    use uuid::Uuid;

    fn test_app() -> App {
        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from(
            "/tmp/test-navigation-delay.sock",
        )))
    }

    fn mk_session(id: Uuid, kind: SessionKind, status: SessionStatus) -> Session {
        Session {
            context_fill_pct: None,
            id,
            claude_session_id: None,
            query: "test".to_string(),
            working_dir: PathBuf::from("/tmp"),
            status,
            session_kind: kind,
            provider: SessionProvider::Claude,
            context_usage_confidence: ContextUsageConfidence::default(),
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
            project_id: None,
            pinned_at: None,
            continued_from: None,
            handoff_filepath: None,
            rotation_depth: 0,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            git_branch: None,
            active_task: None,
            group_id: None,
            pending_archive: false,
            testing_needed_at: None,
            rotation_disabled_at: None,
            effort: None,
            retry_attempt: None,
            max_retries: None,
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
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            pending_question: None,
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

    #[tokio::test]
    async fn model_segments_fetch_triggered_on_session_detail_focus() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();

        // Set up a leaf session in the app
        let session = mk_session(session_id, SessionKind::Standard, SessionStatus::Running);
        let state = SessionState::new(session);
        app.sessions.insert(session_id, state);
        app.session_order.push(session_id);

        // Mock connected state
        app.poll.connected = true;

        // Establish initial state by running navigation effect with default pane
        app.run_navigation_effect_if_changed();

        // Verify no model segments fetch is initially in flight
        assert!(app.model_segments_fetch_inflight.is_empty());

        // Now change to SessionDetail pane to trigger dependency change
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = Pane::SessionDetail { session_id };
        }

        // Trigger navigation effect again - this should detect the change
        app.run_navigation_effect_if_changed();

        // Verify model segments fetch was triggered
        assert!(
            app.model_segments_fetch_inflight.contains(&session_id),
            "Model segments fetch should be triggered when focusing on SessionDetail"
        );
    }

    #[test]
    fn model_segments_fetch_not_triggered_on_container_focus() {
        let mut app = test_app();
        let group_id = Uuid::new_v4();

        // Set up a Group (container) session
        let session = mk_session(group_id, SessionKind::Group, SessionStatus::Running);
        let state = SessionState::new(session);
        app.sessions.insert(group_id, state);
        app.session_order.push(group_id);

        // Mock connected state
        app.poll.connected = true;

        // Focus on the container via descent path
        app.tabs[app.active_tab].descent_path = vec![group_id];

        // Verify no model segments fetch is initially in flight
        assert!(app.model_segments_fetch_inflight.is_empty());

        // Trigger navigation effect
        app.run_navigation_effect_if_changed();

        // Verify no model segments fetch was triggered for container
        assert!(
            app.model_segments_fetch_inflight.is_empty(),
            "Model segments fetch should not be triggered for container nodes"
        );
    }

    #[test]
    fn no_duplicate_model_segments_fetch_when_already_in_flight() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();

        // Set up session
        let session = mk_session(session_id, SessionKind::Standard, SessionStatus::Running);
        let state = SessionState::new(session);
        app.sessions.insert(session_id, state);

        // Mock connected state
        app.poll.connected = true;

        // Manually mark as in flight
        app.model_segments_fetch_inflight.insert(session_id);

        // Try to trigger fetch
        app.trigger_model_segments_fetch_if_needed(session_id);

        // Should only have one entry (the one we manually added)
        assert_eq!(
            app.model_segments_fetch_inflight.len(),
            1,
            "Should not create duplicate model segments fetch when already in flight"
        );
    }

    #[test]
    fn apply_model_segments_fetch_result_updates_state() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();

        // Set up session with empty model segments
        let session = mk_session(session_id, SessionKind::Standard, SessionStatus::Running);
        let state = SessionState::new(session);
        assert!(state.model_segments.is_empty());
        app.sessions.insert(session_id, state);

        // Mark as in flight
        app.model_segments_fetch_inflight.insert(session_id);

        // Create mock model segments
        let segments = vec![rsi_common::types::ModelSegment {
            id: 1,
            session_id,
            model_id: "claude-opus-4-1".to_string(),
            from_sequence: 1,
            to_sequence: Some(10),
            created_at: chrono::Utc::now(),
        }];

        // Apply result
        let result = super::super::ModelSegmentsFetchResult {
            session_id,
            segments: Ok(segments.clone()),
        };

        let dirty = app.apply_model_segments_fetch_result(result);

        // Verify state was updated
        assert!(dirty, "Should mark dirty when model segments change");
        assert!(
            !app.model_segments_fetch_inflight.contains(&session_id),
            "Should remove from inflight"
        );

        let updated_state = app.sessions.get(&session_id).unwrap();
        assert_eq!(
            updated_state.model_segments, segments,
            "Should update model segments"
        );
    }

    #[test]
    fn apply_model_segments_fetch_handles_errors_gracefully() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();

        // Set up session
        let session = mk_session(session_id, SessionKind::Standard, SessionStatus::Running);
        let state = SessionState::new(session);
        app.sessions.insert(session_id, state);

        // Mark as in flight
        app.model_segments_fetch_inflight.insert(session_id);

        // Apply error result
        let result = super::super::ModelSegmentsFetchResult {
            session_id,
            segments: Err("Network error".to_string()),
        };

        let dirty = app.apply_model_segments_fetch_result(result);

        // Verify error handling
        assert!(!dirty, "Should not mark dirty on error");
        assert!(
            !app.model_segments_fetch_inflight.contains(&session_id),
            "Should remove from inflight even on error"
        );

        let state = app.sessions.get(&session_id).unwrap();
        assert!(
            state.model_segments.is_empty(),
            "Should not modify model segments on error"
        );
    }
}
