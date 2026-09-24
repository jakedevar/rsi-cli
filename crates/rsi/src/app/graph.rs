//! Graph draft and execution state helpers.

use super::App;
use crate::types::{
    GraphBrowseMode, GraphDraft, GraphDraftPersistenceState, GraphMode, OverlayState,
};
use rsi_common::types::{WorkflowExecutionLookup, WorkflowExecutionStatus};
use uuid::Uuid;

impl App {
    /// Active draft currently associated with the graph editor.
    pub fn graph_review_draft_id(&self) -> Option<Uuid> {
        match &self.overlay {
            OverlayState::GraphReview { draft_id, .. } => Some(*draft_id),
            _ => self.active_graph_draft_id,
        }
    }

    /// Locate an existing draft for a durable workflow.
    pub fn find_graph_draft_id_by_workflow_id(&self, workflow_id: Uuid) -> Option<Uuid> {
        self.graph_drafts
            .values()
            .find(|draft| draft.workflow_id == Some(workflow_id))
            .map(|draft| draft.draft_id)
    }

    /// Create a fresh graph draft and make it the active draft.
    pub fn create_graph_draft(
        &mut self,
        workflow: rsi_graph::format::WorkflowDefinition,
        workflow_id: Option<Uuid>,
        project_id: Option<Uuid>,
    ) -> Uuid {
        let draft = GraphDraft::new(workflow, workflow_id, project_id);
        let draft_id = draft.draft_id;
        self.graph_drafts.insert(draft_id, draft);
        self.active_graph_draft_id = Some(draft_id);
        draft_id
    }

    /// Borrow a graph draft immutably.
    pub fn graph_draft(&self, draft_id: &Uuid) -> Option<&GraphDraft> {
        self.graph_drafts.get(draft_id)
    }

    /// Borrow a graph draft mutably.
    pub fn graph_draft_mut(&mut self, draft_id: &Uuid) -> Option<&mut GraphDraft> {
        self.graph_drafts.get_mut(draft_id)
    }

    /// Mark a draft dirty unless it is already in `save_failed`.
    pub fn mark_graph_draft_dirty(&mut self, draft_id: Uuid) -> bool {
        let Some(draft) = self.graph_drafts.get_mut(&draft_id) else {
            return false;
        };

        if !matches!(
            draft.persistence_state,
            GraphDraftPersistenceState::SaveFailed
        ) {
            draft.persistence_state = GraphDraftPersistenceState::Dirty;
        }
        true
    }

    /// Fetch the latest execution snapshot associated with a graph draft.
    pub fn graph_execution_for_draft(
        &self,
        draft_id: &Uuid,
    ) -> Option<&rsi_common::types::WorkflowExecutionSnapshot> {
        let draft = self.graph_drafts.get(draft_id)?;
        let execution_id = draft.last_execution_id?;
        self.graph_executions.get(&execution_id)
    }

    /// Return true when the draft's latest execution is still active. A
    /// durable execution blocked on preserved work is resumable, so it stays
    /// active (and interruptible).
    pub fn graph_execution_is_active_for_draft(&self, draft_id: &Uuid) -> bool {
        matches!(
            self.graph_execution_for_draft(draft_id)
                .map(|execution| execution.status),
            Some(
                WorkflowExecutionStatus::Accepted
                    | WorkflowExecutionStatus::Running
                    | WorkflowExecutionStatus::Blocked
            )
        )
    }

    /// Record an execution acknowledgment immediately after the daemon accepts a run.
    pub fn record_graph_execution_ack(
        &mut self,
        draft_id: Uuid,
        response: &rsi_common::rpc::ExecuteWorkflowResponse,
        workflow_name: String,
    ) {
        self.graph_executions.insert(
            response.execution_id,
            rsi_common::types::WorkflowExecutionSnapshot {
                execution_id: response.execution_id,
                workflow_id: response.workflow_id,
                workflow_name,
                status: rsi_common::types::WorkflowExecutionStatus::Accepted,
                accepted_at: response.accepted_at,
                started_at: None,
                finished_at: None,
                dry_run: response.dry_run,
                input: None,
                output: None,
                error: None,
                last_sequence: 0,
                row_version: None,
                blocked_attempt_id: None,
                blocked_reason: None,
                updates: Vec::new(),
            },
        );

        if let Some(draft) = self.graph_drafts.get_mut(&draft_id) {
            draft.last_execution_id = Some(response.execution_id);
        }

        self.reconcile_graph_review_execution_mode();
    }

