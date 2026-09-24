//! Bounded, generation-fenced controller for `Pane::Issues`.

use std::time::{SystemTime, UNIX_EPOCH};

use rsi_common::issue_workspace::{
    ArchiveIssueRequestV1, CreateIssueV2RequestV1, GetIssueInProjectRequestV1,
    ISSUE_WORKSPACE_DEFAULT_LIMIT, IssueContentPatchV2, IssueDependencyCursorV1,
    IssueDependencyDirectionV1, IssueDependencyMutationRequestV1, IssueDependencyMutationResultV1,
    IssueDependencyPageV1, IssueDispatchRecordV1, IssueTrackerStatusV1, IssueTrackerTickResultV1,
    IssueWorkspaceCursorV1, IssueWorkspaceErrorV1, IssueWorkspacePageV1, IssueWorkspaceRowV1,
    ListIssueDependenciesRequestV1, ListIssueEventsV2RequestV1, ListIssuesPageRequestV1,
    NullablePatchV1, OperatorIssueMutationResultV1, PageDirectionV1, RestoreIssueRequestV1,
    UpdateIssueRequestV1, UpdateIssueStatusV2RequestV1,
};
use rsi_common::types::{Issue, IssueEventPageV1, IssueStatus, Session};
use uuid::Uuid;

use super::{App, replace_node};
use crate::types::{
    ISSUE_WORKSPACE_FOCUSED_REFRESH_MS, ISSUE_WORKSPACE_STALE_AFTER_MS, IssueCancelConfirmation,
    IssueDependencyReconciliation, IssueEditorMode, IssueEditorState,
    IssuePendingDependencyMutation, IssuePendingMutation, IssueWorkspaceDataKind,
    IssueWorkspaceFocus, IssueWorkspaceLoadState, IssueWorkspaceLocalRequestIdentity,
    IssueWorkspacePageAnchor, IssueWorkspaceRequestIdentity, IssueWorkspaceSavedView,
    IssueWorkspaceState, IssueWorkspaceTab, Pane, PaneId, SplitNode,
};

#[derive(Debug)]
pub enum IssueWorkspaceAsyncPayload {
    LocalPage {
        cursor: Option<IssueWorkspaceCursorV1>,
        direction: PageDirectionV1,
        page: IssueWorkspacePageV1,
    },
    SelectedIssue {
        requested_issue_id: Uuid,
        row: Option<IssueWorkspaceRowV1>,
    },
    Dependencies {
        requested_issue_id: Uuid,
        direction: IssueDependencyDirectionV1,
        page_direction: PageDirectionV1,
        cursor: Option<rsi_common::issue_workspace::IssueDependencyCursorV1>,
        page: IssueDependencyPageV1,
    },
    Events {
        requested_issue_id: Uuid,
        after_sequence: Option<i64>,
        page: IssueEventPageV1,
    },
    DependencyCandidates {
        requested_issue_id: Uuid,
        query: String,
        page: IssueWorkspacePageV1,
    },
    AssociatedSession {
        requested_session_id: Uuid,
        session: Session,
    },
    Dispatched(Vec<IssueDispatchRecordV1>),
    SyncStatus(IssueTrackerStatusV1),
    ManualPoll(IssueTrackerTickResultV1),
    Mutation(OperatorIssueMutationResultV1),
    DependencyMutation(IssueDependencyMutationResultV1),
}

#[derive(Debug)]
pub struct IssueWorkspaceAsyncEvent {
    pub pane_id: PaneId,
    pub project_id: Option<Uuid>,
    pub data_kind: IssueWorkspaceDataKind,
    pub generation: u64,
    pub request_identity: IssueWorkspaceRequestIdentity,
    pub outcome: Result<IssueWorkspaceAsyncPayload, IssueWorkspaceAsyncError>,
}

#[derive(Debug)]
pub struct IssueWorkspaceAsyncError {
    pub message: String,
    pub access_denied: bool,
    pub workspace: Option<IssueWorkspaceErrorV1>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn refresh_decision(last: Option<i64>, in_flight: bool, now: i64) -> (bool, bool) {
    let Some(last) = last else {
        return (!in_flight, !in_flight);
    };
    let age = now.saturating_sub(last);
    (
        age >= ISSUE_WORKSPACE_STALE_AFTER_MS,
        age >= ISSUE_WORKSPACE_FOCUSED_REFRESH_MS && !in_flight,
    )
}

fn async_error(error: crate::client::ClientError) -> IssueWorkspaceAsyncError {
    let workspace = error.issue_workspace_error();
    let access_denied =
        workspace.as_ref().is_some_and(|data| {
            data.code == rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::AccessDenied
        }) || matches!(&error, crate::client::ClientError::Rpc { code: -32001, .. });
    IssueWorkspaceAsyncError {
        message: error.to_string(),
        access_denied,
        workspace,
    }
}

fn visible_mutation_error(error: &IssueWorkspaceAsyncError) -> String {
    match &error.workspace {
        Some(workspace) => format!(
            "{:?}: {} — {}",
            workspace.code, error.message, workspace.next_action
        ),
        None => error.message.clone(),
    }
}

fn local_request_identity(
    project_id: Uuid,
    state: &IssueWorkspaceState,
) -> IssueWorkspaceLocalRequestIdentity {
    let direction = state.transient.page_direction.unwrap_or_else(|| {
        if matches!(&state.local.page_anchor, IssueWorkspacePageAnchor::End) {
            PageDirectionV1::Backward
        } else {
            PageDirectionV1::Forward
        }
    });
    let cursor = match &state.local.page_anchor {
        IssueWorkspacePageAnchor::Cursor(cursor) => Some(cursor.clone()),
        IssueWorkspacePageAnchor::Start | IssueWorkspacePageAnchor::End => None,
    };
    IssueWorkspaceLocalRequestIdentity {
        project_id,
        filters: state.local.filters.clone(),
        page_anchor: state.local.page_anchor.clone(),
        direction,
        cursor,
    }
}

fn mutation_identity(mutation: &IssuePendingMutation) -> String {
    match mutation {
        IssuePendingMutation::Create(request) => request.idempotency_key.clone(),
        IssuePendingMutation::Update(request) => request.idempotency_key.clone(),
        IssuePendingMutation::Status(request) => request.idempotency_key.clone(),
        IssuePendingMutation::Archive(request) => request.idempotency_key.clone(),
        IssuePendingMutation::Restore(request) => request.idempotency_key.clone(),
        IssuePendingMutation::AddDependency(pending) => {
            format!(
                "add:{}:{}",
                pending.request.issue_id, pending.request.depends_on_id
            )
        }
        IssuePendingMutation::RemoveDependency(pending) => {
            format!(
                "remove:{}:{}",
                pending.request.issue_id, pending.request.depends_on_id
            )
        }
    }
}

fn dependency_mutation_context(
    mutation: &IssuePendingMutation,
) -> Option<&IssuePendingDependencyMutation> {
    match mutation {
        IssuePendingMutation::AddDependency(pending)
        | IssuePendingMutation::RemoveDependency(pending) => Some(pending),
        _ => None,
    }
}

fn dependency_reconciliation_kinds() -> [IssueWorkspaceDataKind; 5] {
    [
        IssueWorkspaceDataKind::SelectedIssue,
        IssueWorkspaceDataKind::BlockedBy,
        IssueWorkspaceDataKind::Blocks,
        IssueWorkspaceDataKind::Events,
        IssueWorkspaceDataKind::LocalPage,
    ]
}

fn begin_dependency_reconciliation(
    state: &mut IssueWorkspaceState,
    pending: &IssuePendingDependencyMutation,
) {
    state.transient.dependency_reconciliation = Some(IssueDependencyReconciliation {
        origin_issue_id: pending.origin_issue_id,
        direction: pending.direction,
        pending_kinds: dependency_reconciliation_kinds().into_iter().collect(),
    });
    for kind in dependency_reconciliation_kinds() {
        let data = state.data_state_mut(kind);
        if data.last_good_ms.is_some() {
            data.load_state = IssueWorkspaceLoadState::Stale;
        }
    }
}

fn finish_dependency_reconciliation_kind(
    state: &mut IssueWorkspaceState,
    kind: IssueWorkspaceDataKind,
) -> Option<Uuid> {
    let completed = state
        .transient
        .dependency_reconciliation
        .as_mut()
        .is_some_and(|reconciliation| {
            reconciliation.pending_kinds.remove(&kind);
            reconciliation.pending_kinds.is_empty()
        });
    if !completed {
        return None;
    }
    let reconciliation = state.transient.dependency_reconciliation.take().unwrap();
    if state
        .transient
        .mutation_retry_error
        .as_deref()
        .is_some_and(|message| message.starts_with("ISSUE WRITE OUTCOME UNKNOWN"))
    {
        state.transient.mutation_retry_error = Some(format!(
            "ISSUE WRITE OUTCOME UNKNOWN — authoritative readback complete for {} ({:?}); Ctrl-Enter retries identical request",
            reconciliation.origin_issue_id, reconciliation.direction
        ));
    }
    state
        .local
        .selected_issue_id
        .filter(|selected| *selected != reconciliation.origin_issue_id)
}

fn dependency_reconciliation_target(
    state: &IssueWorkspaceState,
    kind: IssueWorkspaceDataKind,
) -> Option<Uuid> {
    state
        .transient
        .dependency_reconciliation
        .as_ref()
        .filter(|reconciliation| reconciliation.pending_kinds.contains(&kind))
        .map(|reconciliation| reconciliation.origin_issue_id)
}

fn issue_is_active_unarchived(issue: &Issue) -> bool {
    issue.archived_at.is_none()
        && matches!(issue.status, IssueStatus::Open | IssueStatus::InProgress)
}

fn issue_is_terminal_unarchived(issue: &Issue) -> bool {
    issue.archived_at.is_none()
        && matches!(issue.status, IssueStatus::Closed | IssueStatus::Cancelled)
}

fn issue_is_terminal_archived(issue: &Issue) -> bool {
    issue.archived_at.is_some()
        && matches!(issue.status, IssueStatus::Closed | IssueStatus::Cancelled)
}

impl App {
    /// Focus the existing workspace for this project, or replace the focused leaf.
    pub fn open_or_focus_issues(&mut self) -> PaneId {
        let project_id = self.active_project_id().or(self.current_project_id);
        let existing = {
            let tab = self.active_tab();
            tab.layout.leaf_ids().into_iter().find(|pane_id| {
                matches!(
                    tab.layout.find_pane(*pane_id),
                    Some(Pane::Issues(state)) if state.project_id == project_id
                )
            })
        };
        if let Some(pane_id) = existing {
            self.active_tab_mut().focused_pane = pane_id;
            self.consume_deferred_issue_inspector(pane_id);
            return pane_id;
        }

        let pane_id = self.active_tab().focused_pane;
        let previous = self
            .active_tab()
            .layout
            .find_pane(pane_id)
            .cloned()
            .unwrap_or_else(crate::types::Tab::default_session_list_state);
        self.pre_issues_panes.insert(pane_id, previous);
        let state = IssueWorkspaceState::new(project_id);
        let tab = self.active_tab_mut();
        tab.layout = replace_node(tab.layout.clone(), pane_id, |_| SplitNode::Leaf {
            pane: Pane::Issues(state),
            id: pane_id,
        });
        tab.focused_pane = pane_id;
        self.mark_dirty();
        self.request_issue_workspace_active(pane_id, true);
        pane_id
    }

    /// Restore the pane replaced by the currently focused Issues workspace.
    pub fn restore_pre_issues_pane(&mut self) -> bool {
        let pane_id = self.active_tab().focused_pane;
        if !matches!(
            self.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(_))
        ) {
            return false;
        }
        let pane = self
            .pre_issues_panes
            .remove(&pane_id)
            .unwrap_or_else(crate::types::Tab::default_session_list_state);
        let tab = self.active_tab_mut();
        tab.layout = replace_node(tab.layout.clone(), pane_id, |_| SplitNode::Leaf {
            pane,
            id: pane_id,
        });
        self.mark_dirty();
        true
    }

    pub fn rebind_issue_workspaces(&mut self, project_id: Option<Uuid>) {
        for tab in &mut self.tabs {
            for pane_id in tab.layout.leaf_ids() {
                if let Some(Pane::Issues(state)) = tab.layout.find_pane_mut(pane_id) {
                    state.rebind_project(project_id);
                }
            }
        }
    }

