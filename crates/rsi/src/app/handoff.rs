//! Response-bearing handoff and document-registration launch operations.

use super::{App, AutoResumeHandoffResult, DocregCommandOccurrence};
use rsi_common::types::{Session, SessionStatus};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocregSourceFence {
    normalized_contents: Vec<String>,
    events_generation: u64,
}

pub(crate) struct PendingDocregOperation {
    pub generation: u64,
    pub session_id: Uuid,
    pub launch_commands: Vec<String>,
    pub was_detail: bool,
    pub should_archive: bool,
}

pub(crate) struct DocregOperationContext {
    source_fence: DocregSourceFence,
    latest_source_fence: DocregSourceFence,
    launch_commands: Vec<DocregCommandOccurrence>,
    source_snapshot: crate::types::SessionState,
    source_order_index: usize,
    archive_dispatch_fence: Option<DocregSourceFence>,
    archive_notification_observed: bool,
}

pub(crate) enum DocregOperationOutcome {
    Launches {
        accepted_commands: Vec<DocregCommandOccurrence>,
        error: Option<String>,
    },
    Archive(Result<(), String>),
    Restore(Result<Session, String>),
}

pub(crate) struct DocregOperationResult {
    pub generation: u64,
    pub session_id: Uuid,
    pub launch_commands: Vec<String>,
    pub outcome: DocregOperationOutcome,
}

fn normalized_docreg_contents(contents: &[String]) -> Vec<String> {
    contents
        .iter()
        .map(|content| content.trim().to_string())
        .collect()
}

fn is_replacement_command(content: &str) -> bool {
    let command = content.split_whitespace().next().unwrap_or("");
    let clean = command.strip_prefix('/').unwrap_or(command);
    command.is_empty()
        || !command.starts_with('/')
        || matches!(
            clean,
            "plan" | "implement" | "research" | "resume_handoff" | "iterate_plan"
        )
}

pub(crate) fn replacement_launch_commands(contents: &[String]) -> Vec<DocregCommandOccurrence> {
    let mut ordinals = HashMap::<String, usize>::new();
    normalized_docreg_contents(contents)
        .into_iter()
        .filter(|content| is_replacement_command(content))
        .map(|command| {
            let ordinal = ordinals.entry(command.clone()).or_default();
            let occurrence = DocregCommandOccurrence {
                command,
                ordinal: *ordinal,
            };
            *ordinal += 1;
            occurrence
        })
        .collect()
}

fn source_should_archive(fence: &DocregSourceFence, status: SessionStatus) -> bool {
    let has_continuation = fence.normalized_contents.iter().any(|content| {
        let command = content.split_whitespace().next().unwrap_or("");
        command.starts_with('/')
            && !is_replacement_command(content)
            && command.strip_prefix('/').unwrap_or(command) != "merge_ready"
    });
    !has_continuation && !matches!(status, SessionStatus::Running | SessionStatus::Starting)
}

impl App {
    fn docreg_source_fence_from_state(state: &crate::types::SessionState) -> DocregSourceFence {
        DocregSourceFence {
            normalized_contents: normalized_docreg_contents(&state.docregblock_contents),
            events_generation: state.events_generation,
        }
    }

    fn docreg_source_fence(&self, session_id: Uuid) -> Option<DocregSourceFence> {
        self.sessions
            .get(&session_id)
            .map(Self::docreg_source_fence_from_state)
    }

    pub(crate) fn capture_docreg_source_push(&mut self, session_id: Uuid) {
        if self
            .docreg_operation_pending
            .as_ref()
            .is_none_or(|pending| pending.session_id != session_id)
        {
            return;
        }
        let Some(source_snapshot) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let latest_source_fence = Self::docreg_source_fence_from_state(&source_snapshot);
        let source_order_index = self
            .session_order
            .iter()
            .position(|id| *id == session_id)
            .unwrap_or(self.session_order.len());
        let Some(context) = self.docreg_operation_context.as_mut() else {
            return;
        };
        context.latest_source_fence = latest_source_fence;
        context.source_snapshot = source_snapshot;
        context.source_order_index = source_order_index;
    }

    pub(crate) fn record_docreg_archive_notification(&mut self, session_id: Uuid) {
        let owns_archive = self
            .docreg_operation_pending
            .as_ref()
            .is_some_and(|pending| pending.session_id == session_id)
            && self
                .docreg_operation_context
                .as_ref()
                .is_some_and(|context| context.archive_dispatch_fence.is_some());
        if !owns_archive {
            return;
        }
        self.capture_docreg_source_push(session_id);
        if let Some(context) = self.docreg_operation_context.as_mut() {
            context.archive_notification_observed = true;
        }
    }

