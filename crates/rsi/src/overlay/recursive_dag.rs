//! Recursive DAG browser input and loading.

use std::collections::HashSet;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::rpc::{
    GetRecursiveExecutionArtifactParams, GetRecursiveLiveAttemptArtifactsParams,
    GetRecursiveLiveOutputValidationResultParams, GetRecursiveLiveRecoveryStatusParams,
    GetRecursiveRecoveryStatusParams, ListRecursiveCancellationRequestsParams,
    ListRecursiveExecutionArtifactSummariesParams, ListRecursiveLiveAttemptsParams,
    ListRecursiveLiveOutputValidationResultsParams, ListRecursiveLiveValidationIssuesParams,
    ListRecursiveSchedulerRunEventsParams, ListRecursiveSchedulerRunsParams,
    ListRecursiveTaskGraphsParams, ListStaleRecursiveLiveAttemptHeartbeatsParams,
    PreviewRecursiveExecutionArtifactParams,
};
use rsi_common::{
    RecursiveCancellationRequestStatus, RecursiveCancellationRequestSummary, RecursiveGraphStatus,
    RecursiveLiveAttemptArtifactReadback, RecursiveLiveAttemptId, RecursiveLiveOutputValidationId,
    RecursiveReadbackWarning, RecursiveRecoveryPassSummary, RecursiveSchedulerRunDetail,
    RecursiveSchedulerRunId, RecursiveSchedulerRunStatus, RecursiveTaskGraphDetail,
    RecursiveTaskGraphId,
};

use crate::app::App;
use crate::types::{
    OverlayState, RECURSIVE_DAG_ARTIFACT_LIMIT, RECURSIVE_DAG_ARTIFACT_MAX_LOADED,
    RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT, RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_BYTES,
    RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_LINES, RECURSIVE_DAG_EVENT_LIMIT, RECURSIVE_DAG_LIVE_LIMIT,
    RECURSIVE_DAG_RUN_LIMIT, RECURSIVE_DAG_TASK_LIMIT, RECURSIVE_DAG_VALIDATION_ISSUE_LIMIT,
    RecursiveDagArtifactDetailState, RecursiveDagArtifactNavDirection,
    RecursiveDagArtifactPageAction, RecursiveDagArtifactPreviewState,
    RecursiveDagArtifactSummaryPageState, RecursiveDagAsyncResult, RecursiveDagBrowserState,
    RecursiveDagControlKind, RecursiveDagControlState, RecursiveDagFakeRunState,
    RecursiveDagInspectorLoadStatus, RecursiveDagInspectorRowKind, RecursiveDagInspectorView,
    RecursiveDagLiveAttemptArtifactsState, RecursiveDagLiveRunState, RecursiveDagLoadStatus,
    RecursiveDagRecoveryInputField, RecursiveDagSelectedGraphData,
    RecursiveDagValidationDetailState, parse_recursive_dag_fake_max_steps,
    parse_recursive_dag_live_max_steps, parse_recursive_dag_recovery_budget,
    validate_recursive_dag_cancellation_reason, validate_recursive_dag_requested_by,
};

const RECURSIVE_DAG_CONTROL_REQUESTED_BY: &str = crate::types::RECURSIVE_DAG_CONTROL_REQUESTED_BY;

pub fn open_recursive_dag_browser(app: &mut App) {
    start_recursive_dag_load(app, None);
}

pub fn handle_recursive_dag_key(app: &mut App, key: KeyEvent) {
    if handle_control_input_key(app, key) {
        app.mark_dirty();
        return;
    }

    if handle_live_run_input_key(app, key) {
        app.mark_dirty();
        return;
    }

    if handle_fake_run_input_key(app, key) {
        app.mark_dirty();
        return;
    }

    if handle_artifact_nav_prefix_key(app, key) {
        app.mark_dirty();
        return;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            if close_recursive_dag_inspector(app) {
                app.mark_dirty();
                return;
            }
            app.overlay = OverlayState::None;
            app.recursive_dag_rx = None;
        }
        KeyCode::Char('r') => {
            let graph_id = current_selected_graph_id(app);
            start_recursive_dag_load(app, graph_id);
        }
        KeyCode::Char('R') => {
            open_fake_run_max_steps_input(app);
        }
        KeyCode::Char('L') => {
            open_live_run_max_steps_input(app);
        }
        KeyCode::Char('C') => {
            open_cancellation_prefix(app);
        }
        KeyCode::Char('c') => {
            open_recovery_budget_input(app);
        }
        KeyCode::Char('!') => {
            reveal_control_details(app);
        }
        KeyCode::Enter => {
            handle_recursive_dag_enter(app);
        }
        KeyCode::Char('p') => {
            set_recursive_dag_inspector_view(app, RecursiveDagInspectorView::Preview);
        }
        KeyCode::Char('m') => {
            set_recursive_dag_inspector_view(app, RecursiveDagInspectorView::Metadata);
        }
        KeyCode::Char('t') => {
            set_recursive_dag_inspector_view(app, RecursiveDagInspectorView::Test);
        }
        KeyCode::Char('d') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            set_recursive_dag_inspector_view(app, RecursiveDagInspectorView::Diff);
        }
        KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => {
            if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
                if state.inspector.open && matches!(key.code, KeyCode::Char('l')) {
                    state.inspector.view = RecursiveDagInspectorView::Links;
                    state.message = Some("artifact inspector links view".to_string());
                } else {
                    state.panel = state.panel.next();
                    state.scroll_offset = 0;
                    state.inspector.artifact_nav_prefix = None;
                }
            }
        }
        KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
            if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
                state.panel = state.panel.previous();
                state.scroll_offset = 0;
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
                state.move_selected(1);
                state.inspector.artifact_nav_prefix = None;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
                state.move_selected(-1);
                state.inspector.artifact_nav_prefix = None;
            }
        }
        KeyCode::Char('g') => {
            if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
                state.jump_selected_top();
                state.scroll_offset = 0;
            }
        }
        KeyCode::Char('G') => {
            if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
                state.jump_selected_bottom();
            }
        }
        KeyCode::Char(']') => {
            open_artifact_nav_prefix(app, RecursiveDagArtifactNavDirection::Next);
        }
        KeyCode::Char('[') => {
            open_artifact_nav_prefix(app, RecursiveDagArtifactNavDirection::Previous);
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
                state.scroll_offset = state.scroll_offset.saturating_add(10);
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
                state.scroll_offset = state.scroll_offset.saturating_sub(10);
            }
        }
        _ => {}
    }
    app.mark_dirty();
}

fn handle_recursive_dag_enter(app: &mut App) {
    let panel = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state.panel,
        _ => return,
    };

    match panel {
        crate::types::RecursiveDagPanel::Graphs => {
            let graph_id = current_selected_graph_id(app);
            start_recursive_dag_load(app, graph_id);
        }
        crate::types::RecursiveDagPanel::Live => {
            start_selected_live_attempt_artifact_load(app);
        }
        crate::types::RecursiveDagPanel::Artifacts => {
            open_selected_recursive_dag_inspector(app);
        }
        _ => {}
    }
}

fn close_recursive_dag_inspector(app: &mut App) -> bool {
    let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay else {
        return false;
    };
    if !state.inspector.open {
        return false;
    }
    state.inspector.close();
    state.message = Some("artifact inspector closed".to_string());
    app.recursive_dag_cache = Some(state.clone());
    true
}

pub(crate) fn set_recursive_dag_inspector_view(app: &mut App, view: RecursiveDagInspectorView) {
    if !active_recursive_browser(app).is_some_and(|state| state.inspector.open) {
        return;
    }
    update_control_input(app, |state| {
        state.inspector.view = view;
        state.inspector.artifact_nav_prefix = None;
        state.message = Some(match view {
            RecursiveDagInspectorView::Preview => {
                "artifact inspector preview view".to_string()
            }
            RecursiveDagInspectorView::Test => {
                "typed test detail unavailable: typed recursive test readback is not exposed"
                    .to_string()
            }
            RecursiveDagInspectorView::Diff => {
                "typed diff detail unavailable: typed recursive diff readback is not exposed"
                    .to_string()
            }
            RecursiveDagInspectorView::Report => {
                "typed scheduler report inspector unavailable: report detail readback is not exposed"
                    .to_string()
            }
            _ => format!("artifact inspector {} view", view.label()),
        });
    });
    if view == RecursiveDagInspectorView::Preview {
        start_selected_artifact_preview_load(app);
    }
}

fn open_artifact_nav_prefix(app: &mut App, direction: RecursiveDagArtifactNavDirection) {
    if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
        if state.panel != crate::types::RecursiveDagPanel::Artifacts && !state.inspector.open {
            state.message = Some(
                "artifact row navigation is available inside the artifact inspector only"
                    .to_string(),
            );
            return;
        }
        state.inspector.artifact_nav_prefix = Some(direction);
        state.message = Some(match direction {
            RecursiveDagArtifactNavDirection::Next => {
                "artifact prefix: press a for next artifact row".to_string()
            }
            RecursiveDagArtifactNavDirection::Previous => {
                "artifact prefix: press a for previous artifact row".to_string()
            }
        });
    }
}

fn handle_artifact_nav_prefix_key(app: &mut App, key: KeyEvent) -> bool {
    let direction = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state.inspector.artifact_nav_prefix,
        _ => None,
    };
    let Some(direction) = direction else {
        return false;
    };

    if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
        state.inspector.artifact_nav_prefix = None;
        if matches!(key.code, KeyCode::Char('a')) {
            state.move_artifact_row(direction.delta());
            state.panel = crate::types::RecursiveDagPanel::Artifacts;
            state.message = Some("selected artifact inspector row changed".to_string());
        } else {
            state.message = Some("artifact prefix cancelled; press ]a or [a".to_string());
        }
        app.recursive_dag_cache = Some(state.clone());
    }
    true
}

pub(crate) fn open_selected_recursive_dag_inspector(app: &mut App) {
    let selected =
        active_recursive_browser(app).and_then(RecursiveDagBrowserState::selected_inspector_row);
    let Some(row) = selected else {
        update_control_input(app, |state| {
            state.message =
                Some("no artifact, validation, test, diff, or report row selected".to_string());
        });
        return;
    };

    if let Some(action) = row.artifact_page_action() {
        if action == RecursiveDagArtifactPageAction::LoadMore {
            start_artifact_summary_page_load_more(app);
        } else {
            update_control_input(app, |state| {
                state.message = Some(format!("artifact page control is {}", action.label()));
            });
        }
        return;
    }

    update_control_input(app, |state| {
        state.inspector.open_for(Some(&row));
        state.message = Some(format!(
            "opened {} inspector from {}",
            row.kind.label(),
            row.source.label()
        ));
    });

    if row.kind == RecursiveDagInspectorRowKind::Validation
        && let Some((validation_id, live_attempt_id)) = row.validation_key()
    {
        start_validation_detail_load(app, validation_id, live_attempt_id);
    } else if row.artifact_id().is_some() {
        start_artifact_detail_load(app, row);
    }
}

fn handle_control_input_key(app: &mut App, key: KeyEvent) -> bool {
    let control = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state.control.clone(),
        _ => return false,
    };

    match control {
        RecursiveDagControlState::Idle => false,
        RecursiveDagControlState::CancellationPrefix => {
            handle_cancellation_prefix_key(app, key);
            true
        }
        RecursiveDagControlState::GraphReasonInput {
            graph_id, input, ..
        } => {
            handle_cancellation_reason_key(
                app,
                key,
                RecursiveDagControlKind::GraphCancellation,
                Some(graph_id),
                None,
                input,
            );
            true
        }
        RecursiveDagControlState::RunReasonInput { run_id, input, .. } => {
            handle_cancellation_reason_key(
                app,
                key,
                RecursiveDagControlKind::RunCancellation,
                current_selected_graph_id(app),
                Some(run_id),
                input,
            );
            true
        }
        RecursiveDagControlState::RecoveryBudgetInput { .. } => {
            handle_recovery_budget_key(app, key);
            true
        }
        RecursiveDagControlState::Submitting { kind, .. } => {
            update_control_input(app, |state| {
                state.message = Some(format!(
                    "{} already submitted; waiting for daemon readback refresh",
                    kind.label()
                ));
            });
            true
        }
    }
}

fn handle_cancellation_prefix_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some("recursive DAG cancellation prompt cancelled".to_string());
        }),
        KeyCode::Char('g') => open_graph_cancellation_reason_input(app),
        KeyCode::Char('r') => open_run_cancellation_reason_input(app),
        KeyCode::Char('t') => update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(
                "task cancellation reserved: task-scoped cancellation needs daemon execution semantics"
                    .to_string(),
            );
        }),
        KeyCode::Char('!') => reveal_control_details(app),
        _ => update_control_input(app, |state| {
            state.message = Some(
                "cancel prefix: press g for graph, r for run, t for reserved task".to_string(),
            );
        }),
    }
}

fn handle_cancellation_reason_key(
    app: &mut App,
    key: KeyEvent,
    kind: RecursiveDagControlKind,
    graph_id: Option<RecursiveTaskGraphId>,
    run_id: Option<RecursiveSchedulerRunId>,
    input: String,
) {
    match key.code {
        KeyCode::Esc => update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(format!("{} cancelled before submit", kind.label()));
        }),
        KeyCode::Enter => {
            let reason = match validate_recursive_dag_cancellation_reason(&input) {
                Ok(reason) => reason,
                Err(error) => {
                    set_reason_input_error(app, error);
                    return;
                }
            };
            if let Err(error) =
                validate_recursive_dag_requested_by(RECURSIVE_DAG_CONTROL_REQUESTED_BY)
            {
                set_reason_input_error(app, error);
                return;
            }
            match kind {
                RecursiveDagControlKind::GraphCancellation => {
                    let Some(graph_id) = graph_id else {
                        set_reason_input_error(app, "selected graph is required");
                        return;
                    };
                    start_recursive_dag_graph_cancellation(app, graph_id, reason);
                }
                RecursiveDagControlKind::RunCancellation => {
                    let Some(run_id) = run_id else {
                        set_reason_input_error(app, "selected run is required");
                        return;
                    };
                    start_recursive_dag_run_cancellation(app, run_id, reason);
                }
                RecursiveDagControlKind::RecoveryContinuation => {}
            }
        }
        KeyCode::Backspace => update_control_input(app, |state| {
            match &mut state.control {
                RecursiveDagControlState::GraphReasonInput { input, error, .. }
                | RecursiveDagControlState::RunReasonInput { input, error, .. } => {
                    input.pop();
                    *error = None;
                }
                _ => {}
            }
            state.message = Some(cancellation_reason_prompt(kind).to_string());
        }),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            update_control_input(app, |state| {
                match &mut state.control {
                    RecursiveDagControlState::GraphReasonInput { input, error, .. }
                    | RecursiveDagControlState::RunReasonInput { input, error, .. } => {
                        input.clear();
                        *error = None;
                    }
                    _ => {}
                }
                state.message = Some(cancellation_reason_prompt(kind).to_string());
            });
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            update_control_input(app, |state| {
                match &mut state.control {
                    RecursiveDagControlState::GraphReasonInput { input, error, .. }
                    | RecursiveDagControlState::RunReasonInput { input, error, .. } => {
                        input.push(ch);
                        *error = None;
                    }
                    _ => {}
                }
                state.message = Some(cancellation_reason_prompt(kind).to_string());
            });
        }
        _ => {}
    }
}

fn handle_recovery_budget_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some("recovery continuation cancelled before submit".to_string());
        }),
        KeyCode::Enter => {
            let (max_graphs_input, time_budget_ms_input) = match &app.overlay {
                OverlayState::RecursiveDagBrowser(state) => match &state.control {
                    RecursiveDagControlState::RecoveryBudgetInput {
                        max_graphs_input,
                        time_budget_ms_input,
                        ..
                    } => (max_graphs_input.clone(), time_budget_ms_input.clone()),
                    _ => return,
                },
                _ => return,
            };
            match parse_recursive_dag_recovery_budget(&max_graphs_input, &time_budget_ms_input) {
                Ok((max_graphs, time_budget_ms)) => {
                    start_recursive_dag_recovery_continuation(app, max_graphs, time_budget_ms);
                }
                Err(error) => set_recovery_input_error(app, error),
            }
        }
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
            update_control_input(app, |state| {
                if let RecursiveDagControlState::RecoveryBudgetInput { field, error, .. } =
                    &mut state.control
                {
                    *field = field.next();
                    *error = None;
                    state.message = Some(format!("continue recovery: editing {}", field.label()));
                }
            });
        }
        KeyCode::Backspace => update_control_input(app, |state| {
            if let RecursiveDagControlState::RecoveryBudgetInput {
                max_graphs_input,
                time_budget_ms_input,
                field,
                error,
            } = &mut state.control
            {
                match field {
                    RecursiveDagRecoveryInputField::MaxGraphs => {
                        max_graphs_input.pop();
                    }
                    RecursiveDagRecoveryInputField::TimeBudgetMs => {
                        time_budget_ms_input.pop();
                    }
                }
                *error = None;
                state.message = Some(format!("continue recovery: editing {}", field.label()));
            }
        }),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            update_control_input(app, |state| {
                if let RecursiveDagControlState::RecoveryBudgetInput {
                    max_graphs_input,
                    time_budget_ms_input,
                    field,
                    error,
                } = &mut state.control
                {
                    match field {
                        RecursiveDagRecoveryInputField::MaxGraphs => max_graphs_input.clear(),
                        RecursiveDagRecoveryInputField::TimeBudgetMs => {
                            time_budget_ms_input.clear()
                        }
                    }
                    *error = None;
                    state.message = Some(format!("continue recovery: editing {}", field.label()));
                }
            });
        }
        KeyCode::Char(ch) if ch.is_ascii_digit() => update_control_input(app, |state| {
            if let RecursiveDagControlState::RecoveryBudgetInput {
                max_graphs_input,
                time_budget_ms_input,
                field,
                error,
            } = &mut state.control
            {
                match field {
                    RecursiveDagRecoveryInputField::MaxGraphs => max_graphs_input.push(ch),
                    RecursiveDagRecoveryInputField::TimeBudgetMs => time_budget_ms_input.push(ch),
                }
                *error = None;
                state.message = Some(format!("continue recovery: editing {}", field.label()));
            }
        }),
        KeyCode::Char(_) => set_recovery_input_error(app, "recovery budget accepts digits only"),
        _ => {}
    }
}

fn active_recursive_browser(app: &App) -> Option<&RecursiveDagBrowserState> {
    match &app.overlay {
        OverlayState::RecursiveDagBrowser(state)
        | OverlayState::GraphReview {
            dashboard_state: Some(state),
            ..
        } => Some(state),
        _ => None,
    }
}

fn active_recursive_browser_for_load(app: &App) -> Option<RecursiveDagBrowserState> {
    active_recursive_browser(app)
        .filter(|state| app.recursive_dag_state_matches_current_project(state))
        .cloned()
}

fn update_control_input(app: &mut App, update: impl FnOnce(&mut RecursiveDagBrowserState)) {
    match &mut app.overlay {
        OverlayState::RecursiveDagBrowser(state) => {
            update(state);
            app.recursive_dag_cache = Some(state.clone());
        }
        OverlayState::GraphReview {
            dashboard_state: Some(state),
            ..
        } => {
            update(state);
        }
        _ => {}
    }
}

fn set_reason_input_error(app: &mut App, error_message: &str) {
    update_control_input(app, |state| {
        match &mut state.control {
            RecursiveDagControlState::GraphReasonInput { error, .. }
            | RecursiveDagControlState::RunReasonInput { error, .. } => {
                *error = Some(error_message.to_string());
            }
            _ => {}
        }
        state.message = Some(format!("cancellation blocked: {error_message}"));
    });
}

fn set_recovery_input_error(app: &mut App, error_message: &str) {
    update_control_input(app, |state| {
        if let RecursiveDagControlState::RecoveryBudgetInput { error, .. } = &mut state.control {
            *error = Some(error_message.to_string());
        }
        state.message = Some(format!("recovery continuation blocked: {error_message}"));
    });
}

fn cancellation_reason_prompt(kind: RecursiveDagControlKind) -> &'static str {
    match kind {
        RecursiveDagControlKind::GraphCancellation => {
            "CANCEL GRAPH: type a reason, then Enter to submit or Esc to cancel"
        }
        RecursiveDagControlKind::RunCancellation => {
            "CANCEL RUN: type a reason, then Enter to submit or Esc to cancel"
        }
        RecursiveDagControlKind::RecoveryContinuation => "",
    }
}

#[derive(Debug, Clone, Copy)]
struct RunCancellationTarget {
    run_id: RecursiveSchedulerRunId,
    status: RecursiveSchedulerRunStatus,
}

fn open_cancellation_prefix(app: &mut App) {
    if let Some(message) = cancellation_control_gate_message(app) {
        update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(message);
        });
        return;
    }

    update_control_input(app, |state| {
        state.fake_run = RecursiveDagFakeRunState::Idle;
        state.control = RecursiveDagControlState::CancellationPrefix;
        state.message =
            Some("cancel prefix: press g for graph, r for run, t for reserved task".to_string());
    });
}

fn open_graph_cancellation_reason_input(app: &mut App) {
    if let Some(message) = cancellation_control_gate_message(app) {
        update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(message);
        });
        return;
    }

    let target = match selected_graph_cancellation_target(app) {
        Ok(graph_id) => graph_id,
        Err(message) => {
            update_control_input(app, |state| {
                state.control = RecursiveDagControlState::Idle;
                state.message = Some(message);
            });
            return;
        }
    };

    update_control_input(app, |state| {
        state.fake_run = RecursiveDagFakeRunState::Idle;
        state.control = RecursiveDagControlState::GraphReasonInput {
            graph_id: target,
            input: String::new(),
            error: None,
        };
        state.message = Some(
            cancellation_reason_prompt(RecursiveDagControlKind::GraphCancellation).to_string(),
        );
    });
}

