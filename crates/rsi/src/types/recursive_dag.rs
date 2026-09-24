//! TUI-local state for the read-only recursive DAG browser.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rsi_common::rpc::DaemonCapabilities;
use rsi_common::types::Session;
use rsi_common::{
    RecursiveArtifactMetadataState, RecursiveArtifactRole, RecursiveCancellationRequestStatus,
    RecursiveCancellationRequestSummary, RecursiveDagOperationalStatus, RecursiveDagRecoveryStatus,
    RecursiveDeferredRecoveryGraph, RecursiveExecutionArtifact, RecursiveExecutionArtifactPreview,
    RecursiveExecutionArtifactReadback, RecursiveExecutionArtifactSummary, RecursiveExecutionMode,
    RecursiveGraphRecoveryState, RecursiveGraphStatus, RecursiveLiveAttemptArtifactReadback,
    RecursiveLiveAttemptHeartbeatState, RecursiveLiveAttemptHeartbeatStatus,
    RecursiveLiveAttemptId, RecursiveLiveAttemptListItem, RecursiveLiveAttemptStatus,
    RecursiveLiveInterruptStatus, RecursiveLiveInterruptSummary, RecursiveLiveOutputValidationId,
    RecursiveLiveOutputValidationIssue, RecursiveLiveOutputValidationListItem,
    RecursiveLiveOutputValidationResult, RecursiveLiveRecoveryReadback,
    RecursiveLiveRecoveryStatus, RecursiveLiveValidationIssueSeverity, RecursiveSchedulerRunDetail,
    RecursiveSchedulerRunEvent, RecursiveSchedulerRunId, RecursiveSchedulerRunStatus,
    RecursiveSchedulerRunSummary, RecursiveTaskGraphDetail, RecursiveTaskGraphId,
    RecursiveTaskGraphSummary, RecursiveTaskId, RecursiveTaskLifecycleState,
};
use uuid::Uuid;

pub const RECURSIVE_DAG_GRAPH_LIMIT: usize = 100;
pub const RECURSIVE_DAG_TASK_LIMIT: usize = 200;
pub const RECURSIVE_DAG_RUN_LIMIT: usize = 12;
pub const RECURSIVE_DAG_EVENT_LIMIT: usize = 80;
pub const RECURSIVE_DAG_LIVE_LIMIT: usize = 24;
pub const RECURSIVE_DAG_ARTIFACT_LIMIT: usize = 24;
pub const RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT: usize = 3;
pub const RECURSIVE_DAG_ARTIFACT_MAX_LOADED: usize =
    RECURSIVE_DAG_ARTIFACT_LIMIT * RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT;
pub const RECURSIVE_DAG_INSPECTOR_ROW_LIMIT: usize = 64;
pub const RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_BYTES: u32 = 16 * 1024;
pub const RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_LINES: u32 = 80;
pub const RECURSIVE_DAG_ARTIFACT_PREVIEW_RENDER_LINES: usize = 80;
pub const RECURSIVE_DAG_VALIDATION_ISSUE_LIMIT: usize = 24;
pub const RECURSIVE_DAG_METADATA_KEY_LIMIT: usize = 40;
pub const RECURSIVE_DAG_CONTROL_REASON_LIMIT: usize = 240;
pub const RECURSIVE_DAG_CONTROL_REQUESTED_BY: &str = "rsi-tui";

pub fn recursive_dag_preview_unavailable_message(
    capabilities: Option<&DaemonCapabilities>,
) -> &'static str {
    match capabilities.map(|caps| caps.recursive_dag_artifact_preview_inspection) {
        Some(true) => {
            "bounded artifact preview not loaded: press p inside an artifact inspector to request PreviewRecursiveExecutionArtifact"
        }
        Some(false) => {
            "bounded artifact preview unavailable: recursive_dag_artifact_preview_inspection=false"
        }
        None => "bounded artifact preview unavailable: daemon capabilities unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagPanel {
    Graphs,
    Tasks,
    Runs,
    Recovery,
    Cancellations,
    Heartbeats,
    Interrupts,
    Live,
    Artifacts,
}

