//! Recursive DAG browser rendering.

use std::collections::{BTreeMap, HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};
use rsi_common::{
    RecursiveArtifactMetadataState, RecursiveArtifactPreviewState,
    RecursiveCancellationRequestStatus, RecursiveCancellationRequestSummary,
    RecursiveExecutionArtifact, RecursiveExecutionArtifactPreview,
    RecursiveExecutionArtifactReadback, RecursiveGraphStatus, RecursiveLiveAttemptHeartbeatState,
    RecursiveLiveAttemptHeartbeatStatus, RecursiveLiveAttemptStatus, RecursiveLiveInterruptStatus,
    RecursiveLiveInterruptSummary, RecursiveLiveOutputValidationIssue, RecursiveSchedulerRunStatus,
    RecursiveSchedulerRunSummary, RecursiveSchedulerStopReason, RecursiveTaskGraphId,
    RecursiveTaskId, RecursiveTaskNode,
};

use crate::app::App;
use crate::types::{
    RECURSIVE_DAG_ARTIFACT_PREVIEW_RENDER_LINES, RECURSIVE_DAG_EVENT_LIMIT,
    RECURSIVE_DAG_GRAPH_LIMIT, RECURSIVE_DAG_INSPECTOR_ROW_LIMIT, RECURSIVE_DAG_LIVE_LIMIT,
    RECURSIVE_DAG_METADATA_KEY_LIMIT, RECURSIVE_DAG_RUN_LIMIT, RECURSIVE_DAG_TASK_LIMIT,
    RECURSIVE_DAG_VALIDATION_ISSUE_LIMIT, RecursiveDagArtifactBucket,
    RecursiveDagArtifactPreviewState, RecursiveDagBrowserState, RecursiveDagControlState,
    RecursiveDagFakeRunState, RecursiveDagInspectorLoadStatus, RecursiveDagInspectorRow,
    RecursiveDagInspectorRowKind, RecursiveDagInspectorSource, RecursiveDagInspectorView,
    RecursiveDagLiveRunState, RecursiveDagLoadStatus, RecursiveDagPanel,
    RecursiveDagRecoveryInputField, RecursiveDagSelectedGraphData,
    recursive_dag_preview_unavailable_message,
};
use crate::ui::theme;

