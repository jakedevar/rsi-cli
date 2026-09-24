//! Graph review overlay — key handling for visual workflow editing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::types::{
    GraphBrowseMode, GraphCamera, GraphDraftPersistenceState, GraphEditField, GraphMode,
    GraphPickerKind, GraphViewOrigin, GraphViewport, OverlayState, RecursiveDagBrowserState,
    RecursiveDagInspectorView, RecursiveDagPanel,
};
use crate::ui::overlay::graph_layout::{
    GraphDirection, HORIZONTAL_PAN_STEP, VERTICAL_PAN_STEP, find_directional_neighbor,
    layout_workflow_graph,
};
use rsi_common::types::{Workflow, WorkflowDocument, WorkflowStage};
use rsi_graph::format::{NodeType, WorkflowDefinition};
use rsi_graph::generate::templates::{build_starter_workflow, starter_templates};
use uuid::Uuid;

/// Open the graph review overlay and resume the active draft when possible.
pub async fn open_graph_review(app: &mut App) {
    let selected_session_id = app.selected_session_id();
    let selected = app.selected_session_state();
    let selected_project_id = selected
        .and_then(|state| state.session.project_id)
        .or(app.current_project_id);
    // P1.3: derive topology on read instead of trusting `session.workflow_id`,
    // which is `None` on every spawned child after the kill-the-copy change.
    let selected_workflow_id = selected_session_id.and_then(|sid| app.effective_topology(sid));

    let draft_id = if let Some(draft_id) = app
        .active_graph_draft_id
        .filter(|draft_id| app.graph_drafts.contains_key(draft_id))
    {
        draft_id
    } else if let Some(workflow_id) = selected_workflow_id {
        if let Some(draft_id) = app.find_graph_draft_id_by_workflow_id(workflow_id) {
            draft_id
        } else {
            load_saved_workflow_draft(app, workflow_id, selected_project_id).await
        }
    } else {
        app.create_graph_draft(blank_workflow(), None, selected_project_id)
    };

    let _ = app.resync_graph_execution_for_draft(draft_id, true).await;
    let caps = app.client.get_daemon_capabilities().await.ok();
    let gv_info_dashboard = caps.map_or(false, |c| c.gv_info_dashboard);
    app.show_graph_review_draft(draft_id, gv_info_dashboard);
}

fn blank_workflow() -> WorkflowDefinition {
    WorkflowDefinition::new("Untitled Workflow")
}

async fn load_saved_workflow_draft(
    app: &mut App,
    workflow_id: Uuid,
    fallback_project_id: Option<Uuid>,
) -> Uuid {
    match app.client.get_workflow_definition(workflow_id).await {
        Ok(document) => {
            let WorkflowDocument {
                workflow: stored_workflow,
                definition,
            } = document;
            let project_id = stored_workflow.project_id.or(fallback_project_id);
            let workflow = match serde_json::from_value::<WorkflowDefinition>(definition) {
                Ok(workflow) => workflow,
                Err(error) => {
                    app.notify(format!("workflow definition decode failed: {}", error));
                    WorkflowDefinition::new(stored_workflow.title)
                }
            };
            app.create_graph_draft(workflow, Some(stored_workflow.id), project_id)
        }
        Err(error) => {
            let (name, project_id) = app
                .workflows
                .get(&workflow_id)
                .map(|workflow| (workflow.title.clone(), workflow.project_id))
                .unwrap_or_else(|| ("Untitled Workflow".to_string(), fallback_project_id));
            app.notify(format!("workflow load failed: {}", error));
            app.create_graph_draft(WorkflowDefinition::new(name), Some(workflow_id), project_id)
        }
    }
}

/// Handle keys in the graph review overlay.
pub async fn handle_graph_review_key(app: &mut App, key: KeyEvent) {
    app.reconcile_graph_review_execution_mode();

    let (draft_id, mode, dashboard_focused) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id,
            mode,
            dashboard_focused,
            ..
        } => (*draft_id, *mode, *dashboard_focused),
        _ => return,
    };

    if dashboard_focused {
        handle_dashboard_key(app, key).await;
        return;
    }

    if matches!(mode, GraphMode::EditField { .. }) {
        handle_edit_field_key(app, key).await;
        return;
    }

    if matches!(mode, GraphMode::Picker { .. }) {
        handle_picker_key(app, key).await;
        return;
    }

    let node_count = app
        .graph_draft(&draft_id)
        .map(|draft| draft.workflow.nodes.len())
        .unwrap_or(0);

    if node_count == 0 {
        handle_empty_graph_key(app, key).await;
        return;
    }

    if mode.shows_detail_panel() {
        handle_detail_key(app, key).await;
    } else {
        handle_navigate_key(app, key).await;
    }
}

async fn handle_empty_graph_key(app: &mut App, key: KeyEvent) {
    let mode = match graph_mode(app) {
        Some(mode) => mode,
        None => return,
    };

    match key.code {
        KeyCode::Char('t') if !mode.is_executing() => {
            open_graph_picker(app, GraphPickerKind::Topology);
        }
        KeyCode::Char('o') if !mode.is_executing() => {
            open_graph_picker(app, GraphPickerKind::SavedWorkflow);
        }
        KeyCode::Char('H') => pan_graph_view(app, -HORIZONTAL_PAN_STEP, 0),
        KeyCode::Char('J') => pan_graph_view(app, 0, VERTICAL_PAN_STEP),
        KeyCode::Char('K') => pan_graph_view(app, 0, -VERTICAL_PAN_STEP),
        KeyCode::Char('L') => pan_graph_view(app, HORIZONTAL_PAN_STEP, 0),
        KeyCode::Char('c') => {
            set_graph_camera(app, GraphCamera::FollowSelection);
        }
        KeyCode::Char('q') if mode.allows_overlay_close() => {
            app.overlay = OverlayState::None;
            app.mark_dirty();
        }
        _ => {}
    }
}

async fn handle_navigate_key(app: &mut App, key: KeyEvent) {
    let (draft_id, mode, view_origin, gv_info_dashboard) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id,
            mode,
            view_origin,
            gv_info_dashboard,
            ..
        } => (*draft_id, *mode, view_origin.clone(), *gv_info_dashboard),
        _ => return,
    };

    let is_authored = view_origin == GraphViewOrigin::AuthoredWorkflow;
    let read_only = mode.is_executing() || !is_authored;
    let node_count = app
        .graph_draft(&draft_id)
        .map(|draft| draft.workflow.nodes.len())
        .unwrap_or(0);

    match key.code {
        KeyCode::Tab => {
            if gv_info_dashboard {
                if let GraphViewOrigin::BridgedRecursive { recursive_graph_id } = view_origin {
                    focus_dashboard(app, recursive_graph_id);
                }
            }
        }
        KeyCode::Char('h') | KeyCode::Left => {
            move_selected_node_in_direction(app, draft_id, node_count, GraphDirection::Left);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            move_selected_node_in_direction(app, draft_id, node_count, GraphDirection::Down);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            move_selected_node_in_direction(app, draft_id, node_count, GraphDirection::Up);
        }
        KeyCode::Char('l') | KeyCode::Right => {
            let current = current_selected_node(app);
            let has_neighbor = current.and_then(|curr| {
                app.graph_draft(&draft_id).and_then(|draft| {
                    let layout = layout_workflow_graph(&draft.workflow);
                    find_directional_neighbor(&layout, curr, GraphDirection::Right)
                })
            });
            if has_neighbor.is_some() {
                move_selected_node_in_direction(app, draft_id, node_count, GraphDirection::Right);
            } else if gv_info_dashboard {
                if let GraphViewOrigin::BridgedRecursive { recursive_graph_id } = view_origin {
                    focus_dashboard(app, recursive_graph_id);
                }
            }
        }
        KeyCode::Char('[') => {
            navigate_predecessor(app);
        }
        KeyCode::Char(']') => {
            navigate_successor(app);
        }
        KeyCode::Char('g') => {
            move_selected_node_to(app, 0, node_count);
        }
        KeyCode::Char('G') => {
            move_selected_node_to(app, node_count.saturating_sub(1), node_count);
        }
        KeyCode::Enter => {
            set_graph_mode(app, mode.enter_detail());
        }
        KeyCode::Char('d') if !read_only => {
            delete_selected_node(app);
        }
        KeyCode::Char('u') if !read_only => {
            undo_edit(app);
        }
        KeyCode::Char('z') if !read_only => {
            toggle_collapse(app);
        }
        KeyCode::Char('t') if !read_only => {
            open_graph_picker(app, GraphPickerKind::Topology);
        }
        KeyCode::Char('o') if !read_only => {
            open_graph_picker(app, GraphPickerKind::SavedWorkflow);
        }
        KeyCode::Char('R') => {
            open_recursive_picker(app).await;
        }
        KeyCode::Char('r') if mode.allows_execution_start() && is_authored => {
            run_graph_draft(app, draft_id).await;
        }
        KeyCode::Char('x') if mode.is_executing() => {
            interrupt_graph_execution(app, draft_id).await;
        }
        KeyCode::Char('H') => pan_graph_view(app, -HORIZONTAL_PAN_STEP, 0),
        KeyCode::Char('J') => pan_graph_view(app, 0, VERTICAL_PAN_STEP),
        KeyCode::Char('K') => pan_graph_view(app, 0, -VERTICAL_PAN_STEP),
        KeyCode::Char('L') => pan_graph_view(app, HORIZONTAL_PAN_STEP, 0),
        KeyCode::Char('c') => {
            set_graph_camera(app, GraphCamera::FollowSelection);
        }
        KeyCode::Char('q') if mode.allows_overlay_close() => {
            app.overlay = OverlayState::None;
            app.mark_dirty();
        }
        _ => {}
    }
}

