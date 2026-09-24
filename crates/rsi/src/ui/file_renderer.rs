//! Custom renderer for the file viewer/editor.
//!
//! Reads buffer content from tui_textarea, runs tree-sitter highlighting,
//! and renders line-by-line with: relative line numbers, gutter, syntax colors,
//! cursor line highlight, trailing whitespace visualization, search highlights,
//! code folding, bracket matching, and viewport-aware scrolling.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use super::treesitter;
use crate::file_viewer;
use crate::types::{FileViewerMouseRow, FileViewerState, GitLineState, PopupMode};
use crate::vim_textarea;

/// Compute gutter width sufficient for both absolute and relative line numbers.
///
/// Absolute numbers can be up to `total_lines` digits. Relative numbers are at
/// most `viewport_height - 1`. We take the max and floor at 3 for readability,
/// then add 1 for fold marker, 1 for git sign, 1 for separator space.
fn gutter_width(total_lines: usize, viewport_height: usize, has_git: bool) -> u16 {
    let abs_digits = total_lines.max(1).ilog10() as usize + 1;
    let rel_digits = viewport_height.saturating_sub(1).max(1).ilog10() as usize + 1;
    let digits = abs_digits.max(rel_digits).max(3);
    let git_col = if has_git { 1 } else { 0 };
    (digits + 2 + git_col) as u16 // +1 for fold marker, +1 for separator space, +git_col
}

/// Render one gutter cell: absolute number on cursor line, relative distance elsewhere.
fn render_gutter_cell(
    line_idx: usize,
    cursor_row: usize,
    gutter_w: u16,
    _is_focused: bool,
    fold_char: char,
) -> (String, Style) {
    let num_width = gutter_w as usize - 2; // reserve 1 for fold marker, 1 for trailing space
    if line_idx == cursor_row {
        // Current line: absolute 1-indexed number, bright
        let s = format!("{}{:>width$} ", fold_char, line_idx + 1, width = num_width);
        let style = Style::default()
            .fg(theme::header_fg())
            .add_modifier(Modifier::BOLD);
        (s, style)
    } else {
        // Other lines: unsigned distance from cursor, dim
        let dist = (line_idx as isize - cursor_row as isize).unsigned_abs();
        let s = format!("{}{:>width$} ", fold_char, dist, width = num_width);
        let style = Style::default().fg(theme::overlay0());
        (s, style)
    }
}

/// Split a line into (content_part, trailing_ws_part).
fn trailing_ws_split(line: &str) -> (&str, &str) {
    let last_nonws = line
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    (&line[..last_nonws], &line[last_nonws..])
}

/// Render trailing whitespace characters as visible dots.
fn trailing_ws_spans(ws: &str) -> Vec<Span<'static>> {
    if ws.is_empty() {
        return vec![];
    }
    let rendered: String = ws
        .chars()
        .map(|c| match c {
            ' ' => '\u{00B7}',  // middle dot
            '\t' => '\u{00B7}', // tabs become a single dot
            _ => c,
        })
        .collect();
    let style = Style::default()
        .fg(theme::status_failed()) // dim red
        .add_modifier(Modifier::DIM);
    vec![Span::styled(rendered, style)]
}

fn visible_insert_trailing_ws(
    content_part: &str,
    trailing_ws: &str,
    cursor_col: usize,
    is_cursor_line: bool,
    focused: bool,
    mode: PopupMode,
) -> String {
    if !focused || !is_cursor_line || mode != PopupMode::Insert || trailing_ws.is_empty() {
        return String::new();
    }

    let content_chars = content_part.chars().count();
    if cursor_col < content_chars {
        return String::new();
    }

    let trailing_chars = trailing_ws.chars().count();
    let reveal_chars = cursor_col
        .saturating_sub(content_chars)
        .saturating_add(1)
        .min(trailing_chars);
    trailing_ws.chars().take(reveal_chars).collect()
}

#[derive(Debug)]
struct SourceViewportLine {
    line_num: usize,
    visual_rows: usize,
    cursor_visual_line: Option<usize>,
}

fn rendered_source_line_for_wrap(
    viewer: &FileViewerState,
    line_num: usize,
    cursor_col: usize,
    focused: bool,
) -> String {
    let raw_line = &viewer.surface.textarea.lines()[line_num];
    let (content_part, trailing_ws) = trailing_ws_split(raw_line);
    let mut rendered = String::from(content_part);

    if let Some(hidden_count) = file_viewer::fold_summary_at(&viewer.folds, line_num) {
        rendered.push_str(&format!(
            "  \u{00B7}\u{00B7}\u{00B7}(+{hidden_count} lines)"
        ));
    }

    if viewer.surface.mode == PopupMode::Normal {
        rendered.extend(trailing_ws.chars().map(|c| match c {
            ' ' | '\t' => '\u{00B7}',
            _ => c,
        }));
    } else {
        let visible_ws = visible_insert_trailing_ws(
            content_part,
            trailing_ws,
            cursor_col,
            line_num == viewer.surface.textarea.cursor().0,
            focused,
            viewer.surface.mode,
        );
        rendered.extend(visible_ws.chars().map(|c| match c {
            ' ' | '\t' => '\u{00B7}',
            _ => c,
        }));
    }

    rendered
}