    fn spawn_docreg_archive(&mut self) -> Result<(), String> {
        let pending = self
            .docreg_operation_pending
            .as_ref()
            .ok_or_else(|| "document registration owner missing".to_string())?;
        let context = self
            .docreg_operation_context
            .as_ref()
            .ok_or_else(|| "document registration context missing".to_string())?;
        if self.docreg_source_fence(pending.session_id) != Some(context.source_fence.clone()) {
            return Err("commands changed before archive dispatch".to_string());
        }
        self.docreg_operation_context
            .as_mut()
            .expect("document registration context checked above")
            .archive_dispatch_fence = Some(context.source_fence.clone());
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "async runtime unavailable".to_string())?;
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.docreg_operation_tx.clone();
        let generation = pending.generation;
        let session_id = pending.session_id;
        let launch_commands = pending.launch_commands.clone();
        self.docreg_operation_handle = Some(runtime.spawn(async move {
            let archived = async {
                let mut client = crate::client::DaemonClient::new(socket_path);
                client.connect().await.map_err(|error| error.to_string())?;
                client
                    .archive_session(session_id)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
            .await;
            let _ = tx
                .send(DocregOperationResult {
                    generation,
                    session_id,
                    launch_commands,
                    outcome: DocregOperationOutcome::Archive(archived),
                })
                .await;
        }));
        Ok(())
    }

    fn spawn_docreg_restore(&mut self) -> Result<(), String> {
        let pending = self
            .docreg_operation_pending
            .as_ref()
            .ok_or_else(|| "document registration owner missing".to_string())?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "async runtime unavailable".to_string())?;
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.docreg_operation_tx.clone();
        let generation = pending.generation;
        let session_id = pending.session_id;
        let launch_commands = pending.launch_commands.clone();
        self.docreg_operation_handle = Some(runtime.spawn(async move {
            let restored = async {
                let mut client = crate::client::DaemonClient::new(socket_path);
                client.connect().await.map_err(|error| error.to_string())?;
                client
                    .unarchive_session(session_id)
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            let _ = tx
                .send(DocregOperationResult {
                    generation,
                    session_id,
                    launch_commands,
                    outcome: DocregOperationOutcome::Restore(restored),
                })
                .await;
        }));
        Ok(())
    }

    fn ensure_local_docreg_source(&mut self, context: &DocregOperationContext, session_id: Uuid) {
        if self.sessions.contains_key(&session_id) {
            return;
        }
        self.sessions
            .insert(session_id, context.source_snapshot.clone());
        let index = context.source_order_index.min(self.session_order.len());
        self.session_order.insert(index, session_id);
        self.recalculate_filtered_order();
        self.reconcile_all_session_list_selections(false);
    }

    pub(crate) fn request_docreg_restore_if_needed(
        &mut self,
        session_id: Uuid,
        was_detail: bool,
    ) -> bool {
        if !self.docreg_restore_required(session_id) {
            return false;
        }
        if self.docreg_operation_pending.is_some() {
            self.notify_error("Document registration restore is awaiting daemon acceptance");
            return true;
        }
        let Some(source_snapshot) = self.sessions.get(&session_id).cloned() else {
            self.notify_error("Document registration restore source is unavailable locally");
            return true;
        };
        let source_fence = self
            .docreg_source_fence(session_id)
            .expect("source snapshot has a fence");
        self.docreg_operation_generation = self.docreg_operation_generation.saturating_add(1);
        let generation = self.docreg_operation_generation;
        self.docreg_operation_pending = Some(PendingDocregOperation {
            generation,
            session_id,
            launch_commands: source_fence.normalized_contents.clone(),
            was_detail,
            should_archive: false,
        });
        self.docreg_operation_context = Some(DocregOperationContext {
            latest_source_fence: source_fence.clone(),
            source_fence,
            launch_commands: replacement_launch_commands(&source_snapshot.docregblock_contents),
            source_snapshot,
            source_order_index: self
                .session_order
                .iter()
                .position(|id| *id == session_id)
                .unwrap_or(self.session_order.len()),
            archive_dispatch_fence: None,
            archive_notification_observed: false,
        });
        if let Err(error) = self.spawn_docreg_restore() {
            self.docreg_operation_pending = None;
            self.docreg_operation_context = None;
            self.notify_error(format!(
                "Document registration restore failed: {error}; source preserved and retry required"
            ));
        }
        true
    }