    /// Record a graph execution update from the daemon.
    pub fn apply_graph_execution_update(
        &mut self,
        update: rsi_common::types::GraphExecutionUpdate,
    ) -> bool {
        let workflow_name = self
            .workflows
            .get(&update.workflow_id)
            .map(|workflow| workflow.title.clone())
            .unwrap_or_else(|| "workflow".to_string());

        let execution = self
            .graph_executions
            .entry(update.execution_id)
            .or_insert_with(|| rsi_common::types::WorkflowExecutionSnapshot {
                execution_id: update.execution_id,
                workflow_id: update.workflow_id,
                workflow_name,
                status: update.status,
                accepted_at: update.updated_at,
                started_at: None,
                finished_at: None,
                dry_run: true,
                input: None,
                output: None,
                error: None,
                last_sequence: 0,
                row_version: None,
                blocked_attempt_id: None,
                blocked_reason: None,
                updates: Vec::new(),
            });

        if update.sequence <= execution.last_sequence {
            return false;
        }

        execution.status = update.status;
        if execution.started_at.is_none()
            && matches!(
                update.status,
                rsi_common::types::WorkflowExecutionStatus::Running
                    | rsi_common::types::WorkflowExecutionStatus::Succeeded
                    | rsi_common::types::WorkflowExecutionStatus::Failed
                    | rsi_common::types::WorkflowExecutionStatus::Interrupted
            )
        {
            execution.started_at = Some(update.updated_at);
        }
        if update.finished {
            execution.finished_at = Some(update.updated_at);
        }
        if let Some(error) = update.error.clone() {
            execution.error = Some(error);
        }
        execution.last_sequence = update.sequence;
        execution.updates.push(update.clone());

        for draft in self.graph_drafts.values_mut() {
            if draft.workflow_id == Some(update.workflow_id) {
                draft.last_execution_id = Some(update.execution_id);
            }
        }

        self.reconcile_graph_review_execution_mode();
        true
    }

    /// Re-sync retained graph execution snapshots after reconnect or overlay reopen.
    pub async fn resync_graph_execution_for_draft(
        &mut self,
        draft_id: Uuid,
        notify_user: bool,
    ) -> bool {
        let Some(execution_id) = self
            .graph_drafts
            .get(&draft_id)
            .and_then(|draft| draft.last_execution_id)
        else {
            return false;
        };

        if !self.poll.connected {
            return false;
        }

        let lookup = self
            .client
            .get_workflow_execution(execution_id)
            .await
            .map_err(|error| error.to_string());
        self.apply_graph_execution_lookup(draft_id, lookup, notify_user)
    }