fn wrap_text_ranges(text: &str, max_width: usize) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    if max_width == 0 || chars.len() <= max_width {
        return vec![(0, chars.len())];
    }

    let mut ranges = Vec::new();
    let mut pos = 0;
    while pos < chars.len() {
        let remaining = chars.len() - pos;
        let chunk_end = if remaining <= max_width {
            chars.len()
        } else {
            let search_end = pos + max_width;
            chars[pos..search_end]
                .iter()
                .rposition(|c| *c == ' ')
                .map(|p| pos + p + 1)
                .unwrap_or(search_end)
        };

        ranges.push((pos, chunk_end));
        pos = chunk_end;
    }

    if ranges.is_empty() {
        ranges.push((0, 0));
    }

    ranges
}

fn cursor_visual_line(ranges: &[(usize, usize)], cursor_col: usize) -> usize {
    for (i, &(start, end)) in ranges.iter().enumerate() {
        if cursor_col >= start && cursor_col < end {
            return i;
        }
        if cursor_col == end && i + 1 == ranges.len() {
            return i;
        }
    }

    ranges.len().saturating_sub(1)
}

fn build_source_viewport_lines(
    viewer: &FileViewerState,
    visible_lines: &[usize],
    content_width: usize,
    cursor_row: usize,
    cursor_col: usize,
    focused: bool,
) -> Vec<SourceViewportLine> {
    visible_lines
        .iter()
        .map(|&line_num| {
            let rendered = rendered_source_line_for_wrap(viewer, line_num, cursor_col, focused);
            let ranges = wrap_text_ranges(&rendered, content_width);
            let visual_rows = ranges.len().max(1);
            let cursor_visual_line =
                (line_num == cursor_row).then(|| cursor_visual_line(&ranges, cursor_col));

            SourceViewportLine {
                line_num,
                visual_rows,
                cursor_visual_line,
            }
        })
        .collect()
}

fn source_view_start(viewport_lines: &[SourceViewportLine], viewport_top: usize) -> (usize, usize) {
    let mut rows_before = 0;

    for (idx, line) in viewport_lines.iter().enumerate() {
        let next = rows_before + line.visual_rows;
        if viewport_top < next {
            return (idx, viewport_top - rows_before);
        }
        rows_before = next;
    }

    (viewport_lines.len(), 0)
}

/// Apply cursor line background tint to all spans on a line.
fn apply_cursor_line_bg_to_spans(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let bg = theme::file_viewer_cursor_line_bg();
    spans
        .into_iter()
        .map(|s| {
            let mut style = s.style;
            style.bg = Some(bg);
            Span::styled(s.content, style)
        })
        .collect()
}

/// Build a map of search highlights per line.
/// Returns: line_idx -> Vec<(char_start, char_end, is_current_match)>
fn build_search_highlight_map(
    viewer: &FileViewerState,
) -> HashMap<usize, Vec<(usize, usize, bool)>> {
    let mut map: HashMap<usize, Vec<(usize, usize, bool)>> = HashMap::new();
    let lines = viewer.surface.textarea.lines();
    for (i, &(row, byte_start, byte_end)) in viewer.search.matches.iter().enumerate() {
        let is_current = i == viewer.search.current_match;
        if let Some(line) = lines.get(row) {
            let char_start = line[..byte_start.min(line.len())].chars().count();
            let char_end = char_start
                + line[byte_start.min(line.len())..byte_end.min(line.len())]
                    .chars()
                    .count();
            map.entry(row)
                .or_default()
                .push((char_start, char_end, is_current));
        }
    }
    map
}

/// Compute bracket pair for highlighting (if cursor is on a bracket).
fn compute_bracket_highlight(viewer: &FileViewerState) -> Option<((usize, usize), (usize, usize))> {
    let (cur_row, cur_col) = viewer.surface.textarea.cursor();
    let lines: Vec<String> = viewer
        .surface
        .textarea
        .lines()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let cur_char = lines.get(cur_row).and_then(|l| l.chars().nth(cur_col));
    if cur_char.map(vim_textarea::is_bracket).unwrap_or(false) {
        vim_textarea::find_matching_bracket_pos(&lines, cur_row, cur_col)
            .map(|matched| ((cur_row, cur_col), matched))
    } else {
        None
    }
}