    pub(crate) fn request_docreg_replacements(
        &mut self,
        session_id: Uuid,
        was_detail: bool,
    ) -> bool {
        if self.docreg_operation_pending.is_some() {
            self.notify_error("Document registration is awaiting daemon acceptance");
            return false;
        }
        let Some(source_snapshot) = self.sessions.get(&session_id).cloned() else {
            self.notify_error("Document registration source is unavailable locally");
            return false;
        };
        let source_fence = self
            .docreg_source_fence(session_id)
            .expect("source snapshot has a fence");
        let launch_commands = replacement_launch_commands(&source_snapshot.docregblock_contents);
        let should_archive = source_should_archive(&source_fence, source_snapshot.session.status);
        let remaining = self.remaining_docreg_launches(session_id, &launch_commands);
        if !remaining.is_empty()
            && let Some(error) = self.launch_prerequisite_error(self.selected_provider)
        {
            self.notify_error(error);
            return false;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.notify_error("Document registration failed: async runtime unavailable");
            return false;
        };

        self.docreg_operation_generation = self.docreg_operation_generation.saturating_add(1);
        let generation = self.docreg_operation_generation;
        self.docreg_operation_pending = Some(PendingDocregOperation {
            generation,
            session_id,
            launch_commands: source_fence.normalized_contents.clone(),
            was_detail,
            should_archive,
        });
        self.docreg_operation_context = Some(DocregOperationContext {
            source_fence: source_fence.clone(),
            latest_source_fence: source_fence.clone(),
            launch_commands: launch_commands.clone(),
            source_order_index: self
                .session_order
                .iter()
                .position(|id| *id == session_id)
                .unwrap_or(self.session_order.len()),
            source_snapshot: source_snapshot.clone(),
            archive_dispatch_fence: None,
            archive_notification_observed: false,
        });

        if remaining.is_empty() {
            if should_archive && self.docreg_launches_complete(session_id, &launch_commands) {
                if let Err(error) = self.spawn_docreg_archive() {
                    self.docreg_operation_pending = None;
                    self.docreg_operation_context = None;
                    self.notify_error(format!(
                        "Document registration archive deferred: {error}; source preserved"
                    ));
                }
            } else {
                self.docreg_operation_pending = None;
                self.docreg_operation_context = None;
            }
            return true;
        }

        let socket_path = self.client.socket_path().to_path_buf();
        let requests: Vec<_> = remaining
            .into_iter()
            .map(|command| {
                (
                    command.clone(),
                    self.owned_quick_launch_request(
                        &command.command,
                        Some(source_snapshot.session.working_dir.clone()),
                        self.effective_topology(session_id),
                    ),
                )
            })
            .collect();
        let tx = self.docreg_operation_tx.clone();
        self.docreg_operation_handle = Some(runtime.spawn(async move {
            let mut accepted_commands = Vec::new();
            let mut error = None;
            for (command, request) in requests {
                match request.execute(socket_path.clone()).await {
                    Ok(_) => accepted_commands.push(command),
                    Err(failure) => {
                        error = Some(failure);
                        break;
                    }
                }
            }
            let _ = tx
                .send(DocregOperationResult {
                    generation,
                    session_id,
                    launch_commands: source_fence.normalized_contents,
                    outcome: DocregOperationOutcome::Launches {
                        accepted_commands,
                        error,
                    },
                })
                .await;
        }));
        true
    }

