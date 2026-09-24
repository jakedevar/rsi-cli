//! Parent picker overlay — pick a new hierarchical parent for a session.
//!
//! Mirrors `overlay/label_picker.rs`. The candidate list is precomputed when
//! the overlay opens, filtered through `legal_children(parent_kind)` so that
//! only valid parent options ever appear. A `Uuid::nil()` slot encodes the
//! `[root]` option (parent = None) when the focused session may live at the
//! root of the hierarchy.

use crate::app::App;
use crate::types::OverlayState;
use crate::types::resolve_session_display_identity;
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::types::legal_children;

use super::list;

/// Open the parent picker for `session_id`. Pulls the candidate list from
/// `app.sessions`, filtering against `legal_children` for both directions
/// (the candidate must legally accept this session's kind).
pub fn open_parent_picker(app: &mut App, session_id: uuid::Uuid) {
    let this_kind = match app.sessions.get(&session_id) {
        Some(s) => s.session.session_kind,
        None => {
            tracing::warn!(?session_id, "open_parent_picker: session not found");
            return;
        }
    };

    let candidates = compute_parent_candidates(app, this_kind, Some(session_id));

    app.overlay = OverlayState::ParentPicker {
        session_id,
        candidates,
        selected: 0,
        query: String::new(),
    };
    app.mark_dirty();
}

/// Compute the candidate parent list for a session of `this_kind`.
/// Optionally exclude a specific session id (when reparenting an existing
/// session, we don't want to allow it to choose itself).
///
/// Decision 7 (P2.3): extracted from `open_parent_picker` so the
/// `CreateEntityForm` can compute candidates BEFORE the entity is created
/// (no session id exists yet). The form's open path passes `None` for
/// `exclude` and `form.kind` as `this_kind`.
pub(crate) fn compute_parent_candidates(
    app: &App,
    this_kind: rsi_common::types::SessionKind,
    exclude: Option<uuid::Uuid>,
) -> Vec<uuid::Uuid> {
    let mut candidates: Vec<uuid::Uuid> = Vec::new();

    // Optional [root] option, encoded as Uuid::nil()
    if legal_children(None).contains(&this_kind) {
        candidates.push(uuid::Uuid::nil());
    }

    // Other sessions whose kind legally accepts `this_kind` as a child.
    let mut others: Vec<(String, uuid::Uuid)> = app
        .sessions
        .values()
        .filter(|s| exclude.map(|id| s.session.id != id).unwrap_or(true))
        .filter(|s| legal_children(Some(s.session.session_kind)).contains(&this_kind))
        .map(|s| {
            let label = resolve_session_display_identity(&s.session, &app.sessions).effective_title;
            (label, s.session.id)
        })
        .collect();
    others.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    candidates.extend(others.into_iter().map(|(_, id)| id));

    candidates
}