pub(super) fn render_recursive_dag_browser(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    state: &RecursiveDagBrowserState,
) {
    let popup = browser_rect(area);
    frame.render_widget(Clear, popup);

    let title = format!(
        " recursive DAG browser [{}] ",
        match state.load_status {
            RecursiveDagLoadStatus::Loading => "loading",
            RecursiveDagLoadStatus::Ready => "fake gated",
            RecursiveDagLoadStatus::Disconnected => "disconnected",
            RecursiveDagLoadStatus::CapabilityDisabled => "capability disabled",
            RecursiveDagLoadStatus::Error => "error",
        }
    );
    let block = theme::overlay_block()
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 {
        return;
    }

    let mut lines = build_lines(app, state, inner.width.saturating_sub(1) as usize);
    let max_scroll = lines.len().saturating_sub(inner.height as usize);
    let offset = state.scroll_offset.min(max_scroll);
    lines = lines
        .into_iter()
        .skip(offset)
        .take(inner.height as usize)
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme::overlay_bg()))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn browser_rect(area: Rect) -> Rect {
    let w = (area.width.saturating_mul(94) / 100)
        .clamp(90, 180)
        .min(area.width);
    let h = (area.height.saturating_mul(86) / 100)
        .clamp(24, 54)
        .min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

pub(crate) fn build_dashboard_lines(
    _app: &App,
    state: &RecursiveDagBrowserState,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    match state.load_status {
        RecursiveDagLoadStatus::Disconnected
        | RecursiveDagLoadStatus::CapabilityDisabled
        | RecursiveDagLoadStatus::Error => {
            push_message(
                &mut lines,
                state.message.as_deref().unwrap_or("unavailable"),
            );
            return lines;
        }
        RecursiveDagLoadStatus::Loading => {
            push_message(
                &mut lines,
                state
                    .message
                    .as_deref()
                    .unwrap_or("loading recursive DAG readbacks"),
            );
            return lines;
        }
        RecursiveDagLoadStatus::Ready => {}
    }

    if let Some(detail) = state.selected_detail() {
        push_runs(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_recovery(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_cancellations(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_heartbeats(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_interrupts(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_artifacts(&mut lines, state, detail, width);
    } else {
        push_message(&mut lines, "no details loaded");
    }

    lines
}

fn build_lines(app: &App, state: &RecursiveDagBrowserState, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    push_header(&mut lines, app, state, width);
    push_fake_scheduler_control(&mut lines, app, state, width);
    push_live_scheduler_control(&mut lines, app, state, width);
    push_recursive_control_rail(&mut lines, app, state, width);

    match state.load_status {
        RecursiveDagLoadStatus::Disconnected
        | RecursiveDagLoadStatus::CapabilityDisabled
        | RecursiveDagLoadStatus::Error => {
            lines.push(Line::default());
            push_message(
                &mut lines,
                state.message.as_deref().unwrap_or("unavailable"),
            );
            push_footer(&mut lines, state);
            return lines;
        }
        RecursiveDagLoadStatus::Loading => {
            lines.push(Line::default());
            push_message(
                &mut lines,
                state
                    .message
                    .as_deref()
                    .unwrap_or("loading recursive DAG readbacks"),
            );
            if !state.graphs.is_empty() {
                push_graph_inventory(&mut lines, state, width);
            }
            push_footer(&mut lines, state);
            return lines;
        }
        RecursiveDagLoadStatus::Ready => {}
    }

    push_graph_inventory(&mut lines, state, width);
    if let Some(message) = &state.message {
        push_message(&mut lines, message);
    }

    if state.graphs.is_empty() {
        lines.push(Line::default());
        push_message(&mut lines, "no recursive DAG graphs found for this scope");
        push_footer(&mut lines, state);
        return lines;
    }

    if state.graph_cursor_changed() {
        lines.push(Line::from(vec![Span::styled(
            "selected graph is not hydrated; press Enter to load it",
            Style::default()
                .fg(theme::yellow())
                .add_modifier(Modifier::BOLD),
        )]));
    }

    if let Some(detail) = state.selected_detail() {
        lines.push(Line::default());
        push_graph_summary(&mut lines, state, detail, width);
        push_readback_warnings(&mut lines, detail, width);
        lines.push(Line::default());
        push_task_tree(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_runs(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_recovery(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_cancellations(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_heartbeats(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_interrupts(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_live(&mut lines, state, detail, width);
        lines.push(Line::default());
        push_artifacts(&mut lines, state, detail, width);
    }

    push_footer(&mut lines, state);
    lines
}

fn push_header(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    state: &RecursiveDagBrowserState,
    width: usize,
) {
    let project = state
        .project_id
        .and_then(|id| app.projects.iter().find(|p| p.id == id))
        .map(|project| project.name.as_str())
        .unwrap_or("all projects");
    let loaded = state
        .loaded_at
        .map(|ts| ts.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "-".to_string());
    lines.push(Line::from(vec![
        Span::styled("scope ", label_style()),
        Span::styled(fit(project, 28), value_style()),
        Span::styled(" loaded ", label_style()),
        Span::styled(loaded, value_style()),
        Span::styled(" panel ", label_style()),
        Span::styled(state.panel.label(), active_style()),
    ]));

    let caps = state.capabilities.as_ref();
    let cap_text = caps.map_or_else(
        || "caps unknown".to_string(),
        |caps| {
            format!(
                "inspect={} run={} recovery={} live={} validation={} artifacts={}/{}/{} fakectl={} livectl={} cancelctl={} recoverctl={} liveexec={} background={}",
                yn(caps.recursive_dag_inspection),
                yn(caps.recursive_dag_run_inspection),
                yn(caps.recursive_dag_recovery_status),
                yn(caps.recursive_dag_live_status_inspection),
                yn(caps.recursive_dag_live_validation_inspection),
                yn(caps.recursive_dag_artifact_list_pagination),
                yn(caps.recursive_dag_artifact_lookup),
                yn(caps.recursive_dag_artifact_preview_inspection),
                yn(caps.recursive_dag_scheduler_control),
                yn(caps.recursive_dag_live_scheduler_control),
                yn(caps.recursive_dag_cancellation_control),
                yn(caps.recursive_dag_recovery_control),
                yn(caps.recursive_dag_live_execution),
                yn(caps.recursive_dag_background_loop),
            )
        },
    );
    lines.push(Line::from(Span::styled(
        fit(&cap_text, width),
        Style::default().fg(theme::overlay_hint()),
    )));
}

fn push_graph_inventory(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Graphs,
        state.panel,
        &format!("GRAPHS (showing up to {})", RECURSIVE_DAG_GRAPH_LIMIT),
    );
    lines.push(row(
        vec![
            ("", 2, theme::overlay_hint()),
            ("status", 10, theme::overlay_hint()),
            ("mode", 12, theme::overlay_hint()),
            ("updated", 16, theme::overlay_hint()),
            ("id", 9, theme::overlay_hint()),
            ("title", width.saturating_sub(54), theme::overlay_hint()),
        ],
        false,
        true,
    ));
    for (idx, graph) in state
        .graphs
        .iter()
        .take(RECURSIVE_DAG_GRAPH_LIMIT)
        .enumerate()
    {
        let selected = idx == state.selected_graph;
        lines.push(row(
            vec![
                (if selected { ">" } else { "" }, 2, theme::yellow()),
                (
                    &format!("{:?}", graph.status),
                    10,
                    graph_status_color(graph.status),
                ),
                (&format!("{:?}", graph.execution_mode), 12, theme::blue()),
                (
                    &graph.updated_at.format("%m-%d %H:%M").to_string(),
                    16,
                    theme::subtext0(),
                ),
                (&short_id(graph.id), 9, theme::overlay_hint()),
                (&graph.title, width.saturating_sub(54), theme::text()),
            ],
            selected,
            false,
        ));
        if selected {
            let meta = graph
                .quarantine_reason
                .as_deref()
                .or(graph.malformed_reason.as_deref())
                .or(graph.last_stop_reason.as_deref());
            if let Some(meta) = meta {
                lines.push(dim_kv("reason", meta, width));
            }
        }
    }
}

fn push_fake_scheduler_control(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    state: &RecursiveDagBrowserState,
    width: usize,
) {
    let gate_enabled = state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_scheduler_control)
        && app.poll.connected;
    let gate_text = if app.poll.connected {
        match state.capabilities.as_ref() {
            Some(_) if gate_enabled => "enabled",
            Some(_) => "unavailable: recursive_dag_scheduler_control=false",
            None => "unavailable: capabilities unknown",
        }
    } else {
        "unavailable: daemon disconnected"
    };
    let selected = state
        .selected_graph_id()
        .map_or_else(|| "-".to_string(), |id| id.to_string());

    match &state.fake_run {
        RecursiveDagFakeRunState::Idle => {
            let label = if gate_enabled
                && matches!(state.load_status, RecursiveDagLoadStatus::Ready)
                && state.selected_graph_id().is_some()
            {
                "R prompts for explicit max_steps; no live execution, background loop, or model call"
            } else {
                gate_text
            };
            lines.push(kv_line(
                "FAKE run",
                &format!(
                    "gate={gate_text} graph={} {label}",
                    short_text(&selected, 8)
                ),
                width,
            ));
        }
        RecursiveDagFakeRunState::MaxStepsInput { input, error } => {
            let value = if input.is_empty() {
                "<required>".to_string()
            } else {
                input.clone()
            };
            lines.push(kv_line(
                "FAKE max_steps",
                &format!("{value}  Enter submit  Esc cancel"),
                width,
            ));
            if let Some(error) = error {
                lines.push(dim_kv("input error", error, width));
            }
        }
        RecursiveDagFakeRunState::Running {
            graph_id,
            max_steps,
        } => {
            lines.push(kv_line(
                "FAKE running",
                &format!(
                    "graph={} max_steps={} refreshing read-only state after completion",
                    short_text(&graph_id.to_string(), 8),
                    max_steps
                ),
                width,
            ));
        }
    }
}

fn push_live_scheduler_control(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    state: &RecursiveDagBrowserState,
    width: usize,
) {
    let gate_enabled = state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_live_scheduler_control)
        && app.poll.connected;
    let gate_text = if app.poll.connected {
        match state.capabilities.as_ref() {
            Some(_) if gate_enabled => "enabled",
            Some(_) => "unavailable: recursive_dag_live_scheduler_control=false",
            None => "unavailable: capabilities unknown",
        }
    } else {
        "unavailable: daemon disconnected"
    };
    let selected = state
        .selected_graph_id()
        .map_or_else(|| "-".to_string(), |id| id.to_string());

    match &state.live_run {
        RecursiveDagLiveRunState::Idle => {
            let label = if gate_enabled
                && matches!(state.load_status, RecursiveDagLoadStatus::Ready)
                && state.selected_graph_id().is_some()
            {
                "L prompts for explicit max_steps; ordinary graph live session launch only"
            } else {
                gate_text
            };
            lines.push(kv_line(
                "LIVE run",
                &format!(
                    "gate={gate_text} graph={} {label}",
                    short_text(&selected, 8)
                ),
                width,
            ));
        }
        RecursiveDagLiveRunState::MaxStepsInput { input, error } => {
            let value = if input.is_empty() {
                "<required>".to_string()
            } else {
                input.clone()
            };
            lines.push(kv_line(
                "LIVE max_steps",
                &format!("{value}  Enter submit  Esc cancel"),
                width,
            ));
            if let Some(error) = error {
                lines.push(dim_kv("input error", error, width));
            }
        }
        RecursiveDagLiveRunState::Running {
            graph_id,
            max_steps,
        } => {
            lines.push(kv_line(
                "LIVE running",
                &format!("graph={} max_steps={max_steps}", short_id(*graph_id)),
                width,
            ));
        }
    }
}

fn push_recursive_control_rail(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    state: &RecursiveDagBrowserState,
    width: usize,
) {
    match &state.control {
        RecursiveDagControlState::Idle => {
            lines.push(kv_line(
                "controls",
                &format!(
                    "cancel={} recover={}  C prefix  c continue recovery  ! gates",
                    cancellation_gate_text(app, state),
                    recovery_gate_text(app, state),
                ),
                width,
            ));
            lines.push(dim_kv(
                "reserved",
                "C t task cancellation reserved; no topology live, direct session interrupt, or background loop",
                width,
            ));
        }
        RecursiveDagControlState::CancellationPrefix => {
            lines.push(kv_line(
                "CANCEL",
                "g graph  r run  t reserved task  ! gates  Esc cancel",
                width,
            ));
        }
        RecursiveDagControlState::GraphReasonInput {
            graph_id,
            input,
            error,
        } => {
            lines.push(kv_line(
                "CANCEL GRAPH",
                &format!(
                    "graph={} reason={}  Enter submit  Esc cancel",
                    short_id(*graph_id),
                    reason_display(input)
                ),
                width,
            ));
            if let Some(error) = error {
                lines.push(dim_kv("input error", error, width));
            }
        }
        RecursiveDagControlState::RunReasonInput {
            run_id,
            input,
            error,
        } => {
            lines.push(kv_line(
                "CANCEL RUN",
                &format!(
                    "run={} reason={}  Enter submit  Esc cancel",
                    short_id(*run_id),
                    reason_display(input)
                ),
                width,
            ));
            if let Some(error) = error {
                lines.push(dim_kv("input error", error, width));
            }
        }
        RecursiveDagControlState::RecoveryBudgetInput {
            max_graphs_input,
            time_budget_ms_input,
            field,
            error,
        } => {
            let time_budget = if time_budget_ms_input.is_empty() {
                "<none>"
            } else {
                time_budget_ms_input
            };
            lines.push(kv_line(
                "RECOVER",
                &format!(
                    "max_graphs={} time_budget_ms={} editing={}  Tab field  Enter submit  Esc cancel",
                    required_display(max_graphs_input),
                    time_budget,
                    recovery_field_label(*field)
                ),
                width,
            ));
            lines.push(dim_kv(
                "manual",
                "manual recovery pass only; no recursive DAG background recovery loop is started",
                width,
            ));
            if let Some(error) = error {
                lines.push(dim_kv("input error", error, width));
            }
        }
        RecursiveDagControlState::Submitting { kind, target } => {
            lines.push(kv_line(
                "submitting",
                &format!(
                    "{} target={} awaiting refreshed daemon readback",
                    kind.label(),
                    short_text(target, 16)
                ),
                width,
            ));
        }
    }
}

fn push_graph_summary(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    let graph = &detail.graph.graph;
    push_section(
        lines,
        RecursiveDagPanel::Graphs,
        state.panel,
        "SELECTED GRAPH",
    );
    let counts = task_counts(detail);
    lines.push(kv_line(
        "graph",
        &format!(
            "{}  id={} root={} status={:?} mode={:?}",
            graph.title,
            short_id(graph.id),
            short_id(graph.root_task_id),
            graph.status,
            graph.execution_mode
        ),
        width,
    ));
    lines.push(kv_line(
        "bounds",
        &format!(
            "depth={} fanout={} descendants={} step_limit={} tasks={} ready={} running={} blocked={} failed={} cancelled={} succeeded={}",
            graph.max_depth,
            graph.max_fanout,
            graph.max_descendants,
            graph.step_limit,
            detail.graph.nodes.len(),
            counts.ready,
            counts.running,
            counts.blocked,
            counts.failed,
            counts.cancelled,
            counts.succeeded,
        ),
        width,
    ));
    lines.push(kv_line(
        "provenance",
        &format!(
            "project={} workflow={} topology={} parent_session={} source_execution={}",
            opt_short_uuid(graph.project_id),
            opt_short_uuid(graph.workflow_id),
            opt_short_uuid(graph.topology_id),
            opt_short_uuid(graph.parent_session_id),
            graph.source_execution_id.as_deref().unwrap_or("-"),
        ),
        width,
    ));
    if graph.quarantined_at.is_some() || graph.malformed_reason.is_some() {
        lines.push(kv_line(
            "quarantine",
            &format!(
                "at={} reason={} malformed={}",
                graph
                    .quarantined_at
                    .map(|ts| ts.format("%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "-".to_string()),
                graph.quarantine_reason.as_deref().unwrap_or("-"),
                graph.malformed_reason.as_deref().unwrap_or("-"),
            ),
            width,
        ));
    }
    if let Some(status) = &detail.operational_status {
        lines.push(kv_line(
            "operation",
            &format!(
                "active_run={} latest_run={} running_attempts={} open_cancellations={} live_active={} live_recovery_pending={} validations={}",
                status
                    .active_run
                    .as_ref()
                    .map(|run| short_id(run.id))
                    .unwrap_or_else(|| "-".to_string()),
                status
                    .latest_run
                    .as_ref()
                    .map(|run| short_id(run.id))
                    .unwrap_or_else(|| "-".to_string()),
                status.running_attempts.len(),
                status.open_cancellation_requests.len(),
                status.active_live_attempts.len(),
                status.live_recovery_pending.len(),
                status.latest_live_validations.len(),
            ),
            width,
        ));
    } else {
        lines.push(dim_kv(
            "operation",
            "operational status readback unavailable or capability-disabled",
            width,
        ));
    }
    let panels = detail.status_panel_state(state.capabilities.as_ref());
    lines.push(kv_line(
        "status panels",
        &format!(
            "deferred={} cancellations={} open={} applied={} rejected={} heartbeats={} stale={} missing={} interrupts={} pending={} interrupted={} failed={} lost={}",
            panels.deferred_recovery_rows,
            panels.cancellation_rows,
            panels.open_cancellation_rows,
            panels.applied_cancellation_rows,
            panels.rejected_cancellation_rows,
            panels.heartbeat_rows,
            panels.stale_heartbeat_rows,
            panels.missing_heartbeat_rows,
            panels.interrupt_rows,
            panels.pending_interrupt_rows,
            panels.successful_interrupt_rows,
            panels.failed_interrupt_rows,
            panels.lost_live_attempt_rows,
        ),
        width,
    ));
    if is_blocked_no_runnable(detail) {
        lines.push(dim_kv(
            "blocked",
            "latest scheduler run stopped idle with no runnable task",
            width,
        ));
    }
}

fn push_task_tree(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Tasks,
        state.panel,
        &format!("TASK TREE (showing up to {})", RECURSIVE_DAG_TASK_LIMIT),
    );
    lines.push(row(
        vec![
            ("", 2, theme::overlay_hint()),
            ("path", 8, theme::overlay_hint()),
            ("status", 12, theme::overlay_hint()),
            ("try", 5, theme::overlay_hint()),
            ("deps", 6, theme::overlay_hint()),
            ("kids", 6, theme::overlay_hint()),
            ("id", 9, theme::overlay_hint()),
            ("title", width.saturating_sub(55), theme::overlay_hint()),
        ],
        false,
        true,
    ));
    let rows = task_tree_rows(detail);
    for (visible_idx, (path, task)) in rows.iter().take(RECURSIVE_DAG_TASK_LIMIT).enumerate() {
        let selected =
            state.panel == RecursiveDagPanel::Tasks && visible_idx == state.selected_task;
        let attempts = detail
            .graph
            .attempts
            .iter()
            .filter(|attempt| attempt.task_id == task.id)
            .count();
        let deps = detail
            .graph
            .edges
            .iter()
            .filter(|edge| edge.to_task_id == task.id)
            .count();
        let kids = detail
            .graph
            .nodes
            .iter()
            .filter(|node| node.parent_task_id == Some(task.id))
            .count();
        lines.push(row(
            vec![
                (if selected { ">" } else { "" }, 2, theme::yellow()),
                (path, 8, theme::blue()),
                (
                    &format!("{:?}", task.status),
                    12,
                    task_status_color(task.status),
                ),
                (&attempts.to_string(), 5, theme::subtext0()),
                (&deps.to_string(), 6, theme::subtext0()),
                (&kids.to_string(), 6, theme::subtext0()),
                (&short_id(task.id), 9, theme::overlay_hint()),
                (&task.title, width.saturating_sub(55), theme::text()),
            ],
            selected,
            false,
        ));
        if selected {
            push_selected_task(lines, task, width);
        }
    }
}

fn push_selected_task(lines: &mut Vec<Line<'static>>, task: &RecursiveTaskNode, width: usize) {
    lines.push(dim_kv("objective", &task.objective, width));
    if let Some(blocked) = &task.blocked_reason {
        lines.push(dim_kv("blocked", blocked, width));
    }
    if let Some(strategy) = &task.verification_strategy {
        lines.push(dim_kv("verify", strategy, width));
    }
}

fn push_readback_warnings(
    lines: &mut Vec<Line<'static>>,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    if detail.warnings.is_empty() {
        return;
    }
    lines.push(kv_line(
        "warnings",
        &format!("{} non-fatal readback issue(s)", detail.warnings.len()),
        width,
    ));
    for warning in detail.warnings.iter().take(5) {
        lines.push(dim_kv("-", warning, width));
    }
}

fn push_runs(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Runs,
        state.panel,
        &format!("SCHEDULER RUNS (limit {})", RECURSIVE_DAG_RUN_LIMIT),
    );
    if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_run_inspection)
    {
        lines.push(dim_line("run inspection capability disabled"));
        return;
    }
    if detail.scheduler_runs.is_empty() {
        lines.push(dim_line("no scheduler runs"));
    } else {
        lines.push(row(
            vec![
                ("", 2, theme::overlay_hint()),
                ("status", 13, theme::overlay_hint()),
                ("source", 16, theme::overlay_hint()),
                ("steps", 9, theme::overlay_hint()),
                ("heartbeat", 18, theme::overlay_hint()),
                ("id", 9, theme::overlay_hint()),
                ("reason", width.saturating_sub(72), theme::overlay_hint()),
            ],
            false,
            true,
        ));
        for (idx, run) in detail
            .scheduler_runs
            .iter()
            .take(RECURSIVE_DAG_RUN_LIMIT)
            .enumerate()
        {
            let selected = state.panel == RecursiveDagPanel::Runs && idx == state.selected_run;
            lines.push(row(
                vec![
                    (if selected { ">" } else { "" }, 2, theme::yellow()),
                    (
                        &format!("{:?}", run.status),
                        13,
                        run_status_color(run.status),
                    ),
                    (&format!("{:?}", run.source), 16, theme::blue()),
                    (
                        &format!("{}/{}", run.step_count, run.max_steps),
                        9,
                        theme::text(),
                    ),
                    (
                        &heartbeat_range(run.lease_heartbeat_at, run.lease_expires_at),
                        18,
                        theme::subtext0(),
                    ),
                    (&short_id(run.id), 9, theme::overlay_hint()),
                    (
                        &run.stop_reason
                            .map(|reason| format!("{reason:?}"))
                            .or_else(|| run.failure_reason.clone())
                            .unwrap_or_else(|| "-".to_string()),
                        width.saturating_sub(72),
                        theme::subtext0(),
                    ),
                ],
                selected,
                false,
            ));
        }
    }
    if let Some(run) = &detail.selected_run_detail {
        lines.push(kv_line(
            "selected run",
            &format!(
                "active_attempts={} cancellations={} live_attempts={} live_interrupts={} live_validations={} report_artifact={}",
                run.active_attempts.len(),
                run.cancellation_requests.len(),
                run.live_attempts.len(),
                run.live_interrupts.len(),
                run.latest_live_validations.len(),
                run.report_artifact
                    .as_ref()
                    .map(|artifact| artifact.id.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ),
            width,
        ));
    }
    if detail.run_events.is_empty() {
        lines.push(dim_kv("events", "no run events loaded", width));
    } else {
        lines.push(kv_line(
            "events",
            &format!(
                "showing {} of limit {}",
                detail.run_events.len(),
                RECURSIVE_DAG_EVENT_LIMIT
            ),
            width,
        ));
        for event in detail.run_events.iter().take(6) {
            lines.push(dim_kv(
                &format!("#{}", event.id),
                &format!(
                    "{} {} {}",
                    event.created_at.format("%m-%d %H:%M"),
                    event.event_type,
                    event.message.as_deref().unwrap_or("")
                ),
                width,
            ));
        }
    }
}

fn push_live(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Live,
        state.panel,
        &format!(
            "LIVE ATTEMPTS / VALIDATION (limit {})",
            RECURSIVE_DAG_LIVE_LIMIT
        ),
    );
    if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_live_status_inspection)
    {
        lines.push(dim_line(
            "live readback unavailable: daemon capability disabled",
        ));
        return;
    }
    let panel_state = detail.status_panel_state(state.capabilities.as_ref());
    if panel_state.lost_live_attempt_rows > 0 {
        lines.push(kv_line(
            "lost",
            &format!(
                "{} live attempt(s) are marked lost or recovery-lost",
                panel_state.lost_live_attempt_rows
            ),
            width,
        ));
    }
    if detail.live_attempts.is_empty() {
        lines.push(dim_line("no live attempts"));
    } else {
        lines.push(row(
            vec![
                ("", 2, theme::overlay_hint()),
                ("status", 15, theme::overlay_hint()),
                ("recovery", 12, theme::overlay_hint()),
                ("heartbeat", 12, theme::overlay_hint()),
                ("interrupt", 12, theme::overlay_hint()),
                ("validation", 12, theme::overlay_hint()),
                ("task", 9, theme::overlay_hint()),
                (
                    "session/model",
                    width.saturating_sub(78),
                    theme::overlay_hint(),
                ),
            ],
            false,
            true,
        ));
        for (idx, item) in detail
            .live_attempts
            .iter()
            .take(RECURSIVE_DAG_LIVE_LIMIT)
            .enumerate()
        {
            let selected =
                state.panel == RecursiveDagPanel::Live && idx == state.selected_live_attempt;
            let heartbeat = item
                .heartbeat
                .as_ref()
                .map(|state| format!("{:?}", state.heartbeat_status))
                .unwrap_or_else(|| "missing".to_string());
            let interrupt = item
                .latest_interrupt
                .as_ref()
                .map(|interrupt| format!("{:?}", interrupt.status))
                .unwrap_or_else(|| "-".to_string());
            let validation = item
                .latest_validation
                .as_ref()
                .map(|validation| format!("{:?}", validation.status))
                .unwrap_or_else(|| "-".to_string());
            let session_model = format!(
                "{} {}",
                item.summary
                    .session_id
                    .map(short_uuid)
                    .unwrap_or_else(|| "-".to_string()),
                item.summary.model.as_deref().unwrap_or("-")
            );
            lines.push(row(
                vec![
                    (if selected { ">" } else { "" }, 2, theme::yellow()),
                    (
                        &format!("{:?}", item.summary.status),
                        15,
                        live_status_color(item.summary.status),
                    ),
                    (
                        &format!("{:?}", item.summary.recovery_status),
                        12,
                        theme::peach(),
                    ),
                    (&heartbeat, 12, heartbeat_color(&heartbeat)),
                    (&interrupt, 12, theme::red()),
                    (&validation, 12, validation_color_text(&validation)),
                    (&short_id(item.summary.task_id), 9, theme::overlay_hint()),
                    (&session_model, width.saturating_sub(78), theme::text()),
                ],
                selected,
                false,
            ));
        }
        if state.panel == RecursiveDagPanel::Live {
            lines.push(dim_kv(
                "artifacts",
                "Enter loads artifact buckets for the selected live attempt",
                width,
            ));
        }
    }
    if !detail.validation_results.is_empty() {
        lines.push(kv_line(
            "validation",
            &format!(
                "{} result rows loaded; errors/warnings shown in validation column",
                detail.validation_results.len()
            ),
            width,
        ));
    } else if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_live_validation_inspection)
    {
        lines.push(dim_kv(
            "validation",
            "live validation readback unavailable",
            width,
        ));
    }
}

fn push_recovery(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Recovery,
        state.panel,
        "RECOVERY / DEFERRED / QUARANTINE",
    );
    let panel_state = detail.status_panel_state(state.capabilities.as_ref());
    let graph = &detail.graph.graph;
    if graph.quarantined_at.is_some() || graph.malformed_reason.is_some() {
        lines.push(kv_line(
            "graph health",
            &format!(
                "quarantined={} malformed={} reason={} malformed_reason={}",
                yn(graph.quarantined_at.is_some()),
                yn(graph.malformed_reason.is_some()),
                graph.quarantine_reason.as_deref().unwrap_or("-"),
                graph.malformed_reason.as_deref().unwrap_or("-"),
            ),
            width,
        ));
    }
    let recovery_readback_enabled = state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_recovery_status);
    lines.push(dim_kv(
        "control gate",
        &format!(
            "recoverctl={} manual only; no background recursive recovery loop",
            yn(state
                .capabilities
                .as_ref()
                .is_some_and(|caps| caps.recursive_dag_recovery_control))
        ),
        width,
    ));
    let recovery = detail.recovery_status.as_ref();
    if !recovery_readback_enabled {
        lines.push(dim_line(
            "recovery status capability disabled; recovery controls remain unavailable",
        ));
    } else if recovery.is_none() {
        lines.push(dim_line("recovery status unavailable"));
    }
    if let Some(recovery) = recovery {
        if let Some(graph_status) = &recovery.graph_status {
            lines.push(kv_line(
                "graph recovery",
                &format!(
                    "state={:?} pass={} reason={} last_error={}",
                    graph_status.state,
                    graph_status
                        .pass_id
                        .map(short_id)
                        .unwrap_or_else(|| "-".to_string()),
                    graph_status.reason.as_deref().unwrap_or("-"),
                    graph_status.last_error.as_deref().unwrap_or("-"),
                ),
                width,
            ));
        }
        if let Some(pass) = &recovery.latest_pass {
            lines.push(kv_line(
                "latest pass",
                &format!(
                    "status={:?} source={:?} checked={} recovered={} quarantined={} deferred={} skipped={} errors={} stop={}",
                    pass.status,
                    pass.source,
                    pass.checked,
                    pass.recovered,
                    pass.quarantined,
                    pass.deferred,
                    pass.skipped,
                    pass.errors,
                    pass.stop_reason
                        .map(|reason| format!("{reason:?}"))
                        .unwrap_or_else(|| "-".to_string()),
                ),
                width,
            ));
        }
    }
    if let Some(readback) = &detail.live_recovery_status {
        lines.push(kv_line(
            "live recovery",
            &format!(
                "attempts={} heartbeats={} operator_review={} deferred={} warnings={}",
                readback.live_attempts.len(),
                readback.heartbeat_states.len(),
                yn(readback.operator_review_required),
                yn(readback.deferred_graph.is_some()),
                readback.warnings.len(),
            ),
            width,
        ));
        if let Some(graph_status) = &readback.graph_recovery {
            lines.push(dim_kv(
                "live graph",
                &format!(
                    "state={:?} reason={} last_error={}",
                    graph_status.state,
                    graph_status.reason.as_deref().unwrap_or("-"),
                    graph_status.last_error.as_deref().unwrap_or("-"),
                ),
                width,
            ));
        }
    } else if !panel_state.live_readback_enabled {
        lines.push(dim_kv(
            "live recovery",
            "live recovery readback unavailable: daemon capability disabled",
            width,
        ));
    }
    let deferred_rows = detail.deferred_recovery_rows();
    if let Some(recovery) = recovery {
        lines.push(kv_line(
            "deferred",
            &format!(
                "count={} rows={} oldest={}",
                recovery.deferred_graph_count,
                panel_state.deferred_recovery_rows,
                recovery
                    .oldest_deferred_graph
                    .as_ref()
                    .map(|graph| graph.raw_graph_id.as_str())
                    .unwrap_or("-")
            ),
            width,
        ));
        if deferred_rows.is_empty() && recovery.deferred_graph_count == 0 {
            lines.push(dim_line("no deferred recovery graphs"));
        }
    } else if deferred_rows.is_empty() {
        lines.push(dim_kv(
            "deferred",
            "no deferred recovery rows loaded",
            width,
        ));
    } else {
        lines.push(kv_line(
            "deferred",
            &format!(
                "count=unknown rows={} oldest=-",
                panel_state.deferred_recovery_rows
            ),
            width,
        ));
    }
    for graph in deferred_rows.iter().take(5) {
        lines.push(dim_kv(
            &short_graph_opt(graph.graph_id),
            &format!(
                "state={:?} reason={} next_after={} last_error={}",
                graph.state,
                graph.reason,
                graph
                    .next_after
                    .map(|ts| ts.format("%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "-".to_string()),
                graph.last_error.as_deref().unwrap_or("-"),
            ),
            width,
        ));
    }
}

fn push_cancellations(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Cancellations,
        state.panel,
        &format!("CANCELLATION REQUESTS (limit {})", RECURSIVE_DAG_LIVE_LIMIT),
    );
    let panel_state = detail.status_panel_state(state.capabilities.as_ref());
    lines.push(dim_kv(
        "control gate",
        &format!(
            "cancelctl={} C g graph, C r run, C t task reserved; disabled rows remain read-only",
            yn(state
                .capabilities
                .as_ref()
                .is_some_and(|caps| caps.recursive_dag_cancellation_control))
        ),
        width,
    ));
    let rows = detail.cancellation_rows();
    if rows.is_empty() {
        lines.push(dim_line("no cancellation requests"));
        return;
    }
    lines.push(kv_line(
        "request state",
        &format!(
            "rows={} open={} applied={} rejected={}",
            panel_state.cancellation_rows,
            panel_state.open_cancellation_rows,
            panel_state.applied_cancellation_rows,
            panel_state.rejected_cancellation_rows,
        ),
        width,
    ));
    lines.push(row(
        vec![
            ("", 2, theme::overlay_hint()),
            ("status", 10, theme::overlay_hint()),
            ("scope", 8, theme::overlay_hint()),
            ("source", 12, theme::overlay_hint()),
            ("requested", 14, theme::overlay_hint()),
            ("id", 9, theme::overlay_hint()),
            ("reason", width.saturating_sub(61), theme::overlay_hint()),
        ],
        false,
        true,
    ));
    for (idx, request) in rows.iter().take(RECURSIVE_DAG_LIVE_LIMIT).enumerate() {
        let selected =
            state.panel == RecursiveDagPanel::Cancellations && idx == state.selected_cancellation;
        lines.push(cancellation_row(request, selected, width));
        if selected {
            if let Some(rejection) = &request.rejection_reason {
                lines.push(dim_kv("rejected", rejection, width));
            }
            lines.push(dim_kv(
                "timestamps",
                &format!(
                    "observed={} applied={}",
                    fmt_ts(request.observed_at),
                    fmt_ts(request.applied_at),
                ),
                width,
            ));
        }
    }
}

fn push_heartbeats(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Heartbeats,
        state.panel,
        "HEARTBEATS / LEASES",
    );
    push_scheduler_lease_rows(lines, state, detail, width);

    let panel_state = detail.status_panel_state(state.capabilities.as_ref());
    if !panel_state.live_readback_enabled {
        lines.push(dim_kv(
            "live heartbeat",
            "live readback unavailable: daemon capability disabled",
            width,
        ));
        return;
    }

    let heartbeat_rows = detail.heartbeat_rows();
    if panel_state.stale_heartbeat_rows > 0 || panel_state.missing_heartbeat_rows > 0 {
        lines.push(kv_line(
            "heartbeat warn",
            &format!(
                "stale={} missing={} lost_attempts={}",
                panel_state.stale_heartbeat_rows,
                panel_state.missing_heartbeat_rows,
                panel_state.lost_live_attempt_rows,
            ),
            width,
        ));
    }
    if heartbeat_rows.is_empty() {
        lines.push(dim_line("no live heartbeat readbacks"));
        return;
    }
    lines.push(row(
        vec![
            ("", 2, theme::overlay_hint()),
            ("status", 10, theme::overlay_hint()),
            ("attempt", 9, theme::overlay_hint()),
            ("live", 14, theme::overlay_hint()),
            ("session", 9, theme::overlay_hint()),
            ("last/expires", 18, theme::overlay_hint()),
            ("reason", width.saturating_sub(68), theme::overlay_hint()),
        ],
        false,
        true,
    ));
    for (idx, heartbeat) in heartbeat_rows
        .iter()
        .take(RECURSIVE_DAG_LIVE_LIMIT)
        .enumerate()
    {
        let selected =
            state.panel == RecursiveDagPanel::Heartbeats && idx == state.selected_heartbeat;
        lines.push(heartbeat_row(heartbeat, selected, width));
    }
}

fn push_interrupts(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Interrupts,
        state.panel,
        &format!("LIVE INTERRUPTS (limit {})", RECURSIVE_DAG_LIVE_LIMIT),
    );
    let panel_state = detail.status_panel_state(state.capabilities.as_ref());
    if !panel_state.live_readback_enabled {
        lines.push(dim_line(
            "live interrupt readback unavailable: daemon capability disabled",
        ));
        return;
    }
    let interrupt_rows = detail.interrupt_rows();
    if interrupt_rows.is_empty() {
        lines.push(dim_line("no live interrupt readbacks"));
        return;
    }
    lines.push(kv_line(
        "interrupts",
        &format!(
            "rows={} pending={} interrupted={} failed={}",
            panel_state.interrupt_rows,
            panel_state.pending_interrupt_rows,
            panel_state.successful_interrupt_rows,
            panel_state.failed_interrupt_rows,
        ),
        width,
    ));
    lines.push(row(
        vec![
            ("", 2, theme::overlay_hint()),
            ("status", 12, theme::overlay_hint()),
            ("requested", 14, theme::overlay_hint()),
            ("completed", 14, theme::overlay_hint()),
            ("task", 9, theme::overlay_hint()),
            ("session", 9, theme::overlay_hint()),
            ("reason", width.saturating_sub(66), theme::overlay_hint()),
        ],
        false,
        true,
    ));
    for (idx, interrupt) in interrupt_rows
        .iter()
        .take(RECURSIVE_DAG_LIVE_LIMIT)
        .enumerate()
    {
        let selected =
            state.panel == RecursiveDagPanel::Interrupts && idx == state.selected_interrupt;
        lines.push(interrupt_row(interrupt, selected, width));
        if selected && let Some(failure) = &interrupt.failure_reason {
            lines.push(dim_kv("failure", failure, width));
        }
    }
}

fn push_artifacts(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Artifacts,
        state.panel,
        &format!(
            "ARTIFACTS / INSPECTORS (showing up to {})",
            RECURSIVE_DAG_INSPECTOR_ROW_LIMIT
        ),
    );
    push_artifact_summary_status(lines, detail, width);
    push_artifact_bucket_status(lines, state, width);
    if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_artifact_lookup)
    {
        lines.push(dim_kv(
            "artifact detail",
            "lookup capability disabled; artifact detail readback unavailable",
            width,
        ));
    }
    if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_live_validation_inspection)
    {
        lines.push(dim_kv(
            "validation",
            "live validation capability disabled; validation detail rows unavailable",
            width,
        ));
    }

    let rows = state.inspector_rows();
    if rows.is_empty() {
        lines.push(dim_line(
            "no artifact, validation, test, diff, or report rows loaded",
        ));
        return;
    }
    lines.push(row(
        vec![
            ("", 2, theme::overlay_hint()),
            ("source", 18, theme::overlay_hint()),
            ("type", 10, theme::overlay_hint()),
            ("id", 7, theme::overlay_hint()),
            ("owner", 20, theme::overlay_hint()),
            ("preview", 12, theme::overlay_hint()),
            (
                "label/link",
                width.saturating_sub(76),
                theme::overlay_hint(),
            ),
        ],
        false,
        true,
    ));
    for (idx, inspector_row) in rows.iter().enumerate() {
        let selected =
            state.panel == RecursiveDagPanel::Artifacts && idx == state.selected_artifact;
        lines.push(inspector_row_line(inspector_row, selected, width));
    }
    if rows.len() == RECURSIVE_DAG_INSPECTOR_ROW_LIMIT {
        lines.push(dim_line(
            "inspector row list is locally bounded; page controls remain visible when available",
        ));
    }

    if state.inspector.open {
        lines.push(Line::default());
        push_inspector_detail(lines, state, detail, width);
    }
}