    fn consume_deferred_issue_inspector(&mut self, pane_id: PaneId) -> bool {
        if self.active_tab().focused_pane != pane_id {
            return false;
        }
        let issue_id = {
            let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id)
            else {
                return false;
            };
            let Some(deferred_issue_id) = state.transient.deferred_inspector_target.take() else {
                return false;
            };
            match state.local.selected_issue_id {
                Some(issue_id) if issue_id == deferred_issue_id => issue_id,
                Some(issue_id) => {
                    state.transient.deferred_inspector_target = Some(issue_id);
                    return false;
                }
                None => return false,
            }
        };
        self.reconcile_associated_session_selection(pane_id, issue_id);
        self.request_issue_inspector(pane_id, issue_id);
        true
    }

    /// Called from a one-second event-loop cadence, never from rendering.
    pub fn issue_workspace_tick(&mut self, now: i64) {
        let pane_id = self.active_tab().focused_pane;
        if self.consume_deferred_issue_inspector(pane_id) {
            self.mark_dirty();
        }
        let Some(Pane::Issues(state)) = self.active_tab().layout.find_pane(pane_id) else {
            return;
        };
        if state
            .transient
            .cancel_confirmation
            .as_ref()
            .is_some_and(|confirmation| now.saturating_sub(confirmation.armed_at_ms) > 2_000)
            && let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id)
        {
            state.transient.cancel_confirmation = None;
        }
        let Some(Pane::Issues(state)) = self.active_tab().layout.find_pane(pane_id) else {
            return;
        };
        let kind = active_data_kind(state.active_tab);
        let last = state
            .transient
            .data_states
            .get(&kind)
            .and_then(|data| data.last_refresh_ms);
        let in_flight = state.transient.in_flight.contains(&kind);
        let (mark_stale, request) = refresh_decision(last, in_flight, now);
        if mark_stale
            && let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id)
        {
            state.data_state_mut(kind).load_state = IssueWorkspaceLoadState::Stale;
        }
        if request {
            self.request_issue_workspace_active(pane_id, false);
        }
    }

    pub fn request_issue_workspace_active(&mut self, pane_id: PaneId, _manual: bool) {
        let data_kind = match self.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => active_data_kind(state.active_tab),
            _ => return,
        };
        self.request_issue_workspace_kind(pane_id, data_kind);
    }

    fn request_issue_manual_poll(&mut self, pane_id: PaneId) {
        self.request_issue_workspace_kind(pane_id, IssueWorkspaceDataKind::ManualPoll);
    }

    fn request_issue_workspace_kind(&mut self, pane_id: PaneId, data_kind: IssueWorkspaceDataKind) {
        self.request_issue_workspace_kind_in_tab(self.active_tab, pane_id, data_kind);
    }

    fn request_issue_workspace_kind_in_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        data_kind: IssueWorkspaceDataKind,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        let Some(Pane::Issues(state)) = tab.layout.find_pane_mut(pane_id) else {
            return;
        };
        let Some(project_id) = state.project_id else {
            let data = state.data_state_mut(data_kind);
            data.load_state = IssueWorkspaceLoadState::Error;
            data.error = Some("No project selected".to_string());
            return;
        };
        let request_identity = match data_kind {
            IssueWorkspaceDataKind::LocalPage => {
                IssueWorkspaceRequestIdentity::Local(local_request_identity(project_id, state))
            }
            IssueWorkspaceDataKind::Dispatched => IssueWorkspaceRequestIdentity::Dispatched,
            IssueWorkspaceDataKind::SyncStatus => IssueWorkspaceRequestIdentity::SyncStatus,
            IssueWorkspaceDataKind::ManualPoll => IssueWorkspaceRequestIdentity::ManualPoll,
            _ => unreachable!("active Issue workspace request kind"),
        };
        let Some(generation) = state.begin_request(data_kind, request_identity.clone()) else {
            return;
        };
        let has_last_good = state
            .transient
            .data_states
            .get(&data_kind)
            .is_some_and(|data| data.last_good_ms.is_some());
        let data = state.data_state_mut(data_kind);
        data.last_refresh_ms = Some(now_ms());
        data.error = None;
        if !has_last_good {
            data.load_state = IssueWorkspaceLoadState::Loading;
        }
        if data_kind == IssueWorkspaceDataKind::ManualPoll {
            state.transient.manual_poll_error = None;
            if state.transient.sync_status.is_some() {
                state
                    .data_state_mut(IssueWorkspaceDataKind::SyncStatus)
                    .load_state = IssueWorkspaceLoadState::Stale;
            }
        }
        let local_identity = match &request_identity {
            IssueWorkspaceRequestIdentity::Local(identity) => Some(identity.clone()),
            _ => None,
        };
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.issues_tx.clone();
        tokio::spawn(async move {
            let mut client = crate::client::DaemonClient::new(socket_path);
            let outcome = match client.connect().await {
                Err(error) => Err(async_error(error)),
                Ok(()) => match data_kind {
                    IssueWorkspaceDataKind::LocalPage => client
                        .list_issues_page(ListIssuesPageRequestV1 {
                            project_id,
                            statuses: local_identity.as_ref().unwrap().filters.statuses.clone(),
                            priorities: local_identity.as_ref().unwrap().filters.priorities.clone(),
                            readiness: local_identity.as_ref().unwrap().filters.readiness,
                            assignee: local_identity.as_ref().unwrap().filters.assignee.clone(),
                            unassigned: local_identity.as_ref().unwrap().filters.unassigned,
                            labels_all: local_identity.as_ref().unwrap().filters.labels_all.clone(),
                            provenance: local_identity.as_ref().unwrap().filters.provenance.clone(),
                            query: local_identity.as_ref().unwrap().filters.query.clone(),
                            archive: local_identity.as_ref().unwrap().filters.archive,
                            sort: local_identity.as_ref().unwrap().filters.sort,
                            direction: local_identity.as_ref().unwrap().direction,
                            cursor: local_identity.as_ref().unwrap().cursor.clone(),
                            limit: ISSUE_WORKSPACE_DEFAULT_LIMIT,
                        })
                        .await
                        .map(|page| IssueWorkspaceAsyncPayload::LocalPage {
                            cursor: local_identity.as_ref().unwrap().cursor.clone(),
                            direction: local_identity.as_ref().unwrap().direction,
                            page,
                        })
                        .map_err(async_error),
                    IssueWorkspaceDataKind::Dispatched => client
                        .list_dispatched_issues()
                        .await
                        .map(IssueWorkspaceAsyncPayload::Dispatched)
                        .map_err(async_error),
                    IssueWorkspaceDataKind::ManualPoll => client
                        .trigger_issue_tracker_poll()
                        .await
                        .map(IssueWorkspaceAsyncPayload::ManualPoll)
                        .map_err(async_error),
                    IssueWorkspaceDataKind::SyncStatus => client
                        .get_issue_tracker_status()
                        .await
                        .map(IssueWorkspaceAsyncPayload::SyncStatus)
                        .map_err(async_error),
                    _ => unreachable!("active Issue workspace request kind"),
                },
            };
            let _ = tx.send(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind,
                generation,
                request_identity,
                outcome,
            });
        });
    }

    pub fn request_issue_inspector(&mut self, pane_id: PaneId, issue_id: Uuid) {
        self.request_issue_inspector_in_tab(self.active_tab, pane_id, issue_id);
    }

    fn request_issue_inspector_in_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        issue_id: Uuid,
    ) {
        self.request_selected_issue_in_tab(tab_index, pane_id, issue_id);
        self.request_issue_dependency_page_in_tab(
            tab_index,
            pane_id,
            issue_id,
            IssueDependencyDirectionV1::BlockedBy,
            PageDirectionV1::Forward,
            None,
        );
        self.request_issue_dependency_page_in_tab(
            tab_index,
            pane_id,
            issue_id,
            IssueDependencyDirectionV1::Blocks,
            PageDirectionV1::Forward,
            None,
        );
        self.request_issue_event_page_in_tab(tab_index, pane_id, issue_id, None);
    }

    fn request_issue_dependency_reconciliation_in_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        origin_issue_id: Uuid,
    ) {
        self.request_issue_inspector_in_tab(tab_index, pane_id, origin_issue_id);
        self.request_issue_workspace_kind_in_tab(
            tab_index,
            pane_id,
            IssueWorkspaceDataKind::LocalPage,
        );
    }

    fn request_selected_issue(&mut self, pane_id: PaneId, issue_id: Uuid) {
        self.request_selected_issue_in_tab(self.active_tab, pane_id, issue_id);
    }

    fn request_selected_issue_in_tab(&mut self, tab_index: usize, pane_id: PaneId, issue_id: Uuid) {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        let Some(Pane::Issues(state)) = tab.layout.find_pane_mut(pane_id) else {
            return;
        };
        let Some(project_id) = state.project_id else {
            return;
        };
        let data_kind = IssueWorkspaceDataKind::SelectedIssue;
        let request_identity = IssueWorkspaceRequestIdentity::Issue(issue_id);
        let Some(generation) = state.begin_request(data_kind, request_identity.clone()) else {
            return;
        };
        state.data_state_mut(data_kind).load_state = IssueWorkspaceLoadState::Loading;
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.issues_tx.clone();
        tokio::spawn(async move {
            let mut client = crate::client::DaemonClient::new(socket_path);
            let outcome = match client.connect().await {
                Err(error) => Err(async_error(error)),
                Ok(()) => client
                    .get_issue_in_project(GetIssueInProjectRequestV1 {
                        project_id,
                        issue_id,
                    })
                    .await
                    .map(|row| IssueWorkspaceAsyncPayload::SelectedIssue {
                        requested_issue_id: issue_id,
                        row,
                    })
                    .map_err(async_error),
            };
            let _ = tx.send(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind,
                generation,
                request_identity,
                outcome,
            });
        });
    }

    fn request_issue_dependency_page(
        &mut self,
        pane_id: PaneId,
        issue_id: Uuid,
        direction: IssueDependencyDirectionV1,
        page_direction: PageDirectionV1,
        cursor: Option<IssueDependencyCursorV1>,
    ) {
        self.request_issue_dependency_page_in_tab(
            self.active_tab,
            pane_id,
            issue_id,
            direction,
            page_direction,
            cursor,
        );
    }

    fn request_issue_dependency_page_in_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        issue_id: Uuid,
        direction: IssueDependencyDirectionV1,
        page_direction: PageDirectionV1,
        cursor: Option<IssueDependencyCursorV1>,
    ) {
        let data_kind = match direction {
            IssueDependencyDirectionV1::BlockedBy => IssueWorkspaceDataKind::BlockedBy,
            IssueDependencyDirectionV1::Blocks => IssueWorkspaceDataKind::Blocks,
        };
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        let Some(Pane::Issues(state)) = tab.layout.find_pane_mut(pane_id) else {
            return;
        };
        let Some(project_id) = state.project_id else {
            return;
        };
        let request_identity = IssueWorkspaceRequestIdentity::Dependency {
            issue_id,
            direction,
            page_direction,
            cursor: cursor.clone(),
        };
        let Some(generation) = state.begin_request(data_kind, request_identity.clone()) else {
            return;
        };
        state.data_state_mut(data_kind).load_state = IssueWorkspaceLoadState::Loading;
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.issues_tx.clone();
        tokio::spawn(async move {
            let mut client = crate::client::DaemonClient::new(socket_path);
            let outcome = match client.connect().await {
                Err(error) => Err(async_error(error)),
                Ok(()) => client
                    .list_issue_dependencies(ListIssueDependenciesRequestV1 {
                        project_id,
                        issue_id,
                        direction,
                        page_direction,
                        cursor: cursor.clone(),
                        limit: ISSUE_WORKSPACE_DEFAULT_LIMIT,
                    })
                    .await
                    .map(|page| IssueWorkspaceAsyncPayload::Dependencies {
                        requested_issue_id: issue_id,
                        direction,
                        page_direction,
                        cursor,
                        page,
                    })
                    .map_err(async_error),
            };
            let _ = tx.send(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind,
                generation,
                request_identity,
                outcome,
            });
        });
    }

    fn request_issue_event_page(
        &mut self,
        pane_id: PaneId,
        issue_id: Uuid,
        after_sequence: Option<i64>,
    ) {
        self.request_issue_event_page_in_tab(self.active_tab, pane_id, issue_id, after_sequence);
    }

    fn request_issue_event_page_in_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        issue_id: Uuid,
        after_sequence: Option<i64>,
    ) {
        let data_kind = IssueWorkspaceDataKind::Events;
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        let Some(Pane::Issues(state)) = tab.layout.find_pane_mut(pane_id) else {
            return;
        };
        let Some(project_id) = state.project_id else {
            return;
        };
        let request_identity = IssueWorkspaceRequestIdentity::Events {
            issue_id,
            after_sequence,
        };
        let Some(generation) = state.begin_request(data_kind, request_identity.clone()) else {
            return;
        };
        state.data_state_mut(data_kind).load_state = IssueWorkspaceLoadState::Loading;
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.issues_tx.clone();
        tokio::spawn(async move {
            let mut client = crate::client::DaemonClient::new(socket_path);
            let outcome = match client.connect().await {
                Err(error) => Err(async_error(error)),
                Ok(()) => client
                    .list_issue_events_v2(ListIssueEventsV2RequestV1 {
                        project_id,
                        issue_id,
                        after_sequence,
                        limit: ISSUE_WORKSPACE_DEFAULT_LIMIT,
                    })
                    .await
                    .map(|page| IssueWorkspaceAsyncPayload::Events {
                        requested_issue_id: issue_id,
                        after_sequence,
                        page,
                    })
                    .map_err(async_error),
            };
            let _ = tx.send(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind,
                generation,
                request_identity,
                outcome,
            });
        });
    }

    fn request_issue_dependency_candidates(
        &mut self,
        pane_id: PaneId,
        issue_id: Uuid,
        query: String,
    ) {
        self.request_issue_dependency_candidates_in_tab(self.active_tab, pane_id, issue_id, query);
    }

    fn request_issue_dependency_candidates_in_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        issue_id: Uuid,
        query: String,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        let Some(Pane::Issues(state)) = tab.layout.find_pane_mut(pane_id) else {
            return;
        };
        let Some(project_id) = state.project_id else {
            return;
        };
        let request_identity = IssueWorkspaceRequestIdentity::DependencyCandidates {
            issue_id,
            query: query.clone(),
        };
        let Some(generation) = state.begin_request(
            IssueWorkspaceDataKind::DependencyCandidates,
            request_identity.clone(),
        ) else {
            return;
        };
        state
            .data_state_mut(IssueWorkspaceDataKind::DependencyCandidates)
            .load_state = IssueWorkspaceLoadState::Loading;
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.issues_tx.clone();
        tokio::spawn(async move {
            let mut client = crate::client::DaemonClient::new(socket_path);
            let outcome = match client.connect().await {
                Err(error) => Err(async_error(error)),
                Ok(()) => client
                    .list_issues_page(ListIssuesPageRequestV1 {
                        project_id,
                        statuses: Vec::new(),
                        priorities: Vec::new(),
                        readiness: Default::default(),
                        assignee: None,
                        unassigned: false,
                        labels_all: Vec::new(),
                        provenance: Vec::new(),
                        query: (!query.trim().is_empty()).then_some(query.clone()),
                        archive: rsi_common::types::IssueArchiveFilterV1::Active,
                        sort: Default::default(),
                        direction: PageDirectionV1::Forward,
                        cursor: None,
                        limit: ISSUE_WORKSPACE_DEFAULT_LIMIT,
                    })
                    .await
                    .map(|page| IssueWorkspaceAsyncPayload::DependencyCandidates {
                        requested_issue_id: issue_id,
                        query,
                        page,
                    })
                    .map_err(async_error),
            };
            let _ = tx.send(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: IssueWorkspaceDataKind::DependencyCandidates,
                generation,
                request_identity,
                outcome,
            });
        });
    }

    fn request_issue_associated_session(&mut self, pane_id: PaneId, session_id: Uuid) {
        self.request_issue_associated_session_in_tab(self.active_tab, pane_id, session_id);
    }

    fn request_issue_associated_session_in_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        session_id: Uuid,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        let Some(Pane::Issues(state)) = tab.layout.find_pane_mut(pane_id) else {
            return;
        };
        let Some(project_id) = state.project_id else {
            return;
        };
        let request_identity = IssueWorkspaceRequestIdentity::AssociatedSession(session_id);
        let Some(generation) = state.begin_request(
            IssueWorkspaceDataKind::AssociatedSession,
            request_identity.clone(),
        ) else {
            return;
        };
        state
            .data_state_mut(IssueWorkspaceDataKind::AssociatedSession)
            .load_state = IssueWorkspaceLoadState::Loading;
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.issues_tx.clone();
        tokio::spawn(async move {
            let mut client = crate::client::DaemonClient::new(socket_path);
            let outcome = match client.connect().await {
                Err(error) => Err(async_error(error)),
                Ok(()) => client
                    .get_session(session_id)
                    .await
                    .map(|session| IssueWorkspaceAsyncPayload::AssociatedSession {
                        requested_session_id: session_id,
                        session,
                    })
                    .map_err(async_error),
            };
            let _ = tx.send(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: IssueWorkspaceDataKind::AssociatedSession,
                generation,
                request_identity,
                outcome,
            });
        });
    }

    pub fn apply_issue_workspace_event(&mut self, event: IssueWorkspaceAsyncEvent) {
        let Some((tab_index, _)) = self.tabs.iter().enumerate().find(|(_, tab)| {
            matches!(
                tab.layout.find_pane(event.pane_id),
                Some(Pane::Issues(state)) if state.project_id == event.project_id
            )
        }) else {
            return;
        };
        let mut inspector_selection = None;
        let mut selected_issue_refetch = None;
        let mut refresh_kinds = Vec::new();
        let mut restart = None;
        let mut dependency_reconciliation = None;
        let mut open_fetched_session = None;
        let mut notification = None;
        let mut error_notification = false;
        {
            let Some(Pane::Issues(state)) =
                self.tabs[tab_index].layout.find_pane_mut(event.pane_id)
            else {
                return;
            };
            state.transient.in_flight.remove(&event.data_kind);
            if state.generation(event.data_kind) != event.generation {
                if let Some(identity) = state.request_identity(event.data_kind).cloned() {
                    restart = Some((event.data_kind, identity));
                }
            } else {
                let current_identity = current_request_identity(state, event.data_kind);
                let payload_identity_matches = match &event.outcome {
                    Ok(payload) => {
                        payload_matches_identity(event.data_kind, payload, &event.request_identity)
                    }
                    Err(_) => true,
                };
                if current_identity.as_ref() != Some(&event.request_identity)
                    || !payload_identity_matches
                {
                    if let Some(identity) = current_identity {
                        state
                            .transient
                            .desired_requests
                            .insert(event.data_kind, identity.clone());
                        restart = Some((event.data_kind, identity));
                    }
                } else {
                    match event.outcome {
                        Ok(payload) => {
                            let refreshed_at = now_ms();
                            let data = state.data_state_mut(event.data_kind);
                            data.error = None;
                            data.load_state = IssueWorkspaceLoadState::Fresh;
                            data.last_refresh_ms = Some(refreshed_at);
                            data.last_good_ms = Some(refreshed_at);
                            match payload {
                                IssueWorkspaceAsyncPayload::LocalPage {
                                    cursor,
                                    direction,
                                    page,
                                } => {
                                    let selected_position =
                                        state.local.selected_issue_id.and_then(|id| {
                                            page.rows.iter().position(|row| row.issue.id == id)
                                        });
                                    let previous_selection = state.local.selected_issue_id;
                                    state.local.selected_row =
                                        selected_position.unwrap_or_else(|| {
                                            if cursor.is_some()
                                                || matches!(
                                                    state.local.page_anchor,
                                                    IssueWorkspacePageAnchor::End
                                                )
                                            {
                                                match direction {
                                                    PageDirectionV1::Forward => 0,
                                                    PageDirectionV1::Backward => {
                                                        page.rows.len().saturating_sub(1)
                                                    }
                                                }
                                            } else {
                                                state
                                                    .local
                                                    .selected_row
                                                    .min(page.rows.len().saturating_sub(1))
                                            }
                                        });
                                    state.local.selected_issue_id = page
                                        .rows
                                        .get(state.local.selected_row)
                                        .map(|row| row.issue.id);
                                    if previous_selection != state.local.selected_issue_id {
                                        state.local.selected_associated_session_id = None;
                                    }
                                    if state.transient.dependency_reconciliation.is_none() {
                                        inspector_selection = state.local.selected_issue_id;
                                    }
                                    state.cache_page(cursor, page.clone());
                                    state.transient.current_page = Some(page);
                                    state.transient.page_direction = None;
                                }
                                IssueWorkspaceAsyncPayload::SelectedIssue {
                                    requested_issue_id,
                                    row,
                                } => {
                                    state.transient.selected_issue_target =
                                        Some(requested_issue_id);
                                    if let Some(row) = row {
                                        let issue = row.issue;
                                        state.transient.missing_selected_issue_id = None;
                                        state.transient.selected_issue = Some(issue.clone());
                                        if let Some(editor) = &mut state.transient.editor
                                            && editor.stale_conflict
                                            && editor_target_issue_id(editor)
                                                == Some(requested_issue_id)
                                        {
                                            let base_version = editor
                                                .base_issue
                                                .as_ref()
                                                .map(|base| base.row_version)
                                                .unwrap_or_default();
                                            if issue.row_version > base_version {
                                                editor.latest_issue = Some(issue.clone());
                                                editor.error = Some(format!(
                                                    "STALE VERSION — Base v{base_version}, Latest v{}, Draft retained; Ctrl-L reload or Ctrl-R rebase",
                                                    issue.row_version
                                                ));
                                            } else {
                                                editor.latest_issue = None;
                                                editor.error = Some(format!(
                                                    "STALE VERSION — Base v{base_version}; matching Latest is not newer yet, refetching"
                                                ));
                                                selected_issue_refetch = Some(requested_issue_id);
                                            }
                                        }
                                    } else {
                                        state.transient.missing_selected_issue_id =
                                            Some(requested_issue_id);
                                        state.transient.selected_issue = None;
                                        state.transient.blocked_by = None;
                                        state.transient.blocked_by_target = None;
                                        state.transient.blocks = None;
                                        state.transient.blocks_target = None;
                                        state.transient.events.clear();
                                        state.transient.events_target = None;
                                        state.transient.events_after_sequence = None;
                                        state.transient.events_next_after_sequence = None;
                                        state.transient.blocked_by_cursor = None;
                                        state.transient.blocks_cursor = None;
                                        state.local.selected_associated_session_id = None;
                                        if let Some(editor) = &mut state.transient.editor
                                            && editor_target_issue_id(editor)
                                                == Some(requested_issue_id)
                                        {
                                            editor.latest_issue = None;
                                            editor.error = Some(
                                            "LATEST UNAVAILABLE — selected Issue no longer exists; refresh Local"
                                                .to_string(),
                                        );
                                        }
                                        let data = state
                                            .data_state_mut(IssueWorkspaceDataKind::SelectedIssue);
                                        data.error = Some(
                                            "SELECTED ISSUE UNAVAILABLE — refresh Local to recover"
                                                .to_string(),
                                        );
                                        data.load_state = IssueWorkspaceLoadState::Error;
                                        data.last_good_ms = None;
                                    }
                                }
                                IssueWorkspaceAsyncPayload::Dependencies {
                                    requested_issue_id,
                                    direction,
                                    cursor,
                                    page,
                                    ..
                                } => match direction {
                                    IssueDependencyDirectionV1::BlockedBy => {
                                        state.transient.blocked_by_target =
                                            Some(requested_issue_id);
                                        state.transient.blocked_by_cursor = cursor;
                                        state.transient.blocked_by = Some(page);
                                    }
                                    IssueDependencyDirectionV1::Blocks => {
                                        state.transient.blocks_target = Some(requested_issue_id);
                                        state.transient.blocks_cursor = cursor;
                                        state.transient.blocks = Some(page);
                                    }
                                },
                                IssueWorkspaceAsyncPayload::Events {
                                    requested_issue_id,
                                    after_sequence,
                                    page,
                                } => {
                                    state.transient.events_target = Some(requested_issue_id);
                                    state.transient.events_after_sequence = after_sequence;
                                    state.transient.events_next_after_sequence =
                                        page.next_after_sequence;
                                    state.transient.events = page.events;
                                }
                                IssueWorkspaceAsyncPayload::DependencyCandidates {
                                    requested_issue_id,
                                    query,
                                    page,
                                } => {
                                    if let Some(editor) = &mut state.transient.editor
                                        && matches!(editor.mode, IssueEditorMode::Dependency { issue_id } if issue_id == requested_issue_id)
                                        && editor.dependency_text == query
                                    {
                                        editor.dependency_candidates = page
                                            .rows
                                            .into_iter()
                                            .filter(|row| row.issue.id != requested_issue_id)
                                            .collect();
                                        editor.dependency_selected_row =
                                            editor.dependency_selected_row.min(
                                                editor
                                                    .dependency_candidates
                                                    .len()
                                                    .saturating_sub(1),
                                            );
                                        editor.dependency_issue_id = editor
                                            .dependency_candidates
                                            .get(editor.dependency_selected_row)
                                            .map(|row| row.issue.id);
                                        editor.error = None;
                                    }
                                }
                                IssueWorkspaceAsyncPayload::AssociatedSession {
                                    requested_session_id,
                                    session,
                                } => {
                                    if session.id == requested_session_id
                                        && match state.active_tab {
                                            IssueWorkspaceTab::Local => {
                                                state.local.selected_associated_session_id
                                                    == Some(requested_session_id)
                                            }
                                            IssueWorkspaceTab::Dispatched => {
                                                state.dispatched.selected_session_id
                                                    == Some(requested_session_id)
                                            }
                                            IssueWorkspaceTab::Sync => false,
                                        }
                                    {
                                        open_fetched_session = Some(session);
                                    }
                                }
                                IssueWorkspaceAsyncPayload::Dispatched(rows) => {
                                    let selected_position = state
                                        .dispatched
                                        .selected_session_id
                                        .and_then(|session_id| {
                                            rows.iter().position(|row| {
                                                row.session_id == session_id
                                                    && state.dispatched.selected_issue_id.as_deref()
                                                        == Some(row.issue_id.as_str())
                                            })
                                        });
                                    state.dispatched.selected_row = selected_position
                                        .unwrap_or_else(|| {
                                            state
                                                .dispatched
                                                .selected_row
                                                .min(rows.len().saturating_sub(1))
                                        });
                                    if let Some(record) = rows.get(state.dispatched.selected_row) {
                                        state.dispatched.selected_session_id =
                                            Some(record.session_id);
                                        state.dispatched.selected_issue_id =
                                            Some(record.issue_id.clone());
                                    } else {
                                        state.dispatched.selected_session_id = None;
                                        state.dispatched.selected_issue_id = None;
                                    }
                                    state.transient.dispatched = rows;
                                }
                                IssueWorkspaceAsyncPayload::SyncStatus(status) => {
                                    state.transient.sync_status = Some(status)
                                }
                                IssueWorkspaceAsyncPayload::ManualPoll(result) => {
                                    state.transient.manual_poll_result = Some(result);
                                    state.transient.manual_poll_error = None;
                                    let invalidated = [
                                        IssueWorkspaceDataKind::SyncStatus,
                                        IssueWorkspaceDataKind::Dispatched,
                                    ];
                                    for kind in invalidated {
                                        let data = state.data_state_mut(kind);
                                        if data.last_good_ms.is_some() {
                                            data.load_state = IssueWorkspaceLoadState::Stale;
                                        }
                                    }
                                    refresh_kinds.extend(invalidated);
                                }
                                IssueWorkspaceAsyncPayload::Mutation(result) => {
                                    state.local.selected_issue_id = Some(result.issue.id);
                                    state.transient.selected_issue_target = Some(result.issue.id);
                                    state.transient.missing_selected_issue_id = None;
                                    state.transient.selected_issue = Some(result.issue.clone());
                                    if state.transient.events_target != Some(result.issue.id) {
                                        state.transient.events.clear();
                                        state.transient.events_target = Some(result.issue.id);
                                    }
                                    if !state
                                        .transient
                                        .events
                                        .iter()
                                        .any(|event| event.id == result.event.id)
                                    {
                                        state.transient.events.push(result.event.clone());
                                        state.transient.events.sort_by_key(|event| event.sequence);
                                    }
                                    if let Some(page) = &mut state.transient.current_page {
                                        if let Some(row) = page
                                            .rows
                                            .iter_mut()
                                            .find(|row| row.issue.id == result.issue.id)
                                        {
                                            row.issue = result.issue.clone();
                                        }
                                    }
                                    state.transient.editor = None;
                                    state.transient.pending_mutation = None;
                                    state.transient.mutation_retry_error = None;
                                    state.transient.cancel_confirmation = None;
                                    state.transient.archive_confirmation = None;
                                    inspector_selection = Some(result.issue.id);
                                    refresh_kinds.push(IssueWorkspaceDataKind::LocalPage);
                                }
                                IssueWorkspaceAsyncPayload::DependencyMutation(_) => {
                                    let pending = state
                                        .transient
                                        .pending_mutation
                                        .as_ref()
                                        .and_then(dependency_mutation_context)
                                        .cloned();
                                    state.transient.editor = None;
                                    state.transient.pending_mutation = None;
                                    state.transient.mutation_retry_error = None;
                                    if let Some(pending) = pending {
                                        begin_dependency_reconciliation(state, &pending);
                                        dependency_reconciliation = Some(pending.origin_issue_id);
                                    }
                                }
                            }
                            if let Some(selected) =
                                finish_dependency_reconciliation_kind(state, event.data_kind)
                            {
                                inspector_selection = Some(selected);
                            }
                        }
                        Err(error) => {
                            let definitive = error.workspace.is_some();
                            let uncertain_dependency = (!definitive
                                && event.data_kind == IssueWorkspaceDataKind::Mutation)
                                .then(|| {
                                    state
                                        .transient
                                        .pending_mutation
                                        .as_ref()
                                        .and_then(dependency_mutation_context)
                                        .cloned()
                                })
                                .flatten();
                            let stale = error.workspace.as_ref().is_some_and(|data| {
                                data.code
                            == rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::StaleVersion
                            });
                            if stale {
                                inspector_selection = state.local.selected_issue_id;
                            }
                            if event.data_kind == IssueWorkspaceDataKind::Mutation
                                && let Some(editor) = &mut state.transient.editor
                            {
                                editor.submitted = false;
                                editor.error = Some(if stale {
                                    editor.stale_conflict = true;
                                    editor.latest_issue = None;
                                    "STALE VERSION — fetching matching Latest; Draft retained and save disabled"
                                .to_string()
                                } else {
                                    error.message.clone()
                                });
                            }
                            if event.data_kind == IssueWorkspaceDataKind::DependencyCandidates
                                && let Some(editor) = &mut state.transient.editor
                            {
                                editor.error =
                                    Some(format!("Candidate query failed — {}", error.message));
                            }
                            if definitive {
                                state.transient.pending_mutation = None;
                                state.transient.mutation_retry_error = None;
                                if event.data_kind == IssueWorkspaceDataKind::Mutation {
                                    state.transient.dependency_reconciliation = None;
                                }
                            } else if event.data_kind == IssueWorkspaceDataKind::Mutation {
                                state.transient.mutation_retry_error = Some(format!(
                                    "ISSUE WRITE OUTCOME UNKNOWN — authoritative readback scheduled; Ctrl-Enter retries identical request: {}",
                                    error.message
                                ));
                            }
                            if let Some(pending) = uncertain_dependency {
                                begin_dependency_reconciliation(state, &pending);
                                dependency_reconciliation = Some(pending.origin_issue_id);
                            }
                            if event.data_kind == IssueWorkspaceDataKind::ManualPoll {
                                state.transient.manual_poll_result = None;
                                state.transient.manual_poll_error = Some(error.message.clone());
                            }
                            let visible_error = if definitive
                                && event.data_kind == IssueWorkspaceDataKind::Mutation
                            {
                                visible_mutation_error(&error)
                            } else {
                                error.message.clone()
                            };
                            let data = state.data_state_mut(event.data_kind);
                            data.error = Some(visible_error.clone());
                            data.load_state = if error.access_denied {
                                IssueWorkspaceLoadState::AccessDenied
                            } else if data.last_good_ms.is_some() {
                                IssueWorkspaceLoadState::Stale
                            } else {
                                IssueWorkspaceLoadState::Error
                            };
                            if event.data_kind == IssueWorkspaceDataKind::AssociatedSession {
                                notification =
                                    Some(format!("Linked session unavailable — {}", error.message));
                            } else if definitive
                                && event.data_kind == IssueWorkspaceDataKind::Mutation
                            {
                                notification =
                                    Some(format!("ISSUE WRITE FAILED — {visible_error}"));
                                error_notification = true;
                            }
                        }
                    }
                }
            }
        }
        if let Some((kind, identity)) = restart {
            self.restart_issue_workspace_request(tab_index, event.pane_id, kind, identity);
            self.mark_dirty();
            return;
        }
        if let Some(issue_id) = dependency_reconciliation {
            self.request_issue_dependency_reconciliation_in_tab(tab_index, event.pane_id, issue_id);
        }
        if let Some(issue_id) = inspector_selection {
            if tab_index == self.active_tab {
                if let Some(Pane::Issues(state)) =
                    self.tabs[tab_index].layout.find_pane_mut(event.pane_id)
                {
                    state.transient.deferred_inspector_target = None;
                }
                self.reconcile_associated_session_selection(event.pane_id, issue_id);
                self.request_issue_inspector(event.pane_id, issue_id);
            } else if let Some(Pane::Issues(state)) =
                self.tabs[tab_index].layout.find_pane_mut(event.pane_id)
            {
                state.transient.deferred_inspector_target = Some(issue_id);
            }
        }
        if tab_index == self.active_tab
            && let Some(issue_id) = selected_issue_refetch
        {
            self.request_selected_issue(event.pane_id, issue_id);
        }
        if tab_index == self.active_tab {
            for kind in refresh_kinds {
                self.request_issue_workspace_kind(event.pane_id, kind);
            }
        }
        if let Some(session) = open_fetched_session {
            let session_id = session.id;
            self.upsert_session(session);
            self.open_session_in_current_pane(session_id);
        }
        if let Some(message) = notification {
            if error_notification {
                self.notify_error(message);
            } else {
                self.notify(message);
            }
        }
        self.mark_dirty();
    }

    fn restart_issue_workspace_request(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        kind: IssueWorkspaceDataKind,
        identity: IssueWorkspaceRequestIdentity,
    ) {
        match identity {
            IssueWorkspaceRequestIdentity::Local(_) => self.request_issue_workspace_kind_in_tab(
                tab_index,
                pane_id,
                IssueWorkspaceDataKind::LocalPage,
            ),
            IssueWorkspaceRequestIdentity::Issue(issue_id) => {
                self.request_selected_issue_in_tab(tab_index, pane_id, issue_id)
            }
            IssueWorkspaceRequestIdentity::Dependency {
                issue_id,
                direction,
                page_direction,
                cursor,
            } => self.request_issue_dependency_page_in_tab(
                tab_index,
                pane_id,
                issue_id,
                direction,
                page_direction,
                cursor,
            ),
            IssueWorkspaceRequestIdentity::Events {
                issue_id,
                after_sequence,
            } => self.request_issue_event_page_in_tab(tab_index, pane_id, issue_id, after_sequence),
            IssueWorkspaceRequestIdentity::DependencyCandidates { issue_id, query } => {
                self.request_issue_dependency_candidates_in_tab(tab_index, pane_id, issue_id, query)
            }
            IssueWorkspaceRequestIdentity::AssociatedSession(session_id) => {
                self.request_issue_associated_session_in_tab(tab_index, pane_id, session_id)
            }
            IssueWorkspaceRequestIdentity::Dispatched => self.request_issue_workspace_kind_in_tab(
                tab_index,
                pane_id,
                IssueWorkspaceDataKind::Dispatched,
            ),
            IssueWorkspaceRequestIdentity::SyncStatus => self.request_issue_workspace_kind_in_tab(
                tab_index,
                pane_id,
                IssueWorkspaceDataKind::SyncStatus,
            ),
            IssueWorkspaceRequestIdentity::ManualPoll => self.request_issue_workspace_kind_in_tab(
                tab_index,
                pane_id,
                IssueWorkspaceDataKind::ManualPoll,
            ),
            IssueWorkspaceRequestIdentity::Mutation(_) => {
                let mutation = match self.tabs[tab_index].layout.find_pane(pane_id) {
                    Some(Pane::Issues(state)) => state.transient.pending_mutation.clone(),
                    _ => None,
                };
                if let Some(mutation) = mutation {
                    self.queue_issue_mutation_in_tab(tab_index, pane_id, mutation);
                }
            }
        }
        let _ = kind;
    }
}

fn editor_target_issue_id(editor: &IssueEditorState) -> Option<Uuid> {
    match editor.mode {
        IssueEditorMode::Edit { issue_id, .. }
        | IssueEditorMode::Status { issue_id, .. }
        | IssueEditorMode::Dependency { issue_id } => Some(issue_id),
        IssueEditorMode::Create | IssueEditorMode::Filter => None,
    }
}

fn current_request_identity(
    state: &IssueWorkspaceState,
    kind: IssueWorkspaceDataKind,
) -> Option<IssueWorkspaceRequestIdentity> {
    match kind {
        IssueWorkspaceDataKind::LocalPage => state.project_id.map(|project_id| {
            IssueWorkspaceRequestIdentity::Local(local_request_identity(project_id, state))
        }),
        IssueWorkspaceDataKind::SelectedIssue => dependency_reconciliation_target(state, kind)
            .or(state.local.selected_issue_id)
            .map(IssueWorkspaceRequestIdentity::Issue),
        IssueWorkspaceDataKind::BlockedBy => dependency_reconciliation_target(state, kind)
            .or(state.local.selected_issue_id)
            .and_then(|issue_id| {
                (dependency_reconciliation_target(state, kind).is_some()
                    || state.transient.missing_selected_issue_id != Some(issue_id))
                .then(|| {
                state
                    .request_identity(kind)
                    .filter(|identity| {
                        matches!(identity, IssueWorkspaceRequestIdentity::Dependency { issue_id: target, direction: IssueDependencyDirectionV1::BlockedBy, .. } if *target == issue_id)
                    })
                    .cloned()
                    .unwrap_or(IssueWorkspaceRequestIdentity::Dependency {
                    issue_id,
                    direction: IssueDependencyDirectionV1::BlockedBy,
                    page_direction: PageDirectionV1::Forward,
                    cursor: None,
                })
                })
            }),
        IssueWorkspaceDataKind::Blocks => dependency_reconciliation_target(state, kind)
            .or(state.local.selected_issue_id)
            .and_then(|issue_id| {
                (dependency_reconciliation_target(state, kind).is_some()
                    || state.transient.missing_selected_issue_id != Some(issue_id))
                .then(|| {
                state
                    .request_identity(kind)
                    .filter(|identity| {
                        matches!(identity, IssueWorkspaceRequestIdentity::Dependency { issue_id: target, direction: IssueDependencyDirectionV1::Blocks, .. } if *target == issue_id)
                    })
                    .cloned()
                    .unwrap_or(IssueWorkspaceRequestIdentity::Dependency {
                    issue_id,
                    direction: IssueDependencyDirectionV1::Blocks,
                    page_direction: PageDirectionV1::Forward,
                    cursor: None,
                })
                })
            }),
        IssueWorkspaceDataKind::Events => dependency_reconciliation_target(state, kind)
            .or(state.local.selected_issue_id)
            .and_then(|issue_id| {
                (dependency_reconciliation_target(state, kind).is_some()
                    || state.transient.missing_selected_issue_id != Some(issue_id))
                .then(|| {
                state
                    .request_identity(kind)
                    .filter(|identity| {
                        matches!(identity, IssueWorkspaceRequestIdentity::Events { issue_id: target, .. } if *target == issue_id)
                    })
                    .cloned()
                    .unwrap_or(IssueWorkspaceRequestIdentity::Events {
                    issue_id,
                    after_sequence: None,
                })
                })
            }),
        IssueWorkspaceDataKind::DependencyCandidates => {
            state
                .transient
                .editor
                .as_ref()
                .and_then(|editor| match editor.mode {
                    IssueEditorMode::Dependency { issue_id } => {
                        Some(IssueWorkspaceRequestIdentity::DependencyCandidates {
                            issue_id,
                            query: editor.dependency_text.clone(),
                        })
                    }
                    _ => None,
                })
        }
        IssueWorkspaceDataKind::AssociatedSession => match state.active_tab {
            IssueWorkspaceTab::Local => state.local.selected_associated_session_id,
            IssueWorkspaceTab::Dispatched => state.dispatched.selected_session_id,
            IssueWorkspaceTab::Sync => None,
        }
        .map(IssueWorkspaceRequestIdentity::AssociatedSession),
        IssueWorkspaceDataKind::Dispatched => Some(IssueWorkspaceRequestIdentity::Dispatched),
        IssueWorkspaceDataKind::SyncStatus => Some(IssueWorkspaceRequestIdentity::SyncStatus),
        IssueWorkspaceDataKind::ManualPoll => Some(IssueWorkspaceRequestIdentity::ManualPoll),
        IssueWorkspaceDataKind::Mutation => {
            state.transient.pending_mutation.as_ref().map(|mutation| {
                IssueWorkspaceRequestIdentity::Mutation(mutation_identity(mutation))
            })
        }
    }
}

