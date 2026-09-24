//! Session lifecycle action handlers.
//!
//! Covers: launch (TaskRabbit, blank, standard), interrupt, continue,
//! approve/deny, archive/unarchive, pin, reassign, rotate, docregblock
//! execution, commit-and-push, and yank.

use crate::app::App;
use crate::modalkit_types::LcAction;
use crate::types::{OverlayState, Pane};

pub(super) async fn dispatch(app: &mut App, action: LcAction) {
    match action {
        LcAction::TaskRabbitPrompt => {
            // Always open a new TaskRabbit (input overlays stack)
            crate::overlay::open_taskrabbit_popup(app);
        }

        LcAction::LaunchTaskRabbit(query) => {
            app.launch_taskrabbit(&query, None, None, None, None).await;
        }

        LcAction::BlankPrompt => {
            // Always open a new Blank prompt (input overlays stack)
            crate::overlay::open_blank_popup(app);
        }

        LcAction::LaunchBlank(query) => {
            app.launch_blank(&query, None, None, None, None).await;
        }

        LcAction::InterruptSession => {
            app.interrupt_focused_session().await;
        }

        LcAction::QuickContinue => {
            let session_id = match app.focused_pane().cloned() {
                Some(Pane::SessionDetail { session_id }) => Some(session_id),
                _ => app.selected_session_id(),
            };
            if let Some(sid) = session_id {
                if let Some(state) = app.sessions.get(&sid)
                    && state.session.status == rsi_common::types::SessionStatus::Starting
                {
                    app.notify("Session is starting — wait for it to begin");
                    return;
                }
                app.continue_session(sid, "continue").await;
            } else {
                app.notify("No session selected");
            }
        }

        LcAction::ContinueSession(query) => match query {
            Some(q) => {
                if let Some(session_id) = app.selected_session_id() {
                    app.continue_session(session_id, &q).await;
                } else {
                    app.notify("No session selected");
                }
            }
            None => {
                let session_id = match app.focused_pane().cloned() {
                    Some(Pane::SessionDetail { session_id }) => Some(session_id),
                    _ => app.selected_session_id(),
                };
                if let Some(sid) = session_id {
                    if let Some(state) = app.sessions.get(&sid) {
                        if state.session.status == rsi_common::types::SessionStatus::Starting {
                            app.notify("Session is starting — wait for it to begin");
                        }
                    }
                    crate::overlay::open_continue_popup(app, sid);
                } else {
                    app.notify("No session selected");
                }
            }
        },

        LcAction::OpenQuestionModal => {
            crate::overlay::open_question_modal(app);
        }

        LcAction::DeleteSession => {
            app.delete_focused_session().await;
        }

        LcAction::ArchiveSession => {
            app.archive_focused_session().await;
        }

        LcAction::UnarchiveSession => {
            app.unarchive_focused_session().await;
        }

        LcAction::TogglePinSession => {
            app.toggle_pin_focused_session().await;
        }

        LcAction::ToggleTestingNeeded => {
            app.toggle_testing_needed_focused_session().await;
        }

        LcAction::ToggleRotationDisabled => {
            app.toggle_rotation_disabled_focused_session().await;
        }

        LcAction::CancelRetry => {
            app.cancel_retry_focused_session().await;
        }

        LcAction::ReassignSessionProject => {
            let session_id = match app.focused_pane().cloned() {
                Some(Pane::SessionDetail { session_id }) => Some(session_id),
                Some(Pane::SessionList {
                    selected_session, ..
                }) => selected_session,
                _ => None,
            };
            if let Some(session_id) = session_id {
                crate::overlay::open_project_picker(
                    app,
                    crate::types::ProjectPickerContext::SessionReassign(session_id),
                );
            } else {
                app.notify("No session selected");
            }
        }

        LcAction::RotateSession => {
            app.rotate_focused_session().await;
        }

        LcAction::CommitAndPush => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get(&session_id)
            {
                let status = state.session.status;
                match status {
                    rsi_common::types::SessionStatus::Starting => {
                        app.notify("Session is starting — wait for it to begin");
                    }
                    _ => {
                        let command = "/ci_commit".to_string();
                        app.notify("Committing and pushing...");
                        app.continue_session(session_id, &command).await;
                    }
                }
            }
        }

        LcAction::ExecuteDocRegBlocks => {
            let session_id = match app.focused_pane().cloned() {
                Some(Pane::SessionDetail { session_id }) => Some(session_id),
                _ => app.selected_session_id(),
            };
            if let Some(sid) = session_id {
                let was_detail = matches!(
                    app.focused_pane().cloned(),
                    Some(Pane::SessionDetail { .. })
                );
                if app.request_docreg_restore_if_needed(sid, was_detail) {
                    return;
                }
                let Some(state) = app.sessions.get(&sid) else {
                    return;
                };
                let contents = state.docregblock_contents.clone();
                if contents.is_empty() {
                    app.notify("No docregblock tags found");
                } else {
                    // Partition into slash commands (continue in current session)
                    // vs prompts (launch new sessions). Skip /merge_ready.
                    // Pipeline commands (/plan, /implement) always launch
                    // new sessions rather than continuing the current one.
                    let mut to_continue: Vec<String> = Vec::new();
                    for c in contents {
                        let cmd = c.trim().split_whitespace().next().unwrap_or("");
                        let cmd_clean = cmd.strip_prefix('/').unwrap_or(cmd);
                        if cmd_clean == "merge_ready" {
                            continue;
                        }
                        // Pipeline commands launch new sessions (not continue)
                        let is_pipeline_cmd = matches!(
                            cmd_clean,
                            "plan" | "implement" | "research" | "resume_handoff" | "iterate_plan"
                        );
                        if cmd.starts_with('/') && !is_pipeline_cmd {
                            to_continue.push(c);
                        }
                    }

                    // Send slash commands to the current session.
                    let mut accepted_continues = 0usize;
                    for content in &to_continue {
                        if app.continue_session(sid, content).await {
                            accepted_continues += 1;
                        }
                    }

                    // Launch replacements on an App-owned response task. The
                    // source stays in place until every response and, when
                    // applicable, the archive response is accepted.
                    if accepted_continues > 0 {
                        app.notify_success(format!(
                            "Sent {} command{} to session",
                            accepted_continues,
                            if accepted_continues == 1 { "" } else { "s" }
                        ));
                    }
                    app.request_docreg_replacements(sid, was_detail);
                }
            }
        }

        LcAction::YankEventContent => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get(&session_id)
                && let Some(idx) = state.current_event_index
            {
                let content = &state.events[idx].content;
                if content.is_empty() {
                    app.notify("Nothing to yank");
                } else {
                    crate::clipboard::osc52_copy(content);
                    let lines = content.lines().count();
                    app.notify(format!(
                        "{} line{} yanked",
                        lines,
                        if lines == 1 { "" } else { "s" }
                    ));
                }
            } else {
                let context = crate::action_registry::ActionContext::from_app(app);
                let request = crate::action_registry::ActionRequest::plain(
                    crate::action_registry::ActionId::CopySessionUuid,
                );
                if let crate::action_registry::ActionAvailability::Available(request) =
                    crate::action_registry::recheck_request(&context, request)
                {
                    crate::action_handler::dispatch_registered_action(app, request).await;
                }
            }
        }

        LcAction::RenameSession => {
            if let Some(session_id) = app.selected_session_id() {
                if let Some(state) = app.sessions.get(&session_id) {
                    // Rename edits the recoverable raw/manual title, not the
                    // derived role/lead display identity.
                    let current_title = state
                        .session
                        .title
                        .as_deref()
                        .unwrap_or(&state.session.query)
                        .to_string();
                    app.overlay = OverlayState::RenameSession {
                        session_id,
                        title: current_title,
                    };
                }
            }
        }

        LcAction::SubmitSessionTitle(session_id, title) => {
            app.client
                .update_session_title(session_id, &title)
                .await
                .ok();
            // Update local state immediately for responsiveness
            if let Some(state) = app.sessions.get_mut(&session_id) {
                state.session.title = Some(title);
            }
            app.notify("Title updated");
        }

        LcAction::SetActiveTask(active_task) => {
            if let Some(session_id) = app.selected_session_id() {
                let task_ref = active_task.as_deref();
                match app.client.update_active_task(session_id, task_ref).await {
                    Ok(()) => {
                        // Update local state immediately for responsive UI
                        if let Some(state) = app.sessions.get_mut(&session_id) {
                            state.session.active_task = active_task.clone();
                        }
                        match &active_task {
                            Some(text) => app.notify(&format!("Active task set: {}", text)),
                            None => app.notify("Active task cleared"),
                        }
                    }
                    Err(e) => {
                        app.notify(&format!("Failed to update active task: {}", e));
                    }
                }
            } else {
                app.notify("No session selected");
            }
        }

        // === Phase 4: Hierarchy creation chords ===
        LcAction::CreateGroup => {
            open_create_entity_form_with_kind(app, rsi_common::types::SessionKind::Group).await;
        }

        LcAction::CreateEpic => {
            open_create_entity_form_with_kind(app, rsi_common::types::SessionKind::Epic).await;
        }

        LcAction::CreateStory => {
            open_create_entity_form_with_kind(app, rsi_common::types::SessionKind::Story).await;
        }

        LcAction::CreateTask => {
            open_create_entity_form_with_kind(app, rsi_common::types::SessionKind::Task).await;
        }

        LcAction::CreateBug => {
            open_create_entity_form_with_kind(app, rsi_common::types::SessionKind::Bug).await;
        }

        LcAction::SetEpicLead => {
            let Some(target_id) = app.selected_session_id() else {
                app.notify("No session selected");
                return;
            };
            let target = app.sessions.get(&target_id).map(|s| s.session.clone());
            let Some(target) = target else {
                return;
            };
            let Some(parent_id) = target.parent_id else {
                app.notify("Session has no parent Epic");
                return;
            };
            let parent_kind = app.sessions.get(&parent_id).map(|s| s.session.session_kind);
            if parent_kind != Some(rsi_common::types::SessionKind::Epic) {
                app.notify("Parent is not an Epic");
                return;
            }
            match app.client.set_epic_lead(parent_id, Some(target_id)).await {
                Ok(()) => {
                    if let Some(parent) = app.sessions.get_mut(&parent_id) {
                        parent.session.lead_session_id = Some(target_id);
                    }
                    app.invalidate_card_cache();
                    app.notify_success("Lead set");
                }
                Err(e) => app.notify_error(format!("Set lead failed: {e}")),
            }
        }

        LcAction::RunEpicTopology => {
            run_epic_topology(app).await;
        }

        _ => unreachable!("session::dispatch called with non-session action"),
    }
}