fn open_run_cancellation_reason_input(app: &mut App) {
    if let Some(message) = cancellation_control_gate_message(app) {
        update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(message);
        });
        return;
    }

    let target = match selected_run_cancellation_target(app) {
        Ok(target) => target,
        Err(message) => {
            update_control_input(app, |state| {
                state.control = RecursiveDagControlState::Idle;
                state.message = Some(message);
            });
            return;
        }
    };

    if !scheduler_run_status_accepts_cancellation(target.status) {
        update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(format!(
                "run cancellation unavailable: selected run {} is terminal: {:?}",
                target.run_id, target.status
            ));
        });
        return;
    }

    update_control_input(app, |state| {
        state.fake_run = RecursiveDagFakeRunState::Idle;
        state.control = RecursiveDagControlState::RunReasonInput {
            run_id: target.run_id,
            input: String::new(),
            error: None,
        };
        state.message =
            Some(cancellation_reason_prompt(RecursiveDagControlKind::RunCancellation).to_string());
    });
}

fn open_recovery_budget_input(app: &mut App) {
    if let Some(message) = recovery_control_gate_message(app) {
        update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(message);
        });
        return;
    }

    update_control_input(app, |state| {
        state.fake_run = RecursiveDagFakeRunState::Idle;
        state.control = RecursiveDagControlState::RecoveryBudgetInput {
            max_graphs_input: String::new(),
            time_budget_ms_input: String::new(),
            field: RecursiveDagRecoveryInputField::MaxGraphs,
            error: None,
        };
        state.message = Some(
            "continue recovery: editing max_graphs; Tab switches optional time_budget_ms"
                .to_string(),
        );
    });
}

fn reveal_control_details(app: &mut App) {
    update_control_input(app, |state| {
        let cancellation_gate = if state
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.recursive_dag_cancellation_control)
        {
            "recursive_dag_cancellation_control=true"
        } else {
            "recursive_dag_cancellation_control=false"
        };
        let recovery_gate = if state
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.recursive_dag_recovery_control)
        {
            "recursive_dag_recovery_control=true"
        } else {
            "recursive_dag_recovery_control=false"
        };
        state.message = Some(format!(
            "controls: {cancellation_gate}; {recovery_gate}; C g/C r require typed reason; C t reserved; c requires explicit max_graphs; L requires live scheduler gate; topology live and background loops disabled"
        ));
    });
}

fn cancellation_control_gate_message(app: &App) -> Option<String> {
    if !app.poll.connected {
        return Some(
            "cancellation unavailable: daemon is not connected; start rsid and retry".to_string(),
        );
    }
    if app.recursive_dag_rx.is_some() {
        return Some("cancellation blocked: recursive DAG operation already in flight".to_string());
    }
    let state = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state,
        _ => return Some("cancellation unavailable: recursive DAG browser is closed".to_string()),
    };
    if !matches!(state.load_status, RecursiveDagLoadStatus::Ready) {
        return Some("cancellation unavailable until recursive DAG browser is ready".to_string());
    }
    if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_cancellation_control)
    {
        return Some(
            "cancellation unavailable: recursive_dag_cancellation_control=false".to_string(),
        );
    }
    selected_graph_cancellation_target(app).err()
}

fn recovery_control_gate_message(app: &App) -> Option<String> {
    if !app.poll.connected {
        return Some(
            "recovery continuation unavailable: daemon is not connected; start rsid and retry"
                .to_string(),
        );
    }
    if app.recursive_dag_rx.is_some() {
        return Some(
            "recovery continuation blocked: recursive DAG operation already in flight".to_string(),
        );
    }
    let state = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state,
        _ => {
            return Some(
                "recovery continuation unavailable: recursive DAG browser is closed".to_string(),
            );
        }
    };
    if !matches!(state.load_status, RecursiveDagLoadStatus::Ready) {
        return Some(
            "recovery continuation unavailable until recursive DAG browser is ready".to_string(),
        );
    }
    if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_recovery_control)
    {
        return Some(
            "recovery continuation unavailable: recursive_dag_recovery_control=false".to_string(),
        );
    }
    if !state
        .selected_detail()
        .is_some_and(deferred_recovery_work_visible)
    {
        return Some(
            "recovery continuation unavailable: no deferred recovery work is visible".to_string(),
        );
    }
    None
}

fn selected_graph_cancellation_target(app: &App) -> Result<RecursiveTaskGraphId, String> {
    let state = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state,
        _ => {
            return Err(
                "graph cancellation unavailable: recursive DAG browser is closed".to_string(),
            );
        }
    };
    let graph_id = state
        .selected_graph_id()
        .ok_or_else(|| "graph cancellation unavailable: no graph selected".to_string())?;
    let graph_summary = state.selected_graph_summary().ok_or_else(|| {
        "graph cancellation unavailable: selected graph is unavailable".to_string()
    })?;
    let graph_status = state
        .selected_detail()
        .map(|detail| detail.graph.graph.status)
        .unwrap_or(graph_summary.status);
    if graph_status != RecursiveGraphStatus::Active {
        return Err(format!(
            "graph cancellation unavailable: selected graph is not cancellable: {graph_status:?}"
        ));
    }
    let malformed_or_quarantined = state.selected_detail().map_or_else(
        || graph_summary.malformed_reason.is_some() || graph_summary.quarantined_at.is_some(),
        |detail| {
            detail.graph.graph.malformed_reason.is_some()
                || detail.graph.graph.quarantined_at.is_some()
        },
    );
    if malformed_or_quarantined {
        return Err(
            "graph cancellation unavailable: selected graph is malformed or quarantined"
                .to_string(),
        );
    }
    Ok(graph_id)
}

fn selected_run_cancellation_target(app: &App) -> Result<RunCancellationTarget, String> {
    selected_graph_cancellation_target(app)?;
    let state = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state,
        _ => {
            return Err(
                "run cancellation unavailable: recursive DAG browser is closed".to_string(),
            );
        }
    };
    let detail = state.selected_detail().ok_or_else(|| {
        "run cancellation unavailable: selected graph is not hydrated".to_string()
    })?;

    if state.panel == crate::types::RecursiveDagPanel::Runs {
        let run = detail
            .scheduler_runs
            .get(state.selected_run)
            .ok_or_else(|| {
                "run cancellation unavailable: no scheduler run selected in runs panel".to_string()
            })?;
        return Ok(RunCancellationTarget {
            run_id: run.id,
            status: run.status,
        });
    }

    if let Some(run) = &detail.selected_run_detail {
        return Ok(RunCancellationTarget {
            run_id: run.run.id,
            status: run.run.status,
        });
    }

    if let Some(run) = detail
        .operational_status
        .as_ref()
        .and_then(|status| status.active_run.as_ref())
    {
        return Ok(RunCancellationTarget {
            run_id: run.id,
            status: run.status,
        });
    }

    Err("run cancellation unavailable: no scheduler run selected".to_string())
}

fn scheduler_run_status_accepts_cancellation(status: RecursiveSchedulerRunStatus) -> bool {
    matches!(
        status,
        RecursiveSchedulerRunStatus::Running | RecursiveSchedulerRunStatus::Cancelling
    )
}

fn deferred_recovery_work_visible(detail: &RecursiveDagSelectedGraphData) -> bool {
    detail
        .recovery_status
        .as_ref()
        .is_some_and(|status| status.deferred_graph_count > 0)
        || !detail.deferred_recovery_rows().is_empty()
}

fn start_recursive_dag_graph_cancellation(
    app: &mut App,
    graph_id: RecursiveTaskGraphId,
    reason: String,
) {
    if let Some(message) = cancellation_control_gate_message(app) {
        update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(message);
        });
        return;
    }

    let project_id = app.current_project_id;
    let previous_state = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state.clone(),
        _ => return,
    };

    update_control_input(app, |state| {
        state.control = RecursiveDagControlState::Submitting {
            kind: RecursiveDagControlKind::GraphCancellation,
            target: graph_id.to_string(),
        };
        state.message = Some(format!(
            "graph cancellation submitted locally: graph={} requested_by={}",
            graph_id, RECURSIVE_DAG_CONTROL_REQUESTED_BY
        ));
    });

    let socket_path = app.client.socket_path().to_path_buf();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = request_recursive_graph_cancellation_and_refresh(
            socket_path,
            project_id,
            graph_id,
            reason,
            previous_state,
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
}

fn start_recursive_dag_run_cancellation(
    app: &mut App,
    run_id: RecursiveSchedulerRunId,
    reason: String,
) {
    if let Some(message) = cancellation_control_gate_message(app) {
        update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(message);
        });
        return;
    }

    let project_id = app.current_project_id;
    let selected_graph_id = current_selected_graph_id(app);
    let previous_state = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state.clone(),
        _ => return,
    };

    update_control_input(app, |state| {
        state.control = RecursiveDagControlState::Submitting {
            kind: RecursiveDagControlKind::RunCancellation,
            target: run_id.to_string(),
        };
        state.message = Some(format!(
            "run cancellation submitted locally: run={} requested_by={}",
            run_id, RECURSIVE_DAG_CONTROL_REQUESTED_BY
        ));
    });

    let socket_path = app.client.socket_path().to_path_buf();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = request_recursive_run_cancellation_and_refresh(
            socket_path,
            project_id,
            selected_graph_id,
            run_id,
            reason,
            previous_state,
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
}

fn start_recursive_dag_recovery_continuation(
    app: &mut App,
    max_graphs: u32,
    time_budget_ms: Option<u64>,
) {
    if let Some(message) = recovery_control_gate_message(app) {
        update_control_input(app, |state| {
            state.control = RecursiveDagControlState::Idle;
            state.message = Some(message);
        });
        return;
    }

    let project_id = app.current_project_id;
    let selected_graph_id = current_selected_graph_id(app);
    let previous_state = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => state.clone(),
        _ => return,
    };

    update_control_input(app, |state| {
        state.control = RecursiveDagControlState::Submitting {
            kind: RecursiveDagControlKind::RecoveryContinuation,
            target: format!("max_graphs={max_graphs}"),
        };
        state.message = Some(format!(
            "recovery continuation submitted locally: max_graphs={} time_budget_ms={}",
            max_graphs,
            time_budget_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string())
        ));
    });

    let socket_path = app.client.socket_path().to_path_buf();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = continue_recursive_recovery_and_refresh(
            socket_path,
            project_id,
            selected_graph_id,
            max_graphs,
            time_budget_ms,
            previous_state,
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
}

fn handle_fake_run_input_key(app: &mut App, key: KeyEvent) -> bool {
    let input = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => match &state.fake_run {
            RecursiveDagFakeRunState::MaxStepsInput { input, .. } => input.clone(),
            _ => return false,
        },
        _ => return false,
    };

    match key.code {
        KeyCode::Esc => {
            update_fake_run_input(app, |state| {
                state.fake_run = RecursiveDagFakeRunState::Idle;
                state.message = Some("FAKE scheduler run cancelled before submit".to_string());
            });
        }
        KeyCode::Enter => match parse_recursive_dag_fake_max_steps(&input) {
            Ok(max_steps) => match current_selected_graph_id(app) {
                Some(graph_id) => start_recursive_dag_fake_run(app, graph_id, max_steps),
                None => set_fake_run_input_error(app, "selected graph is required"),
            },
            Err(error) => set_fake_run_input_error(app, error),
        },
        KeyCode::Backspace => update_fake_run_input(app, |state| {
            if let RecursiveDagFakeRunState::MaxStepsInput { input, error } = &mut state.fake_run {
                input.pop();
                *error = None;
                state.message = Some("FAKE scheduler requires explicit max_steps".to_string());
            }
        }),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            update_fake_run_input(app, |state| {
                if let RecursiveDagFakeRunState::MaxStepsInput { input, error } =
                    &mut state.fake_run
                {
                    input.clear();
                    *error = None;
                    state.message = Some("FAKE scheduler requires explicit max_steps".to_string());
                }
            });
        }
        KeyCode::Char(ch) if ch.is_ascii_digit() => update_fake_run_input(app, |state| {
            if let RecursiveDagFakeRunState::MaxStepsInput { input, error } = &mut state.fake_run {
                input.push(ch);
                *error = None;
                state.message = Some("FAKE scheduler requires explicit max_steps".to_string());
            }
        }),
        KeyCode::Char(_) => {
            set_fake_run_input_error(app, "max_steps accepts digits only");
        }
        _ => {}
    }

    true
}

fn update_fake_run_input(app: &mut App, update: impl FnOnce(&mut RecursiveDagBrowserState)) {
    if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
        update(state);
        app.recursive_dag_cache = Some(state.clone());
    }
}

fn set_fake_run_input_error(app: &mut App, error_message: &str) {
    update_fake_run_input(app, |state| {
        if let RecursiveDagFakeRunState::MaxStepsInput { error, .. } = &mut state.fake_run {
            *error = Some(error_message.to_string());
        }
        state.message = Some(format!("FAKE scheduler blocked: {error_message}"));
    });
}

fn open_fake_run_max_steps_input(app: &mut App) {
    if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
        if !app.poll.connected {
            state.message = Some(
                "FAKE scheduler unavailable: daemon is not connected; start rsid and retry"
                    .to_string(),
            );
            state.fake_run = RecursiveDagFakeRunState::Idle;
            app.recursive_dag_cache = Some(state.clone());
            return;
        }

        if !matches!(state.load_status, RecursiveDagLoadStatus::Ready) {
            state.message =
                Some("FAKE scheduler unavailable until recursive DAG browser is ready".to_string());
            state.fake_run = RecursiveDagFakeRunState::Idle;
            app.recursive_dag_cache = Some(state.clone());
            return;
        }

        if !state
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.recursive_dag_scheduler_control)
        {
            state.message = Some(
                "FAKE scheduler unavailable: recursive_dag_scheduler_control capability disabled"
                    .to_string(),
            );
            state.fake_run = RecursiveDagFakeRunState::Idle;
            app.recursive_dag_cache = Some(state.clone());
            return;
        }

        if state.selected_graph_id().is_none() {
            state.message = Some("FAKE scheduler unavailable: no graph selected".to_string());
            state.fake_run = RecursiveDagFakeRunState::Idle;
            app.recursive_dag_cache = Some(state.clone());
            return;
        }

        state.fake_run = RecursiveDagFakeRunState::MaxStepsInput {
            input: String::new(),
            error: None,
        };
        state.control = RecursiveDagControlState::Idle;
        state.message = Some("FAKE scheduler requires explicit max_steps".to_string());
        app.recursive_dag_cache = Some(state.clone());
    }
}

fn start_recursive_dag_fake_run(app: &mut App, graph_id: RecursiveTaskGraphId, max_steps: u32) {
    if max_steps == 0 {
        set_fake_run_input_error(app, "max_steps must be greater than zero");
        return;
    }

    if !app.poll.connected {
        update_fake_run_input(app, |state| {
            state.fake_run = RecursiveDagFakeRunState::Idle;
            state.message = Some(
                "FAKE scheduler unavailable: daemon is not connected; start rsid and retry"
                    .to_string(),
            );
        });
        return;
    }

    if app.recursive_dag_rx.is_some() {
        update_fake_run_input(app, |state| {
            state.message = Some(
                "FAKE scheduler blocked: recursive DAG operation already in flight".to_string(),
            );
        });
        return;
    }

    let project_id = app.current_project_id;
    if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
        if !state
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.recursive_dag_scheduler_control)
        {
            state.fake_run = RecursiveDagFakeRunState::Idle;
            state.message = Some(
                "FAKE scheduler unavailable: recursive_dag_scheduler_control capability disabled"
                    .to_string(),
            );
            app.recursive_dag_cache = Some(state.clone());
            return;
        }
        state.fake_run = RecursiveDagFakeRunState::Running {
            graph_id,
            max_steps,
        };
        state.message = Some(format!(
            "FAKE scheduler running with explicit max_steps={max_steps}"
        ));
        app.recursive_dag_cache = Some(state.clone());
    }

    let socket_path = app.client.socket_path().to_path_buf();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = run_recursive_dag_fake_scheduler_and_refresh(
            socket_path,
            project_id,
            graph_id,
            max_steps,
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
}

fn handle_live_run_input_key(app: &mut App, key: KeyEvent) -> bool {
    let input = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state) => match &state.live_run {
            RecursiveDagLiveRunState::MaxStepsInput { input, .. } => input.clone(),
            _ => return false,
        },
        _ => return false,
    };

    match key.code {
        KeyCode::Esc => {
            update_live_run_input(app, |state| {
                state.live_run = RecursiveDagLiveRunState::Idle;
                state.message = Some("LIVE scheduler run cancelled before submit".to_string());
            });
        }
        KeyCode::Enter => match parse_recursive_dag_live_max_steps(&input) {
            Ok(max_steps) => match current_selected_graph_id(app) {
                Some(graph_id) => start_recursive_dag_live_run(app, graph_id, max_steps),
                None => set_live_run_input_error(app, "selected graph is required"),
            },
            Err(error) => set_live_run_input_error(app, error),
        },
        KeyCode::Backspace => update_live_run_input(app, |state| {
            if let RecursiveDagLiveRunState::MaxStepsInput { input, error } = &mut state.live_run {
                input.pop();
                *error = None;
                state.message = Some("LIVE scheduler requires explicit max_steps".to_string());
            }
        }),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            update_live_run_input(app, |state| {
                if let RecursiveDagLiveRunState::MaxStepsInput { input, error } =
                    &mut state.live_run
                {
                    input.clear();
                    *error = None;
                    state.message = Some("LIVE scheduler requires explicit max_steps".to_string());
                }
            });
        }
        KeyCode::Char(ch) if ch.is_ascii_digit() => update_live_run_input(app, |state| {
            if let RecursiveDagLiveRunState::MaxStepsInput { input, error } = &mut state.live_run {
                input.push(ch);
                *error = None;
                state.message = Some("LIVE scheduler requires explicit max_steps".to_string());
            }
        }),
        KeyCode::Char(_) => {
            set_live_run_input_error(app, "max_steps accepts digits only");
        }
        _ => {}
    }

    true
}

fn update_live_run_input(app: &mut App, update: impl FnOnce(&mut RecursiveDagBrowserState)) {
    if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
        update(state);
        app.recursive_dag_cache = Some(state.clone());
    }
}

fn set_live_run_input_error(app: &mut App, error_message: &str) {
    update_live_run_input(app, |state| {
        if let RecursiveDagLiveRunState::MaxStepsInput { error, .. } = &mut state.live_run {
            *error = Some(error_message.to_string());
        }
        state.message = Some(format!("LIVE scheduler blocked: {error_message}"));
    });
}

fn open_live_run_max_steps_input(app: &mut App) {
    if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
        if !app.poll.connected {
            state.message = Some(
                "LIVE scheduler unavailable: daemon is not connected; start rsid and retry"
                    .to_string(),
            );
            state.live_run = RecursiveDagLiveRunState::Idle;
            app.recursive_dag_cache = Some(state.clone());
            return;
        }

        if !matches!(state.load_status, RecursiveDagLoadStatus::Ready) {
            state.message =
                Some("LIVE scheduler unavailable until recursive DAG browser is ready".to_string());
            state.live_run = RecursiveDagLiveRunState::Idle;
            app.recursive_dag_cache = Some(state.clone());
            return;
        }

        if !state
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.recursive_dag_live_scheduler_control)
        {
            state.message = Some(
                "LIVE scheduler unavailable: recursive_dag_live_scheduler_control capability disabled"
                    .to_string(),
            );
            state.live_run = RecursiveDagLiveRunState::Idle;
            app.recursive_dag_cache = Some(state.clone());
            return;
        }

        if state.selected_graph_id().is_none() {
            state.message = Some("LIVE scheduler unavailable: no graph selected".to_string());
            state.live_run = RecursiveDagLiveRunState::Idle;
            app.recursive_dag_cache = Some(state.clone());
            return;
        }

        state.live_run = RecursiveDagLiveRunState::MaxStepsInput {
            input: String::new(),
            error: None,
        };
        state.fake_run = RecursiveDagFakeRunState::Idle;
        state.control = RecursiveDagControlState::Idle;
        state.message = Some("LIVE scheduler requires explicit max_steps".to_string());
        app.recursive_dag_cache = Some(state.clone());
    }
}

fn start_recursive_dag_live_run(app: &mut App, graph_id: RecursiveTaskGraphId, max_steps: u32) {
    if max_steps == 0 {
        set_live_run_input_error(app, "max_steps must be greater than zero");
        return;
    }

    if !app.poll.connected {
        update_live_run_input(app, |state| {
            state.live_run = RecursiveDagLiveRunState::Idle;
            state.message = Some(
                "LIVE scheduler unavailable: daemon is not connected; start rsid and retry"
                    .to_string(),
            );
        });
        return;
    }

    if app.recursive_dag_rx.is_some() {
        update_live_run_input(app, |state| {
            state.message = Some(
                "LIVE scheduler blocked: recursive DAG operation already in flight".to_string(),
            );
        });
        return;
    }

    let project_id = app.current_project_id;
    if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
        if !state
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.recursive_dag_live_scheduler_control)
        {
            state.live_run = RecursiveDagLiveRunState::Idle;
            state.message = Some(
                "LIVE scheduler unavailable: recursive_dag_live_scheduler_control capability disabled"
                    .to_string(),
            );
            app.recursive_dag_cache = Some(state.clone());
            return;
        }
        state.live_run = RecursiveDagLiveRunState::Running {
            graph_id,
            max_steps,
        };
        state.message = Some(format!(
            "LIVE scheduler running with explicit max_steps={max_steps}"
        ));
        app.recursive_dag_cache = Some(state.clone());
    }

    let socket_path = app.client.socket_path().to_path_buf();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = run_recursive_dag_live_scheduler_and_refresh(
            socket_path,
            project_id,
            graph_id,
            max_steps,
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
}