async fn handle_detail_key(app: &mut App, key: KeyEvent) {
    let (draft_id, selected_node, selected_edge, mode, is_authored) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id,
            selected_node,
            selected_edge,
            mode,
            view_origin,
            ..
        } => (
            *draft_id,
            *selected_node,
            *selected_edge,
            *mode,
            *view_origin == GraphViewOrigin::AuthoredWorkflow,
        ),
        _ => return,
    };

    let read_only = mode.is_executing() || !is_authored;

    match key.code {
        KeyCode::Char('i') if !mode.is_executing() => {
            let (editable, _) = node_is_editable(app, draft_id, selected_node);
            if editable && is_authored {
                begin_edit_field(app, draft_id, selected_node, GraphEditField::Name);
            }
        }
        KeyCode::Char('I') if !mode.is_executing() => {
            let (editable, _) = node_is_editable(app, draft_id, selected_node);
            if editable {
                begin_edit_field(app, draft_id, selected_node, GraphEditField::Instructions);
            }
        }
        KeyCode::Char('s') if !mode.is_executing() && !is_authored => {
            let (editable, _) = node_is_editable(app, draft_id, selected_node);
            if editable {
                begin_edit_field(
                    app,
                    draft_id,
                    selected_node,
                    GraphEditField::IntegrationStrategy,
                );
            }
        }
        KeyCode::Char('S') if !mode.is_executing() && !is_authored => {
            let (editable, _) = node_is_editable(app, draft_id, selected_node);
            if editable {
                begin_edit_field(
                    app,
                    draft_id,
                    selected_node,
                    GraphEditField::VerificationStrategy,
                );
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            move_selected_edge(app, draft_id, selected_node, 1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            move_selected_edge(app, draft_id, selected_node, -1);
        }
        KeyCode::Char('[') => {
            navigate_predecessor(app);
        }
        KeyCode::Char(']') => {
            navigate_successor(app);
        }
        KeyCode::Char('x') if mode.is_executing() => {
            interrupt_graph_execution(app, draft_id).await;
        }
        KeyCode::Char('x') if !read_only => {
            delete_selected_edge(app, draft_id, selected_node, selected_edge);
        }
        KeyCode::Char('t') if !read_only => {
            open_graph_picker(app, GraphPickerKind::Topology);
        }
        KeyCode::Char('o') if !read_only => {
            open_graph_picker(app, GraphPickerKind::SavedWorkflow);
        }
        KeyCode::Char('R') => {
            open_recursive_picker(app).await;
        }
        KeyCode::Char('r') if mode.allows_execution_start() && is_authored => {
            run_graph_draft(app, draft_id).await;
        }
        KeyCode::Char('H') => pan_graph_view(app, -HORIZONTAL_PAN_STEP, 0),
        KeyCode::Char('J') => pan_graph_view(app, 0, VERTICAL_PAN_STEP),
        KeyCode::Char('K') => pan_graph_view(app, 0, -VERTICAL_PAN_STEP),
        KeyCode::Char('L') => pan_graph_view(app, HORIZONTAL_PAN_STEP, 0),
        KeyCode::Char('c') => {
            set_graph_camera(app, GraphCamera::FollowSelection);
        }
        KeyCode::Esc => {
            set_graph_mode(app, detail_step_back_mode(mode));
        }
        KeyCode::Char('q') if mode.allows_overlay_close() => {
            app.overlay = OverlayState::None;
            app.mark_dirty();
        }
        _ => {}
    }
}

async fn handle_edit_field_key(app: &mut App, key: KeyEvent) {
    let (draft_id, selected_node, camera, view_origin) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id,
            selected_node,
            mode: GraphMode::EditField { camera, .. },
            view_origin,
            ..
        } => (*draft_id, *selected_node, *camera, view_origin.clone()),
        _ => return,
    };

    let is_authored = view_origin == GraphViewOrigin::AuthoredWorkflow;
    let is_recursive = matches!(view_origin, GraphViewOrigin::BridgedRecursive { .. });

    // Defense-in-depth: only AuthoredWorkflow or BridgedRecursive (which is editable per-node)
    // can commit.
    if !is_authored && !is_recursive {
        return;
    }

    match key.code {
        KeyCode::Esc => {
            if let OverlayState::GraphReview { edit_buffer, .. } = &mut app.overlay {
                edit_buffer.clear();
            }
            set_graph_mode(app, GraphMode::Detail { camera });
        }
        KeyCode::Enter => {
            if is_authored {
                commit_edit_field(app, draft_id, selected_node);
            } else if is_recursive {
                let (field, value) = match &app.overlay {
                    OverlayState::GraphReview {
                        mode, edit_buffer, ..
                    } => (mode.editing_field(), edit_buffer.clone()),
                    _ => (None, String::new()),
                };
                if let Some(field) = field {
                    commit_bridged_field(app, draft_id, selected_node, field, value).await;
                }
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::GraphReview { edit_buffer, .. } = &mut app.overlay
                && edit_buffer.pop().is_some()
            {
                app.mark_dirty();
            }
        }
        KeyCode::Char(c) => {
            if let OverlayState::GraphReview { edit_buffer, .. } = &mut app.overlay {
                edit_buffer.push(c);
                app.mark_dirty();
            }
        }
        _ => {}
    }
}

/// Half-viewport step for `PageDown` / `PageUp` (and `Ctrl-D` / `Ctrl-U`)
/// in the picker. The key handler does not know the rendered viewport
/// height, so we approximate a conservative ~half-viewport step.
const PICKER_PAGE_STEP: usize = 10;

async fn handle_picker_key(app: &mut App, key: KeyEvent) {
    let Some((kind, selected_index)) = graph_mode(app).and_then(GraphMode::picker) else {
        return;
    };
    let item_count = picker_item_count(app, kind);

    match key.code {
        KeyCode::Esc => {
            if let Some(mode) = graph_mode(app) {
                set_graph_mode(app, picker_exit_mode(mode));
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if item_count > 0 {
                update_picker_index(app, (selected_index + 1).min(item_count - 1));
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            update_picker_index(app, selected_index.saturating_sub(1));
        }
        KeyCode::Char('g') => {
            update_picker_index(app, 0);
        }
        KeyCode::Char('G') if item_count > 0 => {
            update_picker_index(app, item_count - 1);
        }
        KeyCode::PageDown => {
            if item_count > 0 {
                let next = (selected_index + PICKER_PAGE_STEP).min(item_count - 1);
                update_picker_index(app, next);
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if item_count > 0 {
                let next = (selected_index + PICKER_PAGE_STEP).min(item_count - 1);
                update_picker_index(app, next);
            }
        }
        KeyCode::PageUp => {
            update_picker_index(app, selected_index.saturating_sub(PICKER_PAGE_STEP));
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            update_picker_index(app, selected_index.saturating_sub(PICKER_PAGE_STEP));
        }
        KeyCode::Enter => match kind {
            GraphPickerKind::Topology => select_topology_template(app, selected_index),
            GraphPickerKind::SavedWorkflow => {
                select_saved_workflow(app, selected_index).await;
            }
            GraphPickerKind::RecursiveGraphs => {
                select_recursive_graph(app, selected_index).await;
            }
        },
        _ => {}
    }
}

fn graph_mode(app: &App) -> Option<GraphMode> {
    match &app.overlay {
        OverlayState::GraphReview { mode, .. } => Some(*mode),
        _ => None,
    }
}

fn set_graph_mode(app: &mut App, next_mode: GraphMode) {
    if let OverlayState::GraphReview {
        mode,
        viewport,
        edit_buffer,
        selected_edge,
        ..
    } = &mut app.overlay
    {
        *mode = next_mode;
        if !matches!(next_mode, GraphMode::EditField { .. }) {
            edit_buffer.clear();
        }
        if !next_mode.shows_detail_panel() {
            *selected_edge = 0;
        }
        if next_mode.camera() == GraphCamera::FollowSelection {
            viewport.reset();
        }
        app.mark_dirty();
    }
}

fn detail_step_back_mode(mode: GraphMode) -> GraphMode {
    match mode {
        GraphMode::Detail { camera } => GraphMode::Navigate { camera },
        GraphMode::EditField { camera, .. } => GraphMode::Detail { camera },
        GraphMode::Executing { camera, .. } => GraphMode::Executing {
            previous_mode: GraphBrowseMode::Navigate,
            camera,
        },
        _ => mode,
    }
}

fn picker_exit_mode(mode: GraphMode) -> GraphMode {
    match mode {
        GraphMode::Picker {
            previous_mode,
            camera,
            ..
        } => previous_mode.with_camera(camera),
        _ => mode,
    }
}

fn set_graph_camera(app: &mut App, camera: GraphCamera) {
    let Some(mode) = graph_mode(app) else {
        return;
    };
    let viewport = match &app.overlay {
        OverlayState::GraphReview { viewport, .. } => Some(*viewport),
        _ => None,
    };
    if mode.camera() != camera
        || matches!(camera, GraphCamera::FollowSelection)
            && viewport.is_some_and(|viewport| !viewport.is_centered())
    {
        set_graph_mode(app, mode.with_camera(camera));
    }
}

fn pan_graph_view(app: &mut App, dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }

    if let OverlayState::GraphReview { mode, viewport, .. } = &mut app.overlay {
        *mode = mode.with_camera(GraphCamera::Manual);
        viewport.pan(dx, dy);
        app.mark_dirty();
    }
}

fn open_graph_picker(app: &mut App, kind: GraphPickerKind) {
    let Some(mode) = graph_mode(app) else {
        return;
    };
    set_graph_mode(app, mode.open_picker(kind));
    // Reset the embedded picker scroll/offset state on every open transition.
    if let OverlayState::GraphReview {
        picker_list_state, ..
    } = &mut app.overlay
    {
        *picker_list_state = ratatui::widgets::ListState::default();
        picker_list_state.select(Some(0));
    }
}

fn update_picker_index(app: &mut App, selected_index: usize) {
    let Some(mode) = graph_mode(app) else {
        return;
    };
    let next_mode = mode.set_picker_index(selected_index);
    if next_mode != mode {
        set_graph_mode(app, next_mode);
    }
    // Keep ListState in sync with the application's source of truth so the
    // List widget's offset math matches GraphMode::Picker.selected_index.
    if let OverlayState::GraphReview {
        picker_list_state, ..
    } = &mut app.overlay
    {
        picker_list_state.select(Some(selected_index));
    }
}

fn picker_item_count(app: &App, kind: GraphPickerKind) -> usize {
    match kind {
        GraphPickerKind::Topology => starter_templates().len(),
        GraphPickerKind::SavedWorkflow => saved_workflow_picker_entries(app).len(),
        GraphPickerKind::RecursiveGraphs => app.recursive_graphs.len(),
    }
}

pub(crate) fn saved_workflow_picker_entries(app: &App) -> Vec<Workflow> {
    // Defense-in-depth: drop titles starting with '/' (raw session prompts
    // captured by the now-disabled INSERT path at launch.rs). The upstream
    // INSERT was deleted in P1.13 Phase 1, but if any future code path
    // reintroduces junk titles, the display filter keeps the picker clean.
    let mut workflows: Vec<Workflow> = app
        .workflows
        .values()
        .filter(|wf| !wf.title.starts_with('/'))
        .cloned()
        .collect();
    workflows.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.title.cmp(&right.title))
    });
    workflows
}