fn payload_matches_identity(
    kind: IssueWorkspaceDataKind,
    payload: &IssueWorkspaceAsyncPayload,
    identity: &IssueWorkspaceRequestIdentity,
) -> bool {
    match (payload, identity) {
        (
            IssueWorkspaceAsyncPayload::LocalPage {
                cursor, direction, ..
            },
            IssueWorkspaceRequestIdentity::Local(expected),
        ) => {
            kind == IssueWorkspaceDataKind::LocalPage
                && cursor == &expected.cursor
                && direction == &expected.direction
        }
        (IssueWorkspaceAsyncPayload::Dispatched(_), IssueWorkspaceRequestIdentity::Dispatched)
            if kind == IssueWorkspaceDataKind::Dispatched =>
        {
            true
        }
        (IssueWorkspaceAsyncPayload::SyncStatus(_), IssueWorkspaceRequestIdentity::SyncStatus)
            if kind == IssueWorkspaceDataKind::SyncStatus =>
        {
            true
        }
        (IssueWorkspaceAsyncPayload::ManualPoll(_), IssueWorkspaceRequestIdentity::ManualPoll)
            if kind == IssueWorkspaceDataKind::ManualPoll =>
        {
            true
        }
        (IssueWorkspaceAsyncPayload::Mutation(_), IssueWorkspaceRequestIdentity::Mutation(_))
        | (
            IssueWorkspaceAsyncPayload::DependencyMutation(_),
            IssueWorkspaceRequestIdentity::Mutation(_),
        ) if kind == IssueWorkspaceDataKind::Mutation => true,
        (
            IssueWorkspaceAsyncPayload::SelectedIssue {
                requested_issue_id,
                row,
            },
            IssueWorkspaceRequestIdentity::Issue(expected),
        ) => {
            kind == IssueWorkspaceDataKind::SelectedIssue
                && requested_issue_id == expected
                && row.as_ref().is_none_or(|row| row.issue.id == *expected)
        }
        (
            IssueWorkspaceAsyncPayload::Dependencies {
                requested_issue_id,
                direction,
                page_direction,
                cursor,
                ..
            },
            IssueWorkspaceRequestIdentity::Dependency {
                issue_id,
                direction: expected_direction,
                page_direction: expected_page_direction,
                cursor: expected_cursor,
            },
        ) => {
            requested_issue_id == issue_id
                && direction == expected_direction
                && page_direction == expected_page_direction
                && cursor == expected_cursor
                && matches!(
                    (kind, direction),
                    (
                        IssueWorkspaceDataKind::BlockedBy,
                        IssueDependencyDirectionV1::BlockedBy
                    ) | (
                        IssueWorkspaceDataKind::Blocks,
                        IssueDependencyDirectionV1::Blocks
                    )
                )
        }
        (
            IssueWorkspaceAsyncPayload::Events {
                requested_issue_id,
                after_sequence,
                ..
            },
            IssueWorkspaceRequestIdentity::Events {
                issue_id,
                after_sequence: expected_after_sequence,
            },
        ) => {
            kind == IssueWorkspaceDataKind::Events
                && requested_issue_id == issue_id
                && after_sequence == expected_after_sequence
        }
        (
            IssueWorkspaceAsyncPayload::DependencyCandidates {
                requested_issue_id,
                query,
                ..
            },
            IssueWorkspaceRequestIdentity::DependencyCandidates {
                issue_id,
                query: expected_query,
            },
        ) => {
            kind == IssueWorkspaceDataKind::DependencyCandidates
                && requested_issue_id == issue_id
                && query == expected_query
        }
        (
            IssueWorkspaceAsyncPayload::AssociatedSession {
                requested_session_id,
                session,
            },
            IssueWorkspaceRequestIdentity::AssociatedSession(expected),
        ) => {
            kind == IssueWorkspaceDataKind::AssociatedSession
                && requested_session_id == expected
                && session.id == *expected
        }
        _ => false,
    }
}

impl App {
    pub async fn handle_issue_workspace_action(
        &mut self,
        action: crate::action_registry::ActionId,
    ) -> bool {
        use crate::action_registry::ActionId;

        let pane_id = self.active_tab().focused_pane;
        let dismissed_confirmation = if action == ActionId::Close {
            match self.active_tab_mut().layout.find_pane_mut(pane_id) {
                Some(Pane::Issues(state))
                    if state.transient.cancel_confirmation.is_some()
                        || state.transient.archive_confirmation.is_some() =>
                {
                    state.transient.cancel_confirmation = None;
                    state.transient.archive_confirmation = None;
                    true
                }
                _ => false,
            }
        } else {
            false
        };
        if dismissed_confirmation {
            self.mark_dirty();
            return true;
        }
        if action == ActionId::Close {
            let retry_frozen = matches!(
                self.active_tab().layout.find_pane(pane_id),
                Some(Pane::Issues(state))
                    if state.transient.editor.is_some()
                        && state.transient.pending_mutation.is_some()
            );
            if retry_frozen {
                self.set_issue_editor_error(
                    pane_id,
                    "RETRY ENVELOPE FROZEN — Ctrl-Enter retries identical content; form cannot close",
                );
                return true;
            }
        }
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            if action != ActionId::IssueReloadLatest
                && let Some(editor) = &mut state.transient.editor
            {
                editor.discard_armed = false;
            }
            if action != ActionId::IssueCancel {
                state.transient.cancel_confirmation = None;
            }
            if action != ActionId::IssueConfirmArchive {
                state.transient.archive_confirmation = None;
            }
            if !matches!(action, ActionId::IssueCopyUuid | ActionId::IssueCopyNumber) {
                state.transient.pending_yank = false;
            }
            if action != ActionId::IssueJumpStart {
                state.transient.pending_jump = false;
            }
        }
        match action {
            ActionId::MoveDown => self.move_issue_selection(pane_id, 1),
            ActionId::MoveUp => self.move_issue_selection(pane_id, -1),
            ActionId::IssuePreviousTab => self.switch_issue_tab(pane_id, -1),
            ActionId::IssueNextTab => self.switch_issue_tab(pane_id, 1),
            ActionId::IssueRefresh => {
                let inspector_issue = match self.active_tab().layout.find_pane(pane_id) {
                    Some(Pane::Issues(state))
                        if state.active_tab == IssueWorkspaceTab::Local
                            && state.local.inspector_open =>
                    {
                        state.local.selected_issue_id
                    }
                    _ => None,
                };
                self.request_issue_workspace_active(pane_id, false);
                if let Some(issue_id) = inspector_issue {
                    self.request_issue_inspector(pane_id, issue_id);
                }
            }
            ActionId::IssueRunPoll => self.request_issue_manual_poll(pane_id),
            ActionId::IssueSelectFormValue => self.select_issue_editor_value(pane_id),
            ActionId::IssueRetryMutation => {
                let pending = match self.active_tab().layout.find_pane(pane_id) {
                    Some(Pane::Issues(state)) => state.transient.pending_mutation.clone(),
                    _ => None,
                };
                if let Some(pending) = pending {
                    self.queue_issue_mutation(pane_id, pending);
                }
            }
            ActionId::IssueInspectorNextSection => {
                if let Some(Pane::Issues(state)) =
                    self.active_tab_mut().layout.find_pane_mut(pane_id)
                {
                    state.local.inspector_section = match state.local.inspector_section {
                        crate::types::IssueInspectorSection::BlockedBy => {
                            crate::types::IssueInspectorSection::Blocks
                        }
                        crate::types::IssueInspectorSection::Blocks => {
                            crate::types::IssueInspectorSection::Events
                        }
                        crate::types::IssueInspectorSection::Events => {
                            crate::types::IssueInspectorSection::BlockedBy
                        }
                    };
                }
            }
            ActionId::IssueInspectorPreviousPage => {
                self.navigate_issue_inspector_page(pane_id, false)
            }
            ActionId::IssueInspectorNextPage => self.navigate_issue_inspector_page(pane_id, true),
            ActionId::IssueOpenSession => {
                let (
                    tab,
                    session_id,
                    associated_session_id,
                    local_inspector_open,
                    dispatched_inspector_open,
                    focus,
                ) = match self.active_tab().layout.find_pane(pane_id) {
                    Some(Pane::Issues(state)) => (
                        state.active_tab,
                        state.dispatched.selected_session_id,
                        state.local.selected_associated_session_id,
                        state.local.inspector_open,
                        state.dispatched.inspector_open,
                        state.focus,
                    ),
                    _ => return false,
                };
                if tab == IssueWorkspaceTab::Dispatched {
                    if dispatched_inspector_open && focus == IssueWorkspaceFocus::Inspector {
                        if let Some(session_id) = session_id {
                            if self.sessions.contains_key(&session_id) {
                                self.open_session_in_current_pane(session_id);
                            } else {
                                self.request_issue_associated_session(pane_id, session_id);
                            }
                        }
                    } else {
                        if let Some(Pane::Issues(state)) =
                            self.active_tab_mut().layout.find_pane_mut(pane_id)
                        {
                            state.dispatched.inspector_open = true;
                            state.focus = IssueWorkspaceFocus::Inspector;
                        }
                    }
                } else if tab == IssueWorkspaceTab::Local {
                    if local_inspector_open && focus == IssueWorkspaceFocus::Inspector {
                        if let Some(session_id) = associated_session_id {
                            if self.sessions.contains_key(&session_id) {
                                self.open_session_in_current_pane(session_id);
                            } else {
                                self.request_issue_associated_session(pane_id, session_id);
                            }
                        } else {
                            if let Some(Pane::Issues(state)) =
                                self.active_tab_mut().layout.find_pane_mut(pane_id)
                            {
                                state.focus = IssueWorkspaceFocus::Table;
                            }
                        }
                    } else {
                        let issue_id = match self.active_tab().layout.find_pane(pane_id) {
                            Some(Pane::Issues(state)) => state.local.selected_issue_id,
                            _ => None,
                        };
                        if let Some(Pane::Issues(state)) =
                            self.active_tab_mut().layout.find_pane_mut(pane_id)
                        {
                            state.local.inspector_open = true;
                            state.focus = IssueWorkspaceFocus::Inspector;
                        }
                        if let Some(issue_id) = issue_id {
                            self.reconcile_associated_session_selection(pane_id, issue_id);
                        }
                    }
                }
            }
            ActionId::IssueNew => self.open_issue_create_editor(pane_id),
            ActionId::IssueEdit => self.open_issue_edit_editor(pane_id, 0),
            ActionId::IssueStatus => self.open_issue_status_editor(pane_id),
            ActionId::IssueReopen => self.open_issue_reopen_editor(pane_id),
            ActionId::IssuePriority => self.open_issue_edit_editor(pane_id, 2),
            ActionId::IssueAssignee => self.open_issue_edit_editor(pane_id, 3),
            ActionId::IssueLabels => self.open_issue_edit_editor(pane_id, 4),
            ActionId::IssueDependencies => self.open_issue_dependency_editor(pane_id),
            ActionId::IssueReloadLatest => self.reload_issue_editor(pane_id),
            ActionId::IssueRebaseDraft => self.rebase_issue_editor(pane_id),
            ActionId::IssueArmCancel => self.arm_issue_cancel(pane_id),
            ActionId::IssueCancel => self.submit_issue_cancel(pane_id),
            ActionId::IssueArchive => self.arm_issue_archive(pane_id),
            ActionId::IssueConfirmArchive => self.submit_archive_confirmation(pane_id),
            ActionId::IssueRestore => self.submit_restore(pane_id),
            ActionId::IssueYankArm => {
                if let Some(Pane::Issues(state)) =
                    self.active_tab_mut().layout.find_pane_mut(pane_id)
                {
                    state.transient.pending_yank = true;
                    state.transient.cancel_confirmation = None;
                }
            }
            ActionId::IssueCopyUuid => self.copy_selected_issue(pane_id, false),
            ActionId::IssueCopyNumber => self.copy_selected_issue(pane_id, true),
            ActionId::IssueSearch => self.open_issue_filter_editor(pane_id, 1),
            ActionId::IssueFilter => self.open_issue_filter_editor(pane_id, 0),
            ActionId::IssueSort => {
                if let Some(Pane::Issues(state)) =
                    self.active_tab_mut().layout.find_pane_mut(pane_id)
                {
                    state.local.filters.sort = match state.local.filters.sort {
                        rsi_common::issue_workspace::IssueWorkspaceSortV1::UpdatedDesc => {
                            rsi_common::issue_workspace::IssueWorkspaceSortV1::DisplayNumberAsc
                        }
                        rsi_common::issue_workspace::IssueWorkspaceSortV1::DisplayNumberAsc => {
                            rsi_common::issue_workspace::IssueWorkspaceSortV1::PriorityAsc
                        }
                        rsi_common::issue_workspace::IssueWorkspaceSortV1::PriorityAsc => {
                            rsi_common::issue_workspace::IssueWorkspaceSortV1::UpdatedDesc
                        }
                    };
                    state.local.saved_view = None;
                    reset_local_pagination(state);
                }
                self.request_issue_workspace_active(pane_id, false);
            }
            ActionId::IssueJumpArm => {
                if let Some(Pane::Issues(state)) =
                    self.active_tab_mut().layout.find_pane_mut(pane_id)
                {
                    state.transient.pending_jump = true;
                    state.transient.pending_yank = false;
                    state.transient.cancel_confirmation = None;
                }
            }
            ActionId::IssueJumpStart => {
                if let Some(Pane::Issues(state)) =
                    self.active_tab_mut().layout.find_pane_mut(pane_id)
                {
                    state.local.page_anchor = IssueWorkspacePageAnchor::Start;
                    state.local.selected_row = 0;
                    state.transient.page_direction = Some(PageDirectionV1::Forward);
                    state.transient.pending_jump = false;
                }
                self.request_issue_workspace_active(pane_id, false);
            }
            ActionId::IssueJumpEnd => {
                if let Some(Pane::Issues(state)) =
                    self.active_tab_mut().layout.find_pane_mut(pane_id)
                {
                    state.local.page_anchor = IssueWorkspacePageAnchor::End;
                    state.local.selected_row = usize::MAX;
                    state.transient.page_direction = Some(PageDirectionV1::Backward);
                    state.transient.pending_jump = false;
                }
                self.request_issue_workspace_active(pane_id, false);
            }
            ActionId::Close => {
                let inspector_open = match self.active_tab().layout.find_pane(pane_id) {
                    Some(Pane::Issues(state)) => match state.active_tab {
                        IssueWorkspaceTab::Local => state.local.inspector_open,
                        IssueWorkspaceTab::Dispatched => state.dispatched.inspector_open,
                        IssueWorkspaceTab::Sync => false,
                    },
                    _ => false,
                };
                if inspector_open {
                    if let Some(Pane::Issues(state)) =
                        self.active_tab_mut().layout.find_pane_mut(pane_id)
                    {
                        match state.active_tab {
                            IssueWorkspaceTab::Local => state.local.inspector_open = false,
                            IssueWorkspaceTab::Dispatched => {
                                state.dispatched.inspector_open = false
                            }
                            IssueWorkspaceTab::Sync => {}
                        }
                        state.focus = IssueWorkspaceFocus::Table;
                        state.transient.cancel_confirmation = None;
                        state.transient.archive_confirmation = None;
                    }
                } else {
                    self.restore_pre_issues_pane();
                }
            }
            _ => return false,
        }
        self.mark_dirty();
        true
    }

    pub async fn handle_issue_workspace_editor_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        let pane_id = self.active_tab().focused_pane;
        let is_editor = matches!(
            self.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state)) if state.transient.editor.is_some()
        );
        if !is_editor {
            return false;
        }
        if key.code == KeyCode::Char('?') && key.modifiers == KeyModifiers::NONE {
            if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id)
                && let Some(editor) = &mut state.transient.editor
            {
                clear_editor_discard_arm(editor);
            }
            return false;
        }
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('l') | KeyCode::Char('r'))
        {
            return false;
        }
        if key.code != KeyCode::Esc
            && let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            clear_editor_discard_arm(editor);
        }
        let retry_pending = match self.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state.transient.pending_mutation.clone(),
            _ => None,
        };
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
            let stale_conflict = matches!(
                self.active_tab().layout.find_pane(pane_id),
                Some(Pane::Issues(state))
                    if state.transient.editor.as_ref().is_some_and(|editor| editor.stale_conflict)
            );
            if stale_conflict {
                self.set_issue_editor_error(
                    pane_id,
                    "SAVE DISABLED — choose Ctrl-L Reload latest or Ctrl-R Rebase draft",
                );
                return true;
            }
            if let Some(pending) = retry_pending {
                self.queue_issue_mutation(pane_id, pending);
            } else {
                self.submit_issue_editor(pane_id, false);
            }
            return true;
        }
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(pending) = retry_pending {
                self.queue_issue_mutation(pane_id, pending);
            } else {
                self.submit_issue_editor(pane_id, true);
            }
            return true;
        }
        if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
            return false;
        }
        let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) else {
            return false;
        };
        let retry_frozen = state.transient.pending_mutation.is_some();
        let Some(editor) = &mut state.transient.editor else {
            return false;
        };
        if !matches!(key.code, KeyCode::Esc) {
            clear_editor_discard_arm(editor);
        }
        if retry_frozen {
            editor.error = Some(
                "RETRY ENVELOPE FROZEN — Ctrl-Enter retries identical content; form cannot close"
                    .to_string(),
            );
            return true;
        }
        match key.code {
            KeyCode::Esc => {
                if editor.dirty && !editor.discard_armed {
                    editor.discard_armed = true;
                    editor.error = Some("UNSAVED CHANGES — Esc again to discard".to_string());
                } else {
                    state.transient.editor = None;
                }
            }
            KeyCode::Tab => {
                sync_editor_derived(editor);
                editor.active_field = (editor.active_field + 1) % editor_field_count(editor);
                load_editor_scratch(editor);
                editor.discard_armed = false;
            }
            KeyCode::BackTab => {
                sync_editor_derived(editor);
                let count = editor_field_count(editor);
                editor.active_field = (editor.active_field + count - 1) % count;
                load_editor_scratch(editor);
                editor.discard_armed = false;
            }
            KeyCode::Up | KeyCode::Down if matches!(editor.mode, IssueEditorMode::Filter) => {
                cycle_filter_choice(editor, key.code == KeyCode::Down);
                editor.dirty = true;
                load_editor_scratch(editor);
            }
            KeyCode::Up | KeyCode::Down
                if matches!(editor.mode, IssueEditorMode::Dependency { .. }) =>
            {
                if editor.active_field == 0 {
                    editor.dependency_direction = match editor.dependency_direction {
                        IssueDependencyDirectionV1::BlockedBy => IssueDependencyDirectionV1::Blocks,
                        IssueDependencyDirectionV1::Blocks => IssueDependencyDirectionV1::BlockedBy,
                    };
                } else if editor.active_field == 2 && !editor.dependency_candidates.is_empty() {
                    editor.dependency_selected_row = if key.code == KeyCode::Down {
                        (editor.dependency_selected_row + 1)
                            .min(editor.dependency_candidates.len() - 1)
                    } else {
                        editor.dependency_selected_row.saturating_sub(1)
                    };
                    editor.dependency_issue_id = editor
                        .dependency_candidates
                        .get(editor.dependency_selected_row)
                        .map(|row| row.issue.id);
                }
                editor.dirty = true;
            }
            KeyCode::Up | KeyCode::Down
                if matches!(editor.mode, IssueEditorMode::Status { .. }) =>
            {
                editor.status = Some(
                    if editor
                        .base_issue
                        .as_ref()
                        .is_some_and(issue_is_terminal_unarchived)
                    {
                        IssueStatus::Open
                    } else {
                        cycle_status(
                            editor.status.unwrap_or(IssueStatus::Open),
                            key.code == KeyCode::Down,
                        )
                    },
                );
                editor.dirty = true;
                editor.discard_armed = false;
            }
            KeyCode::Backspace => {
                if editor_accepts_text(editor) {
                    editor_field_mut(editor).pop();
                    sync_editor_derived(editor);
                    editor.dirty = true;
                    editor.discard_armed = false;
                }
            }
            KeyCode::Char(character)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                if editor_accepts_text(editor) {
                    editor_field_mut(editor).push(character);
                    sync_editor_derived(editor);
                    editor.dirty = true;
                    editor.discard_armed = false;
                    editor.error = None;
                }
            }
            _ => return false,
        }
        let candidate_refresh = state.transient.editor.as_ref().and_then(|editor| {
            if matches!(editor.mode, IssueEditorMode::Dependency { .. })
                && editor.active_field == 1
                && matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace)
            {
                editor_target_issue_id(editor)
                    .map(|issue_id| (issue_id, editor.dependency_text.clone()))
            } else {
                None
            }
        });
        self.mark_dirty();
        if let Some((issue_id, query)) = candidate_refresh {
            self.request_issue_dependency_candidates(pane_id, issue_id, query);
        }
        true
    }

    fn move_issue_selection(&mut self, pane_id: PaneId, delta: isize) {
        let dispatched_inspector = matches!(
            self.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.active_tab == IssueWorkspaceTab::Dispatched
                    && state.dispatched.inspector_open
                    && state.focus == IssueWorkspaceFocus::Inspector
        );
        if dispatched_inspector {
            if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
                state.dispatched.inspector_scroll_offset = if delta > 0 {
                    state.dispatched.inspector_scroll_offset.saturating_add(1)
                } else {
                    state.dispatched.inspector_scroll_offset.saturating_sub(1)
                };
            }
            return;
        }
        let inspector_issue = match self.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state))
                if state.active_tab == IssueWorkspaceTab::Local
                    && state.local.inspector_open
                    && state.focus == IssueWorkspaceFocus::Inspector =>
            {
                state.local.selected_issue_id
            }
            _ => None,
        };
        if let Some(issue_id) = inspector_issue {
            let sessions = associated_session_ids(self, issue_id);
            if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
                state.local.inspector_scroll_offset = if delta > 0 {
                    state.local.inspector_scroll_offset.saturating_add(1)
                } else {
                    state.local.inspector_scroll_offset.saturating_sub(1)
                };
                if !sessions.is_empty() {
                    let current = state
                        .local
                        .selected_associated_session_id
                        .and_then(|id| sessions.iter().position(|candidate| *candidate == id))
                        .unwrap_or_default();
                    let next = if delta > 0 {
                        (current + 1).min(sessions.len() - 1)
                    } else {
                        current.saturating_sub(1)
                    };
                    state.local.selected_associated_session_id = Some(sessions[next]);
                }
            }
            return;
        }
        let mut selected = None;
        let mut adjacent_page = false;
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.cancel_confirmation = None;
            state.transient.archive_confirmation = None;
            state.transient.pending_yank = false;
            state.transient.pending_jump = false;
            match state.active_tab {
                IssueWorkspaceTab::Local => {
                    if let Some(page) = &state.transient.current_page {
                        let len = page.rows.len();
                        if len > 0 {
                            if delta > 0
                                && state.local.selected_row + 1 >= len
                                && let Some(cursor) = page.next_cursor.clone()
                            {
                                state.local.page_anchor = IssueWorkspacePageAnchor::Cursor(cursor);
                                state.transient.page_direction = Some(PageDirectionV1::Forward);
                                adjacent_page = true;
                            } else if delta < 0
                                && state.local.selected_row == 0
                                && let Some(cursor) = page.previous_cursor.clone()
                            {
                                state.local.page_anchor = IssueWorkspacePageAnchor::Cursor(cursor);
                                state.transient.page_direction = Some(PageDirectionV1::Backward);
                                adjacent_page = true;
                            }
                            state.local.selected_row = if delta > 0 {
                                (state.local.selected_row + 1).min(len - 1)
                            } else {
                                state.local.selected_row.saturating_sub(1)
                            };
                            selected = page
                                .rows
                                .get(state.local.selected_row)
                                .map(|row| row.issue.id);
                            state.local.selected_issue_id = selected;
                            state.local.selected_associated_session_id = None;
                        }
                    }
                }
                IssueWorkspaceTab::Dispatched => {
                    let len = state.transient.dispatched.len();
                    if len > 0 {
                        state.dispatched.selected_row = if delta > 0 {
                            (state.dispatched.selected_row + 1).min(len - 1)
                        } else {
                            state.dispatched.selected_row.saturating_sub(1)
                        };
                        if let Some(record) = state
                            .transient
                            .dispatched
                            .get(state.dispatched.selected_row)
                        {
                            state.dispatched.selected_session_id = Some(record.session_id);
                            state.dispatched.selected_issue_id = Some(record.issue_id.clone());
                        }
                    }
                }
                IssueWorkspaceTab::Sync => {
                    state.sync.scroll_offset = if delta > 0 {
                        state.sync.scroll_offset.saturating_add(1)
                    } else {
                        state.sync.scroll_offset.saturating_sub(1)
                    };
                }
            }
        }
        if let Some(issue_id) = selected {
            self.request_issue_inspector(pane_id, issue_id);
        }
        if adjacent_page {
            self.request_issue_workspace_active(pane_id, false);
        }
    }

    fn reconcile_associated_session_selection(&mut self, pane_id: PaneId, issue_id: Uuid) {
        let sessions = associated_session_ids(self, issue_id);
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            let retained = state
                .local
                .selected_associated_session_id
                .filter(|selected| sessions.contains(selected));
            state.local.selected_associated_session_id =
                retained.or_else(|| sessions.first().copied());
        }
    }

    fn switch_issue_tab(&mut self, pane_id: PaneId, delta: isize) {
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            let index = match state.active_tab {
                IssueWorkspaceTab::Local => 0_i8,
                IssueWorkspaceTab::Dispatched => 1,
                IssueWorkspaceTab::Sync => 2,
            };
            let next = (index + delta as i8).rem_euclid(3);
            state.active_tab = match next {
                0 => IssueWorkspaceTab::Local,
                1 => IssueWorkspaceTab::Dispatched,
                _ => IssueWorkspaceTab::Sync,
            };
            state.focus = match state.active_tab {
                IssueWorkspaceTab::Local if state.local.inspector_open => {
                    IssueWorkspaceFocus::Inspector
                }
                IssueWorkspaceTab::Dispatched if state.dispatched.inspector_open => {
                    IssueWorkspaceFocus::Inspector
                }
                _ => IssueWorkspaceFocus::Table,
            };
            state.transient.cancel_confirmation = None;
            state.transient.archive_confirmation = None;
            state.transient.pending_yank = false;
            state.transient.pending_jump = false;
        }
        self.request_issue_workspace_active(pane_id, false);
    }

    fn select_issue_editor_value(&mut self, pane_id: PaneId) {
        let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) else {
            return;
        };
        let Some(editor) = &mut state.transient.editor else {
            return;
        };
        editor.discard_armed = false;
        editor.error = match editor.mode {
            IssueEditorMode::Filter => match editor.active_field {
                0 => Some(format!(
                    "Selected saved view: {}",
                    editor
                        .filter_saved_view
                        .map(saved_view_label)
                        .unwrap_or("Custom")
                )),
                4 => editor.filter_draft.as_ref().map(|filters| {
                    format!(
                        "Selected readiness: {}",
                        readiness_filter_label(filters.readiness)
                    )
                }),
                8 => editor
                    .filter_draft
                    .as_ref()
                    .map(|filters| format!("Selected archive view: {:?}", filters.archive)),
                _ => None,
            },
            IssueEditorMode::Status { .. } => editor
                .status
                .map(|status| format!("Selected status: {}", status.as_str())),
            IssueEditorMode::Dependency { .. } if editor.active_field == 0 => Some(format!(
                "Selected dependency direction: {:?}",
                editor.dependency_direction
            )),
            IssueEditorMode::Dependency { .. } if editor.active_field == 2 => editor
                .dependency_candidates
                .get(editor.dependency_selected_row)
                .map(|row| {
                    editor.dependency_issue_id = Some(row.issue.id);
                    format!(
                        "Selected dependency: #{} {} ({})",
                        row.issue.display_number, row.issue.title, row.issue.id
                    )
                }),
            IssueEditorMode::Create
            | IssueEditorMode::Edit { .. }
            | IssueEditorMode::Dependency { .. } => None,
        };
    }

    fn navigate_issue_inspector_page(&mut self, pane_id: PaneId, forward: bool) {
        let request = match self.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state))
                if state.active_tab == IssueWorkspaceTab::Local
                    && state.local.inspector_open
                    && state.focus == IssueWorkspaceFocus::Inspector =>
            {
                let issue_id = state.local.selected_issue_id;
                issue_id.and_then(|issue_id| match state.local.inspector_section {
                    crate::types::IssueInspectorSection::BlockedBy => {
                        let cursor = (state.transient.blocked_by_target == Some(issue_id))
                            .then(|| state.transient.blocked_by.as_ref())
                            .flatten()
                            .and_then(|page| {
                                if forward {
                                    page.next_cursor.clone()
                                } else {
                                    page.previous_cursor.clone()
                                }
                            });
                        cursor.map(|cursor| {
                            (
                                issue_id,
                                Some(IssueDependencyDirectionV1::BlockedBy),
                                Some(cursor),
                                None,
                            )
                        })
                    }
                    crate::types::IssueInspectorSection::Blocks => {
                        let cursor = (state.transient.blocks_target == Some(issue_id))
                            .then(|| state.transient.blocks.as_ref())
                            .flatten()
                            .and_then(|page| {
                                if forward {
                                    page.next_cursor.clone()
                                } else {
                                    page.previous_cursor.clone()
                                }
                            });
                        cursor.map(|cursor| {
                            (
                                issue_id,
                                Some(IssueDependencyDirectionV1::Blocks),
                                Some(cursor),
                                None,
                            )
                        })
                    }
                    crate::types::IssueInspectorSection::Events
                        if state.transient.events_target == Some(issue_id) && forward =>
                    {
                        state
                            .transient
                            .events_next_after_sequence
                            .map(|after| (issue_id, None, None, Some(after)))
                    }
                    crate::types::IssueInspectorSection::Events
                        if state.transient.events_target == Some(issue_id) =>
                    {
                        state
                            .transient
                            .events_after_sequence
                            .map(|_| (issue_id, None, None, Some(0)))
                    }
                    crate::types::IssueInspectorSection::Events => None,
                })
            }
            _ => None,
        };
        let Some((issue_id, direction, cursor, events_after)) = request else {
            self.notify("No adjacent bounded inspector page");
            return;
        };
        if let Some(direction) = direction {
            self.request_issue_dependency_page(
                pane_id,
                issue_id,
                direction,
                if forward {
                    PageDirectionV1::Forward
                } else {
                    PageDirectionV1::Backward
                },
                cursor,
            );
        } else {
            self.request_issue_event_page(
                pane_id,
                issue_id,
                events_after.filter(|after| *after > 0),
            );
        }
    }

    fn selected_local_issue(&self, pane_id: PaneId) -> Option<Issue> {
        let Some(Pane::Issues(state)) = self.active_tab().layout.find_pane(pane_id) else {
            return None;
        };
        let issue_id = state.local.selected_issue_id?;
        state
            .transient
            .current_page
            .as_ref()?
            .rows
            .iter()
            .find(|row| row.issue.id == issue_id)
            .map(|row| row.issue.clone())
    }

    fn open_issue_create_editor(&mut self, pane_id: PaneId) {
        let editor = IssueEditorState {
            mode: IssueEditorMode::Create,
            title: String::new(),
            body: String::new(),
            priority: None,
            assignee: None,
            labels: Vec::new(),
            status: Some(IssueStatus::Open),
            dependency_issue_id: None,
            dependency_direction: IssueDependencyDirectionV1::BlockedBy,
            dependency_candidates: Vec::new(),
            dependency_selected_row: 0,
            dependency_text: String::new(),
            filter_draft: None,
            filter_saved_view: None,
            filter_mine_assignee: None,
            base_issue: None,
            latest_issue: None,
            stale_conflict: false,
            active_field: 0,
            dirty: false,
            discard_armed: false,
            retry_key: Uuid::new_v4().to_string(),
            error: None,
            submitted: false,
        };
        self.set_issue_editor(pane_id, editor);
    }

    fn open_issue_edit_editor(&mut self, pane_id: PaneId, active_field: usize) {
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        if !issue_is_active_unarchived(&issue) {
            return;
        }
        let field_text = match active_field {
            2 => issue
                .priority
                .map(|value| value.to_string())
                .unwrap_or_default(),
            3 => issue.assignee.clone().unwrap_or_default(),
            4 => issue.labels.join(","),
            _ => String::new(),
        };
        let base_issue = issue.clone();
        let editor = IssueEditorState {
            mode: IssueEditorMode::Edit {
                issue_id: issue.id,
                row_version: issue.row_version,
            },
            title: issue.title,
            body: issue.body,
            priority: issue.priority,
            assignee: issue.assignee,
            labels: issue.labels,
            status: Some(issue.status),
            dependency_issue_id: None,
            dependency_direction: IssueDependencyDirectionV1::BlockedBy,
            dependency_candidates: Vec::new(),
            dependency_selected_row: 0,
            dependency_text: field_text,
            filter_draft: None,
            filter_saved_view: None,
            filter_mine_assignee: None,
            base_issue: Some(base_issue),
            latest_issue: None,
            stale_conflict: false,
            active_field,
            dirty: false,
            discard_armed: false,
            retry_key: Uuid::new_v4().to_string(),
            error: None,
            submitted: false,
        };
        self.set_issue_editor(pane_id, editor);
    }

    fn open_issue_status_editor(&mut self, pane_id: PaneId) {
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        if !issue_is_active_unarchived(&issue) {
            return;
        }
        self.open_issue_status_editor_for(pane_id, issue, None);
    }

    fn open_issue_reopen_editor(&mut self, pane_id: PaneId) {
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        if !issue_is_terminal_unarchived(&issue) {
            return;
        }
        self.open_issue_status_editor_for(pane_id, issue, Some(IssueStatus::Open));
    }

    fn open_issue_status_editor_for(
        &mut self,
        pane_id: PaneId,
        issue: Issue,
        selected_status: Option<IssueStatus>,
    ) {
        let base_issue = issue.clone();
        let editor = IssueEditorState {
            mode: IssueEditorMode::Status {
                issue_id: issue.id,
                row_version: issue.row_version,
            },
            title: issue.title,
            body: issue.body,
            priority: issue.priority,
            assignee: issue.assignee,
            labels: issue.labels,
            status: selected_status.or(Some(issue.status)),
            dependency_issue_id: None,
            dependency_direction: IssueDependencyDirectionV1::BlockedBy,
            dependency_candidates: Vec::new(),
            dependency_selected_row: 0,
            dependency_text: String::new(),
            filter_draft: None,
            filter_saved_view: None,
            filter_mine_assignee: None,
            base_issue: Some(base_issue),
            latest_issue: None,
            stale_conflict: false,
            active_field: 0,
            dirty: false,
            discard_armed: false,
            retry_key: Uuid::new_v4().to_string(),
            error: Some(if selected_status.is_some() {
                "Reopen selected — Ctrl-Enter submits Open".to_string()
            } else {
                "Use ↑/↓ to choose Open, In progress, Closed, or Cancelled".to_string()
            }),
            submitted: false,
        };
        self.set_issue_editor(pane_id, editor);
    }

    fn open_issue_dependency_editor(&mut self, pane_id: PaneId) {
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        let issue_id = issue.id;
        let base_issue = issue.clone();
        let editor = IssueEditorState {
            mode: IssueEditorMode::Dependency { issue_id: issue.id },
            title: issue.title,
            body: issue.body,
            priority: issue.priority,
            assignee: issue.assignee,
            labels: issue.labels,
            status: Some(issue.status),
            dependency_issue_id: None,
            dependency_direction: IssueDependencyDirectionV1::BlockedBy,
            dependency_candidates: Vec::new(),
            dependency_selected_row: 0,
            dependency_text: String::new(),
            filter_draft: None,
            filter_saved_view: None,
            filter_mine_assignee: None,
            base_issue: Some(base_issue),
            latest_issue: None,
            stale_conflict: false,
            active_field: 0,
            dirty: false,
            discard_armed: false,
            retry_key: String::new(),
            error: Some(
                "Choose Blocked by/Blocks, search a bounded candidate page, then add/remove"
                    .to_string(),
            ),
            submitted: false,
        };
        self.set_issue_editor(pane_id, editor);
        self.request_issue_dependency_candidates(pane_id, issue_id, String::new());
    }

    fn open_issue_filter_editor(&mut self, pane_id: PaneId, active_field: usize) {
        let (filters, saved_view, mine_assignee) = match self.active_tab().layout.find_pane(pane_id)
        {
            Some(Pane::Issues(state)) => (
                state.local.filters.clone(),
                state.local.saved_view,
                state.local.mine_assignee.clone(),
            ),
            _ => return,
        };
        let editor = IssueEditorState {
            mode: IssueEditorMode::Filter,
            title: filters.query.clone().unwrap_or_default(),
            body: String::new(),
            priority: None,
            assignee: None,
            labels: Vec::new(),
            status: None,
            dependency_issue_id: None,
            dependency_direction: IssueDependencyDirectionV1::BlockedBy,
            dependency_candidates: Vec::new(),
            dependency_selected_row: 0,
            dependency_text: String::new(),
            filter_draft: Some(filters),
            filter_saved_view: saved_view,
            filter_mine_assignee: mine_assignee,
            base_issue: None,
            latest_issue: None,
            stale_conflict: false,
            active_field,
            dirty: false,
            discard_armed: false,
            retry_key: String::new(),
            error: Some("Bounded filters · ↑/↓ choices · Ctrl-Enter applies".to_string()),
            submitted: false,
        };
        let mut editor = editor;
        load_editor_scratch(&mut editor);
        self.set_issue_editor(pane_id, editor);
    }

    fn set_issue_editor(&mut self, pane_id: PaneId, editor: IssueEditorState) {
        let retry_frozen = matches!(
            self.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state)) if state.transient.pending_mutation.is_some()
        );
        if retry_frozen {
            if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id)
                && let Some(current) = &mut state.transient.editor
            {
                current.error = Some(
                    "RETRY ENVELOPE FROZEN — Ctrl-Enter retries identical content; form cannot be replaced"
                        .to_string(),
                );
            }
            self.notify("Retry the frozen Issue request before opening another form");
            return;
        }
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.editor = Some(editor);
            state.transient.cancel_confirmation = None;
            state.transient.archive_confirmation = None;
            state.transient.pending_yank = false;
        }
    }

    fn arm_issue_cancel(&mut self, pane_id: PaneId) {
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        if !issue_is_active_unarchived(&issue) {
            return;
        }
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.cancel_confirmation = Some(IssueCancelConfirmation {
                issue_id: issue.id,
                display_number: issue.display_number,
                armed_at_ms: now_ms(),
            });
            state.transient.pending_yank = false;
        }
        self.notify(format!("CANCEL #{} — press d again", issue.display_number));
    }

    fn submit_issue_cancel(&mut self, pane_id: PaneId) {
        let confirmation = match self.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state.transient.cancel_confirmation.clone(),
            _ => None,
        };
        let Some(confirmation) = confirmation else {
            return;
        };
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        if !issue_is_active_unarchived(&issue)
            || issue.id != confirmation.issue_id
            || now_ms().saturating_sub(confirmation.armed_at_ms) > 2_000
        {
            return;
        }
        self.queue_issue_mutation(
            pane_id,
            IssuePendingMutation::Status(UpdateIssueStatusV2RequestV1 {
                project_id: issue.project_id,
                issue_id: issue.id,
                expected_row_version: issue.row_version,
                idempotency_key: Uuid::new_v4().to_string(),
                status: IssueStatus::Cancelled,
            }),
        );
    }

    fn arm_issue_archive(&mut self, pane_id: PaneId) {
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        if !issue_is_terminal_unarchived(&issue) {
            return;
        }
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.archive_confirmation = Some(IssueCancelConfirmation {
                issue_id: issue.id,
                display_number: issue.display_number,
                armed_at_ms: now_ms(),
            });
            state.transient.cancel_confirmation = None;
        }
        self.notify(format!(
            "ARCHIVE #{} — press Enter to confirm",
            issue.display_number
        ));
    }

    fn submit_archive_confirmation(&mut self, pane_id: PaneId) {
        let confirmation = match self.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state.transient.archive_confirmation.clone(),
            _ => None,
        };
        let Some(confirmation) = confirmation else {
            return;
        };
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        if !issue_is_terminal_unarchived(&issue) || issue.id != confirmation.issue_id {
            return;
        }
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.archive_confirmation = None;
        }
        self.queue_issue_mutation(
            pane_id,
            IssuePendingMutation::Archive(ArchiveIssueRequestV1 {
                project_id: issue.project_id,
                issue_id: issue.id,
                expected_row_version: issue.row_version,
                idempotency_key: Uuid::new_v4().to_string(),
            }),
        );
    }

    fn submit_restore(&mut self, pane_id: PaneId) {
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        if !issue_is_terminal_archived(&issue) {
            return;
        }
        self.queue_issue_mutation(
            pane_id,
            IssuePendingMutation::Restore(RestoreIssueRequestV1 {
                project_id: issue.project_id,
                issue_id: issue.id,
                expected_row_version: issue.row_version,
                idempotency_key: Uuid::new_v4().to_string(),
            }),
        );
    }

    fn copy_selected_issue(&mut self, pane_id: PaneId, display_number: bool) {
        let Some(issue) = self.selected_local_issue(pane_id) else {
            return;
        };
        let (payload, toast) = issue_clipboard_payload(&issue, display_number);
        crate::clipboard::osc52_copy(&payload);
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.pending_yank = false;
        }
        self.notify_success(toast);
    }

    fn submit_issue_editor(&mut self, pane_id: PaneId, remove_dependency: bool) {
        let (project_id, editor) = match self.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => (state.project_id, state.transient.editor.clone()),
            _ => return,
        };
        let Some(project_id) = project_id else {
            return;
        };
        let Some(editor) = editor else {
            return;
        };
        if editor.stale_conflict {
            self.set_issue_editor_error(
                pane_id,
                "SAVE DISABLED — choose Ctrl-L Reload latest or Ctrl-R Rebase draft",
            );
            return;
        }
        let mutation = match editor.mode {
            IssueEditorMode::Create => {
                if editor.title.trim().is_empty() {
                    self.set_issue_editor_error(pane_id, "Title must not be blank");
                    return;
                }
                IssuePendingMutation::Create(CreateIssueV2RequestV1 {
                    project_id,
                    title: editor.title,
                    body: editor.body,
                    priority: editor.priority,
                    labels: editor.labels,
                    assignee: editor.assignee,
                    idempotency_key: editor.retry_key,
                })
            }
            IssueEditorMode::Edit {
                issue_id,
                row_version,
            } => IssuePendingMutation::Update(UpdateIssueRequestV1 {
                project_id,
                issue_id,
                expected_row_version: row_version,
                idempotency_key: editor.retry_key,
                patch: IssueContentPatchV2 {
                    title: Some(editor.title),
                    body: Some(editor.body),
                    labels: Some(editor.labels),
                    priority: editor
                        .priority
                        .map(NullablePatchV1::Set)
                        .unwrap_or(NullablePatchV1::Clear),
                    assignee: editor
                        .assignee
                        .map(NullablePatchV1::Set)
                        .unwrap_or(NullablePatchV1::Clear),
                },
            }),
            IssueEditorMode::Status {
                issue_id,
                row_version,
            } => IssuePendingMutation::Status(UpdateIssueStatusV2RequestV1 {
                project_id,
                issue_id,
                expected_row_version: row_version,
                idempotency_key: editor.retry_key,
                status: editor.status.unwrap_or(IssueStatus::Open),
            }),
            IssueEditorMode::Dependency { issue_id } => {
                let Some(candidate_id) = editor.dependency_issue_id else {
                    self.set_issue_editor_error(pane_id, "Select a bounded dependency candidate");
                    return;
                };
                let request = match editor.dependency_direction {
                    IssueDependencyDirectionV1::BlockedBy => IssueDependencyMutationRequestV1 {
                        project_id,
                        issue_id,
                        depends_on_id: candidate_id,
                    },
                    IssueDependencyDirectionV1::Blocks => IssueDependencyMutationRequestV1 {
                        project_id,
                        issue_id: candidate_id,
                        depends_on_id: issue_id,
                    },
                };
                let pending = IssuePendingDependencyMutation {
                    request,
                    origin_issue_id: issue_id,
                    direction: editor.dependency_direction,
                };
                if remove_dependency {
                    IssuePendingMutation::RemoveDependency(pending)
                } else {
                    IssuePendingMutation::AddDependency(pending)
                }
            }
            IssueEditorMode::Filter => {
                let Some(filters) = editor.filter_draft else {
                    return;
                };
                if let Some(Pane::Issues(state)) =
                    self.active_tab_mut().layout.find_pane_mut(pane_id)
                {
                    state.local.filters = filters;
                    state.local.saved_view = editor.filter_saved_view;
                    state.local.mine_assignee = editor.filter_mine_assignee;
                    reset_local_pagination(state);
                    state.transient.editor = None;
                    state.focus = IssueWorkspaceFocus::Table;
                }
                self.request_issue_workspace_active(pane_id, false);
                return;
            }
        };
        self.queue_issue_mutation(pane_id, mutation);
    }

    fn set_issue_editor_error(&mut self, pane_id: PaneId, message: &str) {
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.error = Some(message.to_string());
            editor.submitted = false;
        }
    }

    fn rebase_issue_editor(&mut self, pane_id: PaneId) {
        let latest = match self.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state
                .transient
                .editor
                .as_ref()
                .and_then(|editor| editor.latest_issue.clone()),
            _ => None,
        };
        let Some(latest) = latest else {
            self.set_issue_editor_error(pane_id, "Latest projection is still loading");
            return;
        };
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            match &mut editor.mode {
                IssueEditorMode::Edit {
                    issue_id,
                    row_version,
                }
                | IssueEditorMode::Status {
                    issue_id,
                    row_version,
                } if editor.stale_conflict
                    && *issue_id == latest.id
                    && editor
                        .base_issue
                        .as_ref()
                        .is_some_and(|base| latest.row_version > base.row_version) =>
                {
                    *row_version = latest.row_version;
                    editor.retry_key = Uuid::new_v4().to_string();
                    editor.submitted = false;
                    editor.stale_conflict = false;
                    editor.discard_armed = false;
                    editor.error = Some(format!(
                        "DRAFT REBASED — Base v{}, Latest v{}, Draft retained; save enabled",
                        editor
                            .base_issue
                            .as_ref()
                            .map(|base| base.row_version)
                            .unwrap_or_default(),
                        latest.row_version,
                    ));
                }
                _ => {
                    editor.error = Some(
                        "REBASE DISABLED — wait for a newer matching Latest projection".to_string(),
                    )
                }
            }
        }
    }

    fn reload_issue_editor(&mut self, pane_id: PaneId) {
        let needs_confirmation = matches!(
            self.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.transient.editor.as_ref().is_some_and(|editor| editor.dirty && !editor.discard_armed)
        );
        if needs_confirmation {
            if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id)
                && let Some(editor) = &mut state.transient.editor
            {
                editor.discard_armed = true;
                editor.error =
                    Some("UNSAVED CHANGES — press Ctrl-L again to reload Latest".to_string());
            }
            return;
        }
        let latest = match self.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state
                .transient
                .editor
                .as_ref()
                .and_then(|editor| editor.latest_issue.clone()),
            _ => None,
        };
        let Some(latest) = latest else {
            self.set_issue_editor_error(pane_id, "Latest projection is still loading");
            return;
        };
        let mut message = None;
        if let Some(Pane::Issues(state)) = self.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.editor = None;
            state.transient.pending_mutation = None;
            message = Some(format!(
                "Reloaded Latest v{} for #{}",
                latest.row_version, latest.display_number
            ));
        }
        if let Some(message) = message {
            self.notify_success(message);
        }
    }

    fn queue_issue_mutation(&mut self, pane_id: PaneId, mutation: IssuePendingMutation) {
        self.queue_issue_mutation_in_tab(self.active_tab, pane_id, mutation);
    }

    fn queue_issue_mutation_in_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        mutation: IssuePendingMutation,
    ) {
        let (project_id, generation, request_identity) = {
            let Some(tab) = self.tabs.get_mut(tab_index) else {
                return;
            };
            let Some(Pane::Issues(state)) = tab.layout.find_pane_mut(pane_id) else {
                return;
            };
            let Some(project_id) = state.project_id else {
                return;
            };
            if let Some(pending) = &state.transient.pending_mutation
                && pending != &mutation
            {
                state.transient.mutation_retry_error = Some(
                    "RETRY ENVELOPE FROZEN — Ctrl-Enter retries the identical retained request"
                        .to_string(),
                );
                return;
            }
            let request_identity =
                IssueWorkspaceRequestIdentity::Mutation(mutation_identity(&mutation));
            let Some(generation) =
                state.begin_request(IssueWorkspaceDataKind::Mutation, request_identity.clone())
            else {
                return;
            };
            let data = state.data_state_mut(IssueWorkspaceDataKind::Mutation);
            data.error = None;
            data.load_state = IssueWorkspaceLoadState::Loading;
            state.transient.pending_mutation = Some(mutation.clone());
            state.transient.cancel_confirmation = None;
            state.transient.archive_confirmation = None;
            if let Some(editor) = &mut state.transient.editor {
                editor.submitted = true;
            }
            (project_id, generation, request_identity)
        };
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.issues_tx.clone();
        tokio::spawn(async move {
            let mut client = crate::client::DaemonClient::new(socket_path);
            let outcome = match client.connect().await {
                Err(error) => Err(async_error(error)),
                Ok(()) => match mutation {
                    IssuePendingMutation::Create(request) => client
                        .create_issue_v2(request)
                        .await
                        .map(IssueWorkspaceAsyncPayload::Mutation)
                        .map_err(async_error),
                    IssuePendingMutation::Update(request) => client
                        .update_issue(request)
                        .await
                        .map(IssueWorkspaceAsyncPayload::Mutation)
                        .map_err(async_error),
                    IssuePendingMutation::Status(request) => client
                        .update_issue_status_v2(request)
                        .await
                        .map(IssueWorkspaceAsyncPayload::Mutation)
                        .map_err(async_error),
                    IssuePendingMutation::Archive(request) => client
                        .archive_issue(request)
                        .await
                        .map(IssueWorkspaceAsyncPayload::Mutation)
                        .map_err(async_error),
                    IssuePendingMutation::Restore(request) => client
                        .restore_issue(request)
                        .await
                        .map(IssueWorkspaceAsyncPayload::Mutation)
                        .map_err(async_error),
                    IssuePendingMutation::AddDependency(pending) => client
                        .add_issue_dependency(pending.request)
                        .await
                        .map(IssueWorkspaceAsyncPayload::DependencyMutation)
                        .map_err(async_error),
                    IssuePendingMutation::RemoveDependency(pending) => client
                        .remove_issue_dependency(pending.request)
                        .await
                        .map(IssueWorkspaceAsyncPayload::DependencyMutation)
                        .map_err(async_error),
                },
            };
            let _ = tx.send(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: IssueWorkspaceDataKind::Mutation,
                generation,
                request_identity,
                outcome,
            });
        });
    }
}