    pub(crate) fn apply_docreg_operation_result(&mut self, result: DocregOperationResult) -> bool {
        let Some(pending) = self.docreg_operation_pending.as_ref() else {
            return false;
        };
        if pending.generation != result.generation
            || pending.session_id != result.session_id
            || pending.launch_commands != result.launch_commands
        {
            return false;
        }
        let should_archive = pending.should_archive;
        let was_detail = pending.was_detail;
        self.docreg_operation_handle = None;

        match result.outcome {
            DocregOperationOutcome::Launches {
                accepted_commands,
                error,
            } => {
                for command in &accepted_commands {
                    self.record_docreg_launch_accepted(result.session_id, command.clone());
                }
                if let Some(error) = error {
                    self.docreg_operation_pending = None;
                    self.docreg_operation_context = None;
                    self.notify_error(format!(
                        "Document registration launch failed: {error}; source preserved"
                    ));
                    return true;
                }

                let current_fence = self.docreg_source_fence(result.session_id);
                let expected_fence = self
                    .docreg_operation_context
                    .as_ref()
                    .map(|context| context.source_fence.clone());
                if current_fence != expected_fence {
                    self.docreg_operation_pending = None;
                    self.docreg_operation_context = None;
                    self.notify_error(
                        "Document registration commands changed while launching; accepted replacements retained and source preserved",
                    );
                } else if should_archive
                    && self.docreg_launches_complete(
                        result.session_id,
                        &self
                            .docreg_operation_context
                            .as_ref()
                            .expect("generation-matched document registration")
                            .launch_commands,
                    )
                {
                    if let Err(error) = self.spawn_docreg_archive() {
                        self.docreg_operation_pending = None;
                        self.docreg_operation_context = None;
                        self.notify_error(format!(
                            "Document registration archive deferred: {error}; source preserved"
                        ));
                    }
                } else {
                    self.docreg_operation_pending = None;
                    self.docreg_operation_context = None;
                    if was_detail && !accepted_commands.is_empty() {
                        self.back_to_list();
                    }
                    if !accepted_commands.is_empty() {
                        self.notify_success(format!(
                            "Launched {} session{}",
                            accepted_commands.len(),
                            if accepted_commands.len() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        ));
                    }
                }
                true
            }
            DocregOperationOutcome::Archive(archive_result) => {
                match archive_result {
                    Ok(()) => {
                        let source_present = self.sessions.contains_key(&result.session_id);
                        if source_present {
                            self.capture_docreg_source_push(result.session_id);
                        }
                        let source_is_exact =
                            self.docreg_operation_context
                                .as_ref()
                                .is_some_and(|context| {
                                    context.archive_dispatch_fence.as_ref()
                                        == Some(&context.latest_source_fence)
                                        && (source_present || context.archive_notification_observed)
                                });
                        if !source_is_exact {
                            self.set_docreg_restore_required(result.session_id, true);
                            let context = self
                                .docreg_operation_context
                                .as_ref()
                                .expect("generation-matched document registration");
                            let preserved = DocregOperationContext {
                                source_fence: context.source_fence.clone(),
                                latest_source_fence: context.latest_source_fence.clone(),
                                launch_commands: context.launch_commands.clone(),
                                source_snapshot: context.source_snapshot.clone(),
                                source_order_index: context.source_order_index,
                                archive_dispatch_fence: context.archive_dispatch_fence.clone(),
                                archive_notification_observed: context
                                    .archive_notification_observed,
                            };
                            self.ensure_local_docreg_source(&preserved, result.session_id);
                            if let Err(error) = self.spawn_docreg_restore() {
                                self.docreg_operation_pending = None;
                                self.docreg_operation_context = None;
                                self.notify_error(format!(
                                    "Document registration restore failed: {error}; source preserved and retry required"
                                ));
                            }
                            return true;
                        }
                        let pending = self
                            .docreg_operation_pending
                            .take()
                            .expect("generation-matched document registration");
                        self.docreg_operation_context = None;
                        self.sessions.remove(&pending.session_id);
                        self.session_order.retain(|id| *id != pending.session_id);
                        self.clear_docreg_launch_progress(pending.session_id);
                        if pending.was_detail {
                            self.back_to_list();
                        }
                        self.notify_success("Archived source after accepted replacement launches");
                    }
                    Err(error) => {
                        let pending = self
                            .docreg_operation_pending
                            .take()
                            .expect("generation-matched document registration");
                        let context = self
                            .docreg_operation_context
                            .take()
                            .expect("generation-matched document registration context");
                        self.ensure_local_docreg_source(&context, pending.session_id);
                        self.notify_error(format!(
                            "Document registration archive failed: {error}; source preserved"
                        ));
                    }
                }
                true
            }
            DocregOperationOutcome::Restore(restore_result) => {
                let pending = self
                    .docreg_operation_pending
                    .take()
                    .expect("generation-matched document registration");
                let context = self
                    .docreg_operation_context
                    .take()
                    .expect("generation-matched document registration context");
                self.ensure_local_docreg_source(&context, pending.session_id);
                match restore_result {
                    Ok(restored) => {
                        if let Some(state) = self.sessions.get_mut(&pending.session_id) {
                            state.session.status = restored.status;
                            state.session.updated_at = restored.updated_at;
                        }
                        self.set_docreg_restore_required(pending.session_id, false);
                        self.notify_success(
                            "Restored changed document-registration source; retry to process its current commands",
                        );
                    }
                    Err(error) => {
                        self.set_docreg_restore_required(pending.session_id, true);
                        self.notify_error(format!(
                            "Document registration restore failed: {error}; source preserved and retry required"
                        ));
                    }
                }
                true
            }
        }
    }