/// Apply search highlights and bracket highlights to a vec of spans.
/// `search_hits` is a list of (char_start, char_end, is_current_match).
/// `bracket_positions` is a list of char positions to highlight as brackets.
fn apply_highlights(
    spans: Vec<Span<'static>>,
    search_hits: &[(usize, usize, bool)],
    bracket_positions: &[usize],
) -> Vec<Span<'static>> {
    if search_hits.is_empty() && bracket_positions.is_empty() {
        return spans;
    }

    // Flatten spans into (char, style) pairs for easier manipulation
    let mut chars_styles: Vec<(char, Style)> = Vec::new();
    for span in &spans {
        for ch in span.content.chars() {
            chars_styles.push((ch, span.style));
        }
    }

    if chars_styles.is_empty() {
        return spans;
    }

    // Apply search highlights
    for &(start, end, is_current) in search_hits {
        let bg = if is_current {
            theme::search_current_bg()
        } else {
            theme::search_match_bg()
        };
        for i in start..end.min(chars_styles.len()) {
            chars_styles[i].1 = chars_styles[i].1.bg(bg);
            if is_current {
                chars_styles[i].1 = chars_styles[i].1.fg(theme::base());
            }
        }
    }

    // Apply bracket highlights
    for &pos in bracket_positions {
        if pos < chars_styles.len() {
            chars_styles[pos].1 = chars_styles[pos].1.bg(theme::bracket_match_bg());
        }
    }

    // Rebuild spans by grouping consecutive chars with the same style
    let mut result: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    while i < chars_styles.len() {
        let style = chars_styles[i].1;
        let mut text = String::new();
        while i < chars_styles.len() && chars_styles[i].1 == style {
            text.push(chars_styles[i].0);
            i += 1;
        }
        result.push(Span::styled(text, style));
    }

    result
}