fn select_topology_template(app: &mut App, selected_index: usize) {
    let Some(template) = starter_templates().get(selected_index) else {
        app.notify("No topology templates available");
        app.mark_dirty();
        return;
    };

    let Some(workflow) = build_starter_workflow(template.name) else {
        app.notify_error(format!("starter template unavailable: {}", template.name));
        app.mark_dirty();
        return;
    };

    let (draft_id, gv_info_dashboard) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id,
            gv_info_dashboard,
            ..
        } => (*draft_id, *gv_info_dashboard),
        _ => return,
    };

    let (reuse_draft, project_id, previous_workflow) = match app.graph_draft(&draft_id) {
        Some(draft) => (
            draft.workflow_id.is_none()
                && draft.workflow.nodes.is_empty()
                && draft.workflow.edges.is_empty(),
            draft.project_id.or(app.current_project_id),
            draft.workflow.clone(),
        ),
        None => return,
    };

    if reuse_draft {
        if let Some(draft) = app.graph_draft_mut(&draft_id) {
            draft.workflow = workflow;
            draft.last_execution_id = None;
        }
        if let OverlayState::GraphReview {
            mode,
            edit_history,
            collapsed,
            selected_node,
            selected_edge,
            ..
        } = &mut app.overlay
        {
            edit_history.push(previous_workflow);
            collapsed.clear();
            *selected_node = 0;
            *selected_edge = 0;
            *mode = GraphMode::Navigate {
                camera: GraphCamera::FollowSelection,
            };
        }
        app.mark_graph_draft_dirty(draft_id);
        app.mark_dirty();
        return;
    }

    let new_draft_id = app.create_graph_draft(workflow, None, project_id);
    app.mark_graph_draft_dirty(new_draft_id);
    app.show_graph_review_draft(new_draft_id, gv_info_dashboard);
}

async fn select_saved_workflow(app: &mut App, selected_index: usize) {
    let workflows = saved_workflow_picker_entries(app);
    let Some(workflow) = workflows.get(selected_index).cloned() else {
        app.notify("No saved workflows available");
        app.mark_dirty();
        return;
    };

    let draft_id = if let Some(existing_draft) = app.find_graph_draft_id_by_workflow_id(workflow.id)
    {
        existing_draft
    } else {
        load_saved_workflow_draft(app, workflow.id, workflow.project_id).await
    };

    let _ = app.resync_graph_execution_for_draft(draft_id, true).await;
    let caps = app.client.get_daemon_capabilities().await.ok();
    let gv_info_dashboard = caps.map_or(false, |c| c.gv_info_dashboard);
    app.show_graph_review_draft(draft_id, gv_info_dashboard);
}

/// True when the draft's provenance is the editable native-workflow origin.
/// Missing drafts are treated as non-authored (fail-closed for the write-gate).
fn draft_is_authored_workflow(app: &App, draft_id: &Uuid) -> bool {
    app.graph_draft(draft_id)
        .is_some_and(|d| d.view_origin == GraphViewOrigin::AuthoredWorkflow)
}

/// Open the gated "Recursive graphs" picker. Loads the graph inventory into
/// `app.recursive_graphs` at OPEN time (BLOCKER 2) so the picker has items, and
/// is a no-op when the `gv_render_recursive_origin` cap is observed false
/// (Decision 3 — the daemon cap is the single source of truth).
async fn open_recursive_picker(app: &mut App) {
    let caps = match app.client.get_daemon_capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            app.notify_error(format!("recursive graphs unavailable: {}", error));
            app.mark_dirty();
            return;
        }
    };
    if !caps.gv_render_recursive_origin {
        // Source never opens; the `R` key is inert when the cap is off.
        return;
    }

    app.recursive_graphs = match app
        .client
        .list_recursive_task_graphs(rsi_common::rpc::ListRecursiveTaskGraphsParams::default())
        .await
    {
        Ok(graphs) => graphs,
        Err(error) => {
            app.notify_error(format!("failed to list recursive graphs: {}", error));
            app.mark_dirty();
            Vec::new()
        }
    };

    open_graph_picker(app, GraphPickerKind::RecursiveGraphs);
}

/// Select a recursive graph from the open-time cache (BLOCKER 2 — no re-fetch of
/// the list), fetch its bridged read-only definition, and open it as a
/// `BridgedRecursive` draft. Never persists a workflows row (BLOCKER 1).
async fn select_recursive_graph(app: &mut App, selected_index: usize) {
    let Some(summary) = app.recursive_graphs.get(selected_index) else {
        app.notify("No recursive graphs available");
        app.mark_dirty();
        return;
    };
    let graph_id = summary.id.0;

    let resp = match app.client.get_recursive_graph_as_workflow(graph_id).await {
        Ok(resp) => resp,
        Err(error) => {
            app.notify_error(format!("failed to bridge recursive graph: {}", error));
            app.mark_dirty();
            return;
        }
    };

    let workflow: WorkflowDefinition = match serde_json::from_value(resp.definition) {
        Ok(workflow) => workflow,
        Err(error) => {
            app.notify_error(format!("recursive bridge decode failed: {}", error));
            app.mark_dirty();
            return;
        }
    };

    // In-memory draft only: workflow_id = None (no synthetic id, no persistence).
    let draft_id = app.create_graph_draft(workflow, None, None);
    if let Some(draft) = app.graph_draft_mut(&draft_id) {
        draft.view_origin = GraphViewOrigin::BridgedRecursive {
            recursive_graph_id: graph_id,
        };
    }
    let caps = app.client.get_daemon_capabilities().await.ok();
    let gv_info_dashboard = caps.map_or(false, |c| c.gv_info_dashboard);
    app.show_graph_review_draft(draft_id, gv_info_dashboard);
}

fn move_selected_node_in_direction(
    app: &mut App,
    draft_id: Uuid,
    node_count: usize,
    direction: GraphDirection,
) {
    let Some(current) = current_selected_node(app) else {
        return;
    };

    let next = app.graph_draft(&draft_id).and_then(|draft| {
        let layout = layout_workflow_graph(&draft.workflow);
        find_directional_neighbor(&layout, current, direction)
    });
    if let Some(next) = next {
        move_selected_node_to(app, next, node_count);
    }
}

fn move_selected_node_to(app: &mut App, target: usize, node_count: usize) {
    if node_count == 0 {
        return;
    }
    if let OverlayState::GraphReview {
        selected_node,
        selected_edge,
        viewport,
        mode,
        ..
    } = &mut app.overlay
        && *selected_node != target
    {
        *selected_node = target;
        *selected_edge = 0;
        if mode.camera() == GraphCamera::Manual {
            *mode = mode.with_camera(GraphCamera::FollowSelection);
            *viewport = GraphViewport::default();
        }
        app.mark_dirty();
    }
}

fn current_selected_node(app: &App) -> Option<usize> {
    match &app.overlay {
        OverlayState::GraphReview { selected_node, .. } => Some(*selected_node),
        _ => None,
    }
}

