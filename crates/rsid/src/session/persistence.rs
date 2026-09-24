//! Persistence worker and PersistenceHandle methods for session management.

use super::types::{
    MetadataBarrierScope, PERSISTENCE_WARN_THRESHOLD, PersistenceHandle, StoreCommand,
};
use crate::error::{DaemonError, Result};
use crate::profiling;
use crate::store::Store;
use rsi_common::types::{
    Project, SandboxCleanupState, Session, SessionKind, SessionStatus, TurnMetric, WorkflowStage,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

impl PersistenceHandle {
    pub(super) fn new(store: Arc<tokio::sync::Mutex<Store>>) -> Self {
        let (tx, rx) = mpsc::channel(super::types::PERSISTENCE_QUEUE_CAPACITY);
        let pending = Arc::new(AtomicUsize::new(0));
        let last_command_duration_ms = Arc::new(AtomicU64::new(0));
        spawn_persistence_worker(store, rx, pending.clone(), last_command_duration_ms.clone());
        Self {
            tx,
            pending,
            last_command_duration_ms,
            capacity: super::types::PERSISTENCE_QUEUE_CAPACITY,
        }
    }

    pub(super) async fn send(&self, command: StoreCommand) -> Result<()> {
        let depth = self.pending.fetch_add(1, Ordering::SeqCst) + 1;
        let warn_threshold = (self.capacity as f32 * PERSISTENCE_WARN_THRESHOLD).ceil() as usize;
        if depth >= warn_threshold {
            tracing::warn!(
                depth,
                capacity = self.capacity,
                "Persistence queue above {}% capacity",
                (PERSISTENCE_WARN_THRESHOLD * 100.0) as u32
            );
        }
        if let Err(_) = self.tx.send(command).await {
            self.pending.fetch_sub(1, Ordering::SeqCst);
            return Err(DaemonError::ChannelClosed);
        }
        if profiling::enabled() {
            tracing::trace!(
                target = "rsid::profile",
                depth,
                capacity = self.capacity,
                "persistence_queue_depth"
            );
        }
        Ok(())
    }

    pub(super) async fn insert_event(
        &self,
        event: rsi_common::types::ConversationEvent,
    ) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::InsertEvent {
            event,
            provenance: None,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn insert_event_with_provenance(
        &self,
        event: rsi_common::types::ConversationEvent,
        provenance: rsi_common::closure_kernel::ConversationEventProvenanceV1,
    ) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::InsertEvent {
            event,
            provenance: Some(provenance),
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// Persist an attributed diagnostic through the ordered persistence
    /// worker. Callers report failures directly; this does not recursively
    /// attempt to record its own failure.
    pub(super) async fn insert_session_diagnostic(
        &self,
        diagnostic: rsi_common::types::NewSessionDiagnosticV1,
    ) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::InsertSessionDiagnostic {
            diagnostic,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// Persist one selected daemon diagnostic without changing the producer's
    /// control flow. Failures are reported to tracing and are never recorded
    /// recursively.
    pub(super) async fn record_session_diagnostic(
        &self,
        session_id: Uuid,
        level: rsi_common::types::SessionDiagnosticLevelV1,
        message: &'static str,
    ) {
        let diagnostic = rsi_common::types::NewSessionDiagnosticV1 {
            session_id,
            timestamp: chrono::Utc::now(),
            level,
            message: message.to_string(),
            fields: None,
        };
        if let Err(error) = self.insert_session_diagnostic(diagnostic).await {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "Failed to persist session diagnostic"
            );
        }
    }

    pub(super) async fn resolve_appserver_approval(
        &self,
        event: rsi_common::types::ConversationEvent,
        target: serde_json::Value,
        resolution: serde_json::Value,
    ) -> Result<(bool, bool)> {
        let (respond_to, rx) = oneshot::channel();
        self.send(StoreCommand::ResolveAppServerApproval {
            event,
            target,
            resolution,
            respond_to,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn publish_appserver_approval(
        &self,
        event: rsi_common::types::ConversationEvent,
        target: serde_json::Value,
    ) -> Result<i64> {
        let (respond_to, rx) = oneshot::channel();
        self.send(StoreCommand::PublishAppServerApproval {
            event,
            target,
            respond_to,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// A detected question and its event share one acknowledged producer
    /// operation; the Store invalidates the old identity before event insertion.
    pub(super) async fn publish_question_event(
        &self,
        event: rsi_common::types::ConversationEvent,
        provenance: Option<rsi_common::closure_kernel::ConversationEventProvenanceV1>,
        question: rsi_common::types::PendingQuestion,
    ) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::PublishQuestionEvent {
            event,
            provenance,
            question,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// Wait until the single persistence worker has completed every command
    /// enqueued before this fence. Unscoped callers use it only for ordering;
    /// metadata callers receive their write result directly.
    pub(super) async fn barrier(&self) -> Result<()> {
        self.barrier_with_metadata_scope(None).await
    }

    pub(super) async fn barrier_for_final_session_metadata(&self, session_id: Uuid) -> Result<()> {
        self.barrier_with_metadata_scope(Some(MetadataBarrierScope::finalizer(session_id)))
            .await
    }

    async fn barrier_with_metadata_scope(
        &self,
        metadata_scope: Option<MetadataBarrierScope>,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::Barrier {
            metadata_scope,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn update_status(
        &self,
        session_id: Uuid,
        status: SessionStatus,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateSessionStatus { session_id, status })
            .await
    }

    pub(super) async fn update_failed_and_stage_autofile(
        &self,
        session_id: Uuid,
        cause: crate::store::daemon_settings::AutofileCause,
    ) -> Result<()> {
        self.acknowledge_failed_and_stage_autofile(session_id, cause)
            .await
            .map(|_| ())
            .map_err(Into::into)
    }

    pub(super) async fn acknowledge_failed_and_stage_autofile(
        &self,
        session_id: Uuid,
        cause: crate::store::daemon_settings::AutofileCause,
    ) -> crate::store::daemon_settings::C5TransitionResult<
        crate::store::daemon_settings::C5StageOutcome,
    > {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::UpdateFailedAndStageAutofile {
            session_id,
            cause,
            respond_to: tx,
        })
        .await
        .map_err(|source| {
            crate::store::daemon_settings::C5TransitionError::Retryable {
                operation: "stage_persistence_enqueue",
                source,
            }
        })?;
        rx.await.map_err(
            |_| crate::store::daemon_settings::C5TransitionError::Retryable {
                operation: "stage_persistence_response",
                source: DaemonError::ChannelClosed,
            },
        )?
    }

    pub(super) async fn archive_and_resolve_autofile(&self, session_id: Uuid) -> Result<()> {
        self.acknowledge_archive_and_resolve_autofile(session_id)
            .await
            .map(|_| ())
            .map_err(Into::into)
    }

    pub(super) async fn acknowledge_archive_and_resolve_autofile(
        &self,
        session_id: Uuid,
    ) -> crate::store::daemon_settings::C5TransitionResult<
        crate::store::daemon_settings::C5SuppressionOutcome,
    > {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::ArchiveAndResolveAutofile {
            session_id,
            respond_to: tx,
        })
        .await
        .map_err(|source| {
            crate::store::daemon_settings::C5TransitionError::Retryable {
                operation: "archive_persistence_enqueue",
                source,
            }
        })?;
        rx.await.map_err(
            |_| crate::store::daemon_settings::C5TransitionError::Retryable {
                operation: "archive_persistence_response",
                source: DaemonError::ChannelClosed,
            },
        )?
    }

    pub(super) async fn update_pending_question_json(
        &self,
        session_id: Uuid,
        pending_question_json: Option<String>,
    ) -> Result<()> {
        self.send(StoreCommand::UpdatePendingQuestion {
            session_id,
            pending_question_json,
        })
        .await
    }

    pub(super) async fn insert_snapshot(&self, session_id: Uuid, tokens: u64) -> Result<()> {
        self.send(StoreCommand::InsertContextSnapshot { session_id, tokens })
            .await
    }

    pub(super) async fn insert_turn_metric(&self, metric: TurnMetric) -> Result<()> {
        self.send(StoreCommand::InsertTurnMetric { metric }).await
    }

    pub(super) async fn update_session_kind(
        &self,
        session_id: Uuid,
        kind: SessionKind,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateSessionKind { session_id, kind })
            .await
    }

    pub(super) async fn update_claude_session_id(
        &self,
        session_id: Uuid,
        claude_session_id: String,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateClaudeSessionId {
            session_id,
            claude_session_id,
        })
        .await
    }

    pub(super) async fn update_session_metadata(&self, session: Session) -> Result<()> {
        self.update_session_metadata_scoped(session, None).await
    }

    pub(super) async fn update_final_session_metadata(&self, session: Session) -> Result<()> {
        let scope = MetadataBarrierScope::finalizer(session.id);
        self.update_session_metadata_scoped(session, Some(scope))
            .await
    }

    async fn update_session_metadata_scoped(
        &self,
        session: Session,
        barrier_scope: Option<MetadataBarrierScope>,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::UpdateSessionMetadata {
            session,
            barrier_scope,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// Compare-and-swap one model/window/evidence tuple after all older queued
    /// writes complete. Returns false when another incarnation already changed
    /// the durable tuple.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn compare_and_update_session_model(
        &self,
        store: Arc<tokio::sync::Mutex<Store>>,
        session_id: Uuid,
        expected_model: Option<String>,
        expected_context_window: Option<u64>,
        expected_resolved_context_budget: Option<rsi_common::ResolvedContextBudget>,
        model: Option<String>,
        context_window: Option<u64>,
        resolved_context_budget: Option<rsi_common::ResolvedContextBudget>,
    ) -> Result<bool> {
        self.barrier().await?;
        run_store_op(store, move |store| {
            store.compare_and_update_session_model(
                session_id,
                expected_model.as_deref(),
                expected_context_window,
                expected_resolved_context_budget.as_ref(),
                model.as_deref(),
                context_window,
                resolved_context_budget.as_ref(),
            )
        })
        .await
    }

    /// Persist the provider handshake advertised at `system/init` (V99, P1-A).
    ///
    /// Uses the same barrier as the model tuple: both are init-time facts about
    /// the same session row, so ordering them against queued full-metadata
    /// writes matters for the same reason.
    pub(super) async fn update_session_provider_handshake(
        &self,
        store: Arc<tokio::sync::Mutex<Store>>,
        session_id: Uuid,
        cli_version: Option<String>,
        capabilities: Vec<String>,
    ) -> Result<()> {
        self.barrier().await?;
        run_store_op(store, move |store| {
            store.update_session_provider_handshake(
                session_id,
                cli_version.as_deref(),
                &capabilities,
            )
        })
        .await
    }

    pub(super) async fn update_session_project(
        &self,
        session_id: Uuid,
        project_id: Option<Uuid>,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateSessionProject {
            session_id,
            project_id,
        })
        .await
    }

    pub(super) async fn update_session_title(&self, session_id: Uuid, title: String) -> Result<()> {
        self.send(StoreCommand::UpdateSessionTitle { session_id, title })
            .await
    }

    /// Await the outcome of a generated-title fill so the caller learns whether
    /// it supplied the title or an explicit one was already present.
    pub(super) async fn fill_session_title_if_absent(
        &self,
        session_id: Uuid,
        title: String,
    ) -> Result<bool> {
        let (respond_to, rx) = oneshot::channel();
        self.send(StoreCommand::FillSessionTitleIfAbsent {
            session_id,
            title,
            respond_to,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn update_session_description(
        &self,
        session_id: Uuid,
        description: String,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateSessionDescription {
            session_id,
            description,
        })
        .await
    }

    pub(super) async fn update_session_rating(
        &self,
        session_id: Uuid,
        rating: Option<i16>,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateSessionRating { session_id, rating })
            .await
    }

    pub(super) async fn update_session_active_task(
        &self,
        session_id: Uuid,
        active_task: Option<String>,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateSessionActiveTask {
            session_id,
            active_task,
        })
        .await
    }

    #[allow(dead_code)]
    pub(super) async fn delete_session(&self, session_id: Uuid) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::DeleteSession {
            session_id,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn soft_delete_session(&self, session_id: Uuid) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::SoftDeleteSession {
            session_id,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn purge_session(&self, session_id: Uuid) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::PurgeSession {
            session_id,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn undelete_session(&self, session_id: Uuid) -> Result<Option<Session>> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::UndeleteSession {
            session_id,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn toggle_pin(&self, session_id: Uuid) -> Result<Option<String>> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::TogglePin {
            session_id,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn toggle_testing_needed(&self, session_id: Uuid) -> Result<Option<String>> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::ToggleTestingNeeded {
            session_id,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn toggle_rotation_disabled(
        &self,
        session_id: Uuid,
    ) -> Result<Option<String>> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::ToggleRotationDisabled {
            session_id,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn insert_project(&self, project: Project) -> Result<()> {
        self.send(StoreCommand::InsertProject { project }).await
    }

    pub(super) async fn update_project(&self, project: Project) -> Result<()> {
        self.send(StoreCommand::UpdateProjectRow { project }).await
    }

    pub(super) async fn delete_project(&self, project_id: Uuid) -> Result<()> {
        self.send(StoreCommand::DeleteProject { project_id }).await
    }

    pub(super) async fn unarchive_session(&self, session_id: Uuid) -> Result<Option<Session>> {
        let (tx, rx) = oneshot::channel();
        self.send(StoreCommand::UnarchiveSession {
            session_id,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    pub(super) async fn update_workflow_stage(
        &self,
        workflow_id: Uuid,
        stage: WorkflowStage,
        artifact_path: Option<String>,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateWorkflowStage {
            workflow_id,
            stage,
            artifact_path,
        })
        .await
    }

    pub(super) async fn update_session_workflow(
        &self,
        session_id: Uuid,
        workflow_id: Option<Uuid>,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateSessionWorkflow {
            session_id,
            workflow_id,
        })
        .await
    }

    pub(super) async fn log_rotation_event(
        &self,
        session_id: Uuid,
        rotation_id: &str,
        phase: &str,
        event_type: &str,
        metadata: Option<String>,
    ) -> Result<()> {
        self.send(StoreCommand::InsertRotationEvent {
            session_id,
            rotation_id: rotation_id.to_string(),
            phase: phase.to_string(),
            event_type: event_type.to_string(),
            metadata,
        })
        .await
    }

    pub(super) async fn insert_label(&self, label: rsi_common::types::SessionLabel) -> Result<()> {
        self.send(StoreCommand::InsertLabel { label }).await
    }

    pub(super) async fn update_label(&self, label: rsi_common::types::SessionLabel) -> Result<()> {
        self.send(StoreCommand::UpdateLabelRow { label }).await
    }

    pub(super) async fn delete_label(&self, label_id: Uuid) -> Result<()> {
        self.send(StoreCommand::DeleteLabel { label_id }).await
    }

    pub(super) async fn insert_topology(
        &self,
        topology: rsi_common::types::Topology,
    ) -> Result<()> {
        self.send(StoreCommand::InsertTopology { topology }).await
    }

    pub(super) async fn update_topology(
        &self,
        topology: rsi_common::types::Topology,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateTopologyRow { topology })
            .await
    }

    pub(super) async fn delete_topology(&self, topology_id: Uuid) -> Result<()> {
        self.send(StoreCommand::DeleteTopology { topology_id })
            .await
    }

    pub(super) async fn update_session_label(
        &self,
        session_id: Uuid,
        group_id: Option<Uuid>,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateSessionLabel {
            session_id,
            group_id,
        })
        .await
    }

    pub(super) async fn update_pending_archive(
        &self,
        session_id: Uuid,
        pending_archive: bool,
    ) -> Result<()> {
        self.send(StoreCommand::UpdatePendingArchive {
            session_id,
            pending_archive,
        })
        .await
    }

    pub(super) async fn insert_session_summary(
        &self,
        summary: rsi_common::types::SessionSummary,
    ) -> Result<()> {
        self.send(StoreCommand::InsertSessionSummary { summary })
            .await
    }

    pub(super) async fn upsert_entity_card(
        &self,
        card: rsi_common::types::EntityCard,
    ) -> Result<()> {
        self.send(StoreCommand::UpsertEntityCard { card }).await
    }

    pub(super) async fn update_retry_state(
        &self,
        session_id: Uuid,
        retry_attempt: Option<u8>,
        max_retries: Option<u8>,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateRetryState {
            session_id,
            retry_attempt,
            max_retries,
        })
        .await
    }

    // Callers live in worktree-jiuwenclaw-hooks-permissions-compression (compression path).
    #[allow(dead_code)]
    pub(super) async fn offload_event_content(
        &self,
        session_id: Uuid,
        event_sequence: i32,
        content_hash: String,
        original_content: String,
    ) -> Result<()> {
        self.send(StoreCommand::OffloadEventContent {
            session_id,
            event_sequence,
            content_hash,
            original_content,
        })
        .await
    }

    // Callers live in worktree-jiuwenclaw-hooks-permissions-compression (compression path).
    #[allow(dead_code)]
    pub(super) async fn update_event_content(
        &self,
        event_id: i64,
        new_content: String,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateEventContent {
            event_id,
            new_content,
        })
        .await
    }

    /// Update a session's sandbox cleanup state. Used by allocator/destroy flows
    /// and the orphan sweep. `state = None` clears the column (e.g. on purge).
    #[allow(dead_code)]
    pub(super) async fn update_sandbox_cleanup_state(
        &self,
        session_id: Uuid,
        state: Option<SandboxCleanupState>,
    ) -> Result<()> {
        self.send(StoreCommand::UpdateSandboxCleanupState { session_id, state })
            .await
    }

    /// Atomically tombstone the sandbox metadata after a successful destroy:
    /// clears `sandbox_root`/`sandbox_branch` and stamps
    /// `sandbox_cleanup_state = 'Purged'` in a single UPDATE. Prevents future
    /// readers from picking up a stale on-disk path whose worktree is gone.
    pub(super) async fn mark_sandbox_purged(&self, session_id: Uuid) -> Result<()> {
        self.send(StoreCommand::MarkSandboxPurged { session_id })
            .await
    }
}

fn spawn_persistence_worker(
    store: Arc<tokio::sync::Mutex<Store>>,
    mut rx: mpsc::Receiver<StoreCommand>,
    pending: Arc<AtomicUsize>,
    last_command_duration_ms: Arc<AtomicU64>,
) {
    tokio::spawn(async move {
        let mut metadata_failures = HashMap::<MetadataBarrierScope, String>::new();
        while let Some(command) = rx.recv().await {
            pending.fetch_sub(1, Ordering::SeqCst);
            let start = std::time::Instant::now();
            match command {
                StoreCommand::InsertSession { session } => {
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.insert_session(&session)).await
                    {
                        tracing::error!(error = %e, "Failed to persist session");
                    }
                }
                StoreCommand::Barrier {
                    metadata_scope,
                    respond_to,
                } => {
                    let result = metadata_scope
                        .and_then(|scope| metadata_failures.remove(&scope))
                        .map(|error| {
                            Err(DaemonError::Store(format!(
                                "metadata write before barrier failed: {error}"
                            )))
                        })
                        .unwrap_or(Ok(()));
                    let _ = respond_to.send(result);
                }
                StoreCommand::InsertEvent {
                    event,
                    provenance,
                    respond_to,
                } => {
                    let res = run_store_op(store.clone(), move |s| match provenance {
                        Some(provenance) => s.insert_event_with_provenance(&event, &provenance),
                        None => s.insert_event(&event),
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::InsertSessionDiagnostic {
                    diagnostic,
                    respond_to,
                } => {
                    let res = run_store_op(store.clone(), move |s| {
                        s.insert_session_diagnostic(&diagnostic)
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::ResolveAppServerApproval {
                    event,
                    target,
                    resolution,
                    respond_to,
                } => {
                    let res = run_store_op(store.clone(), move |s| {
                        let closed = s.resolve_appserver_approval(&event, &target, &resolution)?;
                        Ok((closed, s.appserver_approval_waiting(event.session_id)?))
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::PublishAppServerApproval {
                    event,
                    target,
                    respond_to,
                } => {
                    let res = run_store_op(store.clone(), move |s| {
                        s.publish_appserver_approval(&event, &target)
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::PublishQuestionEvent {
                    event,
                    provenance,
                    question,
                    respond_to,
                } => {
                    let res = run_store_op(store.clone(), move |s| {
                        s.publish_pending_question_event(&event, provenance.as_ref(), &question)
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::UpdateSessionStatus { session_id, status } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_status(session_id, status)
                    })
                    .await
                    {
                        tracing::error!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to update session status"
                        );
                    }
                }
                StoreCommand::UpdateFailedAndStageAutofile {
                    session_id,
                    cause,
                    respond_to,
                } => {
                    let res = run_c5_store_op(store.clone(), move |s| {
                        s.update_failed_and_stage_c5_autofile(session_id, cause)
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::ArchiveAndResolveAutofile {
                    session_id,
                    respond_to,
                } => {
                    let res = run_c5_store_op(store.clone(), move |s| {
                        s.archive_and_resolve_c5_autofile_pending(session_id)
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::UpdatePendingQuestion {
                    session_id,
                    pending_question_json,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_pending_question_json(
                            session_id,
                            pending_question_json.as_deref(),
                        )
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to persist pending question"
                        );
                    }
                }
                StoreCommand::UpdateSessionKind { session_id, kind } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_kind(session_id, kind)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to update session kind"
                        );
                    }
                }
                StoreCommand::InsertContextSnapshot { session_id, tokens } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.insert_context_snapshot(session_id, tokens)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to insert context snapshot"
                        );
                    }
                }
                StoreCommand::InsertTurnMetric { metric } => {
                    let sid = metric.session_id;
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.insert_turn_metric(&metric)).await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %sid,
                            "Failed to insert turn metric"
                        );
                    }
                }
                StoreCommand::UpdateClaudeSessionId {
                    session_id,
                    claude_session_id,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_claude_session_id(session_id, &claude_session_id)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to update claude_session_id"
                        );
                    }
                }
                StoreCommand::UpdateSessionMetadata {
                    session,
                    barrier_scope,
                    respond_to,
                } => {
                    let sid = session.id;
                    let result =
                        run_store_op(store.clone(), move |s| s.update_session_metadata(&session))
                            .await;
                    if let Err(e) = &result {
                        if let Some(scope) = barrier_scope {
                            metadata_failures
                                .entry(scope)
                                .or_insert_with(|| format!("session {sid}: {e}"));
                        }
                        tracing::warn!(
                            error = %e,
                            session_id = %sid,
                            "Failed to update session metadata"
                        );
                    }
                    let _ = respond_to.send(result);
                }
                StoreCommand::UpdateSessionProject {
                    session_id,
                    project_id,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_project(session_id, project_id)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to update session project"
                        );
                    }
                }
                StoreCommand::UpdateSessionTitle { session_id, title } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_title(session_id, &title)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to persist session title"
                        );
                    }
                }
                StoreCommand::FillSessionTitleIfAbsent {
                    session_id,
                    title,
                    respond_to,
                } => {
                    let res = run_store_op(store.clone(), move |s| {
                        s.fill_session_title_if_absent(session_id, &title)
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::UpdateSessionDescription {
                    session_id,
                    description,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_description(session_id, &description)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to persist session description"
                        );
                    }
                }
                StoreCommand::UpdateSessionRating { session_id, rating } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_rating(session_id, rating)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to persist session rating"
                        );
                    }
                }
                StoreCommand::UpdateSessionActiveTask {
                    session_id,
                    active_task,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_active_task(session_id, active_task.as_deref())
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to persist session active_task"
                        );
                    }
                }
                StoreCommand::DeleteSession {
                    session_id,
                    respond_to,
                } => {
                    let res =
                        run_store_op(store.clone(), move |s| s.delete_session(session_id)).await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::SoftDeleteSession {
                    session_id,
                    respond_to,
                } => {
                    let res =
                        run_store_op(store.clone(), move |s| s.soft_delete_session(session_id))
                            .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::PurgeSession {
                    session_id,
                    respond_to,
                } => {
                    let res =
                        run_store_op(store.clone(), move |s| s.purge_session(session_id)).await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::UndeleteSession {
                    session_id,
                    respond_to,
                } => {
                    let res =
                        run_store_op(store.clone(), move |s| s.undelete_session(session_id)).await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::TogglePin {
                    session_id,
                    respond_to,
                } => {
                    let res =
                        run_store_op(store.clone(), move |s| s.toggle_session_pin(session_id))
                            .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::ToggleTestingNeeded {
                    session_id,
                    respond_to,
                } => {
                    let res = run_store_op(store.clone(), move |s| {
                        s.toggle_session_testing_needed(session_id)
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::ToggleRotationDisabled {
                    session_id,
                    respond_to,
                } => {
                    let res = run_store_op(store.clone(), move |s| {
                        s.toggle_session_rotation_disabled(session_id)
                    })
                    .await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::InsertProject { project } => {
                    let pid = project.id;
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.insert_project(&project)).await
                    {
                        tracing::error!(project_id = %pid, error = %e, "Failed to insert project");
                    }
                }
                StoreCommand::UpdateProjectRow { project } => {
                    let pid = project.id;
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.update_project(&project)).await
                    {
                        tracing::error!(project_id = %pid, error = %e, "Failed to update project");
                    }
                }
                StoreCommand::DeleteProject { project_id } => {
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.delete_project(project_id)).await
                    {
                        tracing::error!(project_id = %project_id, error = %e, "Failed to delete project");
                    }
                }
                StoreCommand::UnarchiveSession {
                    session_id,
                    respond_to,
                } => {
                    let res =
                        run_store_op(store.clone(), move |s| s.unarchive_session(session_id)).await;
                    let _ = respond_to.send(res);
                }
                StoreCommand::UpdateWorkflowStage {
                    workflow_id,
                    stage,
                    artifact_path,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_workflow_stage(workflow_id, stage, artifact_path.as_deref())
                    })
                    .await
                    {
                        tracing::warn!(
                            workflow_id = %workflow_id,
                            error = %e,
                            "Failed to update workflow stage"
                        );
                    }
                }
                StoreCommand::UpdateSessionWorkflow {
                    session_id,
                    workflow_id,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_workflow(session_id, workflow_id)
                    })
                    .await
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            error = %e,
                            "Failed to update session workflow_id"
                        );
                    }
                }
                StoreCommand::InsertRotationEvent {
                    session_id,
                    rotation_id,
                    phase,
                    event_type,
                    metadata,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.insert_rotation_event(
                            session_id,
                            &rotation_id,
                            &phase,
                            &event_type,
                            metadata.as_deref(),
                        )
                    })
                    .await
                    {
                        tracing::warn!(error = %e, "Failed to insert rotation event");
                    }
                }
                StoreCommand::InsertLabel { label } => {
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.insert_label(&label)).await
                    {
                        tracing::error!(error = %e, "Failed to insert label");
                    }
                }
                StoreCommand::UpdateLabelRow { label } => {
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.update_label(&label)).await
                    {
                        tracing::error!(error = %e, "Failed to update label");
                    }
                }
                StoreCommand::DeleteLabel { label_id } => {
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.delete_label(label_id)).await
                    {
                        tracing::error!(label_id = %label_id, error = %e, "Failed to delete label");
                    }
                }
                StoreCommand::InsertTopology { topology } => {
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.insert_topology(&topology)).await
                    {
                        tracing::error!(error = %e, "Failed to insert topology");
                    }
                }
                StoreCommand::UpdateTopologyRow { topology } => {
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.update_topology(&topology)).await
                    {
                        tracing::error!(error = %e, "Failed to update topology");
                    }
                }
                StoreCommand::DeleteTopology { topology_id } => {
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.delete_topology(topology_id)).await
                    {
                        tracing::error!(topology_id = %topology_id, error = %e, "Failed to delete topology");
                    }
                }
                StoreCommand::UpdateSessionLabel {
                    session_id,
                    group_id,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_session_label(session_id, group_id)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to update session label"
                        );
                    }
                }
                StoreCommand::UpdatePendingArchive {
                    session_id,
                    pending_archive,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_pending_archive(session_id, pending_archive)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to update pending_archive"
                        );
                    }
                }
                StoreCommand::InsertSessionSummary { summary } => {
                    let sid = summary.session_id;
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.insert_session_summary(&summary))
                            .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %sid,
                            "Failed to insert session summary"
                        );
                    }
                }
                StoreCommand::UpsertEntityCard { card } => {
                    let card_id = card.id;
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.upsert_entity_card(&card)).await
                    {
                        tracing::error!(
                            card_id = %card_id,
                            error = %e,
                            "Failed to upsert entity card"
                        );
                    }
                }
                StoreCommand::UpdateRetryState {
                    session_id,
                    retry_attempt,
                    max_retries,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_retry_state(session_id, retry_attempt, max_retries)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to persist retry state"
                        );
                    }
                }
                StoreCommand::OffloadEventContent {
                    session_id,
                    event_sequence,
                    content_hash,
                    original_content,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.insert_offloaded_content(
                            session_id,
                            event_sequence,
                            &content_hash,
                            &original_content,
                        )
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            event_sequence = %event_sequence,
                            "Failed to offload event content"
                        );
                    }
                }
                StoreCommand::UpdateEventContent {
                    event_id,
                    new_content,
                } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_event_content(event_id, &new_content)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            event_id = %event_id,
                            "Failed to update event content"
                        );
                    }
                }
                StoreCommand::UpdateSandboxCleanupState { session_id, state } => {
                    if let Err(e) = run_store_op(store.clone(), move |s| {
                        s.update_sandbox_cleanup_state(session_id, state)
                    })
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to persist sandbox cleanup state"
                        );
                    }
                }
                StoreCommand::MarkSandboxPurged { session_id } => {
                    if let Err(e) =
                        run_store_op(store.clone(), move |s| s.mark_sandbox_purged(session_id))
                            .await
                    {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to tombstone sandbox metadata after destroy"
                        );
                    }
                }
            }
            let elapsed_ms = start.elapsed().as_millis() as u64;
            last_command_duration_ms.store(elapsed_ms, Ordering::Relaxed);
            if elapsed_ms >= 100 {
                tracing::warn!(elapsed_ms, "persistence worker: slow command detected");
            }
        }
    });
}