/// Render the file viewer content area (inside the border block).
///
/// Called from render_file_viewer() in session.rs after the Block is rendered
/// and inner Rect is computed. Takes ownership of nothing -- borrows
/// FileViewerState mutably to update viewport_top and the highlight cache.
pub fn render_file_viewer_content(
    frame: &mut Frame,
    inner: Rect,
    viewer: &mut FileViewerState,
    focused: bool,
) {
    // This is render-time state: clear stale hit-test data before each frame.
    viewer.mouse_layout = Default::default();

    // --- Markdown preview mode: alternate rendering path ---
    if viewer.markdown_preview && viewer.is_markdown() {
        viewer.mouse_layout.editor_area = Some(inner);
        viewer.mouse_layout.content_area = Some(inner);
        render_markdown_preview(frame, inner, viewer);
        return;
    }

    let lines = viewer.surface.textarea.lines();
    let total_lines = lines.len();
    if total_lines == 0 {
        return;
    }

    let (cursor_row, cursor_col) = viewer.surface.textarea.cursor();

    // Reserve 1 line at the bottom for search/command prompt
    let search_active = viewer.search.input_active;
    let has_pattern = viewer.search.pattern.is_some();
    let command_active = viewer.command.active;
    let has_conflict = viewer.external_conflict.is_some();
    let reserve_bottom = if search_active || has_pattern || command_active || has_conflict {
        1
    } else {
        0
    };
    let visible_height = (inner.height as usize).saturating_sub(reserve_bottom);
    viewer.viewport_height = visible_height.max(1);
    if visible_height == 0 {
        return;
    }

    // --- Viewport tracking (accounting for folds and wrapped visual rows) ---
    let visible_lines: Vec<usize> = (0..total_lines)
        .filter(|&i| !file_viewer::line_is_folded(&viewer.folds, i))
        .collect();
    let total_visible = visible_lines.len();

    // --- Layout: gutter | content ---
    let has_git = viewer.git_gutter.loaded
        && viewer
            .git_gutter
            .line_states
            .iter()
            .any(|s| *s != GitLineState::Unchanged);
    let gutter_w = gutter_width(total_lines, visible_height, has_git);
    let [_gutter_area, content_area] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(gutter_w), Constraint::Min(1)])
        .areas(inner);

    let content_width = content_area.width as usize;
    viewer.surface.wrap_width.set(content_width);

    let viewport_lines = build_source_viewport_lines(
        viewer,
        &visible_lines,
        content_width,
        cursor_row,
        cursor_col,
        focused,
    );

    let mut total_visual_rows = 0;
    let mut cursor_visual_row = 0;
    for line in &viewport_lines {
        if let Some(cursor_visual_line) = line.cursor_visual_line {
            cursor_visual_row = total_visual_rows + cursor_visual_line;
        }
        total_visual_rows += line.visual_rows;
    }
    let total_visual_rows = total_visual_rows.max(1);

    if cursor_visual_row < viewer.viewport_top {
        viewer.viewport_top = cursor_visual_row;
    } else if cursor_visual_row >= viewer.viewport_top + visible_height {
        viewer.viewport_top = cursor_visual_row.saturating_sub(visible_height - 1);
    }
    viewer.viewport_top = viewer
        .viewport_top
        .min(total_visual_rows.saturating_sub(visible_height));

    let (viewport_start, mut skip_visual_rows) =
        source_view_start(&viewport_lines, viewer.viewport_top);

    // --- Highlight cache ---
    let need_rebuild =
        viewer.cached_content_version != viewer.content_version || viewer.highlight_cache.is_none();

    if need_rebuild {
        let content = lines.join("\n");
        let ext = viewer.language_ext.as_deref().unwrap_or("");
        let highlighted = treesitter::highlight_file(&content, ext, 0..total_lines);
        viewer.highlight_cache = Some(highlighted);
        viewer.cached_content_version = viewer.content_version;
    }

    let highlighted = viewer.highlight_cache.as_ref().expect("just built");

    // --- Pre-compute search highlights and bracket pair ---
    let search_highlights = build_search_highlight_map(viewer);
    let bracket_highlight = compute_bracket_highlight(viewer);

    // Trailing whitespace is only shown in normal mode
    let show_trailing_ws = viewer.surface.mode == PopupMode::Normal;

    // --- Build gutter and content lines together (with word wrapping) ---
    let mut gutter_lines: Vec<Line<'static>> = Vec::with_capacity(visible_height);
    let mut content_lines: Vec<Line<'static>> = Vec::with_capacity(visible_height);
    let mut mouse_rows = Vec::with_capacity(visible_height);

    let mut visual_rows_used = 0;
    let mut logical_idx = viewport_start;

    while visual_rows_used < visible_height && logical_idx < total_visible {
        let line_num = viewport_lines[logical_idx].line_num;
        let is_cursor_line = line_num == cursor_row;

        // --- Fold gutter marker ---
        let fold_char = if viewer
            .folds
            .folded_ranges
            .iter()
            .any(|&(s, _)| s == line_num)
        {
            '\u{25B8}' // small right triangle (collapsed)
        } else if viewer
            .folds
            .foldable_regions
            .iter()
            .any(|&(s, _)| s == line_num)
        {
            '\u{25BE}' // small down triangle (expandable)
        } else {
            ' '
        };

        // --- Git gutter sign ---
        let git_state = viewer
            .git_gutter
            .line_states
            .get(line_num)
            .copied()
            .unwrap_or(GitLineState::Unchanged);

        // --- Gutter (built for first visual line; continuations get blank gutter) ---
        let (gutter_str, mut gutter_style) =
            render_gutter_cell(line_num, cursor_row, gutter_w, focused, fold_char);
        if is_cursor_line {
            gutter_style = gutter_style.bg(theme::file_viewer_cursor_line_bg());
        }

        let primary_gutter_line = if has_git {
            let (git_char, git_style) = match git_state {
                GitLineState::Unchanged => (" ", Style::default()),
                GitLineState::Added => ("+", Style::default().fg(theme::green())),
                GitLineState::Modified => ("~", Style::default().fg(theme::yellow())),
                GitLineState::Deleted => ("-", Style::default().fg(theme::red())),
            };
            let mut git_s = git_style;
            if is_cursor_line {
                git_s = git_s.bg(theme::file_viewer_cursor_line_bg());
            }
            Line::from(vec![
                Span::styled(git_char.to_string(), git_s),
                Span::styled(gutter_str, gutter_style),
            ])
        } else {
            Line::from(Span::styled(gutter_str, gutter_style))
        };

        // --- Content ---
        let raw_line = &lines[line_num];
        let (content_part, trailing_ws) = trailing_ws_split(raw_line);

        // Get syntax-highlighted spans for the content part (not trailing ws)
        let mut content_spans: Vec<Span<'static>> = if let Some(hl_line) = highlighted.get(line_num)
        {
            if trailing_ws.is_empty() {
                hl_line.spans.clone()
            } else {
                trim_spans_to_char_count(&hl_line.spans, content_part.chars().count())
            }
        } else {
            vec![Span::raw(content_part.to_owned())]
        };

        // Append fold indicator if this line starts a collapsed fold
        if let Some(hidden_count) = file_viewer::fold_summary_at(&viewer.folds, line_num) {
            content_spans.push(Span::styled(
                format!("  \u{00B7}\u{00B7}\u{00B7}(+{hidden_count} lines)"),
                Style::default()
                    .fg(theme::fold_indicator())
                    .add_modifier(Modifier::DIM),
            ));
        }

        // Append trailing whitespace dots if enabled
        if show_trailing_ws && !trailing_ws.is_empty() {
            content_spans.extend(trailing_ws_spans(trailing_ws));
        } else {
            let visible_ws = visible_insert_trailing_ws(
                content_part,
                trailing_ws,
                cursor_col,
                is_cursor_line,
                focused,
                viewer.surface.mode,
            );
            if !visible_ws.is_empty() {
                content_spans.extend(trailing_ws_spans(&visible_ws));
            }
        }

        // Apply search and bracket highlights
        let search_hits = search_highlights.get(&line_num);
        let mut bracket_positions: Vec<usize> = Vec::new();
        if let Some(((ar, ac), (br, bc))) = bracket_highlight {
            if line_num == ar {
                bracket_positions.push(ac);
            }
            if line_num == br {
                bracket_positions.push(bc);
            }
        }
        let empty_hits: Vec<(usize, usize, bool)> = Vec::new();
        content_spans = apply_highlights(
            content_spans,
            search_hits.unwrap_or(&empty_hits),
            &bracket_positions,
        );

        // --- Word-wrap content spans into visual lines ---
        let wrapped_lines = wrap_styled_line(content_spans, content_width);

        let mut cursor_visual_line = 0;
        let mut cursor_col_in_visual = cursor_col;
        if is_cursor_line {
            let mut cumulative_chars = 0;
            for (vi, vline_spans) in wrapped_lines.iter().enumerate() {
                let vline_char_count: usize =
                    vline_spans.iter().map(|s| s.content.chars().count()).sum();
                if cursor_col >= cumulative_chars
                    && cursor_col < cumulative_chars + vline_char_count
                {
                    cursor_visual_line = vi;
                    cursor_col_in_visual = cursor_col - cumulative_chars;
                    break;
                } else if cursor_col == cumulative_chars + vline_char_count
                    && vi + 1 == wrapped_lines.len()
                {
                    cursor_visual_line = vi;
                    cursor_col_in_visual = cursor_col - cumulative_chars;
                    break;
                }
                cumulative_chars += vline_char_count;
            }
        }

        let mut source_char_start = 0;
        for (vi, vline_spans) in wrapped_lines.into_iter().enumerate() {
            let source_char_end = source_char_start
                + vline_spans
                    .iter()
                    .map(|span| span.content.chars().count())
                    .sum::<usize>();
            if vi < skip_visual_rows {
                source_char_start = source_char_end;
                continue;
            }
            if visual_rows_used >= visible_height {
                break;
            }

            // Gutter: primary line number for first visual line, blank for continuations
            if vi == 0 {
                gutter_lines.push(primary_gutter_line.clone());
            } else {
                let cont_style = if is_cursor_line {
                    Style::default().bg(theme::file_viewer_cursor_line_bg())
                } else {
                    Style::default()
                };
                gutter_lines.push(Line::from(Span::styled(
                    " ".repeat(gutter_w as usize),
                    cont_style,
                )));
            }

            // Content: apply cursor treatment to the correct visual line
            if is_cursor_line && focused && vi == cursor_visual_line {
                let bg_spans = apply_cursor_line_bg_to_spans(vline_spans);
                let rendered =
                    apply_cursor_highlight_on_spans(bg_spans, cursor_col_in_visual, content_width);
                content_lines.push(
                    Line::default()
                        .style(Style::default().bg(theme::file_viewer_cursor_line_bg()))
                        .spans(rendered),
                );
            } else if is_cursor_line {
                let bg_spans = apply_cursor_line_bg_to_spans(vline_spans);
                content_lines.push(
                    Line::default()
                        .style(Style::default().bg(theme::file_viewer_cursor_line_bg()))
                        .spans(bg_spans),
                );
            } else {
                content_lines.push(Line::from(vline_spans));
            }

            mouse_rows.push(FileViewerMouseRow {
                line: line_num,
                char_start: source_char_start,
                char_end: source_char_end,
            });

            visual_rows_used += 1;
            source_char_start = source_char_end;
        }

        skip_visual_rows = 0;
        logical_idx += 1;
    }

    // --- Render gutter and content ---
    let content_render_area = if reserve_bottom > 0 {
        Rect {
            height: inner.height.saturating_sub(reserve_bottom as u16),
            ..inner
        }
    } else {
        inner
    };

    let [gutter_render, content_render] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(gutter_w), Constraint::Min(1)])
        .areas(content_render_area);

    viewer.mouse_layout.editor_area = Some(content_render_area);
    viewer.mouse_layout.content_area = Some(content_render);
    viewer.mouse_layout.rows = mouse_rows;

    frame.render_widget(Paragraph::new(gutter_lines), gutter_render);
    frame.render_widget(Paragraph::new(content_lines), content_render);

    // --- Render search prompt / status at bottom ---
    if reserve_bottom > 0 {
        let bottom_area = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        };

        if search_active {
            // Active search input: show /query_ or ?query_
            let prefix = if viewer.search.forward { "/" } else { "?" };
            let prompt_text = format!("{}{}\u{2588}", prefix, viewer.search.input_buffer);
            let match_count = viewer.search.matches.len();
            let status = if match_count > 0 {
                format!(" [{}/{}]", viewer.search.current_match + 1, match_count)
            } else if viewer.search.input_buffer.is_empty() {
                String::new()
            } else {
                " [0/0]".to_string()
            };
            let line = Line::from(vec![
                Span::styled(
                    prompt_text,
                    Style::default().fg(theme::text()).bg(theme::input_bar_bg()),
                ),
                Span::styled(
                    status,
                    Style::default()
                        .fg(theme::overlay1())
                        .bg(theme::input_bar_bg()),
                ),
            ]);
            frame.render_widget(Paragraph::new(vec![line]), bottom_area);
        } else if command_active {
            // Command mode prompt: `:buffer_`
            let buf = &viewer.command.buffer;
            let cursor_pos = viewer.command.cursor;
            let before = &buf[..cursor_pos];
            let cursor_char = buf[cursor_pos..]
                .chars()
                .next()
                .map(|c| c.to_string())
                .unwrap_or_else(|| " ".to_string());
            let after_offset = cursor_pos + cursor_char.trim().len().max(cursor_char.len().min(1));
            let after = if after_offset <= buf.len() {
                &buf[after_offset..]
            } else {
                ""
            };
            let spans = vec![
                Span::styled(
                    ":",
                    Style::default()
                        .fg(theme::text())
                        .bg(theme::input_bar_bg())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    before.to_string(),
                    Style::default().fg(theme::text()).bg(theme::input_bar_bg()),
                ),
                Span::styled(
                    cursor_char,
                    Style::default()
                        .fg(theme::text())
                        .bg(theme::input_bar_bg())
                        .add_modifier(Modifier::REVERSED),
                ),
                Span::styled(
                    after.to_string(),
                    Style::default().fg(theme::text()).bg(theme::input_bar_bg()),
                ),
            ];
            frame.render_widget(Paragraph::new(vec![Line::from(spans)]), bottom_area);
        } else if has_conflict {
            // Keep the conflict visible unless a live prompt needs this row.
            let line = Line::from(Span::styled(
                " \u{26A0} External change \u{2014} :e! reload | :w! overwrite",
                Style::default()
                    .fg(theme::yellow())
                    .add_modifier(Modifier::BOLD),
            ));
            frame.render_widget(Paragraph::new(vec![line]), bottom_area);
        } else if has_pattern {
            // Pattern set but input closed: show match status
            let match_count = viewer.search.matches.len();
            let status = if match_count > 0 {
                format!("[{}/{}]", viewer.search.current_match + 1, match_count)
            } else {
                "[0/0]".to_string()
            };
            let line = Line::from(Span::styled(status, Style::default().fg(theme::overlay1())));
            frame.render_widget(Paragraph::new(vec![line]), bottom_area);
        }
    }
}