pub(crate) fn node_is_editable(
    app: &App,
    draft_id: Uuid,
    selected_node: usize,
) -> (bool, Option<String>) {
    let Some(draft) = app.graph_draft(&draft_id) else {
        return (false, Some("no draft found".to_string()));
    };
    match &draft.view_origin {
        GraphViewOrigin::AuthoredWorkflow => (true, None),
        GraphViewOrigin::BridgedRecursive { .. } => {
            let Some(node) = draft.workflow.nodes.get(selected_node) else {
                return (false, Some("no selected node".to_string()));
            };
            let node_id = &node.id;
            let locks_str = match draft.workflow.metadata.get("recursive_node_locks") {
                Some(rsi_graph::data::Value::String(s)) => s,
                _ => return (false, Some("lock state unknown".to_string())),
            };
            let locks: serde_json::Value = match serde_json::from_str(locks_str) {
                Ok(v) => v,
                Err(_) => return (false, Some("lock state unparseable".to_string())),
            };
            let node_lock = &locks[node_id];
            if node_lock.is_null() {
                return (false, Some("lock state missing".to_string()));
            }
            let locked = node_lock["locked"].as_bool().unwrap_or(true);
            if locked {
                let reason = node_lock["reason"].as_str().unwrap_or("locked").to_string();
                (false, Some(reason))
            } else {
                (true, None)
            }
        }
        _ => (false, Some("read-only bridged graph".to_string())),
    }
}

fn get_node_strategies(
    draft: &crate::types::GraphDraft,
    node_id: &str,
) -> (Option<String>, Option<String>) {
    let strat_str = match draft.workflow.metadata.get("recursive_node_strategies") {
        Some(rsi_graph::data::Value::String(s)) => s,
        _ => return (None, None),
    };
    let strat: serde_json::Value = match serde_json::from_str(strat_str) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let node_strat = &strat[node_id];
    if node_strat.is_null() {
        return (None, None);
    }
    let integration = node_strat["integration_strategy"]
        .as_str()
        .map(|s| s.to_string());
    let verification = node_strat["verification_strategy"]
        .as_str()
        .map(|s| s.to_string());
    (integration, verification)
}

fn begin_edit_field(app: &mut App, draft_id: Uuid, selected_node: usize, field: GraphEditField) {
    let value = app.graph_draft(&draft_id).and_then(|draft| {
        let node = draft.workflow.nodes.get(selected_node)?;
        match field {
            GraphEditField::Name => Some(node.name.clone()),
            GraphEditField::Instructions => Some(node.instructions.clone()),
            GraphEditField::IntegrationStrategy => {
                let (integration, _) = get_node_strategies(draft, &node.id);
                Some(integration.unwrap_or_default())
            }
            GraphEditField::VerificationStrategy => {
                let (_, verification) = get_node_strategies(draft, &node.id);
                Some(verification.unwrap_or_default())
            }
        }
    });

    let Some(value) = value else {
        return;
    };

    if let OverlayState::GraphReview {
        mode, edit_buffer, ..
    } = &mut app.overlay
    {
        let camera = mode.camera();
        *edit_buffer = value;
        *mode = GraphMode::EditField { field, camera };
        app.mark_dirty();
    }
}

fn move_selected_edge(app: &mut App, draft_id: Uuid, selected_node: usize, delta: isize) {
    let edge_count = app
        .graph_draft(&draft_id)
        .map(|draft| count_node_edges(&draft.workflow, selected_node))
        .unwrap_or(0);
    if edge_count == 0 {
        return;
    }

    if let OverlayState::GraphReview { selected_edge, .. } = &mut app.overlay {
        let next = if delta.is_negative() {
            selected_edge.saturating_sub(delta.unsigned_abs())
        } else {
            (*selected_edge + delta as usize).min(edge_count - 1)
        };
        if next != *selected_edge {
            *selected_edge = next;
            app.mark_dirty();
        }
    }
}

async fn run_graph_draft(app: &mut App, draft_id: Uuid) {
    // BLOCKER 1 defense-in-depth: a bridged (non-`AuthoredWorkflow`) draft must
    // never persist a workflows row or execute, even if a future caller reaches
    // this fn on such a draft.
    if !draft_is_authored_workflow(app, &draft_id) {
        return;
    }
    let Some((workflow_id, workflow)) = ensure_graph_draft_saved(app, draft_id).await else {
        return;
    };

    let validation = rsi_graph::validate_executable_workflow(&workflow);
    if validation.has_errors() {
        notify_validation_blockers(app, &validation);
        return;
    }

    let workflow_value = match serde_json::to_value(&workflow) {
        Ok(workflow_value) => workflow_value,
        Err(error) => {
            app.notify_error(format!("workflow encode failed: {}", error));
            app.mark_dirty();
            return;
        }
    };

    let project_id = app.graph_draft(&draft_id).and_then(|d| d.project_id);

    match app
        .client
        .execute_workflow(
            workflow_id,
            &workflow_value,
            None,
            false,
            project_id,
            None,
            None,
        )
        .await
    {
        Ok(response) => {
            app.record_graph_execution_ack(draft_id, &response, workflow.name.clone());

            // Link the currently-selected session to this workflow so the
            // embedded graph view renders in the session detail bottom pane.
            if let Some(session_id) = app.selected_session_id() {
                let needs_update = app
                    .sessions
                    .get(&session_id)
                    .map(|s| s.session.workflow_id != Some(workflow_id))
                    .unwrap_or(false);
                if needs_update {
                    if let Some(state) = app.sessions.get_mut(&session_id) {
                        state.session.workflow_id = Some(workflow_id);
                    }
                    // Persist the link via daemon (fire-and-forget — local state
                    // is already updated for immediate rendering).
                    let _ = app
                        .client
                        .update_session_workflow(session_id, Some(workflow_id))
                        .await;
                }
            }

            app.mark_dirty();
        }
        Err(error) => {
            app.notify_error(format!("workflow execution failed: {}", error));
            app.mark_dirty();
        }
    }
}

async fn interrupt_graph_execution(app: &mut App, draft_id: Uuid) {
    let Some(draft) = app.graph_draft(&draft_id) else {
        return;
    };
    let Some(execution_id) = draft.last_execution_id else {
        app.notify("No active workflow execution");
        app.mark_dirty();
        return;
    };

    if !app.graph_execution_is_active_for_draft(&draft_id) {
        app.notify("Workflow execution is not active");
        app.mark_dirty();
        return;
    }

    match app.client.interrupt_workflow_execution(execution_id).await {
        Ok(response) => {
            app.notify(format!(
                "workflow interrupt requested: {} ({:?})",
                response.execution_id, response.status
            ));
            app.mark_dirty();
        }
        Err(error) => {
            app.notify_error(format!("workflow interrupt failed: {}", error));
            app.mark_dirty();
        }
    }
}

/// Parsed `:topology-resolve [<execution_id>] <action> [<preserved_commit>]`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TopologyResolveCommand {
    pub(crate) execution_id: Option<Uuid>,
    pub(crate) action: rsi_common::rpc::TopologyAttemptAction,
    pub(crate) confirm: Option<String>,
}

pub(crate) fn parse_topology_resolve(args: &str) -> Result<TopologyResolveCommand, String> {
    use rsi_common::rpc::TopologyAttemptAction as Action;
    let mut tokens = args.split_whitespace().peekable();
    let execution_id = tokens
        .peek()
        .and_then(|token| Uuid::parse_str(token).ok())
        .inspect(|_| {
            tokens.next();
        });
    let action = match tokens.next() {
        Some("inspect") => Action::Inspect,
        Some("accept") => Action::Accept,
        Some("retry") => Action::Retry,
        Some("discard") => Action::Discard,
        _ => {
            return Err(
                "usage: :topology-resolve [<execution_id>] inspect|accept|retry|discard [<commit>]"
                    .to_string(),
            );
        }
    };
    let confirm = tokens.next().map(str::to_owned);
    if tokens.next().is_some() {
        return Err("too many arguments for :topology-resolve".to_string());
    }
    Ok(TopologyResolveCommand {
        execution_id,
        action,
        confirm,
    })
}

/// Resolve preserved work on a blocked durable topology execution (#634).
/// Without an explicit id the single blocked execution the TUI knows about is
/// used. The CAS version and blocked attempt come from a fresh snapshot, and
/// the idempotency key is bound to that version, so a repeated command
/// replays instead of acting twice.
pub(crate) async fn resolve_topology_attempt_command(app: &mut App, args: &str) {
    use rsi_common::types::{WorkflowExecutionLookup, WorkflowExecutionStatus};
    let command = match parse_topology_resolve(args) {
        Ok(command) => command,
        Err(error) => {
            app.notify_error(error);
            app.mark_dirty();
            return;
        }
    };
    let execution_id = if let Some(id) = command.execution_id {
        id
    } else {
        let blocked: Vec<Uuid> = app
            .graph_executions
            .values()
            .filter(|execution| execution.status == WorkflowExecutionStatus::Blocked)
            .map(|execution| execution.execution_id)
            .collect();
        let [id] = blocked.as_slice() else {
            app.notify_error(format!(
                "{} blocked topology executions; name one: :topology-resolve <execution_id> <action>",
                blocked.len()
            ));
            app.mark_dirty();
            return;
        };
        *id
    };
    let snapshot = match app.client.get_workflow_execution(execution_id).await {
        Ok(WorkflowExecutionLookup::Found { execution }) => execution,
        Ok(_) => {
            app.notify_error(format!("topology execution not found: {execution_id}"));
            app.mark_dirty();
            return;
        }
        Err(error) => {
            app.notify_error(format!("topology execution lookup failed: {error}"));
            app.mark_dirty();
            return;
        }
    };
    let (Some(row_version), Some(attempt_id)) = (snapshot.row_version, snapshot.blocked_attempt_id)
    else {
        app.notify_error(format!(
            "topology execution {execution_id} is not blocked on preserved work"
        ));
        app.mark_dirty();
        return;
    };
    app.graph_executions.insert(execution_id, snapshot);
    let action = command.action;
    let params = rsi_common::rpc::ResolveTopologyAttemptParams {
        execution_id,
        attempt_id,
        action,
        expected_row_version: row_version,
        idempotency_key: format!(
            "tui:{execution_id}:{attempt_id}:{}:{row_version}",
            action.as_str()
        ),
        confirm_preserved_commit: command.confirm,
    };
    match app.client.resolve_topology_attempt(&params).await {
        Ok(response) => {
            let attempt = &response.attempt;
            let message = match &response.report {
                Some(report) => format!(
                    "{}@{} preserved {} head {} ({} dirty, {} changed)",
                    attempt.node_id,
                    attempt.iteration,
                    report.preserved_commit.as_deref().unwrap_or("-"),
                    report.head.as_deref().unwrap_or("-"),
                    report.dirty_paths.len(),
                    report.diffstat.len()
                ),
                None => format!(
                    "{} {}@{} attempt {}: {}{}",
                    action.as_str(),
                    attempt.node_id,
                    attempt.iteration,
                    attempt.attempt_no,
                    attempt.status,
                    if response.deduplicated {
                        " (replayed)"
                    } else {
                        ""
                    }
                ),
            };
            app.notify(message);
        }
        Err(error) => app.notify_error(format!("topology resolve failed: {error}")),
    }
    app.mark_dirty();
}