impl RecursiveDagPanel {
    pub const ALL: [Self; 9] = [
        Self::Graphs,
        Self::Tasks,
        Self::Runs,
        Self::Recovery,
        Self::Cancellations,
        Self::Heartbeats,
        Self::Interrupts,
        Self::Live,
        Self::Artifacts,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Graphs => "graphs",
            Self::Tasks => "tasks",
            Self::Runs => "runs",
            Self::Recovery => "recovery",
            Self::Cancellations => "cancellations",
            Self::Heartbeats => "heartbeats",
            Self::Interrupts => "interrupts",
            Self::Live => "live",
            Self::Artifacts => "artifacts",
        }
    }

    pub fn next(self) -> Self {
        let idx = Self::ALL
            .iter()
            .position(|panel| *panel == self)
            .unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let idx = Self::ALL
            .iter()
            .position(|panel| *panel == self)
            .unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecursiveDagLoadStatus {
    Loading,
    Ready,
    Disconnected,
    CapabilityDisabled,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagStatusTone {
    Normal,
    Loading,
    Warning,
    Error,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecursiveDagFakeRunState {
    Idle,
    MaxStepsInput {
        input: String,
        error: Option<String>,
    },
    Running {
        graph_id: RecursiveTaskGraphId,
        max_steps: u32,
    },
}

impl Default for RecursiveDagFakeRunState {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecursiveDagLiveRunState {
    Idle,
    MaxStepsInput {
        input: String,
        error: Option<String>,
    },
    Running {
        graph_id: RecursiveTaskGraphId,
        max_steps: u32,
    },
}

impl Default for RecursiveDagLiveRunState {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagControlKind {
    GraphCancellation,
    RunCancellation,
    RecoveryContinuation,
}

impl RecursiveDagControlKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::GraphCancellation => "graph cancellation",
            Self::RunCancellation => "run cancellation",
            Self::RecoveryContinuation => "recovery continuation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagRecoveryInputField {
    MaxGraphs,
    TimeBudgetMs,
}

impl RecursiveDagRecoveryInputField {
    pub const fn label(self) -> &'static str {
        match self {
            Self::MaxGraphs => "max_graphs",
            Self::TimeBudgetMs => "time_budget_ms",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::MaxGraphs => Self::TimeBudgetMs,
            Self::TimeBudgetMs => Self::MaxGraphs,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecursiveDagControlState {
    Idle,
    CancellationPrefix,
    GraphReasonInput {
        graph_id: RecursiveTaskGraphId,
        input: String,
        error: Option<String>,
    },
    RunReasonInput {
        run_id: RecursiveSchedulerRunId,
        input: String,
        error: Option<String>,
    },
    RecoveryBudgetInput {
        max_graphs_input: String,
        time_budget_ms_input: String,
        field: RecursiveDagRecoveryInputField,
        error: Option<String>,
    },
    Submitting {
        kind: RecursiveDagControlKind,
        target: String,
    },
}

impl Default for RecursiveDagControlState {
    fn default() -> Self {
        Self::Idle
    }
}

impl RecursiveDagControlState {
    pub fn is_active_input(&self) -> bool {
        matches!(
            self,
            Self::CancellationPrefix
                | Self::GraphReasonInput { .. }
                | Self::RunReasonInput { .. }
                | Self::RecoveryBudgetInput { .. }
        )
    }

    pub fn is_submitting(&self) -> bool {
        matches!(self, Self::Submitting { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagInspectorLoadStatus {
    Idle,
    Loading,
    Ready,
    Error,
    CapabilityDisabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagInspectorView {
    Metadata,
    Links,
    Preview,
    Test,
    Diff,
    Report,
}

impl RecursiveDagInspectorView {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Links => "links",
            Self::Preview => "preview",
            Self::Test => "test",
            Self::Diff => "diff",
            Self::Report => "report",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagArtifactNavDirection {
    Previous,
    Next,
}

impl RecursiveDagArtifactNavDirection {
    pub const fn delta(self) -> isize {
        match self {
            Self::Previous => -1,
            Self::Next => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagArtifactBucket {
    Graph,
    SchedulerReport,
    Prompt,
    RawOutput,
    NormalizedOutput,
    ValidationReport,
    Diff,
    Test,
    Produced,
}

impl RecursiveDagArtifactBucket {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::SchedulerReport => "scheduler-report",
            Self::Prompt => "prompt",
            Self::RawOutput => "raw-output",
            Self::NormalizedOutput => "normalized-output",
            Self::ValidationReport => "validation",
            Self::Diff => "diff",
            Self::Test => "test",
            Self::Produced => "produced",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagInspectorRowKind {
    Artifact,
    Validation,
    Test,
    Diff,
    Report,
    Page,
}

impl RecursiveDagInspectorRowKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Artifact => "artifact",
            Self::Validation => "validation",
            Self::Test => "test",
            Self::Diff => "diff",
            Self::Report => "report",
            Self::Page => "page",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDagArtifactPageAction {
    LoadMore,
    Loading,
    Error,
    CapabilityDisabled,
    PageCap,
}

impl RecursiveDagArtifactPageAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::LoadMore => "load-more",
            Self::Loading => "loading",
            Self::Error => "error",
            Self::CapabilityDisabled => "capability-disabled",
            Self::PageCap => "page-cap",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecursiveDagInspectorSource {
    GraphArtifact,
    SchedulerReport {
        run_id: RecursiveSchedulerRunId,
    },
    LiveAttemptBucket {
        live_attempt_id: RecursiveLiveAttemptId,
        bucket: RecursiveDagArtifactBucket,
    },
    ValidationResult,
    ArtifactSummaryPage {
        action: RecursiveDagArtifactPageAction,
    },
    Unavailable {
        reason: String,
    },
}

impl RecursiveDagInspectorSource {
    pub fn label(&self) -> String {
        match self {
            Self::GraphArtifact => "graph".to_string(),
            Self::SchedulerReport { .. } => "scheduler-report".to_string(),
            Self::LiveAttemptBucket { bucket, .. } => format!("live:{}", bucket.label()),
            Self::ValidationResult => "validation".to_string(),
            Self::ArtifactSummaryPage { .. } => "artifact-page".to_string(),
            Self::Unavailable { .. } => "unavailable".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagInspectorRow {
    pub key: String,
    pub kind: RecursiveDagInspectorRowKind,
    pub source: RecursiveDagInspectorSource,
    pub artifact_summary: Option<RecursiveExecutionArtifactSummary>,
    pub artifact: Option<RecursiveExecutionArtifact>,
    pub validation: Option<RecursiveLiveOutputValidationListItem>,
    pub label: String,
    pub owner: String,
}

impl RecursiveDagInspectorRow {
    pub fn artifact_id(&self) -> Option<i64> {
        self.artifact_summary
            .as_ref()
            .map(|summary| summary.artifact_id)
            .or_else(|| self.artifact.as_ref().map(|artifact| artifact.id))
    }

    pub fn artifact_graph_id(&self) -> Option<RecursiveTaskGraphId> {
        self.artifact_summary
            .as_ref()
            .map(|summary| summary.graph_id)
            .or_else(|| self.artifact.as_ref().map(|artifact| artifact.graph_id))
    }

    pub fn artifact_task_id(&self) -> Option<RecursiveTaskId> {
        self.artifact_summary
            .as_ref()
            .map(|summary| summary.task_id)
            .or_else(|| self.artifact.as_ref().map(|artifact| artifact.task_id))
    }

    pub fn artifact_attempt_id(&self) -> Option<rsi_common::RecursiveAttemptId> {
        self.artifact_summary
            .as_ref()
            .and_then(|summary| summary.attempt_id)
            .or_else(|| {
                self.artifact
                    .as_ref()
                    .and_then(|artifact| artifact.attempt_id)
            })
    }

    pub fn artifact_metadata_state(&self) -> Option<RecursiveArtifactMetadataState> {
        self.artifact_summary
            .as_ref()
            .map(|summary| summary.metadata_state)
    }

    pub fn validation_key(
        &self,
    ) -> Option<(
        Option<RecursiveLiveOutputValidationId>,
        RecursiveLiveAttemptId,
    )> {
        self.validation.as_ref().map(|validation| {
            (
                validation.summary.validation_id,
                validation.summary.live_attempt_id,
            )
        })
    }

    pub fn artifact_page_action(&self) -> Option<RecursiveDagArtifactPageAction> {
        match self.source {
            RecursiveDagInspectorSource::ArtifactSummaryPage { action } => Some(action),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagArtifactSummaryPageState {
    pub graph_id: RecursiveTaskGraphId,
    pub status: RecursiveDagInspectorLoadStatus,
    pub items: Vec<RecursiveExecutionArtifactSummary>,
    pub limit: u32,
    pub next_cursor: Option<String>,
    pub has_more: bool,
    pub loaded_pages: usize,
    pub page_cap_reached: bool,
    pub message: Option<String>,
    pub warnings: Vec<String>,
}

impl RecursiveDagArtifactSummaryPageState {
    pub fn loading(graph_id: RecursiveTaskGraphId) -> Self {
        Self {
            graph_id,
            status: RecursiveDagInspectorLoadStatus::Loading,
            items: Vec::new(),
            limit: RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            next_cursor: None,
            has_more: false,
            loaded_pages: 0,
            page_cap_reached: false,
            message: Some("loading graph artifact summaries".to_string()),
            warnings: Vec::new(),
        }
    }

    pub fn ready(
        graph_id: RecursiveTaskGraphId,
        items: Vec<RecursiveExecutionArtifactSummary>,
        limit: u32,
        next_cursor: Option<String>,
        has_more: bool,
        warnings: Vec<String>,
    ) -> Self {
        let row_count = items.len();
        Self {
            graph_id,
            status: RecursiveDagInspectorLoadStatus::Ready,
            items,
            limit: limit.min(RECURSIVE_DAG_ARTIFACT_LIMIT as u32),
            next_cursor,
            has_more,
            loaded_pages: 1,
            page_cap_reached: false,
            message: Some(format!("loaded {row_count} graph artifact summary row(s)")),
            warnings,
        }
    }

    pub fn capability_disabled(graph_id: RecursiveTaskGraphId) -> Self {
        Self {
            graph_id,
            status: RecursiveDagInspectorLoadStatus::CapabilityDisabled,
            items: Vec::new(),
            limit: RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            next_cursor: None,
            has_more: false,
            loaded_pages: 0,
            page_cap_reached: false,
            message: Some(
                "artifact summaries unavailable: recursive_dag_artifact_list_pagination=false"
                    .to_string(),
            ),
            warnings: Vec::new(),
        }
    }

    pub fn error(graph_id: RecursiveTaskGraphId, message: impl Into<String>) -> Self {
        Self {
            graph_id,
            status: RecursiveDagInspectorLoadStatus::Error,
            items: Vec::new(),
            limit: RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            next_cursor: None,
            has_more: false,
            loaded_pages: 0,
            page_cap_reached: false,
            message: Some(message.into()),
            warnings: Vec::new(),
        }
    }

    pub fn can_load_more(&self) -> bool {
        self.status != RecursiveDagInspectorLoadStatus::Loading
            && self.has_more
            && self.next_cursor.is_some()
            && !self.page_cap_reached
            && self.loaded_pages < RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT
    }

    pub fn mark_loading_more(&mut self) {
        self.status = RecursiveDagInspectorLoadStatus::Loading;
        self.message = Some("loading next artifact summary page".to_string());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagValidationDetailState {
    pub validation_id: Option<RecursiveLiveOutputValidationId>,
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub status: RecursiveDagInspectorLoadStatus,
    pub result: Option<RecursiveLiveOutputValidationResult>,
    pub issues: Vec<RecursiveLiveOutputValidationIssue>,
    pub message: Option<String>,
}

impl RecursiveDagValidationDetailState {
    pub fn loading(
        validation_id: Option<RecursiveLiveOutputValidationId>,
        live_attempt_id: RecursiveLiveAttemptId,
    ) -> Self {
        Self {
            validation_id,
            live_attempt_id,
            status: RecursiveDagInspectorLoadStatus::Loading,
            result: None,
            issues: Vec::new(),
            message: Some("loading live validation detail".to_string()),
        }
    }

    pub fn matches_row(&self, row: &RecursiveDagInspectorRow) -> bool {
        row.validation_key()
            .is_some_and(|(validation_id, live_attempt_id)| {
                self.validation_id == validation_id && self.live_attempt_id == live_attempt_id
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagLiveAttemptArtifactsState {
    pub graph_id: RecursiveTaskGraphId,
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub status: RecursiveDagInspectorLoadStatus,
    pub artifacts: Option<RecursiveLiveAttemptArtifactReadback>,
    pub message: Option<String>,
    pub warnings: Vec<String>,
}

impl RecursiveDagLiveAttemptArtifactsState {
    pub fn loading(
        graph_id: RecursiveTaskGraphId,
        live_attempt_id: RecursiveLiveAttemptId,
    ) -> Self {
        Self {
            graph_id,
            live_attempt_id,
            status: RecursiveDagInspectorLoadStatus::Loading,
            artifacts: None,
            message: Some("loading selected live attempt artifact buckets".to_string()),
            warnings: Vec::new(),
        }
    }

    pub fn matches_selection(
        &self,
        graph_id: RecursiveTaskGraphId,
        live_attempt_id: RecursiveLiveAttemptId,
    ) -> bool {
        self.graph_id == graph_id && self.live_attempt_id == live_attempt_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagArtifactDetailState {
    pub graph_id: RecursiveTaskGraphId,
    pub artifact_id: i64,
    pub status: RecursiveDagInspectorLoadStatus,
    pub readback: Option<RecursiveExecutionArtifactReadback>,
    pub message: Option<String>,
    pub warnings: Vec<String>,
}

impl RecursiveDagArtifactDetailState {
    pub fn loading(graph_id: RecursiveTaskGraphId, artifact_id: i64) -> Self {
        Self {
            graph_id,
            artifact_id,
            status: RecursiveDagInspectorLoadStatus::Loading,
            readback: None,
            message: Some("loading graph-guarded artifact detail".to_string()),
            warnings: Vec::new(),
        }
    }

    pub fn matches_row(&self, row: &RecursiveDagInspectorRow) -> bool {
        row.artifact_graph_id() == Some(self.graph_id)
            && row.artifact_id() == Some(self.artifact_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagArtifactPreviewState {
    pub graph_id: RecursiveTaskGraphId,
    pub artifact_id: i64,
    pub status: RecursiveDagInspectorLoadStatus,
    pub preview: Option<RecursiveExecutionArtifactPreview>,
    pub message: Option<String>,
    pub warnings: Vec<String>,
}

impl RecursiveDagArtifactPreviewState {
    pub fn loading(graph_id: RecursiveTaskGraphId, artifact_id: i64) -> Self {
        Self {
            graph_id,
            artifact_id,
            status: RecursiveDagInspectorLoadStatus::Loading,
            preview: None,
            message: Some("loading bounded graph-guarded artifact preview".to_string()),
            warnings: Vec::new(),
        }
    }

    pub fn capability_disabled(graph_id: RecursiveTaskGraphId, artifact_id: i64) -> Self {
        Self {
            graph_id,
            artifact_id,
            status: RecursiveDagInspectorLoadStatus::CapabilityDisabled,
            preview: None,
            message: Some(
                "artifact preview unavailable: recursive_dag_artifact_preview_inspection=false"
                    .to_string(),
            ),
            warnings: Vec::new(),
        }
    }

    pub fn error(
        graph_id: RecursiveTaskGraphId,
        artifact_id: i64,
        message: impl Into<String>,
    ) -> Self {
        Self {
            graph_id,
            artifact_id,
            status: RecursiveDagInspectorLoadStatus::Error,
            preview: None,
            message: Some(message.into()),
            warnings: Vec::new(),
        }
    }

    pub fn matches_row(&self, row: &RecursiveDagInspectorRow) -> bool {
        row.artifact_graph_id() == Some(self.graph_id)
            && row.artifact_id() == Some(self.artifact_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagInspectorState {
    pub open: bool,
    pub view: RecursiveDagInspectorView,
    pub artifact_nav_prefix: Option<RecursiveDagArtifactNavDirection>,
    pub artifact_detail: Option<RecursiveDagArtifactDetailState>,
    pub artifact_preview: Option<RecursiveDagArtifactPreviewState>,
    pub validation_detail: Option<RecursiveDagValidationDetailState>,
    pub live_attempt_artifacts: Option<RecursiveDagLiveAttemptArtifactsState>,
}

impl Default for RecursiveDagInspectorState {
    fn default() -> Self {
        Self {
            open: false,
            view: RecursiveDagInspectorView::Metadata,
            artifact_nav_prefix: None,
            artifact_detail: None,
            artifact_preview: None,
            validation_detail: None,
            live_attempt_artifacts: None,
        }
    }
}

impl RecursiveDagInspectorState {
    pub fn open_for(&mut self, row: Option<&RecursiveDagInspectorRow>) {
        self.open = true;
        self.artifact_nav_prefix = None;
        self.view = match row.map(|row| row.kind) {
            Some(RecursiveDagInspectorRowKind::Validation) => RecursiveDagInspectorView::Links,
            Some(RecursiveDagInspectorRowKind::Test) => RecursiveDagInspectorView::Test,
            Some(RecursiveDagInspectorRowKind::Diff) => RecursiveDagInspectorView::Diff,
            Some(RecursiveDagInspectorRowKind::Report) => RecursiveDagInspectorView::Report,
            _ => RecursiveDagInspectorView::Metadata,
        };
    }

    pub fn close(&mut self) {
        self.open = false;
        self.artifact_nav_prefix = None;
    }
}

/// Parse the overlay-local FAKE scheduler `max_steps` field.
///
/// # Errors
///
/// Returns an error when the input is empty, contains anything other than ASCII
/// decimal digits, is zero, or does not fit in `u32`.
pub fn parse_recursive_dag_fake_max_steps(input: &str) -> Result<u32, &'static str> {
    if input.is_empty() {
        return Err("max_steps is required for FAKE scheduler runs");
    }
    if !input.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("max_steps accepts digits only");
    }
    let max_steps = input
        .parse::<u32>()
        .map_err(|_| "max_steps must be a positive integer")?;
    if max_steps == 0 {
        return Err("max_steps must be greater than zero");
    }
    Ok(max_steps)
}

pub fn parse_recursive_dag_live_max_steps(input: &str) -> Result<u32, &'static str> {
    if input.is_empty() {
        return Err("max_steps is required for LIVE scheduler runs");
    }
    if !input.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("max_steps accepts digits only");
    }
    let max_steps = input
        .parse::<u32>()
        .map_err(|_| "max_steps must be a positive integer")?;
    if max_steps == 0 {
        return Err("max_steps must be greater than zero");
    }
    Ok(max_steps)
}

pub fn validate_recursive_dag_requested_by(label: &str) -> Result<(), &'static str> {
    if label.trim().is_empty() {
        return Err("requested_by must be nonempty");
    }
    Ok(())
}

pub fn validate_recursive_dag_cancellation_reason(input: &str) -> Result<String, &'static str> {
    let reason = input.trim();
    if reason.is_empty() {
        return Err("reason is required");
    }
    if reason.chars().count() > RECURSIVE_DAG_CONTROL_REASON_LIMIT {
        return Err("reason must be 240 characters or fewer");
    }
    Ok(reason.to_string())
}

pub fn parse_recursive_dag_recovery_budget(
    max_graphs_input: &str,
    time_budget_ms_input: &str,
) -> Result<(u32, Option<u64>), &'static str> {
    if max_graphs_input.is_empty() {
        return Err("max_graphs is required");
    }
    if !max_graphs_input.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("max_graphs accepts digits only");
    }
    let max_graphs = max_graphs_input
        .parse::<u32>()
        .map_err(|_| "max_graphs must fit in u32")?;
    if max_graphs == 0 {
        return Err("max_graphs must be greater than zero");
    }

    let time_budget_ms = if time_budget_ms_input.is_empty() {
        None
    } else {
        if !time_budget_ms_input.chars().all(|ch| ch.is_ascii_digit()) {
            return Err("time_budget_ms accepts digits only");
        }
        let parsed = time_budget_ms_input
            .parse::<u64>()
            .map_err(|_| "time_budget_ms must fit in u64")?;
        if parsed > i64::MAX as u64 {
            return Err("time_budget_ms must be <= i64::MAX");
        }
        Some(parsed)
    };

    Ok((max_graphs, time_budget_ms))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagStatusSummary {
    pub text: String,
    pub tone: RecursiveDagStatusTone,
}

impl RecursiveDagStatusSummary {
    pub fn loading() -> Self {
        Self {
            text: "DAG loading".to_string(),
            tone: RecursiveDagStatusTone::Loading,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagSessionContext {
    pub graph_id: RecursiveTaskGraphId,
    pub graph_title: String,
    pub graph_status: RecursiveGraphStatus,
    pub execution_mode: RecursiveExecutionMode,
    pub relation: &'static str,
    pub task_id: Option<RecursiveTaskId>,
    pub task_title: Option<String>,
    pub task_status: Option<RecursiveTaskLifecycleState>,
    pub run_id: Option<RecursiveSchedulerRunId>,
    pub run_status: Option<RecursiveSchedulerRunStatus>,
    pub warning_count: usize,
    pub error_count: usize,
    pub open_cancellation_count: usize,
    pub recovery_label: Option<&'static str>,
    pub quarantined: bool,
    pub malformed: bool,
    pub live_disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagStatusPanelState {
    pub recovery_readback_enabled: bool,
    pub recovery_status_loaded: bool,
    pub deferred_recovery_rows: usize,
    pub recovery_quarantined: bool,
    pub recovery_malformed: bool,
    pub cancellation_rows: usize,
    pub open_cancellation_rows: usize,
    pub applied_cancellation_rows: usize,
    pub rejected_cancellation_rows: usize,
    pub live_readback_enabled: bool,
    pub heartbeat_rows: usize,
    pub stale_heartbeat_rows: usize,
    pub missing_heartbeat_rows: usize,
    pub lost_live_attempt_rows: usize,
    pub interrupt_rows: usize,
    pub pending_interrupt_rows: usize,
    pub successful_interrupt_rows: usize,
    pub failed_interrupt_rows: usize,
}

#[derive(Debug, Clone)]
pub struct RecursiveDagSelectedGraphData {
    pub graph: RecursiveTaskGraphDetail,
    pub operational_status: Option<RecursiveDagOperationalStatus>,
    pub scheduler_runs: Vec<RecursiveSchedulerRunSummary>,
    pub selected_run_detail: Option<RecursiveSchedulerRunDetail>,
    pub run_events: Vec<RecursiveSchedulerRunEvent>,
    pub cancellation_requests: Vec<RecursiveCancellationRequestSummary>,
    pub recovery_status: Option<RecursiveDagRecoveryStatus>,
    pub live_attempts: Vec<RecursiveLiveAttemptListItem>,
    pub stale_heartbeats: Vec<RecursiveLiveAttemptHeartbeatState>,
    pub live_recovery_status: Option<RecursiveLiveRecoveryReadback>,
    pub validation_results: Vec<RecursiveLiveOutputValidationListItem>,
    pub artifact_summary_page: RecursiveDagArtifactSummaryPageState,
    pub warnings: Vec<String>,
}

impl RecursiveDagSelectedGraphData {
    pub fn status_panel_state(
        &self,
        capabilities: Option<&DaemonCapabilities>,
    ) -> RecursiveDagStatusPanelState {
        let recovery_readback_enabled =
            capabilities.is_some_and(|caps| caps.recursive_dag_recovery_status);
        let live_readback_enabled =
            capabilities.is_some_and(|caps| caps.recursive_dag_live_status_inspection);
        let cancellation_rows = self.cancellation_rows();
        let heartbeat_rows = self.heartbeat_rows();
        let interrupt_rows = self.interrupt_rows();
        let graph = &self.graph.graph;

        RecursiveDagStatusPanelState {
            recovery_readback_enabled,
            recovery_status_loaded: self.recovery_status.is_some(),
            deferred_recovery_rows: self.deferred_recovery_rows().len(),
            recovery_quarantined: graph.quarantined_at.is_some()
                || self
                    .recovery_status
                    .as_ref()
                    .and_then(|status| status.graph_status.as_ref())
                    .is_some_and(|status| status.state == RecursiveGraphRecoveryState::Quarantined)
                || self
                    .live_recovery_status
                    .as_ref()
                    .and_then(|status| status.graph_recovery.as_ref())
                    .is_some_and(|status| status.state == RecursiveGraphRecoveryState::Quarantined),
            recovery_malformed: graph.malformed_reason.is_some(),
            cancellation_rows: cancellation_rows.len(),
            open_cancellation_rows: cancellation_rows
                .iter()
                .filter(|request| {
                    matches!(
                        request.status,
                        RecursiveCancellationRequestStatus::Requested
                            | RecursiveCancellationRequestStatus::Observed
                    )
                })
                .count(),
            applied_cancellation_rows: cancellation_rows
                .iter()
                .filter(|request| request.status == RecursiveCancellationRequestStatus::Applied)
                .count(),
            rejected_cancellation_rows: cancellation_rows
                .iter()
                .filter(|request| request.status == RecursiveCancellationRequestStatus::Rejected)
                .count(),
            live_readback_enabled,
            heartbeat_rows: heartbeat_rows.len(),
            stale_heartbeat_rows: heartbeat_rows
                .iter()
                .filter(|heartbeat| {
                    heartbeat.heartbeat_status == RecursiveLiveAttemptHeartbeatStatus::Stale
                })
                .count(),
            missing_heartbeat_rows: heartbeat_rows
                .iter()
                .filter(|heartbeat| {
                    heartbeat.heartbeat_status == RecursiveLiveAttemptHeartbeatStatus::Missing
                })
                .count(),
            lost_live_attempt_rows: self.lost_live_attempt_count(),
            interrupt_rows: interrupt_rows.len(),
            pending_interrupt_rows: interrupt_rows
                .iter()
                .filter(|interrupt| {
                    matches!(
                        interrupt.status,
                        RecursiveLiveInterruptStatus::Requested
                            | RecursiveLiveInterruptStatus::Sent
                    )
                })
                .count(),
            successful_interrupt_rows: interrupt_rows
                .iter()
                .filter(|interrupt| interrupt.status == RecursiveLiveInterruptStatus::Interrupted)
                .count(),
            failed_interrupt_rows: interrupt_rows
                .iter()
                .filter(|interrupt| {
                    matches!(
                        interrupt.status,
                        RecursiveLiveInterruptStatus::Failed
                            | RecursiveLiveInterruptStatus::Rejected
                            | RecursiveLiveInterruptStatus::Ignored
                    )
                })
                .count(),
        }
    }

    pub fn cancellation_rows(&self) -> Vec<&RecursiveCancellationRequestSummary> {
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        for request in &self.cancellation_requests {
            if seen.insert(request.id) {
                rows.push(request);
            }
        }
        if let Some(status) = &self.operational_status {
            for request in &status.open_cancellation_requests {
                if seen.insert(request.id) {
                    rows.push(request);
                }
            }
        }
        if let Some(run) = &self.selected_run_detail {
            for request in &run.cancellation_requests {
                if seen.insert(request.id) {
                    rows.push(request);
                }
            }
        }
        rows
    }

    pub fn deferred_recovery_rows(&self) -> Vec<&RecursiveDeferredRecoveryGraph> {
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        if let Some(status) = &self.recovery_status {
            for graph in &status.deferred_graphs {
                let key = graph
                    .graph_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| graph.raw_graph_id.clone());
                if seen.insert(key) {
                    rows.push(graph);
                }
            }
        }
        if let Some(readback) = &self.live_recovery_status
            && let Some(graph) = &readback.deferred_graph
        {
            let key = graph
                .graph_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| graph.raw_graph_id.clone());
            if seen.insert(key) {
                rows.push(graph);
            }
        }
        rows
    }

    pub fn heartbeat_rows(&self) -> Vec<&RecursiveLiveAttemptHeartbeatState> {
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        for heartbeat in &self.stale_heartbeats {
            if seen.insert(heartbeat.live_attempt_id) {
                rows.push(heartbeat);
            }
        }
        if let Some(status) = &self.operational_status {
            for heartbeat in &status.active_live_heartbeats {
                if seen.insert(heartbeat.live_attempt_id) {
                    rows.push(heartbeat);
                }
            }
        }
        if let Some(run) = &self.selected_run_detail {
            for heartbeat in &run.live_heartbeat_states {
                if seen.insert(heartbeat.live_attempt_id) {
                    rows.push(heartbeat);
                }
            }
        }
        for attempt in &self.live_attempts {
            if let Some(heartbeat) = &attempt.heartbeat
                && seen.insert(heartbeat.live_attempt_id)
            {
                rows.push(heartbeat);
            }
        }
        if let Some(readback) = &self.live_recovery_status {
            for heartbeat in &readback.heartbeat_states {
                if seen.insert(heartbeat.live_attempt_id) {
                    rows.push(heartbeat);
                }
            }
        }
        rows
    }

    pub fn interrupt_rows(&self) -> Vec<&RecursiveLiveInterruptSummary> {
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        if let Some(status) = &self.operational_status {
            for interrupt in &status.active_live_interrupts {
                if seen.insert(interrupt.id) {
                    rows.push(interrupt);
                }
            }
        }
        if let Some(run) = &self.selected_run_detail {
            for interrupt in &run.live_interrupts {
                if seen.insert(interrupt.id) {
                    rows.push(interrupt);
                }
            }
        }
        for attempt in &self.live_attempts {
            if let Some(interrupt) = &attempt.latest_interrupt
                && seen.insert(interrupt.id)
            {
                rows.push(interrupt);
            }
        }
        rows
    }

    pub fn warning_count(&self) -> usize {
        self.warnings.len()
            + self.artifact_summary_page.warnings.len()
            + self
                .validation_results
                .iter()
                .map(|result| {
                    let issue_count = result.issues.as_deref().map_or(0, |issues| {
                        issues
                            .iter()
                            .filter(|issue| {
                                issue.severity == RecursiveLiveValidationIssueSeverity::Warning
                            })
                            .count()
                    });
                    issue_count.max(result.summary.warning_count as usize)
                })
                .sum::<usize>()
    }

    pub fn error_count(&self) -> usize {
        self.validation_results
            .iter()
            .map(|result| {
                let issue_count = result.issues.as_deref().map_or(0, |issues| {
                    issues
                        .iter()
                        .filter(|issue| {
                            issue.severity == RecursiveLiveValidationIssueSeverity::Error
                        })
                        .count()
                });
                issue_count.max(result.summary.error_count as usize)
            })
            .sum()
    }

    fn open_cancellation_count(&self) -> usize {
        let from_operational = self
            .operational_status
            .as_ref()
            .map_or(0, |status| status.open_cancellation_requests.len());
        let from_cached_rows = self
            .cancellation_rows()
            .iter()
            .filter(|request| {
                matches!(
                    request.status,
                    RecursiveCancellationRequestStatus::Requested
                        | RecursiveCancellationRequestStatus::Observed
                )
            })
            .count();
        from_operational.max(from_cached_rows)
    }

    fn recovery_label(&self) -> Option<&'static str> {
        let graph_state = self
            .recovery_status
            .as_ref()
            .and_then(|status| status.graph_status.as_ref())
            .map(|status| status.state)
            .or_else(|| {
                self.operational_status
                    .as_ref()
                    .map(|status| status.recovery.state)
            });
        match graph_state {
            Some(RecursiveGraphRecoveryState::PendingRecovery) => Some("recovery pending"),
            Some(RecursiveGraphRecoveryState::Deferred) => Some("recovery deferred"),
            Some(RecursiveGraphRecoveryState::Quarantined) => Some("recovery quarantined"),
            _ => None,
        }
    }

    fn lost_live_attempt_count(&self) -> usize {
        let mut seen = HashSet::new();
        let mut count = 0usize;
        for attempt in &self.live_attempts {
            if seen.insert(attempt.summary.id)
                && (attempt.summary.status == RecursiveLiveAttemptStatus::Lost
                    || attempt.summary.recovery_status == RecursiveLiveRecoveryStatus::Lost)
            {
                count += 1;
            }
        }
        if let Some(readback) = &self.live_recovery_status {
            for attempt in &readback.live_attempts {
                if seen.insert(attempt.summary.id)
                    && (attempt.summary.status == RecursiveLiveAttemptStatus::Lost
                        || attempt.summary.recovery_status == RecursiveLiveRecoveryStatus::Lost)
                {
                    count += 1;
                }
            }
        }
        count
    }

    fn run_for_context(
        &self,
        run_id: Option<RecursiveSchedulerRunId>,
    ) -> Option<&RecursiveSchedulerRunSummary> {
        run_id
            .and_then(|run_id| self.scheduler_runs.iter().find(|run| run.id == run_id))
            .or_else(|| {
                self.operational_status
                    .as_ref()
                    .and_then(|status| status.active_run.as_ref())
            })
            .or_else(|| {
                self.operational_status
                    .as_ref()
                    .and_then(|status| status.latest_run.as_ref())
            })
            .or_else(|| self.scheduler_runs.first())
    }

    fn session_context(
        &self,
        session: &Session,
        capabilities: Option<&DaemonCapabilities>,
    ) -> Option<RecursiveDagSessionContext> {
        let graph = &self.graph.graph;
        let live_attempt = self
            .live_attempts
            .iter()
            .find(|attempt| attempt.summary.session_id == Some(session.id));
        let recursive_attempt = self
            .graph
            .attempts
            .iter()
            .find(|attempt| attempt.session_id == Some(session.id));

        let relation = if live_attempt.is_some() {
            Some("live")
        } else if recursive_attempt.is_some() {
            Some("attempt")
        } else if graph.parent_session_id == Some(session.id) {
            Some("parent")
        } else if session.workflow_id.is_some() && graph.workflow_id == session.workflow_id {
            Some("workflow")
        } else if session.workflow_id_override.is_some()
            && graph.workflow_id == session.workflow_id_override
        {
            Some("workflow")
        } else {
            None
        }?;

        let task_id = live_attempt
            .map(|attempt| attempt.summary.task_id)
            .or_else(|| recursive_attempt.map(|attempt| attempt.task_id));
        let task =
            task_id.and_then(|task_id| self.graph.nodes.iter().find(|node| node.id == task_id));
        let run_id = live_attempt.map(|attempt| attempt.summary.scheduler_run_id);
        let run = self.run_for_context(run_id);

        Some(RecursiveDagSessionContext {
            graph_id: graph.id,
            graph_title: graph.title.clone(),
            graph_status: graph.status,
            execution_mode: graph.execution_mode,
            relation,
            task_id,
            task_title: task.map(|task| task.title.clone()),
            task_status: task.map(|task| task.status),
            run_id: run.map(|run| run.id).or(run_id),
            run_status: run.map(|run| run.status),
            warning_count: self.warning_count(),
            error_count: self.error_count(),
            open_cancellation_count: self.open_cancellation_count(),
            recovery_label: self.recovery_label(),
            quarantined: graph.quarantined_at.is_some(),
            malformed: graph.malformed_reason.is_some(),
            live_disabled: capabilities.is_some_and(|caps| {
                !caps.recursive_dag_live_execution || !caps.recursive_dag_background_loop
            }),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RecursiveDagBrowserState {
    pub load_status: RecursiveDagLoadStatus,
    pub message: Option<String>,
    pub capabilities: Option<DaemonCapabilities>,
    pub project_id: Option<Uuid>,
    pub graphs: Vec<RecursiveTaskGraphSummary>,
    pub selected_graph: usize,
    pub loaded_graph_id: Option<RecursiveTaskGraphId>,
    pub selected_task: usize,
    pub selected_run: usize,
    pub selected_cancellation: usize,
    pub selected_heartbeat: usize,
    pub selected_interrupt: usize,
    pub selected_live_attempt: usize,
    pub selected_artifact: usize,
    pub panel: RecursiveDagPanel,
    pub scroll_offset: usize,
    pub fake_run: RecursiveDagFakeRunState,
    pub live_run: RecursiveDagLiveRunState,
    pub control: RecursiveDagControlState,
    pub inspector: RecursiveDagInspectorState,
    pub detail: Option<RecursiveDagSelectedGraphData>,
    pub loaded_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub enum RecursiveDagAsyncResult {
    Browser(RecursiveDagBrowserState),
    ArtifactSummaryPage {
        graph_id: RecursiveTaskGraphId,
        state: RecursiveDagBrowserState,
    },
}

impl From<RecursiveDagBrowserState> for RecursiveDagAsyncResult {
    fn from(state: RecursiveDagBrowserState) -> Self {
        Self::Browser(state)
    }
}

impl RecursiveDagBrowserState {
    pub fn loading(project_id: Option<Uuid>) -> Self {
        Self {
            load_status: RecursiveDagLoadStatus::Loading,
            message: None,
            capabilities: None,
            project_id,
            graphs: Vec::new(),
            selected_graph: 0,
            loaded_graph_id: None,
            selected_task: 0,
            selected_run: 0,
            selected_cancellation: 0,
            selected_heartbeat: 0,
            selected_interrupt: 0,
            selected_live_attempt: 0,
            selected_artifact: 0,
            panel: RecursiveDagPanel::Graphs,
            scroll_offset: 0,
            fake_run: RecursiveDagFakeRunState::Idle,
            live_run: RecursiveDagLiveRunState::Idle,
            control: RecursiveDagControlState::Idle,
            inspector: RecursiveDagInspectorState::default(),
            detail: None,
            loaded_at: None,
        }
    }

    pub fn disconnected(project_id: Option<Uuid>, message: impl Into<String>) -> Self {
        let mut state = Self::loading(project_id);
        state.load_status = RecursiveDagLoadStatus::Disconnected;
        state.message = Some(message.into());
        state
    }

    pub fn capability_disabled(
        project_id: Option<Uuid>,
        capabilities: DaemonCapabilities,
        message: impl Into<String>,
    ) -> Self {
        let mut state = Self::loading(project_id);
        state.load_status = RecursiveDagLoadStatus::CapabilityDisabled;
        state.capabilities = Some(capabilities);
        state.message = Some(message.into());
        state.loaded_at = Some(Utc::now());
        state
    }

    pub fn error(project_id: Option<Uuid>, message: impl Into<String>) -> Self {
        let mut state = Self::loading(project_id);
        state.load_status = RecursiveDagLoadStatus::Error;
        state.message = Some(message.into());
        state.loaded_at = Some(Utc::now());
        state
    }

    pub fn ready(
        project_id: Option<Uuid>,
        capabilities: DaemonCapabilities,
        mut graphs: Vec<RecursiveTaskGraphSummary>,
        selected_graph_id: Option<RecursiveTaskGraphId>,
        detail: Option<RecursiveDagSelectedGraphData>,
    ) -> Self {
        let original_graph_count = graphs.len();
        graphs.truncate(RECURSIVE_DAG_GRAPH_LIMIT);
        let message = if original_graph_count > RECURSIVE_DAG_GRAPH_LIMIT {
            Some(format!(
                "graph inventory truncated from {original_graph_count} to {RECURSIVE_DAG_GRAPH_LIMIT} rows"
            ))
        } else {
            None
        };
        let selected_graph = selected_graph_id
            .and_then(|id| graphs.iter().position(|graph| graph.id == id))
            .unwrap_or(0)
            .min(graphs.len().saturating_sub(1));
        let loaded_graph_id = detail.as_ref().map(|detail| detail.graph.graph.id);
        Self {
            load_status: RecursiveDagLoadStatus::Ready,
            message,
            capabilities: Some(capabilities),
            project_id,
            graphs,
            selected_graph,
            loaded_graph_id,
            selected_task: 0,
            selected_run: 0,
            selected_cancellation: 0,
            selected_heartbeat: 0,
            selected_interrupt: 0,
            selected_live_attempt: 0,
            selected_artifact: 0,
            panel: RecursiveDagPanel::Graphs,
            scroll_offset: 0,
            fake_run: RecursiveDagFakeRunState::Idle,
            live_run: RecursiveDagLiveRunState::Idle,
            control: RecursiveDagControlState::Idle,
            inspector: RecursiveDagInspectorState::default(),
            detail,
            loaded_at: Some(Utc::now()),
        }
    }

    pub fn selected_graph_id(&self) -> Option<RecursiveTaskGraphId> {
        self.graphs.get(self.selected_graph).map(|graph| graph.id)
    }

    pub fn selected_graph_summary(&self) -> Option<&RecursiveTaskGraphSummary> {
        self.graphs.get(self.selected_graph)
    }

    pub fn graph_cursor_changed(&self) -> bool {
        self.selected_graph_id() != self.loaded_graph_id
    }

    pub fn selected_detail(&self) -> Option<&RecursiveDagSelectedGraphData> {
        match (self.selected_graph_id(), self.loaded_graph_id) {
            (Some(selected), Some(loaded)) if selected == loaded => self.detail.as_ref(),
            _ => None,
        }
    }

    pub fn inspector_rows(&self) -> Vec<RecursiveDagInspectorRow> {
        let Some(detail) = self.selected_detail() else {
            return Vec::new();
        };

        let mut rows = Vec::new();

        if let Some(run_detail) = &detail.selected_run_detail
            && detail.artifact_summary_page.status != RecursiveDagInspectorLoadStatus::Ready
            && let Some(artifact_id) = run_detail.run.report_artifact_id
        {
            rows.push(RecursiveDagInspectorRow {
                key: format!("report:{}:{artifact_id}", run_detail.run.id),
                kind: RecursiveDagInspectorRowKind::Report,
                source: RecursiveDagInspectorSource::Unavailable {
                    reason: "artifact summary pagination unavailable".to_string(),
                },
                artifact_summary: None,
                artifact: None,
                validation: None,
                label: format!("scheduler report artifact {artifact_id}"),
                owner: format!("run {}", run_detail.run.id),
            });
        }

        if let Some(live_artifacts) = self.selected_live_attempt_artifacts()
            && live_artifacts.status == RecursiveDagInspectorLoadStatus::Ready
            && let Some(readback) = &live_artifacts.artifacts
        {
            append_live_attempt_artifact_rows(&mut rows, live_artifacts.live_attempt_id, readback);
        }

        for validation in detail
            .validation_results
            .iter()
            .take(RECURSIVE_DAG_LIVE_LIMIT)
        {
            let summary = &validation.summary;
            rows.push(RecursiveDagInspectorRow {
                key: summary
                    .validation_id
                    .map(|id| format!("validation:{id}"))
                    .unwrap_or_else(|| format!("validation-live:{}", summary.live_attempt_id)),
                kind: RecursiveDagInspectorRowKind::Validation,
                source: RecursiveDagInspectorSource::ValidationResult,
                artifact_summary: None,
                artifact: None,
                validation: Some(validation.clone()),
                label: format!(
                    "{:?} {:?} issues={} raw={} normalized={} report={}",
                    summary.status,
                    summary.output_kind,
                    summary.issue_count,
                    summary
                        .raw_output_artifact_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    summary
                        .normalized_output_artifact_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    summary
                        .validation_artifact_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                ),
                owner: format!("live {}", summary.live_attempt_id),
            });
        }

        if detail.artifact_summary_page.status
            != RecursiveDagInspectorLoadStatus::CapabilityDisabled
        {
            for summary in detail.artifact_summary_page.items.iter() {
                rows.push(artifact_summary_inspector_row(summary.clone()));
            }
        }

        let page_row = artifact_summary_page_control_row(&detail.artifact_summary_page);
        if let Some(page_row) = page_row {
            rows.truncate(RECURSIVE_DAG_INSPECTOR_ROW_LIMIT.saturating_sub(1));
            rows.push(page_row);
        } else {
            rows.truncate(RECURSIVE_DAG_INSPECTOR_ROW_LIMIT);
        }
        rows
    }

    pub fn selected_inspector_row(&self) -> Option<RecursiveDagInspectorRow> {
        self.inspector_rows().get(self.selected_artifact).cloned()
    }

    pub fn selected_live_attempt_id(&self) -> Option<RecursiveLiveAttemptId> {
        self.selected_detail()
            .and_then(|detail| detail.live_attempts.get(self.selected_live_attempt))
            .map(|attempt| attempt.summary.id)
    }

    pub fn selected_live_attempt_context(
        &self,
    ) -> Option<(RecursiveTaskGraphId, RecursiveLiveAttemptId)> {
        let detail = self.selected_detail()?;
        let attempt = detail.live_attempts.get(self.selected_live_attempt)?;
        Some((detail.graph.graph.id, attempt.summary.id))
    }

    pub fn selected_live_attempt_artifacts(
        &self,
    ) -> Option<&RecursiveDagLiveAttemptArtifactsState> {
        let (graph_id, live_attempt_id) = self.selected_live_attempt_context()?;
        self.inspector
            .live_attempt_artifacts
            .as_ref()
            .filter(|artifacts| artifacts.matches_selection(graph_id, live_attempt_id))
    }

    pub fn warning_count(&self) -> usize {
        let state_message_warning = usize::from(
            matches!(self.load_status, RecursiveDagLoadStatus::Ready) && self.message.is_some(),
        );
        let detail_warnings = if matches!(self.load_status, RecursiveDagLoadStatus::Ready) {
            self.selected_detail()
                .map_or(0, RecursiveDagSelectedGraphData::warning_count)
        } else {
            0
        };
        state_message_warning + detail_warnings
    }

    pub fn error_count(&self) -> usize {
        let state_error = usize::from(matches!(self.load_status, RecursiveDagLoadStatus::Error));
        let detail_errors = if matches!(self.load_status, RecursiveDagLoadStatus::Ready) {
            self.selected_detail()
                .map_or(0, RecursiveDagSelectedGraphData::error_count)
        } else {
            0
        };
        state_error + detail_errors
    }

    pub fn session_context(&self, session: &Session) -> Option<RecursiveDagSessionContext> {
        if !matches!(self.load_status, RecursiveDagLoadStatus::Ready) {
            return None;
        }

        if let Some(context) = self
            .selected_detail()
            .and_then(|detail| detail.session_context(session, self.capabilities.as_ref()))
        {
            return Some(context);
        }

        self.graphs
            .iter()
            .find(|graph| {
                graph.parent_session_id == Some(session.id)
                    || (session.workflow_id.is_some() && graph.workflow_id == session.workflow_id)
                    || (session.workflow_id_override.is_some()
                        && graph.workflow_id == session.workflow_id_override)
            })
            .map(|graph| RecursiveDagSessionContext {
                graph_id: graph.id,
                graph_title: graph.title.clone(),
                graph_status: graph.status,
                execution_mode: graph.execution_mode,
                relation: if graph.parent_session_id == Some(session.id) {
                    "parent"
                } else {
                    "workflow"
                },
                task_id: None,
                task_title: None,
                task_status: None,
                run_id: None,
                run_status: None,
                warning_count: self.warning_count(),
                error_count: self.error_count(),
                open_cancellation_count: 0,
                recovery_label: None,
                quarantined: graph.quarantined_at.is_some(),
                malformed: graph.malformed_reason.is_some(),
                live_disabled: self.capabilities.as_ref().is_some_and(|caps| {
                    !caps.recursive_dag_live_execution || !caps.recursive_dag_background_loop
                }),
            })
    }

    pub fn status_summary(&self, focused_session: Option<&Session>) -> RecursiveDagStatusSummary {
        let warning_count = self.warning_count();
        let error_count = self.error_count();
        let focused_context = focused_session.and_then(|session| self.session_context(session));
        let live_disabled = focused_context
            .as_ref()
            .is_some_and(|context| context.live_disabled);

        let mut text = match self.load_status {
            RecursiveDagLoadStatus::Loading => "DAG loading".to_string(),
            RecursiveDagLoadStatus::Ready => format!("DAG {}g", self.graphs.len()),
            RecursiveDagLoadStatus::Disconnected => "DAG offline".to_string(),
            RecursiveDagLoadStatus::CapabilityDisabled => "DAG disabled".to_string(),
            RecursiveDagLoadStatus::Error => "DAG error".to_string(),
        };
        if focused_context.is_some() && matches!(self.load_status, RecursiveDagLoadStatus::Ready) {
            text.push_str(" ctx");
        }
        if live_disabled {
            text.push_str(" LIVE off");
        }
        if warning_count > 0 {
            text.push_str(&format!(" W{warning_count}"));
        }
        if error_count > 0 {
            text.push_str(&format!(" E{error_count}"));
        }

        let tone = match self.load_status {
            RecursiveDagLoadStatus::Loading => RecursiveDagStatusTone::Loading,
            RecursiveDagLoadStatus::CapabilityDisabled => RecursiveDagStatusTone::Disabled,
            RecursiveDagLoadStatus::Error | RecursiveDagLoadStatus::Disconnected => {
                RecursiveDagStatusTone::Error
            }
            RecursiveDagLoadStatus::Ready if error_count > 0 => RecursiveDagStatusTone::Error,
            RecursiveDagLoadStatus::Ready
                if warning_count > 0
                    || focused_context.as_ref().is_some_and(|context| {
                        context.quarantined
                            || context.malformed
                            || context.open_cancellation_count > 0
                            || context.recovery_label.is_some()
                    }) =>
            {
                RecursiveDagStatusTone::Warning
            }
            RecursiveDagLoadStatus::Ready => RecursiveDagStatusTone::Normal,
        };

        RecursiveDagStatusSummary { text, tone }
    }

    pub fn clamp_cursors(&mut self) {
        self.selected_graph = self.selected_graph.min(self.graphs.len().saturating_sub(1));
        let detail_matches_selected = matches!(
            (self.selected_graph_id(), self.loaded_graph_id),
            (Some(selected), Some(loaded)) if selected == loaded
        );
        if detail_matches_selected {
            if let Some(detail) = &self.detail {
                let task_len = detail.graph.nodes.len().min(RECURSIVE_DAG_TASK_LIMIT);
                let run_len = detail.scheduler_runs.len();
                let cancellation_len = detail.cancellation_rows().len();
                let heartbeat_len = detail.heartbeat_rows().len();
                let interrupt_len = detail.interrupt_rows().len();
                let live_len = detail.live_attempts.len();
                let artifact_len = self.inspector_rows().len();
                self.selected_task = self.selected_task.min(task_len.saturating_sub(1));
                self.selected_run = self.selected_run.min(run_len.saturating_sub(1));
                self.selected_cancellation = self
                    .selected_cancellation
                    .min(cancellation_len.saturating_sub(1));
                self.selected_heartbeat =
                    self.selected_heartbeat.min(heartbeat_len.saturating_sub(1));
                self.selected_interrupt =
                    self.selected_interrupt.min(interrupt_len.saturating_sub(1));
                self.selected_live_attempt =
                    self.selected_live_attempt.min(live_len.saturating_sub(1));
                self.selected_artifact = self.selected_artifact.min(artifact_len.saturating_sub(1));
            } else {
                self.selected_task = 0;
                self.selected_run = 0;
                self.selected_cancellation = 0;
                self.selected_heartbeat = 0;
                self.selected_interrupt = 0;
                self.selected_live_attempt = 0;
                self.selected_artifact = 0;
            }
        } else {
            self.selected_task = 0;
            self.selected_run = 0;
            self.selected_cancellation = 0;
            self.selected_heartbeat = 0;
            self.selected_interrupt = 0;
            self.selected_live_attempt = 0;
            self.selected_artifact = 0;
        }
    }

    pub fn move_selected(&mut self, delta: isize) {
        let len = self.active_panel_len();
        if len == 0 {
            return;
        }
        let current = self.active_panel_selected();
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(len - 1)
        };
        self.set_active_panel_selected(next);
    }

    pub fn jump_selected_top(&mut self) {
        self.set_active_panel_selected(0);
    }

    pub fn jump_selected_bottom(&mut self) {
        let len = self.active_panel_len();
        if len > 0 {
            self.set_active_panel_selected(len - 1);
        }
    }

    pub fn active_panel_len(&self) -> usize {
        match self.panel {
            RecursiveDagPanel::Graphs => self.graphs.len(),
            RecursiveDagPanel::Tasks => self.selected_detail().map_or(0, |detail| {
                detail.graph.nodes.len().min(RECURSIVE_DAG_TASK_LIMIT)
            }),
            RecursiveDagPanel::Runs => self
                .selected_detail()
                .map_or(0, |detail| detail.scheduler_runs.len()),
            RecursiveDagPanel::Recovery => 0,
            RecursiveDagPanel::Cancellations => self
                .selected_detail()
                .map_or(0, |detail| detail.cancellation_rows().len()),
            RecursiveDagPanel::Heartbeats => self
                .selected_detail()
                .map_or(0, |detail| detail.heartbeat_rows().len()),
            RecursiveDagPanel::Interrupts => self
                .selected_detail()
                .map_or(0, |detail| detail.interrupt_rows().len()),
            RecursiveDagPanel::Live => self
                .selected_detail()
                .map_or(0, |detail| detail.live_attempts.len()),
            RecursiveDagPanel::Artifacts => self.inspector_rows().len(),
        }
    }

    fn active_panel_selected(&self) -> usize {
        match self.panel {
            RecursiveDagPanel::Graphs => self.selected_graph,
            RecursiveDagPanel::Tasks => self.selected_task,
            RecursiveDagPanel::Runs => self.selected_run,
            RecursiveDagPanel::Recovery => 0,
            RecursiveDagPanel::Cancellations => self.selected_cancellation,
            RecursiveDagPanel::Heartbeats => self.selected_heartbeat,
            RecursiveDagPanel::Interrupts => self.selected_interrupt,
            RecursiveDagPanel::Live => self.selected_live_attempt,
            RecursiveDagPanel::Artifacts => self.selected_artifact,
        }
    }

    fn set_active_panel_selected(&mut self, selected: usize) {
        match self.panel {
            RecursiveDagPanel::Graphs => self.selected_graph = selected,
            RecursiveDagPanel::Tasks => self.selected_task = selected,
            RecursiveDagPanel::Runs => self.selected_run = selected,
            RecursiveDagPanel::Recovery => {}
            RecursiveDagPanel::Cancellations => self.selected_cancellation = selected,
            RecursiveDagPanel::Heartbeats => self.selected_heartbeat = selected,
            RecursiveDagPanel::Interrupts => self.selected_interrupt = selected,
            RecursiveDagPanel::Live => self.selected_live_attempt = selected,
            RecursiveDagPanel::Artifacts => self.selected_artifact = selected,
        }
        self.clamp_cursors();
    }

    pub fn move_artifact_row(&mut self, delta: isize) {
        let previous_panel = self.panel;
        self.panel = RecursiveDagPanel::Artifacts;
        self.move_selected(delta);
        self.panel = previous_panel;
    }
}

fn artifact_inspector_row(
    artifact: RecursiveExecutionArtifact,
    fallback_kind: RecursiveDagInspectorRowKind,
    source: RecursiveDagInspectorSource,
    key_prefix: String,
) -> RecursiveDagInspectorRow {
    let kind = match source {
        RecursiveDagInspectorSource::LiveAttemptBucket {
            bucket: RecursiveDagArtifactBucket::Test,
            ..
        } => RecursiveDagInspectorRowKind::Test,
        RecursiveDagInspectorSource::LiveAttemptBucket {
            bucket: RecursiveDagArtifactBucket::Diff,
            ..
        } => RecursiveDagInspectorRowKind::Diff,
        RecursiveDagInspectorSource::SchedulerReport { .. } => RecursiveDagInspectorRowKind::Report,
        _ => fallback_kind,
    };
    let label = artifact
        .uri
        .as_deref()
        .map(|uri| format!("{} -> {}", artifact.label, uri))
        .unwrap_or_else(|| artifact.label.clone());
    let owner = format!(
        "task {} attempt {}",
        artifact.task_id,
        artifact
            .attempt_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    RecursiveDagInspectorRow {
        key: format!("{key_prefix}:{}", artifact.id),
        kind,
        source,
        artifact_summary: None,
        artifact: Some(artifact),
        validation: None,
        label,
        owner,
    }
}

fn artifact_summary_inspector_row(
    summary: RecursiveExecutionArtifactSummary,
) -> RecursiveDagInspectorRow {
    let source = artifact_summary_source(&summary);
    let kind = artifact_summary_row_kind(&summary);
    let key_prefix = match &source {
        RecursiveDagInspectorSource::SchedulerReport { run_id } => format!("report:{run_id}"),
        RecursiveDagInspectorSource::LiveAttemptBucket {
            live_attempt_id,
            bucket,
        } => format!("live:{live_attempt_id}:{}", bucket.label()),
        _ => "graph".to_string(),
    };
    let label = summary
        .uri_display
        .as_deref()
        .map(|uri| format!("{} -> {}", summary.label, uri))
        .unwrap_or_else(|| summary.label.clone());
    let owner = artifact_summary_owner(&summary);
    RecursiveDagInspectorRow {
        key: format!("{key_prefix}:{}", summary.artifact_id),
        kind,
        source,
        artifact_summary: Some(summary),
        artifact: None,
        validation: None,
        label,
        owner,
    }
}

fn artifact_summary_page_control_row(
    page: &RecursiveDagArtifactSummaryPageState,
) -> Option<RecursiveDagInspectorRow> {
    let action = if page.status == RecursiveDagInspectorLoadStatus::Loading {
        Some(RecursiveDagArtifactPageAction::Loading)
    } else if page.page_cap_reached {
        Some(RecursiveDagArtifactPageAction::PageCap)
    } else if page.can_load_more() {
        Some(RecursiveDagArtifactPageAction::LoadMore)
    } else if page.status == RecursiveDagInspectorLoadStatus::Error && page.items.is_empty() {
        Some(RecursiveDagArtifactPageAction::Error)
    } else if page.status == RecursiveDagInspectorLoadStatus::CapabilityDisabled
        && page.items.is_empty()
    {
        Some(RecursiveDagArtifactPageAction::CapabilityDisabled)
    } else {
        None
    }?;

    let label = match action {
        RecursiveDagArtifactPageAction::LoadMore
            if page.status == RecursiveDagInspectorLoadStatus::Error =>
        {
            page.message
                .as_ref()
                .map(|message| format!("retry load-more after error: {message}"))
                .unwrap_or_else(|| "retry load-more after artifact summary error".to_string())
        }
        RecursiveDagArtifactPageAction::LoadMore => "load more artifact summaries".to_string(),
        RecursiveDagArtifactPageAction::Loading => page
            .message
            .clone()
            .unwrap_or_else(|| "loading artifact summaries".to_string()),
        RecursiveDagArtifactPageAction::Error => page
            .message
            .clone()
            .unwrap_or_else(|| "artifact summary page unavailable".to_string()),
        RecursiveDagArtifactPageAction::CapabilityDisabled => page
            .message
            .clone()
            .unwrap_or_else(|| "artifact summary pagination capability disabled".to_string()),
        RecursiveDagArtifactPageAction::PageCap => format!(
            "local artifact page cap reached at {} page(s)",
            page.loaded_pages
        ),
    };

    Some(RecursiveDagInspectorRow {
        key: format!("artifact-page:{}:{}", page.graph_id, action.label()),
        kind: RecursiveDagInspectorRowKind::Page,
        source: RecursiveDagInspectorSource::ArtifactSummaryPage { action },
        artifact_summary: None,
        artifact: None,
        validation: None,
        label,
        owner: format!("graph {}", page.graph_id),
    })
}

fn artifact_summary_row_kind(
    summary: &RecursiveExecutionArtifactSummary,
) -> RecursiveDagInspectorRowKind {
    match summary.role {
        Some(RecursiveArtifactRole::TestSummary) => RecursiveDagInspectorRowKind::Test,
        Some(RecursiveArtifactRole::DiffSummary) => RecursiveDagInspectorRowKind::Diff,
        Some(RecursiveArtifactRole::SchedulerReport) => RecursiveDagInspectorRowKind::Report,
        _ => RecursiveDagInspectorRowKind::Artifact,
    }
}

fn artifact_summary_source(
    summary: &RecursiveExecutionArtifactSummary,
) -> RecursiveDagInspectorSource {
    if let Some(run_id) = summary.scheduler_run_id
        && summary.role == Some(RecursiveArtifactRole::SchedulerReport)
    {
        return RecursiveDagInspectorSource::SchedulerReport { run_id };
    }
    if let Some(live_attempt_id) = summary.live_attempt_id {
        let bucket = match summary.role {
            Some(RecursiveArtifactRole::RawOutput) => RecursiveDagArtifactBucket::RawOutput,
            Some(RecursiveArtifactRole::NormalizedOutput) => {
                RecursiveDagArtifactBucket::NormalizedOutput
            }
            Some(RecursiveArtifactRole::ValidationReport) => {
                RecursiveDagArtifactBucket::ValidationReport
            }
            Some(RecursiveArtifactRole::ProducedArtifact) => RecursiveDagArtifactBucket::Produced,
            Some(RecursiveArtifactRole::TestSummary) => RecursiveDagArtifactBucket::Test,
            Some(RecursiveArtifactRole::DiffSummary) => RecursiveDagArtifactBucket::Diff,
            _ => RecursiveDagArtifactBucket::Graph,
        };
        return RecursiveDagInspectorSource::LiveAttemptBucket {
            live_attempt_id,
            bucket,
        };
    }
    RecursiveDagInspectorSource::GraphArtifact
}

fn artifact_summary_owner(summary: &RecursiveExecutionArtifactSummary) -> String {
    if let Some(run_id) = summary.scheduler_run_id {
        return format!("run {run_id}");
    }
    if let Some(live_attempt_id) = summary.live_attempt_id {
        return format!("live {live_attempt_id}");
    }
    if let Some(validation_id) = summary.validation_id {
        return format!("validation {validation_id}");
    }
    format!(
        "task {} attempt {}",
        summary.task_id,
        summary
            .attempt_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string())
    )
}

fn append_live_attempt_artifact_rows(
    rows: &mut Vec<RecursiveDagInspectorRow>,
    live_attempt_id: RecursiveLiveAttemptId,
    readback: &RecursiveLiveAttemptArtifactReadback,
) {
    if let Some(artifact) = &readback.prompt_artifact {
        rows.push(live_artifact_row(
            artifact.clone(),
            live_attempt_id,
            RecursiveDagArtifactBucket::Prompt,
        ));
    }
    for artifact in &readback.raw_output_artifacts {
        rows.push(live_artifact_row(
            artifact.clone(),
            live_attempt_id,
            RecursiveDagArtifactBucket::RawOutput,
        ));
    }
    if let Some(artifact) = &readback.normalized_output_artifact {
        rows.push(live_artifact_row(
            artifact.clone(),
            live_attempt_id,
            RecursiveDagArtifactBucket::NormalizedOutput,
        ));
    }
    for artifact in &readback.validation_artifacts {
        rows.push(live_artifact_row(
            artifact.clone(),
            live_attempt_id,
            RecursiveDagArtifactBucket::ValidationReport,
        ));
    }
    for artifact in &readback.diff_artifacts {
        rows.push(live_artifact_row(
            artifact.clone(),
            live_attempt_id,
            RecursiveDagArtifactBucket::Diff,
        ));
    }
    for artifact in &readback.test_artifacts {
        rows.push(live_artifact_row(
            artifact.clone(),
            live_attempt_id,
            RecursiveDagArtifactBucket::Test,
        ));
    }
    for artifact in &readback.produced_artifacts {
        rows.push(live_artifact_row(
            artifact.clone(),
            live_attempt_id,
            RecursiveDagArtifactBucket::Produced,
        ));
    }
}

fn live_artifact_row(
    artifact: RecursiveExecutionArtifact,
    live_attempt_id: RecursiveLiveAttemptId,
    bucket: RecursiveDagArtifactBucket,
) -> RecursiveDagInspectorRow {
    artifact_inspector_row(
        artifact,
        RecursiveDagInspectorRowKind::Artifact,
        RecursiveDagInspectorSource::LiveAttemptBucket {
            live_attempt_id,
            bucket,
        },
        format!("live:{live_attempt_id}:{}", bucket.label()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::{
        RecursiveArtifactContentPresence, RecursiveArtifactPreviewState, RecursiveAttemptId,
        RecursiveAttemptPhase, RecursiveAttemptStatus, RecursiveCancellationRequestId,
        RecursiveCancellationRequestSource, RecursiveCancellationScope,
        RecursiveDeferredRecoveryGraph, RecursiveExecutionArtifact, RecursiveExecutionArtifactKind,
        RecursiveExecutionArtifactSummary, RecursiveExecutionMode, RecursiveGraphRecoveryStatus,
        RecursiveGraphStatus, RecursiveLiveAttemptArtifactReadback,
        RecursiveLiveAttemptHeartbeatState, RecursiveLiveAttemptHeartbeatStatus,
        RecursiveLiveAttemptId, RecursiveLiveAttemptListItem, RecursiveLiveAttemptSummary,
        RecursiveLiveInterruptId, RecursiveLiveInterruptStatus, RecursiveLiveInterruptSummary,
        RecursiveLiveOutputValidationArtifactLinks, RecursiveLiveOutputValidationListItem,
        RecursiveLiveOutputValidationStatus, RecursiveLiveRecoveryStatus, RecursiveRecoveryPassId,
        RecursiveSchedulerRunId, RecursiveTaskAttempt, RecursiveTaskGraphDetail,
        RecursiveTaskLifecycleState, RecursiveTaskNode,
    };

    fn graph_summary(
        graph_id: RecursiveTaskGraphId,
        root_task_id: rsi_common::RecursiveTaskId,
        title: &str,
    ) -> RecursiveTaskGraphSummary {
        let now = Utc::now();
        RecursiveTaskGraphSummary {
            id: graph_id,
            root_task_id,
            title: title.to_string(),
            objective: "objective".to_string(),
            status: RecursiveGraphStatus::Active,
            project_id: None,
            workflow_id: None,
            topology_id: None,
            parent_session_id: None,
            source_execution_id: None,
            source_eval_id: None,
            execution_mode: RecursiveExecutionMode::Fake,
            max_depth: 4,
            max_fanout: 4,
            max_descendants: 16,
            step_limit: 32,
            last_stop_reason: None,
            malformed_reason: None,
            created_at: now,
            updated_at: now,
            recovered_at: None,
            quarantined_at: None,
            quarantine_reason: None,
            recovery_checked_at: None,
        }
    }

    fn task_node(
        graph_id: RecursiveTaskGraphId,
        task_id: rsi_common::RecursiveTaskId,
    ) -> RecursiveTaskNode {
        let now = Utc::now();
        RecursiveTaskNode {
            id: task_id,
            graph_id,
            parent_task_id: None,
            title: "root".to_string(),
            objective: "objective".to_string(),
            scope: "scope".to_string(),
            acceptance_criteria: Vec::new(),
            depth: 0,
            scope_units: 1,
            max_retries: 0,
            status: RecursiveTaskLifecycleState::Ready,
            decomposed_once: false,
            integration_strategy: None,
            verification_strategy: None,
            blocked_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn selected_detail(
        graph: RecursiveTaskGraphSummary,
        root_task_id: rsi_common::RecursiveTaskId,
    ) -> RecursiveDagSelectedGraphData {
        RecursiveDagSelectedGraphData {
            graph: RecursiveTaskGraphDetail {
                graph: graph.clone(),
                nodes: vec![task_node(graph.id, root_task_id)],
                edges: Vec::new(),
                attempts: Vec::new(),
                injection_batches: Vec::new(),
                lifecycle_events: Vec::new(),
                artifacts: Vec::new(),
            },
            operational_status: None,
            scheduler_runs: Vec::new(),
            selected_run_detail: None,
            run_events: Vec::new(),
            cancellation_requests: Vec::new(),
            recovery_status: None,
            live_attempts: Vec::new(),
            stale_heartbeats: Vec::new(),
            live_recovery_status: None,
            validation_results: Vec::new(),
            artifact_summary_page: RecursiveDagArtifactSummaryPageState::ready(
                graph.id,
                Vec::new(),
                RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
                None,
                false,
                Vec::new(),
            ),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn fake_scheduler_max_steps_requires_explicit_positive_value() {
        assert_eq!(parse_recursive_dag_fake_max_steps("7"), Ok(7));
        assert!(parse_recursive_dag_fake_max_steps("").is_err());
        assert!(parse_recursive_dag_fake_max_steps("0").is_err());
        assert!(parse_recursive_dag_fake_max_steps("unbounded").is_err());
        assert!(parse_recursive_dag_fake_max_steps(" 12 ").is_err());
        assert!(parse_recursive_dag_fake_max_steps("+12").is_err());
    }

    #[test]
    fn cancellation_reason_is_trimmed_required_and_bounded() {
        assert_eq!(
            validate_recursive_dag_cancellation_reason("  stop stale work  "),
            Ok("stop stale work".to_string())
        );
        assert!(validate_recursive_dag_cancellation_reason("   ").is_err());
        assert!(
            validate_recursive_dag_cancellation_reason(
                &"x".repeat(RECURSIVE_DAG_CONTROL_REASON_LIMIT + 1)
            )
            .is_err()
        );
        assert!(validate_recursive_dag_requested_by(RECURSIVE_DAG_CONTROL_REQUESTED_BY).is_ok());
        assert!(validate_recursive_dag_requested_by(" ").is_err());
    }

    #[test]
    fn recovery_budget_parsing_requires_positive_max_and_accepts_zero_time_budget() {
        assert_eq!(parse_recursive_dag_recovery_budget("3", ""), Ok((3, None)));
        assert_eq!(
            parse_recursive_dag_recovery_budget("3", "0"),
            Ok((3, Some(0)))
        );
        assert_eq!(
            parse_recursive_dag_recovery_budget("3", "42"),
            Ok((3, Some(42)))
        );
        assert!(parse_recursive_dag_recovery_budget("", "").is_err());
        assert!(parse_recursive_dag_recovery_budget("0", "").is_err());
        assert!(parse_recursive_dag_recovery_budget("x", "").is_err());
        assert!(parse_recursive_dag_recovery_budget("4294967296", "").is_err());
        assert!(parse_recursive_dag_recovery_budget("1", "x").is_err());
        assert!(parse_recursive_dag_recovery_budget("1", "9223372036854775808").is_err());
    }

    fn task_attempt(
        graph_id: RecursiveTaskGraphId,
        task_id: rsi_common::RecursiveTaskId,
        session_id: Uuid,
    ) -> RecursiveTaskAttempt {
        RecursiveTaskAttempt {
            id: RecursiveAttemptId::new(),
            graph_id,
            task_id,
            phase: RecursiveAttemptPhase::Execute,
            attempt_no: 1,
            retry_count: 0,
            status: RecursiveAttemptStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            failure_reason: None,
            block_reason: None,
            dependency_snapshot: Vec::new(),
            executor_kind: RecursiveExecutionMode::Fake,
            session_id: Some(session_id),
            workflow_execution_id: None,
        }
    }

    fn validation_result(
        graph_id: RecursiveTaskGraphId,
        task_id: rsi_common::RecursiveTaskId,
        warning_count: u32,
        error_count: u32,
    ) -> RecursiveLiveOutputValidationListItem {
        RecursiveLiveOutputValidationListItem {
            summary: rsi_common::RecursiveLiveOutputValidationSummary {
                validation_id: None,
                live_attempt_id: RecursiveLiveAttemptId::new(),
                graph_id,
                task_id,
                scheduler_run_id: RecursiveSchedulerRunId::new(),
                attempt_id: RecursiveAttemptId::new(),
                session_id: None,
                status: RecursiveLiveOutputValidationStatus::Invalid,
                output_kind: None,
                mapping_decision: None,
                retry_decision: None,
                raw_output_artifact_id: None,
                normalized_output_artifact_id: None,
                validation_artifact_id: None,
                normalized_digest: None,
                issue_count: warning_count + error_count,
                error_count,
                warning_count,
                info_count: 0,
                created_at: Some(Utc::now()),
            },
            artifact_links: RecursiveLiveOutputValidationArtifactLinks::default(),
            issues: None,
        }
    }

    fn execution_artifact(
        graph_id: RecursiveTaskGraphId,
        task_id: rsi_common::RecursiveTaskId,
        id: i64,
        label: &str,
    ) -> RecursiveExecutionArtifact {
        RecursiveExecutionArtifact {
            id,
            graph_id,
            task_id,
            attempt_id: None,
            kind: RecursiveExecutionArtifactKind::File,
            label: label.to_string(),
            content: None,
            uri: Some(format!("file:///tmp/{label}")),
            metadata: serde_json::json!({"role": label}),
            created_at: Utc::now(),
        }
    }

    fn artifact_summary(
        graph_id: RecursiveTaskGraphId,
        task_id: rsi_common::RecursiveTaskId,
        id: i64,
        label: &str,
    ) -> RecursiveExecutionArtifactSummary {
        RecursiveExecutionArtifactSummary {
            artifact_id: id,
            graph_id,
            task_id,
            attempt_id: None,
            live_attempt_id: None,
            scheduler_run_id: None,
            validation_id: None,
            role: None,
            kind: RecursiveExecutionArtifactKind::File,
            label: label.to_string(),
            uri_display: Some(format!("file:///tmp/{label}")),
            content_presence: RecursiveArtifactContentPresence::Uri,
            content_type: None,
            size_bytes: None,
            digest: None,
            preview_state: RecursiveArtifactPreviewState::Unavailable,
            metadata_state: RecursiveArtifactMetadataState::Valid,
            created_at: Utc::now(),
        }
    }

    fn cancellation_request(
        graph_id: RecursiveTaskGraphId,
        status: RecursiveCancellationRequestStatus,
    ) -> RecursiveCancellationRequestSummary {
        RecursiveCancellationRequestSummary {
            id: RecursiveCancellationRequestId::new(),
            graph_id,
            run_id: None,
            task_id: None,
            scope: RecursiveCancellationScope::Graph,
            status,
            source: RecursiveCancellationRequestSource::ManualRpc,
            reason: "operator requested stop".to_string(),
            requested_by: Some("test".to_string()),
            requested_at: Utc::now(),
            observed_at: None,
            applied_at: None,
            rejection_reason: (status == RecursiveCancellationRequestStatus::Rejected)
                .then(|| "already terminal".to_string()),
            idempotency_key: None,
            request_fingerprint: None,
            source_context: None,
        }
    }

    fn heartbeat(
        live_attempt_id: RecursiveLiveAttemptId,
        status: RecursiveLiveAttemptHeartbeatStatus,
    ) -> RecursiveLiveAttemptHeartbeatState {
        RecursiveLiveAttemptHeartbeatState {
            live_attempt_id,
            live_attempt_status: RecursiveLiveAttemptStatus::Running,
            heartbeat_status: status,
            heartbeat_owner: Some("worker".to_string()),
            heartbeat_token: None,
            heartbeat_at: Some(Utc::now()),
            heartbeat_expires_at: Some(Utc::now()),
            session_id: Some(Uuid::new_v4()),
            failure_reason: None,
            interruption_reason: None,
            cancellation_reason: None,
            recovery_reason: None,
            error: None,
        }
    }

    fn live_interrupt(
        graph_id: RecursiveTaskGraphId,
        task_id: rsi_common::RecursiveTaskId,
        live_attempt_id: RecursiveLiveAttemptId,
        status: RecursiveLiveInterruptStatus,
    ) -> RecursiveLiveInterruptSummary {
        RecursiveLiveInterruptSummary {
            id: RecursiveLiveInterruptId::new(),
            live_attempt_id,
            graph_id,
            task_id,
            scheduler_run_id: RecursiveSchedulerRunId::new(),
            attempt_id: RecursiveAttemptId::new(),
            session_id: Some(Uuid::new_v4()),
            cancellation_request_id: None,
            status,
            reason: "stop requested".to_string(),
            failure_reason: (status == RecursiveLiveInterruptStatus::Failed)
                .then(|| "provider rejected interrupt".to_string()),
            requested_at: Utc::now(),
            sent_at: Some(Utc::now()),
            completed_at: status.is_terminal().then(Utc::now),
        }
    }

    fn live_attempt_item(
        graph_id: RecursiveTaskGraphId,
        task_id: rsi_common::RecursiveTaskId,
        live_attempt_id: RecursiveLiveAttemptId,
        status: RecursiveLiveAttemptStatus,
        recovery_status: RecursiveLiveRecoveryStatus,
        latest_interrupt: Option<RecursiveLiveInterruptSummary>,
    ) -> RecursiveLiveAttemptListItem {
        RecursiveLiveAttemptListItem {
            summary: RecursiveLiveAttemptSummary {
                id: live_attempt_id,
                graph_id,
                task_id,
                scheduler_run_id: RecursiveSchedulerRunId::new(),
                attempt_id: RecursiveAttemptId::new(),
                phase: RecursiveAttemptPhase::Execute,
                attempt_no: 1,
                session_id: Some(Uuid::new_v4()),
                provider: None,
                model: Some("model".to_string()),
                sandbox_kind: None,
                sandbox_root: None,
                sandbox_branch: None,
                sandbox_worktree_id: None,
                execution_mode: RecursiveExecutionMode::LiveSession,
                status,
                recovery_status,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                started_at: Some(Utc::now()),
                launched_at: Some(Utc::now()),
                completed_at: None,
            },
            heartbeat: None,
            latest_interrupt,
            latest_validation: None,
        }
    }

    fn deferred_graph(graph_id: RecursiveTaskGraphId) -> RecursiveDeferredRecoveryGraph {
        RecursiveDeferredRecoveryGraph {
            graph_id: Some(graph_id),
            raw_graph_id: graph_id.to_string(),
            pass_id: RecursiveRecoveryPassId::new(),
            state: RecursiveGraphRecoveryState::Deferred,
            deferred_at: Utc::now(),
            reason: "store busy".to_string(),
            next_after: Some(Utc::now()),
            last_attempted_at: None,
            last_error: Some("transient failure".to_string()),
        }
    }

    #[test]
    fn selected_detail_is_hidden_after_graph_cursor_moves() {
        let graph_id = RecursiveTaskGraphId::new();
        let other_graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let other_root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id, "loaded");
        let other_graph = graph_summary(other_graph_id, other_root_task_id, "other");
        let mut state = RecursiveDagBrowserState::ready(
            None,
            DaemonCapabilities::default(),
            vec![graph.clone(), other_graph],
            Some(graph_id),
            Some(selected_detail(graph, root_task_id)),
        );

        assert!(state.selected_detail().is_some());
        assert_eq!(state.active_panel_len(), 2);

        state.move_selected(1);
        state.panel = RecursiveDagPanel::Tasks;

        assert!(state.graph_cursor_changed());
        assert!(state.selected_detail().is_none());
        assert_eq!(state.active_panel_len(), 0);
    }

    #[test]
    fn session_context_ignores_stale_detail_after_graph_cursor_moves() {
        let session_id = Uuid::new_v4();
        let session = crate::app::app_test_helpers::baseline_session(
            session_id,
            rsi_common::types::SessionKind::Standard,
        );
        let graph_id = RecursiveTaskGraphId::new();
        let other_graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let other_root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id, "loaded");
        let other_graph = graph_summary(other_graph_id, other_root_task_id, "other");
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail
            .graph
            .attempts
            .push(task_attempt(graph_id, root_task_id, session_id));
        detail
            .warnings
            .push("old selected graph warning".to_string());
        detail
            .validation_results
            .push(validation_result(graph_id, root_task_id, 1, 1));
        let mut state = RecursiveDagBrowserState::ready(
            None,
            DaemonCapabilities::default(),
            vec![graph, other_graph],
            Some(graph_id),
            Some(detail),
        );

        state.move_selected(1);

        assert!(state.graph_cursor_changed());
        assert!(state.selected_detail().is_none());
        assert!(state.session_context(&session).is_none());
        assert_eq!(state.warning_count(), 0);
        assert_eq!(state.error_count(), 0);
    }

    #[test]
    fn ready_state_reports_graph_inventory_truncation() {
        let graphs: Vec<_> = (0..=RECURSIVE_DAG_GRAPH_LIMIT)
            .map(|idx| {
                graph_summary(
                    RecursiveTaskGraphId::new(),
                    rsi_common::RecursiveTaskId::new(),
                    &format!("graph {idx}"),
                )
            })
            .collect();

        let state = RecursiveDagBrowserState::ready(
            None,
            DaemonCapabilities::default(),
            graphs,
            None,
            None,
        );

        assert_eq!(state.graphs.len(), RECURSIVE_DAG_GRAPH_LIMIT);
        assert!(
            state
                .message
                .as_deref()
                .is_some_and(|msg| msg.contains("truncated"))
        );
    }

    #[test]
    fn capability_disabled_state_keeps_capability_context() {
        let mut caps = DaemonCapabilities::default();
        caps.recursive_dag_inspection = false;

        let state = RecursiveDagBrowserState::capability_disabled(
            None,
            caps,
            "daemon does not advertise recursive_dag_inspection",
        );

        assert_eq!(
            state.load_status,
            RecursiveDagLoadStatus::CapabilityDisabled
        );
        assert_eq!(
            state.message.as_deref(),
            Some("daemon does not advertise recursive_dag_inspection")
        );
        assert_eq!(
            state
                .capabilities
                .as_ref()
                .map(|caps| caps.recursive_dag_inspection),
            Some(false)
        );
    }

    #[test]
    fn session_context_uses_cached_parent_graph_summary() {
        let session_id = Uuid::new_v4();
        let session = crate::app::app_test_helpers::baseline_session(
            session_id,
            rsi_common::types::SessionKind::Standard,
        );
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let mut graph = graph_summary(graph_id, root_task_id, "loaded");
        graph.parent_session_id = Some(session_id);

        let state = RecursiveDagBrowserState::ready(
            None,
            DaemonCapabilities::default(),
            vec![graph],
            None,
            None,
        );

        let Some(context) = state.session_context(&session) else {
            panic!("parent graph should correlate from bounded graph cache");
        };
        assert_eq!(context.graph_id, graph_id);
        assert_eq!(context.relation, "parent");
        assert!(context.live_disabled);
    }

    #[test]
    fn status_summary_reports_load_and_warning_counts() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id, "loaded");
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail
            .warnings
            .push("scheduler run readback failed".to_string());
        let state = RecursiveDagBrowserState::ready(
            None,
            DaemonCapabilities::default(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );

        let summary = state.status_summary(None);
        assert_eq!(summary.tone, RecursiveDagStatusTone::Warning);
        assert!(summary.text.contains("DAG 1g"));
        assert!(summary.text.contains("W1"));

        let error_summary = RecursiveDagBrowserState::error(None, "boom").status_summary(None);
        assert_eq!(error_summary.tone, RecursiveDagStatusTone::Error);
        assert!(error_summary.text.contains("DAG error"));
        assert!(error_summary.text.contains("E1"));
    }

    #[test]
    fn status_summary_formats_cached_warning_and_error_counts() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id, "loaded");
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail
            .warnings
            .push("graph readback was truncated".to_string());
        detail
            .validation_results
            .push(validation_result(graph_id, root_task_id, 1, 1));
        let state = RecursiveDagBrowserState::ready(
            None,
            DaemonCapabilities::default(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );

        let summary = state.status_summary(None);
        assert_eq!(summary.tone, RecursiveDagStatusTone::Error);
        assert!(summary.text.contains("W2"), "{:?}", summary.text);
        assert!(summary.text.contains("E1"), "{:?}", summary.text);
    }

    #[test]
    fn status_panel_state_constructs_recovery_cancellation_heartbeat_and_interrupt_counts() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let mut graph = graph_summary(graph_id, root_task_id, "loaded");
        graph.quarantined_at = Some(Utc::now());
        graph.quarantine_reason = Some("malformed lineage".to_string());
        graph.malformed_reason = Some("missing root".to_string());
        let mut detail = selected_detail(graph, root_task_id);
        detail.cancellation_requests = vec![
            cancellation_request(graph_id, RecursiveCancellationRequestStatus::Requested),
            cancellation_request(graph_id, RecursiveCancellationRequestStatus::Rejected),
        ];
        detail.stale_heartbeats = vec![
            heartbeat(
                RecursiveLiveAttemptId::new(),
                RecursiveLiveAttemptHeartbeatStatus::Stale,
            ),
            heartbeat(
                RecursiveLiveAttemptId::new(),
                RecursiveLiveAttemptHeartbeatStatus::Missing,
            ),
        ];
        let interrupted_id = RecursiveLiveAttemptId::new();
        let failed_id = RecursiveLiveAttemptId::new();
        detail.live_attempts = vec![
            live_attempt_item(
                graph_id,
                root_task_id,
                interrupted_id,
                RecursiveLiveAttemptStatus::Running,
                RecursiveLiveRecoveryStatus::None,
                Some(live_interrupt(
                    graph_id,
                    root_task_id,
                    interrupted_id,
                    RecursiveLiveInterruptStatus::Interrupted,
                )),
            ),
            live_attempt_item(
                graph_id,
                root_task_id,
                failed_id,
                RecursiveLiveAttemptStatus::Lost,
                RecursiveLiveRecoveryStatus::Lost,
                Some(live_interrupt(
                    graph_id,
                    root_task_id,
                    failed_id,
                    RecursiveLiveInterruptStatus::Failed,
                )),
            ),
        ];
        detail.recovery_status = Some(RecursiveDagRecoveryStatus {
            latest_pass: None,
            graph_status: Some(RecursiveGraphRecoveryStatus {
                graph_id: Some(graph_id),
                raw_graph_id: graph_id.to_string(),
                state: RecursiveGraphRecoveryState::Quarantined,
                pass_id: Some(RecursiveRecoveryPassId::new()),
                last_attempted_at: Some(Utc::now()),
                completed_at: None,
                deferred_at: Some(Utc::now()),
                reason: Some("operator review required".to_string()),
                last_error: Some("bad edge".to_string()),
                updated_at: Utc::now(),
            }),
            deferred_graphs: vec![deferred_graph(graph_id)],
            deferred_graph_count: 1,
            oldest_deferred_graph: None,
        });
        let caps = DaemonCapabilities {
            recursive_dag_live_status_inspection: true,
            ..Default::default()
        };

        let panels = detail.status_panel_state(Some(&caps));

        assert!(panels.recovery_readback_enabled);
        assert!(panels.recovery_status_loaded);
        assert!(panels.recovery_quarantined);
        assert!(panels.recovery_malformed);
        assert_eq!(panels.deferred_recovery_rows, 1);
        assert_eq!(panels.cancellation_rows, 2);
        assert_eq!(panels.open_cancellation_rows, 1);
        assert_eq!(panels.applied_cancellation_rows, 0);
        assert_eq!(panels.rejected_cancellation_rows, 1);
        assert!(panels.live_readback_enabled);
        assert_eq!(panels.heartbeat_rows, 2);
        assert_eq!(panels.stale_heartbeat_rows, 1);
        assert_eq!(panels.missing_heartbeat_rows, 1);
        assert_eq!(panels.lost_live_attempt_rows, 1);
        assert_eq!(panels.interrupt_rows, 2);
        assert_eq!(panels.pending_interrupt_rows, 0);
        assert_eq!(panels.successful_interrupt_rows, 1);
        assert_eq!(panels.failed_interrupt_rows, 1);
    }

    #[test]
    fn status_panel_state_marks_live_readback_disabled_without_discarding_cached_rows() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id, "loaded");
        let mut detail = selected_detail(graph, root_task_id);
        detail.stale_heartbeats = vec![heartbeat(
            RecursiveLiveAttemptId::new(),
            RecursiveLiveAttemptHeartbeatStatus::Stale,
        )];
        let caps = DaemonCapabilities {
            recursive_dag_live_status_inspection: false,
            ..Default::default()
        };

        let panels = detail.status_panel_state(Some(&caps));

        assert!(!panels.live_readback_enabled);
        assert_eq!(panels.heartbeat_rows, 1);
        assert_eq!(panels.stale_heartbeat_rows, 1);
    }

    #[test]
    fn inspector_state_constructs_artifact_validation_and_live_bucket_rows() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id, "loaded");
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "generic",
        ));
        detail
            .validation_results
            .push(validation_result(graph_id, root_task_id, 1, 0));
        let live_attempt_id = RecursiveLiveAttemptId::new();
        detail.live_attempts.push(live_attempt_item(
            graph_id,
            root_task_id,
            live_attempt_id,
            RecursiveLiveAttemptStatus::Running,
            RecursiveLiveRecoveryStatus::None,
            None,
        ));
        let mut state = RecursiveDagBrowserState::ready(
            None,
            DaemonCapabilities::default(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        state.inspector.live_attempt_artifacts = Some(RecursiveDagLiveAttemptArtifactsState {
            graph_id,
            live_attempt_id,
            status: RecursiveDagInspectorLoadStatus::Ready,
            artifacts: Some(RecursiveLiveAttemptArtifactReadback {
                prompt_artifact: Some(execution_artifact(graph_id, root_task_id, 2, "prompt")),
                raw_output_artifacts: Vec::new(),
                normalized_output_artifact: None,
                validation_artifacts: Vec::new(),
                diff_artifacts: vec![execution_artifact(graph_id, root_task_id, 3, "diff")],
                test_artifacts: vec![execution_artifact(graph_id, root_task_id, 4, "test")],
                produced_artifacts: Vec::new(),
            }),
            message: None,
            warnings: Vec::new(),
        });

        let rows = state.inspector_rows();

        assert_eq!(rows.len(), 5);
        assert!(
            rows.iter()
                .any(|row| row.kind == RecursiveDagInspectorRowKind::Validation)
        );
        assert!(
            rows.iter()
                .any(|row| row.kind == RecursiveDagInspectorRowKind::Artifact)
        );
        assert!(
            rows.iter()
                .any(|row| row.kind == RecursiveDagInspectorRowKind::Diff)
        );
        assert!(
            rows.iter()
                .any(|row| row.kind == RecursiveDagInspectorRowKind::Test)
        );
        assert!(rows.iter().any(|row| {
            matches!(
                row.source,
                RecursiveDagInspectorSource::LiveAttemptBucket {
                    bucket: RecursiveDagArtifactBucket::Prompt,
                    ..
                }
            )
        }));
    }

    #[test]
    fn live_attempt_artifact_rows_are_scoped_to_selected_attempt_and_graph() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id, "loaded");
        let mut detail = selected_detail(graph.clone(), root_task_id);
        let loaded_attempt_id = RecursiveLiveAttemptId::new();
        let selected_attempt_id = RecursiveLiveAttemptId::new();
        detail.live_attempts.push(live_attempt_item(
            graph_id,
            root_task_id,
            loaded_attempt_id,
            RecursiveLiveAttemptStatus::Running,
            RecursiveLiveRecoveryStatus::None,
            None,
        ));
        detail.live_attempts.push(live_attempt_item(
            graph_id,
            root_task_id,
            selected_attempt_id,
            RecursiveLiveAttemptStatus::Running,
            RecursiveLiveRecoveryStatus::None,
            None,
        ));
        let mut state = RecursiveDagBrowserState::ready(
            None,
            DaemonCapabilities::default(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        state.inspector.live_attempt_artifacts = Some(RecursiveDagLiveAttemptArtifactsState {
            graph_id,
            live_attempt_id: loaded_attempt_id,
            status: RecursiveDagInspectorLoadStatus::Ready,
            artifacts: Some(RecursiveLiveAttemptArtifactReadback {
                prompt_artifact: Some(execution_artifact(graph_id, root_task_id, 2, "prompt")),
                raw_output_artifacts: Vec::new(),
                normalized_output_artifact: None,
                validation_artifacts: Vec::new(),
                diff_artifacts: Vec::new(),
                test_artifacts: Vec::new(),
                produced_artifacts: Vec::new(),
            }),
            message: None,
            warnings: Vec::new(),
        });

        state.selected_live_attempt = 1;
        assert!(state.selected_live_attempt_artifacts().is_none());
        assert!(
            !state
                .inspector_rows()
                .iter()
                .any(|row| row.source.label().contains("live:prompt"))
        );

        state.selected_live_attempt = 0;
        assert!(state.selected_live_attempt_artifacts().is_some());
        assert!(
            state
                .inspector_rows()
                .iter()
                .any(|row| row.source.label().contains("live:prompt"))
        );
    }

    #[test]
    fn artifact_summary_page_state_clamps_page_size_and_control_rows() {
        let graph_id = RecursiveTaskGraphId::new();
        let ready = RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            Vec::new(),
            u32::MAX,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );

        assert_eq!(ready.limit, RECURSIVE_DAG_ARTIFACT_LIMIT as u32);
        assert_eq!(ready.loaded_pages, 1);
        let load_more = artifact_summary_page_control_row(&ready).expect("load-more row");
        assert_eq!(
            load_more.artifact_page_action(),
            Some(RecursiveDagArtifactPageAction::LoadMore)
        );

        let mut loading = ready.clone();
        loading.mark_loading_more();
        assert_eq!(
            artifact_summary_page_control_row(&loading).and_then(|row| row.artifact_page_action()),
            Some(RecursiveDagArtifactPageAction::Loading)
        );

        let mut page_cap = ready.clone();
        page_cap.loaded_pages = RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT;
        page_cap.page_cap_reached = true;
        assert_eq!(
            artifact_summary_page_control_row(&page_cap).and_then(|row| row.artifact_page_action()),
            Some(RecursiveDagArtifactPageAction::PageCap)
        );

        let disabled = RecursiveDagArtifactSummaryPageState::capability_disabled(graph_id);
        assert_eq!(
            artifact_summary_page_control_row(&disabled).and_then(|row| row.artifact_page_action()),
            Some(RecursiveDagArtifactPageAction::CapabilityDisabled)
        );
    }

    #[test]
    fn artifact_summary_error_with_retained_rows_renders_retry_load_more_row() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let mut page = RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            vec![artifact_summary(graph_id, root_task_id, 1, "first")],
            RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );
        page.status = RecursiveDagInspectorLoadStatus::Error;
        page.message = Some("artifact summary load-more unavailable: socket closed".to_string());

        let row = artifact_summary_page_control_row(&page).expect("retry row");

        assert_eq!(
            row.artifact_page_action(),
            Some(RecursiveDagArtifactPageAction::LoadMore)
        );
        assert!(row.label.contains("retry load-more"));
    }

    #[test]
    fn inspector_artifact_selection_changes_are_clamped() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id, "loaded");
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "first",
        ));
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            2,
            "second",
        ));
        let mut state = RecursiveDagBrowserState::ready(
            None,
            DaemonCapabilities::default(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        state.panel = RecursiveDagPanel::Artifacts;

        state.move_selected(1);
        assert_eq!(state.selected_artifact, 1);
        assert!(
            state
                .selected_inspector_row()
                .is_some_and(|row| row.label.contains("second"))
        );

        state.move_selected(99);
        assert_eq!(state.selected_artifact, 1);
        state.move_selected(-99);
        assert_eq!(state.selected_artifact, 0);
    }
}