/// Split styled spans into multiple visual lines for word wrapping.
///
/// Preserves span styling across line breaks. Breaks at word boundaries
/// (spaces) when possible, hard-breaks when a single word exceeds max_width.
/// Returns one inner Vec per visual line.
fn wrap_styled_line(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Vec<Span<'static>>> {
    if max_width == 0 {
        return vec![spans];
    }

    // Fast path: compute total char width and skip if it fits
    let total_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if total_width <= max_width {
        return vec![spans];
    }

    // Flatten all spans into (char, style) pairs for easy splitting
    let styled_chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();

    let mut result: Vec<Vec<Span<'static>>> = Vec::new();
    let mut pos = 0;

    while pos < styled_chars.len() {
        let remaining = styled_chars.len() - pos;
        let chunk_end = if remaining <= max_width {
            styled_chars.len()
        } else {
            // Find a space to break at, scanning backwards from max_width
            let search_end = pos + max_width;
            styled_chars[pos..search_end]
                .iter()
                .rposition(|(c, _)| *c == ' ')
                .map(|p| pos + p + 1) // break after the space
                .unwrap_or(search_end) // hard break if no space
        };

        // Build spans for this visual line, merging adjacent chars with same style
        let mut line_spans: Vec<Span<'static>> = Vec::new();
        let mut run_start = pos;
        while run_start < chunk_end {
            let run_style = styled_chars[run_start].1;
            let mut run_end = run_start + 1;
            while run_end < chunk_end && styled_chars[run_end].1 == run_style {
                run_end += 1;
            }
            let text: String = styled_chars[run_start..run_end]
                .iter()
                .map(|(c, _)| c)
                .collect();
            line_spans.push(Span::styled(text, run_style));
            run_start = run_end;
        }

        result.push(line_spans);
        pos = chunk_end;
    }

    if result.is_empty() {
        result.push(Vec::new());
    }

    result
}

