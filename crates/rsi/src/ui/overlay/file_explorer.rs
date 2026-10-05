//! File explorer drawer rendering.

use crate::overlay::file_explorer::fs_ops::name_of;
use crate::overlay::file_explorer::state::{ExplorerFinder, ExplorerPrompt, PromptKind};
use crate::overlay::file_explorer::{FileExplorerState, ViewerMarks, drawer_width};
use crate::types::FileExplorerEntry;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate `text` to `width` display columns, ending with `…` when cut.
fn truncate_end(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Bottom-border flags: what non-default display modes are on.
fn flags_label(state: &FileExplorerState) -> String {
    let mut flags = Vec::new();
    if state.show_hidden {
        flags.push("hidden");
    }
    if state.follow_preview {
        flags.push("follow");
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!(" {} ", flags.join(" · "))
    }
}

/// Render the file explorer drawer — a left-anchored panel showing the
/// project tree. Records the drawer and row rectangles on `state` for mouse
/// hit testing and keeps the selection scrolled into view.
pub(super) fn render_file_explorer(
    frame: &mut Frame,
    area: Rect,
    state: &mut FileExplorerState,
    marks: &ViewerMarks,
) {
    let drawer_area = Rect::new(
        area.x,
        area.y,
        drawer_width(area.width, state.width),
        area.height,
    );
    state.drawer_area = drawer_area;
    state.list_area = Rect::default();

    frame.render_widget(Clear, drawer_area);

    let focused = state.explorer_focused;
    let title = format!(" {} ", name_of(&state.root));
    let title_color = if focused {
        theme::overlay_title()
    } else {
        theme::subtext0()
    };
    let mut block = theme::overlay_block()
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let flags = flags_label(state);
    if !flags.is_empty() {
        block = block.title_bottom(
            Line::from(Span::styled(flags, Style::default().fg(theme::subtext0()))).right_aligned(),
        );
    }

    let inner = block.inner(drawer_area);
    frame.render_widget(block, drawer_area);

    if inner.height < 2 || inner.width == 0 {
        return;
    }

    if state.finder.active {
        render_finder(frame, inner, &state.finder);
        return;
    }

    let footer_rows: u16 = if state.prompt.is_some() { 2 } else { 1 };
    let footer_height = footer_rows.min(inner.height - 1);
    let list_area = Rect::new(inner.x, inner.y, inner.width, inner.height - footer_height);
    let footer_area = Rect::new(
        inner.x,
        list_area.y + list_area.height,
        inner.width,
        footer_height,
    );
    state.list_area = list_area;

    if state.entries.is_empty() {
        let empty = Paragraph::new(truncate_end(
            "Empty directory — a: new file",
            usize::from(list_area.width),
        ))
        .style(Style::default().fg(theme::subtext0()));
        frame.render_widget(
            empty,
            Rect::new(list_area.x, list_area.y, list_area.width, 1),
        );
    } else {
        state.ensure_visible(usize::from(list_area.height));
        let selected_bg = if focused {
            theme::surface2()
        } else {
            theme::surface1()
        };
        for (i, entry) in state
            .entries
            .iter()
            .enumerate()
            .skip(state.scroll_offset)
            .take(usize::from(list_area.height))
        {
            let row = Rect::new(
                list_area.x,
                list_area.y + (i - state.scroll_offset) as u16,
                list_area.width,
                1,
            );
            let row_style = if i == state.selected_index {
                Style::default().bg(selected_bg)
            } else {
                Style::default()
            };
            frame.render_widget(
                Paragraph::new(entry_line(entry, marks, list_area.width)).style(row_style),
                row,
            );
        }
    }

    match &state.prompt {
        Some(prompt) => render_prompt(frame, footer_area, state, prompt),
        None => render_hint(frame, footer_area, focused),
    }
}

/// One tree row: indent, expand icon, name, and the unsaved-draft marker.
fn entry_line(entry: &FileExplorerEntry, marks: &ViewerMarks, width: u16) -> Line<'static> {
    let indent = "  ".repeat(entry.depth());
    let (icon, name, color) = match entry {
        FileExplorerEntry::Directory { path, expanded, .. } => (
            if *expanded { "▾ " } else { "▸ " },
            format!("{}/", name_of(path)),
            theme::overlay_title(),
        ),
        FileExplorerEntry::File { path, .. } => ("  ", name_of(path), theme::text()),
    };
    let path = entry.path();
    let dirty = if entry.is_dir() {
        marks.dirty.iter().any(|dirty| dirty.starts_with(path))
    } else {
        marks.dirty.contains(path)
    };
    let active = marks.active.as_deref() == Some(path);

    let mut name_style = Style::default().fg(if entry.is_ignored() {
        theme::overlay0()
    } else {
        color
    });
    if active {
        name_style = name_style.fg(theme::green()).add_modifier(Modifier::BOLD);
    }
    let marker = if dirty { " ●" } else { "" };
    let available =
        usize::from(width).saturating_sub(indent.width() + icon.width() + marker.width());

    Line::from(vec![
        Span::raw(indent),
        Span::styled(icon, Style::default().fg(theme::subtext0())),
        Span::styled(truncate_end(&name, available), name_style),
        Span::styled(marker, Style::default().fg(theme::yellow())),
    ])
}