    /// Apply a graph execution lookup fetched by either an interactive resync
    /// or the background bootstrap snapshot job.
    pub(crate) fn apply_graph_execution_lookup(
        &mut self,
        draft_id: Uuid,
        lookup: Result<WorkflowExecutionLookup, String>,
        notify_user: bool,
    ) -> bool {
        let execution_id = self
            .graph_drafts
            .get(&draft_id)
            .and_then(|draft| draft.last_execution_id);
        let lookup = match lookup {
            Ok(lookup) => lookup,
            Err(error) => {
                if notify_user {
                    self.notify_error(format!("workflow execution resync failed: {error}"));
                } else {
                    tracing::warn!(%draft_id, ?execution_id, %error, "workflow execution resync failed");
                }
                return false;
            }
        };

        let mut changed = false;
        match lookup {
            WorkflowExecutionLookup::Found { execution } => {
                changed |= self
                    .graph_executions
                    .insert(execution.execution_id, execution)
                    .is_none();
            }
            WorkflowExecutionLookup::Expired { execution_id, .. } => {
                self.graph_executions.remove(&execution_id);
                if let Some(draft) = self.graph_drafts.get_mut(&draft_id)
                    && draft.last_execution_id == Some(execution_id)
                {
                    draft.last_execution_id = None;
                    changed = true;
                }
                if notify_user {
                    self.notify(format!("workflow execution expired: {}", execution_id));
                }
            }
            WorkflowExecutionLookup::NotFound { execution_id } => {
                self.graph_executions.remove(&execution_id);
                if let Some(draft) = self.graph_drafts.get_mut(&draft_id)
                    && draft.last_execution_id == Some(execution_id)
                {
                    draft.last_execution_id = None;
                    changed = true;
                }
                if notify_user {
                    self.notify_error(format!("workflow execution not found: {}", execution_id));
                }
            }
            _ => {
                if notify_user {
                    self.notify_error(
                        "workflow execution lookup returned unknown variant".to_string(),
                    );
                }
            }
        }

        changed |= self.reconcile_graph_review_execution_mode();
        changed
    }

    /// Re-sync all persisted graph execution IDs after reconnect.
    pub async fn resync_graph_executions(&mut self) -> bool {
        let active_draft = self.graph_review_draft_id();
        let draft_ids: Vec<Uuid> = self
            .graph_drafts
            .values()
            .filter(|draft| draft.last_execution_id.is_some())
            .map(|draft| draft.draft_id)
            .collect();

        let mut changed = false;
        for draft_id in draft_ids {
            changed |= self
                .resync_graph_execution_for_draft(draft_id, active_draft == Some(draft_id))
                .await;
        }
        changed
    }

    /// Keep the graph review overlay aligned with execution activity.
    pub fn reconcile_graph_review_execution_mode(&mut self) -> bool {
        let (draft_id, mode) = match &self.overlay {
            OverlayState::GraphReview { draft_id, mode, .. } => (*draft_id, *mode),
            _ => return false,
        };

        let execution_active = self.graph_execution_is_active_for_draft(&draft_id);
        let next_mode = match (mode, execution_active) {
            (
                GraphMode::Executing {
                    previous_mode,
                    camera,
                },
                false,
            ) => previous_mode.with_camera(camera),
            (mode, true) if !mode.is_executing() => mode.into_executing(),
            _ => mode,
        };

        if next_mode == mode {
            return false;
        }

        if let OverlayState::GraphReview { mode, .. } = &mut self.overlay {
            *mode = next_mode;
        }
        true
    }

    /// Open a graph draft in the review overlay with fresh UI state.
    pub fn show_graph_review_draft(&mut self, draft_id: Uuid, gv_info_dashboard: bool) {
        let mode = if self.graph_execution_is_active_for_draft(&draft_id) {
            GraphMode::Executing {
                previous_mode: GraphBrowseMode::Navigate,
                camera: crate::types::GraphCamera::FollowSelection,
            }
        } else {
            GraphMode::Navigate {
                camera: crate::types::GraphCamera::FollowSelection,
            }
        };

        // Propagate the draft-carried provenance so the read-only write-gate and
        // honesty badge fire for bridged drafts. A strict no-op for every
        // existing caller (all build `AuthoredWorkflow` drafts via `GraphDraft::new`).
        let view_origin = self
            .graph_draft(&draft_id)
            .map_or(crate::types::GraphViewOrigin::AuthoredWorkflow, |d| {
                d.view_origin.clone()
            });

        self.active_graph_draft_id = Some(draft_id);
        self.overlay = OverlayState::GraphReview {
            draft_id,
            selected_node: 0,
            viewport: crate::types::GraphViewport::default(),
            mode,
            edit_history: Vec::new(),
            collapsed: std::collections::HashSet::new(),
            edit_buffer: String::new(),
            selected_edge: 0,
            picker_list_state: ratatui::widgets::ListState::default(),
            view_origin,
            gv_info_dashboard,
            dashboard_focused: false,
            dashboard_state: None,
        };
        self.mark_dirty();
    }
}
