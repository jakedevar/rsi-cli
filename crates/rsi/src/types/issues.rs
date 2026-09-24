//! Durable and transient state for the first-class Issue workspace pane.

use std::collections::{BTreeMap, BTreeSet};

use rsi_common::issue_workspace::{
    ArchiveIssueRequestV1, CreateIssueV2RequestV1, IssueDependencyCursorV1,
    IssueDependencyDirectionV1, IssueDependencyMutationRequestV1, IssueDependencyPageV1,
    IssueDispatchRecordV1, IssueTrackerStatusV1, IssueTrackerTickResultV1, IssueWorkspaceCursorV1,
    IssueWorkspacePageV1, IssueWorkspaceProvenanceV1, IssueWorkspaceReadinessFilterV1,
    IssueWorkspaceSortV1, PageDirectionV1, RestoreIssueRequestV1, UpdateIssueRequestV1,
    UpdateIssueStatusV2RequestV1,
};
use rsi_common::types::{Issue, IssueArchiveFilterV1, IssueEventV1, IssueStatus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ISSUE_WORKSPACE_PAGE_CACHE_LIMIT: usize = 3;
pub const ISSUE_WORKSPACE_FOCUSED_REFRESH_MS: i64 = 5_000;
pub const ISSUE_WORKSPACE_STALE_AFTER_MS: i64 = 15_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWorkspaceTab {
    #[default]
    Local,
    Dispatched,
    Sync,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWorkspaceFocus {
    #[default]
    Table,
    Inspector,
    Editor,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueInspectorSection {
    #[default]
    BlockedBy,
    Blocks,
    Events,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IssueWorkspaceLoadState {
    #[default]
    Loading,
    Fresh,
    Stale,
    Error,
    AccessDenied,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueLocalFilters {
    #[serde(default)]
    pub statuses: Vec<IssueStatus>,
    #[serde(default)]
    pub priorities: Vec<u8>,
    #[serde(default)]
    pub readiness: IssueWorkspaceReadinessFilterV1,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub unassigned: bool,
    #[serde(default)]
    pub labels_all: Vec<String>,
    #[serde(default)]
    pub provenance: Vec<IssueWorkspaceProvenanceV1>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub archive: IssueArchiveFilterV1,
    #[serde(default)]
    pub sort: IssueWorkspaceSortV1,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueLocalPaneState {
    #[serde(default)]
    pub selected_issue_id: Option<Uuid>,
    #[serde(default)]
    pub selected_row: usize,
    #[serde(default)]
    pub scroll_offset: usize,
    #[serde(default)]
    pub inspector_scroll_offset: usize,
    #[serde(default)]
    pub inspector_section: IssueInspectorSection,
    #[serde(default)]
    pub filters: IssueLocalFilters,
    #[serde(default)]
    pub inspector_open: bool,
    #[serde(default)]
    pub selected_associated_session_id: Option<Uuid>,
    #[serde(default)]
    pub saved_view: Option<IssueWorkspaceSavedView>,
    #[serde(default)]
    pub mine_assignee: Option<String>,
    #[serde(default)]
    pub page_anchor: IssueWorkspacePageAnchor,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDispatchedPaneState {
    #[serde(default)]
    pub selected_session_id: Option<Uuid>,
    #[serde(default)]
    pub selected_issue_id: Option<String>,
    #[serde(default)]
    pub selected_row: usize,
    #[serde(default)]
    pub scroll_offset: usize,
    #[serde(default)]
    pub inspector_scroll_offset: usize,
    #[serde(default)]
    pub inspector_open: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSyncPaneState {
    #[serde(default)]
    pub scroll_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWorkspaceSavedView {
    Ready,
    Blocked,
    Mine,
    Recent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "cursor", rename_all = "snake_case")]
pub enum IssueWorkspacePageAnchor {
    #[default]
    Start,
    End,
    Cursor(IssueWorkspaceCursorV1),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IssueWorkspaceDataKind {
    LocalPage,
    SelectedIssue,
    BlockedBy,
    Blocks,
    Events,
    DependencyCandidates,
    AssociatedSession,
    Dispatched,
    SyncStatus,
    ManualPoll,
    Mutation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueWorkspaceLocalRequestIdentity {
    pub project_id: Uuid,
    pub filters: IssueLocalFilters,
    pub page_anchor: IssueWorkspacePageAnchor,
    pub direction: PageDirectionV1,
    pub cursor: Option<IssueWorkspaceCursorV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueWorkspaceRequestIdentity {
    Local(IssueWorkspaceLocalRequestIdentity),
    Issue(Uuid),
    Dependency {
        issue_id: Uuid,
        direction: IssueDependencyDirectionV1,
        page_direction: PageDirectionV1,
        cursor: Option<IssueDependencyCursorV1>,
    },
    Events {
        issue_id: Uuid,
        after_sequence: Option<i64>,
    },
    DependencyCandidates {
        issue_id: Uuid,
        query: String,
    },
    AssociatedSession(Uuid),
    Dispatched,
    SyncStatus,
    ManualPoll,
    Mutation(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueWorkspaceDataState {
    pub load_state: IssueWorkspaceLoadState,
    pub error: Option<String>,
    pub last_refresh_ms: Option<i64>,
    pub last_good_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueEditorMode {
    Create,
    Edit { issue_id: Uuid, row_version: i64 },
    Status { issue_id: Uuid, row_version: i64 },
    Dependency { issue_id: Uuid },
    Filter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueEditorState {
    pub mode: IssueEditorMode,
    pub title: String,
    pub body: String,
    pub priority: Option<u8>,
    pub assignee: Option<String>,
    pub labels: Vec<String>,
    pub status: Option<IssueStatus>,
    pub dependency_issue_id: Option<Uuid>,
    pub dependency_direction: rsi_common::issue_workspace::IssueDependencyDirectionV1,
    pub dependency_candidates: Vec<rsi_common::issue_workspace::IssueWorkspaceRowV1>,
    pub dependency_selected_row: usize,
    pub dependency_text: String,
    pub filter_draft: Option<IssueLocalFilters>,
    pub filter_saved_view: Option<IssueWorkspaceSavedView>,
    pub filter_mine_assignee: Option<String>,
    pub base_issue: Option<Issue>,
    pub latest_issue: Option<Issue>,
    pub stale_conflict: bool,
    pub active_field: usize,
    pub dirty: bool,
    pub discard_armed: bool,
    pub retry_key: String,
    pub error: Option<String>,
    pub submitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueCancelConfirmation {
    pub issue_id: Uuid,
    pub display_number: i64,
    pub armed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuePendingDependencyMutation {
    pub request: IssueDependencyMutationRequestV1,
    pub origin_issue_id: Uuid,
    pub direction: rsi_common::issue_workspace::IssueDependencyDirectionV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuePendingMutation {
    Create(CreateIssueV2RequestV1),
    Update(UpdateIssueRequestV1),
    Status(UpdateIssueStatusV2RequestV1),
    Archive(ArchiveIssueRequestV1),
    Restore(RestoreIssueRequestV1),
    AddDependency(IssuePendingDependencyMutation),
    RemoveDependency(IssuePendingDependencyMutation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDependencyReconciliation {
    pub origin_issue_id: Uuid,
    pub direction: rsi_common::issue_workspace::IssueDependencyDirectionV1,
    pub pending_kinds: BTreeSet<IssueWorkspaceDataKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueWorkspaceCachedPage {
    pub cursor: Option<IssueWorkspaceCursorV1>,
    pub page: IssueWorkspacePageV1,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueWorkspaceTransient {
    pub data_states: BTreeMap<IssueWorkspaceDataKind, IssueWorkspaceDataState>,
    pub page_cache: Vec<IssueWorkspaceCachedPage>,
    pub current_page: Option<IssueWorkspacePageV1>,
    pub selected_issue: Option<Issue>,
    pub selected_issue_target: Option<Uuid>,
    pub missing_selected_issue_id: Option<Uuid>,
    pub blocked_by: Option<IssueDependencyPageV1>,
    pub blocked_by_target: Option<Uuid>,
    pub blocks: Option<IssueDependencyPageV1>,
    pub blocks_target: Option<Uuid>,
    pub events: Vec<IssueEventV1>,
    pub events_target: Option<Uuid>,
    pub events_after_sequence: Option<i64>,
    pub events_next_after_sequence: Option<i64>,
    pub blocked_by_cursor: Option<IssueDependencyCursorV1>,
    pub blocks_cursor: Option<IssueDependencyCursorV1>,
    pub dispatched: Vec<IssueDispatchRecordV1>,
    pub sync_status: Option<IssueTrackerStatusV1>,
    pub manual_poll_result: Option<IssueTrackerTickResultV1>,
    pub manual_poll_error: Option<String>,
    pub generations: BTreeMap<IssueWorkspaceDataKind, u64>,
    pub in_flight: BTreeSet<IssueWorkspaceDataKind>,
    pub desired_requests: BTreeMap<IssueWorkspaceDataKind, IssueWorkspaceRequestIdentity>,
    pub local_viewport_rows: usize,
    pub inspector_viewport_rows: usize,
    pub dispatched_viewport_rows: usize,
    pub sync_viewport_rows: usize,
    pub editor: Option<IssueEditorState>,
    pub cancel_confirmation: Option<IssueCancelConfirmation>,
    pub archive_confirmation: Option<IssueCancelConfirmation>,
    pub pending_yank: bool,
    pub pending_jump: bool,
    pub pending_mutation: Option<IssuePendingMutation>,
    pub mutation_retry_error: Option<String>,
    pub dependency_reconciliation: Option<IssueDependencyReconciliation>,
    pub deferred_inspector_target: Option<Uuid>,
    pub page_direction: Option<PageDirectionV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWorkspaceState {
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub active_tab: IssueWorkspaceTab,
    #[serde(default)]
    pub focus: IssueWorkspaceFocus,
    #[serde(default)]
    pub local: IssueLocalPaneState,
    #[serde(default)]
    pub dispatched: IssueDispatchedPaneState,
    #[serde(default)]
    pub sync: IssueSyncPaneState,
    #[serde(skip)]
    pub transient: IssueWorkspaceTransient,
}

impl IssueWorkspaceState {
    #[must_use]
    pub fn new(project_id: Option<Uuid>) -> Self {
        Self {
            project_id,
            active_tab: IssueWorkspaceTab::Local,
            focus: IssueWorkspaceFocus::Table,
            local: IssueLocalPaneState::default(),
            dispatched: IssueDispatchedPaneState::default(),
            sync: IssueSyncPaneState::default(),
            transient: IssueWorkspaceTransient::default(),
        }
    }

    pub fn normalize_after_restore(&mut self) {
        self.transient = IssueWorkspaceTransient::default();
        let inspector_open = match self.active_tab {
            IssueWorkspaceTab::Local => self.local.inspector_open,
            IssueWorkspaceTab::Dispatched => self.dispatched.inspector_open,
            IssueWorkspaceTab::Sync => false,
        };
        self.focus = match (self.focus, inspector_open) {
            (IssueWorkspaceFocus::Inspector | IssueWorkspaceFocus::Editor, true) => {
                IssueWorkspaceFocus::Inspector
            }
            _ => IssueWorkspaceFocus::Table,
        };
        for kind in [
            IssueWorkspaceDataKind::LocalPage,
            IssueWorkspaceDataKind::Dispatched,
            IssueWorkspaceDataKind::SyncStatus,
        ] {
            self.transient.data_states.insert(
                kind,
                IssueWorkspaceDataState {
                    load_state: IssueWorkspaceLoadState::Stale,
                    ..IssueWorkspaceDataState::default()
                },
            );
        }
    }

    pub fn rebind_project(&mut self, project_id: Option<Uuid>) {
        if self.project_id == project_id {
            return;
        }
        self.project_id = project_id;
        self.local.selected_issue_id = None;
        self.local.selected_associated_session_id = None;
        self.local.selected_row = 0;
        self.local.scroll_offset = 0;
        self.local.inspector_scroll_offset = 0;
        self.local.inspector_open = false;
        self.local.page_anchor = IssueWorkspacePageAnchor::Start;
        self.dispatched.selected_session_id = None;
        self.dispatched.selected_issue_id = None;
        self.dispatched.selected_row = 0;
        self.dispatched.scroll_offset = 0;
        self.dispatched.inspector_scroll_offset = 0;
        self.dispatched.inspector_open = false;
        self.sync.scroll_offset = 0;
        self.focus = IssueWorkspaceFocus::Table;
        self.transient = IssueWorkspaceTransient::default();
        self.transient.page_direction = Some(PageDirectionV1::Forward);
    }

    pub fn cache_page(
        &mut self,
        cursor: Option<IssueWorkspaceCursorV1>,
        page: IssueWorkspacePageV1,
    ) {
        self.transient
            .page_cache
            .retain(|cached| cached.cursor != cursor);
        self.transient
            .page_cache
            .push(IssueWorkspaceCachedPage { cursor, page });
        if self.transient.page_cache.len() > ISSUE_WORKSPACE_PAGE_CACHE_LIMIT {
            self.transient.page_cache.remove(0);
        }
    }

    #[must_use]
    pub fn generation(&self, kind: IssueWorkspaceDataKind) -> u64 {
        self.transient
            .generations
            .get(&kind)
            .copied()
            .unwrap_or_default()
    }

    pub fn begin_request(
        &mut self,
        kind: IssueWorkspaceDataKind,
        identity: IssueWorkspaceRequestIdentity,
    ) -> Option<u64> {
        let generation = self.generation(kind).wrapping_add(1);
        self.transient.generations.insert(kind, generation);
        self.transient.desired_requests.insert(kind, identity);
        if !self.transient.in_flight.insert(kind) {
            return None;
        }
        Some(generation)
    }

    #[must_use]
    pub fn request_identity(
        &self,
        kind: IssueWorkspaceDataKind,
    ) -> Option<&IssueWorkspaceRequestIdentity> {
        self.transient.desired_requests.get(&kind)
    }

    #[must_use]
    pub fn load_state(&self, kind: IssueWorkspaceDataKind) -> IssueWorkspaceLoadState {
        self.transient
            .data_states
            .get(&kind)
            .map(|state| state.load_state)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn data_error(&self, kind: IssueWorkspaceDataKind) -> Option<&str> {
        self.transient
            .data_states
            .get(&kind)
            .and_then(|state| state.error.as_deref())
    }

    pub fn data_state_mut(&mut self, kind: IssueWorkspaceDataKind) -> &mut IssueWorkspaceDataState {
        self.transient.data_states.entry(kind).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_workspace_serde_preserves_durable_state_and_normalizes_transient_state() {
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.active_tab = IssueWorkspaceTab::Dispatched;
        state.focus = IssueWorkspaceFocus::Inspector;
        state.local.selected_issue_id = Some(issue_id);
        state.local.scroll_offset = 7;
        state.local.inspector_scroll_offset = 11;
        state.local.inspector_open = true;
        state.dispatched.scroll_offset = 5;
        state.dispatched.inspector_open = true;
        state.dispatched.inspector_scroll_offset = 4;
        state.sync.scroll_offset = 3;
        state.local.filters.query = Some("visible issue".to_string());
        state
            .data_state_mut(IssueWorkspaceDataKind::Dispatched)
            .load_state = IssueWorkspaceLoadState::Fresh;

        let mut restored: IssueWorkspaceState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        restored.normalize_after_restore();

        assert_eq!(restored.project_id, Some(project_id));
        assert_eq!(restored.active_tab, IssueWorkspaceTab::Dispatched);
        assert_eq!(restored.focus, IssueWorkspaceFocus::Inspector);
        assert_eq!(restored.local.selected_issue_id, Some(issue_id));
        assert_eq!(restored.local.scroll_offset, 7);
        assert_eq!(restored.local.inspector_scroll_offset, 11);
        assert_eq!(restored.dispatched.scroll_offset, 5);
        assert!(restored.dispatched.inspector_open);
        assert_eq!(restored.dispatched.inspector_scroll_offset, 4);
        assert_eq!(restored.sync.scroll_offset, 3);
        assert_eq!(
            restored.local.filters.query.as_deref(),
            Some("visible issue")
        );
        assert_eq!(
            restored.load_state(IssueWorkspaceDataKind::Dispatched),
            IssueWorkspaceLoadState::Stale
        );
        assert!(restored.transient.page_cache.is_empty());
    }

    #[test]
    fn issue_workspace_restore_normalizes_transient_editor_and_closed_inspector_focus() {
        let project_id = Uuid::new_v4();
        let mut open: IssueWorkspaceState = serde_json::from_value(serde_json::json!({
            "project_id": project_id,
            "focus": "editor",
            "local": { "inspector_open": true }
        }))
        .unwrap();
        open.normalize_after_restore();
        assert_eq!(open.focus, IssueWorkspaceFocus::Inspector);

        let mut closed: IssueWorkspaceState = serde_json::from_value(serde_json::json!({
            "project_id": project_id,
            "focus": "inspector",
            "local": { "inspector_open": false }
        }))
        .unwrap();
        closed.normalize_after_restore();
        assert_eq!(closed.focus, IssueWorkspaceFocus::Table);
    }

    #[test]
    fn issue_workspace_cache_is_bounded_and_request_is_one_in_flight() {
        let mut state = IssueWorkspaceState::new(Some(Uuid::new_v4()));
        for display_number in 1..=4 {
            state.cache_page(
                Some(IssueWorkspaceCursorV1::DisplayNumberAsc {
                    display_number,
                    issue_id: Uuid::from_u128(display_number as u128),
                }),
                IssueWorkspacePageV1 {
                    rows: Vec::new(),
                    previous_cursor: None,
                    next_cursor: None,
                },
            );
        }
        assert_eq!(
            state.transient.page_cache.len(),
            ISSUE_WORKSPACE_PAGE_CACHE_LIMIT
        );
        assert!(matches!(
            state
                .transient
                .page_cache
                .first()
                .map(|cached| &cached.cursor),
            Some(Some(IssueWorkspaceCursorV1::DisplayNumberAsc {
                display_number: 2,
                ..
            }))
        ));
        assert_eq!(
            state.begin_request(
                IssueWorkspaceDataKind::LocalPage,
                IssueWorkspaceRequestIdentity::Local(IssueWorkspaceLocalRequestIdentity {
                    project_id: state.project_id.unwrap(),
                    filters: state.local.filters.clone(),
                    page_anchor: IssueWorkspacePageAnchor::Start,
                    direction: PageDirectionV1::Forward,
                    cursor: None,
                }),
            ),
            Some(1)
        );
        let identity = state
            .request_identity(IssueWorkspaceDataKind::LocalPage)
            .cloned()
            .unwrap();
        assert_eq!(
            state.begin_request(IssueWorkspaceDataKind::LocalPage, identity),
            None
        );
    }
}