fn cancellation_row(
    request: &RecursiveCancellationRequestSummary,
    selected: bool,
    width: usize,
) -> Line<'static> {
    row(
        vec![
            (if selected { ">" } else { "" }, 2, theme::yellow()),
            (
                &format!("{:?}", request.status),
                10,
                cancellation_status_color(request.status),
            ),
            (&format!("{:?}", request.scope), 8, theme::blue()),
            (&format!("{:?}", request.source), 12, theme::subtext0()),
            (
                &request.requested_at.format("%m-%d %H:%M").to_string(),
                14,
                theme::subtext0(),
            ),
            (&short_id(request.id), 9, theme::overlay_hint()),
            (&request.reason, width.saturating_sub(61), theme::text()),
        ],
        selected,
        false,
    )
}

fn push_scheduler_lease_rows(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_run_inspection)
    {
        lines.push(dim_kv(
            "scheduler lease",
            "run inspection capability disabled",
            width,
        ));
        return;
    }
    if detail.scheduler_runs.is_empty() {
        lines.push(dim_kv("scheduler lease", "no scheduler runs", width));
        return;
    }
    let stale_count = detail
        .scheduler_runs
        .iter()
        .filter(|run| scheduler_lease_state(run) == "stale")
        .count();
    if stale_count > 0 {
        lines.push(kv_line(
            "lease warn",
            &format!("{stale_count} scheduler lease heartbeat(s) are stale"),
            width,
        ));
    }
    lines.push(row(
        vec![
            ("lease", 8, theme::overlay_hint()),
            ("run", 9, theme::overlay_hint()),
            ("status", 12, theme::overlay_hint()),
            ("owner", 14, theme::overlay_hint()),
            ("last/expires", 18, theme::overlay_hint()),
            ("reason", width.saturating_sub(65), theme::overlay_hint()),
        ],
        false,
        true,
    ));
    for run in detail.scheduler_runs.iter().take(5) {
        let lease = scheduler_lease_state(run);
        lines.push(row(
            vec![
                (lease, 8, lease_status_color(lease)),
                (&short_id(run.id), 9, theme::overlay_hint()),
                (
                    &format!("{:?}", run.status),
                    12,
                    run_status_color(run.status),
                ),
                (
                    run.lease_owner.as_deref().unwrap_or("-"),
                    14,
                    theme::subtext0(),
                ),
                (
                    &heartbeat_range(run.lease_heartbeat_at, run.lease_expires_at),
                    18,
                    theme::subtext0(),
                ),
                (
                    &run_stop_label(run),
                    width.saturating_sub(65),
                    theme::subtext0(),
                ),
            ],
            false,
            false,
        ));
    }
}

fn heartbeat_row(
    heartbeat: &RecursiveLiveAttemptHeartbeatState,
    selected: bool,
    width: usize,
) -> Line<'static> {
    row(
        vec![
            (if selected { ">" } else { "" }, 2, theme::yellow()),
            (
                &format!("{:?}", heartbeat.heartbeat_status),
                10,
                heartbeat_status_color(heartbeat.heartbeat_status),
            ),
            (
                &short_id(heartbeat.live_attempt_id),
                9,
                theme::overlay_hint(),
            ),
            (
                &format!("{:?}", heartbeat.live_attempt_status),
                14,
                live_status_color(heartbeat.live_attempt_status),
            ),
            (
                &heartbeat
                    .session_id
                    .map(short_uuid)
                    .unwrap_or_else(|| "-".to_string()),
                9,
                theme::overlay_hint(),
            ),
            (
                &heartbeat_range(heartbeat.heartbeat_at, heartbeat.heartbeat_expires_at),
                18,
                theme::subtext0(),
            ),
            (
                &heartbeat_reason(heartbeat),
                width.saturating_sub(68),
                theme::text(),
            ),
        ],
        selected,
        false,
    )
}

fn interrupt_row(
    interrupt: &RecursiveLiveInterruptSummary,
    selected: bool,
    width: usize,
) -> Line<'static> {
    row(
        vec![
            (if selected { ">" } else { "" }, 2, theme::yellow()),
            (
                &format!("{:?}", interrupt.status),
                12,
                interrupt_status_color(interrupt.status),
            ),
            (
                &interrupt.requested_at.format("%m-%d %H:%M").to_string(),
                14,
                theme::subtext0(),
            ),
            (&fmt_ts(interrupt.completed_at), 14, theme::subtext0()),
            (&short_id(interrupt.task_id), 9, theme::overlay_hint()),
            (
                &interrupt
                    .session_id
                    .map(short_uuid)
                    .unwrap_or_else(|| "-".to_string()),
                9,
                theme::overlay_hint(),
            ),
            (&interrupt.reason, width.saturating_sub(66), theme::text()),
        ],
        selected,
        false,
    )
}

fn inspector_row_line(
    inspector_row: &RecursiveDagInspectorRow,
    selected: bool,
    width: usize,
) -> Line<'static> {
    row(
        vec![
            (if selected { ">" } else { "" }, 2, theme::yellow()),
            (
                &inspector_row.source.label(),
                18,
                inspector_source_color(&inspector_row.source),
            ),
            (
                inspector_row.kind.label(),
                10,
                inspector_kind_color(inspector_row.kind),
            ),
            (
                &inspector_row
                    .artifact_id()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                7,
                theme::overlay_hint(),
            ),
            (&inspector_row.owner, 20, theme::overlay_hint()),
            (preview_state_label(inspector_row), 12, theme::subtext0()),
            (
                &inspector_row.label,
                width.saturating_sub(76),
                theme::text(),
            ),
        ],
        selected,
        false,
    )
}

fn push_artifact_summary_status(
    lines: &mut Vec<Line<'static>>,
    detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    let page = &detail.artifact_summary_page;
    let state_label = match page.status {
        RecursiveDagInspectorLoadStatus::Idle => "idle",
        RecursiveDagInspectorLoadStatus::Loading => "loading",
        RecursiveDagInspectorLoadStatus::Ready => "ready",
        RecursiveDagInspectorLoadStatus::Error => "error",
        RecursiveDagInspectorLoadStatus::CapabilityDisabled => "capability-disabled",
    };
    lines.push(kv_line(
        "artifact page",
        &format!(
            "graph={} state={} rows={} page_size={} pages={}/{} more={} cap={} {}",
            short_id(page.graph_id),
            state_label,
            page.items.len(),
            page.limit,
            page.loaded_pages,
            crate::types::RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT,
            yn(page.has_more),
            yn(page.page_cap_reached),
            page.message.as_deref().unwrap_or("-"),
        ),
        width,
    ));
    match page.status {
        RecursiveDagInspectorLoadStatus::Loading => lines.push(dim_kv(
            "artifact page",
            "loading next artifact summary page",
            width,
        )),
        RecursiveDagInspectorLoadStatus::Error => lines.push(dim_kv(
            "artifact page",
            "artifact summary load-more error is local to this panel; loaded rows are retained",
            width,
        )),
        RecursiveDagInspectorLoadStatus::CapabilityDisabled => lines.push(dim_kv(
            "artifact page",
            "artifact summary pagination capability disabled",
            width,
        )),
        RecursiveDagInspectorLoadStatus::Ready if page.page_cap_reached => lines.push(dim_kv(
            "artifact page",
            "local artifact page cap reached; additional daemon rows are not loaded",
            width,
        )),
        RecursiveDagInspectorLoadStatus::Ready if page.can_load_more() => lines.push(dim_kv(
            "artifact page",
            "select the load-more row and press Enter for the next page",
            width,
        )),
        RecursiveDagInspectorLoadStatus::Ready if !page.has_more => lines.push(dim_kv(
            "artifact page",
            "end of artifact summary list",
            width,
        )),
        _ => {}
    }
    if page.has_more && page.next_cursor.is_none() && !page.page_cap_reached {
        lines.push(dim_kv(
            "artifact page",
            "additional artifact summaries are advertised but no cursor was returned",
            width,
        ));
    }
    for warning in page.warnings.iter().take(4) {
        lines.push(dim_kv("artifact warn", warning, width));
    }
}

fn push_artifact_bucket_status(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    width: usize,
) {
    let Some(bucket) = state.selected_live_attempt_artifacts() else {
        if state.inspector.live_attempt_artifacts.is_some() {
            lines.push(dim_kv(
                "live buckets",
                "loaded buckets are for a different live attempt or graph; press Enter in Live to reload",
                width,
            ));
            return;
        }
        if state
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.recursive_dag_live_status_inspection)
        {
            lines.push(dim_kv(
                "live buckets",
                "select a live attempt and press Enter to load its artifact buckets",
                width,
            ));
        } else {
            lines.push(dim_kv(
                "live buckets",
                "live status capability disabled; live attempt buckets unavailable",
                width,
            ));
        }
        return;
    };

    let state_label = match bucket.status {
        RecursiveDagInspectorLoadStatus::Idle => "idle",
        RecursiveDagInspectorLoadStatus::Loading => "loading",
        RecursiveDagInspectorLoadStatus::Ready => "ready",
        RecursiveDagInspectorLoadStatus::Error => "error",
        RecursiveDagInspectorLoadStatus::CapabilityDisabled => "capability-disabled",
    };
    lines.push(kv_line(
        "live buckets",
        &format!(
            "attempt={} state={} {}",
            short_id(bucket.live_attempt_id),
            state_label,
            bucket.message.as_deref().unwrap_or("-"),
        ),
        width,
    ));
    for warning in bucket.warnings.iter().take(3) {
        lines.push(dim_kv("bucket warn", warning, width));
    }
}