fn editor_field_mut(editor: &mut IssueEditorState) -> &mut String {
    match editor.mode {
        IssueEditorMode::Dependency { .. } => &mut editor.dependency_text,
        IssueEditorMode::Filter => &mut editor.dependency_text,
        _ => match editor.active_field {
            0 => &mut editor.title,
            1 => &mut editor.body,
            2 => &mut editor.dependency_text,
            3 => &mut editor.dependency_text,
            _ => &mut editor.dependency_text,
        },
    }
}

fn sync_editor_derived(editor: &mut IssueEditorState) {
    if matches!(editor.mode, IssueEditorMode::Filter) {
        let Some(filters) = &mut editor.filter_draft else {
            return;
        };
        match editor.active_field {
            1 => {
                filters.query = (!editor.dependency_text.trim().is_empty())
                    .then(|| editor.dependency_text.trim().to_string())
            }
            2 => filters.statuses = parse_issue_statuses(&editor.dependency_text),
            3 => filters.priorities = parse_issue_priorities(&editor.dependency_text),
            5 => {
                let value = editor.dependency_text.trim();
                filters.unassigned = value == "-";
                filters.assignee = (!value.is_empty() && value != "-").then(|| value.to_string());
                if editor.filter_saved_view == Some(IssueWorkspaceSavedView::Mine) {
                    editor.filter_mine_assignee = filters.assignee.clone();
                }
            }
            6 => filters.labels_all = parse_csv(&editor.dependency_text),
            7 => filters.provenance = parse_issue_provenance(&editor.dependency_text),
            _ => {}
        }
        if editor.active_field != 0
            && !(editor.active_field == 5
                && editor.filter_saved_view == Some(IssueWorkspaceSavedView::Mine))
        {
            editor.filter_saved_view = None;
        }
        return;
    }
    match editor.active_field {
        2 => {
            editor.priority = editor
                .dependency_text
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|value| (1..=4).contains(value));
        }
        3 => {
            editor.assignee = (!editor.dependency_text.trim().is_empty())
                .then(|| editor.dependency_text.trim().to_string());
        }
        4 => {
            editor.labels = editor
                .dependency_text
                .split(',')
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(str::to_string)
                .collect();
        }
        _ => {}
    }
}

fn load_editor_scratch(editor: &mut IssueEditorState) {
    if matches!(editor.mode, IssueEditorMode::Dependency { .. }) {
        return;
    }
    if matches!(editor.mode, IssueEditorMode::Filter) {
        let Some(filters) = &editor.filter_draft else {
            return;
        };
        editor.dependency_text = match editor.active_field {
            0 => editor
                .filter_saved_view
                .map(saved_view_label)
                .unwrap_or("Custom")
                .to_string(),
            1 => filters.query.clone().unwrap_or_default(),
            2 => filters
                .statuses
                .iter()
                .map(|status| status.as_str())
                .collect::<Vec<_>>()
                .join(","),
            3 => filters
                .priorities
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(","),
            4 => readiness_filter_label(filters.readiness).to_string(),
            5 if filters.unassigned => "-".to_string(),
            5 => filters.assignee.clone().unwrap_or_default(),
            6 => filters.labels_all.join(","),
            7 => filters
                .provenance
                .iter()
                .map(|value| format!("{value:?}").to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(","),
            8 => format!("{:?}", filters.archive),
            _ => String::new(),
        };
        return;
    }
    editor.dependency_text = match editor.active_field {
        2 => editor
            .priority
            .map(|value| value.to_string())
            .unwrap_or_default(),
        3 => editor.assignee.clone().unwrap_or_default(),
        4 => editor.labels.join(","),
        _ => String::new(),
    };
}

fn editor_field_count(editor: &IssueEditorState) -> usize {
    match editor.mode {
        IssueEditorMode::Dependency { .. } => 3,
        IssueEditorMode::Filter => 9,
        _ => 5,
    }
}

fn editor_accepts_text(editor: &IssueEditorState) -> bool {
    match editor.mode {
        IssueEditorMode::Dependency { .. } => editor.active_field == 1,
        IssueEditorMode::Filter => matches!(editor.active_field, 1 | 2 | 3 | 5 | 6 | 7),
        _ => true,
    }
}

fn clear_editor_discard_arm(editor: &mut IssueEditorState) {
    if editor.discard_armed {
        editor.discard_armed = false;
        if editor
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("UNSAVED CHANGES —"))
        {
            editor.error = None;
        }
    }
}

fn cycle_filter_choice(editor: &mut IssueEditorState, forward: bool) {
    let Some(filters) = &mut editor.filter_draft else {
        return;
    };
    match editor.active_field {
        0 => {
            let index = match editor.filter_saved_view {
                None => 0_i8,
                Some(IssueWorkspaceSavedView::Ready) => 1,
                Some(IssueWorkspaceSavedView::Blocked) => 2,
                Some(IssueWorkspaceSavedView::Mine) => 3,
                Some(IssueWorkspaceSavedView::Recent) => 4,
            };
            editor.filter_saved_view = match (index + if forward { 1 } else { -1 }).rem_euclid(5) {
                1 => Some(IssueWorkspaceSavedView::Ready),
                2 => Some(IssueWorkspaceSavedView::Blocked),
                3 => Some(IssueWorkspaceSavedView::Mine),
                4 => Some(IssueWorkspaceSavedView::Recent),
                _ => None,
            };
            *filters = crate::types::IssueLocalFilters::default();
            match editor.filter_saved_view {
                Some(IssueWorkspaceSavedView::Ready) => {
                    filters.readiness =
                        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Ready;
                }
                Some(IssueWorkspaceSavedView::Blocked) => {
                    filters.readiness =
                        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Blocked;
                }
                Some(IssueWorkspaceSavedView::Mine) => {
                    if let Some(assignee) = &editor.filter_mine_assignee {
                        filters.assignee = Some(assignee.clone());
                    } else {
                        editor.active_field = 5;
                        editor.error =
                            Some("MINE — enter this pane's assignee, then Ctrl-Enter".to_string());
                    }
                }
                Some(IssueWorkspaceSavedView::Recent) => {
                    filters.sort = rsi_common::issue_workspace::IssueWorkspaceSortV1::UpdatedDesc;
                }
                None => {}
            }
        }
        4 => {
            filters.readiness = match filters.readiness {
                rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Any => {
                    if forward {
                        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Ready
                    } else {
                        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Blocked
                    }
                }
                rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Ready => {
                    if forward {
                        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Blocked
                    } else {
                        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Any
                    }
                }
                rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Blocked => {
                    if forward {
                        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Any
                    } else {
                        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Ready
                    }
                }
            };
            editor.filter_saved_view = None;
        }
        8 => {
            filters.archive = match (filters.archive, forward) {
                (rsi_common::types::IssueArchiveFilterV1::Active, true)
                | (rsi_common::types::IssueArchiveFilterV1::All, false) => {
                    rsi_common::types::IssueArchiveFilterV1::Archived
                }
                (rsi_common::types::IssueArchiveFilterV1::Archived, true)
                | (rsi_common::types::IssueArchiveFilterV1::Active, false) => {
                    rsi_common::types::IssueArchiveFilterV1::All
                }
                (rsi_common::types::IssueArchiveFilterV1::All, true)
                | (rsi_common::types::IssueArchiveFilterV1::Archived, false) => {
                    rsi_common::types::IssueArchiveFilterV1::Active
                }
            };
            editor.filter_saved_view = None;
        }
        _ => {}
    }
}