async fn ensure_graph_draft_saved(
    app: &mut App,
    draft_id: Uuid,
) -> Option<(Uuid, WorkflowDefinition)> {
    // BLOCKER 1 defense-in-depth: never persist (upsert a workflows row) for a
    // bridged (non-`AuthoredWorkflow`) draft.
    if !draft_is_authored_workflow(app, &draft_id) {
        return None;
    }
    let (workflow_id, persistence_state, workflow) = {
        let draft = app.graph_draft(&draft_id)?;
        (
            draft.workflow_id,
            draft.persistence_state,
            draft.workflow.clone(),
        )
    };

    let needs_save =
        workflow_id.is_none() || !matches!(persistence_state, GraphDraftPersistenceState::Clean);
    if !needs_save {
        return workflow_id.map(|workflow_id| (workflow_id, workflow));
    }

    if let Some(draft) = app.graph_draft_mut(&draft_id) {
        draft.persistence_state = GraphDraftPersistenceState::Saving;
    }
    app.mark_dirty();

    let document = match build_workflow_document(app, draft_id) {
        Some(document) => document,
        None => {
            if let Some(draft) = app.graph_draft_mut(&draft_id) {
                draft.persistence_state = GraphDraftPersistenceState::SaveFailed;
            }
            app.notify_error("workflow save failed: draft is unavailable");
            app.mark_dirty();
            return None;
        }
    };

    match app.client.upsert_workflow_definition(&document).await {
        Ok(saved) => {
            apply_saved_workflow_document(app, draft_id, saved);
            let draft = app.graph_draft(&draft_id)?;
            draft
                .workflow_id
                .map(|saved_workflow_id| (saved_workflow_id, draft.workflow.clone()))
        }
        Err(error) => {
            if let Some(draft) = app.graph_draft_mut(&draft_id) {
                draft.persistence_state = GraphDraftPersistenceState::SaveFailed;
            }
            app.notify_error(format!("workflow save failed: {}", error));
            app.mark_dirty();
            None
        }
    }
}

fn build_workflow_document(app: &App, draft_id: Uuid) -> Option<WorkflowDocument> {
    let draft = app.graph_draft(&draft_id)?;
    let existing = draft
        .workflow_id
        .and_then(|workflow_id| app.workflows.get(&workflow_id))
        .cloned();
    let now = chrono::Utc::now();
    let workflow_id = existing
        .as_ref()
        .map(|workflow| workflow.id)
        .or(draft.workflow_id)
        .unwrap_or_else(Uuid::new_v4);
    let project_id = draft
        .project_id
        .or(existing.as_ref().and_then(|workflow| workflow.project_id))
        .or(app.current_project_id);

    Some(WorkflowDocument {
        workflow: Workflow {
            id: workflow_id,
            title: draft.workflow.name.clone(),
            stage: existing
                .as_ref()
                .map(|workflow| workflow.stage)
                .unwrap_or(WorkflowStage::Planning),
            artifact_path: existing
                .as_ref()
                .and_then(|workflow| workflow.artifact_path.clone()),
            project_id,
            created_at: existing
                .as_ref()
                .map(|workflow| workflow.created_at.to_owned())
                .unwrap_or(now),
            updated_at: now,
        },
        definition: serde_json::to_value(&draft.workflow).ok()?,
    })
}

fn apply_saved_workflow_document(app: &mut App, draft_id: Uuid, document: WorkflowDocument) {
    let WorkflowDocument {
        workflow: saved_workflow,
        definition,
    } = document;
    let normalized_workflow =
        serde_json::from_value::<WorkflowDefinition>(definition).map_err(|error| error.to_string());

    app.workflows
        .insert(saved_workflow.id, saved_workflow.clone());

    if let Some(draft) = app.graph_draft_mut(&draft_id) {
        draft.workflow_id = Some(saved_workflow.id);
        draft.project_id = saved_workflow.project_id;
        draft.persistence_state = GraphDraftPersistenceState::Clean;
        if let Ok(workflow) = normalized_workflow.as_ref() {
            draft.workflow = workflow.clone();
        } else {
            draft.workflow.name = saved_workflow.title.clone();
        }
    }

    if let Err(error) = normalized_workflow {
        app.notify_error(format!("saved workflow reload failed: {}", error));
    }

    app.active_graph_draft_id = Some(draft_id);
    app.mark_dirty();
}

fn notify_validation_blockers(app: &mut App, report: &rsi_common::types::WorkflowValidationReport) {
    let blocking_messages: Vec<&str> = report
        .diagnostics
        .iter()
        .filter(|diag| diag.is_blocking())
        .map(|diag| diag.message.as_str())
        .take(3)
        .collect();

    let message = if blocking_messages.is_empty() {
        "Execution blocked by workflow validation".to_string()
    } else {
        format!(
            "Execution blocked ({} issues): {}",
            report.error_count(),
            blocking_messages.join("; ")
        )
    };

    app.notify_error(message);
    app.mark_dirty();
}

async fn commit_bridged_field(
    app: &mut App,
    draft_id: Uuid,
    selected_node: usize,
    field: GraphEditField,
    new_value: String,
) {
    let draft = match app.graph_draft(&draft_id) {
        Some(d) => d,
        None => return,
    };
    let view_origin = draft.view_origin.clone();
    let (graph_id, task_id) = match &view_origin {
        GraphViewOrigin::BridgedRecursive { recursive_graph_id } => {
            let Some(node) = draft.workflow.nodes.get(selected_node) else {
                return;
            };
            let Ok(task_id) = Uuid::parse_str(&node.id) else {
                app.notify_error("Invalid task ID format");
                return;
            };
            (*recursive_graph_id, task_id)
        }
        _ => return,
    };

    let result = match field {
        GraphEditField::Instructions => {
            app.client
                .edit_recursive_node_instructions(graph_id, task_id, new_value)
                .await
        }
        GraphEditField::IntegrationStrategy => {
            let clean_val = if new_value.trim().is_empty() {
                None
            } else {
                Some(new_value.trim().to_string())
            };
            let (_, verification_strategy) = get_node_strategies(draft, &task_id.to_string());
            app.client
                .edit_recursive_node_settings(graph_id, task_id, clean_val, verification_strategy)
                .await
        }
        GraphEditField::VerificationStrategy => {
            let clean_val = if new_value.trim().is_empty() {
                None
            } else {
                Some(new_value.trim().to_string())
            };
            let (integration_strategy, _) = get_node_strategies(draft, &task_id.to_string());
            app.client
                .edit_recursive_node_settings(graph_id, task_id, integration_strategy, clean_val)
                .await
        }
        _ => {
            app.notify_error("Unsupported field edit for bridged recursive node");
            return;
        }
    };

    match result {
        Ok(_) => match refresh_bridged_draft(app, draft_id, graph_id, selected_node).await {
            Ok(_) => {
                if let OverlayState::GraphReview {
                    edit_buffer, mode, ..
                } = &mut app.overlay
                {
                    edit_buffer.clear();
                    let camera = mode.camera();
                    *mode = GraphMode::Detail { camera };
                }
            }
            Err(e) => {
                app.notify_error(format!("Failed to refresh draft: {}", e));
            }
        },
        Err(e) => {
            app.notify_error(format!("Edit failed: {}", e));
        }
    }
}

async fn refresh_bridged_draft(
    app: &mut App,
    draft_id: Uuid,
    graph_id: Uuid,
    selected_node: usize,
) -> std::result::Result<(), crate::client::ClientError> {
    let node_id = app
        .graph_draft(&draft_id)
        .and_then(|draft| draft.workflow.nodes.get(selected_node))
        .map(|node| node.id.clone());

    let resp = app.client.get_recursive_graph_as_workflow(graph_id).await?;
    let workflow: WorkflowDefinition =
        serde_json::from_value(resp.definition).map_err(|e| crate::client::ClientError::Json(e))?;

    if let Some(draft) = app.graph_draft_mut(&draft_id) {
        draft.workflow = workflow;
    }

    if let OverlayState::GraphReview { edit_history, .. } = &mut app.overlay {
        edit_history.clear();
    }

    if let Some(node_id) = node_id {
        if let Some(draft) = app.graph_draft(&draft_id) {
            let new_index = draft
                .workflow
                .nodes
                .iter()
                .position(|node| node.id == node_id)
                .unwrap_or(selected_node);
            let final_index = new_index.min(draft.workflow.nodes.len().saturating_sub(1));

            if let OverlayState::GraphReview {
                selected_node: ref_node,
                ..
            } = &mut app.overlay
            {
                *ref_node = final_index;
            }
        }
    }

    app.mark_dirty();
    Ok(())
}