fn push_inspector_detail(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    _detail: &RecursiveDagSelectedGraphData,
    width: usize,
) {
    push_section(
        lines,
        RecursiveDagPanel::Artifacts,
        RecursiveDagPanel::Artifacts,
        &format!("INSPECTOR ({})", state.inspector.view.label()),
    );
    let Some(row) = state.selected_inspector_row() else {
        lines.push(dim_line(
            "selected inspector row is stale; refresh to reload rows",
        ));
        return;
    };
    lines.push(kv_line(
        "selected",
        &format!(
            "type={} source={} key={}",
            row.kind.label(),
            row.source.label(),
            row.key
        ),
        width,
    ));

    match row.kind {
        RecursiveDagInspectorRowKind::Validation => {
            push_validation_detail(lines, state, &row, width);
        }
        _ => {
            let detail = state
                .inspector
                .artifact_detail
                .as_ref()
                .filter(|detail| detail.matches_row(&row));
            if let Some(detail) = detail {
                push_artifact_detail_state(lines, state, &row, detail, width);
            } else if row.artifact_id().is_some() {
                push_artifact_summary_inspector(lines, state, &row, width);
            } else {
                push_unavailable_inspector(lines, state, &row, width);
            }
        }
    }
}

fn push_artifact_detail_state(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    row: &RecursiveDagInspectorRow,
    detail: &crate::types::RecursiveDagArtifactDetailState,
    width: usize,
) {
    match detail.status {
        RecursiveDagInspectorLoadStatus::Loading => {
            lines.push(dim_line("loading graph-guarded artifact detail"));
            push_artifact_summary_inspector(lines, state, row, width);
            return;
        }
        RecursiveDagInspectorLoadStatus::CapabilityDisabled => {
            lines.push(dim_kv(
                "detail",
                detail
                    .message
                    .as_deref()
                    .unwrap_or("artifact lookup capability disabled"),
                width,
            ));
            push_artifact_summary_inspector(lines, state, row, width);
            return;
        }
        RecursiveDagInspectorLoadStatus::Error => {
            lines.push(dim_kv(
                "detail",
                detail
                    .message
                    .as_deref()
                    .unwrap_or("artifact detail unavailable"),
                width,
            ));
            push_artifact_summary_inspector(lines, state, row, width);
            return;
        }
        RecursiveDagInspectorLoadStatus::Ready => {}
        RecursiveDagInspectorLoadStatus::Idle => {
            push_artifact_summary_inspector(lines, state, row, width);
            return;
        }
    }

    let Some(readback) = &detail.readback else {
        lines.push(dim_line("artifact detail readback missing"));
        push_artifact_summary_inspector(lines, state, row, width);
        return;
    };
    push_artifact_inspector(lines, state, row, readback, width);
}

fn push_artifact_summary_inspector(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    row: &RecursiveDagInspectorRow,
    width: usize,
) {
    if let Some(summary) = &row.artifact_summary {
        lines.push(kv_line(
            "artifact",
            &format!(
                "id={} kind={:?} task={} attempt={} source={}",
                summary.artifact_id,
                summary.kind,
                short_id(summary.task_id),
                summary
                    .attempt_id
                    .map(short_id)
                    .unwrap_or_else(|| "-".to_string()),
                row.source.label()
            ),
            width,
        ));
        lines.push(kv_line(
            "summary",
            &format!(
                "role={} presence={:?} metadata={:?} size={} digest={}",
                summary
                    .role
                    .map(|role| format!("{role:?}"))
                    .unwrap_or_else(|| "-".to_string()),
                summary.content_presence,
                summary.metadata_state,
                summary
                    .size_bytes
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                summary.digest.as_deref().unwrap_or("-"),
            ),
            width,
        ));
        push_summary_metadata_warning(lines, summary.metadata_state, width);
        if let Some(uri) = &summary.uri_display {
            lines.push(dim_kv(
                "uri",
                &format!("{uri}; local file opening is unavailable in this inspector slice"),
                width,
            ));
        }
    } else if let Some(artifact) = &row.artifact {
        lines.push(kv_line(
            "artifact",
            &format!(
                "id={} kind={:?} task={} attempt={} source={}",
                artifact.id,
                artifact.kind,
                short_id(artifact.task_id),
                artifact
                    .attempt_id
                    .map(short_id)
                    .unwrap_or_else(|| "-".to_string()),
                row.source.label()
            ),
            width,
        ));
    }

    if state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_artifact_lookup)
    {
        lines.push(dim_kv(
            "detail",
            "press Enter on this row to load graph-guarded artifact detail",
            width,
        ));
    } else {
        lines.push(dim_kv(
            "detail",
            "artifact lookup capability disabled",
            width,
        ));
    }
    if state.inspector.view == RecursiveDagInspectorView::Preview {
        push_preview_for_row(lines, state, row, width);
    } else {
        push_preview_hint(lines, state, row, width);
    }
    push_degraded_kind_notice(lines, row, width);
}

fn push_artifact_inspector(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    row: &RecursiveDagInspectorRow,
    readback: &RecursiveExecutionArtifactReadback,
    width: usize,
) {
    let artifact = &readback.artifact;
    lines.push(kv_line(
        "artifact",
        &format!(
            "id={} kind={:?} task={} attempt={} source={}",
            artifact.id,
            artifact.kind,
            short_id(artifact.task_id),
            artifact
                .attempt_id
                .map(short_id)
                .unwrap_or_else(|| "-".to_string()),
            row.source.label()
        ),
        width,
    ));
    lines.push(kv_line(
        "created",
        &format!(
            "{} uri={} inline={}",
            artifact.created_at.format("%m-%d %H:%M:%S"),
            artifact.uri.as_deref().unwrap_or("-"),
            yn(artifact.content.is_some()),
        ),
        width,
    ));
    lines.push(dim_kv(
        "readback",
        &format!(
            "role={} provenance={:?} metadata={:?} preview={:?}",
            readback
                .role
                .map(|role| format!("{role:?}"))
                .unwrap_or_else(|| "-".to_string()),
            readback.provenance,
            readback.metadata_state,
            readback.preview.state,
        ),
        width,
    ));
    push_readback_artifact_warnings(lines, readback, width);

    match state.inspector.view {
        RecursiveDagInspectorView::Preview => {
            push_preview_for_row(lines, state, row, width);
        }
        RecursiveDagInspectorView::Links => {
            push_artifact_links(lines, row, artifact, width);
        }
        RecursiveDagInspectorView::Test => {
            lines.push(dim_kv(
                "test detail",
                "typed test detail unavailable; raw artifact content is not parsed for test status",
                width,
            ));
            push_artifact_links(lines, row, artifact, width);
        }
        RecursiveDagInspectorView::Diff => {
            lines.push(dim_kv(
                "diff detail",
                "typed diff hunks unavailable; raw diff text is not trusted as worktree proof",
                width,
            ));
            push_artifact_links(lines, row, artifact, width);
        }
        RecursiveDagInspectorView::Report => {
            lines.push(dim_kv(
                "report detail",
                "typed scheduler report inspector unavailable; report prose is not parsed",
                width,
            ));
            push_artifact_links(lines, row, artifact, width);
        }
        RecursiveDagInspectorView::Metadata => {
            push_metadata(lines, &artifact.metadata, width);
        }
    }

    if !matches!(state.inspector.view, RecursiveDagInspectorView::Preview) {
        push_preview_hint(lines, state, row, width);
    }
    push_degraded_kind_notice(lines, row, width);
}

fn push_preview_hint(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    row: &RecursiveDagInspectorRow,
    width: usize,
) {
    if row.artifact_id().is_none() {
        lines.push(dim_kv(
            "preview",
            "bounded artifact preview unavailable: selected inspector row has no artifact id",
            width,
        ));
        return;
    }
    if state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_artifact_preview_inspection)
    {
        lines.push(dim_kv(
            "preview",
            "bounded artifact preview not loaded; press p to request PreviewRecursiveExecutionArtifact",
            width,
        ));
    } else {
        lines.push(dim_kv(
            "preview",
            recursive_dag_preview_unavailable_message(state.capabilities.as_ref()),
            width,
        ));
    }
}

fn push_preview_for_row(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    row: &RecursiveDagInspectorRow,
    width: usize,
) {
    let preview = state
        .inspector
        .artifact_preview
        .as_ref()
        .filter(|preview| preview.matches_row(row));

    match preview {
        Some(preview) => push_artifact_preview_state(lines, preview, width),
        None => push_preview_hint(lines, state, row, width),
    }
}

fn push_artifact_preview_state(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagArtifactPreviewState,
    width: usize,
) {
    match state.status {
        RecursiveDagInspectorLoadStatus::Loading => {
            lines.push(dim_kv(
                "preview",
                state
                    .message
                    .as_deref()
                    .unwrap_or("loading bounded artifact preview"),
                width,
            ));
        }
        RecursiveDagInspectorLoadStatus::CapabilityDisabled => {
            lines.push(dim_kv(
                "preview",
                state
                    .message
                    .as_deref()
                    .unwrap_or("artifact preview capability disabled"),
                width,
            ));
        }
        RecursiveDagInspectorLoadStatus::Error => {
            lines.push(dim_kv(
                "preview error",
                state
                    .message
                    .as_deref()
                    .unwrap_or("artifact preview unavailable"),
                width,
            ));
        }
        RecursiveDagInspectorLoadStatus::Ready => {
            if let Some(preview) = &state.preview {
                push_artifact_preview(lines, preview, &state.warnings, width);
            } else {
                lines.push(dim_line("artifact preview readback missing"));
            }
        }
        RecursiveDagInspectorLoadStatus::Idle => {
            lines.push(dim_line("artifact preview idle"));
        }
    }
}

fn push_artifact_preview(
    lines: &mut Vec<Line<'static>>,
    preview: &RecursiveExecutionArtifactPreview,
    warnings: &[String],
    width: usize,
) {
    lines.push(kv_line(
        "preview",
        &format!(
            "state={:?} kind={:?} label={} shown={}B/{}L total={}B/{}L caps={}B/{}L",
            preview.content_state,
            preview.kind,
            preview.label,
            preview.shown_bytes,
            preview.shown_lines,
            opt_u64(preview.total_bytes),
            opt_u64(preview.total_lines),
            preview.applied_max_bytes,
            preview.applied_max_lines,
        ),
        width,
    ));

    if let Some(range) = &preview.byte_range {
        lines.push(dim_kv(
            "byte range",
            &format!("{}..{}", range.start, range.end),
            width,
        ));
    }
    if let Some(range) = &preview.line_range {
        lines.push(dim_kv(
            "line range",
            &format!("{}..{}", range.start, range.end),
            width,
        ));
    }
    if preview.truncated {
        lines.push(dim_kv(
            "truncated",
            &format!(
                "bytes={} lines={} omitted_bytes={} omitted_lines={}",
                yn(preview.truncated_by_bytes),
                yn(preview.truncated_by_lines),
                opt_u64(preview.omitted_bytes),
                opt_u64(preview.omitted_lines),
            ),
            width,
        ));
    }
    if let Some(reason) = &preview.binary_unavailable_reason {
        lines.push(dim_kv("unavailable", reason, width));
    }

    match preview.content_state {
        RecursiveArtifactPreviewState::Available | RecursiveArtifactPreviewState::Truncated => {
            if let Some(text) = &preview.text {
                push_preview_text(lines, text, width);
            } else {
                lines.push(dim_line("inline/database-owned preview text is empty"));
            }
        }
        RecursiveArtifactPreviewState::Binary
        | RecursiveArtifactPreviewState::Oversized
        | RecursiveArtifactPreviewState::UnsupportedKind
        | RecursiveArtifactPreviewState::UriBlocked
        | RecursiveArtifactPreviewState::UriNotFound
        | RecursiveArtifactPreviewState::MalformedMetadata
        | RecursiveArtifactPreviewState::Unavailable => {
            lines.push(dim_kv(
                "preview unavailable",
                &format!(
                    "state={:?}; inline/database-owned text is not available",
                    preview.content_state
                ),
                width,
            ));
        }
    }

    for warning in warnings.iter().take(4) {
        lines.push(dim_kv("preview warn", warning, width));
    }
}

fn push_preview_text(lines: &mut Vec<Line<'static>>, text: &str, width: usize) {
    if text.is_empty() {
        lines.push(dim_line("inline/database-owned preview text is empty"));
        return;
    }

    let mut lines_iter = text.lines();
    for line in lines_iter
        .by_ref()
        .take(RECURSIVE_DAG_ARTIFACT_PREVIEW_RENDER_LINES)
    {
        lines.push(Line::from(vec![
            Span::styled("  ", label_style()),
            Span::styled(fit(line, width.saturating_sub(2)), value_style()),
        ]));
    }
    if lines_iter.next().is_some() {
        lines.push(dim_kv(
            "preview",
            &format!(
                "render locally bounded at {} line(s); additional line(s) hidden",
                RECURSIVE_DAG_ARTIFACT_PREVIEW_RENDER_LINES
            ),
            width,
        ));
    }
}

fn push_degraded_kind_notice(
    lines: &mut Vec<Line<'static>>,
    row: &RecursiveDagInspectorRow,
    width: usize,
) {
    if row.kind == RecursiveDagInspectorRowKind::Diff {
        lines.push(dim_kv("diff rpc", "typed diff hunks unavailable", width));
    }
    if row.kind == RecursiveDagInspectorRowKind::Report {
        lines.push(dim_kv(
            "report rpc",
            "typed scheduler report inspector unavailable",
            width,
        ));
    }
    if row.kind == RecursiveDagInspectorRowKind::Test {
        lines.push(dim_kv("test rpc", "typed test detail unavailable", width));
    }
}

fn push_validation_detail(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    row: &RecursiveDagInspectorRow,
    width: usize,
) {
    let Some(validation) = &row.validation else {
        lines.push(dim_line("validation row is missing summary data"));
        return;
    };
    let summary = &validation.summary;
    lines.push(kv_line(
        "validation",
        &format!(
            "status={:?} id={} live={} task={} issues={} E{} W{} I{}",
            summary.status,
            summary
                .validation_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            short_id(summary.live_attempt_id),
            short_id(summary.task_id),
            summary.issue_count,
            summary.error_count,
            summary.warning_count,
            summary.info_count,
        ),
        width,
    ));
    lines.push(dim_kv(
        "artifact links",
        &format!(
            "raw={} normalized={} report={} produced={} tests={} diffs={}",
            opt_i64(validation.artifact_links.raw_output_artifact_id),
            opt_i64(validation.artifact_links.normalized_output_artifact_id),
            opt_i64(validation.artifact_links.validation_artifact_id),
            validation.artifact_links.produced_artifact_ids.len(),
            validation.artifact_links.test_artifact_ids.len(),
            validation.artifact_links.diff_artifact_ids.len(),
        ),
        width,
    ));

    let detail = state
        .inspector
        .validation_detail
        .as_ref()
        .filter(|detail| detail.matches_row(row));
    match detail.map(|detail| detail.status) {
        Some(RecursiveDagInspectorLoadStatus::Loading) => {
            lines.push(dim_line("loading validation detail and issue rows"));
        }
        Some(RecursiveDagInspectorLoadStatus::CapabilityDisabled) => {
            lines.push(dim_kv(
                "detail",
                "live validation capability disabled",
                width,
            ));
        }
        Some(RecursiveDagInspectorLoadStatus::Error) => {
            lines.push(dim_kv(
                "detail",
                detail
                    .and_then(|detail| detail.message.as_deref())
                    .unwrap_or("validation detail unavailable"),
                width,
            ));
        }
        Some(RecursiveDagInspectorLoadStatus::Ready) => {
            let detail = detail.expect("detail checked above");
            lines.push(kv_line(
                "detail",
                detail
                    .message
                    .as_deref()
                    .unwrap_or("validation detail loaded"),
                width,
            ));
            if let Some(result) = &detail.result {
                lines.push(dim_kv(
                    "parser",
                    &result
                        .parser_source
                        .as_ref()
                        .map(|source| format!("{source:?}"))
                        .unwrap_or_else(|| "-".to_string()),
                    width,
                ));
                lines.push(dim_kv(
                    "normalized",
                    "hidden in degraded inspector; use typed readbacks when exposed",
                    width,
                ));
            }
            push_issue_rows(lines, &detail.issues, width);
        }
        Some(RecursiveDagInspectorLoadStatus::Idle) | None => {
            if let Some(issues) = &validation.issues {
                push_issue_rows(lines, issues, width);
            } else {
                lines.push(dim_line(
                    "press Enter on this row to load validation detail",
                ));
            }
        }
    }
    lines.push(dim_kv(
        "preview",
        recursive_dag_preview_unavailable_message(state.capabilities.as_ref()),
        width,
    ));
}

fn push_unavailable_inspector(
    lines: &mut Vec<Line<'static>>,
    state: &RecursiveDagBrowserState,
    row: &RecursiveDagInspectorRow,
    width: usize,
) {
    if let RecursiveDagInspectorSource::Unavailable { reason } = &row.source {
        lines.push(dim_kv("unavailable", reason, width));
    }
    match row.kind {
        RecursiveDagInspectorRowKind::Report => lines.push(dim_kv(
            "report detail",
            "typed scheduler report inspector unavailable",
            width,
        )),
        RecursiveDagInspectorRowKind::Test => lines.push(dim_kv(
            "test detail",
            "typed test detail unavailable",
            width,
        )),
        RecursiveDagInspectorRowKind::Diff => lines.push(dim_kv(
            "diff detail",
            "typed diff detail unavailable",
            width,
        )),
        _ => lines.push(dim_kv(
            "preview",
            recursive_dag_preview_unavailable_message(state.capabilities.as_ref()),
            width,
        )),
    }
}