fn parse_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_issue_statuses(value: &str) -> Vec<IssueStatus> {
    parse_csv(value)
        .into_iter()
        .filter_map(|value| match value.to_ascii_lowercase().as_str() {
            "open" => Some(IssueStatus::Open),
            "inprogress" | "in_progress" | "in progress" => Some(IssueStatus::InProgress),
            "closed" => Some(IssueStatus::Closed),
            "cancelled" | "canceled" => Some(IssueStatus::Cancelled),
            _ => None,
        })
        .collect()
}

fn parse_issue_priorities(value: &str) -> Vec<u8> {
    parse_csv(value)
        .into_iter()
        .filter_map(|value| value.parse().ok())
        .filter(|value| (1..=4).contains(value))
        .collect()
}

fn parse_issue_provenance(
    value: &str,
) -> Vec<rsi_common::issue_workspace::IssueWorkspaceProvenanceV1> {
    parse_csv(value)
        .into_iter()
        .filter_map(|value| match value.to_ascii_lowercase().as_str() {
            "operator" => Some(rsi_common::issue_workspace::IssueWorkspaceProvenanceV1::Operator),
            "session" => Some(rsi_common::issue_workspace::IssueWorkspaceProvenanceV1::Session),
            "idea" => Some(rsi_common::issue_workspace::IssueWorkspaceProvenanceV1::Idea),
            "finding" => Some(rsi_common::issue_workspace::IssueWorkspaceProvenanceV1::Finding),
            _ => None,
        })
        .collect()
}

fn saved_view_label(view: IssueWorkspaceSavedView) -> &'static str {
    match view {
        IssueWorkspaceSavedView::Ready => "Ready",
        IssueWorkspaceSavedView::Blocked => "Blocked",
        IssueWorkspaceSavedView::Mine => "Mine",
        IssueWorkspaceSavedView::Recent => "Recent",
    }
}

fn readiness_filter_label(
    readiness: rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1,
) -> &'static str {
    match readiness {
        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Any => "Any",
        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Ready => "Ready",
        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Blocked => "Blocked",
    }
}

fn reset_local_pagination(state: &mut IssueWorkspaceState) {
    state.local.page_anchor = IssueWorkspacePageAnchor::Start;
    state.local.selected_row = 0;
    state.transient.page_direction = Some(PageDirectionV1::Forward);
}

