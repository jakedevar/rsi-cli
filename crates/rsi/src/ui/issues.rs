//! Dense first-class Issue workspace renderer.

use crate::app::App;
use crate::types::{
    IssueEditorMode, IssueWorkspaceDataKind, IssueWorkspaceLoadState, IssueWorkspaceState,
    IssueWorkspaceTab,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

pub fn render_issue_workspace(
    frame: &mut Frame,
    area: Rect,
    focused: bool,
    state: &mut IssueWorkspaceState,
    app: &App,
) {
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    let active = |tab| {
        if state.active_tab == tab {
            Modifier::BOLD | Modifier::UNDERLINED
        } else {
            Modifier::empty()
        }
    };
    let project = state
        .project_id
        .and_then(|id| app.projects.iter().find(|project| project.id == id))
        .map(|project| project.name.as_str())
        .unwrap_or("No project");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Issues ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                "Local",
                Style::default().add_modifier(active(IssueWorkspaceTab::Local)),
            ),
            Span::raw("  "),
            Span::styled(
                "Dispatched",
                Style::default().add_modifier(active(IssueWorkspaceTab::Dispatched)),
            ),
            Span::raw("  "),
            Span::styled(
                "Sync",
                Style::default().add_modifier(active(IssueWorkspaceTab::Sync)),
            ),
            Span::raw(format!("  {project}")),
        ])),
        chunks[0],
    );
    if state.project_id.is_none() {
        frame.render_widget(
            Paragraph::new(
                "NO PROJECT SELECTED\nChoose a project, then press r to refresh Issues.",
            )
            .block(
                Block::default()
                    .title(" Issues unavailable ")
                    .borders(Borders::ALL),
            ),
            chunks[1],
        );
        return;
    }
    match state.active_tab {
        IssueWorkspaceTab::Local => render_local(frame, chunks[1], focused, state, app),
        IssueWorkspaceTab::Dispatched => render_dispatched(frame, chunks[1], focused, state, app),
        IssueWorkspaceTab::Sync => render_sync(frame, chunks[1], focused, state),
    }
}

fn render_local(
    frame: &mut Frame,
    area: Rect,
    focused: bool,
    state: &mut IssueWorkspaceState,
    app: &App,
) {
    let regions = if state.local.inspector_open || state.transient.editor.is_some() {
        Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).split(area)
    } else {
        Layout::horizontal([Constraint::Percentage(100), Constraint::Percentage(0)]).split(area)
    };
    let table_area = regions[0];
    let viewport_rows = table_area.height.saturating_sub(3) as usize;
    state.transient.local_viewport_rows = viewport_rows;
    let row_count = state
        .transient
        .current_page
        .as_ref()
        .map_or(0, |page| page.rows.len());
    let selected_row = state
        .local
        .selected_issue_id
        .and_then(|issue_id| {
            state
                .transient
                .current_page
                .as_ref()?
                .rows
                .iter()
                .position(|row| row.issue.id == issue_id)
        })
        .unwrap_or_else(|| state.local.selected_row.min(row_count.saturating_sub(1)));
    state.local.scroll_offset = selection_scroll_offset(
        state.local.scroll_offset,
        selected_row,
        row_count,
        viewport_rows,
    );
    let load_state = state.load_state(IssueWorkspaceDataKind::LocalPage);
    let load_error = state.data_error(IssueWorkspaceDataKind::LocalPage);
    let archive = match state.local.filters.archive {
        rsi_common::types::IssueArchiveFilterV1::Active => "Active",
        rsi_common::types::IssueArchiveFilterV1::Archived => "Archived",
        rsi_common::types::IssueArchiveFilterV1::All => "All",
    };
    let confirmation = state
        .transient
        .cancel_confirmation
        .as_ref()
        .map(|confirmation| format!(" · CANCEL #{} — press d again", confirmation.display_number))
        .or_else(|| {
            state
                .transient
                .archive_confirmation
                .as_ref()
                .map(|confirmation| {
                    format!(
                        " · ARCHIVE #{} — press Enter to confirm",
                        confirmation.display_number
                    )
                })
        })
        .unwrap_or_default();
    let retry = state
        .transient
        .mutation_retry_error
        .as_ref()
        .map(|_| " · WRITE UNKNOWN — Ctrl-Enter retries identical request")
        .unwrap_or_default();
    let mutation_failure = (state.transient.mutation_retry_error.is_none())
        .then(|| state.data_error(IssueWorkspaceDataKind::Mutation))
        .flatten()
        .map(|error| format!(" · WRITE FAILED — {error}"))
        .unwrap_or_default();
    let block = Block::default()
        .title(format!(
            " Local [{} · {} · {archive}]{}{confirmation}{retry}{mutation_failure} ",
            load_label(load_state),
            state
                .local
                .saved_view
                .map(|view| format!("{view:?}"))
                .unwrap_or_else(|| "Custom".to_string()),
            load_error
                .filter(|_| matches!(
                    load_state,
                    IssueWorkspaceLoadState::Stale
                        | IssueWorkspaceLoadState::Error
                        | IssueWorkspaceLoadState::AccessDenied
                ))
                .map(|error| format!(" · {error}"))
                .unwrap_or_default(),
        ))
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        });
    let Some(page) = &state.transient.current_page else {
        let message = match load_state {
            IssueWorkspaceLoadState::Loading => "Loading issues…",
            IssueWorkspaceLoadState::AccessDenied => "Access denied — operator authority required",
            IssueWorkspaceLoadState::Stale => "Stale — refreshing issues…",
            IssueWorkspaceLoadState::Error => load_error.unwrap_or("Issue workspace unavailable"),
            IssueWorkspaceLoadState::Fresh => "No issues",
        };
        frame.render_widget(Paragraph::new(message).block(block), table_area);
        render_local_side(frame, regions[1], state, app);
        return;
    };
    if page.rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No issues match this bounded view — press f or / to adjust filters")
                .block(block),
            table_area,
        );
        render_local_side(frame, regions[1], state, app);
        return;
    }
    let geometry = local_table_geometry(table_area.width);
    let rows = page
        .rows
        .iter()
        .skip(state.local.scroll_offset)
        .map(|row| {
            let selected = state.local.selected_issue_id == Some(row.issue.id);
            let cells = local_row_cells(row, selected, &geometry);
            debug_assert_eq!(cells.len(), geometry.headers.len());
            Row::new(cells).style(if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            })
        })
        .collect::<Vec<_>>();
    debug_assert_eq!(geometry.headers.len(), geometry.widths.len());
    let table = Table::new(rows, geometry.widths)
        .header(Row::new(geometry.headers))
        .block(block);
    frame.render_widget(table, table_area);
    render_local_side(frame, regions[1], state, app);
}

fn render_local_side(frame: &mut Frame, area: Rect, state: &mut IssueWorkspaceState, app: &App) {
    if let Some(editor) = &state.transient.editor {
        render_editor(frame, area, editor);
    } else if state.local.inspector_open {
        render_inspector(frame, area, state, app);
    }
}