fn push_artifact_links(
    lines: &mut Vec<Line<'static>>,
    row: &RecursiveDagInspectorRow,
    artifact: &RecursiveExecutionArtifact,
    width: usize,
) {
    lines.push(kv_line(
        "links",
        &format!(
            "graph={} task={} attempt={} artifact={} source={}",
            short_id(artifact.graph_id),
            short_id(artifact.task_id),
            artifact
                .attempt_id
                .map(short_id)
                .unwrap_or_else(|| "-".to_string()),
            artifact.id,
            row.source.label(),
        ),
        width,
    ));
    match &row.source {
        RecursiveDagInspectorSource::SchedulerReport { run_id } => {
            lines.push(dim_kv("scheduler run", &run_id.to_string(), width));
        }
        RecursiveDagInspectorSource::LiveAttemptBucket {
            live_attempt_id,
            bucket,
        } => {
            lines.push(dim_kv(
                "live attempt",
                &format!("{} bucket={}", live_attempt_id, bucket.label()),
                width,
            ));
        }
        _ => {}
    }
    if let Some(uri) = &artifact.uri {
        lines.push(dim_kv(
            "uri",
            &format!("{uri}; local file opening is unavailable in this inspector slice"),
            width,
        ));
    }
}

fn push_metadata(lines: &mut Vec<Line<'static>>, metadata: &serde_json::Value, width: usize) {
    match metadata {
        serde_json::Value::Object(map) if map.is_empty() => {
            lines.push(dim_line("metadata is empty"));
        }
        serde_json::Value::Object(map) => {
            let mut ordered = BTreeMap::new();
            for (key, value) in map {
                ordered.insert(key, value);
            }
            for (key, value) in ordered.iter().take(RECURSIVE_DAG_METADATA_KEY_LIMIT) {
                lines.push(dim_kv(
                    key,
                    &fit_metadata_value(value, width.saturating_sub(18)),
                    width,
                ));
            }
            if ordered.len() > RECURSIVE_DAG_METADATA_KEY_LIMIT {
                lines.push(dim_kv(
                    "metadata",
                    &format!(
                        "{} additional metadata key(s) folded",
                        ordered.len() - RECURSIVE_DAG_METADATA_KEY_LIMIT
                    ),
                    width,
                ));
            }
        }
        other => lines.push(dim_kv(
            "metadata",
            &fit_metadata_value(other, width.saturating_sub(18)),
            width,
        )),
    }
}

fn push_summary_metadata_warning(
    lines: &mut Vec<Line<'static>>,
    metadata_state: RecursiveArtifactMetadataState,
    width: usize,
) {
    match metadata_state {
        RecursiveArtifactMetadataState::Malformed => lines.push(dim_kv(
            "metadata warn",
            "malformed artifact metadata reported by summary readback",
            width,
        )),
        RecursiveArtifactMetadataState::Legacy => lines.push(dim_kv(
            "metadata warn",
            "legacy artifact metadata; role and owner links may be incomplete",
            width,
        )),
        RecursiveArtifactMetadataState::Unavailable => lines.push(dim_kv(
            "metadata warn",
            "artifact metadata unavailable",
            width,
        )),
        RecursiveArtifactMetadataState::Valid => {}
    }
}

fn push_readback_artifact_warnings(
    lines: &mut Vec<Line<'static>>,
    readback: &RecursiveExecutionArtifactReadback,
    width: usize,
) {
    push_summary_metadata_warning(lines, readback.metadata_state, width);
    for warning in readback.warnings.iter().take(4) {
        lines.push(dim_kv(
            "artifact warn",
            &format!("{}: {}", warning.code, warning.message),
            width,
        ));
    }
}

fn push_issue_rows(
    lines: &mut Vec<Line<'static>>,
    issues: &[RecursiveLiveOutputValidationIssue],
    width: usize,
) {
    if issues.is_empty() {
        lines.push(dim_line("no validation issue rows loaded"));
        return;
    }
    lines.push(row(
        vec![
            ("sev", 8, theme::overlay_hint()),
            ("class", 12, theme::overlay_hint()),
            ("code", 28, theme::overlay_hint()),
            ("location", 18, theme::overlay_hint()),
            ("message", width.saturating_sub(69), theme::overlay_hint()),
        ],
        false,
        true,
    ));
    for issue in issues.iter().take(RECURSIVE_DAG_VALIDATION_ISSUE_LIMIT) {
        lines.push(row(
            vec![
                (
                    &format!("{:?}", issue.severity),
                    8,
                    validation_severity_color(issue),
                ),
                (&format!("{:?}", issue.class), 12, theme::subtext0()),
                (&format!("{:?}", issue.code), 28, theme::overlay_hint()),
                (&issue_location_label(issue), 18, theme::overlay_hint()),
                (&issue.message, width.saturating_sub(69), theme::text()),
            ],
            false,
            false,
        ));
        if let Some(next) = &issue.suggested_next_action {
            lines.push(dim_kv("next action", next, width));
        }
    }
    if issues.len() > RECURSIVE_DAG_VALIDATION_ISSUE_LIMIT {
        lines.push(dim_kv(
            "issues",
            &format!(
                "issue list locally bounded at {} rows",
                RECURSIVE_DAG_VALIDATION_ISSUE_LIMIT
            ),
            width,
        ));
    }
}

fn fit_metadata_value(value: &serde_json::Value, width: usize) -> String {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    fit(&text.replace('\n', "\\n"), width)
}

fn issue_location_label(issue: &RecursiveLiveOutputValidationIssue) -> String {
    issue
        .location
        .as_ref()
        .map(|location| format!("{location:?}"))
        .unwrap_or_else(|| "-".to_string())
}

fn opt_i64(value: Option<i64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn opt_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn preview_state_label(row: &RecursiveDagInspectorRow) -> &'static str {
    if matches!(
        row.source,
        RecursiveDagInspectorSource::ArtifactSummaryPage { .. }
    ) {
        return "page";
    }
    if let Some(summary) = &row.artifact_summary {
        return preview_state_short(summary.preview_state);
    }
    if let Some(artifact) = &row.artifact {
        return if artifact.content.is_some() && artifact.uri.is_none() {
            "inline"
        } else if artifact.uri.is_some() {
            "uri-blocked"
        } else {
            "unavailable"
        };
    }
    "lookup-missing"
}

fn preview_state_short(state: RecursiveArtifactPreviewState) -> &'static str {
    match state {
        RecursiveArtifactPreviewState::Available => "available",
        RecursiveArtifactPreviewState::Truncated => "truncated",
        RecursiveArtifactPreviewState::Binary => "binary",
        RecursiveArtifactPreviewState::Oversized => "oversized",
        RecursiveArtifactPreviewState::UnsupportedKind => "unsupported",
        RecursiveArtifactPreviewState::UriBlocked => "uri-blocked",
        RecursiveArtifactPreviewState::UriNotFound => "uri-missing",
        RecursiveArtifactPreviewState::MalformedMetadata => "bad-meta",
        RecursiveArtifactPreviewState::Unavailable => "unavailable",
    }
}

fn inspector_source_color(source: &RecursiveDagInspectorSource) -> ratatui::style::Color {
    match source {
        RecursiveDagInspectorSource::GraphArtifact => theme::blue(),
        RecursiveDagInspectorSource::SchedulerReport { .. } => theme::mauve(),
        RecursiveDagInspectorSource::LiveAttemptBucket { bucket, .. } => match bucket {
            RecursiveDagArtifactBucket::Diff => theme::peach(),
            RecursiveDagArtifactBucket::Test => theme::yellow(),
            RecursiveDagArtifactBucket::ValidationReport => theme::red(),
            _ => theme::green(),
        },
        RecursiveDagInspectorSource::ValidationResult => theme::yellow(),
        RecursiveDagInspectorSource::ArtifactSummaryPage { .. } => theme::overlay_hint(),
        RecursiveDagInspectorSource::Unavailable { .. } => theme::overlay0(),
    }
}

fn inspector_kind_color(kind: RecursiveDagInspectorRowKind) -> ratatui::style::Color {
    match kind {
        RecursiveDagInspectorRowKind::Artifact => theme::text(),
        RecursiveDagInspectorRowKind::Validation => theme::yellow(),
        RecursiveDagInspectorRowKind::Test => theme::green(),
        RecursiveDagInspectorRowKind::Diff => theme::peach(),
        RecursiveDagInspectorRowKind::Report => theme::mauve(),
        RecursiveDagInspectorRowKind::Page => theme::overlay_hint(),
    }
}

fn validation_severity_color(issue: &RecursiveLiveOutputValidationIssue) -> ratatui::style::Color {
    match format!("{:?}", issue.severity).as_str() {
        "Error" => theme::red(),
        "Warning" => theme::yellow(),
        "Info" => theme::blue(),
        _ => theme::overlay0(),
    }
}

#[derive(Default)]
struct TaskCounts {
    ready: usize,
    running: usize,
    blocked: usize,
    failed: usize,
    cancelled: usize,
    succeeded: usize,
}

fn task_counts(detail: &RecursiveDagSelectedGraphData) -> TaskCounts {
    let mut counts = TaskCounts::default();
    for task in &detail.graph.nodes {
        let status = format!("{:?}", task.status);
        match status.as_str() {
            "Ready" => counts.ready += 1,
            "Running" => counts.running += 1,
            "Blocked" | "BlockedOnChildren" => counts.blocked += 1,
            "Failed" => counts.failed += 1,
            "Cancelled" => counts.cancelled += 1,
            "Succeeded" => counts.succeeded += 1,
            _ => {}
        }
    }
    counts
}

fn task_tree_rows(detail: &RecursiveDagSelectedGraphData) -> Vec<(String, &RecursiveTaskNode)> {
    let mut children: HashMap<Option<RecursiveTaskId>, Vec<&RecursiveTaskNode>> = HashMap::new();
    let mut by_id: HashMap<RecursiveTaskId, &RecursiveTaskNode> = HashMap::new();
    for task in &detail.graph.nodes {
        by_id.insert(task.id, task);
        children.entry(task.parent_task_id).or_default().push(task);
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.title.cmp(&b.title))
        });
    }

    let mut rows = Vec::new();
    let mut visited = HashSet::new();
    walk_task_tree(
        Some(detail.graph.graph.root_task_id),
        "1".to_string(),
        &by_id,
        &children,
        &mut visited,
        &mut rows,
    );
    let mut orphan_index = 2usize;
    for task in &detail.graph.nodes {
        if !visited.contains(&task.id) {
            walk_task_tree(
                Some(task.id),
                orphan_index.to_string(),
                &by_id,
                &children,
                &mut visited,
                &mut rows,
            );
            orphan_index += 1;
        }
    }
    rows
}

fn walk_task_tree<'a>(
    task_id: Option<RecursiveTaskId>,
    path: String,
    by_id: &HashMap<RecursiveTaskId, &'a RecursiveTaskNode>,
    children: &HashMap<Option<RecursiveTaskId>, Vec<&'a RecursiveTaskNode>>,
    visited: &mut HashSet<RecursiveTaskId>,
    rows: &mut Vec<(String, &'a RecursiveTaskNode)>,
) {
    let Some(task_id) = task_id else {
        return;
    };
    if !visited.insert(task_id) {
        return;
    }
    let task = by_id.get(&task_id).copied();
    let Some(task) = task else {
        return;
    };
    rows.push((path.clone(), task));
    if let Some(kids) = children.get(&Some(task.id)) {
        for (idx, child) in kids.iter().enumerate() {
            walk_task_tree(
                Some(child.id),
                format!("{path}.{}", idx + 1),
                by_id,
                children,
                visited,
                rows,
            );
        }
    }
}

fn push_section(
    lines: &mut Vec<Line<'static>>,
    section: RecursiveDagPanel,
    active: RecursiveDagPanel,
    label: &str,
) {
    let style = if section == active {
        active_style()
    } else {
        Style::default()
            .fg(theme::overlay_title())
            .add_modifier(Modifier::BOLD)
    };
    lines.push(Line::from(Span::styled(label.to_string(), style)));
}

fn push_message(lines: &mut Vec<Line<'static>>, message: &str) {
    lines.push(Line::from(Span::styled(
        message.to_string(),
        Style::default().fg(theme::yellow()),
    )));
}

fn push_footer(lines: &mut Vec<Line<'static>>, state: &RecursiveDagBrowserState) {
    lines.push(Line::default());
    let text = if matches!(
        state.control,
        RecursiveDagControlState::GraphReasonInput { .. }
    ) {
        "CANCEL GRAPH reason input  Enter submit  Esc cancel"
    } else if matches!(
        state.control,
        RecursiveDagControlState::RunReasonInput { .. }
    ) {
        "CANCEL RUN reason input  Enter submit  Esc cancel"
    } else if matches!(
        state.control,
        RecursiveDagControlState::RecoveryBudgetInput { .. }
    ) {
        "RECOVER budget input  digits edit  Tab field  Enter submit  Esc cancel"
    } else if matches!(state.control, RecursiveDagControlState::CancellationPrefix) {
        "CANCEL prefix  g graph  r run  t reserved task  Esc cancel"
    } else if matches!(
        state.fake_run,
        RecursiveDagFakeRunState::MaxStepsInput { .. }
    ) {
        "FAKE max_steps input  digits edit  Enter submit  Esc cancel"
    } else if state.inspector.open {
        "inspector  ]a/[a artifact row  m metadata  l links  p preview unavailable  t test unavailable  d diff unavailable  q/Esc close inspector"
    } else {
        "inspect  Tab/h/l panels  j/k nav  Enter load/inspect  ]a/[a artifacts  r refresh  R FAKE  C cancel prefix  c recover  ! gates  q/Esc close"
    };
    lines.push(Line::from(Span::styled(
        text,
        Style::default().fg(theme::overlay_hint()),
    )));
}

fn row(
    parts: Vec<(&str, usize, ratatui::style::Color)>,
    selected: bool,
    header: bool,
) -> Line<'static> {
    let mut spans = Vec::new();
    for (idx, (text, width, color)) in parts.into_iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw(" "));
        }
        let mut style = Style::default().fg(color);
        if selected {
            style = style.bg(theme::surface0()).add_modifier(Modifier::BOLD);
        } else if header {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(fit(text, width), style));
    }
    Line::from(spans)
}

fn kv_line(label: &str, value: &str, width: usize) -> Line<'static> {
    let label_w = 15usize;
    Line::from(vec![
        Span::styled(fit(label, label_w), label_style()),
        Span::raw(" "),
        Span::styled(fit(value, width.saturating_sub(label_w + 1)), value_style()),
    ])
}

fn dim_kv(label: &str, value: &str, width: usize) -> Line<'static> {
    let label_w = 15usize;
    Line::from(vec![
        Span::styled(
            fit(label, label_w),
            Style::default().fg(theme::overlay_hint()),
        ),
        Span::raw(" "),
        Span::styled(
            fit(value, width.saturating_sub(label_w + 1)),
            Style::default().fg(theme::subtext0()),
        ),
    ])
}

fn dim_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme::overlay_hint()),
    ))
}

fn label_style() -> Style {
    Style::default().fg(theme::overlay_hint())
}

fn value_style() -> Style {
    Style::default().fg(theme::text())
}

fn active_style() -> Style {
    Style::default()
        .fg(theme::yellow())
        .add_modifier(Modifier::BOLD)
}

fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= width {
        format!("{text:<width$}")
    } else if width == 1 {
        "~".to_string()
    } else {
        let mut out: String = text.chars().take(width - 1).collect();
        out.push('~');
        out
    }
}