fn render_hint(frame: &mut Frame, area: Rect, focused: bool) {
    let text = if focused {
        "a:add r:rename d:del /:find ?:help"
    } else {
        "^h:tree Space e:close"
    };
    let hint = Span::styled(
        truncate_end(text, usize::from(area.width)),
        Style::default().fg(theme::overlay_hint()),
    );
    frame.render_widget(
        Paragraph::new(Line::from(hint)),
        Rect::new(
            area.x,
            area.y + area.height.saturating_sub(1),
            area.width,
            1,
        ),
    );
}

/// Label and accent color for a prompt's first row.
fn prompt_label(state: &FileExplorerState, prompt: &ExplorerPrompt) -> (String, Color) {
    let dir_label = |dir: &std::path::Path| {
        let rel = state.display_relative(dir);
        if rel == "." {
            "./".to_string()
        } else {
            format!("{rel}/")
        }
    };
    match &prompt.kind {
        PromptKind::Add { dir } => (
            format!("New file in {} (end with / for dir)", dir_label(dir)),
            theme::blue(),
        ),
        PromptKind::AddDirectory { dir } => (
            format!("New directory in {}", dir_label(dir)),
            theme::blue(),
        ),
        PromptKind::Rename { target } => (format!("Rename {}", name_of(target)), theme::blue()),
        PromptKind::Move { target } => (format!("Move {} to", name_of(target)), theme::blue()),
        PromptKind::Copy { source } => (format!("Copy {} to", name_of(source)), theme::blue()),
        PromptKind::ConfirmDelete { target } => (
            format!(
                "Delete {}{}?",
                name_of(target),
                if target.is_dir() {
                    "/ and its contents"
                } else {
                    ""
                }
            ),
            theme::red(),
        ),
    }
}

/// Two-row prompt: label, then the input with a block cursor (or the
/// confirmation keys).
fn render_prompt(
    frame: &mut Frame,
    area: Rect,
    state: &FileExplorerState,
    prompt: &ExplorerPrompt,
) {
    let width = usize::from(area.width);
    let (label, color) = prompt_label(state, prompt);
    let label_row = Rect::new(area.x, area.y, area.width, 1);
    let input_row = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(1),
        area.width,
        1,
    );
    if area.height >= 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_end(&label, width),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )),
            label_row,
        );
    }

    if prompt.is_confirmation() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_end("y: delete (u undoes)  n: cancel", width),
                Style::default().fg(theme::overlay_hint()),
            )),
            input_row,
        );
        return;
    }

    // Horizontal scroll keeps the cursor visible in narrow drawers.
    let prefix = "› ";
    let field_width = width.saturating_sub(prefix.width()).max(1);
    let chars: Vec<char> = prompt.input.chars().collect();
    let cursor = prompt.cursor.min(chars.len());
    let start = (cursor + 1).saturating_sub(field_width);
    let visible_end = (start + field_width).min(chars.len());
    let before: String = chars[start..cursor].iter().collect();
    let at_cursor = chars
        .get(cursor)
        .map_or_else(|| " ".to_string(), char::to_string);
    let after: String = if cursor < visible_end {
        chars[cursor + 1..visible_end].iter().collect()
    } else {
        String::new()
    };
    let line = Line::from(vec![
        Span::styled(prefix, Style::default().fg(color)),
        Span::styled(before, Style::default().fg(theme::text())),
        Span::styled(
            at_cursor,
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::REVERSED),
        ),
        Span::styled(after, Style::default().fg(theme::text())),
    ]);
    frame.render_widget(Paragraph::new(line), input_row);
}