fn render_inspector(frame: &mut Frame, area: Rect, state: &mut IssueWorkspaceState, app: &App) {
    let Some(issue_id) = state.local.selected_issue_id else {
        frame.render_widget(Paragraph::new("No issue selected"), area);
        return;
    };
    if state.transient.missing_selected_issue_id == Some(issue_id) {
        frame.render_widget(
            Paragraph::new(
                state
                    .data_error(IssueWorkspaceDataKind::SelectedIssue)
                    .unwrap_or("SELECTED ISSUE UNAVAILABLE — refresh Local to recover"),
            ),
            area,
        );
        return;
    }
    let issue = state
        .transient
        .selected_issue
        .as_ref()
        .filter(|issue| {
            state.transient.selected_issue_target == Some(issue_id) && issue.id == issue_id
        })
        .or_else(|| {
            state
                .transient
                .current_page
                .as_ref()
                .and_then(|page| page.rows.iter().find(|row| row.issue.id == issue_id))
                .map(|row| &row.issue)
        });
    let Some(issue) = issue else {
        frame.render_widget(
            Paragraph::new(
                state
                    .data_error(IssueWorkspaceDataKind::SelectedIssue)
                    .unwrap_or("Loading selected issue…"),
            ),
            area,
        );
        return;
    };
    let readiness = state
        .transient
        .current_page
        .as_ref()
        .and_then(|page| page.rows.iter().find(|row| row.issue.id == issue.id))
        .map(|row| {
            format!(
                "{:?} ({} open blockers)",
                row.readiness, row.open_blocker_count
            )
        })
        .unwrap_or_else(|| "Loading".to_string());
    let deps = |kind: IssueWorkspaceDataKind,
                target: Option<uuid::Uuid>,
                page: &Option<rsi_common::issue_workspace::IssueDependencyPageV1>| {
        page.as_ref()
            .filter(|_| target == Some(issue_id))
            .map(|page| {
                page.items
                    .iter()
                    .map(|item| {
                        format!(
                            "#{} {}",
                            item.related_issue.display_number, item.related_issue.title
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| state.data_error(kind).unwrap_or("loading").to_string())
    };
    let issue_id_string = issue.id.to_string();
    let mut associated = app
        .sessions
        .values()
        .filter(|session| {
            session.session.issue_tracker_id.as_deref() == Some(issue_id_string.as_str())
        })
        .map(|session| (session.session.id, format!("{:?}", session.session.status)))
        .collect::<Vec<_>>();
    associated.sort_by_key(|(id, _)| *id);
    let associated = associated
        .into_iter()
        .map(|(id, status)| {
            format!(
                "{}{} {status}",
                if state.local.selected_associated_session_id == Some(id) {
                    "> "
                } else {
                    "  "
                },
                id
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let events = state
        .transient
        .events
        .iter()
        .filter(|_| state.transient.events_target == Some(issue_id))
        .map(|event| {
            format!(
                "{} {:?} {:?} {}",
                event.sequence,
                event.operation,
                event.actor_kind,
                event.occurred_at.to_rfc3339()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let events = if events.is_empty() {
        state
            .data_error(IssueWorkspaceDataKind::Events)
            .unwrap_or("none")
            .to_string()
    } else {
        events
    };
    let cursor_state =
        |target: Option<uuid::Uuid>,
         page: &Option<rsi_common::issue_workspace::IssueDependencyPageV1>| {
            (target == Some(issue_id))
                .then(|| page.as_ref())
                .flatten()
                .map(|page| {
                    format!(
                        "prev={} next={}",
                        page.previous_cursor.is_some(),
                        page.next_cursor.is_some()
                    )
                })
                .unwrap_or_else(|| "loading".to_string())
        };
    let (events_after_sequence, events_next_after_sequence) =
        if state.transient.events_target == Some(issue_id) {
            (
                state.transient.events_after_sequence,
                state.transient.events_next_after_sequence,
            )
        } else {
            (None, None)
        };
    let text = format!(
        "UUID: {}\nProject: {}\nNumber: #{}\nTitle: {}\nBody: {}\nStatus: {}\nPriority: {}\nArchive: {}\nReadiness: {}\nLabels: {}\nAssignee: {}\nBlocked by [{}]: {}\nBlocks [{}]: {}\nInspector page: {:?} · Tab section · PageUp/PageDown bounded pages\nCreator session: {}\nLinked idea: {}\nSource event: {}\nSource finding: {}\nCreated: {}\nUpdated: {}\nClosed: {}\nArchived: {}\nRow version: {}\nAssociated sessions: {}\nEvents [after={:?} next={:?}]:\n{}",
        issue.id,
        issue.project_id,
        issue.display_number,
        issue.title,
        issue.body,
        issue_status_label(issue.status),
        issue
            .priority
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".into()),
        if issue.archived_at.is_some() {
            "ARCHIVED"
        } else {
            "Active"
        },
        readiness,
        if issue.labels.is_empty() {
            "none".to_string()
        } else {
            issue.labels.join(", ")
        },
        issue.assignee.as_deref().unwrap_or("unassigned"),
        cursor_state(
            state.transient.blocked_by_target,
            &state.transient.blocked_by,
        ),
        deps(
            IssueWorkspaceDataKind::BlockedBy,
            state.transient.blocked_by_target,
            &state.transient.blocked_by,
        ),
        cursor_state(state.transient.blocks_target, &state.transient.blocks),
        deps(
            IssueWorkspaceDataKind::Blocks,
            state.transient.blocks_target,
            &state.transient.blocks,
        ),
        state.local.inspector_section,
        issue
            .created_by_session_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "operator".into()),
        issue
            .idea_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".into()),
        issue
            .source_event_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".into()),
        issue
            .source_finding_ref
            .as_ref()
            .map(|value| format!("{value:?}"))
            .unwrap_or_else(|| "none".into()),
        issue.created_at.to_rfc3339(),
        issue.updated_at.to_rfc3339(),
        issue
            .closed_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "open".into()),
        issue
            .archived_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "active".into()),
        issue.row_version,
        if associated.is_empty() {
            "none"
        } else {
            &associated
        },
        events_after_sequence,
        events_next_after_sequence,
        events,
    );
    let viewport_rows = area.height.saturating_sub(2) as usize;
    state.transient.inspector_viewport_rows = viewport_rows;
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let line_count = paragraph.line_count(area.width.saturating_sub(2));
    state.local.inspector_scroll_offset = bounded_scroll_offset(
        state.local.inspector_scroll_offset,
        line_count,
        viewport_rows,
    );
    frame.render_widget(
        paragraph
            .scroll((
                state.local.inspector_scroll_offset.min(u16::MAX as usize) as u16,
                0,
            ))
            .block(Block::default().title(" Inspector ").borders(Borders::ALL)),
        area,
    );
}

fn render_editor(frame: &mut Frame, area: Rect, editor: &crate::types::IssueEditorState) {
    let marker = |field| {
        if editor.active_field == field {
            ">"
        } else {
            " "
        }
    };
    let (mode, text) = match editor.mode {
        IssueEditorMode::Filter => {
            let filters = editor.filter_draft.as_ref();
            let value = |field| {
                if editor.active_field == field {
                    editor.dependency_text.clone()
                } else {
                    match (filters, field) {
                        (_, 0) => editor
                            .filter_saved_view
                            .map(|view| format!("{view:?}"))
                            .unwrap_or_else(|| "Custom".to_string()),
                        (Some(filters), 1) => filters.query.clone().unwrap_or_default(),
                        (Some(filters), 2) => filters
                            .statuses
                            .iter()
                            .map(|status| status.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                        (Some(filters), 3) => filters
                            .priorities
                            .iter()
                            .map(u8::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                        (Some(filters), 4) => format!("{:?}", filters.readiness),
                        (Some(filters), 5) if filters.unassigned => "- (unassigned)".to_string(),
                        (Some(filters), 5) => filters.assignee.clone().unwrap_or_default(),
                        (Some(filters), 6) => filters.labels_all.join(","),
                        (Some(filters), 7) => filters
                            .provenance
                            .iter()
                            .map(|value| format!("{value:?}"))
                            .collect::<Vec<_>>()
                            .join(","),
                        (Some(filters), 8) => format!("{:?}", filters.archive),
                        _ => String::new(),
                    }
                }
            };
            (
                "Filters",
                format!(
                    "{} Saved view: {}\n{} Query: {}\n{} Statuses: {}\n{} Priorities: {}\n{} Readiness: {}\n{} Assignee (- = unassigned): {}\n{} Labels: {}\n{} Provenance: {}\n{} Archive: {}\n\nTab fields · ↑/↓ choices · Enter select · Ctrl-Enter apply · Esc cancel{}",
                    marker(0),
                    value(0),
                    marker(1),
                    value(1),
                    marker(2),
                    value(2),
                    marker(3),
                    value(3),
                    marker(4),
                    value(4),
                    marker(5),
                    value(5),
                    marker(6),
                    value(6),
                    marker(7),
                    value(7),
                    marker(8),
                    value(8),
                    editor
                        .error
                        .as_ref()
                        .map(|error| format!("\n{error}"))
                        .unwrap_or_default(),
                ),
            )
        }
        IssueEditorMode::Dependency { .. } => {
            let candidates = if editor.dependency_candidates.is_empty() {
                "  Loading or no matching candidates".to_string()
            } else {
                editor
                    .dependency_candidates
                    .iter()
                    .enumerate()
                    .map(|(index, row)| {
                        format!(
                            "{} #{} {} {}",
                            if index == editor.dependency_selected_row {
                                ">"
                            } else {
                                " "
                            },
                            row.issue.display_number,
                            row.issue.title,
                            row.issue.id
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (
                "Dependencies",
                format!(
                    "{} Direction: {}\n{} Candidate query: {}\n{} Candidates:\n{}\n\nTab fields · ↑/↓ choose · Enter select · Ctrl-Enter add · Ctrl-D remove · Esc cancel{}",
                    marker(0),
                    match editor.dependency_direction {
                        rsi_common::issue_workspace::IssueDependencyDirectionV1::BlockedBy =>
                            "Blocked by",
                        rsi_common::issue_workspace::IssueDependencyDirectionV1::Blocks => "Blocks",
                    },
                    marker(1),
                    editor.dependency_text,
                    marker(2),
                    candidates,
                    editor
                        .error
                        .as_ref()
                        .map(|error| format!("\n{error}"))
                        .unwrap_or_default(),
                ),
            )
        }
        IssueEditorMode::Create | IssueEditorMode::Edit { .. } | IssueEditorMode::Status { .. } => {
            let select_hint = if matches!(editor.mode, IssueEditorMode::Status { .. }) {
                " · Enter select status"
            } else {
                ""
            };
            let reconciliation = if editor.stale_conflict || editor.latest_issue.is_some() {
                format!(
                    "\n\nBase: {}\nLatest: {}\nDraft: title={} | body={} | priority={} | assignee={} | labels={} | status={}{}",
                    editor
                        .base_issue
                        .as_ref()
                        .map(issue_projection)
                        .unwrap_or_else(|| "new issue".to_string()),
                    editor
                        .latest_issue
                        .as_ref()
                        .map(issue_projection)
                        .unwrap_or_else(|| "loading matching Latest".to_string()),
                    editor.title,
                    editor.body,
                    editor
                        .priority
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "clear".to_string()),
                    editor.assignee.as_deref().unwrap_or("unassigned"),
                    if editor.labels.is_empty() {
                        "none".to_string()
                    } else {
                        editor.labels.join(",")
                    },
                    editor.status.map(issue_status_label).unwrap_or("—"),
                    if editor.stale_conflict {
                        "\nSAVE DISABLED — Ctrl-L reload or Ctrl-R rebase"
                    } else {
                        ""
                    },
                )
            } else {
                String::new()
            };
            (
                match editor.mode {
                    IssueEditorMode::Create => "Create",
                    IssueEditorMode::Edit { .. } => "Edit",
                    IssueEditorMode::Status { .. } => "Status",
                    _ => unreachable!(),
                },
                format!(
                    "{} Title: {}\n{} Body: {}\n{} Priority: {}\n{} Assignee: {}\n{} Labels: {}\nStatus: {}\n\nTab/Shift-Tab fields{select_hint} · Ctrl-Enter save · Esc cancel{}{}{}",
                    marker(0),
                    editor.title,
                    marker(1),
                    editor.body,
                    marker(2),
                    editor
                        .priority
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "clear".into()),
                    marker(3),
                    editor.assignee.as_deref().unwrap_or("unassigned"),
                    marker(4),
                    if editor.labels.is_empty() {
                        "none".to_string()
                    } else {
                        editor.labels.join(", ")
                    },
                    editor.status.map(issue_status_label).unwrap_or("—"),
                    if editor.submitted {
                        "\nSaving immutable request…"
                    } else {
                        ""
                    },
                    reconciliation,
                    editor
                        .error
                        .as_ref()
                        .map(|error| format!("\n{error}"))
                        .unwrap_or_default(),
                ),
            )
        }
    };
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .title(format!(" {mode} Issue "))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn issue_projection(issue: &rsi_common::types::Issue) -> String {
    format!(
        "v{} title={} | body={} | priority={} | assignee={} | labels={} | status={}",
        issue.row_version,
        issue.title,
        issue.body,
        issue
            .priority
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".to_string()),
        issue.assignee.as_deref().unwrap_or("unassigned"),
        if issue.labels.is_empty() {
            "none".to_string()
        } else {
            issue.labels.join(",")
        },
        issue_status_label(issue.status),
    )
}

fn issue_age(updated_at: chrono::DateTime<chrono::Utc>) -> String {
    let seconds = chrono::Utc::now()
        .signed_duration_since(updated_at)
        .num_seconds()
        .max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn issue_status_label(status: rsi_common::types::IssueStatus) -> &'static str {
    match status {
        rsi_common::types::IssueStatus::Open => "Open",
        rsi_common::types::IssueStatus::InProgress => "In progress",
        rsi_common::types::IssueStatus::Closed => "Closed",
        rsi_common::types::IssueStatus::Cancelled => "Cancelled",
    }
}

fn render_dispatched(
    frame: &mut Frame,
    area: Rect,
    focused: bool,
    state: &mut IssueWorkspaceState,
    app: &App,
) {
    let regions = if state.dispatched.inspector_open {
        Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)]).split(area)
    } else {
        Layout::horizontal([Constraint::Percentage(100), Constraint::Percentage(0)]).split(area)
    };
    let table_area = regions[0];
    let load_state = state.load_state(IssueWorkspaceDataKind::Dispatched);
    let block = Block::default()
        .title(format!(
            " Dispatched [{}]{} ",
            load_label(load_state),
            state
                .data_error(IssueWorkspaceDataKind::Dispatched)
                .map(|error| format!(" · {error}"))
                .unwrap_or_default()
        ))
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        });
    let viewport_rows = table_area.height.saturating_sub(3) as usize;
    state.transient.dispatched_viewport_rows = viewport_rows;
    if state.transient.dispatched.is_empty() {
        state.dispatched.scroll_offset = 0;
        let message = match load_state {
            IssueWorkspaceLoadState::Loading => "Loading dispatched issues…",
            IssueWorkspaceLoadState::Fresh => "No dispatched issues",
            IssueWorkspaceLoadState::Stale => "Stale — refreshing dispatched issues…",
            IssueWorkspaceLoadState::Error => state
                .data_error(IssueWorkspaceDataKind::Dispatched)
                .unwrap_or("Dispatched issues unavailable"),
            IssueWorkspaceLoadState::AccessDenied => "Access denied — operator authority required",
        };
        frame.render_widget(Paragraph::new(message).block(block), table_area);
        render_dispatched_inspector(frame, regions[1], state, app);
        return;
    }
    let row_count = state.transient.dispatched.len();
    let selected_row = state
        .dispatched
        .selected_session_id
        .and_then(|session_id| {
            state
                .transient
                .dispatched
                .iter()
                .position(|record| record.session_id == session_id)
        })
        .unwrap_or_else(|| {
            state
                .dispatched
                .selected_row
                .min(row_count.saturating_sub(1))
        });
    state.dispatched.scroll_offset = selection_scroll_offset(
        state.dispatched.scroll_offset,
        selected_row,
        row_count,
        viewport_rows,
    );
    let rows = state
        .transient
        .dispatched
        .iter()
        .skip(state.dispatched.scroll_offset)
        .map(|record| {
            let selected = state.dispatched.selected_session_id == Some(record.session_id);
            Row::new(vec![
                Cell::from(format!(
                    "{}{}",
                    if selected { ">" } else { " " },
                    record.issue_identifier
                )),
                Cell::from(record.tracker.clone()),
                Cell::from(record.session_id.to_string()),
                Cell::from(
                    record
                        .terminal_state
                        .clone()
                        .unwrap_or_else(|| "Running".to_string()),
                )
                .style(Style::default().fg(if record.terminal_state.is_some() {
                    crate::ui::theme::disabled()
                } else {
                    crate::ui::theme::status_running()
                })),
                Cell::from(record.dispatched_at.to_rfc3339()),
            ])
            .style(if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            })
        });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(16),
                Constraint::Length(10),
                Constraint::Min(12),
                Constraint::Length(12),
                Constraint::Length(25),
            ],
        )
        .header(Row::new([
            "ID",
            "TRACKER",
            "SESSION",
            "STATE",
            "DISPATCHED",
        ]))
        .block(block),
        table_area,
    );
    render_dispatched_inspector(frame, regions[1], state, app);
}

fn render_dispatched_inspector(
    frame: &mut Frame,
    area: Rect,
    state: &mut IssueWorkspaceState,
    app: &App,
) {
    if !state.dispatched.inspector_open || area.width == 0 || area.height == 0 {
        return;
    }
    let record = state.transient.dispatched.iter().find(|record| {
        state.dispatched.selected_session_id == Some(record.session_id)
            && state.dispatched.selected_issue_id.as_deref() == Some(record.issue_id.as_str())
    });
    let Some(record) = record else {
        frame.render_widget(
            Paragraph::new("Selected dispatch is no longer in the current bounded set").block(
                Block::default()
                    .title(" Dispatch inspector ")
                    .borders(Borders::ALL),
            ),
            area,
        );
        return;
    };
    let cached = app.sessions.get(&record.session_id);
    let cache_state = cached
        .map(|session| {
            format!(
                "Session title: {}\nSession status: {:?}\nEnter: open exact cached session",
                session
                    .session
                    .title
                    .as_deref()
                    .unwrap_or("Untitled session"),
                session.session.status
            )
        })
        .unwrap_or_else(|| {
            "SESSION NOT IN CURRENT CACHE\nPress r to refresh or Enter to recover the exact session"
                .to_string()
        });
    let content = format!(
        "Internal tracker Issue ID: {}\nIdentifier: {}\nTracker: {}\nSession UUID: {}\n{}\nDispatched: {}\nLast reconciled: {}\nTerminal state: {}",
        record.issue_id,
        record.issue_identifier,
        record.tracker,
        record.session_id,
        cache_state,
        record.dispatched_at.to_rfc3339(),
        record
            .last_reconciled_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "never".to_string()),
        record.terminal_state.as_deref().unwrap_or("Running"),
    );
    let viewport_rows = area.height.saturating_sub(2) as usize;
    let paragraph = Paragraph::new(content).wrap(Wrap { trim: false });
    let line_count = paragraph.line_count(area.width.saturating_sub(2));
    state.dispatched.inspector_scroll_offset = bounded_scroll_offset(
        state.dispatched.inspector_scroll_offset,
        line_count,
        viewport_rows,
    );
    frame.render_widget(
        paragraph
            .scroll((
                state
                    .dispatched
                    .inspector_scroll_offset
                    .min(u16::MAX as usize) as u16,
                0,
            ))
            .block(
                Block::default()
                    .title(" Dispatch inspector ")
                    .borders(Borders::ALL),
            ),
        area,
    );
}

fn render_sync(frame: &mut Frame, area: Rect, focused: bool, state: &mut IssueWorkspaceState) {
    let load_state = state.load_state(IssueWorkspaceDataKind::SyncStatus);
    let manual_poll = manual_poll_projection(state);
    let content = if let Some(status) = &state.transient.sync_status {
        let status_error = state
            .data_error(IssueWorkspaceDataKind::SyncStatus)
            .map(|error| format!("\nStatus refresh error: {error}"))
            .unwrap_or_default();
        format!(
            "Tracker: {}\nEnabled: {}\nDispatched: {}/{}\nInterval: {} ms\nLast poll: {}\nNext poll: {}\nActive states: {}{}{}",
            status.tracker,
            status.enabled,
            status.dispatched_count,
            status.max_concurrent,
            status.poll_interval_ms,
            status
                .last_poll_at
                .map(|time| time.to_rfc3339())
                .unwrap_or_else(|| "never".into()),
            status
                .next_poll_at
                .map(|time| time.to_rfc3339())
                .unwrap_or_else(|| "unscheduled".into()),
            status.active_states.join(", "),
            status_error,
            manual_poll,
        )
    } else {
        let status = match load_state {
            IssueWorkspaceLoadState::Loading => "Loading tracker status…".to_string(),
            IssueWorkspaceLoadState::Stale => "Stale — refreshing tracker status…".to_string(),
            IssueWorkspaceLoadState::AccessDenied => {
                "Access denied — operator authority required".to_string()
            }
            IssueWorkspaceLoadState::Error => state
                .data_error(IssueWorkspaceDataKind::SyncStatus)
                .map(str::to_string)
                .unwrap_or_else(|| "Tracker status unavailable".to_string()),
            IssueWorkspaceLoadState::Fresh => "Tracker status unavailable".to_string(),
        };
        format!("{status}{manual_poll}")
    };
    let viewport_rows = area.height.saturating_sub(2) as usize;
    state.transient.sync_viewport_rows = viewport_rows;
    let paragraph = Paragraph::new(content).wrap(Wrap { trim: false });
    let line_count = paragraph.line_count(area.width.saturating_sub(2));
    state.sync.scroll_offset =
        bounded_scroll_offset(state.sync.scroll_offset, line_count, viewport_rows);
    frame.render_widget(
        paragraph
            .scroll((state.sync.scroll_offset.min(u16::MAX as usize) as u16, 0))
            .block(
                Block::default()
                    .title(format!(" Sync [{}] ", load_label(load_state)))
                    .borders(Borders::ALL)
                    .border_style(if focused {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
            ),
        area,
    );
}

fn manual_poll_projection(state: &IssueWorkspaceState) -> String {
    let poll = state
        .transient
        .manual_poll_result
        .as_ref()
        .map(|result| {
            let errors = result
                .errors
                .iter()
                .enumerate()
                .map(|(index, error)| format!("\nPoll error {}: {error}", index + 1))
                .collect::<String>();
            format!(
                "\nLast manual poll: found {}, dispatched {}, claimed {}, blocked {}, errors {}{}",
                result.issues_found,
                result.dispatched,
                result.skipped_claimed,
                result.skipped_blocked,
                result.errors.len(),
                errors,
            )
        })
        .unwrap_or_default();
    let error = state
        .transient
        .manual_poll_error
        .as_ref()
        .map(|error| format!("\nLast manual poll error: {error}"))
        .unwrap_or_default();
    format!("{poll}{error}")
}

#[derive(Debug, Clone, PartialEq)]
struct LocalTableGeometry {
    show_priority: bool,
    show_owner: bool,
    show_age: bool,
    headers: Vec<&'static str>,
    widths: Vec<Constraint>,
}

fn local_table_geometry(width: u16) -> LocalTableGeometry {
    let show_priority = width >= 72;
    let show_owner = width >= 100;
    let show_age = width >= 120;
    let mut headers = vec!["#"];
    let mut widths = vec![Constraint::Length(8)];
    if show_priority {
        headers.push("P");
        widths.push(Constraint::Length(3));
    }
    headers.extend(["STATE", "READY", "TITLE"]);
    widths.extend([
        Constraint::Length(18),
        Constraint::Length(15),
        Constraint::Min(12),
    ]);
    if show_owner {
        headers.push("OWNER");
        widths.push(Constraint::Length(14));
    }
    if show_age {
        headers.push("AGE");
        widths.push(Constraint::Length(8));
    }
    LocalTableGeometry {
        show_priority,
        show_owner,
        show_age,
        headers,
        widths,
    }
}

fn local_row_cells(
    row: &rsi_common::issue_workspace::IssueWorkspaceRowV1,
    selected: bool,
    geometry: &LocalTableGeometry,
) -> Vec<Cell<'static>> {
    let mut cells = vec![Cell::from(format!(
        "{}#{}",
        if selected { ">" } else { " " },
        row.issue.display_number
    ))];
    if geometry.show_priority {
        cells.push(Cell::from(
            row.issue
                .priority
                .map(|value| value.to_string())
                .unwrap_or_else(|| "—".to_string()),
        ));
    }
    cells.push(Cell::from(if row.issue.archived_at.is_some() {
        format!("{} ARCHIVED", issue_status_label(row.issue.status))
    } else {
        issue_status_label(row.issue.status).to_string()
    }));
    cells.push(Cell::from(match row.readiness {
        rsi_common::issue_workspace::IssueWorkspaceReadinessV1::Ready => "● Ready",
        rsi_common::issue_workspace::IssueWorkspaceReadinessV1::Blocked => "× Blocked",
        rsi_common::issue_workspace::IssueWorkspaceReadinessV1::NotEligible => "— Not eligible",
    }));
    cells.push(Cell::from(row.issue.title.clone()));
    if geometry.show_owner {
        cells.push(Cell::from(
            row.issue
                .assignee
                .clone()
                .unwrap_or_else(|| "unassigned".to_string()),
        ));
    }
    if geometry.show_age {
        cells.push(Cell::from(issue_age(row.issue.updated_at)));
    }
    cells
}

fn selection_scroll_offset(
    current: usize,
    selected: usize,
    row_count: usize,
    viewport_rows: usize,
) -> usize {
    if row_count == 0 || viewport_rows == 0 {
        return 0;
    }
    let max_offset = row_count.saturating_sub(viewport_rows);
    let mut offset = current.min(max_offset);
    if selected < offset {
        offset = selected;
    } else if selected >= offset.saturating_add(viewport_rows) {
        offset = selected.saturating_add(1).saturating_sub(viewport_rows);
    }
    offset.min(max_offset)
}

fn bounded_scroll_offset(current: usize, line_count: usize, viewport_rows: usize) -> usize {
    if viewport_rows == 0 {
        0
    } else {
        current.min(line_count.saturating_sub(viewport_rows))
    }
}

fn load_label(state: IssueWorkspaceLoadState) -> &'static str {
    match state {
        IssueWorkspaceLoadState::Loading => "Loading",
        IssueWorkspaceLoadState::Fresh => "Fresh",
        IssueWorkspaceLoadState::Stale => "Stale",
        IssueWorkspaceLoadState::Error => "Error",
        IssueWorkspaceLoadState::AccessDenied => "Access denied",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::with_session_list;
    use crate::types::IssueWorkspaceState;
    use ratatui::{Terminal, backend::TestBackend};
    use rsi_common::issue_workspace::{
        IssueWorkspacePageV1, IssueWorkspaceReadinessV1, IssueWorkspaceRowV1,
    };
    use rsi_common::types::{Issue, IssueStatus};
    use uuid::Uuid;

    fn issue(project_id: Uuid, issue_id: Uuid) -> Issue {
        let now = chrono::Utc::now();
        Issue {
            id: issue_id,
            project_id,
            display_number: 184,
            title: "Recurring jobs skip cadence".to_string(),
            body: "Full visible issue body".to_string(),
            status: IssueStatus::InProgress,
            priority: Some(1),
            labels: vec!["scheduler".to_string(), "reliability".to_string()],
            assignee: Some("jake".to_string()),
            created_by_session_id: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
            archived_at: None,
            row_version: 7,
        }
    }

    fn render_text(app: &crate::app::App, state: &IssueWorkspaceState, width: u16) -> String {
        let mut state = state.clone();
        render_text_mut(app, &mut state, width, 60)
    }

    fn render_text_mut(
        app: &crate::app::App,
        state: &mut IssueWorkspaceState,
        width: u16,
        height: u16,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| {
                render_issue_workspace(frame, frame.area(), true, state, app);
            })
            .expect("render Issues workspace");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn local_table_geometry_keeps_header_cell_and_constraint_counts_exact() {
        let project_id = Uuid::new_v4();
        let row = IssueWorkspaceRowV1 {
            issue: issue(project_id, Uuid::new_v4()),
            readiness: IssueWorkspaceReadinessV1::Ready,
            open_blocker_count: 0,
            dependent_count: 0,
        };
        for (width, expected_headers) in [
            (70, vec!["#", "STATE", "READY", "TITLE"]),
            (80, vec!["#", "P", "STATE", "READY", "TITLE"]),
            (110, vec!["#", "P", "STATE", "READY", "TITLE", "OWNER"]),
            (
                130,
                vec!["#", "P", "STATE", "READY", "TITLE", "OWNER", "AGE"],
            ),
        ] {
            let geometry = local_table_geometry(width);
            assert_eq!(geometry.headers, expected_headers);
            assert_eq!(geometry.widths.len(), geometry.headers.len());
            assert_eq!(
                local_row_cells(&row, true, &geometry).len(),
                geometry.headers.len()
            );
        }
    }

    #[test]
    fn issue_workspace_populated_wide_and_narrow_render_positive_identity() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let issue = issue(project_id, issue_id);
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.local.selected_issue_id = Some(issue_id);
        state.local.inspector_open = true;
        state
            .data_state_mut(IssueWorkspaceDataKind::LocalPage)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.selected_issue_target = Some(issue_id);
        state.transient.selected_issue = Some(issue.clone());
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue,
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 2,
            }],
            previous_cursor: None,
            next_cursor: None,
        });

        let wide = render_text(&app, &state, 180);
        assert!(wide.contains("#184"));
        assert!(wide.contains("Recurring jobs skip cadence"));
        assert!(wide.contains("● Ready"));
        assert!(wide.contains(&issue_id.to_string()));
        assert!(wide.contains("Full visible issue body"));
        assert!(wide.contains("Row version: 7"));

        state.local.inspector_open = false;
        let dense = render_text(&app, &state, 160);
        assert!(dense.contains("#"));
        assert!(dense.contains("P"));
        assert!(dense.contains("STATE"));
        assert!(dense.contains("READY"));
        assert!(dense.contains("TITLE"));
        assert!(dense.contains("OWNER"));
        assert!(dense.contains("AGE"));
        assert!(dense.contains("jake"));
        let narrow = render_text(&app, &state, 70);
        assert!(narrow.contains("#184"));
        assert!(narrow.contains("STATE"));
        assert!(narrow.contains("READY"));
        assert!(narrow.contains("TITLE"));
        assert!(narrow.contains("In progress"));
        assert!(narrow.contains("Recurring"));
        assert!(narrow.contains("● Ready"));
    }

    #[test]
    fn short_viewports_keep_local_and_dispatched_selection_visible_and_offsets_durable() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state
            .data_state_mut(IssueWorkspaceDataKind::LocalPage)
            .load_state = IssueWorkspaceLoadState::Fresh;
        let local_rows = (0..10)
            .map(|index| {
                let mut value = issue(project_id, Uuid::new_v4());
                value.display_number = 200 + index;
                value.title = format!("Viewport local identity {index}");
                IssueWorkspaceRowV1 {
                    issue: value,
                    readiness: IssueWorkspaceReadinessV1::Ready,
                    open_blocker_count: 0,
                    dependent_count: 0,
                }
            })
            .collect::<Vec<_>>();
        state.local.selected_row = 9;
        state.local.selected_issue_id = Some(local_rows[9].issue.id);
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: local_rows,
            previous_cursor: None,
            next_cursor: None,
        });
        let local = render_text_mut(&app, &mut state, 96, 9);
        assert_eq!(state.transient.local_viewport_rows, 4);
        assert_eq!(state.local.scroll_offset, 6);
        assert!(local.contains("Viewport local identity 9"));
        state.transient.current_page.as_mut().unwrap().rows[9]
            .issue
            .title = "Viewport refreshed identity 9".to_string();
        let local_refreshed = render_text_mut(&app, &mut state, 96, 9);
        assert_eq!(state.local.scroll_offset, 6);
        assert!(local_refreshed.contains("Viewport refreshed identity 9"));

        state.active_tab = IssueWorkspaceTab::Dispatched;
        state
            .data_state_mut(IssueWorkspaceDataKind::Dispatched)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.dispatched = (0..10)
            .map(|index| rsi_common::issue_workspace::IssueDispatchRecordV1 {
                issue_id: format!("tracker-{index}"),
                issue_identifier: format!("RSI-{}", 300 + index),
                tracker: "linear".to_string(),
                session_id: Uuid::new_v4(),
                dispatched_at: "2026-09-10T12:00:00Z".parse().unwrap(),
                last_reconciled_at: None,
                terminal_state: None,
            })
            .collect();
        state.dispatched.selected_row = 9;
        state.dispatched.selected_session_id = Some(state.transient.dispatched[9].session_id);
        let dispatched = render_text_mut(&app, &mut state, 120, 9);
        assert_eq!(state.transient.dispatched_viewport_rows, 4);
        assert_eq!(state.dispatched.scroll_offset, 6);
        assert!(dispatched.contains("RSI-309"));

        state.transient.dispatched[9].last_reconciled_at =
            Some("2026-09-10T12:01:00Z".parse().unwrap());
        let refreshed = render_text_mut(&app, &mut state, 120, 9);
        assert_eq!(state.dispatched.scroll_offset, 6);
        assert!(refreshed.contains("RSI-309"));
        state.active_tab = IssueWorkspaceTab::Local;
        let local_again = render_text_mut(&app, &mut state, 96, 9);
        assert_eq!(state.local.scroll_offset, 6);
        assert!(local_again.contains("Viewport refreshed identity 9"));
    }

    #[tokio::test]
    async fn top_level_render_persists_local_viewport_from_the_cloned_split_tree() {
        let mut app = with_session_list(0);
        let project_id = Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        let rows = (0..9)
            .map(|index| {
                let mut value = issue(project_id, Uuid::new_v4());
                value.display_number = 400 + index;
                value.title = format!("Viewport row {index}");
                IssueWorkspaceRowV1 {
                    issue: value,
                    readiness: IssueWorkspaceReadinessV1::Ready,
                    open_blocker_count: 0,
                    dependent_count: 0,
                }
            })
            .collect::<Vec<_>>();
        if let Some(crate::types::Pane::Issues(state)) =
            app.active_tab_mut().layout.find_pane_mut(pane_id)
        {
            state.local.selected_row = 8;
            state.local.selected_issue_id = Some(rows[8].issue.id);
            state.transient.current_page = Some(IssueWorkspacePageV1 {
                rows,
                previous_cursor: None,
                next_cursor: None,
            });
            state
                .data_state_mut(IssueWorkspaceDataKind::LocalPage)
                .load_state = IssueWorkspaceLoadState::Fresh;
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).expect("test terminal");
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .expect("render application");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let state = match app.active_tab().layout.find_pane(pane_id) {
            Some(crate::types::Pane::Issues(state)) => state,
            _ => panic!("Issues pane"),
        };
        assert_eq!(state.transient.local_viewport_rows, 4);
        assert_eq!(state.local.scroll_offset, 5);
        assert!(rendered.contains("Viewport row 8"));
    }

    #[test]
    fn short_inspector_and_sync_views_scroll_to_positive_lower_fields() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let selected = issue(project_id, issue_id);
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.focus = crate::types::IssueWorkspaceFocus::Inspector;
        state.local.inspector_open = true;
        state.local.inspector_scroll_offset = usize::MAX;
        state.local.selected_issue_id = Some(issue_id);
        state.transient.selected_issue_target = Some(issue_id);
        state.transient.selected_issue = Some(selected.clone());
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue: selected,
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        let inspector = render_text_mut(&app, &mut state, 180, 10);
        assert!(state.local.inspector_scroll_offset > 0);
        assert_eq!(state.transient.inspector_viewport_rows, 6);
        assert!(inspector.contains("Row version: 7"));
        assert!(inspector.contains("Associated sessions: none"));
        assert!(inspector.contains("Events ["));

        state.active_tab = IssueWorkspaceTab::Sync;
        state.sync.scroll_offset = usize::MAX;
        state
            .data_state_mut(IssueWorkspaceDataKind::SyncStatus)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.sync_status = Some(rsi_common::issue_workspace::IssueTrackerStatusV1 {
            enabled: true,
            tracker: "linear".to_string(),
            last_poll_at: Some("2026-09-10T12:00:00Z".parse().unwrap()),
            next_poll_at: Some("2026-09-10T12:05:00Z".parse().unwrap()),
            dispatched_count: 2,
            max_concurrent: 5,
            poll_interval_ms: 300_000,
            active_states: vec!["started".to_string(), "in_progress".to_string()],
        });
        state.transient.manual_poll_error = Some("Visible lower poll failure".to_string());
        let sync = render_text_mut(&app, &mut state, 100, 7);
        assert!(state.sync.scroll_offset > 0);
        assert_eq!(state.transient.sync_viewport_rows, 3);
        assert!(sync.contains("Active states: started, in_progress"));
        assert!(sync.contains("Last manual poll error: Visible lower poll failure"));
    }

    #[test]
    fn issue_workspace_empty_loading_stale_error_and_denied_are_textually_distinct() {
        let app = with_session_list(0);
        let mut state = IssueWorkspaceState::new(Some(Uuid::new_v4()));
        for (load_state, visible) in [
            (IssueWorkspaceLoadState::Loading, "Loading issues"),
            (IssueWorkspaceLoadState::Stale, "Stale"),
            (IssueWorkspaceLoadState::AccessDenied, "Access denied"),
        ] {
            state
                .data_state_mut(IssueWorkspaceDataKind::LocalPage)
                .load_state = load_state;
            let text = render_text(&app, &state, 90);
            assert!(text.contains(visible));
        }
        let data = state.data_state_mut(IssueWorkspaceDataKind::LocalPage);
        data.load_state = IssueWorkspaceLoadState::Error;
        data.error = Some("Visible recovery error".to_string());
        assert!(render_text(&app, &state, 90).contains("Visible recovery error"));
        state
            .data_state_mut(IssueWorkspaceDataKind::LocalPage)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: Vec::new(),
            previous_cursor: None,
            next_cursor: None,
        });
        let empty = render_text(&app, &state, 90);
        assert!(empty.contains("Local [Fresh · Custom · Active]"));
        assert!(empty.contains("No issues match this bounded view"));
    }

    #[test]
    fn missing_selected_issue_renders_named_recovery_beside_last_local_row() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let disappearing = issue(project_id, issue_id);
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.local.selected_issue_id = Some(issue_id);
        state.local.inspector_open = true;
        state.transient.missing_selected_issue_id = Some(issue_id);
        state.transient.selected_issue_target = Some(issue_id);
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue: disappearing,
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        let local = state.data_state_mut(IssueWorkspaceDataKind::LocalPage);
        local.load_state = IssueWorkspaceLoadState::Fresh;
        local.last_good_ms = Some(1);
        let selected = state.data_state_mut(IssueWorkspaceDataKind::SelectedIssue);
        selected.load_state = IssueWorkspaceLoadState::Error;
        selected.error = Some("SELECTED ISSUE UNAVAILABLE — refresh Local to recover".to_string());

        let rendered = render_text(&app, &state, 180);
        assert!(rendered.contains("Recurring jobs skip cadence"));
        assert!(rendered.contains("SELECTED ISSUE UNAVAILABLE"));
        assert!(rendered.contains("refresh Local to recover"));
    }

    #[test]
    fn issue_workspace_dispatched_and_sync_render_real_typed_fields() {
        let app = with_session_list(0);
        let mut state = IssueWorkspaceState::new(Some(Uuid::new_v4()));
        let session_id = Uuid::new_v4();
        state.active_tab = IssueWorkspaceTab::Dispatched;
        state.dispatched.selected_session_id = Some(session_id);
        state
            .data_state_mut(IssueWorkspaceDataKind::Dispatched)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.dispatched = vec![rsi_common::issue_workspace::IssueDispatchRecordV1 {
            issue_id: "tracker-internal-42".to_string(),
            issue_identifier: "RSI-42".to_string(),
            tracker: "linear".to_string(),
            session_id,
            dispatched_at: "2026-09-10T12:00:00Z".parse().unwrap(),
            last_reconciled_at: Some("2026-09-10T12:01:00Z".parse().unwrap()),
            terminal_state: None,
        }];
        let dispatched = render_text(&app, &state, 160);
        assert!(dispatched.contains("RSI-42"));
        assert!(dispatched.contains("linear"));
        assert!(dispatched.contains(&session_id.to_string()));
        assert!(dispatched.contains("Running"));
        assert!(dispatched.contains("2026-09-10T12:00:00+00:00"));

        state.active_tab = IssueWorkspaceTab::Sync;
        state
            .data_state_mut(IssueWorkspaceDataKind::SyncStatus)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.sync_status = Some(rsi_common::issue_workspace::IssueTrackerStatusV1 {
            enabled: true,
            tracker: "linear".to_string(),
            last_poll_at: Some("2026-09-10T12:00:00Z".parse().unwrap()),
            next_poll_at: Some("2026-09-10T12:05:00Z".parse().unwrap()),
            dispatched_count: 2,
            max_concurrent: 5,
            poll_interval_ms: 300_000,
            active_states: vec!["started".to_string(), "in_progress".to_string()],
        });
        state.transient.manual_poll_result =
            Some(rsi_common::issue_workspace::IssueTrackerTickResultV1 {
                issues_found: 4,
                dispatched: 2,
                skipped_claimed: 1,
                skipped_blocked: 1,
                errors: vec![
                    "first bounded poll failure".to_string(),
                    "second bounded poll failure".to_string(),
                ],
            });
        let sync = render_text(&app, &state, 120);
        assert!(sync.contains("Tracker: linear"));
        assert!(sync.contains("Dispatched: 2/5"));
        assert!(sync.contains("found 4, dispatched 2"));
        assert!(sync.contains("started, in_progress"));
        assert!(sync.contains("Poll error 1: first bounded poll failure"));
        assert!(sync.contains("Poll error 2: second bounded poll failure"));

        state.transient.sync_status = None;
        state
            .data_state_mut(IssueWorkspaceDataKind::SyncStatus)
            .load_state = IssueWorkspaceLoadState::Loading;
        let poll_before_status = render_text(&app, &state, 120);
        assert!(poll_before_status.contains("Loading tracker status"));
        assert!(poll_before_status.contains("Poll error 1: first bounded poll failure"));
        assert!(poll_before_status.contains("Poll error 2: second bounded poll failure"));

        state.transient.sync_status = Some(rsi_common::issue_workspace::IssueTrackerStatusV1 {
            enabled: true,
            tracker: "linear".to_string(),
            last_poll_at: None,
            next_poll_at: None,
            dispatched_count: 2,
            max_concurrent: 5,
            poll_interval_ms: 300_000,
            active_states: vec!["started".to_string()],
        });
        state.transient.manual_poll_result = None;
        state.transient.manual_poll_error = Some("Issue tracker not configured".to_string());
        let sync_data = state.data_state_mut(IssueWorkspaceDataKind::SyncStatus);
        sync_data.load_state = IssueWorkspaceLoadState::Stale;
        sync_data.error = Some("Status refresh retained last good".to_string());
        let failed_poll = render_text(&app, &state, 120);
        assert!(failed_poll.contains("Last manual poll error: Issue tracker not configured"));
        assert!(failed_poll.contains("Status refresh error: Status refresh retained last good"));
    }

    #[test]
    fn issue_workspace_archived_all_form_conflict_and_confirmations_render_positive_states() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let mut archived_issue = issue(project_id, issue_id);
        archived_issue.status = IssueStatus::Closed;
        archived_issue.closed_at = Some(chrono::Utc::now());
        archived_issue.archived_at = Some(chrono::Utc::now());
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.local.filters.archive = rsi_common::types::IssueArchiveFilterV1::All;
        state.local.selected_issue_id = Some(issue_id);
        state
            .data_state_mut(IssueWorkspaceDataKind::LocalPage)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue: archived_issue.clone(),
                readiness: IssueWorkspaceReadinessV1::NotEligible,
                open_blocker_count: 0,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        let archived = render_text(&app, &state, 120);
        assert!(archived.contains("Local [Fresh · Custom · All]"));
        assert!(archived.contains("Closed ARCHIVED"));
        assert!(archived.contains("Recurring jobs skip cadence"));

        state.transient.editor = Some(crate::types::IssueEditorState {
            mode: crate::types::IssueEditorMode::Edit {
                issue_id,
                row_version: 7,
            },
            title: "Draft keeps the visible identity".to_string(),
            body: "Draft body survives stale conflict".to_string(),
            priority: Some(1),
            assignee: Some("jake".to_string()),
            labels: vec!["scheduler".to_string()],
            status: Some(IssueStatus::Closed),
            dependency_issue_id: None,
            dependency_direction:
                rsi_common::issue_workspace::IssueDependencyDirectionV1::BlockedBy,
            dependency_candidates: Vec::new(),
            dependency_selected_row: 0,
            dependency_text: String::new(),
            filter_draft: None,
            filter_saved_view: None,
            filter_mine_assignee: None,
            base_issue: Some(archived_issue.clone()),
            latest_issue: Some({
                let mut latest = archived_issue.clone();
                latest.row_version = 8;
                latest.title = "Latest server identity".to_string();
                latest
            }),
            stale_conflict: true,
            active_field: 0,
            dirty: true,
            discard_armed: false,
            retry_key: Uuid::new_v4().to_string(),
            error: Some(
                "STALE VERSION — Base v7, Latest v8, Draft retained; reload or rebase".to_string(),
            ),
            submitted: false,
        });
        let conflict = render_text(&app, &state, 160);
        assert!(conflict.contains("Draft keeps the visible identity"));
        assert!(conflict.contains("Base: v7"));
        assert!(conflict.contains("Latest: v8 title=Latest server identity"));
        assert!(conflict.contains("Draft: title=Draft keeps the visible identity"));
        assert!(conflict.contains("SAVE DISABLED"));

        state.transient.editor = None;
        state.transient.cancel_confirmation = Some(crate::types::IssueCancelConfirmation {
            issue_id,
            display_number: 184,
            armed_at_ms: 1,
        });
        let cancel = render_text(&app, &state, 150);
        assert!(cancel.contains("CANCEL #184 — press d again"));
        assert!(cancel.contains("Recurring jobs skip cadence"));

        state.transient.cancel_confirmation = None;
        state.transient.archive_confirmation = Some(crate::types::IssueCancelConfirmation {
            issue_id,
            display_number: 184,
            armed_at_ms: 1,
        });
        let archive = render_text(&app, &state, 160);
        assert!(archive.contains("ARCHIVE #184 — press Enter to confirm"));
        assert!(archive.contains("Recurring jobs skip cadence"));
    }

    #[test]
    fn issue_workspace_filter_and_dependency_picker_render_bounded_positive_state() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let candidate_id = Uuid::new_v4();
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.local.selected_issue_id = Some(issue_id);
        state
            .data_state_mut(IssueWorkspaceDataKind::LocalPage)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue: issue(project_id, issue_id),
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        let filters = crate::types::IssueLocalFilters {
            statuses: vec![IssueStatus::Open, IssueStatus::Closed],
            priorities: vec![1, 4],
            readiness: rsi_common::issue_workspace::IssueWorkspaceReadinessFilterV1::Ready,
            assignee: Some("jake".to_string()),
            unassigned: false,
            labels_all: vec!["daemon".to_string()],
            provenance: vec![rsi_common::issue_workspace::IssueWorkspaceProvenanceV1::Operator],
            query: Some("scheduler".to_string()),
            archive: rsi_common::types::IssueArchiveFilterV1::All,
            sort: rsi_common::issue_workspace::IssueWorkspaceSortV1::UpdatedDesc,
        };
        state.transient.editor = Some(crate::types::IssueEditorState {
            mode: IssueEditorMode::Filter,
            title: String::new(),
            body: String::new(),
            priority: None,
            assignee: None,
            labels: Vec::new(),
            status: None,
            dependency_issue_id: None,
            dependency_direction:
                rsi_common::issue_workspace::IssueDependencyDirectionV1::BlockedBy,
            dependency_candidates: Vec::new(),
            dependency_selected_row: 0,
            dependency_text: "Ready".to_string(),
            filter_draft: Some(filters),
            filter_saved_view: Some(crate::types::IssueWorkspaceSavedView::Ready),
            filter_mine_assignee: Some("jake".to_string()),
            base_issue: None,
            latest_issue: None,
            stale_conflict: false,
            active_field: 0,
            dirty: true,
            discard_armed: false,
            retry_key: String::new(),
            error: Some("Bounded custom filter state".to_string()),
            submitted: false,
        });
        let filter = render_text(&app, &state, 180);
        assert!(filter.contains("Saved view: Ready"));
        assert!(filter.contains("Statuses: Open,Closed"));
        assert!(filter.contains("Priorities: 1,4"));
        assert!(filter.contains("Readiness: Ready"));
        assert!(filter.contains("Assignee (- = unassigned): jake"));
        assert!(filter.contains("Labels: daemon"));
        assert!(filter.contains("Provenance: Operator"));
        assert!(filter.contains("Archive: All"));

        let candidate = issue(project_id, candidate_id);
        state.transient.editor = Some(crate::types::IssueEditorState {
            mode: IssueEditorMode::Dependency { issue_id },
            title: "Dependency source".to_string(),
            body: String::new(),
            priority: None,
            assignee: None,
            labels: Vec::new(),
            status: Some(IssueStatus::Open),
            dependency_issue_id: Some(candidate_id),
            dependency_direction: rsi_common::issue_workspace::IssueDependencyDirectionV1::Blocks,
            dependency_candidates: vec![IssueWorkspaceRowV1 {
                issue: candidate,
                readiness: IssueWorkspaceReadinessV1::Blocked,
                open_blocker_count: 1,
                dependent_count: 0,
            }],
            dependency_selected_row: 0,
            dependency_text: "scheduler".to_string(),
            filter_draft: None,
            filter_saved_view: None,
            filter_mine_assignee: None,
            base_issue: Some(issue(project_id, issue_id)),
            latest_issue: None,
            stale_conflict: false,
            active_field: 2,
            dirty: true,
            discard_armed: false,
            retry_key: String::new(),
            error: Some("Server authority will validate this edge".to_string()),
            submitted: false,
        });
        let dependency = render_text(&app, &state, 180);
        assert!(dependency.contains("Direction: Blocks"));
        assert!(dependency.contains("Candidate query: scheduler"));
        assert!(dependency.contains("#184 Recurring jobs skip cadence"));
        assert!(dependency.contains(&candidate_id.to_string()));
        assert!(dependency.contains("Server authority will validate this edge"));
    }

    #[test]
    fn issue_workspace_inspector_renders_uuid_bound_dependencies_events_and_session_selection() {
        let mut app = with_session_list(1);
        let session_id = app.selected_session_id().unwrap();
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let related_id = Uuid::new_v4();
        app.sessions
            .get_mut(&session_id)
            .unwrap()
            .session
            .issue_tracker_id = Some(issue_id.to_string());
        let selected = issue(project_id, issue_id);
        let related = {
            let mut value = issue(project_id, related_id);
            value.display_number = 185;
            value.title = "Matching blocker projection".to_string();
            value
        };
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.local.selected_issue_id = Some(issue_id);
        state.local.selected_associated_session_id = Some(session_id);
        state.local.inspector_open = true;
        state.transient.selected_issue_target = Some(issue_id);
        state.transient.selected_issue = Some(selected.clone());
        state.transient.blocked_by_target = Some(issue_id);
        state.transient.blocked_by = Some(rsi_common::issue_workspace::IssueDependencyPageV1 {
            items: vec![rsi_common::issue_workspace::IssueDependencyItemV1 {
                dependency: rsi_common::types::IssueDep {
                    project_id,
                    issue_id,
                    depends_on_id: related_id,
                    created_at: chrono::Utc::now(),
                },
                related_issue: related,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        state.transient.blocks_target = Some(issue_id);
        state.transient.blocks = Some(rsi_common::issue_workspace::IssueDependencyPageV1 {
            items: Vec::new(),
            previous_cursor: None,
            next_cursor: None,
        });
        state.transient.events_target = Some(issue_id);
        state.transient.events = vec![rsi_common::types::IssueEventV1 {
            id: Uuid::new_v4(),
            project_id,
            issue_id,
            sequence: 17,
            operation: rsi_common::types::IssueEventOperationV1::StatusUpdated,
            actor_kind: rsi_common::types::IssueActorKindV1::Operator,
            actor_session_id: None,
            owning_epic_id: None,
            actor_label: Some("inspector-event-fixture".to_string()),
            expected_row_version: 6,
            resulting_row_version: 7,
            idempotency_key: None,
            request_fingerprint: "e".repeat(64),
            occurred_at: chrono::Utc::now(),
            request: rsi_common::types::IssueSemanticRequestV1::new(
                rsi_common::types::IssueSemanticOperationV1::StatusUpdated {
                    issue_id,
                    expected_row_version: 6,
                    status: IssueStatus::InProgress,
                },
            ),
            issue: selected,
        }];
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue: issue(project_id, issue_id),
                readiness: IssueWorkspaceReadinessV1::Blocked,
                open_blocker_count: 1,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        state
            .data_state_mut(IssueWorkspaceDataKind::LocalPage)
            .load_state = IssueWorkspaceLoadState::Fresh;
        let text = render_text(&app, &state, 190);
        assert!(text.contains("Blocked by [prev=false next=false]: #185 Matching blocker"));
        assert!(text.contains("17 StatusUpdated Operator"));
        assert!(text.contains("Associated sessions: >"));
        assert!(text.contains(&session_id.to_string()));
        assert!(text.contains(&issue_id.to_string()));
    }

    #[test]
    fn issue_workspace_inspector_cursor_availability_follows_selected_target_identity() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let origin_id = Uuid::new_v4();
        let replacement_id = Uuid::new_v4();
        let replacement = issue(project_id, replacement_id);
        let origin_cursor = rsi_common::issue_workspace::IssueDependencyCursorV1 {
            display_number: 64,
            issue_id: origin_id,
        };
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.local.selected_issue_id = Some(replacement_id);
        state.local.inspector_open = true;
        state.focus = crate::types::IssueWorkspaceFocus::Inspector;
        state.transient.selected_issue_target = Some(replacement_id);
        state.transient.selected_issue = Some(replacement.clone());
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue: replacement,
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        state.transient.blocked_by_target = Some(origin_id);
        state.transient.blocked_by = Some(rsi_common::issue_workspace::IssueDependencyPageV1 {
            items: Vec::new(),
            previous_cursor: None,
            next_cursor: Some(origin_cursor.clone()),
        });
        state.transient.blocks_target = Some(origin_id);
        state.transient.blocks = Some(rsi_common::issue_workspace::IssueDependencyPageV1 {
            items: Vec::new(),
            previous_cursor: Some(origin_cursor),
            next_cursor: None,
        });
        state.transient.events_target = Some(origin_id);
        state.transient.events_after_sequence = Some(64);
        state.transient.events_next_after_sequence = Some(128);
        state
            .data_state_mut(IssueWorkspaceDataKind::LocalPage)
            .load_state = IssueWorkspaceLoadState::Fresh;

        let rendered = render_text(&app, &state, 190);
        assert!(rendered.contains(&replacement_id.to_string()));
        assert!(rendered.contains("Blocked by [loading]: loading"));
        assert!(rendered.contains("Blocks [loading]: loading"));
        assert!(rendered.contains("Events [after=None next=None]:"));
    }

    #[test]
    fn issue_workspace_without_project_renders_named_recovery_state() {
        let app = with_session_list(0);
        let state = IssueWorkspaceState::new(None);
        let text = render_text(&app, &state, 90);
        assert!(text.contains("NO PROJECT SELECTED"));
        assert!(text.contains("Choose a project, then press r to refresh Issues"));
    }

    #[test]
    fn dispatched_table_and_durable_inspector_render_exact_cached_terminal_identity() {
        let mut app = with_session_list(1);
        let session_id = app.selected_session_id().unwrap();
        app.sessions.get_mut(&session_id).unwrap().session.title =
            Some("Cached dispatched session title".to_string());
        let project_id = Uuid::new_v4();
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.active_tab = IssueWorkspaceTab::Dispatched;
        state.focus = crate::types::IssueWorkspaceFocus::Inspector;
        state.dispatched.inspector_open = true;
        state.dispatched.selected_session_id = Some(session_id);
        state.dispatched.selected_issue_id = Some("tracker-internal-900".to_string());
        state
            .data_state_mut(IssueWorkspaceDataKind::Dispatched)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.dispatched = vec![rsi_common::issue_workspace::IssueDispatchRecordV1 {
            issue_id: "tracker-internal-900".to_string(),
            issue_identifier: "LOCAL-900".to_string(),
            tracker: "linear".to_string(),
            session_id,
            dispatched_at: "2026-09-10T12:00:00Z".parse().unwrap(),
            last_reconciled_at: Some("2026-09-10T12:02:00Z".parse().unwrap()),
            terminal_state: Some("Completed".to_string()),
        }];

        let rendered = render_text_mut(&app, &mut state, 220, 24);
        let id_column = rendered.find("ID").unwrap();
        let tracker_column = rendered[id_column..].find("TRACKER").unwrap() + id_column;
        let session_column = rendered[tracker_column..].find("SESSION").unwrap() + tracker_column;
        let state_column = rendered[session_column..].find("STATE").unwrap() + session_column;
        let dispatched_column = rendered[state_column..].find("DISPATCHED").unwrap() + state_column;
        assert!(
            id_column < tracker_column
                && tracker_column < session_column
                && session_column < state_column
                && state_column < dispatched_column
        );
        assert!(rendered.contains("Internal tracker Issue ID: tracker-internal-900"));
        assert!(rendered.contains("Identifier: LOCAL-900"));
        assert!(rendered.contains(&format!("Session UUID: {session_id}")));
        assert!(rendered.contains("Session title: Cached dispatched session title"));
        assert!(rendered.contains("Session status:"));
        assert!(rendered.contains("Last reconciled: 2026-09-10T12:02:00+00:00"));
        assert!(rendered.contains("Terminal state: Completed"));

        let encoded = serde_json::to_value(&state).unwrap();
        let mut restored: IssueWorkspaceState = serde_json::from_value(encoded).unwrap();
        restored.normalize_after_restore();
        assert!(restored.dispatched.inspector_open);
        assert_eq!(restored.focus, crate::types::IssueWorkspaceFocus::Inspector);
    }

    #[test]
    fn dispatched_missing_cache_narrow_table_and_short_inspector_are_recoverable() {
        let app = with_session_list(0);
        let session_id = Uuid::new_v4();
        let mut state = IssueWorkspaceState::new(Some(Uuid::new_v4()));
        state.active_tab = IssueWorkspaceTab::Dispatched;
        state.focus = crate::types::IssueWorkspaceFocus::Inspector;
        state.dispatched.inspector_open = true;
        state.dispatched.inspector_scroll_offset = usize::MAX;
        state.dispatched.selected_session_id = Some(session_id);
        state.dispatched.selected_issue_id = Some("tracker-internal-missing".to_string());
        state
            .data_state_mut(IssueWorkspaceDataKind::Dispatched)
            .load_state = IssueWorkspaceLoadState::Fresh;
        state.transient.dispatched = vec![rsi_common::issue_workspace::IssueDispatchRecordV1 {
            issue_id: "tracker-internal-missing".to_string(),
            issue_identifier: "LOCAL-901".to_string(),
            tracker: "github".to_string(),
            session_id,
            dispatched_at: "2026-09-10T13:00:00Z".parse().unwrap(),
            last_reconciled_at: None,
            terminal_state: Some("Failed".to_string()),
        }];
        let short = render_text_mut(&app, &mut state, 120, 8);
        assert!(short.contains("Terminal state: Failed"));
        assert!(state.dispatched.inspector_scroll_offset > 0);

        state.dispatched.inspector_scroll_offset = 0;
        let recovery = render_text_mut(&app, &mut state, 150, 18);
        assert!(recovery.contains("SESSION NOT IN CURRENT CACHE"));
        assert!(recovery.contains("Enter to recover the exact session"));
        assert!(recovery.contains("tracker-internal-missing"));

        state.dispatched.inspector_open = false;
        state.focus = crate::types::IssueWorkspaceFocus::Table;
        let narrow = render_text_mut(&app, &mut state, 74, 12);
        assert!(narrow.contains("LOCAL-901"));
        assert!(narrow.contains("github"));
        assert!(narrow.contains("Failed"));
    }

    #[test]
    fn narrow_unicode_inspector_wraps_and_reaches_later_dependency_and_event_pages() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let mut selected = issue(project_id, issue_id);
        selected.title =
            "Unicode 界界界 issue title remains reachable across narrow wrapped cells".to_string();
        selected.body =
            "Long body λλ λλ λλ preserves complete wrapped content for the operator".repeat(3);
        selected.labels = vec!["label-界-with-a-long-visible-name".to_string()];
        let later = {
            let mut value = issue(project_id, Uuid::new_v4());
            value.display_number = 265;
            value.title = "Dependency identity beyond first 64".to_string();
            value
        };
        let mut state = IssueWorkspaceState::new(Some(project_id));
        state.local.inspector_open = true;
        state.focus = crate::types::IssueWorkspaceFocus::Inspector;
        state.local.selected_issue_id = Some(issue_id);
        state.transient.selected_issue_target = Some(issue_id);
        state.transient.selected_issue = Some(selected.clone());
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue: selected.clone(),
                readiness: IssueWorkspaceReadinessV1::Blocked,
                open_blocker_count: 1,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        state.transient.blocked_by_target = Some(issue_id);
        state.transient.blocked_by_cursor =
            Some(rsi_common::issue_workspace::IssueDependencyCursorV1 {
                display_number: 64,
                issue_id: Uuid::new_v4(),
            });
        state.transient.blocked_by = Some(rsi_common::issue_workspace::IssueDependencyPageV1 {
            items: vec![rsi_common::issue_workspace::IssueDependencyItemV1 {
                dependency: rsi_common::types::IssueDep {
                    project_id,
                    issue_id,
                    depends_on_id: later.id,
                    created_at: chrono::Utc::now(),
                },
                related_issue: later,
            }],
            previous_cursor: Some(rsi_common::issue_workspace::IssueDependencyCursorV1 {
                display_number: 64,
                issue_id: Uuid::new_v4(),
            }),
            next_cursor: None,
        });
        state.transient.events_target = Some(issue_id);
        state.transient.events_after_sequence = Some(64);
        state.transient.events = vec![rsi_common::types::IssueEventV1 {
            id: Uuid::new_v4(),
            project_id,
            issue_id,
            sequence: 65,
            operation: rsi_common::types::IssueEventOperationV1::ContentUpdated,
            actor_kind: rsi_common::types::IssueActorKindV1::Operator,
            actor_session_id: None,
            owning_epic_id: None,
            actor_label: Some("later-page-actor".to_string()),
            expected_row_version: 64,
            resulting_row_version: 65,
            idempotency_key: None,
            request_fingerprint: "a".repeat(64),
            occurred_at: chrono::Utc::now(),
            request: rsi_common::types::IssueSemanticRequestV1::new(
                rsi_common::types::IssueSemanticOperationV1::ContentUpdated {
                    issue_id,
                    expected_row_version: 64,
                    patch: Default::default(),
                },
            ),
            issue: selected,
        }];
        state.local.inspector_scroll_offset = usize::MAX;
        let later_page = render_text_mut(&app, &mut state, 86, 10);
        assert!(later_page.contains("65 ContentUpdated Operator"));
        assert!(state.local.inspector_scroll_offset > 0);
        state.local.inspector_scroll_offset = 0;
        let wrapped = render_text_mut(&app, &mut state, 86, 16);
        assert!(wrapped.contains("Unicode"));
        assert!(wrapped.contains('界'));
        state.local.inspector_scroll_offset = 0;
        let dependency = render_text_mut(&app, &mut state, 86, 60);
        assert!(dependency.contains("#265 Dependency identity"));
    }

    #[test]
    fn normal_local_view_renders_actionable_immutable_retry_state() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let mut state = IssueWorkspaceState::new(Some(project_id));
        let selected = issue(project_id, Uuid::new_v4());
        state.local.selected_issue_id = Some(selected.id);
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue: selected,
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        state.transient.mutation_retry_error =
            Some("ISSUE WRITE OUTCOME UNKNOWN — transport uncertain".to_string());
        let rendered = render_text_mut(&app, &mut state, 180, 12);
        assert!(rendered.contains("WRITE UNKNOWN — Ctrl-Enter retries identical request"));
        assert!(rendered.contains("Recurring jobs skip cadence"));
    }

    #[test]
    fn normal_local_view_renders_typed_definitive_write_failures_and_issue_identity() {
        let app = with_session_list(0);
        let project_id = Uuid::new_v4();
        let mut state = IssueWorkspaceState::new(Some(project_id));
        let selected = issue(project_id, Uuid::new_v4());
        state.local.selected_issue_id = Some(selected.id);
        state.transient.current_page = Some(IssueWorkspacePageV1 {
            rows: vec![IssueWorkspaceRowV1 {
                issue: selected,
                readiness: IssueWorkspaceReadinessV1::Ready,
                open_blocker_count: 0,
                dependent_count: 0,
            }],
            previous_cursor: None,
            next_cursor: None,
        });
        state
            .data_state_mut(IssueWorkspaceDataKind::LocalPage)
            .load_state = IssueWorkspaceLoadState::Fresh;
        for code in [
            "InvalidTransition",
            "NotTerminal",
            "NotArchived",
            "StaleVersion",
        ] {
            state.data_state_mut(IssueWorkspaceDataKind::Mutation).error = Some(format!(
                "{code}: typed definitive failure — refresh the Issue"
            ));
            let rendered = render_text_mut(&app, &mut state, 240, 12);
            assert!(rendered.contains("WRITE FAILED"));
            assert!(rendered.contains(code));
            assert!(rendered.contains("Recurring jobs skip cadence"));
        }
    }
}