fn short_text(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn short_id<T: ToString>(id: T) -> String {
    let text = id.to_string();
    text.chars().take(8).collect()
}

fn short_uuid(id: uuid::Uuid) -> String {
    short_id(id)
}

fn opt_short_uuid(id: Option<uuid::Uuid>) -> String {
    id.map(short_uuid).unwrap_or_else(|| "-".to_string())
}

fn short_graph_opt(id: Option<RecursiveTaskGraphId>) -> String {
    id.map(short_id).unwrap_or_else(|| "-".to_string())
}

fn heartbeat_range(
    heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    match (heartbeat_at, expires_at) {
        (Some(beat), Some(expires)) => {
            format!("{}>{}", beat.format("%H:%M"), expires.format("%H:%M"))
        }
        (Some(beat), None) => format!("{}>-", beat.format("%H:%M")),
        _ => "-".to_string(),
    }
}

fn fmt_ts(ts: Option<chrono::DateTime<chrono::Utc>>) -> String {
    ts.map(|ts| ts.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn cancellation_gate_text(app: &App, state: &RecursiveDagBrowserState) -> String {
    if !app.poll.connected {
        return "disabled: daemon disconnected".to_string();
    }
    if !matches!(state.load_status, RecursiveDagLoadStatus::Ready) {
        return "disabled: browser not ready".to_string();
    }
    if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_cancellation_control)
    {
        return "disabled: recursive_dag_cancellation_control=false".to_string();
    }
    if state.selected_graph_id().is_none() {
        return "disabled: no selected graph".to_string();
    }
    if let Some(message) = selected_graph_cancellation_gate_text(state) {
        return format!("disabled: {message}");
    }
    "enabled: C g graph / C r run".to_string()
}

fn selected_graph_cancellation_gate_text(state: &RecursiveDagBrowserState) -> Option<String> {
    let summary = state.selected_graph_summary()?;
    let status = state
        .selected_detail()
        .map(|detail| detail.graph.graph.status)
        .unwrap_or(summary.status);
    if status != RecursiveGraphStatus::Active {
        return Some(format!("selected graph not cancellable: {status:?}"));
    }
    let malformed_or_quarantined = state.selected_detail().map_or_else(
        || summary.malformed_reason.is_some() || summary.quarantined_at.is_some(),
        |detail| {
            detail.graph.graph.malformed_reason.is_some()
                || detail.graph.graph.quarantined_at.is_some()
        },
    );
    if malformed_or_quarantined {
        return Some("selected graph malformed or quarantined".to_string());
    }
    None
}

fn recovery_gate_text(app: &App, state: &RecursiveDagBrowserState) -> String {
    if !app.poll.connected {
        return "disabled: daemon disconnected".to_string();
    }
    if !matches!(state.load_status, RecursiveDagLoadStatus::Ready) {
        return "disabled: browser not ready".to_string();
    }
    if !state
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.recursive_dag_recovery_control)
    {
        return "disabled: recursive_dag_recovery_control=false".to_string();
    }
    if !state
        .selected_detail()
        .is_some_and(deferred_recovery_work_visible)
    {
        return "disabled: no deferred work visible".to_string();
    }
    "enabled: c manual pass".to_string()
}

fn deferred_recovery_work_visible(detail: &RecursiveDagSelectedGraphData) -> bool {
    detail
        .recovery_status
        .as_ref()
        .is_some_and(|status| status.deferred_graph_count > 0)
        || !detail.deferred_recovery_rows().is_empty()
}

fn reason_display(input: &str) -> String {
    if input.is_empty() {
        "<required>".to_string()
    } else {
        format!("\"{}\"", input)
    }
}

fn required_display(input: &str) -> &str {
    if input.is_empty() {
        "<required>"
    } else {
        input
    }
}

fn recovery_field_label(field: RecursiveDagRecoveryInputField) -> &'static str {
    match field {
        RecursiveDagRecoveryInputField::MaxGraphs => "max_graphs",
        RecursiveDagRecoveryInputField::TimeBudgetMs => "time_budget_ms",
    }
}

fn scheduler_lease_state(run: &RecursiveSchedulerRunSummary) -> &'static str {
    if !matches!(
        run.status,
        RecursiveSchedulerRunStatus::Running | RecursiveSchedulerRunStatus::Cancelling
    ) {
        return "closed";
    }
    if run.lease_heartbeat_at.is_none() || run.lease_expires_at.is_none() {
        return "missing";
    }
    if run
        .lease_expires_at
        .is_some_and(|expires_at| expires_at <= chrono::Utc::now())
    {
        return "stale";
    }
    "active"
}

fn run_stop_label(run: &RecursiveSchedulerRunSummary) -> String {
    run.stop_reason
        .map(|reason| format!("{reason:?}"))
        .or_else(|| run.failure_reason.clone())
        .unwrap_or_else(|| "-".to_string())
}

fn heartbeat_reason(heartbeat: &RecursiveLiveAttemptHeartbeatState) -> String {
    heartbeat
        .failure_reason
        .as_deref()
        .or(heartbeat.interruption_reason.as_deref())
        .or(heartbeat.cancellation_reason.as_deref())
        .or(heartbeat.recovery_reason.as_deref())
        .or(heartbeat.error.as_deref())
        .unwrap_or("-")
        .to_string()
}

fn is_blocked_no_runnable(detail: &RecursiveDagSelectedGraphData) -> bool {
    detail.scheduler_runs.iter().any(|run| {
        run.stop_reason == Some(RecursiveSchedulerStopReason::IdleNoRunnable)
            || run
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no runnable"))
    })
}