pub(crate) fn start_dashboard_recursive_dag_load(app: &mut App, recursive_graph_id: uuid::Uuid) {
    let project_id = app.current_project_id;
    if app.recursive_dag_rx.is_some() {
        return;
    }
    if !app.poll.connected {
        return;
    }
    let socket_path = app.client.socket_path().to_path_buf();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = load_recursive_dag_browser(
            socket_path,
            project_id,
            Some(RecursiveTaskGraphId(recursive_graph_id)),
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
    app.mark_dirty();
}

pub fn apply_recursive_dag_load_result(
    app: &mut App,
    result: Result<RecursiveDagAsyncResult, String>,
) {
    let result = match result {
        Ok(RecursiveDagAsyncResult::Browser(state)) => Ok(state),
        Ok(RecursiveDagAsyncResult::ArtifactSummaryPage { graph_id, state }) => {
            apply_recursive_dag_artifact_page_result(app, graph_id, state);
            return;
        }
        Err(err) => Err(err),
    };

    let mut new_state = match result {
        Ok(state) => state,
        Err(err) => RecursiveDagBrowserState::error(app.current_project_id, err),
    };

    if !app.recursive_dag_state_matches_current_project(&new_state) {
        if matches!(app.overlay, OverlayState::RecursiveDagBrowser(_)) {
            let state = RecursiveDagBrowserState::error(
                app.current_project_id,
                "recursive DAG result ignored because project scope changed; refresh :dag",
            );
            app.recursive_dag_cache = Some(state.clone());
            app.overlay = OverlayState::RecursiveDagBrowser(state);
        }
        return;
    }

    if let OverlayState::RecursiveDagBrowser(previous) = &app.overlay {
        if matches!(new_state.load_status, RecursiveDagLoadStatus::Ready) {
            let previous_row_key = previous.selected_inspector_row().map(|row| row.key);
            let previous_inspector = previous.inspector.clone();
            let previous_graph_id = previous.selected_graph_id();
            new_state.panel = previous.panel;
            if let Some(graph_id) = previous_graph_id
                && let Some(idx) = new_state
                    .graphs
                    .iter()
                    .position(|graph| graph.id == graph_id)
            {
                new_state.selected_graph = idx;
            }
            new_state.selected_task = previous.selected_task;
            new_state.selected_run = previous.selected_run;
            new_state.selected_cancellation = previous.selected_cancellation;
            new_state.selected_heartbeat = previous.selected_heartbeat;
            new_state.selected_interrupt = previous.selected_interrupt;
            new_state.selected_live_attempt = previous.selected_live_attempt;
            new_state.selected_artifact = previous.selected_artifact;
            new_state.scroll_offset = previous.scroll_offset;
            if new_state.inspector == crate::types::RecursiveDagInspectorState::default() {
                new_state.inspector = previous_inspector;
            } else {
                preserve_current_inspector_chrome_for_artifact_preview_result(
                    &mut new_state,
                    &previous_inspector,
                );
            }
            new_state.clamp_cursors();
            if let Some(row_key) = previous_row_key {
                let rows = new_state.inspector_rows();
                if let Some(idx) = rows.iter().position(|row| row.key == row_key) {
                    new_state.selected_artifact = idx;
                } else {
                    new_state.inspector.close();
                    new_state.selected_artifact = new_state
                        .selected_artifact
                        .min(rows.len().saturating_sub(1));
                }
            }
            prune_stale_artifact_detail(&mut new_state);
            prune_stale_artifact_preview(&mut new_state);
        }
        app.recursive_dag_cache = Some(new_state.clone());
        app.overlay = OverlayState::RecursiveDagBrowser(new_state);
    } else if let OverlayState::GraphReview {
        dashboard_state, ..
    } = &mut app.overlay
    {
        if let Some(previous) = dashboard_state {
            if matches!(new_state.load_status, RecursiveDagLoadStatus::Ready) {
                let previous_row_key = previous.selected_inspector_row().map(|row| row.key);
                let previous_inspector = previous.inspector.clone();
                let previous_graph_id = previous.selected_graph_id();
                new_state.panel = previous.panel;
                if let Some(graph_id) = previous_graph_id
                    && let Some(idx) = new_state
                        .graphs
                        .iter()
                        .position(|graph| graph.id == graph_id)
                {
                    new_state.selected_graph = idx;
                }
                new_state.selected_task = previous.selected_task;
                new_state.selected_run = previous.selected_run;
                new_state.selected_cancellation = previous.selected_cancellation;
                new_state.selected_heartbeat = previous.selected_heartbeat;
                new_state.selected_interrupt = previous.selected_interrupt;
                new_state.selected_live_attempt = previous.selected_live_attempt;
                new_state.selected_artifact = previous.selected_artifact;
                new_state.scroll_offset = previous.scroll_offset;
                if new_state.inspector == crate::types::RecursiveDagInspectorState::default() {
                    new_state.inspector = previous_inspector;
                } else {
                    preserve_current_inspector_chrome_for_artifact_preview_result(
                        &mut new_state,
                        &previous_inspector,
                    );
                }
                new_state.clamp_cursors();
                if let Some(row_key) = previous_row_key {
                    let rows = new_state.inspector_rows();
                    if let Some(idx) = rows.iter().position(|row| row.key == row_key) {
                        new_state.selected_artifact = idx;
                    } else {
                        new_state.inspector.close();
                        new_state.selected_artifact = new_state
                            .selected_artifact
                            .min(rows.len().saturating_sub(1));
                    }
                }
                prune_stale_artifact_detail(&mut new_state);
                prune_stale_artifact_preview(&mut new_state);
            }
        }
        *dashboard_state = Some(new_state.clone());
        app.recursive_dag_cache = Some(new_state);
    } else {
        app.recursive_dag_cache = Some(new_state);
    }
}

fn apply_recursive_dag_artifact_page_result(
    app: &mut App,
    graph_id: RecursiveTaskGraphId,
    new_state: RecursiveDagBrowserState,
) {
    if !app.recursive_dag_state_matches_current_project(&new_state) {
        return;
    }

    let OverlayState::RecursiveDagBrowser(current) = &mut app.overlay else {
        return;
    };

    if current.selected_graph_id() != Some(graph_id) || current.loaded_graph_id != Some(graph_id) {
        current.message =
            Some("artifact summary page result ignored because selected graph changed".to_string());
        app.recursive_dag_cache = Some(current.clone());
        return;
    }

    let Some(new_detail) = new_state
        .detail
        .as_ref()
        .filter(|detail| detail.graph.graph.id == graph_id)
    else {
        current.message =
            Some("artifact summary page result ignored because graph detail was stale".to_string());
        app.recursive_dag_cache = Some(current.clone());
        return;
    };
    let previous_row_key = current.selected_inspector_row().map(|row| row.key);
    let Some(current_detail) = current
        .detail
        .as_mut()
        .filter(|detail| detail.graph.graph.id == graph_id)
    else {
        current.message = Some(
            "artifact summary page result ignored because current graph is not hydrated"
                .to_string(),
        );
        app.recursive_dag_cache = Some(current.clone());
        return;
    };

    current_detail.artifact_summary_page = new_detail.artifact_summary_page.clone();
    current.capabilities = new_state.capabilities;
    current.project_id = new_state.project_id;
    current.message = new_state.message;
    current.loaded_at = new_state.loaded_at;
    current.clamp_cursors();
    if let Some(row_key) = previous_row_key {
        let rows = current.inspector_rows();
        if let Some(idx) = rows.iter().position(|row| row.key == row_key) {
            current.selected_artifact = idx;
        } else {
            current.selected_artifact = current.selected_artifact.min(rows.len().saturating_sub(1));
        }
    }
    prune_stale_artifact_detail(current);
    prune_stale_artifact_preview(current);
    app.recursive_dag_cache = Some(current.clone());
}

fn preserve_current_inspector_chrome_for_artifact_preview_result(
    new_state: &mut RecursiveDagBrowserState,
    previous_inspector: &crate::types::RecursiveDagInspectorState,
) {
    if new_state.inspector.artifact_preview.is_none()
        || new_state.inspector.artifact_preview == previous_inspector.artifact_preview
    {
        return;
    }

    new_state.inspector.open = previous_inspector.open;
    new_state.inspector.view = previous_inspector.view;
    new_state.inspector.artifact_nav_prefix = previous_inspector.artifact_nav_prefix;
}

fn prune_stale_artifact_detail(state: &mut RecursiveDagBrowserState) {
    let Some(detail) = &state.inspector.artifact_detail else {
        return;
    };
    let selected_row = state.selected_inspector_row();
    let detail_is_stale = state.selected_graph_id() != Some(detail.graph_id)
        || !selected_row
            .as_ref()
            .is_some_and(|row| detail.matches_row(row));
    if detail_is_stale {
        state.inspector.artifact_detail = None;
    }
}

fn prune_stale_artifact_preview(state: &mut RecursiveDagBrowserState) {
    let Some(preview) = &state.inspector.artifact_preview else {
        return;
    };
    let selected_row = state.selected_inspector_row();
    let preview_is_stale = state.selected_graph_id() != Some(preview.graph_id)
        || !selected_row
            .as_ref()
            .is_some_and(|row| preview.matches_row(row));
    if preview_is_stale {
        state.inspector.artifact_preview = None;
    }
}

fn current_selected_graph_id(app: &App) -> Option<RecursiveTaskGraphId> {
    active_recursive_browser(app).and_then(RecursiveDagBrowserState::selected_graph_id)
}

fn start_recursive_dag_load(app: &mut App, selected_graph_id: Option<RecursiveTaskGraphId>) {
    let project_id = app.current_project_id;
    if app.recursive_dag_rx.is_some()
        && matches!(
            &app.overlay,
            OverlayState::RecursiveDagBrowser(state)
                if matches!(state.fake_run, RecursiveDagFakeRunState::Running { .. })
                    || matches!(state.live_run, RecursiveDagLiveRunState::Running { .. })
                    || state.control.is_submitting()
        )
    {
        update_control_input(app, |state| {
            let label = match &state.control {
                RecursiveDagControlState::Submitting { kind, .. } => kind.label(),
                _ if matches!(state.live_run, RecursiveDagLiveRunState::Running { .. }) => {
                    "LIVE scheduler"
                }
                _ => "FAKE scheduler",
            };
            state.message = Some(format!(
                "recursive DAG refresh blocked: {label} operation already in flight"
            ));
        });
        app.mark_dirty();
        return;
    }

    if !app.poll.connected {
        let state = RecursiveDagBrowserState::disconnected(
            project_id,
            "daemon is not connected; start rsid and reopen :dag",
        );
        app.recursive_dag_cache = Some(state.clone());
        app.overlay = OverlayState::RecursiveDagBrowser(state);
        app.recursive_dag_rx = None;
        app.mark_dirty();
        return;
    }

    let previous_state = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state)
            if app.recursive_dag_state_matches_current_project(state) =>
        {
            Some(state.clone())
        }
        OverlayState::RecursiveDagBrowser(_) => None,
        _ => app
            .recursive_dag_cache
            .as_ref()
            .filter(|state| app.recursive_dag_state_matches_current_project(state))
            .cloned(),
    };
    let mut loading =
        previous_state.unwrap_or_else(|| RecursiveDagBrowserState::loading(project_id));
    loading.project_id = project_id;
    loading.load_status = RecursiveDagLoadStatus::Loading;
    loading.message = Some("loading recursive DAG readbacks".to_string());
    loading.fake_run = RecursiveDagFakeRunState::Idle;
    loading.live_run = RecursiveDagLiveRunState::Idle;
    loading.control = RecursiveDagControlState::Idle;
    app.overlay = OverlayState::RecursiveDagBrowser(loading);

    let socket_path = app.client.socket_path().to_path_buf();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = load_recursive_dag_browser(socket_path, project_id, selected_graph_id).await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
    app.mark_dirty();
}

fn start_artifact_summary_page_load_more(app: &mut App) {
    if app.recursive_dag_rx.is_some() {
        update_control_input(app, |state| {
            state.message = Some(
                "artifact summary load-more blocked: recursive DAG readback already in flight"
                    .to_string(),
            );
        });
        return;
    }

    if !app.poll.connected {
        update_control_input(app, |state| {
            state.message =
                Some("artifact summary load-more unavailable: daemon is not connected".to_string());
        });
        return;
    }

    let Some(previous_state) = active_recursive_browser_for_load(app) else {
        return;
    };

    if !previous_state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_artifact_list_pagination)
    {
        update_control_input(app, |state| {
            if let Some(detail) = state.detail.as_mut() {
                detail.artifact_summary_page =
                    RecursiveDagArtifactSummaryPageState::capability_disabled(
                        detail.graph.graph.id,
                    );
            }
            state.message =
                Some("artifact summaries unavailable: pagination capability disabled".to_string());
        });
        return;
    }

    let Some(graph_id) = previous_state.selected_graph_id() else {
        update_control_input(app, |state| {
            state.message =
                Some("artifact summary load-more unavailable: no graph selected".to_string());
        });
        return;
    };
    let Some(detail) = previous_state.selected_detail() else {
        update_control_input(app, |state| {
            state.message = Some(
                "artifact summary load-more unavailable: selected graph is not hydrated"
                    .to_string(),
            );
        });
        return;
    };
    let page = &detail.artifact_summary_page;
    if page.page_cap_reached || page.loaded_pages >= RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT {
        update_control_input(app, |state| {
            state.message = Some(format!(
                "artifact summary load-more unavailable: local page cap is {RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT}"
            ));
        });
        return;
    }
    if !page.has_more {
        update_control_input(app, |state| {
            state.message = Some("artifact summary list is already at end".to_string());
        });
        return;
    }
    let Some(cursor) = page.next_cursor.clone() else {
        update_control_input(app, |state| {
            state.message = Some(
                "artifact summary load-more unavailable: daemon did not return a cursor"
                    .to_string(),
            );
        });
        return;
    };

    update_control_input(app, |state| {
        if let Some(detail) = state
            .detail
            .as_mut()
            .filter(|detail| detail.graph.graph.id == graph_id)
        {
            detail.artifact_summary_page.mark_loading_more();
        }
        state.message = Some("loading next artifact summary page".to_string());
    });

    let socket_path = app.client.socket_path().to_path_buf();
    let project_id = app.current_project_id;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = load_more_artifact_summaries_into_state(
            socket_path,
            project_id,
            previous_state,
            graph_id,
            cursor,
        )
        .await;
        let _ = tx.send(
            result.map(|state| RecursiveDagAsyncResult::ArtifactSummaryPage { graph_id, state }),
        );
    });
}

fn start_validation_detail_load(
    app: &mut App,
    validation_id: Option<RecursiveLiveOutputValidationId>,
    live_attempt_id: RecursiveLiveAttemptId,
) {
    if app.recursive_dag_rx.is_some() {
        update_control_input(app, |state| {
            state.message = Some(
                "validation detail blocked: recursive DAG readback already in flight".to_string(),
            );
        });
        return;
    }

    let Some(previous_state) = active_recursive_browser_for_load(app) else {
        return;
    };

    if !previous_state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_live_validation_inspection)
    {
        update_control_input(app, |state| {
            state.inspector.validation_detail = Some(RecursiveDagValidationDetailState {
                validation_id,
                live_attempt_id,
                status: RecursiveDagInspectorLoadStatus::CapabilityDisabled,
                result: None,
                issues: Vec::new(),
                message: Some(
                    "live validation capability disabled: recursive_dag_live_validation_inspection=false"
                        .to_string(),
                ),
            });
            state.message = Some(
                "validation detail unavailable: live validation capability disabled".to_string(),
            );
        });
        return;
    }

    update_control_input(app, |state| {
        state.inspector.validation_detail = Some(RecursiveDagValidationDetailState::loading(
            validation_id,
            live_attempt_id,
        ));
    });

    let socket_path = app.client.socket_path().to_path_buf();
    let project_id = app.current_project_id;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = load_validation_detail_into_state(
            socket_path,
            project_id,
            previous_state,
            validation_id,
            live_attempt_id,
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
}

fn start_artifact_detail_load(app: &mut App, row: crate::types::RecursiveDagInspectorRow) {
    if app.recursive_dag_rx.is_some() {
        update_control_input(app, |state| {
            state.message = Some(
                "artifact detail blocked: recursive DAG readback already in flight".to_string(),
            );
        });
        return;
    }

    let Some(previous_state) = active_recursive_browser_for_load(app) else {
        return;
    };

    let Some(graph_id) = row.artifact_graph_id() else {
        update_control_input(app, |state| {
            state.message = Some("artifact detail unavailable: row has no graph id".to_string());
        });
        return;
    };
    let Some(artifact_id) = row.artifact_id() else {
        update_control_input(app, |state| {
            state.message = Some("artifact detail unavailable: row has no artifact id".to_string());
        });
        return;
    };
    if previous_state.selected_graph_id() != Some(graph_id) {
        update_control_input(app, |state| {
            state.inspector.artifact_detail = None;
            state.message = Some(
                "artifact detail dropped: selected row belongs to a different graph".to_string(),
            );
        });
        return;
    }

    if !previous_state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_artifact_lookup)
    {
        update_control_input(app, |state| {
            state.inspector.artifact_detail = Some(RecursiveDagArtifactDetailState {
                graph_id,
                artifact_id,
                status: RecursiveDagInspectorLoadStatus::CapabilityDisabled,
                readback: None,
                message: Some(
                    "artifact detail unavailable: recursive_dag_artifact_lookup=false".to_string(),
                ),
                warnings: Vec::new(),
            });
            state.message =
                Some("artifact detail unavailable: lookup capability disabled".to_string());
        });
        return;
    }

    update_control_input(app, |state| {
        state.inspector.artifact_detail = Some(RecursiveDagArtifactDetailState::loading(
            graph_id,
            artifact_id,
        ));
        state.message = Some(format!("loading artifact detail {artifact_id}"));
    });

    let socket_path = app.client.socket_path().to_path_buf();
    let project_id = app.current_project_id;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = load_artifact_detail_into_state(
            socket_path,
            project_id,
            previous_state,
            graph_id,
            artifact_id,
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
}

fn start_selected_artifact_preview_load(app: &mut App) {
    if app.recursive_dag_rx.is_some() {
        update_control_input(app, |state| {
            state.message = Some(
                "artifact preview blocked: recursive DAG readback already in flight".to_string(),
            );
        });
        return;
    }

    let Some(previous_state) = active_recursive_browser_for_load(app) else {
        return;
    };

    let Some(row) = previous_state.selected_inspector_row() else {
        update_control_input(app, |state| {
            state.inspector.artifact_preview = None;
            state.message =
                Some("artifact preview unavailable: no inspector row selected".to_string());
        });
        return;
    };
    let Some(graph_id) = row.artifact_graph_id() else {
        update_control_input(app, |state| {
            state.inspector.artifact_preview = None;
            state.message =
                Some("artifact preview unavailable: selected row has no graph id".to_string());
        });
        return;
    };
    let Some(artifact_id) = row.artifact_id() else {
        update_control_input(app, |state| {
            state.inspector.artifact_preview = None;
            state.message =
                Some("artifact preview unavailable: selected row has no artifact id".to_string());
        });
        return;
    };
    if previous_state.selected_graph_id() != Some(graph_id) {
        update_control_input(app, |state| {
            state.inspector.artifact_preview = None;
            state.message = Some(
                "artifact preview dropped: selected row belongs to a different graph".to_string(),
            );
        });
        return;
    }

    if !previous_state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_artifact_preview_inspection)
    {
        update_control_input(app, |state| {
            state.inspector.artifact_preview = Some(
                RecursiveDagArtifactPreviewState::capability_disabled(graph_id, artifact_id),
            );
            state.message =
                Some("artifact preview unavailable: preview capability disabled".to_string());
        });
        return;
    }

    update_control_input(app, |state| {
        state.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState::loading(
            graph_id,
            artifact_id,
        ));
        state.message = Some(format!(
            "loading bounded artifact preview {artifact_id} max_bytes={} max_lines={}",
            RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_BYTES, RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_LINES
        ));
    });

    let socket_path = app.client.socket_path().to_path_buf();
    let project_id = app.current_project_id;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = load_artifact_preview_into_state(
            socket_path,
            project_id,
            previous_state,
            graph_id,
            artifact_id,
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
}

fn start_selected_live_attempt_artifact_load(app: &mut App) {
    if app.recursive_dag_rx.is_some() {
        update_control_input(app, |state| {
            state.message = Some(
                "live attempt artifact load blocked: recursive DAG readback already in flight"
                    .to_string(),
            );
        });
        return;
    }

    let previous_state = match &app.overlay {
        OverlayState::RecursiveDagBrowser(state)
            if app.recursive_dag_state_matches_current_project(state) =>
        {
            state.clone()
        }
        _ => return,
    };
    let Some((graph_id, live_attempt_id)) = previous_state.selected_live_attempt_context() else {
        update_control_input(app, |state| {
            state.message = Some("no live attempt selected for artifact buckets".to_string());
        });
        return;
    };

    if !previous_state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_live_status_inspection)
    {
        update_control_input(app, |state| {
            state.inspector.live_attempt_artifacts = Some(RecursiveDagLiveAttemptArtifactsState {
                graph_id,
                live_attempt_id,
                status: RecursiveDagInspectorLoadStatus::CapabilityDisabled,
                artifacts: None,
                message: Some(
                    "live attempt artifact buckets unavailable: recursive_dag_live_status_inspection=false"
                        .to_string(),
                ),
                warnings: Vec::new(),
            });
            state.panel = crate::types::RecursiveDagPanel::Artifacts;
            state.message = Some(
                "live attempt artifact buckets unavailable: live status capability disabled"
                    .to_string(),
            );
        });
        return;
    }

    update_control_input(app, |state| {
        state.inspector.live_attempt_artifacts = Some(
            RecursiveDagLiveAttemptArtifactsState::loading(graph_id, live_attempt_id),
        );
        state.panel = crate::types::RecursiveDagPanel::Artifacts;
    });

    let socket_path = app.client.socket_path().to_path_buf();
    let project_id = app.current_project_id;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.recursive_dag_rx = Some(rx);
    tokio::spawn(async move {
        let result = load_live_attempt_artifacts_into_state(
            socket_path,
            project_id,
            previous_state,
            graph_id,
            live_attempt_id,
        )
        .await;
        let _ = tx.send(result.map(RecursiveDagAsyncResult::Browser));
    });
}