fn commit_edit_field(app: &mut App, draft_id: Uuid, selected_node: usize) {
    let (field, value, camera) = match &app.overlay {
        OverlayState::GraphReview {
            mode, edit_buffer, ..
        } => (mode.editing_field(), edit_buffer.clone(), mode.camera()),
        _ => return,
    };

    let Some(field) = field else {
        return;
    };

    let previous = app
        .graph_draft(&draft_id)
        .map(|draft| draft.workflow.clone());
    let mut applied = false;

    if let Some(draft) = app.graph_draft_mut(&draft_id)
        && let Some(node) = draft.workflow.nodes.get_mut(selected_node)
    {
        match field {
            GraphEditField::Name => node.name = value,
            GraphEditField::Instructions => node.instructions = value,
            GraphEditField::IntegrationStrategy | GraphEditField::VerificationStrategy => {}
        }
        applied = true;
    }

    if let OverlayState::GraphReview { edit_history, .. } = &mut app.overlay
        && applied
        && let Some(previous) = previous
    {
        edit_history.push(previous);
    }

    if applied {
        app.mark_graph_draft_dirty(draft_id);
    }
    set_graph_mode(app, GraphMode::Detail { camera });
}

fn count_node_edges(workflow: &WorkflowDefinition, node_idx: usize) -> usize {
    if let Some(node) = workflow.nodes.get(node_idx) {
        workflow
            .edges
            .iter()
            .filter(|edge| edge.source == node.id || edge.target == node.id)
            .count()
    } else {
        0
    }
}

fn navigate_predecessor(app: &mut App) {
    let (draft_id, selected_node, node_count) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id,
            selected_node,
            ..
        } => (
            *draft_id,
            *selected_node,
            app.graph_draft(draft_id)
                .map(|draft| draft.workflow.nodes.len())
                .unwrap_or(0),
        ),
        _ => return,
    };

    let target_idx = app.graph_draft(&draft_id).and_then(|draft| {
        let current_id = draft.workflow.nodes.get(selected_node)?.id.clone();
        draft
            .workflow
            .edges
            .iter()
            .find(|edge| edge.target == current_id)
            .and_then(|edge| {
                draft
                    .workflow
                    .nodes
                    .iter()
                    .position(|node| node.id == edge.source)
            })
    });

    if let Some(target_idx) = target_idx {
        move_selected_node_to(app, target_idx, node_count);
    }
}

fn navigate_successor(app: &mut App) {
    let (draft_id, selected_node, node_count) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id,
            selected_node,
            ..
        } => (
            *draft_id,
            *selected_node,
            app.graph_draft(draft_id)
                .map(|draft| draft.workflow.nodes.len())
                .unwrap_or(0),
        ),
        _ => return,
    };

    let target_idx = app.graph_draft(&draft_id).and_then(|draft| {
        let current_id = draft.workflow.nodes.get(selected_node)?.id.clone();
        draft
            .workflow
            .edges
            .iter()
            .find(|edge| edge.source == current_id)
            .and_then(|edge| {
                draft
                    .workflow
                    .nodes
                    .iter()
                    .position(|node| node.id == edge.target)
            })
    });

    if let Some(target_idx) = target_idx {
        move_selected_node_to(app, target_idx, node_count);
    }
}

fn delete_selected_node(app: &mut App) {
    let (draft_id, selected_node, mode) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id,
            selected_node,
            mode,
            ..
        } => (*draft_id, *selected_node, *mode),
        _ => return,
    };

    let previous = match app.graph_draft(&draft_id) {
        Some(draft) if !draft.workflow.nodes.is_empty() => draft.workflow.clone(),
        _ => return,
    };

    let remaining_nodes = if let Some(draft) = app.graph_draft_mut(&draft_id) {
        let removed_id = draft.workflow.nodes[selected_node].id.clone();
        draft.workflow.nodes.remove(selected_node);
        draft
            .workflow
            .edges
            .retain(|edge| edge.source != removed_id && edge.target != removed_id);
        draft.last_execution_id = None;
        draft.workflow.nodes.len()
    } else {
        return;
    };

    if let OverlayState::GraphReview {
        selected_node,
        selected_edge,
        edit_history,
        mode: overlay_mode,
        ..
    } = &mut app.overlay
    {
        edit_history.push(previous);
        *selected_edge = 0;
        if remaining_nodes == 0 {
            *selected_node = 0;
            *overlay_mode = GraphMode::Navigate {
                camera: mode.camera(),
            };
        } else if *selected_node >= remaining_nodes {
            *selected_node = remaining_nodes - 1;
        }
    }

    app.mark_graph_draft_dirty(draft_id);
    app.mark_dirty();
}

fn delete_selected_edge(app: &mut App, draft_id: Uuid, selected_node: usize, selected_edge: usize) {
    let previous = app
        .graph_draft(&draft_id)
        .map(|draft| draft.workflow.clone());

    let removed = if let Some(draft) = app.graph_draft_mut(&draft_id) {
        let Some(node) = draft.workflow.nodes.get(selected_node) else {
            return;
        };
        let node_id = node.id.clone();
        let matching: Vec<usize> = draft
            .workflow
            .edges
            .iter()
            .enumerate()
            .filter(|(_, edge)| edge.source == node_id || edge.target == node_id)
            .map(|(index, _)| index)
            .collect();
        if let Some(&edge_idx) = matching.get(selected_edge) {
            draft.workflow.edges.remove(edge_idx);
            true
        } else {
            false
        }
    } else {
        false
    };

    if !removed {
        return;
    }

    if let OverlayState::GraphReview {
        edit_history,
        selected_edge,
        ..
    } = &mut app.overlay
    {
        if let Some(previous) = previous {
            edit_history.push(previous);
        }
        *selected_edge = selected_edge.saturating_sub(1);
    }

    app.mark_graph_draft_dirty(draft_id);
    app.mark_dirty();
}

fn undo_edit(app: &mut App) {
    let (draft_id, mode) = match &app.overlay {
        OverlayState::GraphReview { draft_id, mode, .. } => (*draft_id, *mode),
        _ => return,
    };

    let previous = if let OverlayState::GraphReview { edit_history, .. } = &mut app.overlay {
        edit_history.pop()
    } else {
        None
    };

    let Some(previous) = previous else {
        return;
    };

    let new_len = if let Some(draft) = app.graph_draft_mut(&draft_id) {
        draft.workflow = previous;
        draft.last_execution_id = None;
        draft.workflow.nodes.len()
    } else {
        0
    };

    if let OverlayState::GraphReview {
        selected_node,
        selected_edge,
        mode: overlay_mode,
        ..
    } = &mut app.overlay
    {
        if *selected_node >= new_len {
            *selected_node = new_len.saturating_sub(1);
        }
        *selected_edge = 0;
        if new_len == 0 {
            *overlay_mode = GraphMode::Navigate {
                camera: mode.camera(),
            };
        }
    }

    app.mark_graph_draft_dirty(draft_id);
    app.mark_dirty();
}

fn toggle_collapse(app: &mut App) {
    let (draft_id, selected_node) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id,
            selected_node,
            ..
        } => (*draft_id, *selected_node),
        _ => return,
    };

    let node_id = app.graph_draft(&draft_id).and_then(|draft| {
        let node = draft.workflow.nodes.get(selected_node)?;
        if matches!(node.node_type, NodeType::Topology | NodeType::Subgraph) {
            Some(node.id.clone())
        } else {
            None
        }
    });

    let Some(node_id) = node_id else {
        return;
    };

    if let OverlayState::GraphReview { collapsed, .. } = &mut app.overlay {
        if collapsed.contains(&node_id) {
            collapsed.remove(&node_id);
        } else {
            collapsed.insert(node_id);
        }
        app.mark_dirty();
    }
}