fn yn(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn graph_status_color(status: RecursiveGraphStatus) -> ratatui::style::Color {
    match status {
        RecursiveGraphStatus::Active => theme::green(),
        RecursiveGraphStatus::Blocked => theme::yellow(),
        RecursiveGraphStatus::Failed | RecursiveGraphStatus::Malformed => theme::red(),
        RecursiveGraphStatus::Cancelled => theme::peach(),
        RecursiveGraphStatus::Terminal => theme::blue(),
    }
}

fn task_status_color(status: rsi_common::RecursiveTaskLifecycleState) -> ratatui::style::Color {
    match format!("{status:?}").as_str() {
        "Ready" | "Running" => theme::green(),
        "Blocked" | "BlockedOnChildren" => theme::yellow(),
        "Failed" => theme::red(),
        "Cancelled" => theme::peach(),
        "Succeeded" | "Decomposed" => theme::blue(),
        _ => theme::overlay0(),
    }
}

fn run_status_color(status: RecursiveSchedulerRunStatus) -> ratatui::style::Color {
    match status {
        RecursiveSchedulerRunStatus::Running => theme::green(),
        RecursiveSchedulerRunStatus::Cancelling => theme::yellow(),
        RecursiveSchedulerRunStatus::Completed => theme::blue(),
        RecursiveSchedulerRunStatus::Failed
        | RecursiveSchedulerRunStatus::Rejected
        | RecursiveSchedulerRunStatus::LeaseExpired => theme::red(),
        RecursiveSchedulerRunStatus::Cancelled => theme::peach(),
    }
}

fn cancellation_status_color(status: RecursiveCancellationRequestStatus) -> ratatui::style::Color {
    match status {
        RecursiveCancellationRequestStatus::Requested
        | RecursiveCancellationRequestStatus::Observed => theme::yellow(),
        RecursiveCancellationRequestStatus::Applied => theme::blue(),
        RecursiveCancellationRequestStatus::Rejected => theme::red(),
    }
}

fn live_status_color(status: RecursiveLiveAttemptStatus) -> ratatui::style::Color {
    match format!("{status:?}").as_str() {
        "Created" | "Launching" | "Running" | "WaitingApproval" => theme::green(),
        "RecoveryPending" | "Lost" | "Blocked" => theme::yellow(),
        "Failed" => theme::red(),
        "Interrupted" | "Cancelled" => theme::peach(),
        "Succeeded" | "Decomposed" => theme::blue(),
        _ => theme::overlay0(),
    }
}

fn heartbeat_status_color(status: RecursiveLiveAttemptHeartbeatStatus) -> ratatui::style::Color {
    match status {
        RecursiveLiveAttemptHeartbeatStatus::Active => theme::green(),
        RecursiveLiveAttemptHeartbeatStatus::Stale
        | RecursiveLiveAttemptHeartbeatStatus::Missing => theme::yellow(),
        RecursiveLiveAttemptHeartbeatStatus::Released => theme::blue(),
    }
}

fn heartbeat_color(value: &str) -> ratatui::style::Color {
    match value {
        "Active" => theme::green(),
        "Stale" | "Missing" => theme::yellow(),
        "Released" => theme::blue(),
        _ => theme::overlay0(),
    }
}

fn interrupt_status_color(status: RecursiveLiveInterruptStatus) -> ratatui::style::Color {
    match status {
        RecursiveLiveInterruptStatus::Requested | RecursiveLiveInterruptStatus::Sent => {
            theme::yellow()
        }
        RecursiveLiveInterruptStatus::Interrupted => theme::blue(),
        RecursiveLiveInterruptStatus::Failed
        | RecursiveLiveInterruptStatus::Rejected
        | RecursiveLiveInterruptStatus::Ignored => theme::red(),
    }
}

fn lease_status_color(value: &str) -> ratatui::style::Color {
    match value {
        "active" => theme::green(),
        "stale" | "missing" => theme::yellow(),
        "closed" => theme::blue(),
        _ => theme::overlay0(),
    }
}

fn validation_color_text(value: &str) -> ratatui::style::Color {
    match value {
        "Valid" => theme::green(),
        "Repairable" | "Ambiguous" | "OperatorReviewRequired" => theme::yellow(),
        "Invalid" => theme::red(),
        _ => theme::overlay0(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use chrono::{Duration, Utc};
    use rsi_common::{
        RecursiveArtifactContentPresence, RecursiveArtifactMetadataState, RecursiveArtifactOwners,
        RecursiveArtifactPreviewAvailability, RecursiveArtifactPreviewState,
        RecursiveArtifactProvenance, RecursiveArtifactRole, RecursiveAttemptId,
        RecursiveAttemptPhase, RecursiveCancellationRequestId, RecursiveCancellationRequestSource,
        RecursiveCancellationScope, RecursiveDeferredRecoveryGraph, RecursiveExecutionArtifact,
        RecursiveExecutionArtifactKind, RecursiveExecutionArtifactReadback,
        RecursiveExecutionArtifactSummary, RecursiveExecutionMode, RecursiveGraphRecoveryState,
        RecursiveLiveAttemptArtifactReadback, RecursiveLiveAttemptId, RecursiveLiveAttemptListItem,
        RecursiveLiveAttemptSummary, RecursiveLiveInterruptId,
        RecursiveLiveOutputValidationArtifactLinks, RecursiveLiveOutputValidationIssue,
        RecursiveLiveOutputValidationListItem, RecursiveLiveOutputValidationResult,
        RecursiveLiveOutputValidationStatus, RecursiveLiveRecoveryStatus,
        RecursiveLiveValidationIssueClass, RecursiveLiveValidationIssueCode,
        RecursiveLiveValidationIssueSeverity, RecursiveRecoveryPassId, RecursiveSchedulerRunId,
        RecursiveSchedulerRunSource, RecursiveTaskGraphDetail, RecursiveTaskGraphSummary,
    };

    use crate::client::DaemonClient;
    use crate::types::{
        RecursiveDagArtifactDetailState, RecursiveDagLiveAttemptArtifactsState,
        RecursiveDagValidationDetailState,
    };

    fn test_app() -> App {
        let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
        app.poll.connected = true;
        app
    }

    fn rendered_lines(state: &RecursiveDagBrowserState, width: usize) -> Vec<String> {
        build_lines(&test_app(), state, width)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn lines_text_with_connection(state: &RecursiveDagBrowserState, connected: bool) -> String {
        let mut app = test_app();
        app.poll.connected = connected;
        build_lines(&app, state, 132)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn lines_text(state: &RecursiveDagBrowserState) -> String {
        rendered_lines(state, 132).join("\n")
    }

    fn caps(live: bool, recovery: bool) -> rsi_common::rpc::DaemonCapabilities {
        rsi_common::rpc::DaemonCapabilities {
            recursive_dag_live_status_inspection: live,
            recursive_dag_recovery_status: recovery,
            ..Default::default()
        }
    }

    fn graph_summary(
        graph_id: RecursiveTaskGraphId,
        root_task_id: RecursiveTaskId,
    ) -> RecursiveTaskGraphSummary {
        let now = Utc::now();
        RecursiveTaskGraphSummary {
            id: graph_id,
            root_task_id,
            title: "status graph".to_string(),
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

    fn detail_for(graph: RecursiveTaskGraphSummary) -> RecursiveDagSelectedGraphData {
        RecursiveDagSelectedGraphData {
            graph: RecursiveTaskGraphDetail {
                graph: graph.clone(),
                nodes: Vec::new(),
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
            artifact_summary_page: crate::types::RecursiveDagArtifactSummaryPageState::ready(
                graph.id,
                Vec::new(),
                crate::types::RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
                None,
                false,
                Vec::new(),
            ),
            warnings: Vec::new(),
        }
    }

    fn state_with(
        graph: RecursiveTaskGraphSummary,
        detail: RecursiveDagSelectedGraphData,
        caps: rsi_common::rpc::DaemonCapabilities,
    ) -> RecursiveDagBrowserState {
        let graph_id = graph.id;
        RecursiveDagBrowserState::ready(None, caps, vec![graph], Some(graph_id), Some(detail))
    }

    fn deferred_graph(graph_id: RecursiveTaskGraphId) -> RecursiveDeferredRecoveryGraph {
        RecursiveDeferredRecoveryGraph {
            graph_id: Some(graph_id),
            raw_graph_id: graph_id.to_string(),
            pass_id: RecursiveRecoveryPassId::new(),
            state: RecursiveGraphRecoveryState::Deferred,
            deferred_at: Utc::now(),
            reason: "store busy".to_string(),
            next_after: Some(Utc::now() + Duration::minutes(5)),
            last_attempted_at: None,
            last_error: Some("retry later".to_string()),
        }
    }

    fn live_recovery_readback(
        graph_id: RecursiveTaskGraphId,
    ) -> rsi_common::RecursiveLiveRecoveryReadback {
        rsi_common::RecursiveLiveRecoveryReadback {
            graph_recovery: Some(rsi_common::RecursiveGraphRecoveryStatus {
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
            live_attempts: Vec::new(),
            heartbeat_states: Vec::new(),
            scheduler_runs: Vec::new(),
            linked_sessions: Vec::new(),
            deferred_graph: Some(deferred_graph(graph_id)),
            operator_review_required: true,
            warnings: Vec::new(),
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
            heartbeat_at: Some(Utc::now() - Duration::minutes(10)),
            heartbeat_expires_at: Some(Utc::now() - Duration::minutes(1)),
            session_id: Some(uuid::Uuid::new_v4()),
            failure_reason: None,
            interruption_reason: None,
            cancellation_reason: None,
            recovery_reason: Some("lease expired".to_string()),
            error: None,
        }
    }

    fn scheduler_run(
        graph_id: RecursiveTaskGraphId,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> RecursiveSchedulerRunSummary {
        RecursiveSchedulerRunSummary {
            id: RecursiveSchedulerRunId::new(),
            graph_id,
            status: RecursiveSchedulerRunStatus::Running,
            source: RecursiveSchedulerRunSource::ManualRpc,
            operator: Some("test".to_string()),
            started_at: Utc::now() - Duration::minutes(15),
            completed_at: None,
            stop_reason: Some(RecursiveSchedulerStopReason::IdleNoRunnable),
            step_count: 1,
            max_steps: 8,
            executor_mode: RecursiveExecutionMode::Fake,
            failure_reason: None,
            cancellation_request_id: None,
            cancellation_reason: None,
            lease_owner: Some("worker".to_string()),
            lease_token: None,
            lease_heartbeat_at: Some(Utc::now() - Duration::minutes(10)),
            lease_expires_at: Some(expires_at),
            report_artifact_id: None,
        }
    }

    fn live_interrupt(
        graph_id: RecursiveTaskGraphId,
        task_id: RecursiveTaskId,
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
            session_id: Some(uuid::Uuid::new_v4()),
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
        task_id: RecursiveTaskId,
        live_attempt_id: RecursiveLiveAttemptId,
        latest_interrupt: RecursiveLiveInterruptSummary,
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
                session_id: Some(uuid::Uuid::new_v4()),
                provider: None,
                model: Some("model".to_string()),
                sandbox_kind: None,
                sandbox_root: None,
                sandbox_branch: None,
                sandbox_worktree_id: None,
                execution_mode: RecursiveExecutionMode::LiveSession,
                status: RecursiveLiveAttemptStatus::Running,
                recovery_status: RecursiveLiveRecoveryStatus::None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                started_at: Some(Utc::now()),
                launched_at: Some(Utc::now()),
                completed_at: None,
            },
            heartbeat: None,
            latest_interrupt: Some(latest_interrupt),
            latest_validation: None,
        }
    }

    fn execution_artifact(
        graph_id: RecursiveTaskGraphId,
        task_id: RecursiveTaskId,
        id: i64,
        label: &str,
    ) -> RecursiveExecutionArtifact {
        RecursiveExecutionArtifact {
            id,
            graph_id,
            task_id,
            attempt_id: Some(RecursiveAttemptId::new()),
            kind: RecursiveExecutionArtifactKind::File,
            label: label.to_string(),
            content: Some("line one\nline two\nline three".to_string()),
            uri: Some(format!("file:///tmp/{label}.txt")),
            metadata: serde_json::json!({
                "role": label,
                "zeta": "last",
                "alpha": "first"
            }),
            created_at: Utc::now(),
        }
    }

    fn artifact_summary(
        graph_id: RecursiveTaskGraphId,
        task_id: RecursiveTaskId,
        id: i64,
        label: &str,
    ) -> RecursiveExecutionArtifactSummary {
        RecursiveExecutionArtifactSummary {
            artifact_id: id,
            graph_id,
            task_id,
            attempt_id: Some(RecursiveAttemptId::new()),
            live_attempt_id: None,
            scheduler_run_id: None,
            validation_id: None,
            role: None,
            kind: RecursiveExecutionArtifactKind::File,
            label: label.to_string(),
            uri_display: Some(format!("file:///tmp/{label}.txt")),
            content_presence: RecursiveArtifactContentPresence::Uri,
            content_type: None,
            size_bytes: Some(128),
            digest: None,
            preview_state: RecursiveArtifactPreviewState::Unavailable,
            metadata_state: RecursiveArtifactMetadataState::Valid,
            created_at: Utc::now(),
        }
    }

    fn artifact_readback(
        graph_id: RecursiveTaskGraphId,
        task_id: RecursiveTaskId,
        id: i64,
        label: &str,
    ) -> RecursiveExecutionArtifactReadback {
        let artifact = execution_artifact(graph_id, task_id, id, label);
        RecursiveExecutionArtifactReadback {
            owners: RecursiveArtifactOwners {
                graph_id,
                task_id,
                attempt_id: artifact.attempt_id,
                live_attempt_id: None,
                scheduler_run_id: None,
                validation_id: None,
            },
            artifact,
            role: Some(RecursiveArtifactRole::ProducedArtifact),
            provenance: RecursiveArtifactProvenance::DaemonRecorded,
            preview: RecursiveArtifactPreviewAvailability {
                state: RecursiveArtifactPreviewState::Unavailable,
                reason: Some("preview RPC unavailable".to_string()),
            },
            metadata_state: RecursiveArtifactMetadataState::Valid,
            warnings: Vec::new(),
        }
    }

    fn inline_artifact_preview(
        graph_id: RecursiveTaskGraphId,
        id: i64,
        label: &str,
        text: &str,
        truncated: bool,
    ) -> RecursiveExecutionArtifactPreview {
        RecursiveExecutionArtifactPreview {
            artifact_id: id,
            graph_id,
            kind: RecursiveExecutionArtifactKind::Inline,
            label: label.to_string(),
            content_state: if truncated {
                RecursiveArtifactPreviewState::Truncated
            } else {
                RecursiveArtifactPreviewState::Available
            },
            content_type: Some("text/plain".to_string()),
            charset: Some("utf-8".to_string()),
            digest: Some("sha256:abc".to_string()),
            digest_algorithm: Some("sha256".to_string()),
            total_bytes: Some(text.len() as u64 + u64::from(truncated) * 5),
            total_lines: Some(text.lines().count() as u64 + u64::from(truncated)),
            byte_range: Some(rsi_common::RecursiveByteRange {
                start: 0,
                end: text.len() as u64,
            }),
            line_range: Some(rsi_common::RecursiveLineRange {
                start: 0,
                end: text.lines().count() as u64,
            }),
            shown_bytes: text.len() as u64,
            shown_lines: text.lines().count() as u64,
            text: Some(text.to_string()),
            binary_unavailable_reason: None,
            truncated,
            truncated_by_bytes: truncated,
            truncated_by_lines: false,
            omitted_bytes: truncated.then_some(5),
            omitted_lines: truncated.then_some(1),
            applied_max_bytes: crate::types::RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_BYTES,
            applied_max_lines: crate::types::RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_LINES,
            warnings: Vec::new(),
        }
    }

    fn unavailable_artifact_preview(
        graph_id: RecursiveTaskGraphId,
        id: i64,
        label: &str,
        content_state: RecursiveArtifactPreviewState,
        reason: &str,
    ) -> RecursiveExecutionArtifactPreview {
        RecursiveExecutionArtifactPreview {
            artifact_id: id,
            graph_id,
            kind: RecursiveExecutionArtifactKind::File,
            label: label.to_string(),
            content_state,
            content_type: None,
            charset: None,
            digest: None,
            digest_algorithm: None,
            total_bytes: None,
            total_lines: None,
            byte_range: None,
            line_range: None,
            shown_bytes: 0,
            shown_lines: 0,
            text: None,
            binary_unavailable_reason: Some(reason.to_string()),
            truncated: false,
            truncated_by_bytes: false,
            truncated_by_lines: false,
            omitted_bytes: None,
            omitted_lines: None,
            applied_max_bytes: crate::types::RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_BYTES,
            applied_max_lines: crate::types::RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_LINES,
            warnings: Vec::new(),
        }
    }

    fn validation_issue(message: &str) -> RecursiveLiveOutputValidationIssue {
        RecursiveLiveOutputValidationIssue {
            code: RecursiveLiveValidationIssueCode::MissingRequiredField,
            severity: RecursiveLiveValidationIssueSeverity::Error,
            class: RecursiveLiveValidationIssueClass::Missing,
            location: Some(
                rsi_common::RecursiveLiveValidationIssueLocation::OutputPath {
                    path: "tests[0].status".to_string(),
                },
            ),
            message: message.to_string(),
            evidence: Vec::new(),
            suggested_next_action: Some("rerun the failed validation".to_string()),
            metadata: serde_json::Value::Null,
        }
    }

    fn validation_item(
        graph_id: RecursiveTaskGraphId,
        task_id: RecursiveTaskId,
        live_attempt_id: RecursiveLiveAttemptId,
    ) -> RecursiveLiveOutputValidationListItem {
        RecursiveLiveOutputValidationListItem {
            summary: rsi_common::RecursiveLiveOutputValidationSummary {
                validation_id: Some(rsi_common::RecursiveLiveOutputValidationId::new()),
                live_attempt_id,
                graph_id,
                task_id,
                scheduler_run_id: RecursiveSchedulerRunId::new(),
                attempt_id: RecursiveAttemptId::new(),
                session_id: None,
                status: RecursiveLiveOutputValidationStatus::Invalid,
                output_kind: None,
                mapping_decision: None,
                retry_decision: None,
                raw_output_artifact_id: Some(10),
                normalized_output_artifact_id: Some(11),
                validation_artifact_id: Some(12),
                normalized_digest: Some("digest".to_string()),
                issue_count: 1,
                error_count: 1,
                warning_count: 0,
                info_count: 0,
                created_at: Some(Utc::now()),
            },
            artifact_links: RecursiveLiveOutputValidationArtifactLinks {
                raw_output_artifact_id: Some(10),
                normalized_output_artifact_id: Some(11),
                validation_artifact_id: Some(12),
                produced_artifact_ids: Vec::new(),
                test_artifact_ids: vec![13],
                diff_artifact_ids: vec![14],
            },
            issues: Some(vec![validation_issue("missing required test status")]),
        }
    }

    #[test]
    fn recovery_and_deferred_rendering_includes_quarantine_and_deferred_state() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let mut graph = graph_summary(graph_id, root_task_id);
        graph.quarantined_at = Some(Utc::now());
        graph.quarantine_reason = Some("operator review".to_string());
        graph.malformed_reason = Some("missing root".to_string());
        let mut detail = detail_for(graph.clone());
        detail.recovery_status = Some(rsi_common::RecursiveDagRecoveryStatus {
            latest_pass: None,
            graph_status: None,
            deferred_graphs: vec![deferred_graph(graph_id)],
            deferred_graph_count: 1,
            oldest_deferred_graph: None,
        });

        let text = lines_text(&state_with(graph, detail, caps(true, true)));

        assert!(text.contains("RECOVERY / DEFERRED / QUARANTINE"));
        assert!(text.contains("graph health"));
        assert!(text.contains("store busy"));
        assert!(text.contains("retry later"));
    }

    #[test]
    fn cancellation_rendering_shows_open_and_rejected_requests() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.cancellation_requests = vec![
            cancellation_request(graph_id, RecursiveCancellationRequestStatus::Requested),
            cancellation_request(graph_id, RecursiveCancellationRequestStatus::Rejected),
        ];
        let mut state = state_with(graph, detail, caps(true, true));
        state.panel = RecursiveDagPanel::Cancellations;
        state.selected_cancellation = 1;

        let text = lines_text(&state);

        assert!(text.contains("CANCELLATION REQUESTS"));
        assert!(text.contains("open=1 applied=0 rejected=1"));
        assert!(text.contains("operator requested stop"));
        assert!(text.contains("already terminal"));
    }

    #[test]
    fn stale_heartbeat_warning_rendering_includes_live_and_scheduler_leases() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.scheduler_runs = vec![scheduler_run(graph_id, Utc::now() - Duration::minutes(1))];
        detail.stale_heartbeats = vec![heartbeat(
            RecursiveLiveAttemptId::new(),
            RecursiveLiveAttemptHeartbeatStatus::Stale,
        )];
        let state = state_with(graph, detail, caps(true, true));

        let text = lines_text(&state);

        assert!(text.contains("HEARTBEATS / LEASES"));
        assert!(text.contains("lease warn"));
        assert!(text.contains("heartbeat warn"));
        assert!(text.contains("Stale"));
        assert!(text.contains("lease expired"));
    }

    #[test]
    fn interrupt_failure_and_success_rendering_counts_terminal_outcomes() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        let interrupted_id = RecursiveLiveAttemptId::new();
        let failed_id = RecursiveLiveAttemptId::new();
        detail.live_attempts = vec![
            live_attempt_item(
                graph_id,
                root_task_id,
                interrupted_id,
                live_interrupt(
                    graph_id,
                    root_task_id,
                    interrupted_id,
                    RecursiveLiveInterruptStatus::Interrupted,
                ),
            ),
            live_attempt_item(
                graph_id,
                root_task_id,
                failed_id,
                live_interrupt(
                    graph_id,
                    root_task_id,
                    failed_id,
                    RecursiveLiveInterruptStatus::Failed,
                ),
            ),
        ];
        let mut state = state_with(graph, detail, caps(true, true));
        state.panel = RecursiveDagPanel::Interrupts;
        state.selected_interrupt = 1;

        let text = lines_text(&state);

        assert!(text.contains("LIVE INTERRUPTS"));
        assert!(text.contains("interrupted=1 failed=1"));
        assert!(text.contains("provider rejected interrupt"));
    }

    #[test]
    fn empty_status_panel_rendering_is_explicit() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.recovery_status = Some(rsi_common::RecursiveDagRecoveryStatus {
            latest_pass: None,
            graph_status: None,
            deferred_graphs: Vec::new(),
            deferred_graph_count: 0,
            oldest_deferred_graph: None,
        });

        let text = lines_text(&state_with(graph, detail, caps(true, true)));

        assert!(text.contains("no deferred recovery graphs"));
        assert!(text.contains("no cancellation requests"));
        assert!(text.contains("no live heartbeat readbacks"));
        assert!(text.contains("no live interrupt readbacks"));
    }

    #[test]
    fn capability_disabled_status_panels_degrade_without_healthy_claims() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let detail = detail_for(graph.clone());

        let text = lines_text(&state_with(graph, detail, caps(false, false)));

        assert!(text.contains("recovery status capability disabled"));
        assert!(text.contains("live readback unavailable: daemon capability disabled"));
        assert!(text.contains("live interrupt readback unavailable"));
    }

    #[test]
    fn fake_scheduler_control_renders_unavailable_when_capability_disabled() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let detail = detail_for(graph.clone());

        let text = lines_text(&state_with(graph, detail, caps(true, true)));

        assert!(text.contains("FAKE run"));
        assert!(text.contains("recursive_dag_scheduler_control=false"));
        assert!(!text.contains("FAKE running"));
    }

    #[test]
    fn fake_scheduler_control_renders_unavailable_when_daemon_disconnected() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let detail = detail_for(graph.clone());
        let mut caps = caps(true, true);
        caps.recursive_dag_scheduler_control = true;

        let text = lines_text_with_connection(&state_with(graph, detail, caps), false);

        assert!(text.contains("FAKE run"));
        assert!(text.contains("daemon disconnected"));
        assert!(!text.contains("R prompts for explicit max_steps"));
    }

    #[test]
    fn fake_scheduler_input_rendering_requires_max_steps() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let detail = detail_for(graph.clone());
        let mut caps = caps(true, true);
        caps.recursive_dag_scheduler_control = true;
        let mut state = state_with(graph, detail, caps);
        state.fake_run = RecursiveDagFakeRunState::MaxStepsInput {
            input: String::new(),
            error: Some("max_steps is required for FAKE scheduler runs".to_string()),
        };

        let text = lines_text(&state);

        assert!(text.contains("FAKE max_steps"));
        assert!(text.contains("<required>"));
        assert!(text.contains("input error"));
        assert!(text.contains("Enter submit"));
    }

    #[test]
    fn cancellation_and_recovery_disabled_gates_render_missing_capabilities() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let detail = detail_for(graph.clone());

        let text = lines_text(&state_with(graph, detail, caps(true, true)));

        assert!(text.contains("recursive_dag_cancellation_control=false"));
        assert!(text.contains("recursive_dag_recovery_control=false"));
        assert!(text.contains("C t task cancellation reserved"));
    }

    #[test]
    fn cancellation_gate_renders_terminal_graph_disabled() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let mut graph = graph_summary(graph_id, root_task_id);
        graph.status = RecursiveGraphStatus::Terminal;
        let detail = detail_for(graph.clone());
        let mut caps = caps(true, true);
        caps.recursive_dag_cancellation_control = true;

        let text = lines_text(&state_with(graph, detail, caps));

        assert!(text.contains("selected graph not cancellable: Terminal"));
        assert!(!text.contains("enabled: C g graph / C r run"));
    }

    #[test]
    fn cancellation_reason_prompts_render_specific_titles_and_errors() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let detail = detail_for(graph.clone());
        let mut caps = caps(true, true);
        caps.recursive_dag_cancellation_control = true;
        let mut state = state_with(graph, detail, caps);
        state.control = RecursiveDagControlState::GraphReasonInput {
            graph_id,
            input: String::new(),
            error: Some("reason is required".to_string()),
        };

        let text = lines_text(&state);

        assert!(text.contains("CANCEL GRAPH"));
        assert!(text.contains("<required>"));
        assert!(text.contains("reason is required"));

        state.control = RecursiveDagControlState::RunReasonInput {
            run_id: RecursiveSchedulerRunId::new(),
            input: "stop run".to_string(),
            error: None,
        };
        let text = lines_text(&state);
        assert!(text.contains("CANCEL RUN"));
        assert!(text.contains("stop run"));
    }

    #[test]
    fn recovery_budget_prompt_renders_zero_time_budget_as_explicit_value() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let detail = detail_for(graph.clone());
        let mut caps = caps(true, true);
        caps.recursive_dag_recovery_control = true;
        let mut state = state_with(graph, detail, caps);
        state.control = RecursiveDagControlState::RecoveryBudgetInput {
            max_graphs_input: "1".to_string(),
            time_budget_ms_input: "0".to_string(),
            field: RecursiveDagRecoveryInputField::TimeBudgetMs,
            error: None,
        };

        let text = lines_text(&state);

        assert!(text.contains("RECOVER"));
        assert!(text.contains("max_graphs=1"));
        assert!(text.contains("time_budget_ms=0"));
        assert!(text.contains("manual recovery pass only"));
    }

    #[test]
    fn control_result_warning_message_renders_rejected_status() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let detail = detail_for(graph.clone());
        let mut state = state_with(graph, detail, caps(true, true));
        state.message = Some(
            "run cancellation warning: request=abcd scope=Run status=Rejected requested_by=rsi-tui rejection=already terminal"
                .to_string(),
        );

        let text = lines_text(&state);

        assert!(text.contains("run cancellation warning"));
        assert!(text.contains("status=Rejected"));
        assert!(text.contains("already terminal"));
    }

    #[test]
    fn fake_scheduler_rpc_error_message_renders_as_warning() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let detail = detail_for(graph.clone());
        let mut caps = caps(true, true);
        caps.recursive_dag_scheduler_control = true;
        let mut state = state_with(graph, detail, caps);
        state.message = Some("FAKE scheduler failed: RPC error (-32602): rejected".to_string());

        let text = lines_text(&state);

        assert!(text.contains("FAKE scheduler failed"));
        assert_eq!(state.warning_count(), 1);
    }

    #[test]
    fn control_rail_exposes_gated_cancellation_and_recovery_without_live_actions() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.recovery_status = Some(rsi_common::RecursiveDagRecoveryStatus {
            latest_pass: None,
            graph_status: None,
            deferred_graphs: vec![deferred_graph(graph_id)],
            deferred_graph_count: 1,
            oldest_deferred_graph: None,
        });
        let mut caps = caps(true, true);
        caps.recursive_dag_scheduler_control = true;
        caps.recursive_dag_cancellation_control = true;
        caps.recursive_dag_recovery_control = true;
        caps.recursive_dag_live_execution = true;
        caps.recursive_dag_background_loop = true;

        let text = lines_text(&state_with(graph, detail, caps));

        assert!(text.contains("R prompts for explicit max_steps"));
        assert!(text.contains("C prefix"));
        assert!(text.contains("c continue recovery"));
        assert!(text.contains("C t task cancellation reserved"));
        assert!(text.contains("no topology"));
        assert!(text.contains("background loop"));
    }

    #[test]
    fn degraded_artifact_inspector_renders_metadata_and_preview_unavailable() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "generic",
        ));
        let mut caps = caps(true, true);
        caps.recursive_dag_artifact_lookup = true;
        caps.recursive_dag_artifact_list_pagination = true;
        let mut state = state_with(graph, detail, caps);
        state.panel = RecursiveDagPanel::Artifacts;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        state.inspector.artifact_detail = Some(RecursiveDagArtifactDetailState {
            graph_id,
            artifact_id: 1,
            status: RecursiveDagInspectorLoadStatus::Ready,
            readback: Some(artifact_readback(graph_id, root_task_id, 1, "generic")),
            message: Some("loaded graph-guarded artifact detail 1".to_string()),
            warnings: Vec::new(),
        });

        let text = lines_text(&state);

        assert!(text.contains("ARTIFACTS / INSPECTORS"));
        assert!(text.contains("INSPECTOR (metadata)"));
        assert!(text.contains("bounded artifact preview unavailable"));
        assert!(text.contains("alpha"));
        assert!(text.contains("first"));

        state.inspector.view = RecursiveDagInspectorView::Preview;
        let text = lines_text(&state);
        assert!(text.contains("bounded artifact preview unavailable"));
        assert!(!text.contains("line one"));
    }

    #[test]
    fn preview_unavailable_rendering_distinguishes_capability_state() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "generic",
        ));
        let mut caps = caps(true, true);
        caps.recursive_dag_artifact_lookup = true;
        caps.recursive_dag_artifact_list_pagination = true;
        let mut state = state_with(graph, detail, caps);
        state.panel = RecursiveDagPanel::Artifacts;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        state.inspector.view = RecursiveDagInspectorView::Preview;
        state.inspector.artifact_detail = Some(RecursiveDagArtifactDetailState {
            graph_id,
            artifact_id: 1,
            status: RecursiveDagInspectorLoadStatus::Ready,
            readback: Some(artifact_readback(graph_id, root_task_id, 1, "generic")),
            message: Some("loaded graph-guarded artifact detail 1".to_string()),
            warnings: Vec::new(),
        });

        let disabled_text = lines_text(&state);
        assert!(disabled_text.contains("recursive_dag_artifact_preview_inspection=false"));
        assert!(!disabled_text.contains("line one"));

        state
            .capabilities
            .as_mut()
            .expect("capabilities")
            .recursive_dag_artifact_preview_inspection = true;
        let degraded_text = lines_text(&state);
        assert!(degraded_text.contains("bounded artifact preview not loaded"));
        assert!(!degraded_text.contains("recursive_dag_artifact_preview_inspection=false"));
        assert!(!degraded_text.contains("line one"));
    }

    #[test]
    fn inline_preview_rendering_includes_truncation_metadata() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        let mut summary = artifact_summary(graph_id, root_task_id, 1, "inline");
        summary.preview_state = RecursiveArtifactPreviewState::Truncated;
        detail.artifact_summary_page.items.push(summary);
        let mut caps = caps(true, true);
        caps.recursive_dag_artifact_preview_inspection = true;
        let mut state = state_with(graph, detail, caps);
        state.panel = RecursiveDagPanel::Artifacts;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        state.inspector.view = RecursiveDagInspectorView::Preview;
        state.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState {
            graph_id,
            artifact_id: 1,
            status: RecursiveDagInspectorLoadStatus::Ready,
            preview: Some(inline_artifact_preview(
                graph_id,
                1,
                "inline",
                "alpha\nbeta",
                true,
            )),
            message: Some("loaded bounded artifact preview 1".to_string()),
            warnings: Vec::new(),
        });

        let text = lines_text(&state);

        assert!(text.contains("state=Truncated"));
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));
        assert!(text.contains("truncated"));
        assert!(text.contains("omitted_bytes=5"));
        assert!(text.contains("caps=16384B/80L"));
    }

    #[test]
    fn uri_and_unsupported_preview_render_as_structured_unavailable_states() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "external",
        ));
        let mut caps = caps(true, true);
        caps.recursive_dag_artifact_preview_inspection = true;
        let mut state = state_with(graph, detail, caps);
        state.panel = RecursiveDagPanel::Artifacts;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        state.inspector.view = RecursiveDagInspectorView::Preview;
        state.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState {
            graph_id,
            artifact_id: 1,
            status: RecursiveDagInspectorLoadStatus::Ready,
            preview: Some(unavailable_artifact_preview(
                graph_id,
                1,
                "external",
                RecursiveArtifactPreviewState::UriBlocked,
                "artifact preview will not read URI or filesystem-backed content",
            )),
            message: Some("loaded bounded artifact preview 1".to_string()),
            warnings: vec!["RECURSIVE_ARTIFACT_URI_PREVIEW_BLOCKED: blocked".to_string()],
        });

        let uri_text = lines_text(&state);
        assert!(uri_text.contains("state=UriBlocked"));
        assert!(uri_text.contains("filesystem-backed content"));
        assert!(uri_text.contains("preview unavailable"));
        assert!(!uri_text.contains("filesystem content must not leak"));

        state.inspector.artifact_preview = Some(RecursiveDagArtifactPreviewState {
            graph_id,
            artifact_id: 1,
            status: RecursiveDagInspectorLoadStatus::Ready,
            preview: Some(unavailable_artifact_preview(
                graph_id,
                1,
                "external",
                RecursiveArtifactPreviewState::UnsupportedKind,
                "artifact kind has no inline/database-owned content to preview",
            )),
            message: Some("loaded bounded artifact preview 1".to_string()),
            warnings: vec!["RECURSIVE_ARTIFACT_PREVIEW_UNSUPPORTED_KIND: unsupported".to_string()],
        });

        let unsupported_text = lines_text(&state);
        assert!(unsupported_text.contains("state=UnsupportedKind"));
        assert!(unsupported_text.contains("inline/database-owned content"));
        assert!(unsupported_text.contains("preview warn"));
    }

    #[test]
    fn malformed_metadata_warning_renders_inside_artifact_inspector() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        let mut summary = artifact_summary(graph_id, root_task_id, 9, "malformed");
        summary.metadata_state = RecursiveArtifactMetadataState::Malformed;
        detail.artifact_summary_page.items.push(summary);
        detail
            .artifact_summary_page
            .warnings
            .push("RECURSIVE_ARTIFACT_METADATA_MALFORMED: bad json".to_string());
        let mut state = state_with(graph, detail, caps(true, true));
        state.panel = RecursiveDagPanel::Artifacts;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());

        let text = lines_text(&state);

        assert!(text.contains("metadata warn"));
        assert!(text.contains("malformed artifact metadata"));
        assert!(text.contains("artifact warn"));
    }

    #[test]
    fn artifact_summary_end_of_list_rendering_is_explicit() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "final",
        ));
        detail.artifact_summary_page.has_more = false;
        detail.artifact_summary_page.next_cursor = None;
        let mut caps = caps(true, true);
        caps.recursive_dag_artifact_list_pagination = true;
        let mut state = state_with(graph, detail, caps);
        state.panel = RecursiveDagPanel::Artifacts;

        let text = lines_text(&state);

        assert!(text.contains("end of artifact summary list"));
        assert!(!text.contains("next-page controls are not exposed"));
    }

    #[test]
    fn artifact_summary_page_control_states_render_explicitly() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let render_page = |page: crate::types::RecursiveDagArtifactSummaryPageState| {
            let graph = graph_summary(graph_id, root_task_id);
            let mut detail = detail_for(graph.clone());
            detail.artifact_summary_page = page;
            let mut caps = caps(true, true);
            caps.recursive_dag_artifact_list_pagination = true;
            let mut state = state_with(graph, detail, caps);
            state.panel = RecursiveDagPanel::Artifacts;
            lines_text(&state)
        };

        let mut loading = crate::types::RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            vec![artifact_summary(graph_id, root_task_id, 1, "first")],
            crate::types::RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );
        loading.mark_loading_more();
        assert!(render_page(loading).contains("loading next artifact summary page"));

        let mut error = crate::types::RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            vec![artifact_summary(graph_id, root_task_id, 1, "first")],
            crate::types::RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );
        error.status = RecursiveDagInspectorLoadStatus::Error;
        error.message = Some("artifact summary load-more unavailable: socket closed".to_string());
        let error_text = render_page(error);
        assert!(error_text.contains("artifact summary load-more error is local"));
        assert!(error_text.contains("retry load-more"));

        let mut page_cap = crate::types::RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            vec![artifact_summary(graph_id, root_task_id, 1, "first")],
            crate::types::RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );
        page_cap.loaded_pages = crate::types::RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT;
        page_cap.page_cap_reached = true;
        assert!(render_page(page_cap).contains("local artifact page cap reached"));

        let disabled =
            crate::types::RecursiveDagArtifactSummaryPageState::capability_disabled(graph_id);
        assert!(render_page(disabled).contains("artifact summary pagination capability disabled"));

        let load_more = crate::types::RecursiveDagArtifactSummaryPageState::ready(
            graph_id,
            vec![artifact_summary(graph_id, root_task_id, 1, "first")],
            crate::types::RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
            Some("cursor-1".to_string()),
            true,
            Vec::new(),
        );
        assert!(render_page(load_more).contains("select the load-more row and press Enter"));
    }

    #[test]
    fn validation_detail_rendering_uses_issue_readbacks() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let live_attempt_id = RecursiveLiveAttemptId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        let item = validation_item(graph_id, root_task_id, live_attempt_id);
        detail.validation_results.push(item.clone());
        let mut caps = caps(true, true);
        caps.recursive_dag_live_validation_inspection = true;
        let mut state = state_with(graph, detail, caps);
        state.panel = RecursiveDagPanel::Artifacts;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        state.inspector.validation_detail = Some(RecursiveDagValidationDetailState {
            validation_id: item.summary.validation_id,
            live_attempt_id,
            status: RecursiveDagInspectorLoadStatus::Ready,
            result: Some(RecursiveLiveOutputValidationResult {
                summary: item.summary,
                artifact_links: item.artifact_links,
                parser_source: None,
                issues: vec![validation_issue("missing required test status")],
                normalized_output: None,
                validation_report: None,
                metadata: serde_json::Value::Null,
            }),
            issues: vec![validation_issue("missing required test status")],
            message: Some("loaded validation detail and 1 issue row".to_string()),
        });

        let text = lines_text(&state);

        assert!(text.contains("INSPECTOR (links)"));
        assert!(text.contains("loaded validation detail"));
        assert!(text.contains("missing required test status"));
        assert!(text.contains("next action"));
        assert!(text.contains("normalized"));
    }

    #[test]
    fn selected_live_attempt_artifact_bucket_rendering_is_bounded_and_grouped() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let live_attempt_id = RecursiveLiveAttemptId::new();
        let mut detail = detail_for(graph.clone());
        detail.live_attempts.push(live_attempt_item(
            graph_id,
            root_task_id,
            live_attempt_id,
            live_interrupt(
                graph_id,
                root_task_id,
                live_attempt_id,
                RecursiveLiveInterruptStatus::Requested,
            ),
        ));
        let mut state = state_with(graph, detail, caps(true, true));
        state.panel = RecursiveDagPanel::Artifacts;
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
            message: Some("loaded 3 selected live attempt artifact row(s)".to_string()),
            warnings: Vec::new(),
        });

        let text = lines_text(&state);

        assert!(text.contains("live buckets"));
        assert!(text.contains("live:prompt"));
        assert!(text.contains("live:diff"));
        assert!(text.contains("live:test"));
        assert!(text.contains("loaded 3 selected live attempt artifact row"));
    }

    #[test]
    fn unavailable_preview_test_diff_and_report_states_are_explicit() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        let run = scheduler_run(graph_id, Utc::now() + Duration::minutes(5));
        let run_id = run.id;
        let mut report_summary = artifact_summary(graph_id, root_task_id, 5, "report");
        report_summary.role = Some(RecursiveArtifactRole::SchedulerReport);
        report_summary.scheduler_run_id = Some(run_id);
        detail.artifact_summary_page.items.push(report_summary);
        detail.selected_run_detail = Some(rsi_common::RecursiveSchedulerRunDetail {
            run,
            graph: graph.clone(),
            active_attempts: Vec::new(),
            cancellation_requests: Vec::new(),
            events: Vec::new(),
            report_artifact: Some(execution_artifact(graph_id, root_task_id, 5, "report")),
            live_attempts: Vec::new(),
            live_heartbeat_states: Vec::new(),
            live_interrupts: Vec::new(),
            latest_live_validations: Vec::new(),
        });
        let live_attempt_id = RecursiveLiveAttemptId::new();
        detail.live_attempts.push(live_attempt_item(
            graph_id,
            root_task_id,
            live_attempt_id,
            live_interrupt(
                graph_id,
                root_task_id,
                live_attempt_id,
                RecursiveLiveInterruptStatus::Requested,
            ),
        ));
        let mut state = state_with(graph, detail, caps(true, true));
        state.panel = RecursiveDagPanel::Artifacts;
        state.inspector.live_attempt_artifacts = Some(RecursiveDagLiveAttemptArtifactsState {
            graph_id,
            live_attempt_id,
            status: RecursiveDagInspectorLoadStatus::Ready,
            artifacts: Some(RecursiveLiveAttemptArtifactReadback {
                prompt_artifact: None,
                raw_output_artifacts: Vec::new(),
                normalized_output_artifact: None,
                validation_artifacts: Vec::new(),
                diff_artifacts: vec![execution_artifact(graph_id, root_task_id, 6, "diff")],
                test_artifacts: vec![execution_artifact(graph_id, root_task_id, 7, "test")],
                produced_artifacts: Vec::new(),
            }),
            message: None,
            warnings: Vec::new(),
        });

        let rows = state.inspector_rows();
        let Some(report_idx) = rows
            .iter()
            .position(|row| row.kind == RecursiveDagInspectorRowKind::Report)
        else {
            panic!("report row");
        };
        state.selected_artifact = report_idx;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        state.inspector.view = RecursiveDagInspectorView::Report;
        let text = lines_text(&state);
        assert!(text.contains("typed scheduler report inspector unavailable"));

        let Some(diff_idx) = state
            .inspector_rows()
            .iter()
            .position(|row| row.kind == RecursiveDagInspectorRowKind::Diff)
        else {
            panic!("diff row");
        };
        state.selected_artifact = diff_idx;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        state.inspector.view = RecursiveDagInspectorView::Diff;
        let text = lines_text(&state);
        assert!(text.contains("typed diff hunks unavailable"));

        let Some(test_idx) = state
            .inspector_rows()
            .iter()
            .position(|row| row.kind == RecursiveDagInspectorRowKind::Test)
        else {
            panic!("test row");
        };
        state.selected_artifact = test_idx;
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        state.inspector.view = RecursiveDagInspectorView::Test;
        let text = lines_text(&state);
        assert!(text.contains("typed test detail unavailable"));

        state.inspector.view = RecursiveDagInspectorView::Preview;
        let text = lines_text(&state);
        assert!(text.contains("bounded artifact preview unavailable"));
    }

    #[test]
    fn inspector_capability_disabled_rendering_keeps_artifact_rows_visible() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.artifact_summary_page.items.push(artifact_summary(
            graph_id,
            root_task_id,
            1,
            "generic",
        ));
        let mut state = state_with(graph, detail, caps(false, true));
        state.panel = RecursiveDagPanel::Artifacts;

        let text = lines_text(&state);

        assert!(text.contains("live status capability disabled"));
        assert!(text.contains("live validation capability disabled"));
        assert!(text.contains("generic"));
    }

    #[test]
    fn recovery_panel_shows_live_partial_data_when_recovery_capability_is_disabled() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let mut detail = detail_for(graph.clone());
        detail.live_recovery_status = Some(live_recovery_readback(graph_id));

        let text = lines_text(&state_with(graph, detail, caps(true, false)));

        assert!(text.contains("recovery status capability disabled"));
        assert!(text.contains("live recovery"));
        assert!(text.contains("operator_review=on"));
        assert!(text.contains("live graph"));
        assert!(text.contains("state=Quarantined"));
        assert!(text.contains("count=unknown rows=1"));
        assert!(text.contains("store busy"));
        assert!(text.contains("retry later"));
    }

    #[test]
    fn failed_interrupt_long_reason_is_truncated_safely() {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let graph = graph_summary(graph_id, root_task_id);
        let live_attempt_id = RecursiveLiveAttemptId::new();
        let mut interrupt = live_interrupt(
            graph_id,
            root_task_id,
            live_attempt_id,
            RecursiveLiveInterruptStatus::Failed,
        );
        let long_reason = "provider interrupt failed after repeated attempts because the linked session was already terminal and the provider rejected the stale token";
        interrupt.reason = long_reason.to_string();
        interrupt.failure_reason = Some(long_reason.to_string());
        let mut detail = detail_for(graph.clone());
        detail.live_attempts = vec![live_attempt_item(
            graph_id,
            root_task_id,
            live_attempt_id,
            interrupt,
        )];
        let mut state = state_with(graph, detail, caps(true, true));
        state.panel = RecursiveDagPanel::Interrupts;

        let lines = rendered_lines(&state, 80);
        let failed_row = lines
            .iter()
            .find(|line| line.contains("Failed") && line.contains("provider"))
            .expect("failed interrupt row should render");
        let failure_detail = lines
            .iter()
            .find(|line| line.contains("failure") && line.contains("provider interrupt"))
            .expect("selected failure reason should render");

        assert!(failed_row.chars().count() <= 80, "{failed_row}");
        assert!(failed_row.contains('~'), "{failed_row}");
        assert!(failure_detail.chars().count() <= 80, "{failure_detail}");
        assert!(failure_detail.contains('~'), "{failure_detail}");
    }

    #[test]
    fn empty_recursive_dag_inventory_is_explicit() {
        let state = RecursiveDagBrowserState::ready(None, caps(true, true), Vec::new(), None, None);

        let text = lines_text(&state);

        assert!(text.contains("GRAPHS"));
        assert!(text.contains("no recursive DAG graphs found for this scope"));
        assert!(!text.contains("SELECTED GRAPH"));
    }
}