async fn load_validation_detail_into_state(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    mut state: RecursiveDagBrowserState,
    validation_id: Option<RecursiveLiveOutputValidationId>,
    live_attempt_id: RecursiveLiveAttemptId,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path);
    if let Err(error) = client.connect().await {
        set_validation_detail_error(
            &mut state,
            validation_id,
            live_attempt_id,
            format!("validation detail unavailable: {}", error),
        );
        return Ok(state);
    }
    let caps = match client.get_daemon_capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            set_validation_detail_error(
                &mut state,
                validation_id,
                live_attempt_id,
                format!("validation detail unavailable: {}", error),
            );
            return Ok(state);
        }
    };
    state.project_id = project_id;
    state.capabilities = Some(caps.clone());

    if !caps.recursive_dag_live_validation_inspection {
        state.inspector.validation_detail = Some(RecursiveDagValidationDetailState {
            validation_id,
            live_attempt_id,
            status: RecursiveDagInspectorLoadStatus::CapabilityDisabled,
            result: None,
            issues: Vec::new(),
            message: Some(
                "live validation capability disabled: recursive_dag_live_validation_inspection=false"
                    .to_string(),
            ),
        });
        return Ok(state);
    }

    let result = match client
        .get_recursive_live_output_validation_result(GetRecursiveLiveOutputValidationResultParams {
            validation_id,
            live_attempt_id: Some(live_attempt_id),
            latest: validation_id.is_none(),
            include_issues: true,
            include_normalized_output: false,
            include_validation_report: false,
        })
        .await
    {
        Ok(result) => result,
        Err(error) => {
            set_validation_detail_error(
                &mut state,
                validation_id,
                live_attempt_id,
                format!("validation detail unavailable: {}", error),
            );
            return Ok(state);
        }
    };

    let mut issues = match client
        .list_recursive_live_validation_issues(ListRecursiveLiveValidationIssuesParams {
            validation_id,
            live_attempt_id: validation_id.is_none().then_some(live_attempt_id),
            limit: Some(RECURSIVE_DAG_VALIDATION_ISSUE_LIMIT as u32),
            ..Default::default()
        })
        .await
    {
        Ok(issues) => issues,
        Err(error) => {
            let fallback_issues = result
                .as_ref()
                .map(|result| result.issues.clone())
                .unwrap_or_default();
            state.inspector.validation_detail = Some(RecursiveDagValidationDetailState {
                validation_id,
                live_attempt_id,
                status: if result.is_some() {
                    RecursiveDagInspectorLoadStatus::Ready
                } else {
                    RecursiveDagInspectorLoadStatus::Error
                },
                result,
                issues: fallback_issues,
                message: Some(format!("validation issue rows unavailable: {}", error)),
            });
            state.message = Some("validation issue rows unavailable".to_string());
            return Ok(state);
        }
    };
    issues.truncate(RECURSIVE_DAG_VALIDATION_ISSUE_LIMIT);

    let message = if result.is_some() {
        format!(
            "loaded validation detail and {} issue row(s) for live {}",
            issues.len(),
            live_attempt_id
        )
    } else {
        format!("validation detail missing for live {live_attempt_id}")
    };

    state.inspector.validation_detail = Some(RecursiveDagValidationDetailState {
        validation_id,
        live_attempt_id,
        status: if result.is_some() {
            RecursiveDagInspectorLoadStatus::Ready
        } else {
            RecursiveDagInspectorLoadStatus::Error
        },
        result,
        issues,
        message: Some(message.clone()),
    });
    state.message = Some(message);
    Ok(state)
}

async fn load_artifact_detail_into_state(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    mut state: RecursiveDagBrowserState,
    graph_id: RecursiveTaskGraphId,
    artifact_id: i64,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path);
    if let Err(error) = client.connect().await {
        set_artifact_detail_error(
            &mut state,
            graph_id,
            artifact_id,
            format!("artifact detail unavailable: {}", error),
        );
        return Ok(state);
    }
    let caps = match client.get_daemon_capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            set_artifact_detail_error(
                &mut state,
                graph_id,
                artifact_id,
                format!("artifact detail unavailable: {}", error),
            );
            return Ok(state);
        }
    };
    state.project_id = project_id;
    state.capabilities = Some(caps.clone());

    if !caps.recursive_dag_artifact_lookup {
        state.inspector.artifact_detail = Some(RecursiveDagArtifactDetailState {
            graph_id,
            artifact_id,
            status: RecursiveDagInspectorLoadStatus::CapabilityDisabled,
            readback: None,
            message: Some(
                "artifact detail unavailable: recursive_dag_artifact_lookup=false".to_string(),
            ),
            warnings: Vec::new(),
        });
        state.message = Some("artifact detail unavailable: lookup capability disabled".to_string());
        return Ok(state);
    }

    let readback = match client
        .get_recursive_execution_artifact(GetRecursiveExecutionArtifactParams {
            graph_id,
            artifact_id,
            include_links: true,
        })
        .await
    {
        Ok(readback) => readback,
        Err(error) => {
            set_artifact_detail_error(
                &mut state,
                graph_id,
                artifact_id,
                format!("artifact detail unavailable: {}", error),
            );
            return Ok(state);
        }
    };

    if readback.artifact.graph_id != graph_id || readback.artifact.id != artifact_id {
        state.inspector.artifact_detail = Some(RecursiveDagArtifactDetailState {
            graph_id,
            artifact_id,
            status: RecursiveDagInspectorLoadStatus::Error,
            readback: None,
            message: Some(
                "artifact detail dropped: daemon returned a different graph or artifact"
                    .to_string(),
            ),
            warnings: Vec::new(),
        });
        state.message = Some("artifact detail dropped before rendering".to_string());
        return Ok(state);
    }

    let warnings = readback_warnings_to_strings(&readback.warnings);
    let message = format!("loaded graph-guarded artifact detail {artifact_id}");
    state.inspector.artifact_detail = Some(RecursiveDagArtifactDetailState {
        graph_id,
        artifact_id,
        status: RecursiveDagInspectorLoadStatus::Ready,
        readback: Some(readback),
        message: Some(message.clone()),
        warnings,
    });
    state.message = Some(message);
    state.clamp_cursors();
    Ok(state)
}

async fn load_artifact_preview_into_state(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    mut state: RecursiveDagBrowserState,
    graph_id: RecursiveTaskGraphId,
    artifact_id: i64,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path);
    if let Err(error) = client.connect().await {
        set_artifact_preview_error(
            &mut state,
            graph_id,
            artifact_id,
            format!("artifact preview unavailable: {}", error),
        );
        return Ok(state);
    }
    let caps = match client.get_daemon_capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            set_artifact_preview_error(
                &mut state,
                graph_id,
                artifact_id,
                format!("artifact preview unavailable: {}", error),
            );
            return Ok(state);
        }
    };
    state.project_id = project_id;
    state.capabilities = Some(caps.clone());

    if !caps.recursive_dag_artifact_preview_inspection {
        state.inspector.artifact_preview = Some(
            RecursiveDagArtifactPreviewState::capability_disabled(graph_id, artifact_id),
        );
        state.message =
            Some("artifact preview unavailable: preview capability disabled".to_string());
        return Ok(state);
    }

    let preview = match client
        .preview_recursive_execution_artifact(PreviewRecursiveExecutionArtifactParams {
            graph_id,
            artifact_id,
            byte_offset: None,
            line_offset: None,
            max_bytes: Some(RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_BYTES),
            max_lines: Some(RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_LINES),
            render_hint: None,
            require_complete: false,
        })
        .await
    {
        Ok(preview) => preview,
        Err(error) => {
            set_artifact_preview_error(
                &mut state,
                graph_id,
                artifact_id,
                format!("artifact preview unavailable: {}", error),
            );
            return Ok(state);
        }
    };

    if preview.graph_id != graph_id || preview.artifact_id != artifact_id {
        set_artifact_preview_error(
            &mut state,
            graph_id,
            artifact_id,
            "artifact preview dropped: daemon returned a different graph or artifact".to_string(),
        );
        return Ok(state);
    }

    let warnings = readback_warnings_to_strings(&preview.warnings);
    let message = format!(
        "loaded bounded artifact preview {artifact_id} state={:?} shown={}B/{}L",
        preview.content_state, preview.shown_bytes, preview.shown_lines
    );
    state.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState {
        graph_id,
        artifact_id,
        status: RecursiveDagInspectorLoadStatus::Ready,
        preview: Some(preview),
        message: Some(message.clone()),
        warnings,
    });
    state.message = Some(message);
    state.clamp_cursors();
    Ok(state)
}

async fn load_live_attempt_artifacts_into_state(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    mut state: RecursiveDagBrowserState,
    graph_id: RecursiveTaskGraphId,
    live_attempt_id: RecursiveLiveAttemptId,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path);
    if let Err(error) = client.connect().await {
        set_live_attempt_artifacts_error(
            &mut state,
            graph_id,
            live_attempt_id,
            format!("live attempt artifact buckets unavailable: {}", error),
        );
        return Ok(state);
    }
    let caps = match client.get_daemon_capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            set_live_attempt_artifacts_error(
                &mut state,
                graph_id,
                live_attempt_id,
                format!("live attempt artifact buckets unavailable: {}", error),
            );
            return Ok(state);
        }
    };
    state.project_id = project_id;
    state.capabilities = Some(caps.clone());

    if !caps.recursive_dag_live_status_inspection {
        state.inspector.live_attempt_artifacts = Some(RecursiveDagLiveAttemptArtifactsState {
            graph_id,
            live_attempt_id,
            status: RecursiveDagInspectorLoadStatus::CapabilityDisabled,
            artifacts: None,
            message: Some(
                "live attempt artifact buckets unavailable: recursive_dag_live_status_inspection=false"
                    .to_string(),
            ),
            warnings: Vec::new(),
        });
        return Ok(state);
    }

    let mut artifacts = match client
        .get_recursive_live_attempt_artifacts(GetRecursiveLiveAttemptArtifactsParams {
            live_attempt_id,
            include_prompt: true,
            include_raw_output: true,
            include_normalized_output: true,
            include_validation_report: true,
            include_diff: true,
            include_tests: true,
            include_produced_artifacts: true,
        })
        .await
    {
        Ok(artifacts) => artifacts,
        Err(error) => {
            set_live_attempt_artifacts_error(
                &mut state,
                graph_id,
                live_attempt_id,
                format!("live attempt artifact buckets unavailable: {}", error),
            );
            return Ok(state);
        }
    };
    let mut warnings = Vec::new();
    retain_live_attempt_artifacts_for_graph(&mut artifacts, graph_id, &mut warnings);
    truncate_live_attempt_artifact_readback(&mut artifacts, &mut warnings);
    let row_count = live_attempt_artifact_count(&artifacts);

    state.inspector.live_attempt_artifacts = Some(RecursiveDagLiveAttemptArtifactsState {
        graph_id,
        live_attempt_id,
        status: RecursiveDagInspectorLoadStatus::Ready,
        artifacts: Some(artifacts),
        message: Some(format!(
            "loaded {row_count} selected live attempt artifact row(s)"
        )),
        warnings,
    });
    state.panel = crate::types::RecursiveDagPanel::Artifacts;
    state.message = Some(format!(
        "loaded selected live attempt artifact buckets for {live_attempt_id}"
    ));
    state.clamp_cursors();
    Ok(state)
}

fn set_validation_detail_error(
    state: &mut RecursiveDagBrowserState,
    validation_id: Option<RecursiveLiveOutputValidationId>,
    live_attempt_id: RecursiveLiveAttemptId,
    message: String,
) {
    state.inspector.validation_detail = Some(RecursiveDagValidationDetailState {
        validation_id,
        live_attempt_id,
        status: RecursiveDagInspectorLoadStatus::Error,
        result: None,
        issues: Vec::new(),
        message: Some(message.clone()),
    });
    state.message = Some(message);
}

fn set_artifact_detail_error(
    state: &mut RecursiveDagBrowserState,
    graph_id: RecursiveTaskGraphId,
    artifact_id: i64,
    message: String,
) {
    state.inspector.artifact_detail = Some(RecursiveDagArtifactDetailState {
        graph_id,
        artifact_id,
        status: RecursiveDagInspectorLoadStatus::Error,
        readback: None,
        message: Some(message.clone()),
        warnings: Vec::new(),
    });
    state.message = Some(message);
}

fn set_artifact_preview_error(
    state: &mut RecursiveDagBrowserState,
    graph_id: RecursiveTaskGraphId,
    artifact_id: i64,
    message: String,
) {
    state.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState::error(
        graph_id,
        artifact_id,
        message.clone(),
    ));
    state.message = Some(message);
}

fn set_live_attempt_artifacts_error(
    state: &mut RecursiveDagBrowserState,
    graph_id: RecursiveTaskGraphId,
    live_attempt_id: RecursiveLiveAttemptId,
    message: String,
) {
    state.inspector.live_attempt_artifacts = Some(RecursiveDagLiveAttemptArtifactsState {
        graph_id,
        live_attempt_id,
        status: RecursiveDagInspectorLoadStatus::Error,
        artifacts: None,
        message: Some(message.clone()),
        warnings: Vec::new(),
    });
    state.panel = crate::types::RecursiveDagPanel::Artifacts;
    state.message = Some(message);
}

fn readback_warnings_to_strings(warnings: &[RecursiveReadbackWarning]) -> Vec<String> {
    warnings
        .iter()
        .map(
            |warning| match (&warning.resource_type, &warning.resource_id) {
                (Some(resource_type), Some(resource_id)) => format!(
                    "{} {} {}: {}",
                    warning.code, resource_type, resource_id, warning.message
                ),
                _ => format!("{}: {}", warning.code, warning.message),
            },
        )
        .collect()
}

fn retain_live_attempt_artifacts_for_graph(
    artifacts: &mut RecursiveLiveAttemptArtifactReadback,
    graph_id: RecursiveTaskGraphId,
    warnings: &mut Vec<String>,
) {
    if artifacts
        .prompt_artifact
        .as_ref()
        .is_some_and(|artifact| artifact.graph_id != graph_id)
    {
        artifacts.prompt_artifact = None;
        warnings.push("dropped prompt artifact from a different graph".to_string());
    }
    if artifacts
        .normalized_output_artifact
        .as_ref()
        .is_some_and(|artifact| artifact.graph_id != graph_id)
    {
        artifacts.normalized_output_artifact = None;
        warnings.push("dropped normalized output artifact from a different graph".to_string());
    }
    retain_graph_artifacts(
        &mut artifacts.raw_output_artifacts,
        graph_id,
        "raw output",
        warnings,
    );
    retain_graph_artifacts(
        &mut artifacts.validation_artifacts,
        graph_id,
        "validation",
        warnings,
    );
    retain_graph_artifacts(&mut artifacts.diff_artifacts, graph_id, "diff", warnings);
    retain_graph_artifacts(&mut artifacts.test_artifacts, graph_id, "test", warnings);
    retain_graph_artifacts(
        &mut artifacts.produced_artifacts,
        graph_id,
        "produced",
        warnings,
    );
}

fn retain_graph_artifacts(
    artifacts: &mut Vec<rsi_common::RecursiveExecutionArtifact>,
    graph_id: RecursiveTaskGraphId,
    label: &str,
    warnings: &mut Vec<String>,
) {
    let before = artifacts.len();
    artifacts.retain(|artifact| artifact.graph_id == graph_id);
    let dropped = before.saturating_sub(artifacts.len());
    if dropped > 0 {
        warnings.push(format!(
            "dropped {dropped} {label} artifact(s) from a different graph"
        ));
    }
}

fn truncate_live_attempt_artifact_readback(
    artifacts: &mut RecursiveLiveAttemptArtifactReadback,
    warnings: &mut Vec<String>,
) {
    truncate_rows(
        &mut artifacts.raw_output_artifacts,
        RECURSIVE_DAG_ARTIFACT_LIMIT,
        "live raw output artifacts",
        warnings,
    );
    truncate_rows(
        &mut artifacts.validation_artifacts,
        RECURSIVE_DAG_ARTIFACT_LIMIT,
        "live validation artifacts",
        warnings,
    );
    truncate_rows(
        &mut artifacts.diff_artifacts,
        RECURSIVE_DAG_ARTIFACT_LIMIT,
        "live diff artifacts",
        warnings,
    );
    truncate_rows(
        &mut artifacts.test_artifacts,
        RECURSIVE_DAG_ARTIFACT_LIMIT,
        "live test artifacts",
        warnings,
    );
    truncate_rows(
        &mut artifacts.produced_artifacts,
        RECURSIVE_DAG_ARTIFACT_LIMIT,
        "live produced artifacts",
        warnings,
    );
}

fn live_attempt_artifact_count(artifacts: &RecursiveLiveAttemptArtifactReadback) -> usize {
    usize::from(artifacts.prompt_artifact.is_some())
        + artifacts.raw_output_artifacts.len()
        + usize::from(artifacts.normalized_output_artifact.is_some())
        + artifacts.validation_artifacts.len()
        + artifacts.diff_artifacts.len()
        + artifacts.test_artifacts.len()
        + artifacts.produced_artifacts.len()
}

async fn run_recursive_dag_fake_scheduler_and_refresh(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    graph_id: RecursiveTaskGraphId,
    max_steps: u32,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path.clone());
    client.connect().await.map_err(|e| e.to_string())?;
    let caps = client
        .get_daemon_capabilities()
        .await
        .map_err(|e| e.to_string())?;

    if !caps.recursive_dag_inspection {
        return Ok(RecursiveDagBrowserState::capability_disabled(
            project_id,
            caps,
            "FAKE scheduler unavailable: daemon does not advertise recursive_dag_inspection",
        ));
    }

    if !caps.recursive_dag_scheduler_control {
        return load_recursive_dag_browser_with_message(
            socket_path,
            project_id,
            Some(graph_id),
            "FAKE scheduler unavailable: recursive_dag_scheduler_control capability disabled",
        )
        .await;
    }

    let run_result = client
        .run_recursive_fake_scheduler(graph_id, max_steps)
        .await
        .map_err(|error| error.to_string());

    match run_result {
        Ok(run) => {
            load_recursive_dag_browser_with_message(
                socket_path,
                project_id,
                Some(graph_id),
                &format!(
                    "FAKE scheduler completed: run={} status={:?} steps={}/{}",
                    run.id, run.status, run.step_count, run.max_steps
                ),
            )
            .await
        }
        Err(error) => {
            load_recursive_dag_browser_with_message(
                socket_path,
                project_id,
                Some(graph_id),
                &format!("FAKE scheduler failed: {error}"),
            )
            .await
        }
    }
}

async fn run_recursive_dag_live_scheduler_and_refresh(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    graph_id: RecursiveTaskGraphId,
    max_steps: u32,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path.clone());
    client.connect().await.map_err(|e| e.to_string())?;
    let caps = client
        .get_daemon_capabilities()
        .await
        .map_err(|e| e.to_string())?;

    if !caps.recursive_dag_inspection {
        return Ok(RecursiveDagBrowserState::capability_disabled(
            project_id,
            caps,
            "LIVE scheduler unavailable: daemon does not advertise recursive_dag_inspection",
        ));
    }

    if !caps.recursive_dag_live_scheduler_control {
        return load_recursive_dag_browser_with_message(
            socket_path,
            project_id,
            Some(graph_id),
            "LIVE scheduler unavailable: recursive_dag_live_scheduler_control capability disabled",
        )
        .await;
    }

    let run_result = client
        .run_recursive_live_scheduler(graph_id, max_steps)
        .await
        .map_err(|error| error.to_string());

    match run_result {
        Ok(run) => {
            load_recursive_dag_browser_with_message(
                socket_path,
                project_id,
                Some(graph_id),
                &format!(
                    "LIVE scheduler completed: run={} status={:?} steps={}/{} attempts={}",
                    run.scheduler_run.id,
                    run.scheduler_run.status,
                    run.step_count,
                    run.scheduler_run.max_steps,
                    run.live_attempts.len()
                ),
            )
            .await
        }
        Err(error) => {
            load_recursive_dag_browser_with_message(
                socket_path,
                project_id,
                Some(graph_id),
                &format!("LIVE scheduler failed: {error}"),
            )
            .await
        }
    }
}

async fn request_recursive_graph_cancellation_and_refresh(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    graph_id: RecursiveTaskGraphId,
    reason: String,
    previous_state: RecursiveDagBrowserState,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path.clone());
    if let Err(error) = client.connect().await {
        return Ok(control_error_state(
            previous_state,
            format!("graph cancellation failed before RPC: {error}"),
        ));
    }
    match recursive_control_capabilities(&mut client, RecursiveDagControlKind::GraphCancellation)
        .await
    {
        Ok(None) => {}
        Ok(Some(message)) => return Ok(control_error_state(previous_state, message)),
        Err(error) => {
            return Ok(control_error_state(
                previous_state,
                format!("graph cancellation failed before RPC: {error}"),
            ));
        }
    }

    let request = match client
        .request_recursive_graph_cancellation(
            graph_id,
            reason,
            Some(RECURSIVE_DAG_CONTROL_REQUESTED_BY.to_string()),
        )
        .await
    {
        Ok(request) => request,
        Err(error) => {
            return Ok(control_error_state(
                previous_state,
                format!("graph cancellation failed: {error}"),
            ));
        }
    };
    let message = cancellation_request_result_message("graph cancellation", &request);
    load_recursive_dag_browser_with_message(socket_path, project_id, Some(graph_id), &message).await
}

