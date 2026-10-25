//! Bounded SQLite source reads for Remote V1 (#2410). The daemon's RPC owner
//! (`rsid::remote_read`) supplies a read transaction, project authorization,
//! runtime observations and transport lifetime guards; this leaf crate takes a
//! borrowed `rusqlite::Connection` plus value observations and never acquires a
//! Store lock, authorizes a request or owns a transport permit. No function here
//! exposes an agent control method.
//!
//! It must stay a leaf: `rsi-common` and third-party libraries only, never
//! `rsid`, `rsid-store` or `rsid-core` (the store/core seal in
//! `crates/rsid/tests/store_crate_seal.rs`).

mod cursor;
mod cursor_pages;
mod dashboard;
mod decisions_cursor;
mod detail;
mod history;
mod pending;
mod projection;
mod projects;
mod providers;
mod responses;
mod selected_session;
mod sessions;
mod source;

pub use cursor::RemoteCursorSigner;
pub use dashboard::{
    DASHBOARD_AGENT_ROWS_SQL, DASHBOARD_SATELLITES_SQL, DashboardSatelliteReport, DashboardSource,
    dashboard_response, dashboard_source,
};
pub use decisions_cursor::DecisionsPositionV1;
pub use detail::{SessionDetailSource, session_detail_source};
pub use history::{
    HistoryObservedPage, HistoryRange, HistoryRow, history_page, observed_history_page,
    tool_pair_ambiguous,
};
pub use pending::{
    DurablePendingSelection, NativeRuntimeApprovalListRecheckObservation,
    NativeRuntimeApprovalListSnapshot, NativeRuntimeApprovalListState,
    NativeRuntimeApprovalRecheck, NativeRuntimeApprovalRecheckObservation,
    NativeRuntimeApprovalRow, NativeRuntimeApprovalSnapshot, NativeRuntimeApprovalState,
    PendingCandidate, PendingKey, PendingSource, PendingSourceRow, PendingUnionCandidate,
    PendingUnionSelection, QuestionSlotGeneration, QuestionSlotMirror, SelectedPendingSource,
    durable_question_fallback, pending_candidate_hydrate, pending_source_exact,
    pending_source_key_page, pending_source_page, select_durable_pending_keys,
    select_pending_union_keys, selected_pending_source,
};
pub use projection::{
    PendingAcquiredSources, PendingCoverageInputs, PendingFinishedPage, PendingPageCandidate,
    PendingPageInputs, PendingPagePosition, PendingPageSelection, PendingPreparedPage, PendingRead,
    PendingResponseInputs, PendingResponseStatus, PendingStoreSources, PendingUnionProjection,
    QuestionProjection, RuntimeOnlyPendingCapture, RuntimeOnlyPendingSources,
    RuntimeOnlyPreparedPage, RuntimeQuestionSlotListEntry,
    RuntimeQuestionSlotListRecheckObservation, RuntimeQuestionSlotListSnapshot,
    RuntimeQuestionSlotRecheck, RuntimeQuestionSlotRecheckObservation, RuntimeQuestionSlotSnapshot,
    RuntimeQuestionSlotState, SavedPendingRuntimeCapture, approval_display,
    capture_runtime_only_pending_sources, capture_saved_pending_runtime_sources,
    durable_approval_summary, durable_question_summary, finish_runtime_only_pending_sources,
    finish_saved_pending_sources, history_event, missing_selected_decision, native_approval_union,
    pending_decisions_response, pending_response_status, pending_source_coverage,
    pending_store_fallback, pending_store_sources, pending_store_sources_with_fallback,
    prepare_pending_page, project, project_pending_page_candidates, project_pending_union_keys,
    project_questions, runtime_native_approval_summary, runtime_question_slot_summary,
    select_pending_page_keys, selected_runtime_only_decision, selected_saved_decision,
    selected_saved_pending_sources, session_attention, session_detail, session_summary,
};
pub use projects::{ProjectRow, list_projects};
pub use providers::{
    PROVIDER_WINDOWS_SQL, ProviderSpendRow, ProviderSpendView, ProvidersSource, providers_response,
    providers_source,
};
pub use responses::{
    SessionsResponseSources, info_response, observed_history_response, projects_response,
    selected_session_response, sessions_response,
};
pub use selected_session::{
    SelectedRuntimeOnlySession, SelectedRuntimeSession, SelectedSavedSession,
    SelectedSessionSources, finish_runtime_only_session_response, finish_selected_session_response,
    runtime_summary_row, selected_runtime_store_miss, selected_saved_session,
};
pub use sessions::{
    RuntimeSessionCandidate, RuntimeSessionCandidateSnapshot, RuntimeSessionObservation,
    RuntimeSessionRecheck, RuntimeSessionRecheckObservation, SessionCandidateOrigin,
    SessionCandidateSelection, SessionRow, get_session, list_merged_session_candidates,
    list_sessions, selected_session_coverage, stored_has_candidate_after,
};
pub use source::{
    BoundedText, MAX_TEXT_BYTES, ReadError, Result, SourcePage, require_session_project,
    source_uuid,
};
pub(crate) use source::{bounded_column, bounded_text};