async fn handle_dashboard_key(app: &mut App, key: KeyEvent) {
    let (_draft_id, recursive_graph_id) = match &app.overlay {
        OverlayState::GraphReview {
            draft_id: _draft_id,
            view_origin: GraphViewOrigin::BridgedRecursive { recursive_graph_id },
            ..
        } => (*_draft_id, *recursive_graph_id),
        _ => return,
    };

    let state = match &app.overlay {
        OverlayState::GraphReview {
            dashboard_state, ..
        } => dashboard_state.clone(),
        _ => None,
    };

    let mut browser_state = match state {
        Some(s) => s,
        None => {
            let mut s = RecursiveDagBrowserState::loading(app.current_project_id);
            s.panel = RecursiveDagPanel::Runs;
            s
        }
    };

    let mut focus_back_to_canvas = false;
    let mut trigger_reload = false;
    let mut inspector_action: Option<DashboardInspectorAction> = None;
    let inspector_open = browser_state.inspector.open;
    let on_artifacts_panel = browser_state.panel == RecursiveDagPanel::Artifacts;

    match key.code {
        KeyCode::Esc => {
            if inspector_open {
                browser_state.inspector.close();
                browser_state.message = Some("artifact inspector closed".to_string());
            } else {
                focus_back_to_canvas = true;
            }
        }
        KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
            app.mark_dirty();
            return;
        }
        KeyCode::Char('r') => {
            trigger_reload = true;
        }
        KeyCode::Enter => {
            if on_artifacts_panel {
                inspector_action = Some(DashboardInspectorAction::Open);
            } else {
                browser_state.message = Some(
                    "press Enter on an artifact/validation row in the ARTIFACTS panel (Tab to reach it)"
                        .to_string(),
                );
            }
        }
        KeyCode::Char('p') if on_artifacts_panel && inspector_open => {
            inspector_action = Some(DashboardInspectorAction::View(
                RecursiveDagInspectorView::Preview,
            ));
        }
        KeyCode::Char('m') if on_artifacts_panel && inspector_open => {
            inspector_action = Some(DashboardInspectorAction::View(
                RecursiveDagInspectorView::Metadata,
            ));
        }
        KeyCode::Char('t') if on_artifacts_panel && inspector_open => {
            inspector_action = Some(DashboardInspectorAction::View(
                RecursiveDagInspectorView::Test,
            ));
        }
        KeyCode::Char('d')
            if on_artifacts_panel
                && inspector_open
                && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            inspector_action = Some(DashboardInspectorAction::View(
                RecursiveDagInspectorView::Diff,
            ));
        }
        KeyCode::Char('l') if on_artifacts_panel && inspector_open => {
            inspector_action = Some(DashboardInspectorAction::View(
                RecursiveDagInspectorView::Links,
            ));
        }
        KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => {
            let current_panel = browser_state.panel;
            let next_panel = get_next_dashboard_panel(current_panel);
            browser_state.panel = next_panel;
            browser_state.scroll_offset = 0;
            browser_state.inspector.artifact_nav_prefix = None;
        }
        KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
            let current_panel = browser_state.panel;
            if current_panel == RecursiveDagPanel::Runs {
                focus_back_to_canvas = true;
            } else {
                let prev_panel = get_prev_dashboard_panel(current_panel);
                browser_state.panel = prev_panel;
                browser_state.scroll_offset = 0;
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            browser_state.move_selected(1);
            browser_state.inspector.artifact_nav_prefix = None;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            browser_state.move_selected(-1);
            browser_state.inspector.artifact_nav_prefix = None;
        }
        KeyCode::Char('g') => {
            browser_state.jump_selected_top();
            browser_state.scroll_offset = 0;
        }
        KeyCode::Char('G') => {
            browser_state.jump_selected_bottom();
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            browser_state.scroll_offset = browser_state.scroll_offset.saturating_add(10);
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            browser_state.scroll_offset = browser_state.scroll_offset.saturating_sub(10);
        }
        _ => {}
    }

    if let OverlayState::GraphReview {
        dashboard_focused,
        dashboard_state,
        ..
    } = &mut app.overlay
    {
        if focus_back_to_canvas {
            *dashboard_focused = false;
        }
        *dashboard_state = Some(browser_state);
        app.mark_dirty();
    }

    if let Some(action) = inspector_action {
        match action {
            DashboardInspectorAction::Open => {
                crate::overlay::recursive_dag::open_selected_recursive_dag_inspector(app);
            }
            DashboardInspectorAction::View(view) => {
                crate::overlay::recursive_dag::set_recursive_dag_inspector_view(app, view);
            }
        }
        app.mark_dirty();
    }

    if trigger_reload {
        crate::overlay::recursive_dag::start_dashboard_recursive_dag_load(app, recursive_graph_id);
    }
}

enum DashboardInspectorAction {
    Open,
    View(RecursiveDagInspectorView),
}

const DASHBOARD_PANELS: &[RecursiveDagPanel] = &[
    RecursiveDagPanel::Runs,
    RecursiveDagPanel::Recovery,
    RecursiveDagPanel::Cancellations,
    RecursiveDagPanel::Heartbeats,
    RecursiveDagPanel::Interrupts,
    RecursiveDagPanel::Artifacts,
];

fn get_next_dashboard_panel(current: RecursiveDagPanel) -> RecursiveDagPanel {
    let pos = DASHBOARD_PANELS
        .iter()
        .position(|&p| p == current)
        .unwrap_or(0);
    let next_pos = (pos + 1) % DASHBOARD_PANELS.len();
    DASHBOARD_PANELS[next_pos]
}

fn get_prev_dashboard_panel(current: RecursiveDagPanel) -> RecursiveDagPanel {
    let pos = DASHBOARD_PANELS
        .iter()
        .position(|&p| p == current)
        .unwrap_or(0);
    let prev_pos = if pos == 0 {
        DASHBOARD_PANELS.len() - 1
    } else {
        pos - 1
    };
    DASHBOARD_PANELS[prev_pos]
}