/// Phase 4 helper: descent head of the active tab (current container scope).
pub(crate) fn current_descent_head(app: &App) -> Option<uuid::Uuid> {
    app.tabs
        .get(app.active_tab)
        .and_then(|t| t.descent_path.last().copied())
}

/// Phase 4 helper: resolve a Uuid to its SessionKind (None when uuid is None
/// or the session is not loaded — interpreted as root for the matrix lookup).
pub(crate) fn parent_kind_of(
    app: &App,
    parent_uuid: Option<uuid::Uuid>,
) -> Option<rsi_common::types::SessionKind> {
    parent_uuid
        .and_then(|id| app.sessions.get(&id))
        .map(|s| s.session.session_kind)
}

// ── P1.12 §7: gR — RunEpicTopology ───────────────────────────────────────────

/// Outcome of validating a focused session against the `gR` chord's
/// preconditions. Extracted as a pure helper so unit tests can exercise the
/// rejection branches without spinning up tokio + a mock client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunEpicTopologyValidation {
    /// Session is an Epic with a `workflow_id` binding — proceed with
    /// `ExecuteTopology` using these values.
    Ready {
        topology_id: uuid::Uuid,
        epic_id: uuid::Uuid,
        project_id: Option<uuid::Uuid>,
    },
    /// No session is focused — produce "No session selected".
    NoSelection,
    /// Focused session not loaded in the in-memory map — produce nothing
    /// (mirrors `SetEpicLead`'s silent early-return).
    UnknownSession,
    /// Focused session is not an Epic — produce "Not an Epic" toast.
    NotEpic,
    /// Focused session is an Epic but has no `workflow_id` binding —
    /// produce "no topology binding" toast.
    NoTopologyBinding,
}

/// Validate the focused session against the `gR` chord's preconditions.
///
/// Pure — no async, no mutation. Tests call this directly to exercise each
/// rejection branch without spinning up a tokio runtime or a mock client.
pub(crate) fn validate_run_epic_topology(app: &App) -> RunEpicTopologyValidation {
    let Some(target_id) = app.selected_session_id() else {
        return RunEpicTopologyValidation::NoSelection;
    };
    let Some(tracked) = app.sessions.get(&target_id) else {
        return RunEpicTopologyValidation::UnknownSession;
    };
    let session = &tracked.session;
    if !matches!(session.session_kind, rsi_common::types::SessionKind::Epic) {
        return RunEpicTopologyValidation::NotEpic;
    }
    let Some(topology_id) = session.workflow_id else {
        return RunEpicTopologyValidation::NoTopologyBinding;
    };
    RunEpicTopologyValidation::Ready {
        topology_id,
        epic_id: session.id,
        project_id: session.project_id,
    }
}