/// Compute filtered candidate indices that match the current query.
pub(crate) fn filtered_indices(app: &App) -> Vec<usize> {
    let (candidates, query) = match &app.overlay {
        OverlayState::ParentPicker {
            candidates, query, ..
        } => (candidates.clone(), query.clone()),
        _ => return Vec::new(),
    };
    let q = query.to_lowercase();
    candidates
        .iter()
        .enumerate()
        .filter(|(_, id)| {
            if q.is_empty() {
                return true;
            }
            if id.is_nil() {
                return "[root]".contains(&q);
            }
            let label = app
                .sessions
                .get(id)
                .map(|s| {
                    resolve_session_display_identity(&s.session, &app.sessions).effective_title
                })
                .unwrap_or_default();
            label.to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Handle keys in the parent picker overlay.
pub(super) async fn handle_parent_picker_key(app: &mut App, key: KeyEvent) {
    let visible = filtered_indices(app);
    let visible_count = visible.len();

    if let OverlayState::ParentPicker { selected, .. } = &mut app.overlay
        && list::handle_list_nav_key(selected, visible_count, &key)
    {
        app.mark_dirty();
        return;
    }

    match key.code {
        KeyCode::Enter => {
            submit_parent_picker(app).await;
        }
        KeyCode::Esc => {
            // Sentinel routing: when opened from CreateEntityForm via `gp`,
            // restore the pending form instead of clearing the overlay.
            let is_form_sentinel = matches!(
                &app.overlay,
                OverlayState::ParentPicker { session_id, .. } if session_id.is_nil()
            );
            if is_form_sentinel {
                if let Some(pending) = app.create_entity_form_pending.take() {
                    app.overlay = *pending;
                } else {
                    app.overlay = OverlayState::None;
                }
            } else {
                app.overlay = OverlayState::None;
            }
            app.mark_dirty();
        }
        KeyCode::Char(c) => {
            if let OverlayState::ParentPicker {
                query, selected, ..
            } = &mut app.overlay
            {
                query.push(c);
                *selected = 0;
                app.mark_dirty();
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::ParentPicker {
                query, selected, ..
            } = &mut app.overlay
            {
                query.pop();
                *selected = 0;
                app.mark_dirty();
            }
        }
        _ => {}
    }
}

async fn submit_parent_picker(app: &mut App) {
    let visible = filtered_indices(app);
    let (session_id, candidates, selected_idx) = match &app.overlay {
        OverlayState::ParentPicker {
            session_id,
            candidates,
            selected,
            ..
        } => (*session_id, candidates.clone(), *selected),
        _ => return,
    };

    let original_idx = match visible.get(selected_idx) {
        Some(i) => *i,
        None => return,
    };
    let chosen = match candidates.get(original_idx) {
        Some(id) => *id,
        None => return,
    };
    let new_parent: Option<uuid::Uuid> = if chosen.is_nil() { None } else { Some(chosen) };

    // Sentinel routing (P2.3 Phase 5): session_id == Uuid::nil() means the
    // picker was opened from the CreateEntityForm `gp` chord. Write back
    // into the pending form state, restore the form, do NOT call the
    // set_session_parent daemon RPC (there is no session yet).
    if session_id.is_nil() {
        if let Some(mut pending) = app.create_entity_form_pending.take() {
            if let OverlayState::CreateEntityForm { parent_id, .. } = pending.as_mut() {
                *parent_id = new_parent;
            }
            app.overlay = *pending;
        } else {
            // Defensive fallback: no pending form. Just close the picker.
            app.overlay = OverlayState::None;
        }
        app.mark_dirty();
        return;
    }

    match app.client.set_session_parent(session_id, new_parent).await {
        Ok(()) => {
            // Optimistic local update so the next render reflects the new parent.
            let old_parent = app.sessions.get(&session_id).map(|s| s.session.parent_id);
            if let Some(state) = app.sessions.get_mut(&session_id) {
                state.session.parent_id = new_parent;
            }
            app.sort_sessions(true);
            if let Some(parent_id) = old_parent {
                app.invalidate_hierarchy_node(parent_id);
            }
            app.invalidate_hierarchy_node(new_parent);
            app.notify_success("Parent updated");
        }
        Err(e) => {
            tracing::warn!(?session_id, error = %e, "set_session_parent failed");
            app.notify_error(format!("Reparent failed: {}", e));
        }
    }
    app.overlay = OverlayState::None;
    app.mark_dirty();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::state::{DevState, PersistedState};
    use crate::types::SessionState;
    use rsi_common::types::{Session, SessionKind, SessionProvider, SessionStatus};
    use std::path::PathBuf;

    fn test_app() -> App {
        DevState::clear();
        PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")))
    }

    fn make_session(kind: SessionKind) -> Session {
        Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: format!("query {:?}", kind),
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
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
            continued_from: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
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

    #[test]
    fn parent_picker_filters_to_legal_parents_only_for_epic_under_group() {
        let mut app = test_app();
        // A Group (legal parent for Epic), an Epic (illegal: cannot contain Epic),
        // and a Standard (illegal). Only the Group should remain.
        let group = make_session(SessionKind::Group);
        let other_epic = make_session(SessionKind::Epic);
        let standard = make_session(SessionKind::Standard);
        let target_epic = make_session(SessionKind::Epic);

        let target_id = target_epic.id;
        let group_id = group.id;
        for s in [group, other_epic, standard, target_epic] {
            let id = s.id;
            app.sessions.insert(id, SessionState::new(s));
        }

        open_parent_picker(&mut app, target_id);

        let candidates = match &app.overlay {
            OverlayState::ParentPicker { candidates, .. } => candidates.clone(),
            _ => panic!("Expected ParentPicker"),
        };

        // Epic can be a child of Group only — `[root]` is not legal for Epic.
        assert!(!candidates.iter().any(|id| id.is_nil()));
        assert!(candidates.contains(&group_id));
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn parent_picker_includes_root_for_standard() {
        let mut app = test_app();
        // Standard is a legal child of root and of Group.
        let group = make_session(SessionKind::Group);
        let target = make_session(SessionKind::Standard);
        let target_id = target.id;
        let group_id = group.id;
        for s in [group, target] {
            let id = s.id;
            app.sessions.insert(id, SessionState::new(s));
        }

        open_parent_picker(&mut app, target_id);

        let candidates = match &app.overlay {
            OverlayState::ParentPicker { candidates, .. } => candidates.clone(),
            _ => panic!("Expected ParentPicker"),
        };

        // Should include both [root] (nil) and Group.
        assert!(candidates.iter().any(|id| id.is_nil()));
        assert!(candidates.contains(&group_id));
    }
}