fn focus_dashboard(app: &mut App, recursive_graph_id: Uuid) {
    if let OverlayState::GraphReview {
        dashboard_focused,
        dashboard_state,
        ..
    } = &mut app.overlay
    {
        *dashboard_focused = true;
        if dashboard_state.is_none() {
            let mut state = RecursiveDagBrowserState::loading(app.current_project_id);
            state.panel = RecursiveDagPanel::Runs;
            *dashboard_state = Some(state);
            crate::overlay::recursive_dag::start_dashboard_recursive_dag_load(
                app,
                recursive_graph_id,
            );
        }
        app.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::state::{DevState, PersistedState};
    use crate::types::{RecursiveDagInspectorView, RecursiveDagSelectedGraphData};
    use crossterm::event::KeyModifiers;
    use rsi_common::recursive_dag::{
        RecursiveAttemptId, RecursiveExecutionMode, RecursiveGraphStatus, RecursiveLiveAttemptId,
        RecursiveLiveOutputValidationArtifactLinks, RecursiveLiveOutputValidationIssue,
        RecursiveLiveOutputValidationListItem, RecursiveLiveOutputValidationStatus,
        RecursiveLiveValidationIssueClass, RecursiveLiveValidationIssueCode,
        RecursiveLiveValidationIssueSeverity, RecursiveSchedulerRunId, RecursiveTaskGraphDetail,
        RecursiveTaskGraphId, RecursiveTaskGraphSummary, RecursiveTaskId,
    };
    use rsi_common::rpc::DaemonCapabilities;
    use std::path::PathBuf;

    fn test_app() -> App {
        DevState::clear();
        PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from("/tmp/rsi-graph-test.sock")))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Open a `BridgedRecursive` draft (with a real workflow) in the review
    /// overlay, returning the draft id. Mirrors how `select_recursive_graph`
    /// stamps the origin before `show_graph_review_draft`.
    fn open_bridged_recursive_draft(app: &mut App) -> Uuid {
        let workflow =
            rsi_graph::generate::templates::build_starter_workflow("pingpong").expect("template");
        let draft_id = app.create_graph_draft(workflow, None, None);
        if let Some(draft) = app.graph_draft_mut(&draft_id) {
            draft.view_origin = GraphViewOrigin::BridgedRecursive {
                recursive_graph_id: uuid::Uuid::new_v4(),
            };
        }
        app.show_graph_review_draft(draft_id, false);
        draft_id
    }

    fn sample_summary(title: &str) -> RecursiveTaskGraphSummary {
        let now = chrono::Utc::now();
        RecursiveTaskGraphSummary {
            id: RecursiveTaskGraphId::new(),
            root_task_id: RecursiveTaskId::new(),
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
            max_depth: 1,
            max_fanout: 1,
            max_descendants: 1,
            step_limit: 1,
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

    #[tokio::test]
    async fn bridged_recursive_overlay_is_read_only_and_mutators_inert() {
        let mut app = test_app();
        let draft_id = open_bridged_recursive_draft(&mut app);

        // The propagated overlay origin is BridgedRecursive (gate + badge fire).
        match &app.overlay {
            OverlayState::GraphReview { view_origin, .. } => assert!(
                matches!(view_origin, GraphViewOrigin::BridgedRecursive { .. }),
                "overlay origin must propagate as BridgedRecursive"
            ),
            _ => panic!("expected GraphReview overlay"),
        }

        let before_nodes = app
            .graph_draft(&draft_id)
            .map(|d| d.workflow.nodes.len())
            .unwrap();

        // Navigate-mode delete (`d`) is inert on a read-only bridged draft.
        handle_graph_review_key(&mut app, key(KeyCode::Char('d'))).await;
        let after_delete = app
            .graph_draft(&draft_id)
            .map(|d| d.workflow.nodes.len())
            .unwrap();
        assert_eq!(before_nodes, after_delete, "delete must be inert");

        // Enter detail, attempt edit-field begin (`i`) — must NOT enter EditField.
        handle_graph_review_key(&mut app, key(KeyCode::Enter)).await;
        handle_graph_review_key(&mut app, key(KeyCode::Char('i'))).await;
        match &app.overlay {
            OverlayState::GraphReview { mode, .. } => {
                assert!(
                    !matches!(mode, GraphMode::EditField { .. }),
                    "bridged draft must not enter EditField"
                );
            }
            _ => panic!("expected GraphReview overlay"),
        }
    }

    #[tokio::test]
    async fn bridged_recursive_run_key_does_not_persist_or_execute() {
        let mut app = test_app();
        let draft_id = open_bridged_recursive_draft(&mut app);

        // Pre-condition: draft is Clean (not yet saved).
        assert_eq!(
            app.graph_draft(&draft_id).map(|d| d.persistence_state),
            Some(GraphDraftPersistenceState::Clean)
        );

        // `r` on a BridgedRecursive draft is DOUBLE-blocked: key guard +
        // run_graph_draft/ensure_graph_draft_saved early-return. No persistence.
        handle_graph_review_key(&mut app, key(KeyCode::Char('r'))).await;

        // persistence_state never transitioned to Saving (no upsert attempted).
        assert_eq!(
            app.graph_draft(&draft_id).map(|d| d.persistence_state),
            Some(GraphDraftPersistenceState::Clean),
            "bridged run key must not persist a workflows row"
        );

        // Defense-in-depth: ensure_graph_draft_saved early-returns for a bridged draft.
        assert!(
            ensure_graph_draft_saved(&mut app, draft_id).await.is_none(),
            "ensure_graph_draft_saved must refuse a bridged draft"
        );
        assert!(!draft_is_authored_workflow(&app, &draft_id));
    }

    #[test]
    fn recursive_picker_source_gated_on_cap() {
        // BLOCKER 2: the picker is data-driven on app.recursive_graphs, which is
        // populated only at gated open-time. Empty cache ⇒ zero items (Enter
        // unreachable); populated cache ⇒ item_count tracks the cache length.
        let mut app = test_app();
        assert!(app.recursive_graphs.is_empty());
        assert_eq!(
            picker_item_count(&app, GraphPickerKind::RecursiveGraphs),
            0,
            "empty cache yields zero items"
        );

        app.recursive_graphs = vec![sample_summary("g1"), sample_summary("g2")];
        assert_eq!(
            picker_item_count(&app, GraphPickerKind::RecursiveGraphs),
            2,
            "item count tracks the open-time cache (BLOCKER 2)"
        );
    }

    fn validation_issue(message: &str) -> RecursiveLiveOutputValidationIssue {
        RecursiveLiveOutputValidationIssue {
            code: RecursiveLiveValidationIssueCode::MissingRequiredField,
            severity: RecursiveLiveValidationIssueSeverity::Error,
            class: RecursiveLiveValidationIssueClass::Missing,
            location: None,
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
                created_at: Some(chrono::Utc::now()),
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

    fn artifacts_browser_state(pagination: bool) -> RecursiveDagBrowserState {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let live_attempt_id = RecursiveLiveAttemptId::new();

        let graph_summary = RecursiveTaskGraphSummary {
            id: graph_id,
            root_task_id,
            ..sample_summary("artifacts graph")
        };

        let caps = DaemonCapabilities {
            recursive_dag_inspection: true,
            recursive_dag_live_validation_inspection: false,
            recursive_dag_artifact_list_pagination: pagination,
            recursive_dag_artifact_lookup: pagination,
            recursive_dag_artifact_preview_inspection: pagination,
            ..Default::default()
        };

        let artifact_summary_page = if pagination {
            crate::types::RecursiveDagArtifactSummaryPageState::ready(
                graph_id,
                Vec::new(),
                crate::types::RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
                None,
                false,
                Vec::new(),
            )
        } else {
            crate::types::RecursiveDagArtifactSummaryPageState::capability_disabled(graph_id)
        };

        let detail = RecursiveDagSelectedGraphData {
            graph: RecursiveTaskGraphDetail {
                graph: graph_summary.clone(),
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
            validation_results: vec![validation_item(graph_id, root_task_id, live_attempt_id)],
            artifact_summary_page,
            warnings: Vec::new(),
        };

        let mut state = RecursiveDagBrowserState::ready(
            None,
            caps,
            vec![graph_summary],
            Some(graph_id),
            Some(detail),
        );
        state.panel = RecursiveDagPanel::Artifacts;
        state
    }

    fn set_dashboard_state(app: &mut App, state: RecursiveDagBrowserState) {
        match &mut app.overlay {
            OverlayState::GraphReview {
                dashboard_state,
                dashboard_focused,
                gv_info_dashboard,
                ..
            } => {
                *gv_info_dashboard = true;
                *dashboard_focused = true;
                *dashboard_state = Some(state);
            }
            _ => panic!("expected GraphReview overlay"),
        }
    }

    fn dashboard_state_ref(app: &App) -> &RecursiveDagBrowserState {
        match &app.overlay {
            OverlayState::GraphReview {
                dashboard_state: Some(state),
                ..
            } => state,
            _ => panic!("expected hydrated GraphReview dashboard_state"),
        }
    }

    #[tokio::test]
    async fn gv_renders_inspector_drill_down_modal() {
        let mut app = test_app();
        open_bridged_recursive_draft(&mut app);
        set_dashboard_state(&mut app, artifacts_browser_state(true));

        let rows = dashboard_state_ref(&app).inspector_rows();
        assert!(
            rows.iter().any(|row| matches!(
                row.kind,
                crate::types::RecursiveDagInspectorRowKind::Validation
            )),
            "fixture must yield a validation row"
        );

        handle_dashboard_key(&mut app, key(KeyCode::Enter)).await;

        let state = dashboard_state_ref(&app);
        assert!(
            state.inspector.open,
            "Enter on an Artifacts-panel validation row must open the inspector"
        );
        assert_eq!(state.inspector.view, RecursiveDagInspectorView::Links);
    }

    #[tokio::test]
    async fn gv_inspector_drill_down_gated_when_pagination_disabled() {
        let mut app = test_app();
        open_bridged_recursive_draft(&mut app);
        set_dashboard_state(&mut app, artifacts_browser_state(false));

        handle_dashboard_key(&mut app, key(KeyCode::Enter)).await;

        let state = dashboard_state_ref(&app);
        assert!(
            app.recursive_dag_rx.is_none(),
            "disabled artifact lookup must not start an async artifact load"
        );

        let lines = crate::ui::overlay::recursive_dag::build_dashboard_lines(&app, state, 400);
        let rendered: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            rendered.contains("recursive_dag_artifact_list_pagination=false"),
            "disabled pagination fallback message must render; got: {rendered}"
        );
    }

    #[tokio::test]
    async fn gv_inspector_inherits_test_diff_report_stub_honesty() {
        let mut app = test_app();
        open_bridged_recursive_draft(&mut app);
        let mut state = artifacts_browser_state(true);
        let selected = state.selected_inspector_row();
        state.inspector.open_for(selected.as_ref());
        set_dashboard_state(&mut app, state);

        crate::overlay::recursive_dag::set_recursive_dag_inspector_view(
            &mut app,
            RecursiveDagInspectorView::Test,
        );
        assert_eq!(
            dashboard_state_ref(&app).message.as_deref(),
            Some("typed test detail unavailable: typed recursive test readback is not exposed"),
        );

        crate::overlay::recursive_dag::set_recursive_dag_inspector_view(
            &mut app,
            RecursiveDagInspectorView::Diff,
        );
        assert_eq!(
            dashboard_state_ref(&app).message.as_deref(),
            Some("typed diff detail unavailable: typed recursive diff readback is not exposed"),
        );

        crate::overlay::recursive_dag::set_recursive_dag_inspector_view(
            &mut app,
            RecursiveDagInspectorView::Report,
        );
        assert_eq!(
            dashboard_state_ref(&app).message.as_deref(),
            Some(
                "typed scheduler report inspector unavailable: report detail readback is not exposed"
            ),
        );
    }

    /// Open an authored (editable) draft and begin editing the first node's
    /// instructions, returning the pre-paste buffer contents.
    fn open_authored_editing_instructions(app: &mut App) -> (Uuid, String) {
        let workflow = rsi_graph::generate::templates::build_starter_workflow("master_orchestrate")
            .expect("template");
        let draft_id = app.create_graph_draft(workflow, None, None);
        app.show_graph_review_draft(draft_id, false);
        begin_edit_field(app, draft_id, 0, GraphEditField::Instructions);
        let buffer = match &app.overlay {
            OverlayState::GraphReview { edit_buffer, .. } => edit_buffer.clone(),
            _ => panic!("expected GraphReview overlay"),
        };
        (draft_id, buffer)
    }

    #[test]
    fn paste_appends_into_instructions_edit_buffer() {
        let mut app = test_app();
        let (_draft_id, before) = open_authored_editing_instructions(&mut app);

        let handled = crate::overlay::try_paste_text_overlay(&mut app, "PASTED_CLIP");
        assert!(handled, "paste should be consumed by the graph overlay");

        match &app.overlay {
            OverlayState::GraphReview {
                edit_buffer, mode, ..
            } => {
                assert!(mode.editing_field().is_some(), "still in edit mode");
                assert_eq!(*edit_buffer, format!("{before}PASTED_CLIP"));
            }
            _ => panic!("expected GraphReview overlay"),
        }
    }

    #[test]
    fn paste_is_noop_when_not_editing_a_field() {
        let mut app = test_app();
        let workflow = rsi_graph::generate::templates::build_starter_workflow("master_orchestrate")
            .expect("template");
        let draft_id = app.create_graph_draft(workflow, None, None);
        app.show_graph_review_draft(draft_id, false);

        // Navigation mode (no field being edited) -> paste must not seed the buffer.
        let handled = crate::overlay::try_paste_text_overlay(&mut app, "PASTED_CLIP");
        assert!(handled, "overlay still consumes the paste event");
        match &app.overlay {
            OverlayState::GraphReview {
                edit_buffer, mode, ..
            } => {
                assert!(mode.editing_field().is_none(), "not editing any field");
                assert!(
                    edit_buffer.is_empty(),
                    "buffer must stay empty when not editing"
                );
            }
            _ => panic!("expected GraphReview overlay"),
        }
    }
}