/// Handler for `LcAction::RunEpicTopology` (the `gR` chord).
///
/// Fires `ExecuteTopology` against the focused Epic's bound topology with
/// `parent_id = Epic.id`. On success, spawned executor children become
/// visible in the Epic's session-list child view. On failure (validation
/// or RPC error), surfaces an error toast and returns silently.
async fn run_epic_topology(app: &mut App) {
    match validate_run_epic_topology(app) {
        RunEpicTopologyValidation::NoSelection => {
            app.notify("No session selected");
        }
        RunEpicTopologyValidation::UnknownSession => {
            // Mirror SetEpicLead's silent early-return on a missing session.
        }
        RunEpicTopologyValidation::NotEpic => {
            app.notify_error("Not an Epic — gR only fires on Epic containers");
        }
        RunEpicTopologyValidation::NoTopologyBinding => {
            app.notify_error("Epic has no topology binding — set one via gv overlay first");
        }
        RunEpicTopologyValidation::Ready {
            topology_id,
            epic_id,
            project_id,
        } => {
            match app
                .client
                .execute_topology(
                    topology_id,
                    project_id,
                    serde_json::json!({}),
                    Some(epic_id),
                )
                .await
            {
                Ok(exec_id) => app.notify_success(format!(
                    "Topology execution started under Epic {epic_id} (exec {exec_id})"
                )),
                Err(e) => app.notify_error(format!("ExecuteTopology failed: {e}")),
            }
        }
    }
}