async fn request_recursive_run_cancellation_and_refresh(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    selected_graph_id: Option<RecursiveTaskGraphId>,
    run_id: RecursiveSchedulerRunId,
    reason: String,
    previous_state: RecursiveDagBrowserState,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path.clone());
    if let Err(error) = client.connect().await {
        return Ok(control_error_state(
            previous_state,
            format!("run cancellation failed before RPC: {error}"),
        ));
    }
    match recursive_control_capabilities(&mut client, RecursiveDagControlKind::RunCancellation)
        .await
    {
        Ok(None) => {}
        Ok(Some(message)) => return Ok(control_error_state(previous_state, message)),
        Err(error) => {
            return Ok(control_error_state(
                previous_state,
                format!("run cancellation failed before RPC: {error}"),
            ));
        }
    }

    let request = match client
        .request_recursive_scheduler_run_cancellation(
            run_id,
            reason,
            Some(RECURSIVE_DAG_CONTROL_REQUESTED_BY.to_string()),
        )
        .await
    {
        Ok(request) => request,
        Err(error) => {
            return Ok(control_error_state(
                previous_state,
                format!("run cancellation failed: {error}"),
            ));
        }
    };
    let refresh_graph_id = selected_graph_id.or(Some(request.graph_id));
    let message = cancellation_request_result_message("run cancellation", &request);
    load_recursive_dag_browser_with_message(socket_path, project_id, refresh_graph_id, &message)
        .await
}

async fn continue_recursive_recovery_and_refresh(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    selected_graph_id: Option<RecursiveTaskGraphId>,
    max_graphs: u32,
    time_budget_ms: Option<u64>,
    previous_state: RecursiveDagBrowserState,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path.clone());
    if let Err(error) = client.connect().await {
        return Ok(control_error_state(
            previous_state,
            format!("recovery continuation failed before RPC: {error}"),
        ));
    }
    match recursive_control_capabilities(&mut client, RecursiveDagControlKind::RecoveryContinuation)
        .await
    {
        Ok(None) => {}
        Ok(Some(message)) => return Ok(control_error_state(previous_state, message)),
        Err(error) => {
            return Ok(control_error_state(
                previous_state,
                format!("recovery continuation failed before RPC: {error}"),
            ));
        }
    }

    let pass = match client
        .continue_recursive_recovery(max_graphs, time_budget_ms)
        .await
    {
        Ok(pass) => pass,
        Err(error) => {
            return Ok(control_error_state(
                previous_state,
                format!("recovery continuation failed: {error}"),
            ));
        }
    };
    let message = recovery_pass_result_message(&pass);
    load_recursive_dag_browser_with_message(socket_path, project_id, selected_graph_id, &message)
        .await
}

async fn recursive_control_capabilities(
    client: &mut crate::client::DaemonClient,
    kind: RecursiveDagControlKind,
) -> Result<Option<String>, String> {
    let caps = client
        .get_daemon_capabilities()
        .await
        .map_err(|error| error.to_string())?;
    if !caps.recursive_dag_inspection {
        return Ok(Some(
            "recursive DAG control unavailable: recursive_dag_inspection=false".to_string(),
        ));
    }
    match kind {
        RecursiveDagControlKind::GraphCancellation | RecursiveDagControlKind::RunCancellation => {
            if !caps.recursive_dag_cancellation_control {
                return Ok(Some(
                    "cancellation unavailable: recursive_dag_cancellation_control=false"
                        .to_string(),
                ));
            }
        }
        RecursiveDagControlKind::RecoveryContinuation => {
            if !caps.recursive_dag_recovery_control {
                return Ok(Some(
                    "recovery continuation unavailable: recursive_dag_recovery_control=false"
                        .to_string(),
                ));
            }
        }
    }
    Ok(None)
}

fn control_error_state(
    mut state: RecursiveDagBrowserState,
    message: impl Into<String>,
) -> RecursiveDagBrowserState {
    state.control = RecursiveDagControlState::Idle;
    state.fake_run = RecursiveDagFakeRunState::Idle;
    state.live_run = RecursiveDagLiveRunState::Idle;
    state.message = Some(message.into());
    state
}

fn cancellation_request_result_message(
    label: &str,
    request: &RecursiveCancellationRequestSummary,
) -> String {
    let prefix = if request.status == RecursiveCancellationRequestStatus::Requested {
        format!("{label} requested")
    } else {
        format!("{label} warning")
    };
    let mut message = format!(
        "{prefix}: request={} scope={:?} status={:?} requested_by={}",
        short_request_id(request.id),
        request.scope,
        request.status,
        request.requested_by.as_deref().unwrap_or("-")
    );
    if let Some(rejection) = &request.rejection_reason {
        message.push_str(&format!(" rejection={rejection}"));
    }
    message
}

fn recovery_pass_result_message(pass: &RecursiveRecoveryPassSummary) -> String {
    format!(
        "recovery continuation completed: pass={} status={:?} checked={} recovered={} quarantined={} deferred={} skipped={} errors={} stop={}",
        short_request_id(pass.id),
        pass.status,
        pass.checked,
        pass.recovered,
        pass.quarantined,
        pass.deferred,
        pass.skipped,
        pass.errors,
        pass.stop_reason
            .map(|reason| format!("{reason:?}"))
            .unwrap_or_else(|| "-".to_string())
    )
}

fn short_request_id<T: ToString>(id: T) -> String {
    id.to_string().chars().take(8).collect()
}

async fn load_recursive_dag_browser_with_message(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    selected_graph_id: Option<RecursiveTaskGraphId>,
    message: &str,
) -> Result<RecursiveDagBrowserState, String> {
    let mut state = load_recursive_dag_browser(socket_path, project_id, selected_graph_id).await?;
    state.message = Some(match state.message.take() {
        Some(existing) => format!("{message}; {existing}"),
        None => message.to_string(),
    });
    Ok(state)
}