/// Trim a list of spans to at most `max_chars` characters total.
fn trim_spans_to_char_count(spans: &[Span<'static>], max_chars: usize) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let mut remaining = max_chars;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let span_chars: usize = span.content.chars().count();
        if span_chars <= remaining {
            result.push(span.clone());
            remaining -= span_chars;
        } else {
            // Partial span
            let truncated: String = span.content.chars().take(remaining).collect();
            result.push(Span::styled(truncated, span.style));
            break;
        }
    }
    result
}

/// Apply a block-cursor highlight at `cursor_col` within pre-styled spans.
/// Splits spans around the cursor character and inverts its style.
fn apply_cursor_highlight_on_spans(
    spans: Vec<Span<'static>>,
    cursor_col: usize,
    _content_width: usize,
) -> Vec<Span<'static>> {
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let chars: Vec<char> = text.chars().collect();

    if chars.is_empty() {
        // Empty line: render a single cursor block
        return vec![Span::styled(
            " ",
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .bg(theme::file_viewer_cursor_line_bg()),
        )];
    }

    let mut result_spans: Vec<Span<'static>> = Vec::new();
    let mut char_pos: usize = 0;

    for span in spans {
        let span_chars: Vec<char> = span.content.chars().collect();
        let span_start = char_pos;
        let span_end = char_pos + span_chars.len();

        if cursor_col >= span_end || cursor_col < span_start {
            result_spans.push(span);
        } else {
            let local_cursor = cursor_col - span_start;

            if local_cursor > 0 {
                let before: String = span_chars[..local_cursor].iter().collect();
                result_spans.push(Span::styled(before, span.style));
            }

            let cursor_char: String = span_chars[local_cursor..local_cursor + 1].iter().collect();
            result_spans.push(Span::styled(
                cursor_char,
                span.style.add_modifier(Modifier::REVERSED),
            ));

            if local_cursor + 1 < span_chars.len() {
                let after: String = span_chars[local_cursor + 1..].iter().collect();
                result_spans.push(Span::styled(after, span.style));
            }
        }

        char_pos = span_end;
    }

    // If cursor is past the end, append a cursor block
    if cursor_col >= chars.len() {
        result_spans.push(Span::styled(
            " ",
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .bg(theme::file_viewer_cursor_line_bg()),
        ));
    }

    result_spans
}