/// Phase 2 (P2.2) helper: open the unified `CreateEntityForm` overlay with
/// `kind` pre-selected, after performing the chord-level `legal_children`
/// precheck.
///
/// The overlay re-runs the same precheck at open time (and surfaces the
/// rejection in its red banner). The chord-level precheck preserves the
/// pre-P2.2 UX of "illegal chord = silent notify, no overlay opens".
///
/// P2.3: async because `open_create_entity_form` now fetches the topology
/// list once at open time.
async fn open_create_entity_form_with_kind(app: &mut App, kind: rsi_common::types::SessionKind) {
    let parent_uuid = current_descent_head(app);
    let parent_kind = parent_kind_of(app, parent_uuid);
    if !rsi_common::types::legal_children(parent_kind).contains(&kind) {
        tracing::warn!(
            ?kind,
            ?parent_kind,
            "Create{:?} rejected: parent does not accept this kind",
            kind
        );
        app.notify(format!(
            "{:?} not allowed under {:?}",
            kind,
            parent_kind.unwrap_or(rsi_common::types::SessionKind::Standard)
        ));
        return;
    }
    crate::overlay::open_create_entity_form(app, kind, parent_uuid).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    // Inline-build a minimal App for these tests via app_test_helpers.

    fn terminal_docreg_detail_app() -> (App, uuid::Uuid) {
        let mut app = crate::app::app_test_helpers::with_focused_kind(
            rsi_common::types::SessionKind::Standard,
            None,
        );
        let session_id = app.selected_session_id().expect("selected source");
        let state = app.sessions.get_mut(&session_id).expect("source state");
        state.session.status = rsi_common::types::SessionStatus::Completed;
        state.session.title = Some("Retryable handoff source".to_string());
        state.docregblock_contents =
            vec!["/resume_handoff @thoughts/shared/handoffs/retry.md".to_string()];
        *app.focused_pane_mut().expect("focused pane") = Pane::SessionDetail { session_id };
        (app, session_id)
    }

    fn assert_retryable_source_and_detail(app: &App, session_id: uuid::Uuid, message: &str) {
        assert_eq!(
            app.sessions[&session_id].session.title.as_deref(),
            Some("Retryable handoff source")
        );
        assert!(matches!(
            app.focused_pane(),
            Some(Pane::SessionDetail { session_id: focused }) if *focused == session_id
        ));
        assert!(
            app.notifications
                .iter()
                .any(|notification| { notification.message.contains(message) })
        );
    }

    #[derive(Debug)]
    enum ArchiveAdversaryCommand {
        PushConversation(rsi_common::types::ConversationEvent),
        PushArchive,
        RespondArchive,
        RespondRestoreError,
        AssertNoFurtherRequests,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ArchiveAdversaryEvent {
        Subscribed,
        ArchiveRequested,
    }

    async fn read_adversary_request(
        stream: tokio::net::UnixStream,
    ) -> (serde_json::Value, tokio::net::unix::OwnedWriteHalf) {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let (reader, writer) = stream.into_split();
        let line = BufReader::new(reader)
            .lines()
            .next_line()
            .await
            .expect("adversary request read")
            .expect("adversary request line");
        (
            serde_json::from_str(&line).expect("adversary request JSON"),
            writer,
        )
    }

    async fn write_adversary_json(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        value: &serde_json::Value,
    ) {
        use tokio::io::AsyncWriteExt;

        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .expect("adversary response write");
    }

    fn spawn_archive_adversary(
        listener: tokio::net::UnixListener,
        session_id: uuid::Uuid,
    ) -> (
        tokio::sync::mpsc::UnboundedSender<ArchiveAdversaryCommand>,
        tokio::sync::mpsc::UnboundedReceiver<ArchiveAdversaryEvent>,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (subscription, _) = listener.accept().await.expect("subscription client");
            let (subscribe_request, mut subscription_writer) =
                read_adversary_request(subscription).await;
            assert_eq!(subscribe_request["method"], "Subscribe");
            let subscribe_response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": subscribe_request["id"].clone(),
                "result": {},
            });
            write_adversary_json(&mut subscription_writer, &subscribe_response).await;
            event_tx
                .send(ArchiveAdversaryEvent::Subscribed)
                .expect("subscription event receiver");

            let (archive, _) = listener.accept().await.expect("archive client");
            let (archive_request, mut archive_writer) = read_adversary_request(archive).await;
            assert_eq!(archive_request["method"], "ArchiveSession");
            assert_eq!(
                archive_request["params"]["session_id"],
                session_id.to_string()
            );
            let mut requests = vec!["Subscribe".to_string(), "ArchiveSession".to_string()];
            event_tx
                .send(ArchiveAdversaryEvent::ArchiveRequested)
                .expect("archive event receiver");

            while let Some(command) = command_rx.recv().await {
                match command {
                    ArchiveAdversaryCommand::PushConversation(event) => {
                        let push = serde_json::to_value(rsi_common::rpc::BusEvent {
                            event_type: "conversation_event".to_string(),
                            data: serde_json::json!({
                                "session_id": session_id,
                                "event": event,
                            }),
                            timestamp: chrono::Utc::now(),
                        })
                        .expect("conversation push JSON");
                        write_adversary_json(&mut subscription_writer, &push).await;
                    }
                    ArchiveAdversaryCommand::PushArchive => {
                        let push = serde_json::to_value(rsi_common::rpc::BusEvent {
                            event_type: "session_archived".to_string(),
                            data: serde_json::json!({"session_id": session_id}),
                            timestamp: chrono::Utc::now(),
                        })
                        .expect("archive push JSON");
                        write_adversary_json(&mut subscription_writer, &push).await;
                    }
                    ArchiveAdversaryCommand::RespondArchive => {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": archive_request["id"].clone(),
                            "result": rsi_common::archive_cleanup::ArchiveSessionResultV1::no_cleanup_required(),
                        });
                        write_adversary_json(&mut archive_writer, &response).await;
                    }
                    ArchiveAdversaryCommand::RespondRestoreError => {
                        let (restore, _) = tokio::time::timeout(
                            std::time::Duration::from_secs(1),
                            listener.accept(),
                        )
                        .await
                        .expect("restore request deadline")
                        .expect("restore client");
                        let (restore_request, mut restore_writer) =
                            read_adversary_request(restore).await;
                        let method = restore_request["method"]
                            .as_str()
                            .expect("restore method")
                            .to_string();
                        requests.push(method.clone());
                        assert_eq!(method, "UnarchiveSession");
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": restore_request["id"].clone(),
                            "error": {
                                "code": -32076,
                                "message": "S1 restore rejected exactly",
                            },
                        });
                        write_adversary_json(&mut restore_writer, &response).await;
                    }
                    ArchiveAdversaryCommand::AssertNoFurtherRequests => {
                        if let Ok(Ok((unexpected, _))) = tokio::time::timeout(
                            std::time::Duration::from_millis(150),
                            listener.accept(),
                        )
                        .await
                        {
                            let (request, _) = read_adversary_request(unexpected).await;
                            let method = request["method"]
                                .as_str()
                                .expect("unexpected method")
                                .to_string();
                            requests.push(method.clone());
                            panic!("unexpected post-archive request {method}");
                        }
                        break;
                    }
                }
            }
            requests
        });
        (command_tx, event_rx, server)
    }

    async fn recv_notification(
        app: &mut App,
    ) -> crate::notification_stream::NotificationStreamEvent {
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.notification_stream
                .as_mut()
                .expect("notification stream")
                .recv(),
        )
        .await
        .expect("notification deadline")
        .expect("notification event")
    }

    #[tokio::test]
    async fn docreg_config_pending_and_error_preserve_retry_source_and_detail_navigation() {
        let (mut pending, pending_id) = terminal_docreg_detail_app();
        dispatch(&mut pending, LcAction::ExecuteDocRegBlocks).await;
        assert_retryable_source_and_detail(
            &pending,
            pending_id,
            "Daemon configuration is still loading; draft preserved",
        );

        let (mut errored, errored_id) = terminal_docreg_detail_app();
        errored.poll.connected = true;
        errored.mark_daemon_config_error("detail config unavailable".to_string());
        dispatch(&mut errored, LcAction::ExecuteDocRegBlocks).await;
        assert_retryable_source_and_detail(
            &errored,
            errored_id,
            "Daemon configuration unavailable: detail config unavailable; draft preserved",
        );
    }

    #[tokio::test]
    async fn docreg_provider_unavailable_preserves_retry_source_and_detail_navigation() {
        let (mut app, session_id) = terminal_docreg_detail_app();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.provider_availability
            .insert(rsi_common::types::SessionProvider::Claude, false);

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;

        assert_retryable_source_and_detail(
            &app,
            session_id,
            "Claude provider unavailable (check daemon health)",
        );
    }

    #[test]
    fn docreg_partial_launch_progress_retries_only_unaccepted_commands() {
        let (mut app, session_id) = terminal_docreg_detail_app();
        let commands = vec![
            crate::app::DocregCommandOccurrence {
                command: "first".to_string(),
                ordinal: 0,
            },
            crate::app::DocregCommandOccurrence {
                command: "second".to_string(),
                ordinal: 0,
            },
        ];
        assert_eq!(
            app.remaining_docreg_launches(session_id, &commands).len(),
            2
        );
        app.record_docreg_launch_accepted(session_id, commands[0].clone());
        assert_eq!(
            app.remaining_docreg_launches(session_id, &commands),
            vec![commands[1].clone()]
        );
        assert!(!app.docreg_launches_complete(session_id, &commands));
        app.record_docreg_launch_accepted(session_id, commands[1].clone());
        assert!(app.docreg_launches_complete(session_id, &commands));
    }

    #[tokio::test]
    async fn docreg_protocol_partial_rejection_retries_without_duplicate_then_archives() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = tempfile::tempdir().expect("temporary docreg socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("docreg listener");
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_requests = Arc::clone(&requests);
        let first_id = uuid::Uuid::new_v4();
        let second_id = uuid::Uuid::new_v4();
        let server = tokio::spawn(async move {
            for step in 0..5 {
                let (stream, _) = listener.accept().await.expect("docreg client");
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let line = lines
                    .next_line()
                    .await
                    .expect("docreg request read")
                    .expect("docreg request line");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("docreg request JSON");
                let method = request["method"].as_str().expect("docreg method");
                let record = if method == "LaunchSession" {
                    format!("launch:{}", request["params"]["query"].as_str().unwrap())
                } else {
                    method.to_string()
                };
                server_requests.lock().expect("request log").push(record);
                let response = match step {
                    0 => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "result": {"session_id": first_id},
                    }),
                    1 => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "error": {"code": -32073, "message": "second replacement rejected exactly"},
                    }),
                    2 => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "result": {"session_id": second_id},
                    }),
                    3 => {
                        assert_eq!(method, "ArchiveSession");
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request["id"].clone(),
                            "error": {"code": -32075, "message": "archive rejected exactly"},
                        })
                    }
                    4 => {
                        assert_eq!(method, "ArchiveSession");
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request["id"].clone(),
                            "result": rsi_common::archive_cleanup::ArchiveSessionResultV1::no_cleanup_required(),
                        })
                    }
                    _ => unreachable!(),
                };
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("docreg response write");
            }
        });

        let (mut app, session_id) = terminal_docreg_detail_app();
        app.client = crate::client::DaemonClient::new(socket_path);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.sessions
            .get_mut(&session_id)
            .expect("docreg source")
            .docregblock_contents = vec![
            "/plan @thoughts/shared/plans/first.md".to_string(),
            "/implement @thoughts/shared/plans/second.md".to_string(),
        ];

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        let partial = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("partial docreg deadline")
        .expect("partial docreg result");
        assert!(app.apply_docreg_operation_result(partial));
        assert_retryable_source_and_detail(&app, session_id, "second replacement rejected exactly");
        assert_eq!(
            app.remaining_docreg_launches(
                session_id,
                &[
                    crate::app::DocregCommandOccurrence {
                        command: "/plan @thoughts/shared/plans/first.md".to_string(),
                        ordinal: 0,
                    },
                    crate::app::DocregCommandOccurrence {
                        command: "/implement @thoughts/shared/plans/second.md".to_string(),
                        ordinal: 0,
                    },
                ],
            ),
            vec![crate::app::DocregCommandOccurrence {
                command: "/implement @thoughts/shared/plans/second.md".to_string(),
                ordinal: 0,
            }]
        );

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("retry docreg deadline")
        .expect("retry docreg result");
        assert!(app.apply_docreg_operation_result(retry));
        assert!(
            app.sessions.contains_key(&session_id),
            "archive awaits its own semantic response"
        );
        let archive = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("archive docreg deadline")
        .expect("archive docreg result");
        assert!(app.apply_docreg_operation_result(archive));
        assert_retryable_source_and_detail(&app, session_id, "archive rejected exactly");

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        let archive_retry = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("archive retry deadline")
        .expect("archive retry result");
        assert!(app.apply_docreg_operation_result(archive_retry));
        assert!(matches!(app.focused_pane(), Some(Pane::SessionList { .. })));
        server.await.expect("docreg server");
        assert_eq!(
            requests.lock().expect("request log").as_slice(),
            [
                "launch:/plan @thoughts/shared/plans/first.md",
                "launch:/implement @thoughts/shared/plans/second.md",
                "launch:/implement @thoughts/shared/plans/second.md",
                "ArchiveSession",
                "ArchiveSession",
            ]
        );
    }

    #[tokio::test]
    async fn docreg_protocol_appended_continuation_during_launch_is_processed_without_archive() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::sync::{Notify, mpsc};

        let temp_dir = tempfile::tempdir().expect("temporary docreg socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("docreg listener");
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_requests = Arc::clone(&requests);
        let release_launch = Arc::new(Notify::new());
        let server_release = Arc::clone(&release_launch);
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let accepted_id = uuid::Uuid::new_v4();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&server_requests);
                let release = Arc::clone(&server_release);
                let entered_tx = entered_tx.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let request: serde_json::Value =
                            serde_json::from_str(&line).expect("docreg request JSON");
                        let method = request["method"].as_str().expect("docreg method");
                        let record = if method == "LaunchSession" {
                            format!("launch:{}", request["params"]["query"].as_str().unwrap())
                        } else {
                            method.to_string()
                        };
                        requests.lock().expect("request log").push(record.clone());
                        let _ = entered_tx.send(record);
                        if method == "LaunchSession" {
                            release.notified().await;
                        }
                        let response = if method == "LaunchSession" {
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": request["id"].clone(),
                                "result": {"session_id": accepted_id},
                            })
                        } else {
                            assert_eq!(method, "ContinueSession");
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": request["id"].clone(),
                                "result": null,
                            })
                        };
                        writer
                            .write_all(format!("{response}\n").as_bytes())
                            .await
                            .expect("docreg response write");
                    }
                });
            }
        });

        let (mut app, session_id) = terminal_docreg_detail_app();
        app.client = crate::client::DaemonClient::new(socket_path);
        app.client
            .connect()
            .await
            .expect("connect persistent client");
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.sessions
            .get_mut(&session_id)
            .expect("docreg source")
            .docregblock_contents = vec!["/plan @first.md".to_string()];

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), entered_rx.recv())
                .await
                .expect("launch entry deadline")
                .expect("launch entry"),
            "launch:/plan @first.md"
        );
        let state = app.sessions.get_mut(&session_id).expect("docreg source");
        state.docregblock_contents.push("/ci_commit".to_string());
        state.events_generation += 1;
        release_launch.notify_one();

        let launched = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("launch result deadline")
        .expect("launch result");
        assert!(app.apply_docreg_operation_result(launched));
        assert_retryable_source_and_detail(&app, session_id, "accepted replacements retained");

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), entered_rx.recv())
                .await
                .expect("continue entry deadline")
                .expect("continue entry"),
            "ContinueSession"
        );
        assert_eq!(
            app.sessions[&session_id].docregblock_contents,
            vec!["/plan @first.md".to_string(), "/ci_commit".to_string()]
        );
        assert!(matches!(
            app.focused_pane(),
            Some(Pane::SessionDetail { session_id: focused }) if *focused == session_id
        ));
        assert_eq!(
            requests.lock().expect("request log").as_slice(),
            ["launch:/plan @first.md", "ContinueSession"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn docreg_protocol_reorder_append_retains_accepted_command_occurrences() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = tempfile::tempdir().expect("temporary docreg socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("docreg listener");
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_requests = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            for step in 0..6 {
                let (stream, _) = listener.accept().await.expect("docreg client");
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let line = lines
                    .next_line()
                    .await
                    .expect("docreg request read")
                    .expect("docreg request line");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("docreg request JSON");
                let method = request["method"].as_str().expect("docreg method");
                let record = if method == "LaunchSession" {
                    format!("launch:{}", request["params"]["query"].as_str().unwrap())
                } else {
                    method.to_string()
                };
                server_requests.lock().expect("request log").push(record);
                let response = match step {
                    1 => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "error": {"code": -32073, "message": "second duplicate rejected"},
                    }),
                    5 => {
                        assert_eq!(method, "ArchiveSession");
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request["id"].clone(),
                            "error": {"code": -32075, "message": "archive stays retryable"},
                        })
                    }
                    _ => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "result": {"session_id": uuid::Uuid::new_v4()},
                    }),
                };
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("docreg response write");
            }
        });

        let (mut app, session_id) = terminal_docreg_detail_app();
        app.client = crate::client::DaemonClient::new(socket_path);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        let duplicate = "/plan @same.md".to_string();
        app.sessions
            .get_mut(&session_id)
            .expect("docreg source")
            .docregblock_contents = vec![
            duplicate.clone(),
            duplicate.clone(),
            "/implement @b.md".into(),
        ];

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        let partial = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("partial result deadline")
        .expect("partial result");
        assert!(app.apply_docreg_operation_result(partial));
        let state = app.sessions.get_mut(&session_id).expect("docreg source");
        state.docregblock_contents = vec![
            "/implement @b.md".into(),
            duplicate.clone(),
            duplicate.clone(),
            "/research @c.md".into(),
        ];
        state.events_generation += 1;

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("retry result deadline")
        .expect("retry result");
        assert!(app.apply_docreg_operation_result(retry));
        let archive = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("archive result deadline")
        .expect("archive result");
        assert!(app.apply_docreg_operation_result(archive));
        assert_retryable_source_and_detail(&app, session_id, "archive stays retryable");
        server.await.expect("docreg server");
        assert_eq!(
            requests.lock().expect("request log").as_slice(),
            [
                "launch:/plan @same.md",
                "launch:/plan @same.md",
                "launch:/implement @b.md",
                "launch:/plan @same.md",
                "launch:/research @c.md",
                "ArchiveSession",
            ]
        );
    }

    #[tokio::test]
    async fn docreg_protocol_archive_race_requires_response_backed_restore_and_retry() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::sync::Notify;

        let temp_dir = tempfile::tempdir().expect("temporary docreg socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("docreg listener");
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_requests = Arc::clone(&requests);
        let archive_entered = Arc::new(Notify::new());
        let server_archive_entered = Arc::clone(&archive_entered);
        let release_archive = Arc::new(Notify::new());
        let server_release_archive = Arc::clone(&release_archive);
        let restore_attempt = Arc::new(AtomicUsize::new(0));
        let server_restore_attempt = Arc::clone(&restore_attempt);
        let (mut app, session_id) = terminal_docreg_detail_app();
        let restored_session = app.sessions[&session_id].session.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&server_requests);
                let archive_entered = Arc::clone(&server_archive_entered);
                let release_archive = Arc::clone(&server_release_archive);
                let restore_attempt = Arc::clone(&server_restore_attempt);
                let restored_session = restored_session.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let request: serde_json::Value =
                            serde_json::from_str(&line).expect("docreg request JSON");
                        let method = request["method"].as_str().expect("docreg method");
                        let record = if method == "LaunchSession" {
                            format!("launch:{}", request["params"]["query"].as_str().unwrap())
                        } else {
                            method.to_string()
                        };
                        requests.lock().expect("request log").push(record);
                        let response = match method {
                            "LaunchSession" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": request["id"].clone(),
                                "result": {"session_id": uuid::Uuid::new_v4()},
                            }),
                            "ArchiveSession" => {
                                archive_entered.notify_one();
                                release_archive.notified().await;
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": request["id"].clone(),
                                    "result": rsi_common::archive_cleanup::ArchiveSessionResultV1::no_cleanup_required(),
                                })
                            }
                            "UnarchiveSession" => {
                                if restore_attempt.fetch_add(1, Ordering::SeqCst) == 0 {
                                    serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": request["id"].clone(),
                                        "error": {"code": -32076, "message": "restore rejected exactly"},
                                    })
                                } else {
                                    serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": request["id"].clone(),
                                        "result": restored_session,
                                    })
                                }
                            }
                            "ContinueSession" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": request["id"].clone(),
                                "result": null,
                            }),
                            _ => panic!("unexpected docreg method {method}"),
                        };
                        writer
                            .write_all(format!("{response}\n").as_bytes())
                            .await
                            .expect("docreg response write");
                    }
                });
            }
        });

        app.client = crate::client::DaemonClient::new(socket_path);
        app.client
            .connect()
            .await
            .expect("connect persistent client");
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.sessions
            .get_mut(&session_id)
            .expect("docreg source")
            .docregblock_contents = vec!["/plan @race.md".to_string()];

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        let launch = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("launch result deadline")
        .expect("launch result");
        assert!(app.apply_docreg_operation_result(launch));
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            archive_entered.notified(),
        )
        .await
        .expect("archive entry deadline");
        let state = app.sessions.get_mut(&session_id).expect("docreg source");
        state.docregblock_contents.push("/ci_commit".to_string());
        state.events_generation += 1;
        release_archive.notify_one();

        let archive = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("archive result deadline")
        .expect("archive result");
        assert!(app.apply_docreg_operation_result(archive));
        let restore_error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("restore error deadline")
        .expect("restore error");
        assert!(app.apply_docreg_operation_result(restore_error));
        assert!(app.docreg_restore_required(session_id));
        assert_retryable_source_and_detail(&app, session_id, "restore rejected exactly");

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        let restored = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("restore retry deadline")
        .expect("restore retry");
        assert!(app.apply_docreg_operation_result(restored));
        assert!(!app.docreg_restore_required(session_id));
        assert_eq!(
            app.sessions[&session_id].docregblock_contents,
            vec!["/plan @race.md".to_string(), "/ci_commit".to_string()]
        );
        assert!(matches!(
            app.focused_pane(),
            Some(Pane::SessionDetail { session_id: focused }) if *focused == session_id
        ));

        dispatch(&mut app, LcAction::ExecuteDocRegBlocks).await;
        assert!(app.sessions.contains_key(&session_id));
        assert_eq!(
            requests.lock().expect("request log").as_slice(),
            [
                "launch:/plan @race.md",
                "ArchiveSession",
                "UnarchiveSession",
                "UnarchiveSession",
                "ContinueSession",
            ]
        );
        server.abort();
    }

    #[tokio::test]
    async fn docreg_protocol_accepts_archive_push_and_response_in_both_orders_without_unarchive() {
        for event_first in [true, false] {
            let temp_dir = tempfile::tempdir().expect("temporary archive-order socket directory");
            let socket_path = temp_dir.path().join("daemon.sock");
            let listener =
                tokio::net::UnixListener::bind(&socket_path).expect("archive-order listener");
            let (mut app, session_id) = terminal_docreg_detail_app();
            app.client = crate::client::DaemonClient::new(socket_path.clone());
            app.poll.connected = true;
            app.poll.authoritative_config_ready = true;
            app.sessions
                .get_mut(&session_id)
                .expect("docreg source")
                .docregblock_contents = vec!["/merge_ready".to_string()];

            let (commands, mut events, server) = spawn_archive_adversary(listener, session_id);
            app.notification_stream = Some(crate::notification_stream::NotificationStream::spawn(
                &socket_path,
            ));
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                    .await
                    .expect("subscription deadline"),
                Some(ArchiveAdversaryEvent::Subscribed)
            );

            assert!(app.request_docreg_replacements(session_id, false));
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                    .await
                    .expect("archive request deadline"),
                Some(ArchiveAdversaryEvent::ArchiveRequested)
            );

            if event_first {
                commands
                    .send(ArchiveAdversaryCommand::PushArchive)
                    .expect("archive push command");
                let push = recv_notification(&mut app).await;
                assert!(app.apply_notification_stream_event(push));
                assert!(!app.sessions.contains_key(&session_id));
                commands
                    .send(ArchiveAdversaryCommand::RespondArchive)
                    .expect("archive response command");
                let response = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    app.docreg_operation_rx.recv(),
                )
                .await
                .expect("archive response deadline")
                .expect("archive response");
                assert!(app.apply_docreg_operation_result(response));
            } else {
                commands
                    .send(ArchiveAdversaryCommand::RespondArchive)
                    .expect("archive response command");
                let response = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    app.docreg_operation_rx.recv(),
                )
                .await
                .expect("archive response deadline")
                .expect("archive response");
                assert!(app.apply_docreg_operation_result(response));
                commands
                    .send(ArchiveAdversaryCommand::PushArchive)
                    .expect("archive push command");
                let push = recv_notification(&mut app).await;
                assert!(app.apply_notification_stream_event(push));
            }

            assert!(!app.sessions.contains_key(&session_id));
            assert!(app.docreg_operation_pending.is_none());
            assert!(app.docreg_operation_handle.is_none());
            commands
                .send(ArchiveAdversaryCommand::AssertNoFurtherRequests)
                .expect("no-more-requests command");
            assert_eq!(
                server.await.expect("archive-order adversary"),
                vec!["Subscribe".to_string(), "ArchiveSession".to_string()],
                "ordinary success must accept exactly one archive and issue no unarchive"
            );
        }
    }

    #[tokio::test]
    async fn docreg_protocol_s1_push_then_archive_push_preserves_exact_s1_for_compensation() {
        let temp_dir = tempfile::tempdir().expect("temporary S1 archive socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("S1 archive listener");
        let (mut app, session_id) = terminal_docreg_detail_app();
        app.client = crate::client::DaemonClient::new(socket_path.clone());
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        let source = app.sessions.get_mut(&session_id).expect("docreg source");
        source.docregblock_contents = vec!["/merge_ready".to_string()];
        let initial_generation = source.events_generation;

        let (commands, mut events, server) = spawn_archive_adversary(listener, session_id);
        app.notification_stream = Some(crate::notification_stream::NotificationStream::spawn(
            &socket_path,
        ));
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                .await
                .expect("subscription deadline"),
            Some(ArchiveAdversaryEvent::Subscribed)
        );
        assert!(app.request_docreg_replacements(session_id, false));
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                .await
                .expect("archive request deadline"),
            Some(ArchiveAdversaryEvent::ArchiveRequested)
        );

        let s1_content =
            "authoritative S1 <docregblock>/plan @thoughts/shared/plans/s1.md</docregblock>";
        let s1_event = rsi_common::types::ConversationEvent {
            id: 91,
            session_id,
            sequence: 1,
            event_type: rsi_common::types::EventType::Message,
            role: Some(rsi_common::types::Role::Assistant),
            content: s1_content.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        commands
            .send(ArchiveAdversaryCommand::PushConversation(s1_event.clone()))
            .expect("S1 push command");
        let conversation = recv_notification(&mut app).await;
        assert!(app.apply_notification_stream_event(conversation));
        let s1 = &app.sessions[&session_id];
        assert_eq!(
            s1.docregblock_contents,
            vec!["/plan @thoughts/shared/plans/s1.md".to_string()]
        );
        assert_eq!(s1.events.len(), 1);
        assert_eq!(s1.events[0].id, s1_event.id);
        assert_eq!(s1.events[0].sequence, s1_event.sequence);
        assert_eq!(s1.events[0].content, s1_content);
        assert_eq!(s1.events_generation, initial_generation + 1);
        assert_eq!(s1.last_sequence, Some(1));

        commands
            .send(ArchiveAdversaryCommand::PushArchive)
            .expect("archive push command");
        let archive_push = recv_notification(&mut app).await;
        assert!(app.apply_notification_stream_event(archive_push));
        assert!(!app.sessions.contains_key(&session_id));
        commands
            .send(ArchiveAdversaryCommand::RespondArchive)
            .expect("archive response command");
        let archive_response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("archive response deadline")
        .expect("archive response");
        assert!(app.apply_docreg_operation_result(archive_response));

        commands
            .send(ArchiveAdversaryCommand::RespondRestoreError)
            .expect("restore response command");
        let restore_response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.docreg_operation_rx.recv(),
        )
        .await
        .expect("restore response deadline")
        .expect("restore response");
        assert!(app.apply_docreg_operation_result(restore_response));
        assert!(app.docreg_restore_required(session_id));
        assert_retryable_source_and_detail(&app, session_id, "S1 restore rejected exactly");
        let preserved = &app.sessions[&session_id];
        assert_eq!(
            preserved.docregblock_contents,
            vec!["/plan @thoughts/shared/plans/s1.md".to_string()]
        );
        assert_eq!(preserved.events.len(), 1);
        assert_eq!(preserved.events[0].id, s1_event.id);
        assert_eq!(preserved.events[0].sequence, s1_event.sequence);
        assert_eq!(preserved.events[0].content, s1_content);
        assert_eq!(preserved.events_generation, initial_generation + 1);
        assert_eq!(preserved.last_sequence, Some(1));

        commands
            .send(ArchiveAdversaryCommand::AssertNoFurtherRequests)
            .expect("no-more-requests command");
        assert_eq!(
            server.await.expect("S1 archive adversary"),
            vec![
                "Subscribe".to_string(),
                "ArchiveSession".to_string(),
                "UnarchiveSession".to_string(),
            ],
            "S1 compensation must follow exactly one accepted archive"
        );
    }

    /// gR rejects a focused leaf session ("Not an Epic" toast).
    #[test]
    fn test_gr_action_rejects_non_epic_focus() {
        let app = crate::app::app_test_helpers::with_focused_kind(
            rsi_common::types::SessionKind::Task,
            None,
        );
        assert_eq!(
            validate_run_epic_topology(&app),
            RunEpicTopologyValidation::NotEpic
        );
    }

    /// gR rejects Group focus even when the Group has a topology binding.
    /// Topology runs attach at the Epic level, never at Group level.
    #[test]
    fn test_gr_action_rejects_group_even_with_topology_binding() {
        let app = crate::app::app_test_helpers::with_focused_kind(
            rsi_common::types::SessionKind::Group,
            Some(uuid::Uuid::new_v4()),
        );
        assert_eq!(
            validate_run_epic_topology(&app),
            RunEpicTopologyValidation::NotEpic
        );
    }

    /// gR rejects an Epic that has no `workflow_id` binding.
    #[test]
    fn test_gr_action_rejects_epic_without_topology_binding() {
        let app = crate::app::app_test_helpers::with_focused_kind(
            rsi_common::types::SessionKind::Epic,
            None,
        );
        assert_eq!(
            validate_run_epic_topology(&app),
            RunEpicTopologyValidation::NoTopologyBinding
        );
    }

    /// gR happy path: Epic with `workflow_id` returns Ready{...}.
    #[test]
    fn test_gr_action_ready_on_epic_with_binding() {
        let topology_id = uuid::Uuid::new_v4();
        let app = crate::app::app_test_helpers::with_focused_kind(
            rsi_common::types::SessionKind::Epic,
            Some(topology_id),
        );
        let focused_epic_id = app
            .selected_session_id()
            .expect("fixture should select the focused Epic");
        match validate_run_epic_topology(&app) {
            RunEpicTopologyValidation::Ready {
                topology_id: t,
                epic_id,
                project_id,
            } => {
                assert_eq!(t, topology_id);
                assert_eq!(
                    epic_id, focused_epic_id,
                    "ExecuteTopology parent_id must be the focused Epic id"
                );
                assert_eq!(project_id, None);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    // ── P2.2 §6: g{G,E,S,T,B} dispatch via `open_create_entity_form_with_kind` ──

    /// `gG` from root → opens `CreateEntityForm` with `kind=Group`, `parent_id=None`.
    #[tokio::test]
    async fn test_gg_action_opens_form_with_group_kind_from_root() {
        let mut app = crate::app::app_test_helpers::with_focused_kind(
            rsi_common::types::SessionKind::Standard,
            None,
        );
        // descent_path is empty — root context.
        assert!(app.tabs[app.active_tab].descent_path.is_empty());

        open_create_entity_form_with_kind(&mut app, rsi_common::types::SessionKind::Group).await;

        match &app.overlay {
            crate::types::OverlayState::CreateEntityForm {
                kind, parent_id, ..
            } => {
                assert_eq!(*kind, rsi_common::types::SessionKind::Group);
                assert_eq!(*parent_id, None);
            }
            _ => panic!("expected CreateEntityForm overlay, got a different variant"),
        }
        assert!(app.notifications.is_empty(), "no rejection notify expected");
    }

    /// `gE` from inside a Group descent → opens `CreateEntityForm` with
    /// `kind=Epic`, `parent_id=Some(group_uuid)`.
    #[tokio::test]
    async fn test_ge_action_opens_form_with_epic_kind_under_group() {
        let mut app = crate::app::app_test_helpers::with_focused_kind(
            rsi_common::types::SessionKind::Standard,
            None,
        );
        let group_id = uuid::Uuid::new_v4();
        let group_session =
            baseline_container_session(group_id, rsi_common::types::SessionKind::Group);
        app.sessions
            .insert(group_id, crate::types::SessionState::new(group_session));
        app.tabs[app.active_tab].descent_path.push(group_id);

        open_create_entity_form_with_kind(&mut app, rsi_common::types::SessionKind::Epic).await;

        match &app.overlay {
            crate::types::OverlayState::CreateEntityForm {
                kind, parent_id, ..
            } => {
                assert_eq!(*kind, rsi_common::types::SessionKind::Epic);
                assert_eq!(*parent_id, Some(group_id));
            }
            _ => panic!("expected CreateEntityForm overlay, got a different variant"),
        }
        assert!(app.notifications.is_empty(), "no rejection notify expected");
    }

    /// `gE` from root → rejected via `app.notify()`; overlay stays None.
    #[tokio::test]
    async fn test_ge_action_rejects_from_root_with_notify() {
        let mut app = crate::app::app_test_helpers::with_focused_kind(
            rsi_common::types::SessionKind::Standard,
            None,
        );
        assert!(matches!(app.overlay, crate::types::OverlayState::None));

        open_create_entity_form_with_kind(&mut app, rsi_common::types::SessionKind::Epic).await;

        assert!(
            matches!(app.overlay, crate::types::OverlayState::None),
            "overlay must not open on rejected chord"
        );
        assert_eq!(app.notifications.len(), 1);
        let Some(last) = app.notifications.back() else {
            panic!("expected at least one notification");
        };
        let msg = &last.message;
        assert!(
            msg.contains("Epic") && msg.contains("not allowed"),
            "expected rejection toast mentioning Epic and 'not allowed'; got {msg:?}"
        );
    }

    /// `gS` / `gT` / `gB` from inside an Epic descent → opens `CreateEntityForm`
    /// with the matching Kind and `parent_id=Some(epic_uuid)`. One parameterized
    /// loop over the three leaf kinds.
    #[tokio::test]
    async fn test_g_leaf_actions_open_form_under_epic() {
        for kind in [
            rsi_common::types::SessionKind::Story,
            rsi_common::types::SessionKind::Task,
            rsi_common::types::SessionKind::Bug,
        ] {
            let mut app = crate::app::app_test_helpers::with_focused_kind(
                rsi_common::types::SessionKind::Standard,
                None,
            );
            let epic_id = uuid::Uuid::new_v4();
            let epic_session =
                baseline_container_session(epic_id, rsi_common::types::SessionKind::Epic);
            app.sessions
                .insert(epic_id, crate::types::SessionState::new(epic_session));
            app.tabs[app.active_tab].descent_path.push(epic_id);

            open_create_entity_form_with_kind(&mut app, kind).await;

            match &app.overlay {
                crate::types::OverlayState::CreateEntityForm {
                    kind: ovk,
                    parent_id,
                    ..
                } => {
                    assert_eq!(*ovk, kind, "kind mismatch for {kind:?} chord");
                    assert_eq!(
                        *parent_id,
                        Some(epic_id),
                        "parent_id mismatch for {kind:?} chord"
                    );
                }
                _ => panic!(
                    "expected CreateEntityForm overlay for {kind:?}, got a different variant"
                ),
            }
            assert!(
                app.notifications.is_empty(),
                "no rejection notify expected for {kind:?}"
            );
        }
    }

    /// All creation shortcuts use one unified `CreateEntityForm` flow. The old
    /// typed prompt path must not receive g{G,E,S,T,B} traffic.
    #[tokio::test]
    async fn test_create_chords_all_route_to_single_create_entity_form_flow() {
        let scenarios = [
            (rsi_common::types::SessionKind::Group, None),
            (
                rsi_common::types::SessionKind::Epic,
                Some(rsi_common::types::SessionKind::Group),
            ),
            (
                rsi_common::types::SessionKind::Story,
                Some(rsi_common::types::SessionKind::Epic),
            ),
            (
                rsi_common::types::SessionKind::Task,
                Some(rsi_common::types::SessionKind::Epic),
            ),
            (
                rsi_common::types::SessionKind::Bug,
                Some(rsi_common::types::SessionKind::Epic),
            ),
        ];

        for (kind, parent_kind) in scenarios {
            let mut app = crate::app::app_test_helpers::with_focused_kind(
                rsi_common::types::SessionKind::Standard,
                None,
            );
            let expected_parent_id = parent_kind.map(|pk| {
                let parent_id = uuid::Uuid::new_v4();
                let parent_session = baseline_container_session(parent_id, pk);
                app.sessions
                    .insert(parent_id, crate::types::SessionState::new(parent_session));
                app.tabs[app.active_tab].descent_path.push(parent_id);
                parent_id
            });

            open_create_entity_form_with_kind(&mut app, kind).await;

            match &app.overlay {
                crate::types::OverlayState::CreateEntityForm {
                    kind: opened_kind,
                    parent_id,
                    ..
                } => {
                    assert_eq!(*opened_kind, kind, "kind mismatch for {kind:?}");
                    assert_eq!(
                        *parent_id, expected_parent_id,
                        "parent mismatch for {kind:?}"
                    );
                }
                other => panic!(
                    "expected CreateEntityForm for {kind:?}, got {:?}",
                    std::mem::discriminant(other)
                ),
            }
            assert!(
                app.input_overlays.is_empty(),
                "creation shortcuts must not open typed prompt input overlays"
            );
            assert!(
                app.notifications.is_empty(),
                "legal creation scenario unexpectedly notified for {kind:?}"
            );
        }
    }

    /// `gS` from root → rejected (`legal_children` rejects Story under root).
    #[tokio::test]
    async fn test_gs_action_rejects_from_root_with_notify() {
        let mut app = crate::app::app_test_helpers::with_focused_kind(
            rsi_common::types::SessionKind::Standard,
            None,
        );
        assert!(matches!(app.overlay, crate::types::OverlayState::None));

        open_create_entity_form_with_kind(&mut app, rsi_common::types::SessionKind::Story).await;

        assert!(
            matches!(app.overlay, crate::types::OverlayState::None),
            "overlay must not open on rejected chord"
        );
        assert_eq!(app.notifications.len(), 1);
        let Some(last) = app.notifications.back() else {
            panic!("expected at least one notification");
        };
        let msg = &last.message;
        assert!(
            msg.contains("Story") && msg.contains("not allowed"),
            "expected rejection toast mentioning Story and 'not allowed'; got {msg:?}"
        );
    }

    /// Local helper: a baseline `Session` row for an arbitrary container id.
    /// Caller specifies `session_kind` (Group / Epic). Kept inside `mod tests`
    /// so it doesn't expand the public `app_test_helpers` surface (P2.2 only —
    /// extract to `app_test_helpers` if a third caller emerges).
    fn baseline_container_session(
        id: uuid::Uuid,
        kind: rsi_common::types::SessionKind,
    ) -> rsi_common::types::Session {
        use rsi_common::types::{ContextUsageConfidence, Session, SessionProvider, SessionStatus};
        Session {
            context_fill_pct: None,
            id,
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "container".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Completed,
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
}