/// Render the fuzzy finder sub-mode: search input at top, scored results
/// below, hint bar.
fn render_finder(frame: &mut Frame, inner: Rect, finder: &ExplorerFinder) {
    if inner.height < 3 {
        return;
    }

    let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let input_line = Line::from(vec![
        Span::styled("/ ", Style::default().fg(theme::blue())),
        Span::raw(finder.query.clone()),
        Span::styled("█", Style::default().fg(theme::blue())),
    ]);
    frame.render_widget(Paragraph::new(input_line), input_area);

    let list_height = usize::from(inner.height.saturating_sub(2)); // -1 input, -1 hint
    let list_y = inner.y + 1;

    if finder.results.is_empty() {
        let msg = if finder.query.is_empty() {
            "Type to search files..."
        } else {
            "No matches"
        };
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(theme::subtext0())),
            Rect::new(inner.x, list_y, inner.width, 1),
        );
    } else {
        let scroll = (finder.selected + 1).saturating_sub(list_height);
        for (vi, &cache_idx) in finder
            .results
            .iter()
            .enumerate()
            .skip(scroll)
            .take(list_height)
        {
            let row_area = Rect::new(inner.x, list_y + (vi - scroll) as u16, inner.width, 1);
            let path_str = finder
                .cache
                .get(cache_idx)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let row_style = if vi == finder.selected {
                Style::default().bg(theme::surface2())
            } else {
                Style::default()
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate_end(&path_str, usize::from(inner.width)),
                    Style::default().fg(theme::text()),
                ))
                .style(row_style),
                row_area,
            );
        }
    }

    let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    let hint = Line::from(vec![
        Span::styled(
            "↑↓:nav Enter:open Esc:back ",
            Style::default().fg(theme::overlay_hint()),
        ),
        Span::styled(
            format!("{}/{}", finder.results.len(), finder.cache.len()),
            Style::default().fg(theme::subtext0()),
        ),
    ]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn file(path: &str, depth: usize) -> FileExplorerEntry {
        FileExplorerEntry::File {
            path: PathBuf::from(path),
            depth,
            ignored: false,
        }
    }

    fn dir(path: &str, depth: usize, expanded: bool) -> FileExplorerEntry {
        FileExplorerEntry::Directory {
            path: PathBuf::from(path),
            depth,
            expanded,
            ignored: false,
        }
    }

    fn state_with(entries: Vec<FileExplorerEntry>) -> FileExplorerState {
        FileExplorerState::with_entries(
            PathBuf::from("/tmp/project"),
            entries,
            std::env::temp_dir(),
        )
    }

    fn render(
        state: &mut FileExplorerState,
        marks: &ViewerMarks,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        theme::with_theme_state(|| {
            let mut terminal = Terminal::new(TestBackend::new(width, height))
                .expect("test terminal should initialize");
            terminal
                .draw(|frame| render_file_explorer(frame, frame.area(), state, marks))
                .expect("file explorer should render");
            let buffer = terminal.backend().buffer().clone();
            (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect()
        })
    }

    #[test]
    fn drawer_keeps_usable_width_on_compact_wide_terminals() {
        // A 160-column terminal has only a 14-column transcript gutter when
        // the transcript is capped at 129 columns. The drawer must retain its
        // documented usable maximum instead of inheriting that gutter width.
        assert_eq!(drawer_width(160, None), 50);
    }

    #[test]
    fn drawer_width_stays_within_documented_bounds() {
        assert_eq!(drawer_width(80, None), 32);
        assert_eq!(drawer_width(120, None), 48);
        assert_eq!(drawer_width(200, None), 50);
    }

    #[test]
    fn chosen_drawer_width_is_clamped_to_leave_room_for_the_viewer() {
        assert_eq!(drawer_width(120, Some(70)), 70);
        assert_eq!(drawer_width(120, Some(5)), 20);
        assert_eq!(drawer_width(120, Some(200)), 100);
        // Tiny terminals never exceed their own width.
        assert_eq!(drawer_width(15, Some(40)), 15);
    }

    #[test]
    fn drawer_border_matches_input_bar() {
        theme::with_theme_state(|| {
            let mut terminal =
                Terminal::new(TestBackend::new(80, 12)).expect("test terminal should initialize");
            let mut state = state_with(Vec::new());
            terminal
                .draw(|frame| {
                    render_file_explorer(frame, frame.area(), &mut state, &ViewerMarks::default());
                })
                .expect("file explorer should render");

            assert_eq!(
                terminal.backend().buffer()[(0, 1)].fg,
                theme::input_bar_border(true),
                "drawer boundary must match focused input bar"
            );
        });
    }

    #[test]
    fn renders_tree_rows_and_records_hit_areas() {
        let mut state = state_with(vec![
            dir("/tmp/project/src", 0, true),
            file("/tmp/project/src/main.rs", 1),
            file("/tmp/project/README.md", 0),
        ]);
        let rows = render(&mut state, &ViewerMarks::default(), 80, 10);

        assert!(rows[0].contains("project"), "title row: {:?}", rows[0]);
        assert!(rows[1].contains("▾ src/"), "dir row: {:?}", rows[1]);
        assert!(rows[2].contains("  main.rs"), "child row: {:?}", rows[2]);
        assert!(rows[3].contains("README.md"), "file row: {:?}", rows[3]);
        assert!(
            rows.iter().any(|row| row.contains("a:add")),
            "hint row advertises add: {rows:?}"
        );
        assert_eq!(state.drawer_area, Rect::new(0, 0, 32, 10));
        assert_eq!(state.list_area.y, 1);
        assert_eq!(state.list_area.height, 7);
    }

    #[test]
    fn honors_operator_width() {
        let mut state = state_with(vec![file("/tmp/project/a.rs", 0)]);
        state.width = Some(60);
        render(&mut state, &ViewerMarks::default(), 120, 8);
        assert_eq!(state.drawer_area.width, 60);
    }

    #[test]
    fn marks_unsaved_drafts_on_files_and_their_directories() {
        let mut state = state_with(vec![
            dir("/tmp/project/src", 0, true),
            file("/tmp/project/src/lib.rs", 1),
        ]);
        let marks = ViewerMarks {
            active: Some(PathBuf::from("/tmp/project/src/lib.rs")),
            dirty: [PathBuf::from("/tmp/project/src/lib.rs")]
                .into_iter()
                .collect(),
        };
        let rows = render(&mut state, &marks, 80, 8);
        assert!(rows[1].contains("src/ ●"), "dir marker: {:?}", rows[1]);
        assert!(rows[2].contains("lib.rs ●"), "file marker: {:?}", rows[2]);
    }

    #[test]
    fn renders_add_prompt_with_target_directory() {
        let mut state = state_with(vec![dir("/tmp/project/src", 0, false)]);
        let mut prompt = ExplorerPrompt::new(
            PromptKind::Add {
                dir: PathBuf::from("/tmp/project/src"),
            },
            "",
        );
        prompt.insert_str("new.rs");
        state.prompt = Some(prompt);
        let rows = render(&mut state, &ViewerMarks::default(), 100, 10);
        assert!(
            rows.iter().any(|row| row.contains("New file in src/")),
            "prompt label: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("› new.rs")),
            "prompt input: {rows:?}"
        );
    }

    #[test]
    fn renders_delete_confirmation() {
        let mut state = state_with(vec![file("/tmp/project/old.txt", 0)]);
        state.prompt = Some(ExplorerPrompt::new(
            PromptKind::ConfirmDelete {
                target: PathBuf::from("/tmp/project/old.txt"),
            },
            "",
        ));
        let rows = render(&mut state, &ViewerMarks::default(), 100, 10);
        assert!(
            rows.iter().any(|row| row.contains("Delete old.txt?")),
            "confirmation label: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("y: delete")),
            "confirmation keys: {rows:?}"
        );
    }

    #[test]
    fn shows_display_flags_on_bottom_border() {
        let mut state = state_with(vec![file("/tmp/project/a.rs", 0)]);
        state.show_hidden = true;
        state.follow_preview = true;
        let rows = render(&mut state, &ViewerMarks::default(), 100, 8);
        assert!(
            rows[7].contains("hidden · follow"),
            "bottom border flags: {:?}",
            rows[7]
        );
    }

    #[test]
    fn keeps_selection_visible_when_scrolling() {
        let entries = (0..40)
            .map(|i| file(&format!("/tmp/project/f{i:02}.rs"), 0))
            .collect();
        let mut state = state_with(entries);
        state.selected_index = 30;
        let rows = render(&mut state, &ViewerMarks::default(), 80, 12);
        assert!(state.scroll_offset > 0);
        assert!(
            rows.iter().any(|row| row.contains("f30.rs")),
            "selected row visible: {rows:?}"
        );
        // Moving up one row keeps the viewport still (no snap-to-bottom).
        let offset = state.scroll_offset;
        state.selected_index = 29;
        render(&mut state, &ViewerMarks::default(), 80, 12);
        assert_eq!(state.scroll_offset, offset);
    }

    #[test]
    fn truncates_long_names_with_ellipsis() {
        assert_eq!(truncate_end("abcdef", 4), "abc…");
        assert_eq!(truncate_end("abc", 4), "abc");
        assert_eq!(truncate_end("abc", 0), "");
    }
}