fn associated_session_ids(app: &App, issue_id: Uuid) -> Vec<Uuid> {
    let issue_id = issue_id.to_string();
    let mut ids = app
        .sessions
        .values()
        .filter(|state| state.session.issue_tracker_id.as_deref() == Some(issue_id.as_str()))
        .map(|state| state.session.id)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn cycle_status(status: IssueStatus, forward: bool) -> IssueStatus {
    let index = match status {
        IssueStatus::Open => 0_i8,
        IssueStatus::InProgress => 1,
        IssueStatus::Closed => 2,
        IssueStatus::Cancelled => 3,
    };
    match (index + if forward { 1 } else { -1 }).rem_euclid(4) {
        0 => IssueStatus::Open,
        1 => IssueStatus::InProgress,
        2 => IssueStatus::Closed,
        _ => IssueStatus::Cancelled,
    }
}

#[must_use]
pub fn issue_clipboard_payload(issue: &Issue, display_number: bool) -> (String, String) {
    if display_number {
        (
            format!("#{}", issue.display_number),
            format!("issue #{} copied", issue.display_number),
        )
    } else {
        let id = issue.id.to_string();
        (id.clone(), format!("issue {id} copied"))
    }
}

fn active_data_kind(tab: IssueWorkspaceTab) -> IssueWorkspaceDataKind {
    match tab {
        IssueWorkspaceTab::Local => IssueWorkspaceDataKind::LocalPage,
        IssueWorkspaceTab::Dispatched => IssueWorkspaceDataKind::Dispatched,
        IssueWorkspaceTab::Sync => IssueWorkspaceDataKind::SyncStatus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::with_session_list;
    use rsi_common::issue_workspace::{IssueWorkspaceReadinessV1, IssueWorkspaceRowV1};
    use rsi_common::types::IssueEventV1;

    fn fixture_issue(project_id: Uuid, issue_id: Uuid, number: i64, title: &str) -> Issue {
        let now = chrono::Utc::now();
        Issue {
            id: issue_id,
            project_id,
            display_number: number,
            title: title.to_string(),
            body: "Visible fixture body".to_string(),
            priority: Some(2),
            labels: vec!["visible-label".to_string()],
            assignee: Some("operator".to_string()),
            status: IssueStatus::Open,
            created_by_session_id: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
            archived_at: None,
            row_version: 3,
        }
    }

    fn fixture_row(issue: Issue) -> IssueWorkspaceRowV1 {
        IssueWorkspaceRowV1 {
            issue,
            readiness: IssueWorkspaceReadinessV1::Ready,
            open_blocker_count: 0,
            dependent_count: 0,
        }
    }

    fn install_issue_page(app: &mut App, pane_id: PaneId, issue: Issue) {
        let state = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        state.transient.in_flight.clear();
        state.local.selected_issue_id = Some(issue.id);
        state.local.selected_row = 0;
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue,
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        let data = state.data_state_mut(IssueWorkspaceDataKind::LocalPage);
        data.load_state = IssueWorkspaceLoadState::Fresh;
        data.last_good_ms = Some(now_ms());
    }

    #[test]
    fn issue_workspace_fake_clock_enforces_cadence_stale_and_one_in_flight() {
        assert_eq!(
            refresh_decision(Some(10_000), false, 14_999),
            (false, false)
        );
        assert_eq!(refresh_decision(Some(10_000), false, 15_000), (false, true));
        assert_eq!(refresh_decision(Some(10_000), false, 25_000), (true, true));
        assert_eq!(refresh_decision(Some(10_000), true, 25_000), (true, false));
        assert_eq!(refresh_decision(None, false, 25_000), (true, true));
        assert_eq!(refresh_decision(None, true, 25_000), (false, false));
    }

    #[tokio::test]
    async fn issue_workspace_open_focus_restore_and_project_rebind_use_physical_pane_identity() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state)) if state.project_id == Some(project_id)
        ));
        assert_eq!(app.open_or_focus_issues(), pane_id);
        let replacement = Uuid::new_v4();
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.local.page_anchor =
                IssueWorkspacePageAnchor::Cursor(IssueWorkspaceCursorV1::DisplayNumberAsc {
                    display_number: 91,
                    issue_id: Uuid::new_v4(),
                });
            state.transient.page_direction = Some(PageDirectionV1::Backward);
        }
        app.rebind_issue_workspaces(Some(replacement));
        app.request_issue_workspace_active(pane_id, false);
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.project_id == Some(replacement)
                    && state.local.page_anchor == IssueWorkspacePageAnchor::Start
                    && matches!(
                        state.request_identity(IssueWorkspaceDataKind::LocalPage),
                        Some(IssueWorkspaceRequestIdentity::Local(identity))
                            if identity.project_id == replacement
                                && identity.cursor.is_none()
                                && identity.direction == PageDirectionV1::Forward
                    )
        ));
        assert!(app.restore_pre_issues_pane());
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::SessionList { .. })
        ));
    }

    #[tokio::test]
    async fn issue_workspace_stale_generation_and_wrong_project_results_are_discarded() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        let state = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        state.transient.in_flight.clear();
        state
            .transient
            .generations
            .insert(IssueWorkspaceDataKind::LocalPage, 2);
        let page = IssueWorkspacePageV1 {
            rows: Vec::new(),
            previous_cursor: None,
            next_cursor: None,
        };
        let request_identity = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => {
                IssueWorkspaceRequestIdentity::Local(local_request_identity(project_id, state))
            }
            _ => panic!("Issues pane"),
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::LocalPage,
            generation: 1,
            request_identity: request_identity.clone(),
            outcome: Ok(IssueWorkspaceAsyncPayload::LocalPage {
                cursor: None,
                direction: PageDirectionV1::Forward,
                page: page.clone(),
            }),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert!(state.transient.current_page.is_none());
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(Uuid::new_v4()),
            data_kind: IssueWorkspaceDataKind::LocalPage,
            generation: 2,
            request_identity,
            outcome: Ok(IssueWorkspaceAsyncPayload::LocalPage {
                cursor: None,
                direction: PageDirectionV1::Forward,
                page,
            }),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert!(state.transient.current_page.is_none());
    }

    #[test]
    fn issue_workspace_clipboard_payloads_are_exact_and_positive() {
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap();
        let issue = fixture_issue(project_id, issue_id, 184, "OSC52 visible identity");
        let (uuid_payload, uuid_toast) = issue_clipboard_payload(&issue, false);
        assert_eq!(uuid_payload, issue_id.to_string());
        assert_eq!(uuid_toast, format!("issue {issue_id} copied"));
        assert_eq!(
            crate::clipboard::encode_osc52(&uuid_payload),
            "\u{1b}]52;c;MTIzZTQ1NjctZTg5Yi0xMmQzLWE0NTYtNDI2NjE0MTc0MDAw\u{7}"
        );

        let (number_payload, number_toast) = issue_clipboard_payload(&issue, true);
        assert_eq!(number_payload, "#184");
        assert_eq!(number_toast, "issue #184 copied");
        assert_eq!(
            crate::clipboard::encode_osc52(&number_payload),
            "\u{1b}]52;c;IzE4NA==\u{7}"
        );
    }

    #[tokio::test]
    async fn issue_workspace_cancel_is_uuid_bound_and_queues_cancelled_not_delete() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 185, "Cancel preserves provenance"),
        );
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueArmCancel)
            .await;
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueCancel)
            .await;
        let pending = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state.transient.pending_mutation.as_ref(),
            _ => None,
        };
        assert!(matches!(
            pending,
            Some(IssuePendingMutation::Status(request))
                if request.issue_id == issue_id && request.status == IssueStatus::Cancelled
        ));
    }

    #[tokio::test]
    async fn issue_workspace_dirty_editor_requires_two_escape_presses() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueNew)
            .await;
        app.handle_issue_workspace_editor_key(KeyEvent::new(
            KeyCode::Char('V'),
            KeyModifiers::SHIFT,
        ))
        .await;
        app.handle_issue_workspace_editor_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        let editor = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state.transient.editor.as_ref(),
            _ => None,
        }
        .expect("dirty editor remains visible");
        assert_eq!(
            editor.error.as_deref(),
            Some("UNSAVED CHANGES — Esc again to discard")
        );
        app.handle_issue_workspace_editor_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state)) if state.transient.editor.is_none()
        ));
    }

    #[tokio::test]
    async fn issue_workspace_dispatched_enter_uses_exact_session_uuid() {
        let mut app = with_session_list(1);
        let session_id = app.selected_session_id().expect("fixture session");
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.in_flight.clear();
            state.active_tab = IssueWorkspaceTab::Dispatched;
            state.dispatched.selected_session_id = Some(session_id);
            state.transient.dispatched = vec![IssueDispatchRecordV1 {
                issue_id: "tracker-42".to_string(),
                issue_identifier: "RSI-42".to_string(),
                tracker: "fixture".to_string(),
                session_id,
                dispatched_at: chrono::Utc::now(),
                last_reconciled_at: None,
                terminal_state: None,
            }];
        }
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueOpenSession)
            .await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.dispatched.inspector_open
                    && state.focus == IssueWorkspaceFocus::Inspector
        ));
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueOpenSession)
            .await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::SessionDetail { session_id: actual }) if *actual == session_id
        ));
    }

    #[tokio::test]
    async fn dispatched_refresh_preserves_exact_inspector_selection_and_recovers_missing_session() {
        let mut app = with_session_list(1);
        let session_id = app.selected_session_id().expect("fixture session");
        let mut recovered = app.sessions.get(&session_id).unwrap().session.clone();
        recovered.title = Some("Recovered dispatched session".to_string());
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        let selected = IssueDispatchRecordV1 {
            issue_id: "tracker-internal-refresh".to_string(),
            issue_identifier: "RSI-REFRESH".to_string(),
            tracker: "fixture".to_string(),
            session_id,
            dispatched_at: chrono::Utc::now(),
            last_reconciled_at: None,
            terminal_state: None,
        };
        let other = IssueDispatchRecordV1 {
            issue_id: "tracker-internal-other".to_string(),
            issue_identifier: "RSI-OTHER".to_string(),
            tracker: "fixture".to_string(),
            session_id: Uuid::new_v4(),
            dispatched_at: chrono::Utc::now(),
            last_reconciled_at: None,
            terminal_state: None,
        };
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.in_flight.clear();
            state.active_tab = IssueWorkspaceTab::Dispatched;
            state.dispatched.selected_row = 1;
            state.dispatched.selected_session_id = Some(session_id);
            state.dispatched.selected_issue_id = Some(selected.issue_id.clone());
            state.transient.dispatched = vec![other.clone(), selected.clone()];
        }

        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueOpenSession)
            .await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.dispatched.inspector_open
                    && state.focus == IssueWorkspaceFocus::Inspector
        ));

        let generation = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
            Some(Pane::Issues(state)) => state
                .begin_request(
                    IssueWorkspaceDataKind::Dispatched,
                    IssueWorkspaceRequestIdentity::Dispatched,
                )
                .expect("dispatch refresh"),
            _ => panic!("Issues pane"),
        };
        let mut terminal = selected.clone();
        terminal.last_reconciled_at = Some(chrono::Utc::now());
        terminal.terminal_state = Some("completed".to_string());
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::Dispatched,
            generation,
            request_identity: IssueWorkspaceRequestIdentity::Dispatched,
            outcome: Ok(IssueWorkspaceAsyncPayload::Dispatched(vec![
                terminal.clone(),
                other,
            ])),
        });
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.dispatched.selected_row == 0
                    && state.dispatched.selected_session_id == Some(session_id)
                    && state.dispatched.selected_issue_id.as_deref()
                        == Some("tracker-internal-refresh")
                    && state.dispatched.inspector_open
                    && state.focus == IssueWorkspaceFocus::Inspector
                    && state.transient.dispatched[0].terminal_state.as_deref()
                        == Some("completed")
        ));

        app.sessions.remove(&session_id);
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueOpenSession)
            .await;
        let generation = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => {
                assert!(
                    state
                        .transient
                        .in_flight
                        .contains(&IssueWorkspaceDataKind::AssociatedSession)
                );
                state.generation(IssueWorkspaceDataKind::AssociatedSession)
            }
            _ => panic!("Issues pane"),
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::AssociatedSession,
            generation,
            request_identity: IssueWorkspaceRequestIdentity::AssociatedSession(session_id),
            outcome: Ok(IssueWorkspaceAsyncPayload::AssociatedSession {
                requested_session_id: session_id,
                session: recovered,
            }),
        });
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::SessionDetail { session_id: actual }) if *actual == session_id
        ));
        assert_eq!(
            app.sessions
                .get(&session_id)
                .and_then(|entry| entry.session.title.as_deref()),
            Some("Recovered dispatched session")
        );
    }

    fn dependency_page(
        project_id: Uuid,
        issue_id: Uuid,
        related: Issue,
        direction: IssueDependencyDirectionV1,
    ) -> IssueDependencyPageV1 {
        let dependency = match direction {
            IssueDependencyDirectionV1::BlockedBy => rsi_common::types::IssueDep {
                project_id,
                issue_id,
                depends_on_id: related.id,
                created_at: chrono::Utc::now(),
            },
            IssueDependencyDirectionV1::Blocks => rsi_common::types::IssueDep {
                project_id,
                issue_id: related.id,
                depends_on_id: issue_id,
                created_at: chrono::Utc::now(),
            },
        };
        IssueDependencyPageV1 {
            items: vec![rsi_common::issue_workspace::IssueDependencyItemV1 {
                dependency,
                related_issue: related,
            }],
            previous_cursor: None,
            next_cursor: None,
        }
    }

    fn issue_event(issue: &Issue, sequence: i64) -> IssueEventV1 {
        rsi_common::types::IssueEventV1 {
            id: Uuid::new_v4(),
            project_id: issue.project_id,
            issue_id: issue.id,
            sequence,
            operation: rsi_common::types::IssueEventOperationV1::StatusUpdated,
            actor_kind: rsi_common::types::IssueActorKindV1::Operator,
            actor_session_id: None,
            owning_epic_id: None,
            actor_label: Some("rapid-selection-fixture".to_string()),
            expected_row_version: issue.row_version.saturating_sub(1),
            resulting_row_version: issue.row_version,
            idempotency_key: None,
            request_fingerprint: "f".repeat(64),
            occurred_at: chrono::Utc::now(),
            request: rsi_common::types::IssueSemanticRequestV1::new(
                rsi_common::types::IssueSemanticOperationV1::StatusUpdated {
                    issue_id: issue.id,
                    expected_row_version: issue.row_version.saturating_sub(1),
                    status: issue.status,
                },
            ),
            issue: issue.clone(),
        }
    }

    fn inspector_identity(
        kind: IssueWorkspaceDataKind,
        issue_id: Uuid,
    ) -> IssueWorkspaceRequestIdentity {
        match kind {
            IssueWorkspaceDataKind::SelectedIssue => IssueWorkspaceRequestIdentity::Issue(issue_id),
            IssueWorkspaceDataKind::BlockedBy => IssueWorkspaceRequestIdentity::Dependency {
                issue_id,
                direction: IssueDependencyDirectionV1::BlockedBy,
                page_direction: PageDirectionV1::Forward,
                cursor: None,
            },
            IssueWorkspaceDataKind::Blocks => IssueWorkspaceRequestIdentity::Dependency {
                issue_id,
                direction: IssueDependencyDirectionV1::Blocks,
                page_direction: PageDirectionV1::Forward,
                cursor: None,
            },
            IssueWorkspaceDataKind::Events => IssueWorkspaceRequestIdentity::Events {
                issue_id,
                after_sequence: None,
            },
            _ => panic!("not an inspector data kind"),
        }
    }

    #[tokio::test]
    async fn rapid_selection_restarts_all_inspector_kinds_for_b_and_applies_b_identity() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let a_id = Uuid::new_v4();
        let b_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        let a = fixture_issue(project_id, a_id, 201, "Issue A stale inspector");
        let b = fixture_issue(project_id, b_id, 202, "Issue B current inspector");
        let b_blocker = fixture_issue(project_id, Uuid::new_v4(), 203, "B blocker identity");
        let b_dependent = fixture_issue(project_id, Uuid::new_v4(), 204, "B dependent identity");
        {
            let state = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            state.transient.in_flight.clear();
            state.local.selected_issue_id = Some(a_id);
            for kind in [
                IssueWorkspaceDataKind::SelectedIssue,
                IssueWorkspaceDataKind::BlockedBy,
                IssueWorkspaceDataKind::Blocks,
                IssueWorkspaceDataKind::Events,
            ] {
                assert_eq!(
                    state.begin_request(kind, inspector_identity(kind, a_id)),
                    Some(1)
                );
            }
            state.local.selected_issue_id = Some(b_id);
            for kind in [
                IssueWorkspaceDataKind::SelectedIssue,
                IssueWorkspaceDataKind::BlockedBy,
                IssueWorkspaceDataKind::Blocks,
                IssueWorkspaceDataKind::Events,
            ] {
                assert_eq!(
                    state.begin_request(kind, inspector_identity(kind, b_id)),
                    None
                );
            }
        }

        let stale_payloads = [
            (
                IssueWorkspaceDataKind::SelectedIssue,
                IssueWorkspaceAsyncPayload::SelectedIssue {
                    requested_issue_id: a_id,
                    row: Some(fixture_row(a.clone())),
                },
            ),
            (
                IssueWorkspaceDataKind::BlockedBy,
                IssueWorkspaceAsyncPayload::Dependencies {
                    requested_issue_id: a_id,
                    direction: IssueDependencyDirectionV1::BlockedBy,
                    page_direction: PageDirectionV1::Forward,
                    cursor: None,
                    page: dependency_page(
                        project_id,
                        a_id,
                        a.clone(),
                        IssueDependencyDirectionV1::BlockedBy,
                    ),
                },
            ),
            (
                IssueWorkspaceDataKind::Blocks,
                IssueWorkspaceAsyncPayload::Dependencies {
                    requested_issue_id: a_id,
                    direction: IssueDependencyDirectionV1::Blocks,
                    page_direction: PageDirectionV1::Forward,
                    cursor: None,
                    page: dependency_page(
                        project_id,
                        a_id,
                        a.clone(),
                        IssueDependencyDirectionV1::Blocks,
                    ),
                },
            ),
            (
                IssueWorkspaceDataKind::Events,
                IssueWorkspaceAsyncPayload::Events {
                    requested_issue_id: a_id,
                    after_sequence: None,
                    page: IssueEventPageV1 {
                        events: vec![issue_event(&a, 1)],
                        next_after_sequence: None,
                    },
                },
            ),
        ];
        for (kind, payload) in stale_payloads {
            app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: kind,
                generation: 1,
                request_identity: inspector_identity(kind, a_id),
                outcome: Ok(payload),
            });
        }
        {
            let state = match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            for kind in [
                IssueWorkspaceDataKind::SelectedIssue,
                IssueWorkspaceDataKind::BlockedBy,
                IssueWorkspaceDataKind::Blocks,
                IssueWorkspaceDataKind::Events,
            ] {
                assert_eq!(state.generation(kind), 3);
                assert_eq!(
                    state.request_identity(kind),
                    Some(&inspector_identity(kind, b_id))
                );
                assert!(state.transient.in_flight.contains(&kind));
            }
        }

        let current_payloads = [
            (
                IssueWorkspaceDataKind::SelectedIssue,
                IssueWorkspaceAsyncPayload::SelectedIssue {
                    requested_issue_id: b_id,
                    row: Some(fixture_row(b.clone())),
                },
            ),
            (
                IssueWorkspaceDataKind::BlockedBy,
                IssueWorkspaceAsyncPayload::Dependencies {
                    requested_issue_id: b_id,
                    direction: IssueDependencyDirectionV1::BlockedBy,
                    page_direction: PageDirectionV1::Forward,
                    cursor: None,
                    page: dependency_page(
                        project_id,
                        b_id,
                        b_blocker.clone(),
                        IssueDependencyDirectionV1::BlockedBy,
                    ),
                },
            ),
            (
                IssueWorkspaceDataKind::Blocks,
                IssueWorkspaceAsyncPayload::Dependencies {
                    requested_issue_id: b_id,
                    direction: IssueDependencyDirectionV1::Blocks,
                    page_direction: PageDirectionV1::Forward,
                    cursor: None,
                    page: dependency_page(
                        project_id,
                        b_id,
                        b_dependent.clone(),
                        IssueDependencyDirectionV1::Blocks,
                    ),
                },
            ),
            (
                IssueWorkspaceDataKind::Events,
                IssueWorkspaceAsyncPayload::Events {
                    requested_issue_id: b_id,
                    after_sequence: None,
                    page: IssueEventPageV1 {
                        events: vec![issue_event(&b, 9)],
                        next_after_sequence: None,
                    },
                },
            ),
        ];
        for (kind, payload) in current_payloads {
            app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: kind,
                generation: 3,
                request_identity: inspector_identity(kind, b_id),
                outcome: Ok(payload),
            });
        }
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.transient.selected_issue_target, Some(b_id));
        assert_eq!(
            state
                .transient
                .selected_issue
                .as_ref()
                .map(|issue| issue.id),
            Some(b_id)
        );
        assert_eq!(state.transient.blocked_by_target, Some(b_id));
        assert_eq!(
            state.transient.blocked_by.as_ref().unwrap().items[0]
                .related_issue
                .title,
            "B blocker identity"
        );
        assert_eq!(state.transient.blocks_target, Some(b_id));
        assert_eq!(
            state.transient.blocks.as_ref().unwrap().items[0]
                .related_issue
                .title,
            "B dependent identity"
        );
        assert_eq!(state.transient.events_target, Some(b_id));
        assert_eq!(state.transient.events[0].issue.id, b_id);
        assert_eq!(state.transient.events[0].sequence, 9);
    }

    #[tokio::test]
    async fn missing_selected_issue_clears_inspector_identity_and_exposes_recovery_state() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        let issue = fixture_issue(project_id, issue_id, 205, "Disappearing Issue identity");
        install_issue_page(&mut app, pane_id, issue.clone());
        let generation = {
            let state = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            state.transient.selected_issue_target = Some(issue_id);
            state.transient.selected_issue = Some(issue.clone());
            state.transient.blocked_by_target = Some(issue_id);
            state.transient.blocked_by = Some(dependency_page(
                project_id,
                issue_id,
                fixture_issue(project_id, Uuid::new_v4(), 206, "Old blocker identity"),
                IssueDependencyDirectionV1::BlockedBy,
            ));
            state.transient.events_target = Some(issue_id);
            state.transient.events = vec![issue_event(&issue, 1)];
            state
                .begin_request(
                    IssueWorkspaceDataKind::SelectedIssue,
                    IssueWorkspaceRequestIdentity::Issue(issue_id),
                )
                .unwrap()
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::SelectedIssue,
            generation,
            request_identity: IssueWorkspaceRequestIdentity::Issue(issue_id),
            outcome: Ok(IssueWorkspaceAsyncPayload::SelectedIssue {
                requested_issue_id: issue_id,
                row: None,
            }),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.local.selected_issue_id, Some(issue_id));
        assert_eq!(state.transient.missing_selected_issue_id, Some(issue_id));
        assert_eq!(state.transient.selected_issue, None);
        assert_eq!(state.transient.blocked_by, None);
        assert!(state.transient.events.is_empty());
        assert_eq!(
            state.load_state(IssueWorkspaceDataKind::SelectedIssue),
            IssueWorkspaceLoadState::Error
        );
        assert_eq!(
            state.data_error(IssueWorkspaceDataKind::SelectedIssue),
            Some("SELECTED ISSUE UNAVAILABLE — refresh Local to recover")
        );
        assert_eq!(
            current_request_identity(state, IssueWorkspaceDataKind::BlockedBy),
            None
        );
        assert_eq!(
            current_request_identity(state, IssueWorkspaceDataKind::Events),
            None
        );
    }

    fn local_identity_race(
        mutate: impl FnOnce(&mut IssueWorkspaceState),
    ) -> IssueWorkspaceLocalRequestIdentity {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        let old_identity = {
            let state = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            state.transient.in_flight.clear();
            let identity = local_request_identity(project_id, state);
            assert!(
                state
                    .begin_request(
                        IssueWorkspaceDataKind::LocalPage,
                        IssueWorkspaceRequestIdentity::Local(identity.clone()),
                    )
                    .is_some()
            );
            mutate(state);
            let newest = local_request_identity(project_id, state);
            assert_eq!(
                state.begin_request(
                    IssueWorkspaceDataKind::LocalPage,
                    IssueWorkspaceRequestIdentity::Local(newest),
                ),
                None
            );
            identity
        };
        let generation = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state.generation(IssueWorkspaceDataKind::LocalPage),
            _ => panic!("Issues pane"),
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::LocalPage,
            generation,
            request_identity: IssueWorkspaceRequestIdentity::Local(old_identity.clone()),
            outcome: Ok(IssueWorkspaceAsyncPayload::LocalPage {
                cursor: old_identity.cursor,
                direction: old_identity.direction,
                page: IssueWorkspacePageV1 {
                    rows: vec![IssueWorkspaceRowV1 {
                        issue: fixture_issue(project_id, Uuid::new_v4(), 301, "Old request row"),
                        readiness: IssueWorkspaceReadinessV1::Ready,
                        open_blocker_count: 0,
                        dependent_count: 0,
                    }],
                    previous_cursor: None,
                    next_cursor: None,
                },
            }),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert!(state.transient.current_page.is_none());
        assert!(
            state
                .transient
                .in_flight
                .contains(&IssueWorkspaceDataKind::LocalPage)
        );
        match state
            .request_identity(IssueWorkspaceDataKind::LocalPage)
            .cloned()
            .unwrap()
        {
            IssueWorkspaceRequestIdentity::Local(identity) => identity,
            _ => panic!("Local request identity"),
        }
    }

    #[tokio::test]
    async fn local_query_change_discards_inflight_page_and_restarts_from_start() {
        let identity = local_identity_race(|state| {
            state.local.filters.query = Some("new query identity".to_string());
            reset_local_pagination(state);
        });
        assert_eq!(
            identity.filters.query.as_deref(),
            Some("new query identity")
        );
        assert_eq!(identity.page_anchor, IssueWorkspacePageAnchor::Start);
        assert_eq!(identity.cursor, None);
    }

    #[tokio::test]
    async fn local_archive_change_discards_inflight_page_and_restarts_from_start() {
        let identity = local_identity_race(|state| {
            state.local.filters.archive = rsi_common::types::IssueArchiveFilterV1::Archived;
            reset_local_pagination(state);
        });
        assert_eq!(
            identity.filters.archive,
            rsi_common::types::IssueArchiveFilterV1::Archived
        );
        assert_eq!(identity.page_anchor, IssueWorkspacePageAnchor::Start);
    }

    #[tokio::test]
    async fn local_sort_change_discards_inflight_page_and_restarts_from_start() {
        let identity = local_identity_race(|state| {
            state.local.filters.sort =
                rsi_common::issue_workspace::IssueWorkspaceSortV1::PriorityAsc;
            reset_local_pagination(state);
        });
        assert_eq!(
            identity.filters.sort,
            rsi_common::issue_workspace::IssueWorkspaceSortV1::PriorityAsc
        );
        assert_eq!(identity.page_anchor, IssueWorkspacePageAnchor::Start);
    }

    #[tokio::test]
    async fn local_page_intent_discards_inflight_start_and_restarts_at_exact_end_direction() {
        let identity = local_identity_race(|state| {
            state.local.page_anchor = IssueWorkspacePageAnchor::End;
            state.transient.page_direction = Some(PageDirectionV1::Backward);
        });
        assert_eq!(identity.page_anchor, IssueWorkspacePageAnchor::End);
        assert_eq!(identity.direction, PageDirectionV1::Backward);
        assert_eq!(identity.cursor, None);
    }

    #[tokio::test]
    async fn manual_poll_error_survives_status_and_dispatch_success_until_manual_poll_succeeds() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        {
            let state = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            state.transient.in_flight.clear();
            state.active_tab = IssueWorkspaceTab::Sync;
            state
                .transient
                .generations
                .insert(IssueWorkspaceDataKind::ManualPoll, 1);
        }
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::ManualPoll,
            generation: 1,
            request_identity: IssueWorkspaceRequestIdentity::ManualPoll,
            outcome: Err(IssueWorkspaceAsyncError {
                message: "manual poll authority denied".to_string(),
                access_denied: true,
                workspace: None,
            }),
        });
        for (kind, identity, payload) in [
            (
                IssueWorkspaceDataKind::SyncStatus,
                IssueWorkspaceRequestIdentity::SyncStatus,
                IssueWorkspaceAsyncPayload::SyncStatus(IssueTrackerStatusV1 {
                    enabled: true,
                    tracker: "manual-poll-fixture".to_string(),
                    last_poll_at: None,
                    next_poll_at: None,
                    dispatched_count: 0,
                    max_concurrent: 3,
                    poll_interval_ms: 5_000,
                    active_states: vec!["active".to_string()],
                }),
            ),
            (
                IssueWorkspaceDataKind::Dispatched,
                IssueWorkspaceRequestIdentity::Dispatched,
                IssueWorkspaceAsyncPayload::Dispatched(Vec::new()),
            ),
        ] {
            if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
                state.transient.generations.insert(kind, 1);
            }
            app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: kind,
                generation: 1,
                request_identity: identity,
                outcome: Ok(payload),
            });
            let state = match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            assert_eq!(
                state.transient.manual_poll_error.as_deref(),
                Some("manual poll authority denied")
            );
        }
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state
                .transient
                .generations
                .insert(IssueWorkspaceDataKind::ManualPoll, 2);
        }
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::ManualPoll,
            generation: 2,
            request_identity: IssueWorkspaceRequestIdentity::ManualPoll,
            outcome: Ok(IssueWorkspaceAsyncPayload::ManualPoll(
                IssueTrackerTickResultV1 {
                    issues_found: 7,
                    dispatched: 2,
                    skipped_claimed: 3,
                    skipped_blocked: 2,
                    errors: Vec::new(),
                },
            )),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.transient.manual_poll_error, None);
        assert_eq!(
            state
                .transient
                .manual_poll_result
                .as_ref()
                .map(|result| result.issues_found),
            Some(7)
        );
        assert!(
            state
                .transient
                .in_flight
                .contains(&IssueWorkspaceDataKind::SyncStatus)
        );
        assert!(
            state
                .transient
                .in_flight
                .contains(&IssueWorkspaceDataKind::Dispatched)
        );
        assert_eq!(
            state.load_state(IssueWorkspaceDataKind::SyncStatus),
            IssueWorkspaceLoadState::Stale
        );
        assert_eq!(
            state.load_state(IssueWorkspaceDataKind::Dispatched),
            IssueWorkspaceLoadState::Stale
        );
    }

    #[tokio::test]
    async fn clean_sync_raw_refresh_and_poll_are_distinct_and_poll_refreshes_both_views() {
        use crate::action_registry::{ActionAvailability, ActionId};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = with_session_list(0);
        app.poll.connected = true;
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient = Default::default();
            state.active_tab = IssueWorkspaceTab::Sync;
        }
        let context = crate::action_registry::ActionContext::from_app(&app);
        assert!(!context.has_selection);
        assert_eq!(
            crate::action_registry::request_for_key(
                &context,
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            ),
            ActionAvailability::Available(crate::action_registry::ActionRequest::plain(
                ActionId::IssueRefresh,
            ))
        );
        assert_eq!(
            crate::action_registry::request_for_key(
                &context,
                KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
            ),
            ActionAvailability::Available(crate::action_registry::ActionRequest::plain(
                ActionId::IssueRunPoll,
            ))
        );
        assert!(matches!(
            crate::action_registry::request_for_key(
                &context,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            ),
            ActionAvailability::Unavailable { .. }
        ));

        crate::event::step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        )
        .await;
        crate::event::step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        )
        .await;
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert!(
            state
                .transient
                .in_flight
                .contains(&IssueWorkspaceDataKind::SyncStatus)
        );
        assert!(
            !state
                .transient
                .in_flight
                .contains(&IssueWorkspaceDataKind::ManualPoll)
        );
        assert_eq!(state.generation(IssueWorkspaceDataKind::SyncStatus), 2);

        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::SyncStatus,
            generation: 1,
            request_identity: IssueWorkspaceRequestIdentity::SyncStatus,
            outcome: Ok(IssueWorkspaceAsyncPayload::SyncStatus(
                IssueTrackerStatusV1 {
                    enabled: true,
                    tracker: "stale same-identity status".to_string(),
                    last_poll_at: None,
                    next_poll_at: None,
                    dispatched_count: 0,
                    max_concurrent: 2,
                    poll_interval_ms: 5_000,
                    active_states: vec!["active".to_string()],
                },
            )),
        });
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.generation(IssueWorkspaceDataKind::SyncStatus) == 3
                    && state.transient.sync_status.is_none()
                    && state.transient.in_flight.contains(&IssueWorkspaceDataKind::SyncStatus)
        ));

        crate::event::step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
        )
        .await;
        let manual_generation = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => {
                assert!(
                    state
                        .transient
                        .in_flight
                        .contains(&IssueWorkspaceDataKind::ManualPoll)
                );
                state.generation(IssueWorkspaceDataKind::ManualPoll)
            }
            _ => panic!("Issues pane"),
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::ManualPoll,
            generation: manual_generation,
            request_identity: IssueWorkspaceRequestIdentity::ManualPoll,
            outcome: Ok(IssueWorkspaceAsyncPayload::ManualPoll(
                IssueTrackerTickResultV1 {
                    issues_found: 4,
                    dispatched: 1,
                    skipped_claimed: 2,
                    skipped_blocked: 1,
                    errors: vec!["bounded poll error row".to_string()],
                },
            )),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(
            state.transient.manual_poll_result.as_ref().unwrap().errors,
            vec!["bounded poll error row"]
        );
        assert_eq!(state.generation(IssueWorkspaceDataKind::SyncStatus), 4);
        assert_eq!(state.generation(IssueWorkspaceDataKind::Dispatched), 1);
        assert!(
            state
                .transient
                .in_flight
                .contains(&IssueWorkspaceDataKind::Dispatched)
        );
    }

    #[tokio::test]
    async fn per_kind_success_cannot_relabel_or_erase_local_failure() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        {
            let state = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            state.transient.in_flight.clear();
            state.local.selected_issue_id = Some(issue_id);
            let local = state.data_state_mut(IssueWorkspaceDataKind::LocalPage);
            local.load_state = IssueWorkspaceLoadState::Error;
            local.error = Some("local page failed visibly".to_string());
            state
                .transient
                .generations
                .insert(IssueWorkspaceDataKind::SelectedIssue, 1);
        }
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::SelectedIssue,
            generation: 1,
            request_identity: IssueWorkspaceRequestIdentity::Issue(issue_id),
            outcome: Ok(IssueWorkspaceAsyncPayload::SelectedIssue {
                requested_issue_id: issue_id,
                row: Some(fixture_row(fixture_issue(
                    project_id,
                    issue_id,
                    401,
                    "Inspector success identity",
                ))),
            }),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(
            state.load_state(IssueWorkspaceDataKind::LocalPage),
            IssueWorkspaceLoadState::Error
        );
        assert_eq!(
            state.data_error(IssueWorkspaceDataKind::LocalPage),
            Some("local page failed visibly")
        );
        assert_eq!(
            state.load_state(IssueWorkspaceDataKind::SelectedIssue),
            IssueWorkspaceLoadState::Fresh
        );
        assert_eq!(state.transient.selected_issue_target, Some(issue_id));
    }

    #[tokio::test]
    async fn saved_views_capture_mine_and_custom_filters_retain_every_bounded_value() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.in_flight.clear();
        }
        app.open_issue_filter_editor(pane_id, 0);
        for _ in 0..3 {
            app.handle_issue_workspace_editor_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                .await;
        }
        for character in "jake".chars() {
            app.handle_issue_workspace_editor_key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            ))
            .await;
        }
        app.handle_issue_workspace_editor_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL))
            .await;
        {
            let state = match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            assert_eq!(state.local.saved_view, Some(IssueWorkspaceSavedView::Mine));
            assert_eq!(state.local.mine_assignee.as_deref(), Some("jake"));
            assert_eq!(state.local.filters.assignee.as_deref(), Some("jake"));
            assert_eq!(state.local.page_anchor, IssueWorkspacePageAnchor::Start);
        }

        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.in_flight.clear();
        }
        app.open_issue_filter_editor(pane_id, 1);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.active_field = 1;
            editor.dependency_text = "scheduler race".to_string();
            sync_editor_derived(editor);
            editor.active_field = 2;
            editor.dependency_text = "Open,Closed".to_string();
            sync_editor_derived(editor);
            editor.active_field = 3;
            editor.dependency_text = "1,4".to_string();
            sync_editor_derived(editor);
            editor.active_field = 4;
            cycle_filter_choice(editor, true);
            editor.active_field = 5;
            editor.dependency_text = "-".to_string();
            sync_editor_derived(editor);
            editor.active_field = 6;
            editor.dependency_text = "daemon,reliability".to_string();
            sync_editor_derived(editor);
            editor.active_field = 7;
            editor.dependency_text = "operator,finding".to_string();
            sync_editor_derived(editor);
            editor.active_field = 8;
            cycle_filter_choice(editor, true);
        }
        app.submit_issue_editor(pane_id, false);
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.local.saved_view, None);
        assert_eq!(state.local.filters.query.as_deref(), Some("scheduler race"));
        assert_eq!(
            state.local.filters.statuses,
            vec![IssueStatus::Open, IssueStatus::Closed]
        );
        assert_eq!(state.local.filters.priorities, vec![1, 4]);
        assert_eq!(
            state.local.filters.readiness,
            rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Ready
        );
        assert!(state.local.filters.unassigned);
        assert_eq!(
            state.local.filters.labels_all,
            vec!["daemon", "reliability"]
        );
        assert_eq!(state.local.filters.provenance.len(), 2);
        assert_eq!(
            state.local.filters.archive,
            rsi_common::types::IssueArchiveFilterV1::Archived
        );
        assert_eq!(state.local.page_anchor, IssueWorkspacePageAnchor::Start);
    }

    #[tokio::test]
    async fn local_associated_session_selection_enters_exact_cached_and_recovered_session() {
        let mut app = with_session_list(1);
        let session_id = app.selected_session_id().unwrap();
        let recovered = app.sessions.get(&session_id).unwrap().session.clone();
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.sessions
            .get_mut(&session_id)
            .unwrap()
            .session
            .issue_tracker_id = Some(issue_id.to_string());
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 501, "Associated session issue"),
        );
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueOpenSession)
            .await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.local.selected_associated_session_id == Some(session_id)
        ));
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueOpenSession)
            .await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::SessionDetail { session_id: actual }) if *actual == session_id
        ));

        let mut app = with_session_list(1);
        let session_id = app.selected_session_id().unwrap();
        let mut recovered = recovered;
        recovered.id = session_id;
        recovered.issue_tracker_id = Some(issue_id.to_string());
        app.sessions
            .get_mut(&session_id)
            .unwrap()
            .session
            .issue_tracker_id = Some(issue_id.to_string());
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 502, "Recovered session issue"),
        );
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueOpenSession)
            .await;
        app.sessions.remove(&session_id);
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueOpenSession)
            .await;
        let generation = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => {
                state.generation(IssueWorkspaceDataKind::AssociatedSession)
            }
            _ => panic!("Issues pane"),
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::AssociatedSession,
            generation,
            request_identity: IssueWorkspaceRequestIdentity::AssociatedSession(session_id),
            outcome: Ok(IssueWorkspaceAsyncPayload::AssociatedSession {
                requested_session_id: session_id,
                session: recovered,
            }),
        });
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::SessionDetail { session_id: actual }) if *actual == session_id
        ));
        assert!(app.sessions.contains_key(&session_id));
    }

    #[tokio::test]
    async fn stale_editor_waits_for_newer_latest_disables_save_and_rebases_with_new_key() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 601, "Immutable Base title"),
        );
        app.open_issue_edit_editor(pane_id, 0);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.title = "Retained Draft title".to_string();
            editor.dirty = true;
        }
        app.submit_issue_editor(pane_id, false);
        let (generation, request_identity, original_key) =
            match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => {
                    let editor = state.transient.editor.as_ref().unwrap();
                    (
                        state.generation(IssueWorkspaceDataKind::Mutation),
                        state
                            .request_identity(IssueWorkspaceDataKind::Mutation)
                            .cloned()
                            .unwrap(),
                        editor.retry_key.clone(),
                    )
                }
                _ => panic!("Issues pane"),
            };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::Mutation,
            generation,
            request_identity,
            outcome: Err(IssueWorkspaceAsyncError {
                message: "stale version".to_string(),
                access_denied: false,
                workspace: Some(IssueWorkspaceErrorV1 {
                    code: rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::StaleVersion,
                    issue_id: Some(issue_id),
                    expected_row_version: Some(3),
                    actual_row_version: Some(4),
                    retryable: false,
                    next_action: "reload".to_string(),
                }),
            }),
        });
        let selected_generation = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => {
                let editor = state.transient.editor.as_ref().unwrap();
                assert!(editor.stale_conflict);
                assert_eq!(editor.title, "Retained Draft title");
                assert_eq!(
                    editor.base_issue.as_ref().unwrap().title,
                    "Immutable Base title"
                );
                assert_eq!(editor.retry_key, original_key);
                assert_eq!(editor.latest_issue, None);
                state.generation(IssueWorkspaceDataKind::SelectedIssue)
            }
            _ => panic!("Issues pane"),
        };
        let mut latest = fixture_issue(project_id, issue_id, 601, "Latest server title");
        latest.row_version = 4;
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::SelectedIssue,
            generation: selected_generation,
            request_identity: IssueWorkspaceRequestIdentity::Issue(issue_id),
            outcome: Ok(IssueWorkspaceAsyncPayload::SelectedIssue {
                requested_issue_id: issue_id,
                row: Some(fixture_row(latest)),
            }),
        });
        app.handle_issue_workspace_editor_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL))
            .await;
        {
            let state = match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            let editor = state.transient.editor.as_ref().unwrap();
            assert_eq!(editor.retry_key, original_key);
            assert!(editor.error.as_deref().unwrap().contains("SAVE DISABLED"));
        }
        crate::event::step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        )
        .await;
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        let editor = state.transient.editor.as_ref().unwrap();
        assert!(matches!(
            editor.mode,
            IssueEditorMode::Edit { row_version: 4, .. }
        ));
        assert_ne!(editor.retry_key, original_key);
        assert_eq!(editor.title, "Retained Draft title");
        assert!(editor.error.as_deref().unwrap().contains("DRAFT REBASED"));
    }

    #[tokio::test]
    async fn help_round_trip_clears_dirty_reload_arm_before_latest_discard() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 602, "Reload Base"),
        );
        app.open_issue_edit_editor(pane_id, 0);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            let mut latest = editor.base_issue.clone().unwrap();
            latest.row_version = 4;
            latest.title = "Reload Latest".to_string();
            editor.latest_issue = Some(latest);
            editor.stale_conflict = true;
            editor.title = "Dirty Draft".to_string();
            editor.dirty = true;
        }
        let ctrl_l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        crate::event::step_once(&mut app, ctrl_l).await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.transient.editor.as_ref().is_some_and(|editor| editor.discard_armed)
        ));
        assert!(
            !app.handle_issue_workspace_editor_key(KeyEvent::new(
                KeyCode::Char('?'),
                KeyModifiers::NONE,
            ))
            .await
        );
        crate::overlay::keybindings_help::open_contextual_help(&mut app);
        assert!(matches!(
            app.overlay,
            crate::types::OverlayState::KeybindingsHelp { .. }
        ));
        crate::overlay::keybindings_help::close_keybindings_help(&mut app);
        crate::event::step_once(&mut app, ctrl_l).await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.transient.editor.as_ref().is_some_and(|editor| editor.discard_armed)
        ));
        crate::event::step_once(&mut app, ctrl_l).await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state)) if state.transient.editor.is_none()
        ));
    }

    #[tokio::test]
    async fn async_local_result_under_help_preserves_exact_form_draft_and_focus() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        app.open_issue_create_editor(pane_id);
        let (generation, request_identity) = {
            let state = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            state.transient.in_flight.clear();
            let editor = state.transient.editor.as_mut().unwrap();
            editor.title = "Help-retained draft identity".to_string();
            editor.body = "Help-retained draft body".to_string();
            editor.active_field = 1;
            editor.dirty = true;
            let identity =
                IssueWorkspaceRequestIdentity::Local(local_request_identity(project_id, state));
            let generation = state
                .begin_request(IssueWorkspaceDataKind::LocalPage, identity.clone())
                .unwrap();
            (generation, identity)
        };
        crate::overlay::keybindings_help::open_contextual_help(&mut app);
        assert!(matches!(
            app.overlay,
            crate::types::OverlayState::KeybindingsHelp { .. }
        ));
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::LocalPage,
            generation,
            request_identity,
            outcome: Ok(IssueWorkspaceAsyncPayload::LocalPage {
                cursor: None,
                direction: PageDirectionV1::Forward,
                page: IssueWorkspacePageV1 {
                    rows: vec![IssueWorkspaceRowV1 {
                        issue: fixture_issue(
                            project_id,
                            issue_id,
                            603,
                            "Fresh page applied behind help",
                        ),
                        readiness: IssueWorkspaceReadinessV1::Ready,
                        open_blocker_count: 0,
                        dependent_count: 0,
                    }],
                    previous_cursor: None,
                    next_cursor: None,
                },
            }),
        });
        crate::overlay::keybindings_help::close_keybindings_help(&mut app);
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        let editor = state.transient.editor.as_ref().unwrap();
        assert_eq!(editor.title, "Help-retained draft identity");
        assert_eq!(editor.body, "Help-retained draft body");
        assert_eq!(editor.active_field, 1);
        assert!(editor.dirty);
        assert_eq!(state.focus, IssueWorkspaceFocus::Table);
        assert_eq!(
            state
                .transient
                .current_page
                .as_ref()
                .and_then(|page| page.rows.first())
                .map(|row| (row.issue.id, row.issue.title.as_str())),
            Some((issue_id, "Fresh page applied behind help"))
        );
        assert_eq!(
            state.load_state(IssueWorkspaceDataKind::LocalPage),
            IssueWorkspaceLoadState::Fresh
        );
    }

    #[tokio::test]
    async fn dependency_picker_submits_exact_direction_and_keeps_server_error_visible() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let candidate_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 701, "Dependency source"),
        );
        app.open_issue_dependency_editor(pane_id);
        let generation = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => {
                state.generation(IssueWorkspaceDataKind::DependencyCandidates)
            }
            _ => panic!("Issues pane"),
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::DependencyCandidates,
            generation,
            request_identity: IssueWorkspaceRequestIdentity::DependencyCandidates {
                issue_id,
                query: String::new(),
            },
            outcome: Ok(IssueWorkspaceAsyncPayload::DependencyCandidates {
                requested_issue_id: issue_id,
                query: String::new(),
                page: IssueWorkspacePageV1 {
                    rows: vec![IssueWorkspaceRowV1 {
                        issue: fixture_issue(project_id, candidate_id, 702, "Bounded candidate"),
                        readiness: IssueWorkspaceReadinessV1::Ready,
                        open_blocker_count: 0,
                        dependent_count: 0,
                    }],
                    previous_cursor: None,
                    next_cursor: None,
                },
            }),
        });
        app.submit_issue_editor(pane_id, false);
        let (mutation_generation, mutation_identity) =
            match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => {
                    assert!(matches!(
                        state.transient.pending_mutation,
                        Some(IssuePendingMutation::AddDependency(ref pending))
                            if pending.request.issue_id == issue_id
                                && pending.request.depends_on_id == candidate_id
                                && pending.origin_issue_id == issue_id
                                && pending.direction == IssueDependencyDirectionV1::BlockedBy
                    ));
                    (
                        state.generation(IssueWorkspaceDataKind::Mutation),
                        state
                            .request_identity(IssueWorkspaceDataKind::Mutation)
                            .cloned()
                            .unwrap(),
                    )
                }
                _ => panic!("Issues pane"),
            };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::Mutation,
            generation: mutation_generation,
            request_identity: mutation_identity,
            outcome: Err(IssueWorkspaceAsyncError {
                message: "dependency cycle from server".to_string(),
                access_denied: false,
                workspace: Some(IssueWorkspaceErrorV1 {
                    code: rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::DependencyCycle,
                    issue_id: Some(issue_id),
                    expected_row_version: None,
                    actual_row_version: None,
                    retryable: false,
                    next_action: "choose another candidate".to_string(),
                }),
            }),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        let editor = state.transient.editor.as_ref().unwrap();
        assert_eq!(editor.dependency_issue_id, Some(candidate_id));
        assert_eq!(
            editor.dependency_candidates[0].issue.title,
            "Bounded candidate"
        );
        assert_eq!(
            editor.error.as_deref(),
            Some("dependency cycle from server")
        );
    }

    #[tokio::test]
    async fn dependency_picker_blocks_direction_reverses_exact_uuid_edge() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let candidate_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 703, "Dependency blocker"),
        );
        app.open_issue_dependency_editor(pane_id);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.dependency_candidates = vec![IssueWorkspaceRowV1 {
                issue: fixture_issue(project_id, candidate_id, 704, "Dependent candidate"),
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 0,
            }];
            editor.dependency_issue_id = Some(candidate_id);
        }
        app.handle_issue_workspace_editor_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        app.submit_issue_editor(pane_id, true);
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert!(matches!(
            state.transient.pending_mutation,
            Some(IssuePendingMutation::RemoveDependency(ref pending))
                if pending.request.issue_id == candidate_id
                    && pending.request.depends_on_id == issue_id
                    && pending.origin_issue_id == issue_id
                    && pending.direction == IssueDependencyDirectionV1::Blocks
        ));
    }

    #[tokio::test]
    async fn dependency_success_closes_picker_and_refetches_authoritative_graph_and_readiness() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let candidate_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 705, "Converging dependency source"),
        );
        app.open_issue_dependency_editor(pane_id);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.dependency_candidates = vec![IssueWorkspaceRowV1 {
                issue: fixture_issue(
                    project_id,
                    candidate_id,
                    706,
                    "Converging dependency candidate",
                ),
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 0,
            }];
            editor.dependency_issue_id = Some(candidate_id);
        }
        app.submit_issue_editor(pane_id, false);
        let (generation, request_identity) = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => (
                state.generation(IssueWorkspaceDataKind::Mutation),
                state
                    .request_identity(IssueWorkspaceDataKind::Mutation)
                    .cloned()
                    .unwrap(),
            ),
            _ => panic!("Issues pane"),
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::Mutation,
            generation,
            request_identity,
            outcome: Ok(IssueWorkspaceAsyncPayload::DependencyMutation(
                IssueDependencyMutationResultV1 {
                    issue_id,
                    depends_on_id: candidate_id,
                    changed: true,
                },
            )),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert!(state.transient.editor.is_none());
        assert!(state.transient.pending_mutation.is_none());
        for kind in [
            IssueWorkspaceDataKind::SelectedIssue,
            IssueWorkspaceDataKind::BlockedBy,
            IssueWorkspaceDataKind::Blocks,
            IssueWorkspaceDataKind::Events,
        ] {
            assert!(state.transient.in_flight.contains(&kind));
            assert_eq!(
                state.request_identity(kind),
                Some(&inspector_identity(kind, issue_id))
            );
        }
        assert!(
            state
                .transient
                .in_flight
                .contains(&IssueWorkspaceDataKind::LocalPage)
        );
    }

    #[tokio::test]
    async fn uncertain_dependency_add_and_remove_keep_retry_and_refetch_authoritative_state() {
        for remove in [false, true] {
            let mut app = with_session_list(0);
            app.poll.connected = true;
            let project_id = Uuid::new_v4();
            let issue_id = Uuid::new_v4();
            let candidate_id = Uuid::new_v4();
            app.tabs[app.active_tab].project_id = Some(project_id);
            let pane_id = app.open_or_focus_issues();
            let issue = fixture_issue(
                project_id,
                issue_id,
                if remove { 708 } else { 707 },
                if remove {
                    "Dependency removal lost response"
                } else {
                    "Dependency addition lost response"
                },
            );
            let candidate = fixture_issue(
                project_id,
                candidate_id,
                if remove { 710 } else { 709 },
                "Authoritative dependency candidate",
            );
            install_issue_page(&mut app, pane_id, issue.clone());
            if remove
                && let Some(Pane::Issues(state)) =
                    app.active_tab_mut().layout.find_pane_mut(pane_id)
                && let Some(row) = state
                    .transient
                    .current_page
                    .as_mut()
                    .and_then(|page| page.rows.first_mut())
            {
                row.readiness = IssueWorkspaceReadinessV1::Blocked;
                row.open_blocker_count = 1;
            }
            let request = IssueDependencyMutationRequestV1 {
                project_id,
                issue_id,
                depends_on_id: candidate_id,
            };
            let pending = IssuePendingDependencyMutation {
                request,
                origin_issue_id: issue_id,
                direction: IssueDependencyDirectionV1::BlockedBy,
            };
            let frozen = if remove {
                IssuePendingMutation::RemoveDependency(pending)
            } else {
                IssuePendingMutation::AddDependency(pending)
            };
            let (generation, request_identity) =
                match app.active_tab_mut().layout.find_pane_mut(pane_id) {
                    Some(Pane::Issues(state)) => {
                        let identity =
                            IssueWorkspaceRequestIdentity::Mutation(mutation_identity(&frozen));
                        state.transient.pending_mutation = Some(frozen.clone());
                        let generation = state
                            .begin_request(IssueWorkspaceDataKind::Mutation, identity.clone())
                            .expect("mutation request");
                        (generation, identity)
                    }
                    _ => panic!("Issues pane"),
                };
            app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: IssueWorkspaceDataKind::Mutation,
                generation,
                request_identity,
                outcome: Err(IssueWorkspaceAsyncError {
                    message: "server committed but response was lost".to_string(),
                    access_denied: false,
                    workspace: None,
                }),
            });

            let (requests, local_generation, local_identity) =
                match app.active_tab().layout.find_pane(pane_id) {
                    Some(Pane::Issues(state)) => {
                        assert_eq!(state.transient.pending_mutation, Some(frozen.clone()));
                        assert!(state.transient.mutation_retry_error.as_deref().is_some_and(
                            |message| {
                                message
                                    .contains("OUTCOME UNKNOWN — authoritative readback scheduled")
                            }
                        ));
                        for kind in [
                            IssueWorkspaceDataKind::SelectedIssue,
                            IssueWorkspaceDataKind::BlockedBy,
                            IssueWorkspaceDataKind::Blocks,
                            IssueWorkspaceDataKind::Events,
                            IssueWorkspaceDataKind::LocalPage,
                        ] {
                            assert!(state.transient.in_flight.contains(&kind));
                        }
                        let inspector = [
                            IssueWorkspaceDataKind::SelectedIssue,
                            IssueWorkspaceDataKind::BlockedBy,
                            IssueWorkspaceDataKind::Blocks,
                            IssueWorkspaceDataKind::Events,
                        ]
                        .map(|kind| {
                            (
                                kind,
                                state.generation(kind),
                                state.request_identity(kind).cloned().unwrap(),
                            )
                        });
                        (
                            inspector,
                            state.generation(IssueWorkspaceDataKind::LocalPage),
                            state
                                .request_identity(IssueWorkspaceDataKind::LocalPage)
                                .cloned()
                                .unwrap(),
                        )
                    }
                    _ => panic!("Issues pane"),
                };

            for (kind, generation, identity) in requests {
                let payload = match kind {
                    IssueWorkspaceDataKind::SelectedIssue => {
                        IssueWorkspaceAsyncPayload::SelectedIssue {
                            requested_issue_id: issue_id,
                            row: Some(IssueWorkspaceRowV1 {
                                issue: issue.clone(),
                                readiness: if remove {
                                    IssueWorkspaceReadinessV1::Ready
                                } else {
                                    IssueWorkspaceReadinessV1::Blocked
                                },
                                open_blocker_count: if remove { 0 } else { 1 },
                                dependent_count: 0,
                            }),
                        }
                    }
                    IssueWorkspaceDataKind::BlockedBy => IssueWorkspaceAsyncPayload::Dependencies {
                        requested_issue_id: issue_id,
                        direction: IssueDependencyDirectionV1::BlockedBy,
                        page_direction: PageDirectionV1::Forward,
                        cursor: None,
                        page: if remove {
                            IssueDependencyPageV1 {
                                items: Vec::new(),
                                previous_cursor: None,
                                next_cursor: None,
                            }
                        } else {
                            dependency_page(
                                project_id,
                                issue_id,
                                candidate.clone(),
                                IssueDependencyDirectionV1::BlockedBy,
                            )
                        },
                    },
                    IssueWorkspaceDataKind::Blocks => IssueWorkspaceAsyncPayload::Dependencies {
                        requested_issue_id: issue_id,
                        direction: IssueDependencyDirectionV1::Blocks,
                        page_direction: PageDirectionV1::Forward,
                        cursor: None,
                        page: IssueDependencyPageV1 {
                            items: Vec::new(),
                            previous_cursor: None,
                            next_cursor: None,
                        },
                    },
                    IssueWorkspaceDataKind::Events => IssueWorkspaceAsyncPayload::Events {
                        requested_issue_id: issue_id,
                        after_sequence: None,
                        page: IssueEventPageV1 {
                            events: vec![issue_event(&issue, 4)],
                            next_after_sequence: None,
                        },
                    },
                    _ => unreachable!(),
                };
                app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                    pane_id,
                    project_id: Some(project_id),
                    data_kind: kind,
                    generation,
                    request_identity: identity,
                    outcome: Ok(payload),
                });
            }
            let local_request = match local_identity.clone() {
                IssueWorkspaceRequestIdentity::Local(request) => request,
                _ => panic!("Local request identity"),
            };
            app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: IssueWorkspaceDataKind::LocalPage,
                generation: local_generation,
                request_identity: local_identity,
                outcome: Ok(IssueWorkspaceAsyncPayload::LocalPage {
                    cursor: local_request.cursor,
                    direction: local_request.direction,
                    page: IssueWorkspacePageV1 {
                        rows: vec![IssueWorkspaceRowV1 {
                            issue: issue.clone(),
                            readiness: if remove {
                                IssueWorkspaceReadinessV1::Ready
                            } else {
                                IssueWorkspaceReadinessV1::Blocked
                            },
                            open_blocker_count: if remove { 0 } else { 1 },
                            dependent_count: 0,
                        }],
                        previous_cursor: None,
                        next_cursor: None,
                    },
                }),
            });
            let state = match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            assert_eq!(state.transient.pending_mutation, Some(frozen));
            assert!(state.transient.mutation_retry_error.is_some());
            assert_eq!(
                state
                    .transient
                    .current_page
                    .as_ref()
                    .and_then(|page| page.rows.first())
                    .map(|row| (row.issue.id, row.readiness, row.open_blocker_count)),
                Some((
                    issue_id,
                    if remove {
                        IssueWorkspaceReadinessV1::Ready
                    } else {
                        IssueWorkspaceReadinessV1::Blocked
                    },
                    if remove { 0 } else { 1 },
                ))
            );
            assert_eq!(
                state
                    .transient
                    .blocked_by
                    .as_ref()
                    .map(|page| page.items.len()),
                Some(if remove { 0 } else { 1 })
            );
            assert_eq!(state.transient.events.len(), 1);
        }
    }

    #[tokio::test]
    async fn uncertain_dependency_defers_replacement_and_fences_cursors() {
        for remove in [false, true] {
            for direction in [
                IssueDependencyDirectionV1::BlockedBy,
                IssueDependencyDirectionV1::Blocks,
            ] {
                let mut app = with_session_list(0);
                app.poll.connected = true;
                let project_id = Uuid::new_v4();
                let origin_id = Uuid::new_v4();
                let candidate_id = Uuid::new_v4();
                let replacement_id = Uuid::new_v4();
                let origin_blocked_by_cursor = IssueDependencyCursorV1 {
                    display_number: 730,
                    issue_id: Uuid::new_v4(),
                };
                let origin_blocks_cursor = IssueDependencyCursorV1 {
                    display_number: 731,
                    issue_id: Uuid::new_v4(),
                };
                let replacement_blocked_by_cursor = IssueDependencyCursorV1 {
                    display_number: 740,
                    issue_id: Uuid::new_v4(),
                };
                let replacement_blocks_cursor = IssueDependencyCursorV1 {
                    display_number: 741,
                    issue_id: Uuid::new_v4(),
                };
                app.tabs[app.active_tab].project_id = Some(project_id);
                let origin_tab = app.active_tab;
                let pane_id = app.open_or_focus_issues();
                let origin = fixture_issue(
                    project_id,
                    origin_id,
                    720,
                    "Immutable dependency reconciliation origin",
                );
                let candidate = fixture_issue(
                    project_id,
                    candidate_id,
                    721,
                    "Authoritative dependency relation",
                );
                let replacement = fixture_issue(
                    project_id,
                    replacement_id,
                    722,
                    "Ready-filter replacement selection",
                );
                install_issue_page(&mut app, pane_id, origin.clone());
                let request = match direction {
                    IssueDependencyDirectionV1::BlockedBy => IssueDependencyMutationRequestV1 {
                        project_id,
                        issue_id: origin_id,
                        depends_on_id: candidate_id,
                    },
                    IssueDependencyDirectionV1::Blocks => IssueDependencyMutationRequestV1 {
                        project_id,
                        issue_id: candidate_id,
                        depends_on_id: origin_id,
                    },
                };
                let pending = IssuePendingDependencyMutation {
                    request,
                    origin_issue_id: origin_id,
                    direction,
                };
                let frozen = if remove {
                    IssuePendingMutation::RemoveDependency(pending)
                } else {
                    IssuePendingMutation::AddDependency(pending)
                };
                let (generation, request_identity) = {
                    let state = match app.tabs[origin_tab].layout.find_pane_mut(pane_id) {
                        Some(Pane::Issues(state)) => state,
                        _ => panic!("Issues pane"),
                    };
                    state.local.filters.readiness =
                        rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Ready;
                    state.local.inspector_open = true;
                    state.focus = IssueWorkspaceFocus::Inspector;
                    let identity =
                        IssueWorkspaceRequestIdentity::Mutation(mutation_identity(&frozen));
                    state.transient.pending_mutation = Some(frozen.clone());
                    let generation = state
                        .begin_request(IssueWorkspaceDataKind::Mutation, identity.clone())
                        .expect("mutation request");
                    (generation, identity)
                };

                app.create_tab();
                assert_ne!(app.active_tab, origin_tab);
                app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                    pane_id,
                    project_id: Some(project_id),
                    data_kind: IssueWorkspaceDataKind::Mutation,
                    generation,
                    request_identity,
                    outcome: Err(IssueWorkspaceAsyncError {
                        message: "server committed dependency but response was lost".to_string(),
                        access_denied: false,
                        workspace: None,
                    }),
                });

                let (inspector_requests, local_generation, local_identity) = {
                    let state = match app.tabs[origin_tab].layout.find_pane(pane_id) {
                        Some(Pane::Issues(state)) => state,
                        _ => panic!("Issues pane"),
                    };
                    assert_eq!(state.project_id, Some(project_id));
                    assert_eq!(state.transient.pending_mutation, Some(frozen.clone()));
                    let reconciliation = state
                        .transient
                        .dependency_reconciliation
                        .as_ref()
                        .expect("immutable dependency reconciliation");
                    assert_eq!(reconciliation.origin_issue_id, origin_id);
                    assert_eq!(reconciliation.direction, direction);
                    assert_eq!(
                        reconciliation.pending_kinds,
                        dependency_reconciliation_kinds().into_iter().collect()
                    );
                    let inspector = [
                        IssueWorkspaceDataKind::SelectedIssue,
                        IssueWorkspaceDataKind::BlockedBy,
                        IssueWorkspaceDataKind::Blocks,
                        IssueWorkspaceDataKind::Events,
                    ]
                    .map(|kind| {
                        assert!(state.transient.in_flight.contains(&kind));
                        assert_eq!(
                            state.request_identity(kind),
                            Some(&inspector_identity(kind, origin_id))
                        );
                        (
                            kind,
                            state.generation(kind),
                            state.request_identity(kind).cloned().unwrap(),
                        )
                    });
                    assert!(
                        state
                            .transient
                            .in_flight
                            .contains(&IssueWorkspaceDataKind::LocalPage)
                    );
                    (
                        inspector,
                        state.generation(IssueWorkspaceDataKind::LocalPage),
                        state
                            .request_identity(IssueWorkspaceDataKind::LocalPage)
                            .cloned()
                            .unwrap(),
                    )
                };

                let local_request = match local_identity.clone() {
                    IssueWorkspaceRequestIdentity::Local(identity) => identity,
                    _ => panic!("Local request identity"),
                };
                assert_eq!(local_request.project_id, project_id);
                assert_eq!(
                    local_request.filters.readiness,
                    rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Ready
                );
                app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                    pane_id,
                    project_id: Some(project_id),
                    data_kind: IssueWorkspaceDataKind::LocalPage,
                    generation: local_generation,
                    request_identity: local_identity,
                    outcome: Ok(IssueWorkspaceAsyncPayload::LocalPage {
                        cursor: local_request.cursor,
                        direction: local_request.direction,
                        page: IssueWorkspacePageV1 {
                            rows: vec![fixture_row(replacement.clone())],
                            previous_cursor: None,
                            next_cursor: None,
                        },
                    }),
                });

                {
                    let state = match app.tabs[origin_tab].layout.find_pane(pane_id) {
                        Some(Pane::Issues(state)) => state,
                        _ => panic!("Issues pane"),
                    };
                    assert_eq!(state.local.selected_issue_id, Some(replacement_id));
                    let reconciliation = state
                        .transient
                        .dependency_reconciliation
                        .as_ref()
                        .expect("inspector readback still pending");
                    assert_eq!(reconciliation.origin_issue_id, origin_id);
                    assert_eq!(reconciliation.pending_kinds.len(), 4);
                    for kind in [
                        IssueWorkspaceDataKind::SelectedIssue,
                        IssueWorkspaceDataKind::BlockedBy,
                        IssueWorkspaceDataKind::Blocks,
                        IssueWorkspaceDataKind::Events,
                    ] {
                        assert_eq!(
                            current_request_identity(state, kind),
                            Some(inspector_identity(kind, origin_id))
                        );
                    }
                    assert!(state.transient.mutation_retry_error.as_deref().is_some_and(
                        |message| {
                            message.contains("OUTCOME UNKNOWN — authoritative readback scheduled")
                        }
                    ));
                }

                let readiness = if direction == IssueDependencyDirectionV1::BlockedBy && !remove {
                    IssueWorkspaceReadinessV1::Blocked
                } else {
                    IssueWorkspaceReadinessV1::Ready
                };
                for (index, (kind, generation, identity)) in
                    inspector_requests.into_iter().enumerate()
                {
                    let payload = match kind {
                        IssueWorkspaceDataKind::SelectedIssue => {
                            IssueWorkspaceAsyncPayload::SelectedIssue {
                                requested_issue_id: origin_id,
                                row: Some(IssueWorkspaceRowV1 {
                                    issue: origin.clone(),
                                    readiness,
                                    open_blocker_count: u32::from(
                                        readiness == IssueWorkspaceReadinessV1::Blocked,
                                    ),
                                    dependent_count: 0,
                                }),
                            }
                        }
                        IssueWorkspaceDataKind::BlockedBy => {
                            IssueWorkspaceAsyncPayload::Dependencies {
                                requested_issue_id: origin_id,
                                direction: IssueDependencyDirectionV1::BlockedBy,
                                page_direction: PageDirectionV1::Forward,
                                cursor: None,
                                page: {
                                    let mut page = if direction
                                        == IssueDependencyDirectionV1::BlockedBy
                                        && !remove
                                    {
                                        dependency_page(
                                            project_id,
                                            origin_id,
                                            candidate.clone(),
                                            IssueDependencyDirectionV1::BlockedBy,
                                        )
                                    } else {
                                        IssueDependencyPageV1 {
                                            items: Vec::new(),
                                            previous_cursor: None,
                                            next_cursor: None,
                                        }
                                    };
                                    page.next_cursor = Some(origin_blocked_by_cursor.clone());
                                    page
                                },
                            }
                        }
                        IssueWorkspaceDataKind::Blocks => {
                            IssueWorkspaceAsyncPayload::Dependencies {
                                requested_issue_id: origin_id,
                                direction: IssueDependencyDirectionV1::Blocks,
                                page_direction: PageDirectionV1::Forward,
                                cursor: None,
                                page: {
                                    let mut page = if direction
                                        == IssueDependencyDirectionV1::Blocks
                                        && !remove
                                    {
                                        dependency_page(
                                            project_id,
                                            origin_id,
                                            candidate.clone(),
                                            IssueDependencyDirectionV1::Blocks,
                                        )
                                    } else {
                                        IssueDependencyPageV1 {
                                            items: Vec::new(),
                                            previous_cursor: None,
                                            next_cursor: None,
                                        }
                                    };
                                    page.next_cursor = Some(origin_blocks_cursor.clone());
                                    page
                                },
                            }
                        }
                        IssueWorkspaceDataKind::Events => IssueWorkspaceAsyncPayload::Events {
                            requested_issue_id: origin_id,
                            after_sequence: None,
                            page: IssueEventPageV1 {
                                events: vec![issue_event(&origin, 5)],
                                next_after_sequence: Some(64),
                            },
                        },
                        _ => unreachable!(),
                    };
                    app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                        pane_id,
                        project_id: Some(project_id),
                        data_kind: kind,
                        generation,
                        request_identity: identity,
                        outcome: Ok(payload),
                    });
                    if index < 3 {
                        let state = match app.tabs[origin_tab].layout.find_pane(pane_id) {
                            Some(Pane::Issues(state)) => state,
                            _ => panic!("Issues pane"),
                        };
                        assert_eq!(
                            state
                                .transient
                                .dependency_reconciliation
                                .as_ref()
                                .map(|value| value.origin_issue_id),
                            Some(origin_id)
                        );
                        assert!(
                            state
                                .transient
                                .mutation_retry_error
                                .as_deref()
                                .is_some_and(|message| message.contains("OUTCOME UNKNOWN"))
                        );
                    }
                }

                {
                    let state = match app.tabs[origin_tab].layout.find_pane(pane_id) {
                        Some(Pane::Issues(state)) => state,
                        _ => panic!("Issues pane"),
                    };
                    assert_eq!(state.local.selected_issue_id, Some(replacement_id));
                    assert_eq!(state.transient.pending_mutation, Some(frozen.clone()));
                    assert!(state.transient.dependency_reconciliation.is_none());
                    assert!(state.transient.mutation_retry_error.as_deref().is_some_and(
                        |message| {
                            message.contains("OUTCOME UNKNOWN — authoritative readback complete")
                        }
                    ));
                    assert_eq!(
                        state.transient.deferred_inspector_target,
                        Some(replacement_id)
                    );
                    assert_eq!(state.transient.selected_issue_target, Some(origin_id));
                    assert_eq!(state.transient.blocked_by_target, Some(origin_id));
                    assert_eq!(state.transient.blocks_target, Some(origin_id));
                    assert_eq!(state.transient.events_target, Some(origin_id));
                    assert_eq!(
                        state
                            .transient
                            .selected_issue
                            .as_ref()
                            .map(|issue| (issue.id, issue.title.as_str())),
                        Some((origin_id, "Immutable dependency reconciliation origin"))
                    );
                    let authoritative_relation = match direction {
                        IssueDependencyDirectionV1::BlockedBy => {
                            state.transient.blocked_by.as_ref().unwrap()
                        }
                        IssueDependencyDirectionV1::Blocks => {
                            state.transient.blocks.as_ref().unwrap()
                        }
                    };
                    assert_eq!(authoritative_relation.items.len(), usize::from(!remove));
                    if let Some(item) = authoritative_relation.items.first() {
                        assert_eq!(
                            (item.related_issue.id, item.related_issue.title.as_str()),
                            (candidate_id, "Authoritative dependency relation")
                        );
                    }
                }
                assert_ne!(app.active_tab, origin_tab);

                app.prev_tab();
                assert_eq!(app.active_tab, origin_tab);
                let origin_generation = match app.active_tab().layout.find_pane(pane_id) {
                    Some(Pane::Issues(state)) => {
                        state.generation(IssueWorkspaceDataKind::BlockedBy)
                    }
                    _ => panic!("Issues pane"),
                };
                app.navigate_issue_inspector_page(pane_id, true);
                let state = match app.active_tab().layout.find_pane(pane_id) {
                    Some(Pane::Issues(state)) => state,
                    _ => panic!("Issues pane"),
                };
                assert_eq!(
                    state.generation(IssueWorkspaceDataKind::BlockedBy),
                    origin_generation
                );
                assert_eq!(state.transient.blocked_by_target, Some(origin_id));

                app.issue_workspace_tick(now_ms());
                let replacement_requests = {
                    let state = match app.active_tab().layout.find_pane(pane_id) {
                        Some(Pane::Issues(state)) => state,
                        _ => panic!("Issues pane"),
                    };
                    assert_eq!(state.transient.deferred_inspector_target, None);
                    [
                        IssueWorkspaceDataKind::SelectedIssue,
                        IssueWorkspaceDataKind::BlockedBy,
                        IssueWorkspaceDataKind::Blocks,
                        IssueWorkspaceDataKind::Events,
                    ]
                    .map(|kind| {
                        assert!(state.transient.in_flight.contains(&kind));
                        assert_eq!(
                            state.request_identity(kind),
                            Some(&inspector_identity(kind, replacement_id))
                        );
                        (
                            kind,
                            state.generation(kind),
                            state.request_identity(kind).cloned().unwrap(),
                        )
                    })
                };

                for (kind, generation, identity) in replacement_requests {
                    let payload = match kind {
                        IssueWorkspaceDataKind::SelectedIssue => {
                            IssueWorkspaceAsyncPayload::SelectedIssue {
                                requested_issue_id: replacement_id,
                                row: Some(fixture_row(replacement.clone())),
                            }
                        }
                        IssueWorkspaceDataKind::BlockedBy => {
                            let mut page = dependency_page(
                                project_id,
                                replacement_id,
                                candidate.clone(),
                                IssueDependencyDirectionV1::BlockedBy,
                            );
                            page.next_cursor = Some(replacement_blocked_by_cursor.clone());
                            IssueWorkspaceAsyncPayload::Dependencies {
                                requested_issue_id: replacement_id,
                                direction: IssueDependencyDirectionV1::BlockedBy,
                                page_direction: PageDirectionV1::Forward,
                                cursor: None,
                                page,
                            }
                        }
                        IssueWorkspaceDataKind::Blocks => {
                            let mut page = dependency_page(
                                project_id,
                                replacement_id,
                                candidate.clone(),
                                IssueDependencyDirectionV1::Blocks,
                            );
                            page.next_cursor = Some(replacement_blocks_cursor.clone());
                            IssueWorkspaceAsyncPayload::Dependencies {
                                requested_issue_id: replacement_id,
                                direction: IssueDependencyDirectionV1::Blocks,
                                page_direction: PageDirectionV1::Forward,
                                cursor: None,
                                page,
                            }
                        }
                        IssueWorkspaceDataKind::Events => IssueWorkspaceAsyncPayload::Events {
                            requested_issue_id: replacement_id,
                            after_sequence: None,
                            page: IssueEventPageV1 {
                                events: vec![issue_event(&replacement, 96)],
                                next_after_sequence: Some(96),
                            },
                        },
                        _ => unreachable!(),
                    };
                    app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                        pane_id,
                        project_id: Some(project_id),
                        data_kind: kind,
                        generation,
                        request_identity: identity,
                        outcome: Ok(payload),
                    });
                }

                for (section, expected_identity) in [
                    (
                        crate::types::IssueInspectorSection::BlockedBy,
                        IssueWorkspaceRequestIdentity::Dependency {
                            issue_id: replacement_id,
                            direction: IssueDependencyDirectionV1::BlockedBy,
                            page_direction: PageDirectionV1::Forward,
                            cursor: Some(replacement_blocked_by_cursor.clone()),
                        },
                    ),
                    (
                        crate::types::IssueInspectorSection::Blocks,
                        IssueWorkspaceRequestIdentity::Dependency {
                            issue_id: replacement_id,
                            direction: IssueDependencyDirectionV1::Blocks,
                            page_direction: PageDirectionV1::Forward,
                            cursor: Some(replacement_blocks_cursor.clone()),
                        },
                    ),
                    (
                        crate::types::IssueInspectorSection::Events,
                        IssueWorkspaceRequestIdentity::Events {
                            issue_id: replacement_id,
                            after_sequence: Some(96),
                        },
                    ),
                ] {
                    if let Some(Pane::Issues(state)) =
                        app.active_tab_mut().layout.find_pane_mut(pane_id)
                    {
                        state.local.inspector_section = section;
                    }
                    app.navigate_issue_inspector_page(pane_id, true);
                    let state = match app.active_tab().layout.find_pane(pane_id) {
                        Some(Pane::Issues(state)) => state,
                        _ => panic!("Issues pane"),
                    };
                    let kind = match section {
                        crate::types::IssueInspectorSection::BlockedBy => {
                            IssueWorkspaceDataKind::BlockedBy
                        }
                        crate::types::IssueInspectorSection::Blocks => {
                            IssueWorkspaceDataKind::Blocks
                        }
                        crate::types::IssueInspectorSection::Events => {
                            IssueWorkspaceDataKind::Events
                        }
                    };
                    assert_eq!(state.request_identity(kind), Some(&expected_identity));
                }

                let state = match app.active_tab().layout.find_pane(pane_id) {
                    Some(Pane::Issues(state)) => state,
                    _ => panic!("Issues pane"),
                };
                assert_eq!(state.transient.selected_issue_target, Some(replacement_id));
                assert_eq!(state.transient.blocked_by_target, Some(replacement_id));
                assert_eq!(state.transient.blocks_target, Some(replacement_id));
                assert_eq!(state.transient.events_target, Some(replacement_id));
                assert_eq!(state.transient.pending_mutation, Some(frozen));
            }
        }
    }

    fn clear_mutation_state(app: &mut App, pane_id: PaneId) {
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state
                .transient
                .in_flight
                .remove(&IssueWorkspaceDataKind::Mutation);
            state.transient.pending_mutation = None;
            state.transient.editor = None;
        }
    }

    #[tokio::test]
    async fn archive_confirmation_enter_uses_registered_route_and_exact_terminal_uuid() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = with_session_list(0);
        app.poll.connected = true;
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        let mut terminal = fixture_issue(
            project_id,
            issue_id,
            800,
            "Registered archive confirmation identity",
        );
        terminal.status = IssueStatus::Closed;
        terminal.closed_at = Some(chrono::Utc::now());
        install_issue_page(&mut app, pane_id, terminal);

        crate::event::step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
        )
        .await;
        let context = crate::action_registry::ActionContext::from_app(&app);
        assert_eq!(
            context.mode,
            crate::action_registry::ActionMode::PendingArchive
        );
        assert_eq!(
            crate::action_registry::request_for_key(
                &context,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            ),
            crate::action_registry::ActionAvailability::Available(
                crate::action_registry::ActionRequest::plain(
                    crate::action_registry::ActionId::IssueConfirmArchive,
                ),
            )
        );
        crate::event::step_once(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.transient.archive_confirmation, None);
        assert!(matches!(
            state.transient.pending_mutation,
            Some(IssuePendingMutation::Archive(ref request))
                if request.project_id == project_id
                    && request.issue_id == issue_id
                    && request.expected_row_version == 3
        ));
        assert_eq!(
            state.transient.current_page.as_ref().unwrap().rows[0]
                .issue
                .title,
            "Registered archive confirmation identity"
        );
    }

    #[tokio::test]
    async fn dependency_action_opens_for_terminal_and_archived_store_authoritative_endpoints() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for archived in [false, true] {
            let mut app = with_session_list(0);
            app.poll.connected = true;
            let project_id = Uuid::new_v4();
            let issue_id = Uuid::new_v4();
            app.tabs[app.active_tab].project_id = Some(project_id);
            let pane_id = app.open_or_focus_issues();
            let mut terminal = fixture_issue(
                project_id,
                issue_id,
                if archived { 812 } else { 811 },
                if archived {
                    "Archived dependency endpoint identity"
                } else {
                    "Terminal dependency endpoint identity"
                },
            );
            terminal.status = IssueStatus::Closed;
            terminal.closed_at = Some(chrono::Utc::now());
            terminal.archived_at = archived.then(chrono::Utc::now);
            install_issue_page(&mut app, pane_id, terminal.clone());

            crate::event::step_once(
                &mut app,
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            )
            .await;
            let editor = match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => state
                    .transient
                    .editor
                    .as_ref()
                    .expect("dependency editor opens for Store-authoritative endpoint"),
                _ => panic!("Issues pane"),
            };
            assert!(matches!(
                editor.mode,
                IssueEditorMode::Dependency { issue_id: target } if target == issue_id
            ));
            let base = editor.base_issue.as_ref().expect("endpoint projection");
            assert_eq!(base.title, terminal.title);
            assert_eq!(base.archived_at.is_some(), archived);
        }
    }

    #[tokio::test]
    async fn inspector_and_sync_navigation_update_durable_scroll_without_resetting_tab_offsets() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 813, "Durable scroll identity"),
        );
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.local.inspector_open = true;
            state.focus = IssueWorkspaceFocus::Inspector;
            state.local.scroll_offset = 4;
            state.dispatched.scroll_offset = 3;
        }
        app.move_issue_selection(pane_id, 1);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            assert_eq!(state.local.inspector_scroll_offset, 1);
            state.active_tab = IssueWorkspaceTab::Sync;
            state
                .data_state_mut(IssueWorkspaceDataKind::SyncStatus)
                .load_state = IssueWorkspaceLoadState::Fresh;
        }
        for key in [KeyCode::Char('j'), KeyCode::Char('j'), KeyCode::Char('k')] {
            crate::event::step_once(&mut app, KeyEvent::new(key, KeyModifiers::NONE)).await;
        }
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.sync.scroll_offset, 1);
        assert_eq!(state.local.scroll_offset, 4);
        assert_eq!(state.dispatched.scroll_offset, 3);
        assert_eq!(state.local.selected_issue_id, Some(issue_id));
        assert_eq!(
            state.transient.current_page.as_ref().unwrap().rows[0]
                .issue
                .title,
            "Durable scroll identity"
        );
    }

    #[tokio::test]
    async fn transport_failure_keeps_editor_and_retries_byte_identical_wire_envelope() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::net::UnixListener;

        let directory = crate::test_support::short_socket_dir("rsi-issue-mutation-retry-");
        let socket_path = directory.path().join("issue-mutation-retry.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (wire_tx, mut wire_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                wire_tx
                    .send(serde_json::from_str::<serde_json::Value>(line.trim()).unwrap())
                    .unwrap();
            }
        });

        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        app.client = crate::client::DaemonClient::new(socket_path);
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 799, "Frozen retry Issue identity"),
        );
        app.open_issue_edit_editor(pane_id, 0);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.title = "Frozen retry revised identity".to_string();
            editor.body = "Frozen retry body".to_string();
            editor.dirty = true;
        }
        app.submit_issue_editor(pane_id, false);
        let frozen = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state.transient.pending_mutation.clone().unwrap(),
            _ => panic!("Issues pane"),
        };
        let first_wire = tokio::time::timeout(std::time::Duration::from_secs(2), wire_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let first_event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let event = app.issues_rx.recv().await.unwrap();
                if event.data_kind == IssueWorkspaceDataKind::Mutation {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        app.apply_issue_workspace_event(first_event);

        app.handle_issue_workspace_editor_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueNew)
            .await;
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.transient.pending_mutation, Some(frozen.clone()));
        let editor = state
            .transient
            .editor
            .as_ref()
            .expect("retry editor retained");
        assert_eq!(editor.title, "Frozen retry revised identity");
        assert!(
            editor
                .error
                .as_deref()
                .is_some_and(|error| error.contains("form cannot be replaced"))
        );

        app.handle_issue_workspace_editor_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL))
            .await;
        let second_wire = tokio::time::timeout(std::time::Duration::from_secs(2), wire_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first_wire.get("method"),
            Some(&serde_json::json!("UpdateIssue"))
        );
        assert_eq!(second_wire.get("method"), first_wire.get("method"));
        assert_eq!(second_wire.get("params"), first_wire.get("params"));
        let frozen_json = match &frozen {
            IssuePendingMutation::Update(request) => serde_json::to_value(request).unwrap(),
            other => panic!("expected frozen update request, got {other:?}"),
        };
        assert_eq!(first_wire.get("params"), Some(&frozen_json));
        assert_eq!(
            first_wire
                .get("params")
                .and_then(|params| params.get("idempotency_key")),
            second_wire
                .get("params")
                .and_then(|params| params.get("idempotency_key"))
        );
        let second_event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let event = app.issues_rx.recv().await.unwrap();
                if event.data_kind == IssueWorkspaceDataKind::Mutation {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        app.apply_issue_workspace_event(second_event);
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.transient.pending_mutation.as_ref() == Some(&frozen)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn create_edit_metadata_archive_and_restore_actions_freeze_exact_requests() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.in_flight.clear();
        }

        app.open_issue_create_editor(pane_id);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.title = "Created visible issue".to_string();
            editor.body = "Created visible body".to_string();
            editor.priority = Some(1);
            editor.assignee = Some("owner-a".to_string());
            editor.labels = vec!["created-label".to_string()];
        }
        app.submit_issue_editor(pane_id, false);
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if matches!(
                    state.transient.pending_mutation,
                    Some(IssuePendingMutation::Create(ref request))
                        if request.title == "Created visible issue"
                            && request.body == "Created visible body"
                            && request.priority == Some(1)
                            && request.assignee.as_deref() == Some("owner-a")
                            && request.labels == vec!["created-label"]
                )
        ));

        clear_mutation_state(&mut app, pane_id);
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 801, "Original edit identity"),
        );
        app.open_issue_edit_editor(pane_id, 0);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.title = "Edited visible identity".to_string();
            editor.body = "Edited visible body".to_string();
            editor.priority = Some(4);
            editor.assignee = Some("owner-b".to_string());
            editor.labels = vec!["edited-label".to_string(), "audit".to_string()];
        }
        app.submit_issue_editor(pane_id, false);
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if matches!(
                    state.transient.pending_mutation,
                    Some(IssuePendingMutation::Update(ref request))
                        if request.issue_id == issue_id
                            && request.expected_row_version == 3
                            && request.patch.title.as_deref() == Some("Edited visible identity")
                            && request.patch.body.as_deref() == Some("Edited visible body")
                            && request.patch.labels.as_deref() == Some(&["edited-label".to_string(), "audit".to_string()][..])
                )
        ));

        clear_mutation_state(&mut app, pane_id);
        let mut terminal = fixture_issue(project_id, issue_id, 801, "Terminal archive identity");
        terminal.status = IssueStatus::Closed;
        terminal.closed_at = Some(chrono::Utc::now());
        install_issue_page(&mut app, pane_id, terminal.clone());
        app.arm_issue_archive(pane_id);
        app.submit_archive_confirmation(pane_id);
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if matches!(
                    state.transient.pending_mutation,
                    Some(IssuePendingMutation::Archive(ref request))
                        if request.issue_id == issue_id && request.expected_row_version == 3
                )
        ));

        clear_mutation_state(&mut app, pane_id);
        terminal.archived_at = Some(chrono::Utc::now());
        terminal.row_version = 4;
        install_issue_page(&mut app, pane_id, terminal);
        app.submit_restore(pane_id);
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if matches!(
                    state.transient.pending_mutation,
                    Some(IssuePendingMutation::Restore(ref request))
                        if request.issue_id == issue_id && request.expected_row_version == 4
                )
        ));
    }

    #[tokio::test]
    async fn cancel_then_open_applies_authoritative_issue_and_both_visible_events() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 802, "Cancel reopen identity"),
        );
        app.arm_issue_cancel(pane_id);
        app.submit_issue_cancel(pane_id);
        let (generation, identity) = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => (
                state.generation(IssueWorkspaceDataKind::Mutation),
                state
                    .request_identity(IssueWorkspaceDataKind::Mutation)
                    .cloned()
                    .unwrap(),
            ),
            _ => panic!("Issues pane"),
        };
        let mut cancelled = fixture_issue(project_id, issue_id, 802, "Cancel reopen identity");
        cancelled.status = IssueStatus::Cancelled;
        cancelled.closed_at = Some(chrono::Utc::now());
        cancelled.row_version = 4;
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::Mutation,
            generation,
            request_identity: identity,
            outcome: Ok(IssueWorkspaceAsyncPayload::Mutation(
                OperatorIssueMutationResultV1 {
                    issue: cancelled.clone(),
                    event: issue_event(&cancelled, 1),
                    deduplicated: false,
                },
            )),
        });
        app.open_issue_reopen_editor(pane_id);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.status = Some(IssueStatus::Open);
            editor.dirty = true;
        }
        app.submit_issue_editor(pane_id, false);
        let (generation, identity) = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => {
                assert!(matches!(
                    state.transient.pending_mutation,
                    Some(IssuePendingMutation::Status(ref request))
                        if request.issue_id == issue_id
                            && request.status == IssueStatus::Open
                            && request.expected_row_version == 4
                ));
                (
                    state.generation(IssueWorkspaceDataKind::Mutation),
                    state
                        .request_identity(IssueWorkspaceDataKind::Mutation)
                        .cloned()
                        .unwrap(),
                )
            }
            _ => panic!("Issues pane"),
        };
        let mut reopened = cancelled;
        reopened.status = IssueStatus::Open;
        reopened.closed_at = None;
        reopened.row_version = 5;
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::Mutation,
            generation,
            request_identity: identity,
            outcome: Ok(IssueWorkspaceAsyncPayload::Mutation(
                OperatorIssueMutationResultV1 {
                    issue: reopened,
                    event: {
                        let mut event = issue_event(
                            &fixture_issue(project_id, issue_id, 802, "Cancel reopen identity"),
                            2,
                        );
                        event.issue.status = IssueStatus::Open;
                        event.issue.row_version = 5;
                        event
                    },
                    deduplicated: false,
                },
            )),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(
            state
                .transient
                .selected_issue
                .as_ref()
                .map(|issue| issue.status),
            Some(IssueStatus::Open)
        );
        assert_eq!(
            state
                .transient
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            state.transient.events[0].issue.status,
            IssueStatus::Cancelled
        );
        assert_eq!(state.transient.events[1].issue.status, IssueStatus::Open);
    }

    #[tokio::test]
    async fn same_identity_mutation_and_inspector_refresh_intents_restart_after_old_completion() {
        let mut app = with_session_list(0);
        app.poll.connected = true;
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        let issue = fixture_issue(project_id, issue_id, 901, "Newest mutation projection");
        install_issue_page(&mut app, pane_id, issue.clone());
        let local_identity = match app.active_tab_mut().layout.find_pane_mut(pane_id) {
            Some(Pane::Issues(state)) => {
                state.local.inspector_open = true;
                state.focus = IssueWorkspaceFocus::Inspector;
                state.transient.generations.clear();
                state.transient.desired_requests.clear();
                let identity =
                    IssueWorkspaceRequestIdentity::Local(local_request_identity(project_id, state));
                assert_eq!(
                    state.begin_request(IssueWorkspaceDataKind::LocalPage, identity.clone()),
                    Some(1)
                );
                let mutation = IssuePendingMutation::Status(UpdateIssueStatusV2RequestV1 {
                    project_id,
                    issue_id,
                    expected_row_version: issue.row_version,
                    idempotency_key: "same-identity-mutation".to_string(),
                    status: IssueStatus::InProgress,
                });
                state.transient.pending_mutation = Some(mutation.clone());
                assert_eq!(
                    state.begin_request(
                        IssueWorkspaceDataKind::Mutation,
                        IssueWorkspaceRequestIdentity::Mutation(mutation_identity(&mutation)),
                    ),
                    Some(1)
                );
                identity
            }
            _ => panic!("Issues pane"),
        };
        let mut updated = issue.clone();
        updated.status = IssueStatus::InProgress;
        updated.row_version += 1;
        let mutation_event = issue_event(&updated, 2);
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::Mutation,
            generation: 1,
            request_identity: IssueWorkspaceRequestIdentity::Mutation(
                "same-identity-mutation".to_string(),
            ),
            outcome: Ok(IssueWorkspaceAsyncPayload::Mutation(
                OperatorIssueMutationResultV1 {
                    issue: updated.clone(),
                    event: mutation_event,
                    deduplicated: false,
                },
            )),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.generation(IssueWorkspaceDataKind::LocalPage), 2);
        assert!(
            state
                .transient
                .in_flight
                .contains(&IssueWorkspaceDataKind::LocalPage)
        );
        for kind in [
            IssueWorkspaceDataKind::SelectedIssue,
            IssueWorkspaceDataKind::BlockedBy,
            IssueWorkspaceDataKind::Blocks,
            IssueWorkspaceDataKind::Events,
        ] {
            assert_eq!(state.generation(kind), 1);
        }

        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueRefresh)
            .await;
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.generation(IssueWorkspaceDataKind::LocalPage), 3);
        assert_eq!(state.generation(IssueWorkspaceDataKind::SelectedIssue), 2);

        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::LocalPage,
            generation: 1,
            request_identity: local_identity.clone(),
            outcome: Ok(IssueWorkspaceAsyncPayload::LocalPage {
                cursor: None,
                direction: PageDirectionV1::Forward,
                page: IssueWorkspacePageV1 {
                    rows: vec![fixture_row(fixture_issue(
                        project_id,
                        Uuid::new_v4(),
                        902,
                        "Old completion must restart",
                    ))],
                    previous_cursor: None,
                    next_cursor: None,
                },
            }),
        });
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::SelectedIssue,
            generation: 1,
            request_identity: IssueWorkspaceRequestIdentity::Issue(issue_id),
            outcome: Ok(IssueWorkspaceAsyncPayload::SelectedIssue {
                requested_issue_id: issue_id,
                row: Some(fixture_row(fixture_issue(
                    project_id,
                    issue_id,
                    901,
                    "Old inspector completion",
                ))),
            }),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.generation(IssueWorkspaceDataKind::LocalPage), 4);
        assert_eq!(state.generation(IssueWorkspaceDataKind::SelectedIssue), 3);
        assert_eq!(
            state
                .transient
                .selected_issue
                .as_ref()
                .map(|issue| &issue.title),
            Some(&updated.title)
        );
    }

    #[tokio::test]
    async fn inspector_bounded_page_actions_retain_dependency_and_event_cursors() {
        let mut app = with_session_list(0);
        app.poll.connected = true;
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 910, "Paged inspector identity"),
        );
        let next = IssueDependencyCursorV1 {
            display_number: 64,
            issue_id: Uuid::new_v4(),
        };
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.local.inspector_open = true;
            state.focus = IssueWorkspaceFocus::Inspector;
            state.local.inspector_section = crate::types::IssueInspectorSection::BlockedBy;
            state.transient.blocked_by_target = Some(issue_id);
            state.transient.blocked_by = Some(IssueDependencyPageV1 {
                items: Vec::new(),
                previous_cursor: None,
                next_cursor: Some(next.clone()),
            });
        }
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueInspectorNextPage)
            .await;
        let (dependency_generation, dependency_identity) =
            match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => (
                    state.generation(IssueWorkspaceDataKind::BlockedBy),
                    state
                        .request_identity(IssueWorkspaceDataKind::BlockedBy)
                        .cloned()
                        .unwrap(),
                ),
                _ => panic!("Issues pane"),
            };
        assert_eq!(
            dependency_identity,
            IssueWorkspaceRequestIdentity::Dependency {
                issue_id,
                direction: IssueDependencyDirectionV1::BlockedBy,
                page_direction: PageDirectionV1::Forward,
                cursor: Some(next.clone()),
            }
        );
        let later = fixture_issue(project_id, Uuid::new_v4(), 265, "Dependency after first 64");
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::BlockedBy,
            generation: dependency_generation,
            request_identity: dependency_identity,
            outcome: Ok(IssueWorkspaceAsyncPayload::Dependencies {
                requested_issue_id: issue_id,
                direction: IssueDependencyDirectionV1::BlockedBy,
                page_direction: PageDirectionV1::Forward,
                cursor: Some(next),
                page: dependency_page(
                    project_id,
                    issue_id,
                    later.clone(),
                    IssueDependencyDirectionV1::BlockedBy,
                ),
            }),
        });
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            assert_eq!(
                state.transient.blocked_by.as_ref().unwrap().items[0]
                    .related_issue
                    .title,
                later.title
            );
            state.local.inspector_section = crate::types::IssueInspectorSection::Events;
            state.transient.events_target = Some(issue_id);
            state.transient.events_next_after_sequence = Some(64);
        }
        app.handle_issue_workspace_action(crate::action_registry::ActionId::IssueInspectorNextPage)
            .await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.request_identity(IssueWorkspaceDataKind::Events)
                    == Some(&IssueWorkspaceRequestIdentity::Events {
                        issue_id,
                        after_sequence: Some(64),
                    })
        ));
    }

    #[tokio::test]
    async fn plain_enter_selects_filter_status_and_dependency_values_through_registered_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = with_session_list(0);
        app.poll.connected = true;
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        install_issue_page(
            &mut app,
            pane_id,
            fixture_issue(project_id, issue_id, 920, "Form selection identity"),
        );

        crate::event::step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
        )
        .await;
        crate::event::step_once(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)).await;
        crate::event::step_once(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.transient.editor.as_ref().and_then(|editor| editor.error.as_deref())
                    == Some("Selected saved view: Ready")
        ));

        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.editor = None;
            state.focus = IssueWorkspaceFocus::Table;
        }
        crate::event::step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT),
        )
        .await;
        crate::event::step_once(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)).await;
        crate::event::step_once(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.transient.editor.as_ref().and_then(|editor| editor.error.as_deref())
                    == Some("Selected status: InProgress")
        ));

        let candidate_id = Uuid::new_v4();
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.editor = None;
            state.focus = IssueWorkspaceFocus::Table;
        }
        crate::event::step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
        )
        .await;
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.active_field = 2;
            editor.dependency_candidates = vec![fixture_row(fixture_issue(
                project_id,
                candidate_id,
                921,
                "Selected dependency candidate",
            ))];
            editor.dependency_selected_row = 0;
        }
        crate::event::step_once(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(Pane::Issues(state))
                if state.transient.editor.as_ref().is_some_and(|editor|
                    editor.dependency_issue_id == Some(candidate_id)
                        && editor.error.as_deref().is_some_and(|error|
                            error.contains("Selected dependency: #921 Selected dependency candidate")))
        ));
    }

    #[tokio::test]
    async fn unknown_project_create_definitively_releases_envelope_and_retains_visible_draft() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id) {
            state.transient.in_flight.clear();
        }
        app.open_issue_create_editor(pane_id);
        if let Some(Pane::Issues(state)) = app.active_tab_mut().layout.find_pane_mut(pane_id)
            && let Some(editor) = &mut state.transient.editor
        {
            editor.title = "Unknown project retained draft".to_string();
            editor.body = "Recoverable draft body".to_string();
            editor.dirty = true;
        }
        app.submit_issue_editor(pane_id, false);
        let (generation, identity) = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => (
                state.generation(IssueWorkspaceDataKind::Mutation),
                state
                    .request_identity(IssueWorkspaceDataKind::Mutation)
                    .cloned()
                    .unwrap(),
            ),
            _ => panic!("Issues pane"),
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::Mutation,
            generation,
            request_identity: identity,
            outcome: Err(IssueWorkspaceAsyncError {
                message: "issue_workspace_error: project unavailable".to_string(),
                access_denied: false,
                workspace: Some(IssueWorkspaceErrorV1 {
                    code: rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::NotFoundInProject,
                    issue_id: Some(rsi_common::issue_workspace::operator_issue_create_id(
                        project_id,
                        "unknown-project-deterministic",
                    )),
                    expected_row_version: None,
                    actual_row_version: None,
                    retryable: false,
                    next_action: "refresh the project Issue list".to_string(),
                }),
            }),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert!(state.transient.pending_mutation.is_none());
        let editor = state.transient.editor.as_ref().expect("draft retained");
        assert_eq!(editor.title, "Unknown project retained draft");
        assert_eq!(editor.body, "Recoverable draft body");
        assert_eq!(
            editor.error.as_deref(),
            Some("issue_workspace_error: project unavailable")
        );
        assert!(!editor.submitted);
    }

    #[tokio::test]
    async fn cancel_archive_restore_transport_uncertainty_use_normal_identical_retry_route() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for (variant, code) in [
            (
                "cancel",
                rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::InvalidTransition,
            ),
            (
                "archive",
                rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::NotTerminal,
            ),
            (
                "restore",
                rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::NotArchived,
            ),
        ] {
            let mut app = with_session_list(0);
            app.poll.connected = true;
            let project_id = Uuid::new_v4();
            let issue_id = Uuid::new_v4();
            app.tabs[app.active_tab].project_id = Some(project_id);
            let pane_id = app.open_or_focus_issues();
            let mut issue = fixture_issue(
                project_id,
                issue_id,
                930,
                &format!("{variant} immutable retry identity"),
            );
            if variant != "cancel" {
                issue.status = IssueStatus::Closed;
                issue.closed_at = Some(chrono::Utc::now());
            }
            if variant == "restore" {
                issue.archived_at = Some(chrono::Utc::now());
            }
            install_issue_page(&mut app, pane_id, issue);
            match variant {
                "cancel" => {
                    app.arm_issue_cancel(pane_id);
                    app.submit_issue_cancel(pane_id);
                }
                "archive" => {
                    app.arm_issue_archive(pane_id);
                    app.submit_archive_confirmation(pane_id);
                }
                "restore" => app.submit_restore(pane_id),
                _ => unreachable!(),
            }
            let (frozen, first_generation, identity) =
                match app.active_tab().layout.find_pane(pane_id) {
                    Some(Pane::Issues(state)) => (
                        state.transient.pending_mutation.clone().unwrap(),
                        state.generation(IssueWorkspaceDataKind::Mutation),
                        state
                            .request_identity(IssueWorkspaceDataKind::Mutation)
                            .cloned()
                            .unwrap(),
                    ),
                    _ => panic!("Issues pane"),
                };
            app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: IssueWorkspaceDataKind::Mutation,
                generation: first_generation,
                request_identity: identity.clone(),
                outcome: Err(IssueWorkspaceAsyncError {
                    message: format!("{variant} transport uncertain"),
                    access_denied: false,
                    workspace: None,
                }),
            });
            assert!(matches!(
                app.active_tab().layout.find_pane(pane_id),
                Some(Pane::Issues(state))
                    if state.transient.pending_mutation.as_ref() == Some(&frozen)
                        && state.transient.mutation_retry_error.as_deref().is_some_and(|error|
                            error.contains(&format!("{variant} transport uncertain")))
            ));
            crate::event::step_once(
                &mut app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            )
            .await;
            let retry_generation = match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => {
                    assert_eq!(state.transient.pending_mutation, Some(frozen.clone()));
                    assert_eq!(
                        state.request_identity(IssueWorkspaceDataKind::Mutation),
                        Some(&identity)
                    );
                    assert!(
                        state
                            .transient
                            .in_flight
                            .contains(&IssueWorkspaceDataKind::Mutation)
                    );
                    state.generation(IssueWorkspaceDataKind::Mutation)
                }
                _ => panic!("Issues pane"),
            };
            assert!(retry_generation > first_generation);
            app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
                pane_id,
                project_id: Some(project_id),
                data_kind: IssueWorkspaceDataKind::Mutation,
                generation: retry_generation,
                request_identity: identity,
                outcome: Err(IssueWorkspaceAsyncError {
                    message: format!("{variant} definitively rejected"),
                    access_denied: false,
                    workspace: Some(IssueWorkspaceErrorV1 {
                        code,
                        issue_id: Some(issue_id),
                        expected_row_version: Some(3),
                        actual_row_version: Some(4),
                        retryable: false,
                        next_action: "refresh the Issue".to_string(),
                    }),
                }),
            });
            assert!(matches!(
                app.active_tab().layout.find_pane(pane_id),
                Some(Pane::Issues(state))
                    if state.transient.pending_mutation.is_none()
                        && state.transient.mutation_retry_error.is_none()
                        && state.data_error(IssueWorkspaceDataKind::Mutation)
                            .is_some_and(|error| error.contains(&format!("{code:?}")))
            ));
            let state = match app.active_tab().layout.find_pane(pane_id) {
                Some(Pane::Issues(state)) => state,
                _ => panic!("Issues pane"),
            };
            let row = state
                .transient
                .current_page
                .as_ref()
                .and_then(|page| page.rows.first())
                .expect("authoritative row retained after definitive failure");
            assert_eq!(
                row.issue.title,
                format!("{variant} immutable retry identity")
            );
            assert_eq!(
                row.issue.status,
                if variant == "cancel" {
                    IssueStatus::Open
                } else {
                    IssueStatus::Closed
                }
            );
            assert_eq!(row.issue.archived_at.is_some(), variant == "restore");
            let notification = app.notifications.back().expect("definitive failure toast");
            assert!(notification.message.contains("ISSUE WRITE FAILED"));
            assert!(notification.message.contains(&format!("{code:?}")));
            assert!(
                notification
                    .message
                    .contains(&format!("{variant} definitively rejected"))
            );
        }
    }

    #[tokio::test]
    async fn normal_pane_stale_version_is_visible_and_preserves_issue_axes() {
        let mut app = with_session_list(0);
        app.poll.connected = true;
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        let issue = fixture_issue(project_id, issue_id, 931, "Stale cancel identity retained");
        install_issue_page(&mut app, pane_id, issue.clone());
        app.arm_issue_cancel(pane_id);
        app.submit_issue_cancel(pane_id);
        let (generation, identity) = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => (
                state.generation(IssueWorkspaceDataKind::Mutation),
                state
                    .request_identity(IssueWorkspaceDataKind::Mutation)
                    .cloned()
                    .unwrap(),
            ),
            _ => panic!("Issues pane"),
        };
        app.apply_issue_workspace_event(IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: IssueWorkspaceDataKind::Mutation,
            generation,
            request_identity: identity,
            outcome: Err(IssueWorkspaceAsyncError {
                message: "selected row version advanced".to_string(),
                access_denied: false,
                workspace: Some(IssueWorkspaceErrorV1 {
                    code: rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1::StaleVersion,
                    issue_id: Some(issue_id),
                    expected_row_version: Some(3),
                    actual_row_version: Some(4),
                    retryable: false,
                    next_action: "refresh the Issue and retry".to_string(),
                }),
            }),
        });
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert!(state.transient.pending_mutation.is_none());
        assert_eq!(state.local.selected_issue_id, Some(issue_id));
        let row = state
            .transient
            .current_page
            .as_ref()
            .and_then(|page| page.rows.first())
            .expect("authoritative row retained");
        assert_eq!(row.issue.title, "Stale cancel identity retained");
        assert_eq!(row.issue.status, IssueStatus::Open);
        assert!(row.issue.archived_at.is_none());
        assert!(
            state
                .data_error(IssueWorkspaceDataKind::Mutation)
                .is_some_and(|error| error.contains("StaleVersion"))
        );
        let notification = app.notifications.back().expect("stale failure toast");
        assert!(notification.message.contains("StaleVersion"));
        assert!(
            notification
                .message
                .contains("selected row version advanced")
        );
    }
}
