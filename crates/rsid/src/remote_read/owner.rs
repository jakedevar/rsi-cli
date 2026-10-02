use super::projection::{pending_store_fallback, pending_store_sources_with_fallback};
use super::sessions::stored_has_candidate_after;
use super::{
    HistoryRange, NativeRuntimeApprovalListState, NativeRuntimeApprovalRecheck,
    PendingAcquiredSources, PendingKey, PendingPagePosition, PendingRead, PendingResponseInputs,
    ReadError, RemoteCursorSigner, RemoteReadCompleted, RemoteReadLimiter, Result,
    RuntimeQuestionSlotRecheck, RuntimeSessionCandidate, RuntimeSessionCandidateSnapshot,
    RuntimeSessionObservation, RuntimeSessionRecheck, SavedPendingRuntimeCapture,
    SelectedRuntimeOnlySession, SelectedRuntimeSession, SelectedSavedSession,
    SelectedSessionSources, SessionCandidateOrigin, SessionCandidateSelection, SessionRow,
    SourcePage, finish_runtime_only_session_response, finish_selected_session_response,
    get_session, info_response, list_merged_session_candidates, list_projects,
    observed_history_page, observed_history_response, pending_decisions_response,
    pending_store_sources, project, projects_response, selected_pending_source,
    selected_runtime_only_decision, selected_runtime_store_miss, selected_saved_decision,
    selected_saved_pending_sources, selected_saved_session, session_summary, sessions_response,
};
use crate::{session::SessionManager, store::Store};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::remote_read::{
    AttentionV1, CoverageStateV1, DecimalU64, DecisionsResponseV1, HistoryResponseV1,
    HistoryWindowV1, InfoResponseV1, ProjectsResponseV1, ReadRequestV1, SelectedDecisionV1,
    SessionResponseV1, SessionsResponseV1, SourceCoverageV1, SourceV1, Timestamp,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Admit public Remote v1 contract discovery under the same response-lifetime
/// permit used by scoped reads. No Store or runtime source is needed.
pub fn spawn_info_read(
    limiter: &RemoteReadLimiter,
    request: &ReadRequestV1,
    daemon_epoch: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<InfoResponseV1>>> {
    if !matches!(request, ReadRequestV1::RemoteGetInfoV1(_)) {
        return Err(ReadError::InvalidSource);
    }
    limiter.spawn(move |budget| {
        budget.check()?;
        info_response(daemon_epoch, Utc::now())
    })
}

/// Execute one project-list request using a trusted configured ID set and
/// policy-scope digest. The RPC owner derives both from its authorization
/// context; the request alone never grants project visibility.
pub fn spawn_projects_read(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    signer: Arc<RemoteCursorSigner>,
    request: ReadRequestV1,
    trusted_configured: Vec<Uuid>,
    policy_scope: [u8; 32],
    daemon_epoch: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<ProjectsResponseV1>>> {
    let ReadRequestV1::RemoteListProjectsV1(params) = &request else {
        return Err(ReadError::InvalidSource);
    };
    if trusted_configured.len() > 32 {
        return Err(ReadError::ResourceLimit);
    }
    let mut configured = trusted_configured;
    configured.sort_unstable();
    if configured.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ReadError::InvalidSource);
    }
    let mut requested = params
        .project_ids
        .iter()
        .map(|id| Uuid::parse_str(id.as_str()).map_err(|_| ReadError::InvalidSource))
        .collect::<Result<Vec<_>>>()?;
    requested.sort_unstable();
    if requested != configured || !(1..=50).contains(&params.limit) {
        return Err(ReadError::InvalidSource);
    }
    let limit = params.limit as usize;
    let incoming = params.cursor.clone();
    let capture_signer = Arc::clone(&signer);
    let capture_request = request.clone();
    limiter.spawn_staged_store(
        store,
        move |_| {
            let after = incoming
                .as_ref()
                .map(|cursor| {
                    capture_signer.verify_list_position(&capture_request, policy_scope, cursor)
                })
                .transpose()?;
            Ok(after)
        },
        {
            let configured = configured.clone();
            move |conn, after, _| {
                let page = list_projects(conn, &configured, *after, limit)?;
                Ok((page, Utc::now()))
            }
        },
        move |after, (page, observed_at), _| {
            let cursor =
                signer.sign_list_position(&request, policy_scope, page.next, page.has_more)?;
            projects_response(
                &configured,
                after,
                limit,
                page,
                daemon_epoch,
                observed_at,
                cursor,
            )
        },
    )
}

struct SavedSessionsPage {
    project: super::ProjectRow,
    page: SourcePage<SessionCandidateSelection, Uuid>,
    rows: Vec<Option<SessionRow>>,
    store_has_more: bool,
    observed_at: DateTime<Utc>,
}

enum SelectedSessionStore {
    Saved {
        selected: SelectedSavedSession,
        pending: Box<super::PendingStoreSources>,
    },
    RuntimeOnly(SelectedRuntimeOnlySession),
}

fn known_runtime_signals(
    native: &super::PendingRead<(
        super::NativeRuntimeApprovalListSnapshot,
        NativeRuntimeApprovalRecheck,
    )>,
    slots: &super::PendingRead<(
        super::RuntimeQuestionSlotListSnapshot,
        RuntimeQuestionSlotRecheck,
    )>,
) -> u64 {
    let native_count = match native {
        super::PendingRead::Observed {
            value: (snapshot, NativeRuntimeApprovalRecheck::NoObservedChange),
            ..
        } => match &snapshot.state {
            NativeRuntimeApprovalListState::Present(rows) => {
                u64::try_from(rows.len()).unwrap_or(u64::MAX)
            }
            _ => 0,
        },
        _ => 0,
    };
    let slot_count = match slots {
        super::PendingRead::Observed {
            value: (snapshot, RuntimeQuestionSlotRecheck::NoObservedChange),
            ..
        } if !snapshot.slots.is_empty() => 1,
        _ => 0,
    };
    native_count.saturating_add(slot_count)
}

/// Read one exact selected session under one permit. Runtime sources are
/// copied before Store and reread only after its transaction is released.
#[allow(clippy::too_many_lines)] // Keep capture, Store and reread in one guarded sequence.
pub fn spawn_selected_session_read(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    manager: Arc<SessionManager>,
    request: &ReadRequestV1,
    trusted_project: Uuid,
    daemon_epoch: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<SessionResponseV1>>> {
    let ReadRequestV1::RemoteGetSessionV1(params) = request else {
        return Err(ReadError::InvalidSource);
    };
    let project =
        Uuid::parse_str(params.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let session =
        Uuid::parse_str(params.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    if project != trusted_project {
        return Err(ReadError::InvalidSource);
    }
    let request_params = params.clone();
    let capture_manager = Arc::clone(&manager);
    limiter.spawn_staged_store(
        store,
        move |_| {
            let runtime = capture_manager.remote_selected_runtime_session(project, session)?;
            let candidates = capture_manager.remote_runtime_session_candidates()?;
            let pending = capture_manager.remote_saved_pending_capture(project, session)?;
            Ok((candidates, runtime, pending))
        },
        move |conn, (_, runtime, _), budget| {
            budget.check()?;
            match pending_store_sources(conn, project, session, [None; 4], 1) {
                Ok(pending) => {
                    budget.check()?;
                    let selected = selected_saved_session(conn, project, session)?;
                    Ok(SelectedSessionStore::Saved {
                        selected,
                        pending: Box::new(pending),
                    })
                }
                Err(ReadError::NotFound) => {
                    let runtime = runtime.clone().ok_or(ReadError::NotFound)?;
                    let selected = selected_runtime_store_miss(conn, project, session, runtime)?;
                    Ok(SelectedSessionStore::RuntimeOnly(selected))
                }
                Err(error) => Err(error),
            }
        },
        move |(before, selected_before, pending), selected, budget| {
            budget.check()?;
            let mut after = manager.remote_runtime_session_recheck(&before)?;
            let selected_after = if selected_before.is_some() {
                manager.remote_selected_runtime_session(project, session)?
            } else {
                None
            };
            let runtime_sequences = match (&selected_before, &selected_after) {
                (Some(before), Some(reread)) if before.matches_reread(reread) => {
                    before.runtime_sequence().into_iter().collect()
                }
                (Some(_), _) => {
                    after.state = RuntimeSessionRecheck::Changed;
                    Vec::new()
                }
                (None, _) => Vec::new(),
            };
            let sources = |pending_coverage, live_signals_lower_bound| SelectedSessionSources {
                runtime_capture: RuntimeSessionObservation::Observed(&before),
                runtime_reread: Some(RuntimeSessionObservation::Observed(&after)),
                runtime_sequences: runtime_sequences.clone(),
                pending_coverage,
                live_signals_lower_bound,
            };
            match selected {
                SelectedSessionStore::Saved {
                    selected,
                    pending: stored,
                } => {
                    let acquired =
                        manager.remote_saved_pending_finish(&selected, pending, *stored)?;
                    let live =
                        known_runtime_signals(&acquired.native_runtime, &acquired.question_slots);
                    finish_selected_session_response(
                        &request_params,
                        selected,
                        sources(acquired.coverage, live),
                        daemon_epoch,
                        Utc::now(),
                    )
                }
                SelectedSessionStore::RuntimeOnly(selected) => {
                    let reread = selected_after.ok_or(ReadError::SourceUnavailable)?;
                    if !selected.matches_runtime_reread(&reread) {
                        return Err(ReadError::SourceUnavailable);
                    }
                    let acquired = manager.remote_runtime_only_pending_finish(
                        &selected,
                        pending.into_runtime_only(),
                    )?;
                    let live =
                        known_runtime_signals(&acquired.native_runtime, &acquired.question_slots);
                    finish_runtime_only_session_response(
                        &request_params,
                        selected,
                        sources(acquired.coverage, live),
                        daemon_epoch,
                        Utc::now(),
                    )
                }
            }
        },
    )
}

/// Read a merged session page for a project already authorized by the owner.
/// Runtime key capture precedes the Store transaction; selected runtime rows
/// and the key reread follow its release. Any observed change refuses the
/// page, since the three sources are not a frozen snapshot.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Keep all three guarded stages together.
pub fn spawn_sessions_read(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    manager: Arc<SessionManager>,
    signer: Arc<RemoteCursorSigner>,
    request: ReadRequestV1,
    trusted_project: Uuid,
    policy_scope: [u8; 32],
    daemon_epoch: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<SessionsResponseV1>>> {
    let ReadRequestV1::RemoteListSessionsV1(params) = &request else {
        return Err(ReadError::InvalidSource);
    };
    let requested =
        Uuid::parse_str(params.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    if requested != trusted_project || !(1..=100).contains(&params.limit) {
        return Err(ReadError::InvalidSource);
    }
    let incoming = params.cursor.clone();
    let capture_signer = Arc::clone(&signer);
    let capture_request = request.clone();
    let capture_manager = Arc::clone(&manager);
    let limit = params.limit as usize;
    limiter.spawn_staged_store(
        store,
        move |_| {
            let after = incoming
                .as_ref()
                .map(|cursor| {
                    capture_signer.verify_list_position(&capture_request, policy_scope, cursor)
                })
                .transpose()?;
            Ok((after, capture_manager.remote_runtime_session_candidates()?))
        },
        move |conn, (after, before), budget| {
            let project = list_projects(conn, &[trusted_project], None, 1)?
                .items
                .into_iter()
                .next()
                .ok_or(ReadError::NotFound)?;
            budget.check()?;
            let page = list_merged_session_candidates(
                conn,
                trusted_project,
                *after,
                limit,
                &before.active,
                &before.completed,
            )?;
            let mut rows = Vec::with_capacity(page.items.len());
            for selected in &page.items {
                budget.check()?;
                rows.push(match selected.origin {
                    SessionCandidateOrigin::Store => {
                        Some(get_session(conn, trusted_project, selected.id)?)
                    }
                    SessionCandidateOrigin::Active | SessionCandidateOrigin::Completed => None,
                });
            }
            let store_has_more = page
                .next
                .map(|edge| stored_has_candidate_after(conn, trusted_project, edge))
                .transpose()?
                .unwrap_or(false);
            Ok(SavedSessionsPage {
                project,
                page,
                rows,
                store_has_more,
                observed_at: Utc::now(),
            })
        },
        move |(after, before), saved, budget| {
            budget.check()?;
            let runtime_rows = manager.remote_runtime_session_page_rows(
                trusted_project,
                &saved.page.items,
                &before,
            )?;
            let reread = manager.remote_runtime_session_recheck(&before)?;
            if reread.state != RuntimeSessionRecheck::NoObservedChange {
                return Err(ReadError::SourceUnavailable);
            }
            let observed_at = Utc::now();
            let coverage = session_page_coverage(
                trusted_project,
                &before,
                reread.active_observed_at,
                reread.completed_observed_at,
                &saved,
            )?;
            let mut items = Vec::with_capacity(saved.page.items.len());
            for (stored, runtime) in saved.rows.into_iter().zip(runtime_rows) {
                budget.check()?;
                let row = stored.or(runtime).ok_or(ReadError::SourceUnavailable)?;
                let attention = AttentionV1 {
                    requires_local_action: true,
                    incomplete: true,
                    live_signals_lower_bound: DecimalU64::new("0".into())
                        .map_err(|_| ReadError::InvalidSource)?,
                };
                items.push(session_summary(row, trusted_project, attention)?);
            }
            let cursor = signer.sign_list_position(
                &request,
                policy_scope,
                saved.page.next,
                saved.page.has_more,
            )?;
            let ReadRequestV1::RemoteListSessionsV1(params) = &request else {
                return Err(ReadError::InvalidSource);
            };
            sessions_response(
                params,
                super::SessionsResponseSources {
                    project: project(saved.project)?,
                    after,
                    page: SourcePage {
                        items,
                        next: saved.page.next,
                        has_more: saved.page.has_more,
                    },
                    coverage,
                    cursor,
                },
                daemon_epoch,
                observed_at,
            )
        },
    )
}

struct DecisionsCapture {
    runtime: Option<SelectedRuntimeSession>,
    pending: SavedPendingRuntimeCapture,
    position: Option<super::DecisionsPositionV1>,
}

struct SavedRoutedDecisions {
    store: super::PendingStoreSources,
    selected: SelectedSavedSession,
    prepared: super::PendingPreparedPage,
    exact: Option<super::SelectedPendingSource>,
    durable_at: Timestamp,
}

enum RoutedDecisions {
    Saved(Box<SavedRoutedDecisions>),
    RuntimeOnly {
        selected: SelectedRuntimeOnlySession,
        prepared: super::RuntimeOnlyPreparedPage,
    },
}

/// Route an initial or resumed decisions request in its bounded Store
/// transaction. A saved-row miss becomes runtime-only only after the exact
/// session/event Store miss rejects foreign or partially persisted identity.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn spawn_decisions_read(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    manager: Arc<SessionManager>,
    signer: Arc<RemoteCursorSigner>,
    request: ReadRequestV1,
    trusted_project: Uuid,
    policy_scope: [u8; 32],
    daemon_epoch: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<DecisionsResponseV1>>> {
    let ReadRequestV1::RemoteGetDecisionsV1(params) = &request else {
        return Err(ReadError::InvalidSource);
    };
    let project =
        Uuid::parse_str(params.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let session =
        Uuid::parse_str(params.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    if project != trusted_project || !(1..=32).contains(&params.limit) {
        return Err(ReadError::InvalidSource);
    }
    let limit = params.limit as usize;
    let mode = params.mode;
    let incoming = params.cursor.clone();
    let selected_id = params.selected_decision_id.clone();
    let store_selected_id = selected_id.clone();
    let capture_manager = Arc::clone(&manager);
    let verify_signer = Arc::clone(&signer);
    let verify_request = request.clone();
    limiter.spawn_staged_store(
        store,
        move |_| {
            let position = incoming
                .as_ref()
                .map(|cursor| verify_signer.verify_decisions(&verify_request, policy_scope, cursor))
                .transpose()?;
            let runtime = capture_manager.remote_selected_runtime_session(project, session)?;
            let pending = capture_manager.remote_saved_pending_capture(project, session)?;
            Ok(DecisionsCapture {
                runtime,
                pending,
                position,
            })
        },
        move |conn, capture, budget| {
            budget.check()?;
            match pending_store_fallback(conn, project, session) {
                Ok(fallback) => {
                    let (previous_after, start) = if let Some(position) = &capture.position {
                        let present = match &fallback {
                            PendingRead::Observed { value, .. } => value.is_some(),
                            PendingRead::Busy { .. } => return Err(ReadError::Busy),
                            PendingRead::Unavailable { .. } => {
                                return Err(ReadError::SourceUnavailable);
                            }
                        };
                        capture.pending.resume_position(position, present)?
                    } else {
                        (
                            [None; 4],
                            PendingPagePosition {
                                examined: [0; 5],
                                key_bucket: 0,
                                slot_offset: 0,
                                page_bucket: 0,
                            },
                        )
                    };
                    budget.check()?;
                    let stored = pending_store_sources_with_fallback(
                        conn,
                        project,
                        session,
                        previous_after,
                        limit,
                        fallback,
                    )?;
                    let selected = selected_saved_session(conn, project, session)?;
                    let prepared = capture.pending.prepare_page(
                        conn,
                        &stored,
                        previous_after,
                        start,
                        mode,
                        limit,
                    )?;
                    let exact = store_selected_id
                        .as_ref()
                        .map(|id| selected_pending_source(conn, project, session, id))
                        .transpose()?;
                    let durable_at =
                        Timestamp::new(Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true))
                            .map_err(|_| ReadError::InvalidSource)?;
                    Ok(RoutedDecisions::Saved(Box::new(SavedRoutedDecisions {
                        store: stored,
                        selected,
                        prepared,
                        exact,
                        durable_at,
                    })))
                }
                Err(ReadError::NotFound) => {
                    let runtime = capture.runtime.clone().ok_or(ReadError::NotFound)?;
                    let selected = selected_runtime_store_miss(conn, project, session, runtime)?;
                    let prepared = capture.pending.runtime_only().prepare_page(
                        conn,
                        &selected,
                        capture.position.clone(),
                        limit,
                    )?;
                    Ok(RoutedDecisions::RuntimeOnly { selected, prepared })
                }
                Err(error) => Err(error),
            }
        },
        move |capture, routed, budget| {
            budget.check()?;
            let (finished, exact, coverage) = match routed {
                RoutedDecisions::Saved(saved) => {
                    let SavedRoutedDecisions {
                        store,
                        selected,
                        prepared,
                        exact,
                        durable_at,
                    } = *saved;
                    let acquired =
                        manager.remote_saved_pending_finish(&selected, capture.pending, store)?;
                    let finished = prepared.finish(&acquired)?;
                    let exact = match (selected_id.clone(), exact) {
                        (Some(id), Some(source)) => {
                            selected_saved_decision(id, source, &acquired, durable_at)?
                        }
                        (None, None) => SelectedDecisionV1::None {},
                        _ => return Err(ReadError::InvalidSource),
                    };
                    (finished, exact, acquired.coverage)
                }
                RoutedDecisions::RuntimeOnly { selected, prepared } => {
                    let reread = manager
                        .remote_selected_runtime_session(project, session)?
                        .ok_or(ReadError::SourceUnavailable)?;
                    if !selected.matches_runtime_reread(&reread) {
                        return Err(ReadError::SourceUnavailable);
                    }
                    let acquired = manager.remote_runtime_only_pending_finish(
                        &selected,
                        capture.pending.into_runtime_only(),
                    )?;
                    let finished = prepared.finish(&acquired)?;
                    let exact = match selected_id {
                        Some(id) => {
                            selected_runtime_only_decision(&id, project, session, &acquired)?
                        }
                        None => SelectedDecisionV1::None {},
                    };
                    (finished, exact, acquired.coverage)
                }
            };
            let has_more = finished.remaining_in_inputs || coverage.iter().any(|row| row.has_more);
            let cursor = has_more
                .then(|| signer.sign_decisions(&request, policy_scope, &finished.next_position))
                .transpose()?;
            pending_decisions_response(PendingResponseInputs {
                project,
                session,
                daemon_epoch,
                observed_at: Utc::now(),
                mode,
                page_limit: limit as u32,
                items: finished.projection.items,
                selected: exact,
                coverage,
                page_remaining_in_inputs: finished.remaining_in_inputs,
                next_cursor: cursor,
            })
        },
    )
}

/// Owned source observations for an initial saved-session decisions read.
/// The caller still has to select keys, project decisions and sign a cursor.
pub struct InitialSavedDecisionsSources {
    pub selected: SelectedSavedSession,
    pub acquired: PendingAcquiredSources,
}

/// Acquire the initial saved-session decision sources under one Remote permit.
/// Runtime observations are copied before the Store transaction and reread
/// after it is released. A resumed cursor needs an additional witness-bound
/// position step and is refused here until that owner path is complete.
pub fn spawn_initial_saved_decisions_sources(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    manager: Arc<SessionManager>,
    request: &ReadRequestV1,
    trusted_project: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<InitialSavedDecisionsSources>>> {
    let ReadRequestV1::RemoteGetDecisionsV1(params) = request else {
        return Err(ReadError::InvalidSource);
    };
    let project =
        Uuid::parse_str(params.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let session =
        Uuid::parse_str(params.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    if project != trusted_project
        || !(1..=32).contains(&params.limit)
        || params.cursor.is_some()
        || params.selected_decision_id.is_some()
    {
        return Err(ReadError::InvalidSource);
    }
    let capture_manager = Arc::clone(&manager);
    let limit = params.limit as usize;
    limiter.spawn_staged_store(
        store,
        move |_| capture_manager.remote_saved_pending_capture(project, session),
        move |conn, _, budget| {
            budget.check()?;
            selected_saved_pending_sources(conn, project, session, [None; 4], limit)
        },
        move |capture, (store_sources, selected), budget| {
            budget.check()?;
            let acquired =
                manager.remote_saved_pending_finish(&selected, capture, store_sources)?;
            Ok(InitialSavedDecisionsSources { selected, acquired })
        },
    )
}

/// Assemble an initial unselected saved-session decisions page. Selection and
/// chosen payload hydration stay inside one bounded Store transaction; the
/// response and next cursor are produced only after both runtime rereads.
#[allow(clippy::too_many_arguments)]
pub fn spawn_initial_saved_decisions_page(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    manager: Arc<SessionManager>,
    signer: Arc<RemoteCursorSigner>,
    request: ReadRequestV1,
    trusted_project: Uuid,
    policy_scope: [u8; 32],
    daemon_epoch: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<DecisionsResponseV1>>> {
    let ReadRequestV1::RemoteGetDecisionsV1(params) = &request else {
        return Err(ReadError::InvalidSource);
    };
    let project =
        Uuid::parse_str(params.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let session =
        Uuid::parse_str(params.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    if project != trusted_project || !(1..=32).contains(&params.limit) || params.cursor.is_some() {
        return Err(ReadError::InvalidSource);
    }
    let limit = params.limit as usize;
    let mode = params.mode;
    let selected_id = params.selected_decision_id.clone();
    let store_selected_id = selected_id.clone();
    let capture_manager = Arc::clone(&manager);
    limiter.spawn_staged_store(
        store,
        move |_| capture_manager.remote_saved_pending_capture(project, session),
        move |conn, capture, budget| {
            budget.check()?;
            let (store_sources, selected) =
                selected_saved_pending_sources(conn, project, session, [None; 4], limit)?;
            let prepared = capture.prepare_page(
                conn,
                &store_sources,
                [None; 4],
                PendingPagePosition {
                    examined: [0; 5],
                    key_bucket: 0,
                    slot_offset: 0,
                    page_bucket: 0,
                },
                mode,
                limit,
            )?;
            let exact_source = store_selected_id
                .as_ref()
                .map(|id| selected_pending_source(conn, project, session, id))
                .transpose()?;
            let durable_at = Timestamp::new(Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true))
                .map_err(|_| ReadError::InvalidSource)?;
            Ok((store_sources, selected, prepared, exact_source, durable_at))
        },
        move |capture, (store_sources, selected, prepared, exact_source, durable_at), budget| {
            budget.check()?;
            let acquired =
                manager.remote_saved_pending_finish(&selected, capture, store_sources)?;
            let finished = prepared.finish(&acquired)?;
            let exact = match (selected_id, exact_source) {
                (Some(id), Some(source)) => {
                    selected_saved_decision(id, source, &acquired, durable_at)?
                }
                (None, None) => SelectedDecisionV1::None {},
                _ => return Err(ReadError::InvalidSource),
            };
            let has_more =
                finished.remaining_in_inputs || acquired.coverage.iter().any(|row| row.has_more);
            let cursor = has_more
                .then(|| signer.sign_decisions(&request, policy_scope, &finished.next_position))
                .transpose()?;
            pending_decisions_response(PendingResponseInputs {
                project,
                session,
                daemon_epoch,
                observed_at: Utc::now(),
                mode,
                page_limit: limit as u32,
                items: finished.projection.items,
                selected: exact,
                coverage: acquired.coverage,
                page_remaining_in_inputs: finished.remaining_in_inputs,
                next_cursor: cursor,
            })
        },
    )
}

pub struct ResumedSavedDecisionsSources {
    pub selected: SelectedSavedSession,
    pub acquired: PendingAcquiredSources,
    pub previous_after: [Option<PendingKey>; 4],
    pub start: PendingPagePosition,
}

/// Resume a saved-session decisions page only when its signed position still
/// matches captured runtime identities and the transaction's fallback slot.
/// Source projection and signing the next page remain with the caller.
#[allow(clippy::too_many_arguments)]
pub fn spawn_resumed_saved_decisions_sources(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    manager: Arc<SessionManager>,
    signer: Arc<RemoteCursorSigner>,
    request: &ReadRequestV1,
    trusted_project: Uuid,
    policy_scope: [u8; 32],
) -> Result<JoinHandle<RemoteReadCompleted<ResumedSavedDecisionsSources>>> {
    let ReadRequestV1::RemoteGetDecisionsV1(params) = request else {
        return Err(ReadError::InvalidSource);
    };
    let project =
        Uuid::parse_str(params.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let session =
        Uuid::parse_str(params.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let incoming = params.cursor.clone().ok_or(ReadError::InvalidSource)?;
    if project != trusted_project
        || !(1..=32).contains(&params.limit)
        || params.selected_decision_id.is_some()
    {
        return Err(ReadError::InvalidSource);
    }
    let request = request.clone();
    let capture_manager = Arc::clone(&manager);
    let limit = params.limit as usize;
    limiter.spawn_staged_store(
        store,
        move |_| {
            let position = signer.verify_decisions(&request, policy_scope, &incoming)?;
            let capture = capture_manager.remote_saved_pending_capture(project, session)?;
            Ok((position, capture))
        },
        move |conn, (position, capture), budget| {
            budget.check()?;
            let fallback = pending_store_fallback(conn, project, session)?;
            let fallback_present = match &fallback {
                PendingRead::Observed { value, .. } => value.is_some(),
                PendingRead::Busy { .. } => return Err(ReadError::Busy),
                PendingRead::Unavailable { .. } => return Err(ReadError::SourceUnavailable),
            };
            let (previous_after, start) = capture.resume_position(position, fallback_present)?;
            budget.check()?;
            let store_sources = pending_store_sources_with_fallback(
                conn,
                project,
                session,
                previous_after,
                limit,
                fallback,
            )?;
            let selected = selected_saved_session(conn, project, session)?;
            Ok((store_sources, selected, previous_after, start))
        },
        move |(_, capture), (store_sources, selected, previous_after, start), budget| {
            budget.check()?;
            let acquired =
                manager.remote_saved_pending_finish(&selected, capture, store_sources)?;
            let native_unchanged = matches!(
                &acquired.native_runtime,
                PendingRead::Observed {
                    value: (_, super::NativeRuntimeApprovalRecheck::NoObservedChange),
                    ..
                }
            );
            let slots_unchanged = matches!(
                &acquired.question_slots,
                PendingRead::Observed {
                    value: (_, super::RuntimeQuestionSlotRecheck::NoObservedChange),
                    ..
                }
            );
            if !native_unchanged || !slots_unchanged {
                return Err(ReadError::SourceUnavailable);
            }
            Ok(ResumedSavedDecisionsSources {
                selected,
                acquired,
                previous_after,
                start,
            })
        },
    )
}

/// Resume a saved-session decisions page from a verified cursor.
/// The cursor's runtime witnesses and durable fallback are checked before
/// scheduling; selected payloads are copied in Store and projected only after
/// both runtime sources have been reread outside Store.
#[allow(clippy::too_many_arguments)]
pub fn spawn_resumed_saved_decisions_page(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    manager: Arc<SessionManager>,
    signer: Arc<RemoteCursorSigner>,
    request: ReadRequestV1,
    trusted_project: Uuid,
    policy_scope: [u8; 32],
    daemon_epoch: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<DecisionsResponseV1>>> {
    let ReadRequestV1::RemoteGetDecisionsV1(params) = &request else {
        return Err(ReadError::InvalidSource);
    };
    let project =
        Uuid::parse_str(params.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let session =
        Uuid::parse_str(params.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let incoming = params.cursor.clone().ok_or(ReadError::InvalidSource)?;
    if project != trusted_project || !(1..=32).contains(&params.limit) {
        return Err(ReadError::InvalidSource);
    }
    let limit = params.limit as usize;
    let mode = params.mode;
    let selected_id = params.selected_decision_id.clone();
    let store_selected_id = selected_id.clone();
    let capture_manager = Arc::clone(&manager);
    let verify_signer = Arc::clone(&signer);
    let verify_request = request.clone();
    limiter.spawn_staged_store(
        store,
        move |_| {
            let position =
                verify_signer.verify_decisions(&verify_request, policy_scope, &incoming)?;
            let capture = capture_manager.remote_saved_pending_capture(project, session)?;
            Ok((position, capture))
        },
        move |conn, (position, capture), budget| {
            budget.check()?;
            let fallback = pending_store_fallback(conn, project, session)?;
            let fallback_present = match &fallback {
                PendingRead::Observed { value, .. } => value.is_some(),
                PendingRead::Busy { .. } => return Err(ReadError::Busy),
                PendingRead::Unavailable { .. } => return Err(ReadError::SourceUnavailable),
            };
            let (previous_after, start) = capture.resume_position(position, fallback_present)?;
            budget.check()?;
            let store_sources = pending_store_sources_with_fallback(
                conn,
                project,
                session,
                previous_after,
                limit,
                fallback,
            )?;
            let selected = selected_saved_session(conn, project, session)?;
            let prepared =
                capture.prepare_page(conn, &store_sources, previous_after, start, mode, limit)?;
            let exact_source = store_selected_id
                .as_ref()
                .map(|id| selected_pending_source(conn, project, session, id))
                .transpose()?;
            let durable_at = Timestamp::new(Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true))
                .map_err(|_| ReadError::InvalidSource)?;
            Ok((store_sources, selected, prepared, exact_source, durable_at))
        },
        move |(_, capture),
              (store_sources, selected, prepared, exact_source, durable_at),
              budget| {
            budget.check()?;
            let acquired =
                manager.remote_saved_pending_finish(&selected, capture, store_sources)?;
            let finished = prepared.finish(&acquired)?;
            let exact = match (selected_id, exact_source) {
                (Some(id), Some(source)) => {
                    selected_saved_decision(id, source, &acquired, durable_at)?
                }
                (None, None) => SelectedDecisionV1::None {},
                _ => return Err(ReadError::InvalidSource),
            };
            let has_more =
                finished.remaining_in_inputs || acquired.coverage.iter().any(|row| row.has_more);
            let cursor = has_more
                .then(|| signer.sign_decisions(&request, policy_scope, &finished.next_position))
                .transpose()?;
            pending_decisions_response(PendingResponseInputs {
                project,
                session,
                daemon_epoch,
                observed_at: Utc::now(),
                mode,
                page_limit: limit as u32,
                items: finished.projection.items,
                selected: exact,
                coverage: acquired.coverage,
                page_remaining_in_inputs: finished.remaining_in_inputs,
                next_cursor: cursor,
            })
        },
    )
}

/// Read a runtime-only decisions page under one permit. The runtime session
/// and pending sources are captured before an exact session/event Store miss;
/// all three runtime witnesses are reread after Store release. Saved-session
/// pending sources retain Unavailable coverage even when their scheduler
/// slices are empty.
#[allow(clippy::too_many_arguments)]
pub fn spawn_runtime_only_decisions_page(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    manager: Arc<SessionManager>,
    signer: Arc<RemoteCursorSigner>,
    request: ReadRequestV1,
    trusted_project: Uuid,
    policy_scope: [u8; 32],
    daemon_epoch: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<DecisionsResponseV1>>> {
    let ReadRequestV1::RemoteGetDecisionsV1(params) = &request else {
        return Err(ReadError::InvalidSource);
    };
    let project =
        Uuid::parse_str(params.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let session =
        Uuid::parse_str(params.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    if project != trusted_project || !(1..=32).contains(&params.limit) {
        return Err(ReadError::InvalidSource);
    }
    let limit = params.limit as usize;
    let mode = params.mode;
    let selected_id = params.selected_decision_id.clone();
    let incoming = params.cursor.clone();
    let verify_signer = Arc::clone(&signer);
    let verify_request = request.clone();
    let capture_manager = Arc::clone(&manager);
    limiter.spawn_staged_store(
        store,
        move |_| {
            let position = incoming
                .as_ref()
                .map(|cursor| verify_signer.verify_decisions(&verify_request, policy_scope, cursor))
                .transpose()?;
            let runtime = capture_manager
                .remote_selected_runtime_session(project, session)?
                .ok_or(ReadError::NotFound)?;
            let pending = capture_manager.remote_runtime_only_pending_capture(project, session)?;
            Ok((runtime, pending, position))
        },
        move |conn, (runtime, pending, position), budget| {
            budget.check()?;
            let selected = selected_runtime_store_miss(conn, project, session, runtime.clone())?;
            let prepared = pending.prepare_page(conn, &selected, position.clone(), limit)?;
            Ok((selected, prepared))
        },
        move |(_, pending, _), (selected, prepared), budget| {
            budget.check()?;
            let after = manager
                .remote_selected_runtime_session(project, session)?
                .ok_or(ReadError::SourceUnavailable)?;
            if !selected.matches_runtime_reread(&after) {
                return Err(ReadError::SourceUnavailable);
            }
            let acquired = manager.remote_runtime_only_pending_finish(&selected, pending)?;
            let finished = prepared.finish(&acquired)?;
            let exact = match selected_id {
                Some(id) => selected_runtime_only_decision(&id, project, session, &acquired)?,
                None => SelectedDecisionV1::None {},
            };
            let has_more =
                finished.remaining_in_inputs || acquired.coverage.iter().any(|row| row.has_more);
            let cursor = has_more
                .then(|| signer.sign_decisions(&request, policy_scope, &finished.next_position))
                .transpose()?;
            pending_decisions_response(PendingResponseInputs {
                project,
                session,
                daemon_epoch,
                observed_at: Utc::now(),
                mode,
                page_limit: limit as u32,
                items: finished.projection.items,
                selected: exact,
                coverage: acquired.coverage,
                page_remaining_in_inputs: finished.remaining_in_inputs,
                next_cursor: cursor,
            })
        },
    )
}

fn session_page_coverage(
    project: Uuid,
    before: &RuntimeSessionCandidateSnapshot,
    active_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    saved: &SavedSessionsPage,
) -> Result<[SourceCoverageV1; 3]> {
    let source_row =
        |source, at: DateTime<Utc>, count: usize, more, order| -> Result<SourceCoverageV1> {
            Ok(SourceCoverageV1 {
                source,
                state: CoverageStateV1::Complete,
                has_more: more,
                lower_bound: DecimalU64::new(count.to_string())
                    .map_err(|_| ReadError::InvalidSource)?,
                observed_at: Timestamp::new(at.to_rfc3339_opts(SecondsFormat::Nanos, true))
                    .map_err(|_| ReadError::InvalidSource)?,
                observation_order: order,
            })
        };
    let source_count = |rows: &[RuntimeSessionCandidate]| {
        rows.iter()
            .filter(|row| row.project_id == Some(project) && !row.is_eval)
            .count()
    };
    let source_more = |rows: &[RuntimeSessionCandidate]| {
        saved.page.next.is_some_and(|edge| {
            rows.iter()
                .any(|row| row.project_id == Some(project) && row.id > edge)
        })
    };
    Ok([
        source_row(
            SourceV1::ActiveSessions,
            active_at,
            source_count(&before.active),
            source_more(&before.active),
            1,
        )?,
        source_row(
            SourceV1::CompletedSessions,
            completed_at,
            source_count(&before.completed),
            source_more(&before.completed),
            2,
        )?,
        source_row(
            SourceV1::StoreSessions,
            saved.observed_at,
            saved.rows.iter().flatten().count(),
            saved.store_has_more,
            3,
        )?,
    ])
}

/// Execute a saved-history page for a project already authorized by the RPC
/// owner. The request's project is checked against that trusted scope before
/// admission. Pairing and page projection run within the Store budget because
/// they require exact indexed reads on the same transaction.
pub fn spawn_history_read(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    signer: &Arc<RemoteCursorSigner>,
    request: &ReadRequestV1,
    trusted_project: Uuid,
    policy_scope: [u8; 32],
    daemon_epoch: Uuid,
) -> Result<JoinHandle<RemoteReadCompleted<HistoryResponseV1>>> {
    let ReadRequestV1::RemoteGetHistoryPageV1(params) = request else {
        return Err(ReadError::InvalidSource);
    };
    let project =
        Uuid::parse_str(params.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let session =
        Uuid::parse_str(params.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    if project != trusted_project || !(1..=50).contains(&params.limit) {
        return Err(ReadError::InvalidSource);
    }
    let incoming = params.cursor.clone();
    let capture_signer = Arc::clone(signer);
    let capture_request = (*request).clone();
    let store_signer = Arc::clone(signer);
    let store_request = (*request).clone();
    let store_params = (*params).clone();
    limiter.spawn_staged_store(
        store,
        move |_| {
            incoming
                .as_ref()
                .map(|cursor| {
                    capture_signer.verify_history_position(&capture_request, policy_scope, cursor)
                })
                .transpose()?
                .map_or_else(|| initial_history_range(&capture_request), Ok)
        },
        move |conn, range, _| {
            let observed =
                observed_history_page(conn, project, session, *range, store_params.limit as usize)?;
            let cursor = store_signer.sign_history_position(
                &store_request,
                policy_scope,
                observed.page.next,
                observed.page.has_more,
            )?;
            observed_history_response(
                conn,
                &store_params,
                observed,
                daemon_epoch,
                Utc::now(),
                cursor,
            )
        },
        |_, response, _| Ok(response),
    )
}

fn initial_history_range(request: &ReadRequestV1) -> Result<HistoryRange> {
    let ReadRequestV1::RemoteGetHistoryPageV1(params) = request else {
        return Err(ReadError::InvalidSource);
    };
    let key = |event: &rsi_common::remote_read::EventKeyV1| (event.sequence, event.id.get());
    let range = match &params.window {
        HistoryWindowV1::Latest {} => HistoryRange::Latest,
        HistoryWindowV1::Older { anchor } => HistoryRange::Older {
            anchor: key(anchor),
        },
        HistoryWindowV1::Newer { anchor, through } => HistoryRange::Newer {
            anchor: key(anchor),
            through: through.as_ref().map(key),
        },
        HistoryWindowV1::Interval {
            lower_exclusive,
            upper_inclusive,
        } => HistoryRange::Interval {
            lower_exclusive: key(lower_exclusive),
            upper_inclusive: key(upper_inclusive),
        },
        HistoryWindowV1::Locate { .. } => return Err(ReadError::SourceUnavailable),
    };
    Ok(range)
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::{
        bus::EventBus,
        config::{Config, RuntimeConfig},
    };
    use rsi_common::remote_read::CursorV1;
    use rusqlite::params;
    use tokio::io::AsyncReadExt;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn info_owner_reports_fixed_capabilities_under_permit() {
        let limiter = RemoteReadLimiter::new();
        let epoch = Uuid::new_v4();
        let request: ReadRequestV1 = serde_json::from_value(serde_json::json!({
            "method":"RemoteGetInfoV1","params":{}
        }))
        .unwrap();
        let wrong: ReadRequestV1 = serde_json::from_value(serde_json::json!({
            "method":"RemoteGetSessionV1",
            "params":{"project_id":Uuid::new_v4(),"session_id":Uuid::new_v4()}
        }))
        .unwrap();
        assert!(matches!(
            spawn_info_read(&limiter, &wrong, epoch),
            Err(ReadError::InvalidSource)
        ));
        let completed = spawn_info_read(&limiter, &request, epoch)
            .unwrap()
            .await
            .unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        completed
            .send_json_line(&mut writer, |result| result.expect("info response"))
            .await
            .unwrap();
        drop(writer);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        let response: InfoResponseV1 = serde_json::from_slice(&bytes).unwrap();
        assert!(response.complete);
        assert_eq!(response.item.daemon_boot_id.as_str(), epoch.to_string());
        assert_eq!(
            response.item.required_capabilities,
            rsi_common::remote_read::REQUIRED_CAPABILITIES
        );
        assert!(response.coverage.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn sessions_owner_rechecks_runtime_and_preserves_hidden_saved_cursor() {
        let store = Store::open_in_memory().unwrap();
        let project_id = Uuid::new_v4();
        let ids = [
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        ];
        let at = Utc::now().to_rfc3339();
        store
            .conn
            .execute(
                "INSERT INTO projects(id,name,color,created_at,updated_at)
             VALUES(?1,'Remote project','#000000',?2,?2)",
                params![project_id.to_string(), at],
            )
            .unwrap();
        for (index, id) in ids.iter().enumerate() {
            store.conn.execute(
                "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at,project_id,is_eval)
                 VALUES(?1,'saved','/saved','Completed',?2,?2,?3,?4)",
                params![id.to_string(), at, project_id.to_string(), i64::from(index == 1)],
            ).unwrap();
        }
        store
            .conn
            .busy_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let dir = tempfile::TempDir::new().unwrap();
        let config = Config::from_env();
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                Store::open_in_memory().unwrap(),
                false,
                dir.path().join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&config),
                dir.path().join("sandboxes"),
            )
            .unwrap(),
        );
        let signer = Arc::new(RemoteCursorSigner::new(Uuid::new_v4()));
        let limiter = RemoteReadLimiter::new();
        let mut cursor: Option<CursorV1> = None;
        let mut visible = Vec::new();
        for _ in 0..3 {
            let request: ReadRequestV1 = serde_json::from_value(serde_json::json!({
                "method":"RemoteListSessionsV1",
                "params":{"project_id":project_id,"limit":1,"cursor":cursor}
            }))
            .unwrap();
            let completed = spawn_sessions_read(
                &limiter,
                Arc::clone(&store),
                Arc::clone(&manager),
                Arc::clone(&signer),
                request,
                project_id,
                [8; 32],
                Uuid::new_v4(),
            )
            .unwrap()
            .await
            .unwrap();
            let (mut writer, mut reader) = tokio::io::duplex(4096);
            completed
                .send_json_line(&mut writer, |result| result.expect("sessions owner stage"))
                .await
                .unwrap();
            drop(writer);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            let response: SessionsResponseV1 = serde_json::from_slice(&bytes).unwrap();
            assert!(response.complete);
            assert!(response.items.iter().all(|item| item.attention.incomplete));
            visible.push(response.items.len());
            cursor = response.next_cursor;
        }
        assert_eq!(visible, [1, 0, 1]);
        assert!(cursor.is_none());
        let request: ReadRequestV1 = serde_json::from_value(serde_json::json!({
            "method":"RemoteListSessionsV1",
            "params":{"project_id":project_id,"limit":1}
        }))
        .unwrap();
        assert!(matches!(
            spawn_sessions_read(
                &limiter,
                store,
                manager,
                signer,
                request,
                Uuid::new_v4(),
                [8; 32],
                Uuid::new_v4(),
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn project_owner_preserves_filtered_empty_cursor_and_trusted_scope() {
        let store = Store::open_in_memory().unwrap();
        let ids = [
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        ];
        for id in [ids[0], ids[2]] {
            store.conn.execute(
                "INSERT INTO projects(id,name,color,created_at,updated_at) VALUES(?1,?2,'#000000',?3,?3)",
                params![id.to_string(), format!("visible-{id}"), Utc::now().to_rfc3339()],
            ).unwrap();
        }
        store
            .conn
            .busy_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let signer = Arc::new(RemoteCursorSigner::new(Uuid::new_v4()));
        let limiter = RemoteReadLimiter::new();
        let epoch = Uuid::new_v4();
        let mut cursor: Option<CursorV1> = None;
        let mut counts = Vec::new();
        for _ in 0..3 {
            let request: ReadRequestV1 = serde_json::from_value(serde_json::json!({
                "method":"RemoteListProjectsV1",
                "params":{"project_ids":ids,"limit":1,"cursor":cursor}
            }))
            .unwrap();
            let completed = spawn_projects_read(
                &limiter,
                Arc::clone(&store),
                Arc::clone(&signer),
                request,
                ids.to_vec(),
                [9; 32],
                epoch,
            )
            .unwrap()
            .await
            .unwrap();
            let (mut writer, mut reader) = tokio::io::duplex(4096);
            completed
                .send_json_line(&mut writer, |result| result.unwrap())
                .await
                .unwrap();
            drop(writer);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            let response: ProjectsResponseV1 = serde_json::from_slice(&bytes).unwrap();
            counts.push(response.items.len());
            cursor = response.next_cursor;
        }
        assert_eq!(counts, [1, 0, 1]);
        assert!(cursor.is_none());
        let request: ReadRequestV1 = serde_json::from_value(serde_json::json!({
            "method":"RemoteListProjectsV1",
            "params":{"project_ids":ids,"limit":1}
        }))
        .unwrap();
        assert!(matches!(
            spawn_projects_read(
                &limiter,
                store,
                signer,
                request,
                vec![ids[0]],
                [9; 32],
                epoch
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn history_owner_keeps_lossless_tied_keys_across_signed_pages() {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let at = Utc::now().to_rfc3339();
        store.conn.execute(
            "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at,project_id,is_eval)
             VALUES(?1,'question','/saved','Running',?2,?2,?3,0)",
            params![session.to_string(), at, project.to_string()],
        ).unwrap();
        let base = 9_007_199_254_740_992_i64;
        for offset in 1..=3_i64 {
            store.conn.execute(
                "INSERT INTO conversation_events(id,session_id,sequence,event_type,content,created_at)
                 VALUES(?1,?2,7,'message','ok',?3)",
                params![base + offset, session.to_string(), at],
            ).unwrap();
        }
        store
            .conn
            .busy_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let signer = Arc::new(RemoteCursorSigner::new(Uuid::new_v4()));
        let limiter = RemoteReadLimiter::new();
        let epoch = Uuid::new_v4();
        let mut cursor: Option<CursorV1> = None;
        let mut pages = Vec::new();
        for _ in 0..2 {
            let request: ReadRequestV1 = serde_json::from_value(serde_json::json!({
                "method":"RemoteGetHistoryPageV1",
                "params":{"project_id":project,"session_id":session,"window":{"kind":"latest"},"limit":2,"cursor":cursor}
            })).unwrap();
            let completed = spawn_history_read(
                &limiter,
                Arc::clone(&store),
                &signer,
                &request,
                project,
                [6; 32],
                epoch,
            )
            .unwrap()
            .await
            .unwrap();
            let (mut writer, mut reader) = tokio::io::duplex(4096);
            completed
                .send_json_line(&mut writer, |result| result.unwrap())
                .await
                .unwrap();
            drop(writer);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            let response: HistoryResponseV1 = serde_json::from_slice(&bytes).unwrap();
            pages.push(
                response
                    .items
                    .iter()
                    .map(|row| row.id.get())
                    .collect::<Vec<_>>(),
            );
            cursor = response.next_cursor;
        }
        assert_eq!(pages, [vec![base + 2, base + 3], vec![base + 1]]);
        assert!(cursor.is_none());
        let request: ReadRequestV1 = serde_json::from_value(serde_json::json!({
            "method":"RemoteGetHistoryPageV1",
            "params":{"project_id":project,"session_id":session,"window":{"kind":"latest"},"limit":2}
        })).unwrap();
        assert!(matches!(
            spawn_history_read(
                &limiter,
                store,
                &signer,
                &request,
                Uuid::new_v4(),
                [6; 32],
                epoch
            ),
            Err(ReadError::InvalidSource)
        ));
    }
}