pub(super) async fn run_store_op<F, T>(store: Arc<tokio::sync::Mutex<Store>>, op: F) -> Result<T>
where
    F: FnOnce(&mut Store) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = store.blocking_lock();
        op(&mut guard)
    })
    .await
    .map_err(|e| DaemonError::Process(format!("Store worker join error: {}", e)))?
}

async fn run_c5_store_op<F, T>(
    store: Arc<tokio::sync::Mutex<Store>>,
    op: F,
) -> crate::store::daemon_settings::C5TransitionResult<T>
where
    F: FnOnce(&mut Store) -> crate::store::daemon_settings::C5TransitionResult<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = store.blocking_lock();
        op(&mut guard)
    })
    .await
    .map_err(
        |source| crate::store::daemon_settings::C5TransitionError::Retryable {
            operation: "persistence_store_worker_join",
            source: DaemonError::Process(format!("Store worker join error: {source}")),
        },
    )?
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::provider_capabilities::{
        CapabilityConfidence, CapabilityEvidence, CapabilitySource, ContextCapacity,
        ResolvedContextBudget,
    };
    use rsi_common::types::{ContextUsageConfidence, SessionProvider};
    use std::path::PathBuf;

    fn context_budget(active_tokens: u64, source: CapabilitySource) -> ResolvedContextBudget {
        let confidence = match source {
            CapabilitySource::RuntimeTelemetry | CapabilitySource::Configured => {
                CapabilityConfidence::Authoritative
            }
            CapabilitySource::OfficialDocumentation | CapabilitySource::ProviderCatalog => {
                CapabilityConfidence::Verified
            }
            CapabilitySource::RepositoryFallback | CapabilitySource::LegacyUnverified => {
                CapabilityConfidence::Degraded
            }
        };
        ResolvedContextBudget {
            active_tokens,
            capacity: ContextCapacity::default(),
            evidence: CapabilityEvidence {
                source,
                source_version: Some("codex-cli 0.155.1".to_string()),
                source_digest: Some(format!("sha256:{}", "a".repeat(64))),
                observed_at: Some(
                    "2026-09-02T12:34:56.123456789Z"
                        .parse()
                        .expect("fixed provenance timestamp"),
                ),
                confidence,
            },
        }
    }

    fn test_session(session_id: Uuid) -> Session {
        let now = chrono::Utc::now() - chrono::Duration::seconds(1);
        Session {
            context_fill_pct: None,
            id: session_id,
            provider: SessionProvider::Codex,
            claude_session_id: None,
            query: "h2 model durability".to_string(),
            title: Some("unrelated title".to_string()),
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp/rsi-h2-model-durability"),
            git_branch: Some("main".to_string()),
            status: SessionStatus::Running,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: SessionKind::Task,
            created_at: now,
            updated_at: now,
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: Some("old-model".to_string()),
            input_tokens: None,
            output_tokens: None,
            context_window: Some(1),
            resolved_context_budget: Some(context_budget(1, CapabilitySource::Configured)),
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
            approval_wait_ms: Some(0),
            approval_started_at: None,
            work_time_ms: None,
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

    async fn store_with_session() -> (Arc<tokio::sync::Mutex<Store>>, tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().expect("create persistence test directory");
        let store = Arc::new(tokio::sync::Mutex::new(
            Store::open(&dir.path().join("rsi.db")).expect("open persistence test store"),
        ));
        let session = test_session(Uuid::new_v4());
        store
            .lock()
            .await
            .insert_session(&session)
            .expect("insert persistence test session");
        (store, dir, session)
    }

    #[tokio::test]
    async fn c4a_context_tuple_compare_and_swap_reports_closed_queue() {
        let (store, _dir, original) = store_with_session().await;
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let handle = PersistenceHandle {
            tx,
            pending: Arc::new(AtomicUsize::new(0)),
            last_command_duration_ms: Arc::new(AtomicU64::new(0)),
            capacity: 1,
        };

        let error = handle
            .compare_and_update_session_model(
                store.clone(),
                original.id,
                original.model.clone(),
                original.context_window,
                original.resolved_context_budget.clone(),
                Some("new-model".to_string()),
                Some(262_144),
                Some(context_budget(262_144, CapabilitySource::RuntimeTelemetry)),
            )
            .await
            .expect_err("closed queue must fail before focused write");
        assert!(matches!(error, DaemonError::ChannelClosed));

        let persisted = store
            .lock()
            .await
            .get_session(original.id)
            .expect("load model after closed queue")
            .expect("focused model session remains");
        assert_eq!(persisted.model, original.model);
        assert_eq!(persisted.context_window, original.context_window);
        assert_eq!(
            persisted.resolved_context_budget,
            original.resolved_context_budget
        );
    }

    #[tokio::test]
    async fn c4a_context_tuple_compare_and_swap_rejects_stale_generation() {
        let (store, _dir, original) = store_with_session().await;
        let handle = PersistenceHandle::new(store.clone());
        let runtime = context_budget(258_400, CapabilitySource::RuntimeTelemetry);

        assert!(
            handle
                .compare_and_update_session_model(
                    store.clone(),
                    original.id,
                    original.model.clone(),
                    original.context_window,
                    original.resolved_context_budget.clone(),
                    original.model.clone(),
                    Some(runtime.active_tokens),
                    Some(runtime.clone()),
                )
                .await
                .expect("current generation owns the tuple")
        );

        let stale_replacement = context_budget(300_000, CapabilitySource::Configured);
        assert!(
            !handle
                .compare_and_update_session_model(
                    store.clone(),
                    original.id,
                    original.model.clone(),
                    original.context_window,
                    original.resolved_context_budget.clone(),
                    original.model.clone(),
                    Some(stale_replacement.active_tokens),
                    Some(stale_replacement),
                )
                .await
                .expect("stale generation is rejected without mutation")
        );

        let persisted = store
            .lock()
            .await
            .get_session(original.id)
            .expect("load fenced context tuple")
            .expect("session remains");
        assert_eq!(persisted.context_window, Some(258_400));
        assert_eq!(persisted.resolved_context_budget, Some(runtime));
    }

    #[tokio::test]
    async fn c4a_context_tuple_write_failure_rolls_back_value_and_provenance() {
        let (store, _dir, original) = store_with_session().await;
        let handle = PersistenceHandle::new(store.clone());
        let runtime = context_budget(258_400, CapabilitySource::RuntimeTelemetry);
        store
            .lock()
            .await
            .conn
            .execute_batch(
                "CREATE TRIGGER c4a_context_tuple_fail_update\n\
                 BEFORE UPDATE OF context_window ON sessions\n\
                 BEGIN SELECT RAISE(ABORT, 'c4a injected context tuple failure'); END;",
            )
            .expect("install isolated context-tuple trigger");

        let error = handle
            .compare_and_update_session_model(
                store.clone(),
                original.id,
                original.model.clone(),
                original.context_window,
                original.resolved_context_budget.clone(),
                original.model.clone(),
                Some(runtime.active_tokens),
                Some(runtime),
            )
            .await
            .expect_err("injected tuple write must fail atomically");
        assert!(
            error
                .to_string()
                .contains("c4a injected context tuple failure")
        );

        let persisted = store
            .lock()
            .await
            .get_session(original.id)
            .expect("load rolled-back tuple")
            .expect("session remains");
        assert_eq!(persisted.context_window, original.context_window);
        assert_eq!(
            persisted.resolved_context_budget,
            original.resolved_context_budget
        );
    }

    #[tokio::test]
    async fn failed_metadata_write_fails_its_ack_and_next_barrier_then_allows_recovery() {
        let (store, _dir, original) = store_with_session().await;
        let handle = PersistenceHandle::new(store.clone());
        let mut invalid = original.clone();
        invalid.context_window = Some(0);
        invalid.permission_denial_count = Some(4);

        let write_error = handle
            .update_final_session_metadata(invalid)
            .await
            .expect_err("invalid metadata must fail its acknowledged write");
        assert!(matches!(write_error, DaemonError::InvalidParam(_)));
        let barrier_error = handle
            .barrier_for_final_session_metadata(original.id)
            .await
            .expect_err("barrier must report the preceding failed write");
        assert!(
            barrier_error
                .to_string()
                .contains("metadata write before barrier failed")
        );
        let persisted = store
            .lock()
            .await
            .get_session(original.id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.context_window, original.context_window);
        assert_eq!(persisted.permission_denial_count, None);

        let mut corrected = original.clone();
        corrected.context_window = Some(2);
        corrected.resolved_context_budget = Some(context_budget(2, CapabilitySource::Configured));
        corrected.permission_denial_count = Some(4);
        handle
            .update_final_session_metadata(corrected)
            .await
            .unwrap();
        handle
            .barrier_for_final_session_metadata(original.id)
            .await
            .unwrap();
        let persisted = store
            .lock()
            .await
            .get_session(original.id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.context_window, Some(2));
        assert_eq!(persisted.permission_denial_count, Some(4));
    }

    #[tokio::test]
    async fn metadata_failures_do_not_poison_unrelated_barriers_or_sessions() {
        let (store, _dir, original) = store_with_session().await;
        let mut other = original.clone();
        other.id = Uuid::new_v4();
        assert!(store.lock().await.insert_session(&other).is_ok());
        let handle = PersistenceHandle::new(store);

        let mut invalid_snapshot = original.clone();
        invalid_snapshot.context_window = Some(0);
        assert!(
            handle
                .update_session_metadata(invalid_snapshot)
                .await
                .is_err()
        );
        assert!(handle.barrier().await.is_ok());
        assert!(
            handle
                .barrier_for_final_session_metadata(original.id)
                .await
                .is_ok()
        );

        let mut invalid_final = original.clone();
        invalid_final.context_window = Some(0);
        assert!(
            handle
                .update_final_session_metadata(invalid_final)
                .await
                .is_err()
        );
        assert!(handle.barrier().await.is_ok());
        assert!(
            handle
                .barrier_for_final_session_metadata(other.id)
                .await
                .is_ok()
        );
        assert!(
            handle
                .barrier_for_final_session_metadata(original.id)
                .await
                .is_err()
        );
    }

    #[test]
    fn c4a_runtime_context_tuple_survives_database_reopen() {
        let directory = tempfile::tempdir().expect("create reopen directory");
        let database = directory.path().join("context-budget.db");
        let original = test_session(Uuid::new_v4());
        let runtime = context_budget(258_400, CapabilitySource::RuntimeTelemetry);
        {
            let store = Store::open(&database).expect("open initial store");
            store.insert_session(&original).expect("insert session");
            assert!(
                store
                    .compare_and_update_session_model(
                        original.id,
                        original.model.as_deref(),
                        original.context_window,
                        original.resolved_context_budget.as_ref(),
                        original.model.as_deref(),
                        Some(runtime.active_tokens),
                        Some(&runtime),
                    )
                    .expect("persist runtime tuple")
            );
        }

        let reopened = Store::open(&database).expect("reopen store");
        let loaded = reopened
            .get_session(original.id)
            .expect("read reopened session")
            .expect("session survived reopen");
        assert_eq!(loaded.context_window, Some(258_400));
        assert_eq!(loaded.resolved_context_budget, Some(runtime));
    }

    #[test]
    fn configured_raw_window_survives_runtime_cas_and_database_reopen() {
        let directory = tempfile::tempdir().expect("create configured reopen directory");
        let database = directory.path().join("configured-context-budget.db");
        let original = test_session(Uuid::new_v4());
        let mut configured = context_budget(380_000, CapabilitySource::Configured);
        configured.capacity.configured_tokens = Some(400_000);
        let mut runtime = context_budget(380_000, CapabilitySource::RuntimeTelemetry);
        runtime.capacity.configured_tokens = Some(400_000);
        {
            let store = Store::open(&database).expect("open initial store");
            store.insert_session(&original).expect("insert session");
            assert!(
                store
                    .compare_and_update_session_model(
                        original.id,
                        original.model.as_deref(),
                        original.context_window,
                        original.resolved_context_budget.as_ref(),
                        original.model.as_deref(),
                        Some(configured.active_tokens),
                        Some(&configured),
                    )
                    .expect("persist configured tuple")
            );
            assert!(
                store
                    .compare_and_update_session_model(
                        original.id,
                        original.model.as_deref(),
                        Some(configured.active_tokens),
                        Some(&configured),
                        original.model.as_deref(),
                        Some(runtime.active_tokens),
                        Some(&runtime),
                    )
                    .expect("persist runtime tuple")
            );
        }

        let reopened = Store::open(&database).expect("reopen configured store");
        let loaded = reopened
            .get_session(original.id)
            .expect("read configured session")
            .expect("configured session survived reopen");
        assert_eq!(loaded.context_window, Some(380_000));
        assert_eq!(loaded.resolved_context_budget, Some(runtime));
    }

    #[tokio::test]
    async fn diagnostic_recording_uses_ordered_persistence_worker() {
        let store = Store::open_in_memory().expect("open store");
        let session_id = Uuid::new_v4();
        store
            .insert_session(&test_session(session_id))
            .expect("insert session");
        store
            .install_session_diagnostics_schema_for_test()
            .expect("install diagnostics schema");

        let store = Arc::new(tokio::sync::Mutex::new(store));
        let persistence = PersistenceHandle::new(Arc::clone(&store));
        persistence
            .record_session_diagnostic(
                session_id,
                rsi_common::types::SessionDiagnosticLevelV1::Warn,
                "Session rotation deadline expired",
            )
            .await;
        persistence
            .record_session_diagnostic(
                session_id,
                rsi_common::types::SessionDiagnosticLevelV1::Error,
                "CodexAppServer handshake failed",
            )
            .await;

        let diagnostics = store
            .lock()
            .await
            .list_session_diagnostics(session_id, None, 10)
            .expect("read persisted diagnostic");
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].session_id, session_id);
        assert_eq!(
            diagnostics[0].level,
            rsi_common::types::SessionDiagnosticLevelV1::Warn
        );
        assert_eq!(
            diagnostics[1].level,
            rsi_common::types::SessionDiagnosticLevelV1::Error
        );
        assert_eq!(diagnostics[0].message, "Session rotation deadline expired");
        assert_eq!(diagnostics[1].message, "CodexAppServer handshake failed");
    }
}