    /// Auto-launch a new session when a completed session has a `/resume_handoff` docregblock.
    pub(crate) fn request_auto_launch_resume_handoff(&mut self) {
        if self.auto_resume_handoff_handle.is_some()
            || !self.authoritative_config_ready()
            || !self.poll.connected
        {
            return;
        }
        if self.custom_provider_index.is_none()
            && !self.is_provider_available(self.selected_provider)
        {
            self.notify_error(format!(
                "{} provider unavailable (check daemon health)",
                Self::provider_label(self.selected_provider)
            ));
            return;
        }

        let Some((session_id, query, working_dir)) =
            self.sessions.iter().find_map(|(&session_id, state)| {
                if state.session.status != rsi_common::types::SessionStatus::Completed {
                    return None;
                }
                if self.auto_launched_handoff_sessions.contains(&session_id)
                    || self.auto_resume_handoff_inflight.contains(&session_id)
                    || self
                        .auto_resume_handoff_failed
                        .get(&session_id)
                        .is_some_and(|epoch| *epoch == self.auto_resume_progress_epoch)
                {
                    return None;
                }
                let found = state.docregblock_contents.iter().find(|c| {
                    c.trim().split_whitespace().next().map_or(false, |cmd| {
                        cmd.strip_prefix('/').unwrap_or(cmd) == "resume_handoff"
                    })
                });
                found.map(|content| {
                    let handoff_path = content
                        .trim()
                        .strip_prefix("/resume_handoff")
                        .unwrap_or(content.trim())
                        .trim()
                        .trim_start_matches('@');
                    let query = format!("resume handoff @{}", handoff_path);
                    (session_id, query, state.session.working_dir.clone())
                })
            })
        else {
            return;
        };

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.auto_resume_handoff_inflight.insert(session_id);
        self.auto_resume_handoff_generation = self.auto_resume_handoff_generation.saturating_add(1);
        let generation = self.auto_resume_handoff_generation;
        self.auto_resume_handoff_active_generation = Some(generation);
        let socket_path = self.client.socket_path().to_path_buf();
        let request = self.owned_quick_launch_request(&query, Some(working_dir.clone()), None);
        let options = request.options();
        let model_warning = crate::app::session_actions::command_frontmatter_model_conflict(
            &query,
            Some(&working_dir),
            options.provider,
            options.model.as_deref(),
        );
        let tx = self.auto_resume_handoff_tx.clone();
        self.auto_resume_handoff_handle = Some(runtime.spawn(async move {
            // LaunchSession is non-idempotent. This owned task stays off the
            // event loop and awaits the semantic response identity.
            let accepted = request.execute(socket_path).await;
            let _ = tx
                .send(AutoResumeHandoffResult {
                    generation,
                    source_session_id: session_id,
                    accepted,
                    model_warning,
                })
                .await;
        }));
    }

    pub(crate) fn apply_auto_resume_handoff_result(
        &mut self,
        result: AutoResumeHandoffResult,
    ) -> bool {
        if self.auto_resume_handoff_active_generation != Some(result.generation) {
            return false;
        }
        self.auto_resume_handoff_active_generation = None;
        self.auto_resume_handoff_handle = None;
        self.auto_resume_handoff_inflight
            .remove(&result.source_session_id);
        match result.accepted {
            Ok(target_session_id) => {
                self.auto_launched_handoff_sessions
                    .insert(result.source_session_id);
                self.push_notification(
                    crate::types::NotificationKind::SessionLaunching,
                    crate::types::NotificationPriority::Low,
                    "Session launching...".to_string(),
                    Some(target_session_id),
                );
                if let Some(warning) = result.model_warning {
                    self.notify(warning);
                }
                self.request_auto_launch_resume_handoff();
                true
            }
            Err(error) => {
                tracing::warn!(session_id = %result.source_session_id, %error, "Automatic resume handoff launch rejected");
                self.auto_resume_handoff_failed
                    .insert(result.source_session_id, self.auto_resume_progress_epoch);
                self.notify_error(format!("Automatic resume handoff launch failed: {error}"));
                self.request_auto_launch_resume_handoff();
                true
            }
        }
    }

    pub(crate) fn advance_auto_resume_progress(&mut self) {
        self.auto_resume_progress_epoch = self.auto_resume_progress_epoch.saturating_add(1);
        self.request_auto_launch_resume_handoff();
    }
}