// =============================================================================
// Markdown Preview
// =============================================================================

/// Render the file viewer in markdown preview mode.
/// Uses the existing markdown rendering pipeline from `content.rs`.
fn render_markdown_preview(frame: &mut Frame, area: Rect, viewer: &mut FileViewerState) {
    // Build or use cached rendered lines
    if viewer.markdown_cache.is_none() {
        viewer.markdown_cache = Some(build_markdown_preview(viewer));
    }
    let cached_lines = viewer.markdown_cache.as_ref().unwrap();

    // Viewport scrolling via viewport_top
    let visible_height = area.height as usize;
    viewer.viewport_height = visible_height.max(1);
    if visible_height == 0 {
        return;
    }
    let total = cached_lines.len();
    if total == 0 {
        viewer.viewport_top = 0;
        frame.render_widget(Paragraph::new(Vec::<Line<'_>>::new()), area);
        return;
    }

    // Clamp viewport
    let max_top = total.saturating_sub(visible_height);
    viewer.viewport_top = viewer.viewport_top.min(max_top);

    let start = viewer.viewport_top.min(total);
    let end = (start + visible_height).min(total);

    let visible_lines: Vec<Line<'_>> = cached_lines[start..end].to_vec();
    frame.render_widget(
        Paragraph::new(visible_lines).wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

/// Build rendered markdown lines from the file content.
fn build_markdown_preview(viewer: &FileViewerState) -> Vec<Line<'static>> {
    use super::content::{
        BlockElement, ContentSegment, detect_block_element, parse_content, render_markdown_line,
    };
    use super::highlight::highlight_code;

    let content = viewer.surface.content();
    let segments = parse_content(&content);
    let mut lines: Vec<Line<'static>> = Vec::new();

    for segment in &segments {
        match segment {
            ContentSegment::Text(text) => {
                for line in text.lines() {
                    let (block, content_text) = detect_block_element(line);
                    match &block {
                        BlockElement::TableSeparator => {
                            // Skip table separator rows
                            continue;
                        }
                        _ => {
                            lines.push(render_markdown_line("", &block, content_text));
                        }
                    }
                }
                // Add blank line between segments
                if !text.is_empty() {
                    lines.push(Line::from(""));
                }
            }
            ContentSegment::Code {
                language,
                content: code,
            } => {
                // Render code fence header
                let lang_label = language.unwrap_or("text");
                lines.push(Line::from(vec![Span::styled(
                    format!("  {lang_label}"),
                    Style::default()
                        .fg(theme::code_fence_lang())
                        .add_modifier(Modifier::DIM),
                )]));
                // Syntax-highlighted code lines
                let highlighted = highlight_code(code, *language);
                lines.extend(highlighted);
                // Code fence footer
                lines.push(Line::from(vec![Span::raw("")]));
            }
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unwrap_used)]
    fn bottom_bar_text(viewer: &mut FileViewerState) -> String {
        use ratatui::{Terminal, backend::TestBackend};
        let _theme = crate::ui::theme::pin_theme_state();
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| render_file_viewer_content(frame, Rect::new(0, 0, 80, 8), viewer, true))
            .unwrap();
        (0..80)
            .map(|x| terminal.backend().buffer()[(x, 7)].symbol())
            .collect::<String>()
    }

    #[test]
    fn conflict_indicator_renders_when_prompts_are_closed() {
        let mut viewer = FileViewerState::new("conflict.rs".into(), "draft".into());
        viewer.external_conflict = Some("external".into());
        assert!(bottom_bar_text(&mut viewer).contains("External change"));
    }

    #[test]
    fn live_command_prompt_has_priority_over_conflict_and_finished_search() {
        let mut viewer = FileViewerState::new("conflict.rs".into(), "draft".into());
        viewer.external_conflict = Some("external".into());
        viewer.search.pattern = Some("draft".into());
        viewer.command.active = true;
        viewer.command.buffer = "w!".into();
        viewer.command.cursor = 2;
        assert!(bottom_bar_text(&mut viewer).contains(":w!"));
    }

    #[test]
    fn live_search_prompt_has_priority_over_conflict_and_finished_search() {
        let mut viewer = FileViewerState::new("conflict.rs".into(), "draft".into());
        viewer.external_conflict = Some("external".into());
        viewer.search.pattern = Some("old".into());
        viewer.search.input_active = true;
        viewer.search.forward = true;
        viewer.search.input_buffer = "new".into();
        assert!(bottom_bar_text(&mut viewer).contains("/new"));
    }

    #[test]
    fn test_trailing_ws_split_with_spaces() {
        let (content, ws) = trailing_ws_split("hello   ");
        assert_eq!(content, "hello");
        assert_eq!(ws, "   ");
    }

    #[test]
    fn test_trailing_ws_split_no_trailing() {
        let (content, ws) = trailing_ws_split("hello");
        assert_eq!(content, "hello");
        assert_eq!(ws, "");
    }

    #[test]
    fn test_trailing_ws_split_all_whitespace() {
        let (content, ws) = trailing_ws_split("   ");
        assert_eq!(content, "");
        assert_eq!(ws, "   ");
    }

    #[test]
    fn test_trailing_ws_split_empty() {
        let (content, ws) = trailing_ws_split("");
        assert_eq!(content, "");
        assert_eq!(ws, "");
    }

    #[test]
    fn test_gutter_width_small_file() {
        // Small file: 5 lines, 20 viewport. abs_digits=1, rel_digits=2, floor=3 → 3+2=5 (no git)
        assert_eq!(gutter_width(5, 20, false), 5);
        // With git: +1 = 6
        assert_eq!(gutter_width(5, 20, true), 6);
    }

    #[test]
    fn test_gutter_width_large_file() {
        // Large file: 1500 lines, 40 viewport. abs_digits=4, rel_digits=2, floor=3 → 4+2=6 (no git)
        assert_eq!(gutter_width(1500, 40, false), 6);
        // With git: +1 = 7
        assert_eq!(gutter_width(1500, 40, true), 7);
    }

    #[test]
    fn test_trim_spans_to_char_count() {
        let spans = vec![
            Span::raw("hello".to_owned()),
            Span::raw(" world".to_owned()),
        ];
        let trimmed = trim_spans_to_char_count(&spans, 7);
        let text: String = trimmed.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello w");
    }

    #[test]
    fn test_trim_spans_exact() {
        let spans = vec![Span::raw("hello".to_owned())];
        let trimmed = trim_spans_to_char_count(&spans, 5);
        let text: String = trimmed.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello");
    }
}