async fn load_recursive_dag_browser(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    selected_graph_id: Option<RecursiveTaskGraphId>,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path);
    client.connect().await.map_err(|e| e.to_string())?;
    let caps = client
        .get_daemon_capabilities()
        .await
        .map_err(|e| e.to_string())?;

    if !caps.recursive_dag_inspection {
        return Ok(RecursiveDagBrowserState::capability_disabled(
            project_id,
            caps,
            "daemon does not advertise recursive_dag_inspection",
        ));
    }

    let mut graphs = client
        .list_recursive_task_graphs(ListRecursiveTaskGraphsParams {
            project_id,
            include_quarantined: true,
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;

    graphs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let graph_id = selected_graph_id
        .filter(|id| graphs.iter().any(|graph| graph.id == *id))
        .or_else(|| graphs.first().map(|graph| graph.id));

    let detail = match graph_id {
        Some(graph_id) => Some(load_selected_graph(&mut client, graph_id, &caps).await?),
        None => None,
    };

    Ok(RecursiveDagBrowserState::ready(
        project_id, caps, graphs, graph_id, detail,
    ))
}

async fn load_selected_graph(
    client: &mut crate::client::DaemonClient,
    graph_id: RecursiveTaskGraphId,
    caps: &rsi_common::rpc::DaemonCapabilities,
) -> Result<RecursiveDagSelectedGraphData, String> {
    let mut warnings = Vec::new();
    let mut graph = client
        .get_recursive_task_graph(graph_id)
        .await
        .map_err(|e| e.to_string())?;
    truncate_graph_detail_for_browser(&mut graph, &mut warnings);

    let operational_status = match client.get_recursive_dag_operational_status(graph_id).await {
        Ok(status) => Some(status),
        Err(error) => {
            warnings.push(format!("operational status readback failed: {error}"));
            None
        }
    };

    let mut scheduler_runs = if caps.recursive_dag_run_inspection {
        match client
            .list_recursive_scheduler_runs(ListRecursiveSchedulerRunsParams {
                graph_id: graph_id.0,
                limit: Some(RECURSIVE_DAG_RUN_LIMIT as u32),
                include_terminal: true,
                ..Default::default()
            })
            .await
        {
            Ok(runs) => runs,
            Err(error) => {
                warnings.push(format!("scheduler run list readback failed: {error}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    truncate_rows(
        &mut scheduler_runs,
        RECURSIVE_DAG_RUN_LIMIT,
        "scheduler runs",
        &mut warnings,
    );

    let selected_run_id: Option<RecursiveSchedulerRunId> = operational_status
        .as_ref()
        .and_then(|status| status.active_run.as_ref().map(|run| run.id))
        .or_else(|| scheduler_runs.first().map(|run| run.id));

    let selected_run_detail = if caps.recursive_dag_run_inspection {
        match selected_run_id {
            Some(run_id) => match client.get_recursive_scheduler_run(run_id).await {
                Ok(mut detail) => {
                    truncate_scheduler_run_detail_for_browser(&mut detail, &mut warnings);
                    Some(detail)
                }
                Err(error) => {
                    warnings.push(format!("selected scheduler run readback failed: {error}"));
                    None
                }
            },
            None => None,
        }
    } else {
        None
    };

    let mut run_events = if caps.recursive_dag_run_inspection {
        match selected_run_id {
            Some(run_id) => match client
                .list_recursive_scheduler_run_events(ListRecursiveSchedulerRunEventsParams {
                    run_id: run_id.0,
                    limit: Some(RECURSIVE_DAG_EVENT_LIMIT as u32),
                    ..Default::default()
                })
                .await
            {
                Ok(events) => events,
                Err(error) => {
                    warnings.push(format!("scheduler run event readback failed: {error}"));
                    Vec::new()
                }
            },
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    truncate_rows(
        &mut run_events,
        RECURSIVE_DAG_EVENT_LIMIT,
        "scheduler run events",
        &mut warnings,
    );

    let mut cancellation_requests = match client
        .list_recursive_cancellation_requests(ListRecursiveCancellationRequestsParams {
            graph_id: Some(graph_id.0),
            limit: Some(RECURSIVE_DAG_LIVE_LIMIT as u32),
            ..Default::default()
        })
        .await
    {
        Ok(requests) => requests,
        Err(error) => {
            warnings.push(format!("cancellation request readback failed: {error}"));
            Vec::new()
        }
    };
    truncate_rows(
        &mut cancellation_requests,
        RECURSIVE_DAG_LIVE_LIMIT,
        "cancellation requests",
        &mut warnings,
    );

    let recovery_status = if caps.recursive_dag_recovery_status {
        match client
            .get_recursive_recovery_status(GetRecursiveRecoveryStatusParams {
                graph_id: Some(graph_id.0),
            })
            .await
        {
            Ok(mut status) => {
                truncate_rows(
                    &mut status.deferred_graphs,
                    RECURSIVE_DAG_LIVE_LIMIT,
                    "deferred recovery graphs",
                    &mut warnings,
                );
                Some(status)
            }
            Err(error) => {
                warnings.push(format!("recovery status readback failed: {error}"));
                None
            }
        }
    } else {
        None
    };

    let mut stale_heartbeats = if caps.recursive_dag_live_status_inspection {
        match client
            .list_stale_recursive_live_attempt_heartbeats(
                ListStaleRecursiveLiveAttemptHeartbeatsParams {
                    graph_id: Some(graph_id),
                    limit: Some(RECURSIVE_DAG_LIVE_LIMIT as u32),
                },
            )
            .await
        {
            Ok(heartbeats) => heartbeats,
            Err(error) => {
                warnings.push(format!("stale heartbeat readback failed: {error}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    truncate_rows(
        &mut stale_heartbeats,
        RECURSIVE_DAG_LIVE_LIMIT,
        "stale heartbeat states",
        &mut warnings,
    );

    let live_recovery_status = if caps.recursive_dag_live_status_inspection {
        match client
            .get_recursive_live_recovery_status(GetRecursiveLiveRecoveryStatusParams {
                graph_id: Some(graph_id),
                ..Default::default()
            })
            .await
        {
            Ok(mut readback) => {
                truncate_rows(
                    &mut readback.live_attempts,
                    RECURSIVE_DAG_LIVE_LIMIT,
                    "live recovery attempts",
                    &mut warnings,
                );
                truncate_rows(
                    &mut readback.heartbeat_states,
                    RECURSIVE_DAG_LIVE_LIMIT,
                    "live recovery heartbeat states",
                    &mut warnings,
                );
                truncate_rows(
                    &mut readback.scheduler_runs,
                    RECURSIVE_DAG_RUN_LIMIT,
                    "live recovery scheduler runs",
                    &mut warnings,
                );
                truncate_rows(
                    &mut readback.linked_sessions,
                    RECURSIVE_DAG_LIVE_LIMIT,
                    "live recovery linked sessions",
                    &mut warnings,
                );
                for warning in readback.warnings.iter().take(5) {
                    warnings.push(format!(
                        "live recovery readback warning {}: {}",
                        warning.code, warning.message
                    ));
                }
                Some(readback)
            }
            Err(error) => {
                warnings.push(format!("live recovery readback failed: {error}"));
                None
            }
        }
    } else {
        None
    };

    let mut live_attempts = if caps.recursive_dag_live_status_inspection {
        match client
            .list_recursive_live_attempts(ListRecursiveLiveAttemptsParams {
                graph_id: Some(graph_id),
                include_terminal: true,
                include_status: true,
                limit: Some(RECURSIVE_DAG_LIVE_LIMIT as u32),
                ..Default::default()
            })
            .await
        {
            Ok(attempts) => attempts,
            Err(error) => {
                warnings.push(format!("live attempt readback failed: {error}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    truncate_rows(
        &mut live_attempts,
        RECURSIVE_DAG_LIVE_LIMIT,
        "live attempts",
        &mut warnings,
    );

    let mut validation_results = if caps.recursive_dag_live_validation_inspection {
        match client
            .list_recursive_live_output_validation_results(
                ListRecursiveLiveOutputValidationResultsParams {
                    graph_id: Some(graph_id),
                    include_issues: true,
                    limit: Some(RECURSIVE_DAG_LIVE_LIMIT as u32),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(results) => results,
            Err(error) => {
                warnings.push(format!("live validation readback failed: {error}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    truncate_rows(
        &mut validation_results,
        RECURSIVE_DAG_LIVE_LIMIT,
        "live validation results",
        &mut warnings,
    );

    let artifact_summary_page = load_artifact_summaries_for_graph(
        client,
        graph_id,
        caps.recursive_dag_artifact_list_pagination,
        &mut warnings,
    )
    .await;

    Ok(RecursiveDagSelectedGraphData {
        graph,
        operational_status,
        scheduler_runs,
        selected_run_detail,
        run_events,
        cancellation_requests,
        recovery_status,
        live_attempts,
        stale_heartbeats,
        live_recovery_status,
        validation_results,
        artifact_summary_page,
        warnings,
    })
}

async fn load_artifact_summaries_for_graph(
    client: &mut crate::client::DaemonClient,
    graph_id: RecursiveTaskGraphId,
    enabled: bool,
    warnings: &mut Vec<String>,
) -> RecursiveDagArtifactSummaryPageState {
    if !enabled {
        return RecursiveDagArtifactSummaryPageState::capability_disabled(graph_id);
    }

    let page = match client
        .list_recursive_execution_artifact_summaries(
            ListRecursiveExecutionArtifactSummariesParams {
                graph_id: Some(graph_id),
                limit: Some(RECURSIVE_DAG_ARTIFACT_LIMIT as u32),
                include_total: false,
                ..Default::default()
            },
        )
        .await
    {
        Ok(page) => page,
        Err(error) => {
            let message = format!("artifact summary readback failed: {error}");
            warnings.push(message.clone());
            return RecursiveDagArtifactSummaryPageState::error(graph_id, message);
        }
    };

    let mut page_warnings = readback_warnings_to_strings(&page.warnings);
    let mut items = page.items;
    let before = items.len();
    items.retain(|summary| summary.graph_id == graph_id);
    let dropped = before.saturating_sub(items.len());
    if dropped > 0 {
        page_warnings.push(format!(
            "dropped {dropped} artifact summary row(s) from a different graph"
        ));
    }
    truncate_rows(
        &mut items,
        RECURSIVE_DAG_ARTIFACT_LIMIT,
        "artifact summaries",
        &mut page_warnings,
    );
    let mut state = RecursiveDagArtifactSummaryPageState::ready(
        graph_id,
        items,
        page.limit,
        page.next_cursor,
        page.has_more,
        page_warnings,
    );
    if state.has_more && state.next_cursor.is_none() {
        state
            .warnings
            .push("artifact summary page has more rows but no next cursor".to_string());
    }
    state
}

async fn load_more_artifact_summaries_into_state(
    socket_path: std::path::PathBuf,
    project_id: Option<uuid::Uuid>,
    mut state: RecursiveDagBrowserState,
    graph_id: RecursiveTaskGraphId,
    cursor: String,
) -> Result<RecursiveDagBrowserState, String> {
    let mut client = crate::client::DaemonClient::new(socket_path);
    state.project_id = project_id;

    if let Err(error) = client.connect().await {
        set_artifact_summary_page_error(
            &mut state,
            graph_id,
            format!("artifact summary load-more unavailable: {}", error),
        );
        return Ok(state);
    }

    let page = match client
        .list_recursive_execution_artifact_summaries(
            ListRecursiveExecutionArtifactSummariesParams {
                graph_id: Some(graph_id),
                cursor: Some(cursor),
                limit: Some(RECURSIVE_DAG_ARTIFACT_LIMIT as u32),
                include_total: false,
                ..Default::default()
            },
        )
        .await
    {
        Ok(page) => page,
        Err(error) => {
            set_artifact_summary_page_error(
                &mut state,
                graph_id,
                format!("artifact summary load-more unavailable: {}", error),
            );
            return Ok(state);
        }
    };

    append_artifact_summary_page(&mut state, graph_id, page);
    state.loaded_at = Some(Utc::now());
    state.clamp_cursors();
    Ok(state)
}

fn set_artifact_summary_page_error(
    state: &mut RecursiveDagBrowserState,
    graph_id: RecursiveTaskGraphId,
    message: String,
) {
    if let Some(detail) = state
        .detail
        .as_mut()
        .filter(|detail| detail.graph.graph.id == graph_id)
    {
        detail.artifact_summary_page.status = RecursiveDagInspectorLoadStatus::Error;
        detail.artifact_summary_page.message = Some(message.clone());
    }
    state.message = Some(message);
}

fn append_artifact_summary_page(
    state: &mut RecursiveDagBrowserState,
    graph_id: RecursiveTaskGraphId,
    page: rsi_common::RecursiveReadPage<rsi_common::RecursiveExecutionArtifactSummary>,
) {
    let Some(detail) = state
        .detail
        .as_mut()
        .filter(|detail| detail.graph.graph.id == graph_id)
    else {
        state.message =
            Some("artifact summary load-more dropped: selected graph detail is stale".to_string());
        return;
    };

    let page_state = &mut detail.artifact_summary_page;
    let previous_count = page_state.items.len();
    let mut warnings = page_state.warnings.clone();
    warnings.extend(readback_warnings_to_strings(&page.warnings));

    let returned_count = page.items.len();
    let mut incoming = page.items;
    let before_graph_filter = incoming.len();
    incoming.retain(|summary| summary.graph_id == graph_id);
    let graph_dropped = before_graph_filter.saturating_sub(incoming.len());
    if graph_dropped > 0 {
        warnings.push(format!(
            "dropped {graph_dropped} artifact summary row(s) from a different graph"
        ));
    }
    truncate_rows(
        &mut incoming,
        RECURSIVE_DAG_ARTIFACT_LIMIT,
        "artifact summary page",
        &mut warnings,
    );

    let mut seen: HashSet<i64> = page_state
        .items
        .iter()
        .map(|summary| summary.artifact_id)
        .collect();
    let before_dedup = incoming.len();
    incoming.retain(|summary| seen.insert(summary.artifact_id));
    let duplicate_count = before_dedup.saturating_sub(incoming.len());
    if duplicate_count > 0 {
        warnings.push(format!(
            "dropped {duplicate_count} duplicate artifact summary row(s)"
        ));
    }

    let appended_count = incoming.len();
    page_state.items.extend(incoming);
    let loaded_pages = page_state
        .loaded_pages
        .saturating_add(1)
        .max(1)
        .min(RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT);
    page_state.loaded_pages = loaded_pages;

    if page_state.items.len() > RECURSIVE_DAG_ARTIFACT_MAX_LOADED {
        let original = page_state.items.len();
        page_state.items.truncate(RECURSIVE_DAG_ARTIFACT_MAX_LOADED);
        warnings.push(format!(
            "artifact summaries truncated from {original} to {RECURSIVE_DAG_ARTIFACT_MAX_LOADED} loaded row(s)"
        ));
    }

    page_state.status = RecursiveDagInspectorLoadStatus::Ready;
    page_state.limit = page.limit.min(RECURSIVE_DAG_ARTIFACT_LIMIT as u32);
    page_state.next_cursor = page.next_cursor;
    page_state.has_more = page.has_more;
    page_state.page_cap_reached = page_state.has_more
        && (page_state.loaded_pages >= RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT
            || page_state.items.len() >= RECURSIVE_DAG_ARTIFACT_MAX_LOADED);
    if page_state.has_more && page_state.next_cursor.is_none() && !page_state.page_cap_reached {
        warnings.push("artifact summary page has more rows but no next cursor".to_string());
    }
    page_state.message = Some(format!(
        "loaded {appended_count} additional artifact summary row(s); total={}",
        page_state.items.len()
    ));
    page_state.warnings = warnings;
    state.message = Some(format!(
        "loaded artifact summary page returned={returned_count} appended={appended_count} previous={previous_count}"
    ));
}

fn truncate_graph_detail_for_browser(
    graph: &mut RecursiveTaskGraphDetail,
    warnings: &mut Vec<String>,
) {
    if graph.nodes.len() > RECURSIVE_DAG_TASK_LIMIT {
        let original = graph.nodes.len();
        let root_task_id = graph.graph.root_task_id;
        let mut retained = Vec::with_capacity(RECURSIVE_DAG_TASK_LIMIT);
        if let Some(root) = graph.nodes.iter().find(|node| node.id == root_task_id) {
            retained.push(root.clone());
        }
        for node in &graph.nodes {
            if retained.len() >= RECURSIVE_DAG_TASK_LIMIT {
                break;
            }
            if node.id != root_task_id {
                retained.push(node.clone());
            }
        }
        graph.nodes = retained;
        warnings.push(format!(
            "task nodes truncated from {original} to {RECURSIVE_DAG_TASK_LIMIT} rows"
        ));
    }

    let retained_tasks: HashSet<_> = graph.nodes.iter().map(|node| node.id).collect();
    graph.edges.retain(|edge| {
        retained_tasks.contains(&edge.from_task_id) && retained_tasks.contains(&edge.to_task_id)
    });
    graph
        .attempts
        .retain(|attempt| retained_tasks.contains(&attempt.task_id));
    if !graph.artifacts.is_empty() {
        graph.artifacts.clear();
    }

    truncate_rows(
        &mut graph.edges,
        RECURSIVE_DAG_TASK_LIMIT.saturating_mul(2),
        "task edges",
        warnings,
    );
    truncate_rows(
        &mut graph.attempts,
        RECURSIVE_DAG_TASK_LIMIT.saturating_mul(2),
        "task attempts",
        warnings,
    );
    truncate_rows(
        &mut graph.lifecycle_events,
        RECURSIVE_DAG_EVENT_LIMIT,
        "lifecycle events",
        warnings,
    );
    truncate_rows(
        &mut graph.injection_batches,
        RECURSIVE_DAG_EVENT_LIMIT,
        "injection batches",
        warnings,
    );
}

fn truncate_scheduler_run_detail_for_browser(
    detail: &mut RecursiveSchedulerRunDetail,
    warnings: &mut Vec<String>,
) {
    truncate_rows(
        &mut detail.active_attempts,
        RECURSIVE_DAG_TASK_LIMIT,
        "selected run active attempts",
        warnings,
    );
    truncate_rows(
        &mut detail.cancellation_requests,
        RECURSIVE_DAG_LIVE_LIMIT,
        "selected run cancellation requests",
        warnings,
    );
    truncate_rows(
        &mut detail.events,
        RECURSIVE_DAG_EVENT_LIMIT,
        "selected run events",
        warnings,
    );
    truncate_rows(
        &mut detail.live_attempts,
        RECURSIVE_DAG_LIVE_LIMIT,
        "selected run live attempts",
        warnings,
    );
    truncate_rows(
        &mut detail.live_heartbeat_states,
        RECURSIVE_DAG_LIVE_LIMIT,
        "selected run heartbeat states",
        warnings,
    );
    truncate_rows(
        &mut detail.live_interrupts,
        RECURSIVE_DAG_LIVE_LIMIT,
        "selected run live interrupts",
        warnings,
    );
    truncate_rows(
        &mut detail.latest_live_validations,
        RECURSIVE_DAG_LIVE_LIMIT,
        "selected run live validations",
        warnings,
    );
}

fn truncate_rows<T>(rows: &mut Vec<T>, limit: usize, label: &str, warnings: &mut Vec<String>) {
    if rows.len() > limit {
        let original = rows.len();
        rows.truncate(limit);
        warnings.push(format!("{label} truncated from {original} to {limit} rows"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use chrono::Utc;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rsi_common::rpc::{RpcError, RpcRequest, RpcResponse};
    use rsi_common::{
        RecursiveArtifactContentPresence, RecursiveArtifactMetadataState,
        RecursiveArtifactPreviewState, RecursiveCancellationRequestId,
        RecursiveCancellationRequestSource, RecursiveCancellationRequestStatus,
        RecursiveCancellationRequestSummary, RecursiveCancellationScope,
        RecursiveExecutionArtifactKind, RecursiveExecutionArtifactSummary, RecursiveExecutionMode,
        RecursiveGraphRecoveryState, RecursiveGraphRecoveryStatus, RecursiveGraphStatus,
        RecursiveRecoveryPassId, RecursiveSchedulerRunSource, RecursiveSchedulerRunStatus,
        RecursiveSchedulerRunSummary, RecursiveTaskGraphDetail, RecursiveTaskGraphSummary,
        RecursiveTaskLifecycleState, RecursiveTaskNode,
    };
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    fn graph_summary(
        graph_id: RecursiveTaskGraphId,
        root_task_id: rsi_common::RecursiveTaskId,
    ) -> RecursiveTaskGraphSummary {
        let now = Utc::now();
        RecursiveTaskGraphSummary {
            id: graph_id,
            root_task_id,
            title: "graph".to_string(),
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
            max_fanout: 8,
            max_descendants: 256,
            step_limit: 512,
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
        parent_task_id: Option<rsi_common::RecursiveTaskId>,
    ) -> RecursiveTaskNode {
        let now = Utc::now();
        RecursiveTaskNode {
            id: task_id,
            graph_id,
            parent_task_id,
            title: "task".to_string(),
            objective: "objective".to_string(),
            scope: "scope".to_string(),
            acceptance_criteria: Vec::new(),
            depth: u32::from(parent_task_id.is_some()),
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
                nodes: vec![task_node(graph.id, root_task_id, None)],
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

    fn execution_artifact_for_readback(
        graph_id: RecursiveTaskGraphId,
        task_id: rsi_common::RecursiveTaskId,
        id: i64,
        label: &str,
    ) -> rsi_common::RecursiveExecutionArtifact {
        rsi_common::RecursiveExecutionArtifact {
            id,
            graph_id,
            task_id,
            attempt_id: None,
            kind: RecursiveExecutionArtifactKind::File,
            label: label.to_string(),
            content: Some("detail content is not previewed".to_string()),
            uri: Some(format!("file:///tmp/{label}")),
            metadata: serde_json::json!({"label": label}),
            created_at: Utc::now(),
        }
    }

    fn execution_artifact_preview(
        graph_id: RecursiveTaskGraphId,
        id: i64,
        label: &str,
    ) -> rsi_common::RecursiveExecutionArtifactPreview {
        rsi_common::RecursiveExecutionArtifactPreview {
            artifact_id: id,
            graph_id,
            kind: RecursiveExecutionArtifactKind::Inline,
            label: label.to_string(),
            content_state: RecursiveArtifactPreviewState::Available,
            content_type: Some("text/plain".to_string()),
            charset: Some("utf-8".to_string()),
            digest: Some("sha256:abc".to_string()),
            digest_algorithm: Some("sha256".to_string()),
            total_bytes: Some(11),
            total_lines: Some(2),
            byte_range: Some(rsi_common::RecursiveByteRange { start: 0, end: 11 }),
            line_range: Some(rsi_common::RecursiveLineRange { start: 0, end: 2 }),
            shown_bytes: 11,
            shown_lines: 2,
            text: Some("hello\nworld".to_string()),
            binary_unavailable_reason: None,
            truncated: false,
            truncated_by_bytes: false,
            truncated_by_lines: false,
            omitted_bytes: None,
            omitted_lines: None,
            applied_max_bytes: RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_BYTES,
            applied_max_lines: RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_LINES,
            warnings: Vec::new(),
        }
    }

    fn scheduler_run(
        graph_id: RecursiveTaskGraphId,
        status: RecursiveSchedulerRunStatus,
    ) -> RecursiveSchedulerRunSummary {
        RecursiveSchedulerRunSummary {
            id: RecursiveSchedulerRunId::new(),
            graph_id,
            status,
            source: RecursiveSchedulerRunSource::ManualRpc,
            operator: Some("test".to_string()),
            started_at: Utc::now(),
            completed_at: None,
            stop_reason: None,
            step_count: 0,
            max_steps: 10,
            executor_mode: RecursiveExecutionMode::Fake,
            failure_reason: None,
            cancellation_request_id: None,
            cancellation_reason: None,
            lease_owner: None,
            lease_token: None,
            lease_heartbeat_at: None,
            lease_expires_at: None,
            report_artifact_id: None,
        }
    }

    fn recovery_status_with_deferred(
        graph_id: RecursiveTaskGraphId,
        deferred_graph_count: u64,
    ) -> rsi_common::RecursiveDagRecoveryStatus {
        rsi_common::RecursiveDagRecoveryStatus {
            latest_pass: None,
            graph_status: Some(RecursiveGraphRecoveryStatus {
                graph_id: Some(graph_id),
                raw_graph_id: graph_id.to_string(),
                state: RecursiveGraphRecoveryState::Deferred,
                pass_id: Some(RecursiveRecoveryPassId::new()),
                last_attempted_at: Some(Utc::now()),
                completed_at: None,
                deferred_at: Some(Utc::now()),
                reason: Some("retry later".to_string()),
                last_error: Some("transient".to_string()),
                updated_at: Utc::now(),
            }),
            deferred_graphs: if deferred_graph_count > 0 {
                vec![rsi_common::RecursiveDeferredRecoveryGraph {
                    graph_id: Some(graph_id),
                    raw_graph_id: graph_id.to_string(),
                    pass_id: RecursiveRecoveryPassId::new(),
                    state: RecursiveGraphRecoveryState::Deferred,
                    deferred_at: Utc::now(),
                    reason: "retry later".to_string(),
                    next_after: None,
                    last_attempted_at: None,
                    last_error: None,
                }]
            } else {
                Vec::new()
            },
            deferred_graph_count,
            oldest_deferred_graph: None,
        }
    }

    fn cancellation_request_with_status(
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
            reason: "operator reason".to_string(),
            requested_by: Some("rsi-tui".to_string()),
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

    fn ready_state(caps: rsi_common::rpc::DaemonCapabilities) -> RecursiveDagBrowserState {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        ready_state_for(graph_id, root_task_id, caps)
    }

    fn ready_state_for(
        graph_id: RecursiveTaskGraphId,
        root_task_id: rsi_common::RecursiveTaskId,
        caps: rsi_common::rpc::DaemonCapabilities,
    ) -> RecursiveDagBrowserState {
        let graph = graph_summary(graph_id, root_task_id);
        RecursiveDagBrowserState::ready(
            None,
            caps,
            vec![graph.clone()],
            Some(graph_id),
            Some(selected_detail(graph, root_task_id)),
        )
    }

    fn app_with_state(state: RecursiveDagBrowserState) -> App {
        app_with_state_and_socket(state, PathBuf::from("/tmp/test.sock"))
    }

    fn app_with_state_and_socket(state: RecursiveDagBrowserState, socket_path: PathBuf) -> App {
        let project_id = state.project_id;
        let mut app = App::new(DaemonClient::new(socket_path));
        app.current_project_id = project_id;
        app.poll.connected = true;
        app.overlay = OverlayState::RecursiveDagBrowser(state);
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            handle_recursive_dag_key(app, key(KeyCode::Char(ch)));
        }
    }

    fn browser_state(app: &App) -> &RecursiveDagBrowserState {
        let OverlayState::RecursiveDagBrowser(state) = &app.overlay else {
            panic!("expected recursive DAG browser");
        };
        state
    }

    fn rpc_socket_path(label: &str) -> PathBuf {
        crate::test_support::short_socket_path(label)
    }

    fn spawn_rpc_server<F>(
        socket_path: PathBuf,
        mut responder: F,
    ) -> tokio::task::JoinHandle<Vec<RpcRequest>>
    where
        F: FnMut(&RpcRequest) -> serde_json::Value + Send + 'static,
    {
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("bind rpc test socket");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept rpc client");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let mut requests = Vec::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await.expect("read rpc line");
                if n == 0 {
                    break;
                }
                let request: RpcRequest =
                    serde_json::from_str(line.trim()).expect("decode rpc request");
                let result = responder(&request);
                let response = RpcResponse::success(request.id.clone(), result);
                let encoded = serde_json::to_string(&response).expect("encode rpc response");
                reader
                    .get_mut()
                    .write_all(format!("{encoded}\n").as_bytes())
                    .await
                    .expect("write rpc response");
                requests.push(request);
            }
            requests
        })
    }

    fn spawn_rpc_server_with_replies<F>(
        socket_path: PathBuf,
        expected_requests: usize,
        responder: F,
    ) -> tokio::task::JoinHandle<Vec<RpcRequest>>
    where
        F: FnMut(&RpcRequest) -> std::result::Result<serde_json::Value, RpcError> + Send + 'static,
    {
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("bind rpc test socket");
        let responder = std::sync::Arc::new(tokio::sync::Mutex::new(responder));
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut requests = Vec::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.expect("accept rpc client");
                        let request_tx = request_tx.clone();
                        let responder = std::sync::Arc::clone(&responder);
                        tokio::spawn(async move {
                            let mut reader = BufReader::new(stream);
                            let mut line = String::new();
                            loop {
                                line.clear();
                                let n = reader.read_line(&mut line).await.expect("read rpc line");
                                if n == 0 {
                                    break;
                                }
                                let request: RpcRequest =
                                    serde_json::from_str(line.trim()).expect("decode rpc request");
                                let response = {
                                    let mut responder = responder.lock().await;
                                    match responder(&request) {
                                        Ok(result) => RpcResponse::success(request.id.clone(), result),
                                        Err(error) => RpcResponse::error(request.id.clone(), error),
                                    }
                                };
                                let encoded = serde_json::to_string(&response).expect("encode rpc response");
                                reader
                                    .get_mut()
                                    .write_all(format!("{encoded}\n").as_bytes())
                                    .await
                                    .expect("write rpc response");
                                let _ = request_tx.send(request);
                            }
                        });
                    }
                    Some(request) = request_rx.recv() => {
                        requests.push(request);
                        if requests.len() >= expected_requests {
                            break;
                        }
                    }
                }
            }
            requests
        })
    }

    fn assert_no_mutation_or_control_rpcs(requests: &[RpcRequest]) {
        let blocked = [
            "RunRecursiveFakeScheduler",
            "RequestRecursiveGraphCancellation",
            "RequestRecursiveSchedulerRunCancellation",
            "ContinueRecursiveRecovery",
            "ContinueTopologyRecursiveRecovery",
        ];
        for request in requests {
            assert!(
                !blocked.contains(&request.method.as_str()),
                "unexpected mutation/control RPC {}",
                request.method
            );
        }
    }

    fn assert_only_fake_scheduler_control_rpc(requests: &[RpcRequest]) {
        let control_methods = [
            "RunRecursiveFakeScheduler",
            "RunRecursiveDagFakeScheduler",
            "RequestRecursiveGraphCancellation",
            "RequestRecursiveSchedulerRunCancellation",
            "ContinueRecursiveRecovery",
            "ContinueTopologyRecursiveRecovery",
        ];
        let actual = requests
            .iter()
            .filter_map(|request| {
                control_methods
                    .contains(&request.method.as_str())
                    .then_some(request.method.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, vec!["RunRecursiveFakeScheduler"]);
    }

    fn assert_message_contains(app: &App, expected: &str) {
        let state = browser_state(app);
        assert!(
            state
                .message
                .as_deref()
                .is_some_and(|message| message.contains(expected)),
            "message was {:?}",
            state.message
        );
    }

    #[test]
    fn fake_run_key_renders_unavailable_without_scheduler_capability() {
        let caps = rsi_common::rpc::DaemonCapabilities::default();
        let mut app = app_with_state(ready_state(caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('R')));

        let OverlayState::RecursiveDagBrowser(state) = &app.overlay else {
            panic!("expected recursive DAG browser");
        };
        assert!(matches!(state.fake_run, RecursiveDagFakeRunState::Idle));
        assert!(
            state
                .message
                .as_deref()
                .is_some_and(|message| message.contains("FAKE scheduler unavailable"))
        );
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn cancellation_capability_disabled_does_not_open_prompt_or_mutate() {
        let caps = rsi_common::rpc::DaemonCapabilities::default();
        let mut app = app_with_state(ready_state(caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('C')));

        let state = browser_state(&app);
        assert!(matches!(state.control, RecursiveDagControlState::Idle));
        assert_message_contains(&app, "recursive_dag_cancellation_control=false");
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn cancellation_prefix_does_not_open_for_terminal_graph() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_cancellation_control: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let mut graph = graph_summary(graph_id, root_task_id);
        graph.status = RecursiveGraphStatus::Terminal;
        let state = RecursiveDagBrowserState::ready(
            None,
            caps,
            vec![graph.clone()],
            Some(graph_id),
            Some(selected_detail(graph, root_task_id)),
        );
        let mut app = app_with_state(state);

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('C')));

        assert!(matches!(
            browser_state(&app).control,
            RecursiveDagControlState::Idle
        ));
        assert_message_contains(&app, "selected graph is not cancellable");
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn recovery_capability_disabled_does_not_open_prompt_or_mutate() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_recovery_status: true,
            ..Default::default()
        };
        let mut app = app_with_state(ready_state(caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('c')));

        let state = browser_state(&app);
        assert!(matches!(state.control, RecursiveDagControlState::Idle));
        assert_message_contains(&app, "recursive_dag_recovery_control=false");
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn disconnected_controls_do_not_open_prompts_or_mutate() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_cancellation_control: true,
            recursive_dag_recovery_control: true,
            recursive_dag_recovery_status: true,
            ..Default::default()
        };
        let mut app = app_with_state(ready_state(caps));
        app.poll.connected = false;

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('C')));
        assert_message_contains(&app, "daemon is not connected");
        assert!(matches!(
            browser_state(&app).control,
            RecursiveDagControlState::Idle
        ));
        assert!(app.recursive_dag_rx.is_none());

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('c')));
        assert_message_contains(&app, "daemon is not connected");
        assert!(matches!(
            browser_state(&app).control,
            RecursiveDagControlState::Idle
        ));
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn artifact_prefix_keys_change_selection_without_rpc_or_controls() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
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
            rsi_common::rpc::DaemonCapabilities::default(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        state.panel = crate::types::RecursiveDagPanel::Artifacts;
        let mut app = app_with_state(state);

        handle_recursive_dag_key(&mut app, key(KeyCode::Char(']')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Char('a')));

        let state = browser_state(&app);
        assert_eq!(state.selected_artifact, 1);
        assert!(matches!(state.control, RecursiveDagControlState::Idle));
        assert!(matches!(state.fake_run, RecursiveDagFakeRunState::Idle));
        assert!(app.recursive_dag_rx.is_none());

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('[')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Char('a')));

        assert_eq!(browser_state(&app).selected_artifact, 0);
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn degraded_inspector_keys_do_not_call_preview_test_diff_or_report_rpcs() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "artifact",
        ));
        let mut state = RecursiveDagBrowserState::ready(
            None,
            rsi_common::rpc::DaemonCapabilities::default(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        state.panel = crate::types::RecursiveDagPanel::Artifacts;
        let mut app = app_with_state(state);

        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));
        for key_code in [
            KeyCode::Char('p'),
            KeyCode::Char('m'),
            KeyCode::Char('l'),
            KeyCode::Char('t'),
            KeyCode::Char('d'),
        ] {
            handle_recursive_dag_key(&mut app, key(key_code));
        }

        let state = browser_state(&app);
        assert!(state.inspector.open);
        assert!(matches!(state.control, RecursiveDagControlState::Idle));
        assert!(matches!(state.fake_run, RecursiveDagFakeRunState::Idle));
        assert!(app.recursive_dag_rx.is_none());
        assert_message_contains(&app, "typed diff detail unavailable");
    }

    #[test]
    fn preview_capability_disabled_sets_local_state_without_rpc() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "artifact",
        ));
        let mut state = RecursiveDagBrowserState::ready(
            None,
            rsi_common::rpc::DaemonCapabilities::default(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        state.panel = crate::types::RecursiveDagPanel::Artifacts;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        let mut app = app_with_state(state);

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('p')));

        let state = browser_state(&app);
        assert_eq!(state.inspector.view, RecursiveDagInspectorView::Preview);
        assert!(matches!(
            state
                .inspector
                .artifact_preview
                .as_ref()
                .map(|preview| preview.status),
            Some(RecursiveDagInspectorLoadStatus::CapabilityDisabled)
        ));
        assert!(app.recursive_dag_rx.is_none());
    }

    #[tokio::test]
    async fn preview_capability_enabled_requests_selected_artifact_with_bounds() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            42,
            "artifact",
        ));
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_artifact_preview_inspection: true,
            ..Default::default()
        };
        let preview = execution_artifact_preview(graph_id, 42, "artifact");
        let socket_path = rpc_socket_path("artifact-preview");
        let server_caps = caps.clone();
        let server = spawn_rpc_server(socket_path.clone(), move |request| {
            match request.method.as_str() {
                "GetDaemonCapabilities" => serde_json::to_value(&server_caps).unwrap(),
                "PreviewRecursiveExecutionArtifact" => serde_json::to_value(&preview).unwrap(),
                other => panic!("unexpected RPC {other}"),
            }
        });
        let mut state = RecursiveDagBrowserState::ready(
            None,
            caps.clone(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        state.panel = crate::types::RecursiveDagPanel::Artifacts;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        let mut app = App::new(DaemonClient::new(socket_path));
        app.current_project_id = state.project_id;
        app.poll.connected = true;
        app.overlay = OverlayState::RecursiveDagBrowser(state);

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('p')));

        let rx = app.recursive_dag_rx.take().expect("preview receiver");
        let loaded = rx
            .await
            .expect("preview task")
            .expect("preview state should load");
        let RecursiveDagAsyncResult::Browser(loaded) = loaded else {
            panic!("expected browser result");
        };
        let requests = server.await.unwrap();

        let preview_state = loaded
            .inspector
            .artifact_preview
            .expect("artifact preview state");
        assert_eq!(preview_state.status, RecursiveDagInspectorLoadStatus::Ready);
        assert_eq!(preview_state.graph_id, graph_id);
        assert_eq!(preview_state.artifact_id, 42);
        let preview_request = requests
            .iter()
            .find(|request| request.method == "PreviewRecursiveExecutionArtifact")
            .expect("preview RPC");
        assert_eq!(
            preview_request.params["graph_id"],
            serde_json::json!(graph_id)
        );
        assert_eq!(preview_request.params["artifact_id"], serde_json::json!(42));
        assert_eq!(
            preview_request.params["max_bytes"],
            serde_json::json!(RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_BYTES)
        );
        assert_eq!(
            preview_request.params["max_lines"],
            serde_json::json!(RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_LINES)
        );
        assert_eq!(
            preview_request.params["require_complete"],
            serde_json::json!(false)
        );
        assert_no_mutation_or_control_rpcs(&requests);
    }

    #[test]
    fn recursive_dag_keybinding_docs_include_shipped_inspector_keys() {
        let docs = include_str!("../../../../docs/keybindings.md");
        for needle in [
            "`]a` / `[a`",
            "`Enter`",
            "`p`",
            "`m`",
            "`l`",
            "`t`",
            "`d`",
            "load-more",
            "bounded artifact preview unavailable",
            "capability-disabled",
        ] {
            assert!(docs.contains(needle), "docs missing {needle}");
        }
    }

    #[test]
    fn cancellation_prefix_reserves_task_scope_without_mutation() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_cancellation_control: true,
            ..Default::default()
        };
        let mut app = app_with_state(ready_state(caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('C')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Char('t')));

        assert!(matches!(
            browser_state(&app).control,
            RecursiveDagControlState::Idle
        ));
        assert_message_contains(
            &app,
            "task-scoped cancellation needs daemon execution semantics",
        );
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn graph_cancellation_reason_prompt_rejects_empty_and_over_cap_before_rpc() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_cancellation_control: true,
            ..Default::default()
        };
        let mut app = app_with_state(ready_state(caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('C')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Char('g')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));

        match &browser_state(&app).control {
            RecursiveDagControlState::GraphReasonInput { error, .. } => {
                assert!(
                    error
                        .as_deref()
                        .is_some_and(|error| error.contains("required"))
                );
            }
            other => panic!("expected graph reason input, got {other:?}"),
        }
        assert!(app.recursive_dag_rx.is_none());

        type_text(
            &mut app,
            &"x".repeat(crate::types::RECURSIVE_DAG_CONTROL_REASON_LIMIT + 1),
        );
        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));

        match &browser_state(&app).control {
            RecursiveDagControlState::GraphReasonInput { error, .. } => {
                assert!(error.as_deref().is_some_and(|error| error.contains("240")));
            }
            other => panic!("expected graph reason input, got {other:?}"),
        }
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn run_cancellation_terminal_selected_row_is_disabled_without_retargeting() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_cancellation_control: true,
            recursive_dag_run_inspection: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.scheduler_runs = vec![
            scheduler_run(graph_id, RecursiveSchedulerRunStatus::Completed),
            scheduler_run(graph_id, RecursiveSchedulerRunStatus::Running),
        ];
        let mut state =
            RecursiveDagBrowserState::ready(None, caps, vec![graph], Some(graph_id), Some(detail));
        state.panel = crate::types::RecursiveDagPanel::Runs;
        state.selected_run = 0;
        let mut app = app_with_state(state);

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('C')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Char('r')));

        assert!(matches!(
            browser_state(&app).control,
            RecursiveDagControlState::Idle
        ));
        assert_message_contains(&app, "selected run");
        assert_message_contains(&app, "terminal");
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn recovery_continuation_is_disabled_without_visible_deferred_work() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_recovery_control: true,
            recursive_dag_recovery_status: true,
            ..Default::default()
        };
        let mut app = app_with_state(ready_state(caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('c')));

        assert!(matches!(
            browser_state(&app).control,
            RecursiveDagControlState::Idle
        ));
        assert_message_contains(&app, "no deferred recovery work is visible");
        assert!(app.recursive_dag_rx.is_none());
    }

    #[tokio::test]
    async fn graph_cancellation_submits_with_requested_by_and_refresh_receiver() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_cancellation_control: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let mut app = app_with_state(ready_state_for(graph_id, root_task_id, caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('C')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Char('g')));
        type_text(&mut app, "  stop graph  ");
        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));

        assert!(matches!(
            browser_state(&app).control,
            RecursiveDagControlState::Submitting {
                kind: RecursiveDagControlKind::GraphCancellation,
                ..
            }
        ));
        assert_message_contains(&app, "requested_by=rsi-tui");
        assert!(app.recursive_dag_rx.is_some());
    }

    #[tokio::test]
    async fn run_cancellation_submits_selected_run_and_refresh_receiver() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_cancellation_control: true,
            recursive_dag_run_inspection: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        let run = scheduler_run(graph_id, RecursiveSchedulerRunStatus::Running);
        let run_id = run.id;
        detail.scheduler_runs = vec![run];
        let mut state =
            RecursiveDagBrowserState::ready(None, caps, vec![graph], Some(graph_id), Some(detail));
        state.panel = crate::types::RecursiveDagPanel::Runs;
        let mut app = app_with_state(state);

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('C')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Char('r')));
        type_text(&mut app, "stop run");
        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));

        assert!(matches!(
            browser_state(&app).control,
            RecursiveDagControlState::Submitting {
                kind: RecursiveDagControlKind::RunCancellation,
                ref target,
            } if target == &run_id.to_string()
        ));
        assert!(app.recursive_dag_rx.is_some());
    }

    #[tokio::test]
    async fn recovery_continuation_submits_time_budget_zero_and_refresh_receiver() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_recovery_control: true,
            recursive_dag_recovery_status: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.recovery_status = Some(recovery_status_with_deferred(graph_id, 1));
        let state =
            RecursiveDagBrowserState::ready(None, caps, vec![graph], Some(graph_id), Some(detail));
        let mut app = app_with_state(state);

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('c')));
        type_text(&mut app, "1");
        handle_recursive_dag_key(&mut app, key(KeyCode::Tab));
        type_text(&mut app, "0");
        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));

        assert!(matches!(
            browser_state(&app).control,
            RecursiveDagControlState::Submitting {
                kind: RecursiveDagControlKind::RecoveryContinuation,
                ..
            }
        ));
        assert_message_contains(&app, "time_budget_ms=0");
        assert!(app.recursive_dag_rx.is_some());
    }

    #[test]
    fn terminal_cancellation_response_messages_render_as_warnings() {
        let graph_id = RecursiveTaskGraphId::new();
        let applied =
            cancellation_request_with_status(graph_id, RecursiveCancellationRequestStatus::Applied);
        let rejected = cancellation_request_with_status(
            graph_id,
            RecursiveCancellationRequestStatus::Rejected,
        );

        let applied_message = cancellation_request_result_message("graph cancellation", &applied);
        let rejected_message = cancellation_request_result_message("run cancellation", &rejected);

        assert!(applied_message.contains("warning"));
        assert!(applied_message.contains("status=Applied"));
        assert!(applied_message.contains("requested_by=rsi-tui"));
        assert!(rejected_message.contains("warning"));
        assert!(rejected_message.contains("status=Rejected"));
        assert!(rejected_message.contains("already terminal"));
    }

    #[test]
    fn successful_control_refresh_clears_submitting_state_and_preserves_selection() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_cancellation_control: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let mut previous = ready_state_for(graph_id, root_task_id, caps.clone());
        previous.control = RecursiveDagControlState::Submitting {
            kind: RecursiveDagControlKind::GraphCancellation,
            target: graph_id.to_string(),
        };
        let mut app = app_with_state(previous);
        let mut refreshed = ready_state_for(graph_id, root_task_id, caps);
        refreshed.message =
            Some("graph cancellation requested: request=abc status=Requested".to_string());
        if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
            state.panel = crate::types::RecursiveDagPanel::Tasks;
        }

        apply_recursive_dag_load_result(&mut app, Ok(refreshed.into()));

        let state = browser_state(&app);
        assert!(matches!(state.control, RecursiveDagControlState::Idle));
        assert_eq!(state.selected_graph_id(), Some(graph_id));
        assert_eq!(state.panel, crate::types::RecursiveDagPanel::Tasks);
        assert!(
            app.recursive_dag_cache
                .as_ref()
                .is_some_and(|state| state.panel == crate::types::RecursiveDagPanel::Tasks)
        );
        assert_message_contains(&app, "graph cancellation requested");
    }

    #[tokio::test]
    async fn inspector_readback_errors_stay_local_to_inspector_state() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let live_attempt_id = RecursiveLiveAttemptId::new();
        let socket_path = std::env::temp_dir().join(format!(
            "rsi-missing-inspector-{}.sock",
            uuid::Uuid::new_v4()
        ));

        let validation_state = load_validation_detail_into_state(
            socket_path.clone(),
            None,
            ready_state_for(
                graph_id,
                root_task_id,
                rsi_common::rpc::DaemonCapabilities::default(),
            ),
            None,
            live_attempt_id,
        )
        .await
        .expect("validation error should stay local");

        assert!(matches!(
            validation_state.load_status,
            RecursiveDagLoadStatus::Ready
        ));
        assert!(matches!(
            validation_state
                .inspector
                .validation_detail
                .as_ref()
                .map(|detail| detail.status),
            Some(RecursiveDagInspectorLoadStatus::Error)
        ));

        let bucket_state = load_live_attempt_artifacts_into_state(
            socket_path,
            None,
            ready_state_for(
                graph_id,
                root_task_id,
                rsi_common::rpc::DaemonCapabilities::default(),
            ),
            graph_id,
            live_attempt_id,
        )
        .await
        .expect("live bucket error should stay local");

        assert!(matches!(
            bucket_state.load_status,
            RecursiveDagLoadStatus::Ready
        ));
        assert!(matches!(
            bucket_state
                .inspector
                .live_attempt_artifacts
                .as_ref()
                .map(|artifacts| artifacts.status),
            Some(RecursiveDagInspectorLoadStatus::Error)
        ));

        let preview_state = load_artifact_preview_into_state(
            std::env::temp_dir().join(format!("rsi-missing-preview-{}.sock", uuid::Uuid::new_v4())),
            None,
            ready_state_for(
                graph_id,
                root_task_id,
                rsi_common::rpc::DaemonCapabilities {
                    recursive_dag_artifact_preview_inspection: true,
                    ..Default::default()
                },
            ),
            graph_id,
            77,
        )
        .await
        .expect("preview error should stay local");

        assert!(matches!(
            preview_state.load_status,
            RecursiveDagLoadStatus::Ready
        ));
        assert!(matches!(
            preview_state
                .inspector
                .artifact_preview
                .as_ref()
                .map(|preview| preview.status),
            Some(RecursiveDagInspectorLoadStatus::Error)
        ));
    }

    #[tokio::test]
    async fn selected_graph_artifact_summary_loading_uses_graph_guarded_page() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let graph_detail = selected_detail(graph.clone(), root_task_id).graph;
        let summary = artifact_summary(graph_id, root_task_id, 17, "summary");
        let page = rsi_common::RecursiveReadPage {
            items: vec![summary.clone()],
            limit: RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            next_cursor: Some("next".to_string()),
            has_more: true,
            total_count: None,
            warnings: Vec::new(),
        };
        let socket_path = rpc_socket_path("artifact-summary-page");
        let server = spawn_rpc_server(socket_path.clone(), move |request| {
            match request.method.as_str() {
                "GetRecursiveTaskGraph" => serde_json::to_value(&graph_detail).unwrap(),
                "GetRecursiveDagOperationalStatus" => serde_json::Value::Null,
                "ListRecursiveSchedulerRuns" => serde_json::json!([]),
                "GetRecursiveRecoveryStatus" => serde_json::Value::Null,
                "ListStaleRecursiveLiveAttemptHeartbeats" => serde_json::json!([]),
                "GetRecursiveLiveRecoveryStatus" => serde_json::Value::Null,
                "ListRecursiveLiveAttempts" => serde_json::json!([]),
                "ListRecursiveLiveOutputValidationResults" => serde_json::json!([]),
                "ListRecursiveCancellationRequests" => serde_json::json!([]),
                "ListRecursiveExecutionArtifactSummaries" => serde_json::to_value(&page).unwrap(),
                other => panic!("unexpected RPC {other}"),
            }
        });
        let mut client = DaemonClient::new(socket_path);
        client.connect().await.unwrap();
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_artifact_list_pagination: true,
            ..Default::default()
        };

        let detail = load_selected_graph(&mut client, graph_id, &caps)
            .await
            .expect("selected graph should load");
        drop(client);
        let requests = server.await.unwrap();

        assert_eq!(
            detail.artifact_summary_page.status,
            RecursiveDagInspectorLoadStatus::Ready
        );
        assert_eq!(detail.artifact_summary_page.items, vec![summary]);
        assert!(detail.artifact_summary_page.has_more);
        let list_request = requests
            .iter()
            .find(|request| request.method == "ListRecursiveExecutionArtifactSummaries")
            .expect("summary list RPC");
        assert_eq!(list_request.params["graph_id"], serde_json::json!(graph_id));
        assert_eq!(
            list_request.params["limit"],
            serde_json::json!(RECURSIVE_DAG_ARTIFACT_LIMIT as u32)
        );
        assert_eq!(
            list_request.params["include_total"],
            serde_json::json!(false)
        );
        assert!(
            requests
                .iter()
                .all(|request| request.method != "ListRecursiveExecutionArtifacts")
        );
        assert_no_mutation_or_control_rpcs(&requests);
    }

    #[tokio::test]
    async fn artifact_summary_load_more_uses_graph_cursor_and_page_bounds() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page = RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            vec![artifact_summary(graph_id, root_task_id, 1, "first")],
            RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );
        let mut caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_artifact_list_pagination: true,
            ..Default::default()
        };
        caps.recursive_dag_artifact_lookup = true;
        let mut state =
            RecursiveDagBrowserState::ready(None, caps, vec![graph], Some(graph_id), Some(detail));
        state.panel = crate::types::RecursiveDagPanel::Artifacts;
        state.selected_artifact = state
            .inspector_rows()
            .iter()
            .position(|row| {
                row.artifact_page_action() == Some(RecursiveDagArtifactPageAction::LoadMore)
            })
            .expect("load-more row");

        let next_summary = artifact_summary(graph_id, root_task_id, 2, "second");
        let page = rsi_common::RecursiveReadPage {
            items: vec![next_summary.clone()],
            limit: RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            next_cursor: Some("cursor-2".to_string()),
            has_more: true,
            total_count: None,
            warnings: Vec::new(),
        };
        let socket_path = rpc_socket_path("artifact-summary-load-more");
        let server = spawn_rpc_server(socket_path.clone(), move |request| {
            match request.method.as_str() {
                "ListRecursiveExecutionArtifactSummaries" => serde_json::to_value(&page).unwrap(),
                other => panic!("unexpected RPC {other}"),
            }
        });
        let mut app = app_with_state_and_socket(state, socket_path);

        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));

        assert!(app.recursive_dag_rx.is_some());
        let result = app
            .recursive_dag_rx
            .take()
            .expect("load-more receiver")
            .await
            .expect("load-more task")
            .expect("load-more state");
        apply_recursive_dag_load_result(&mut app, Ok(result));
        let requests = server.await.unwrap();

        let list_request = requests
            .iter()
            .find(|request| request.method == "ListRecursiveExecutionArtifactSummaries")
            .expect("summary list RPC");
        assert_eq!(list_request.params["graph_id"], serde_json::json!(graph_id));
        assert_eq!(list_request.params["cursor"], serde_json::json!("cursor-1"));
        assert_eq!(
            list_request.params["limit"],
            serde_json::json!(RECURSIVE_DAG_ARTIFACT_LIMIT as u32)
        );
        assert_eq!(
            list_request.params["include_total"],
            serde_json::json!(false)
        );
        assert_no_mutation_or_control_rpcs(&requests);

        let state = browser_state(&app);
        let detail = state.selected_detail().expect("selected detail");
        assert_eq!(detail.artifact_summary_page.items.len(), 2);
        assert_eq!(detail.artifact_summary_page.items[1], next_summary);
        assert_eq!(detail.artifact_summary_page.loaded_pages, 2);
        assert!(detail.artifact_summary_page.has_more);
    }

    #[test]
    fn stale_artifact_summary_page_result_is_dropped_when_graph_changes() {
        let graph_a = RecursiveTaskGraphId::new();
        let graph_b = RecursiveTaskGraphId::new();
        let root_a = rsi_common::RecursiveTaskId::new();
        let root_b = rsi_common::RecursiveTaskId::new();
        let summary_a = graph_summary(graph_a, root_a);
        let summary_b = graph_summary(graph_b, root_b);
        let mut detail_a = selected_detail(summary_a.clone(), root_a);
        detail_a.artifact_summary_page.items = vec![artifact_summary(graph_a, root_a, 1, "first")];
        let mut state = RecursiveDagBrowserState::ready(
            None,
            rsi_common::rpc::DaemonCapabilities {
                recursive_dag_artifact_list_pagination: true,
                ..Default::default()
            },
            vec![summary_a, summary_b],
            Some(graph_a),
            Some(detail_a.clone()),
        );
        state.selected_graph = 1;
        state.panel = crate::types::RecursiveDagPanel::Artifacts;
        let mut app = app_with_state(state);

        let mut loaded = browser_state(&app).clone();
        loaded.selected_graph = 0;
        loaded.loaded_graph_id = Some(graph_a);
        loaded.detail = Some(detail_a);
        if let Some(detail) = loaded.detail.as_mut() {
            detail
                .artifact_summary_page
                .items
                .push(artifact_summary(graph_a, root_a, 2, "stale"));
        }

        apply_recursive_dag_load_result(
            &mut app,
            Ok(RecursiveDagAsyncResult::ArtifactSummaryPage {
                graph_id: graph_a,
                state: loaded,
            }),
        );

        let state = browser_state(&app);
        assert_eq!(state.selected_graph_id(), Some(graph_b));
        assert!(state.selected_detail().is_none());
        assert!(
            state
                .message
                .as_deref()
                .is_some_and(|message| message.contains("selected graph changed"))
        );
    }

    #[tokio::test]
    async fn artifact_summary_load_more_error_stays_local_to_artifact_page() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page = RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            vec![artifact_summary(graph_id, root_task_id, 1, "first")],
            RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );
        let state = RecursiveDagBrowserState::ready(
            None,
            rsi_common::rpc::DaemonCapabilities {
                recursive_dag_artifact_list_pagination: true,
                ..Default::default()
            },
            vec![graph],
            Some(graph_id),
            Some(detail),
        );

        let loaded = load_more_artifact_summaries_into_state(
            std::env::temp_dir().join(format!(
                "rsi-missing-artifact-page-{}.sock",
                uuid::Uuid::new_v4()
            )),
            None,
            state,
            graph_id,
            "cursor-1".to_string(),
        )
        .await
        .expect("load-more error remains a browser state");

        assert!(matches!(loaded.load_status, RecursiveDagLoadStatus::Ready));
        let page = &loaded.selected_detail().unwrap().artifact_summary_page;
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.status, RecursiveDagInspectorLoadStatus::Error);
        assert!(page.has_more);
        assert_eq!(page.next_cursor.as_deref(), Some("cursor-1"));
    }

    #[test]
    fn artifact_summary_page_cap_blocks_additional_load_more_rpc() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page = RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            vec![artifact_summary(graph_id, root_task_id, 1, "first")],
            RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );
        detail.artifact_summary_page.loaded_pages = RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT;
        detail.artifact_summary_page.page_cap_reached = true;
        let mut state = RecursiveDagBrowserState::ready(
            None,
            rsi_common::rpc::DaemonCapabilities {
                recursive_dag_artifact_list_pagination: true,
                ..Default::default()
            },
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        state.panel = crate::types::RecursiveDagPanel::Artifacts;
        state.selected_artifact = state
            .inspector_rows()
            .iter()
            .position(|row| {
                row.artifact_page_action() == Some(RecursiveDagArtifactPageAction::PageCap)
            })
            .expect("page cap row");
        let mut app = app_with_state(state);

        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));

        assert!(app.recursive_dag_rx.is_none());
        assert_message_contains(&app, "page-cap");
    }

    #[test]
    fn artifact_summary_page_append_preserves_selected_artifact_and_preview() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page = RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            vec![
                artifact_summary(graph_id, root_task_id, 1, "first"),
                artifact_summary(graph_id, root_task_id, 2, "second"),
            ],
            RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );
        let mut state = RecursiveDagBrowserState::ready(
            None,
            rsi_common::rpc::DaemonCapabilities {
                recursive_dag_artifact_list_pagination: true,
                ..Default::default()
            },
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        state.panel = crate::types::RecursiveDagPanel::Artifacts;
        state.selected_artifact = 1;
        state
            .inspector
            .open_for(state.selected_inspector_row().as_ref());
        state.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState {
            graph_id,
            artifact_id: 2,
            status: RecursiveDagInspectorLoadStatus::Ready,
            preview: Some(execution_artifact_preview(graph_id, 2, "second")),
            message: Some("loaded bounded artifact preview 2".to_string()),
            warnings: Vec::new(),
        });
        let mut app = app_with_state(state.clone());
        let mut loaded = state;
        if let Some(detail) = loaded.detail.as_mut() {
            detail.artifact_summary_page.items.push(artifact_summary(
                graph_id,
                root_task_id,
                3,
                "third",
            ));
            detail.artifact_summary_page.loaded_pages = 2;
            detail.artifact_summary_page.next_cursor = None;
            detail.artifact_summary_page.has_more = false;
        }

        apply_recursive_dag_load_result(
            &mut app,
            Ok(RecursiveDagAsyncResult::ArtifactSummaryPage {
                graph_id,
                state: loaded,
            }),
        );

        let state = browser_state(&app);
        assert_eq!(state.selected_artifact, 1);
        assert_eq!(
            state
                .selected_inspector_row()
                .and_then(|row| row.artifact_id()),
            Some(2)
        );
        assert!(state.inspector.open);
        assert!(state.inspector.artifact_preview.is_some());
        assert_eq!(
            state
                .selected_detail()
                .unwrap()
                .artifact_summary_page
                .items
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn artifact_detail_loading_uses_graph_and_artifact_id_only() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            42,
            "artifact",
        ));
        let mut caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_artifact_lookup: true,
            ..Default::default()
        };
        let state = RecursiveDagBrowserState::ready(
            None,
            caps.clone(),
            vec![graph],
            Some(graph_id),
            Some(detail),
        );
        let readback = rsi_common::RecursiveExecutionArtifactReadback {
            artifact: execution_artifact_for_readback(graph_id, root_task_id, 42, "artifact"),
            role: None,
            provenance: rsi_common::RecursiveArtifactProvenance::DaemonRecorded,
            owners: rsi_common::RecursiveArtifactOwners {
                graph_id,
                task_id: root_task_id,
                attempt_id: None,
                live_attempt_id: None,
                scheduler_run_id: None,
                validation_id: None,
            },
            preview: rsi_common::RecursiveArtifactPreviewAvailability {
                state: RecursiveArtifactPreviewState::Unavailable,
                reason: None,
            },
            metadata_state: RecursiveArtifactMetadataState::Valid,
            warnings: Vec::new(),
        };
        caps.recursive_dag_artifact_lookup = true;
        let socket_path = rpc_socket_path("artifact-detail");
        let server = spawn_rpc_server(socket_path.clone(), move |request| {
            match request.method.as_str() {
                "GetDaemonCapabilities" => serde_json::to_value(&caps).unwrap(),
                "GetRecursiveExecutionArtifact" => serde_json::to_value(&readback).unwrap(),
                other => panic!("unexpected RPC {other}"),
            }
        });

        let loaded = load_artifact_detail_into_state(socket_path, None, state, graph_id, 42)
            .await
            .expect("artifact detail should load");
        let requests = server.await.unwrap();

        let detail = loaded
            .inspector
            .artifact_detail
            .expect("artifact detail state");
        assert_eq!(detail.status, RecursiveDagInspectorLoadStatus::Ready);
        assert_eq!(detail.graph_id, graph_id);
        assert_eq!(detail.artifact_id, 42);
        assert_eq!(detail.readback.unwrap().artifact.id, 42);
        let lookup = requests
            .iter()
            .find(|request| request.method == "GetRecursiveExecutionArtifact")
            .expect("artifact lookup RPC");
        assert_eq!(lookup.params["graph_id"], serde_json::json!(graph_id));
        assert_eq!(lookup.params["artifact_id"], serde_json::json!(42));
        assert_eq!(lookup.params["include_links"], serde_json::json!(true));
        assert_eq!(lookup.params.as_object().unwrap().len(), 3);
        assert_no_mutation_or_control_rpcs(&requests);
    }

    #[test]
    fn stale_artifact_detail_is_dropped_when_selected_row_changes() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut previous_detail = selected_detail(graph.clone(), root_task_id);
        previous_detail
            .artifact_summary_page
            .items
            .push(artifact_summary(graph_id, root_task_id, 1, "first"));
        previous_detail
            .artifact_summary_page
            .items
            .push(artifact_summary(graph_id, root_task_id, 2, "second"));
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_artifact_lookup: true,
            ..Default::default()
        };
        let mut app = app_with_state(RecursiveDagBrowserState::ready(
            None,
            caps.clone(),
            vec![graph.clone()],
            Some(graph_id),
            Some(previous_detail.clone()),
        ));
        if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
            state.panel = crate::types::RecursiveDagPanel::Artifacts;
            state.selected_artifact = 1;
        }
        let mut loaded = RecursiveDagBrowserState::ready(
            None,
            caps,
            vec![graph],
            Some(graph_id),
            Some(previous_detail),
        );
        loaded.inspector.artifact_detail = Some(RecursiveDagArtifactDetailState {
            graph_id,
            artifact_id: 1,
            status: RecursiveDagInspectorLoadStatus::Ready,
            readback: None,
            message: Some("loaded old artifact".to_string()),
            warnings: Vec::new(),
        });

        apply_recursive_dag_load_result(&mut app, Ok(loaded.into()));

        let state = browser_state(&app);
        assert_eq!(state.selected_artifact, 1);
        assert!(state.inspector.artifact_detail.is_none());
    }

    #[test]
    fn stale_artifact_preview_is_dropped_when_selected_row_changes() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut previous_detail = selected_detail(graph.clone(), root_task_id);
        previous_detail
            .artifact_summary_page
            .items
            .push(artifact_summary(graph_id, root_task_id, 1, "first"));
        previous_detail
            .artifact_summary_page
            .items
            .push(artifact_summary(graph_id, root_task_id, 2, "second"));
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_artifact_preview_inspection: true,
            ..Default::default()
        };
        let mut app = app_with_state(RecursiveDagBrowserState::ready(
            None,
            caps.clone(),
            vec![graph.clone()],
            Some(graph_id),
            Some(previous_detail.clone()),
        ));
        if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
            state.panel = crate::types::RecursiveDagPanel::Artifacts;
            state.selected_artifact = 1;
            state.inspector.open = true;
            state.inspector.view = RecursiveDagInspectorView::Preview;
        }
        let mut loaded = RecursiveDagBrowserState::ready(
            None,
            caps,
            vec![graph],
            Some(graph_id),
            Some(previous_detail),
        );
        loaded.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState {
            graph_id,
            artifact_id: 1,
            status: RecursiveDagInspectorLoadStatus::Ready,
            preview: Some(execution_artifact_preview(graph_id, 1, "first")),
            message: Some("loaded old preview".to_string()),
            warnings: Vec::new(),
        });

        apply_recursive_dag_load_result(&mut app, Ok(loaded.into()));

        let state = browser_state(&app);
        assert_eq!(state.selected_artifact, 1);
        assert!(state.inspector.artifact_preview.is_none());
    }

    #[test]
    fn artifact_preview_result_preserves_current_inspector_view() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "artifact",
        ));
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_artifact_preview_inspection: true,
            ..Default::default()
        };
        let mut app = app_with_state(RecursiveDagBrowserState::ready(
            None,
            caps.clone(),
            vec![graph.clone()],
            Some(graph_id),
            Some(detail.clone()),
        ));
        if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
            state.panel = crate::types::RecursiveDagPanel::Artifacts;
            state.inspector.open = true;
            state.inspector.view = RecursiveDagInspectorView::Metadata;
            state.inspector.artifact_preview =
                Some(RecursiveDagArtifactPreviewState::loading(graph_id, 1));
        }

        let mut loaded =
            RecursiveDagBrowserState::ready(None, caps, vec![graph], Some(graph_id), Some(detail));
        loaded.inspector.open = true;
        loaded.inspector.view = RecursiveDagInspectorView::Preview;
        loaded.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState {
            graph_id,
            artifact_id: 1,
            status: RecursiveDagInspectorLoadStatus::Ready,
            preview: Some(execution_artifact_preview(graph_id, 1, "artifact")),
            message: Some("loaded bounded artifact preview 1".to_string()),
            warnings: Vec::new(),
        });

        apply_recursive_dag_load_result(&mut app, Ok(loaded.into()));

        let state = browser_state(&app);
        assert!(state.inspector.open);
        assert_eq!(state.inspector.view, RecursiveDagInspectorView::Metadata);
        assert!(matches!(
            state
                .inspector
                .artifact_preview
                .as_ref()
                .map(|preview| preview.status),
            Some(RecursiveDagInspectorLoadStatus::Ready)
        ));
    }

    #[test]
    fn artifact_preview_result_does_not_reopen_closed_inspector() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = selected_detail(graph.clone(), root_task_id);
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "artifact",
        ));
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_artifact_preview_inspection: true,
            ..Default::default()
        };
        let mut app = app_with_state(RecursiveDagBrowserState::ready(
            None,
            caps.clone(),
            vec![graph.clone()],
            Some(graph_id),
            Some(detail.clone()),
        ));
        if let OverlayState::RecursiveDagBrowser(state) = &mut app.overlay {
            state.panel = crate::types::RecursiveDagPanel::Artifacts;
            state.inspector.open = false;
            state.inspector.view = RecursiveDagInspectorView::Preview;
            state.inspector.artifact_preview =
                Some(RecursiveDagArtifactPreviewState::loading(graph_id, 1));
        }

        let mut loaded =
            RecursiveDagBrowserState::ready(None, caps, vec![graph], Some(graph_id), Some(detail));
        loaded.inspector.open = true;
        loaded.inspector.view = RecursiveDagInspectorView::Preview;
        loaded.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState {
            graph_id,
            artifact_id: 1,
            status: RecursiveDagInspectorLoadStatus::Ready,
            preview: Some(execution_artifact_preview(graph_id, 1, "artifact")),
            message: Some("loaded bounded artifact preview 1".to_string()),
            warnings: Vec::new(),
        });

        apply_recursive_dag_load_result(&mut app, Ok(loaded.into()));

        let state = browser_state(&app);
        assert!(!state.inspector.open);
        assert!(matches!(
            state
                .inspector
                .artifact_preview
                .as_ref()
                .map(|preview| preview.status),
            Some(RecursiveDagInspectorLoadStatus::Ready)
        ));
    }

    #[test]
    fn fake_run_prompt_requires_max_steps_before_rpc() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_scheduler_control: true,
            ..Default::default()
        };
        let mut app = app_with_state(ready_state(caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('R')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));

        let OverlayState::RecursiveDagBrowser(state) = &app.overlay else {
            panic!("expected recursive DAG browser");
        };
        match &state.fake_run {
            RecursiveDagFakeRunState::MaxStepsInput { input, error } => {
                assert!(input.is_empty());
                assert!(
                    error
                        .as_deref()
                        .is_some_and(|error| error.contains("max_steps is required"))
                );
            }
            other => panic!("expected max_steps input, got {other:?}"),
        }
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn fake_run_prompt_rejects_zero_before_rpc() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_scheduler_control: true,
            ..Default::default()
        };
        let mut app = app_with_state(ready_state(caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('R')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Char('0')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Enter));

        let OverlayState::RecursiveDagBrowser(state) = &app.overlay else {
            panic!("expected recursive DAG browser");
        };
        match &state.fake_run {
            RecursiveDagFakeRunState::MaxStepsInput { error, .. } => {
                assert!(
                    error
                        .as_deref()
                        .is_some_and(|error| error.contains("greater than zero"))
                );
            }
            other => panic!("expected max_steps input, got {other:?}"),
        }
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn fake_run_prompt_does_not_open_when_daemon_disconnected() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_scheduler_control: true,
            ..Default::default()
        };
        let mut app = app_with_state(ready_state(caps));
        app.poll.connected = false;

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('R')));

        let OverlayState::RecursiveDagBrowser(state) = &app.overlay else {
            panic!("expected recursive DAG browser");
        };
        assert!(matches!(state.fake_run, RecursiveDagFakeRunState::Idle));
        assert!(
            state
                .message
                .as_deref()
                .is_some_and(|message| message.contains("daemon is not connected"))
        );
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn fake_run_success_result_refreshes_read_only_state_and_clears_running() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_scheduler_control: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let mut previous = ready_state_for(graph_id, root_task_id, caps.clone());
        previous.fake_run = RecursiveDagFakeRunState::Running {
            graph_id,
            max_steps: 3,
        };
        let mut app = app_with_state(previous);
        let mut refreshed = ready_state_for(graph_id, root_task_id, caps);
        refreshed.message =
            Some("FAKE scheduler completed: run=abc status=Completed steps=1/3".to_string());

        apply_recursive_dag_load_result(&mut app, Ok(refreshed.into()));

        let OverlayState::RecursiveDagBrowser(state) = &app.overlay else {
            panic!("expected recursive DAG browser");
        };
        assert!(matches!(state.fake_run, RecursiveDagFakeRunState::Idle));
        assert_eq!(state.selected_graph_id(), Some(graph_id));
        assert!(state.selected_detail().is_some());
        assert!(
            state
                .message
                .as_deref()
                .is_some_and(|message| message.contains("FAKE scheduler completed"))
        );
        assert!(
            app.recursive_dag_cache
                .as_ref()
                .is_some_and(|state| state.selected_graph_id() == Some(graph_id))
        );
    }

    #[tokio::test]
    async fn fake_run_success_accepts_top_level_run_response_and_refreshes_readback() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_inspection: true,
            recursive_dag_run_inspection: false,
            recursive_dag_recovery_status: false,
            recursive_dag_scheduler_control: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let graph_detail = selected_detail(graph.clone(), root_task_id).graph;
        let mut run = scheduler_run(graph_id, RecursiveSchedulerRunStatus::Completed);
        run.operator = Some("tui".to_string());
        run.completed_at = Some(Utc::now());
        run.step_count = 1;
        run.max_steps = 1;
        run.report_artifact_id = Some(87);

        let socket_path = rpc_socket_path("fake-run-success");
        let server_caps = caps.clone();
        let server_graph = graph.clone();
        let server_graph_detail = graph_detail.clone();
        let server_run = run.clone();
        let server = spawn_rpc_server_with_replies(socket_path.clone(), 7, move |request| {
            match request.method.as_str() {
                "GetDaemonCapabilities" => Ok(serde_json::to_value(&server_caps).unwrap()),
                "RunRecursiveFakeScheduler" => Ok(serde_json::to_value(&server_run).unwrap()),
                "ListRecursiveTaskGraphs" => {
                    Ok(serde_json::to_value(vec![server_graph.clone()]).unwrap())
                }
                "GetRecursiveTaskGraph" => Ok(serde_json::to_value(&server_graph_detail).unwrap()),
                "GetRecursiveDagOperationalStatus" => Ok(serde_json::Value::Null),
                "ListRecursiveCancellationRequests" => Ok(serde_json::json!([])),
                other => panic!("unexpected RPC {other}"),
            }
        });

        let state = run_recursive_dag_fake_scheduler_and_refresh(socket_path, None, graph_id, 1)
            .await
            .expect("fake scheduler success should refresh");
        let requests = server.await.unwrap();

        let message = state.message.as_deref().expect("success message");
        assert!(message.contains("FAKE scheduler completed"));
        assert!(message.contains(&format!("run={}", run.id)));
        assert!(!message.contains("JSON error"));
        assert!(!message.contains("missing field"));
        assert_eq!(state.selected_graph_id(), Some(graph_id));
        assert!(state.selected_detail().is_some());
        assert!(
            requests
                .iter()
                .any(|request| request.method == "ListRecursiveTaskGraphs")
        );
        assert!(
            requests
                .iter()
                .any(|request| request.method == "GetRecursiveTaskGraph")
        );
        let fake_request = requests
            .iter()
            .find(|request| request.method == "RunRecursiveFakeScheduler")
            .expect("fake scheduler RPC");
        assert_eq!(
            fake_request.params["graph_id"],
            serde_json::json!(graph_id.0)
        );
        assert_eq!(fake_request.params["max_steps"], serde_json::json!(1));
        assert_eq!(fake_request.params["operator"], serde_json::json!("tui"));
        assert_eq!(
            fake_request.params["execution_mode"],
            serde_json::json!("fake")
        );
        assert_only_fake_scheduler_control_rpc(&requests);
    }

    #[tokio::test]
    async fn fake_run_daemon_error_response_renders_failure_and_refreshes() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_inspection: true,
            recursive_dag_run_inspection: false,
            recursive_dag_recovery_status: false,
            recursive_dag_scheduler_control: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let graph_detail = selected_detail(graph.clone(), root_task_id).graph;

        let socket_path = rpc_socket_path("fake-run-error");
        let server_caps = caps.clone();
        let server_graph = graph.clone();
        let server_graph_detail = graph_detail.clone();
        let server = spawn_rpc_server_with_replies(socket_path.clone(), 7, move |request| {
            match request.method.as_str() {
                "GetDaemonCapabilities" => Ok(serde_json::to_value(&server_caps).unwrap()),
                "RunRecursiveFakeScheduler" => Err(RpcError {
                    code: rsi_common::rpc::INVALID_PARAMS,
                    message: "rejected".to_string(),
                    data: None,
                }),
                "ListRecursiveTaskGraphs" => {
                    Ok(serde_json::to_value(vec![server_graph.clone()]).unwrap())
                }
                "GetRecursiveTaskGraph" => Ok(serde_json::to_value(&server_graph_detail).unwrap()),
                "GetRecursiveDagOperationalStatus" => Ok(serde_json::Value::Null),
                "ListRecursiveCancellationRequests" => Ok(serde_json::json!([])),
                other => panic!("unexpected RPC {other}"),
            }
        });

        let state = run_recursive_dag_fake_scheduler_and_refresh(socket_path, None, graph_id, 1)
            .await
            .expect("fake scheduler error should refresh");
        let requests = server.await.unwrap();

        let message = state.message.as_deref().expect("failure message");
        assert!(message.contains("FAKE scheduler failed: RPC error (-32602): rejected"));
        assert!(!message.contains("JSON error"));
        assert!(!message.contains("missing field"));
        assert_eq!(state.selected_graph_id(), Some(graph_id));
        assert!(state.selected_detail().is_some());
        assert!(
            requests
                .iter()
                .any(|request| request.method == "ListRecursiveTaskGraphs")
        );
        assert!(
            requests
                .iter()
                .any(|request| request.method == "GetRecursiveTaskGraph")
        );
        assert_only_fake_scheduler_control_rpc(&requests);
    }

    #[test]
    fn fake_run_refresh_does_not_clobber_in_flight_result() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_scheduler_control: true,
            ..Default::default()
        };
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let mut state = ready_state_for(graph_id, root_task_id, caps);
        state.fake_run = RecursiveDagFakeRunState::Running {
            graph_id,
            max_steps: 3,
        };
        let mut app = app_with_state(state);
        let (_tx, rx) = tokio::sync::oneshot::channel();
        app.recursive_dag_rx = Some(rx);

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('r')));

        let OverlayState::RecursiveDagBrowser(state) = &app.overlay else {
            panic!("expected recursive DAG browser");
        };
        assert!(matches!(
            state.fake_run,
            RecursiveDagFakeRunState::Running {
                graph_id: running_graph_id,
                max_steps: 3,
            } if running_graph_id == graph_id
        ));
        assert!(
            state
                .message
                .as_deref()
                .is_some_and(|message| message.contains("already in flight"))
        );
        assert!(app.recursive_dag_rx.is_some());
    }

    #[test]
    fn stale_project_result_does_not_replace_current_scope() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_scheduler_control: true,
            ..Default::default()
        };
        let current_project_id = uuid::Uuid::new_v4();
        let stale_project_id = uuid::Uuid::new_v4();
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let mut app = app_with_state(RecursiveDagBrowserState::loading(Some(stale_project_id)));
        app.current_project_id = Some(current_project_id);
        let refreshed = RecursiveDagBrowserState::ready(
            Some(stale_project_id),
            caps,
            Vec::new(),
            Some(graph_id),
            Some(selected_detail(
                graph_summary(graph_id, root_task_id),
                root_task_id,
            )),
        );

        apply_recursive_dag_load_result(&mut app, Ok(refreshed.into()));

        let OverlayState::RecursiveDagBrowser(state) = &app.overlay else {
            panic!("expected recursive DAG browser");
        };
        assert_eq!(state.project_id, Some(current_project_id));
        assert_eq!(state.load_status, RecursiveDagLoadStatus::Error);
        assert!(
            state
                .message
                .as_deref()
                .is_some_and(|message| message.contains("project scope changed"))
        );
    }

    #[test]
    fn fake_run_prompt_rejects_non_digit_input_before_rpc() {
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_scheduler_control: true,
            ..Default::default()
        };
        let mut app = app_with_state(ready_state(caps));

        handle_recursive_dag_key(&mut app, key(KeyCode::Char('R')));
        handle_recursive_dag_key(&mut app, key(KeyCode::Char('x')));

        let OverlayState::RecursiveDagBrowser(state) = &app.overlay else {
            panic!("expected recursive DAG browser");
        };
        match &state.fake_run {
            RecursiveDagFakeRunState::MaxStepsInput { error, .. } => {
                assert!(
                    error
                        .as_deref()
                        .is_some_and(|error| error.contains("digits only"))
                );
            }
            other => panic!("expected max_steps input, got {other:?}"),
        }
        assert!(app.recursive_dag_rx.is_none());
    }

    #[test]
    fn truncate_graph_detail_for_browser_bounds_nodes_and_keeps_root() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = rsi_common::RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut nodes = (0..=RECURSIVE_DAG_TASK_LIMIT)
            .map(|_| {
                task_node(
                    graph_id,
                    rsi_common::RecursiveTaskId::new(),
                    Some(root_task_id),
                )
            })
            .collect::<Vec<_>>();
        nodes.push(task_node(graph_id, root_task_id, None));
        let mut detail = RecursiveTaskGraphDetail {
            graph,
            nodes,
            edges: Vec::new(),
            attempts: Vec::new(),
            injection_batches: Vec::new(),
            lifecycle_events: Vec::new(),
            artifacts: Vec::new(),
        };
        let mut warnings = Vec::new();

        truncate_graph_detail_for_browser(&mut detail, &mut warnings);

        assert_eq!(detail.nodes.len(), RECURSIVE_DAG_TASK_LIMIT);
        assert!(detail.nodes.iter().any(|node| node.id == root_task_id));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("task nodes truncated"))
        );
    }
}
