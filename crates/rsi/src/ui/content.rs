//! Content parsing for conversation event text.
//!
//! Detects markdown code fences in event content and splits
//! into typed segments for differential rendering.

/// Maximum content lines before truncation. Events with more lines
/// show a "... N more lines" indicator unless expanded via `zo`.
pub const MAX_CONTENT_LINES: usize = 30;

/// A segment of parsed content.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentSegment<'a> {
    /// Plain text lines (not inside a code fence).
    Text(&'a str),
    /// A code block with optional language tag and content.
    Code {
        language: Option<&'a str>,
        content: &'a str,
    },
}

/// Parse event content into segments, detecting markdown code fences.
///
/// Recognizes ``` fences with optional language tags.
/// Returns segments in order, where each segment is either plain text
/// or a code block. Fence lines themselves are excluded from content.
pub fn parse_content(content: &str) -> Vec<ContentSegment<'_>> {
    let mut segments = Vec::new();
    let mut current_text_start: Option<usize> = None;
    let mut in_code_block = false;
    let mut code_language: Option<&str> = None;
    let mut code_start: Option<usize> = None;

    let mut offset = 0;
    for line in content.split('\n') {
        let trimmed = line.trim_start();

        if !in_code_block && trimmed.starts_with("```") {
            // Flush accumulated text
            if let Some(start) = current_text_start {
                let text = &content[start..offset];
                if !text.is_empty() {
                    segments.push(ContentSegment::Text(text.trim_end_matches('\n')));
                }
                current_text_start = None;
            }

            // Enter code block
            in_code_block = true;
            let lang = trimmed[3..].trim();
            code_language = if lang.is_empty() { None } else { Some(lang) };
            code_start = Some((offset + line.len() + 1).min(content.len())); // after the newline, clamped
        } else if in_code_block && trimmed.starts_with("```") {
            // Exit code block
            if let Some(start) = code_start {
                let end = offset;
                let code = if start <= end {
                    content[start..end].trim_end_matches('\n')
                } else {
                    ""
                };
                segments.push(ContentSegment::Code {
                    language: code_language,
                    content: code,
                });
            }
            in_code_block = false;
            code_language = None;
            code_start = None;
        } else if !in_code_block && current_text_start.is_none() {
            current_text_start = Some(offset);
        }
        // Inside code block: accumulate (handled by start/end tracking)

        offset += line.len() + 1; // +1 for the \n
    }

    // Flush remaining
    if in_code_block {
        // Unclosed code block — treat as code
        if let Some(start) = code_start {
            let code = content[start..].trim_end_matches('\n');
            if !code.is_empty() {
                segments.push(ContentSegment::Code {
                    language: code_language,
                    content: code,
                });
            }
        }
    } else if let Some(start) = current_text_start {
        let text = content[start..].trim_end_matches('\n');
        if !text.is_empty() {
            segments.push(ContentSegment::Text(text));
        }
    }

    segments
}

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use rsi_common::types::{ConversationEvent, EventType, Role};

/// Block-level markdown element type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockElement {
    /// No block-level element detected.
    None,
    /// ATX header with level (1-6).
    Header(u8),
    /// Blockquote (> prefix).
    Blockquote,
    /// Unordered list item (- or * prefix).
    UnorderedList,
    /// Ordered list item with number.
    OrderedList(u32),
    /// Horizontal rule (---, ***, ___).
    HorizontalRule,
    /// Task list item, unchecked (- [ ]).
    TaskUnchecked,
    /// Task list item, checked (- [x]).
    TaskChecked,
    /// Table data row (| col | col |).
    TableRow,
    /// Table separator row (|---|---|) — should be hidden.
    TableSeparator,
}

/// Detect block-level markdown element from a line prefix.
/// Returns (element_type, remaining_content_after_prefix).
pub fn detect_block_element(line: &str) -> (BlockElement, &str) {
    let trimmed = line.trim_start();

    // Horizontal rule: 3+ of same char (-, *, _), optionally with spaces
    if trimmed.len() >= 3 {
        let hr_char = trimmed.chars().next().unwrap();
        if matches!(hr_char, '-' | '*' | '_')
            && trimmed.chars().all(|c| c == hr_char || c == ' ')
            && trimmed.chars().filter(|&c| c == hr_char).count() >= 3
        {
            return (BlockElement::HorizontalRule, "");
        }
    }

    // GFM-style table separator: every cell contains one or more dashes,
    // with optional leading/trailing alignment colons.
    if is_table_separator_line(trimmed) {
        return (BlockElement::TableSeparator, trimmed);
    }

    // Table row: starts and ends with |
    if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
        return (BlockElement::TableRow, trimmed);
    }

    // Header: # prefix
    if trimmed.starts_with('#') {
        let level = trimmed.chars().take_while(|&c| c == '#').count();
        if level <= 6 {
            let rest = trimmed[level..].trim_start();
            return (BlockElement::Header(level as u8), rest);
        }
    }

    // Blockquote: > prefix
    if let Some(rest) = trimmed.strip_prefix('>') {
        return (BlockElement::Blockquote, rest.trim_start());
    }

    // Task list: - [ ] or - [x] or * [ ] or * [x]
    if (trimmed.starts_with("- [") || trimmed.starts_with("* [")) && trimmed.len() >= 5 {
        let checkbox_char = trimmed.as_bytes()[3];
        if trimmed.as_bytes()[4] == b']' {
            let rest = trimmed[5..].trim_start();
            match checkbox_char {
                b'x' | b'X' => return (BlockElement::TaskChecked, rest),
                b' ' => return (BlockElement::TaskUnchecked, rest),
                _ => {} // fall through to normal list
            }
        }
    }

    // Unordered list: - or * followed by space
    if (trimmed.starts_with("- ") || trimmed.starts_with("* ")) && trimmed.len() > 2 {
        return (BlockElement::UnorderedList, &trimmed[2..]);
    }

    // Ordered list: digits followed by . and space
    if let Some(dot_pos) = trimmed.find(". ") {
        let num_str = &trimmed[..dot_pos];
        if !num_str.is_empty()
            && num_str.chars().all(|c| c.is_ascii_digit())
            && let Ok(n) = num_str.parse::<u32>()
        {
            return (BlockElement::OrderedList(n), &trimmed[dot_pos + 2..]);
        }
    }

    (BlockElement::None, trimmed)
}

fn pipe_cells(line: &str) -> Vec<&str> {
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() >= 3
        && parts.first().is_some_and(|part| part.trim().is_empty())
        && parts.last().is_some_and(|part| part.trim().is_empty())
    {
        parts[1..parts.len() - 1]
            .iter()
            .map(|part| part.trim())
            .collect()
    } else {
        Vec::new()
    }
}

fn is_table_separator_line(line: &str) -> bool {
    let cells = table_separator_cells(line);
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let rule = cell.strip_prefix(':').unwrap_or(cell);
            let rule = rule.strip_suffix(':').unwrap_or(rule);
            !rule.is_empty() && rule.chars().all(|ch| ch == '-')
        })
}

fn table_separator_cells(line: &str) -> Vec<&str> {
    let Some(body) = line.trim_end().strip_prefix('|') else {
        return Vec::new();
    };
    let body = body.strip_suffix('|').unwrap_or(body);
    let cells: Vec<&str> = body.split('|').map(str::trim).collect();
    if cells.iter().any(|cell| cell.is_empty()) {
        Vec::new()
    } else {
        cells
    }
}

/// Render a single markdown line with block-level prefix and inline styles.
///
/// Shared between session.rs (rendering) and height.rs (height calculation)
/// to ensure both produce identical line structures.
pub fn render_markdown_line(
    indent: &str,
    block: &BlockElement,
    content_text: &str,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();

    // Always start with indent
    if !indent.is_empty() {
        spans.push(Span::raw(indent.to_string()));
    }

    match block {
        BlockElement::Header(level) => {
            // Render header text without # prefix — styled bold + color is sufficient
            // to distinguish headers. Lower levels get dimmer colors.
            let header_style = match level {
                1 => Style::default()
                    .fg(super::theme::md_header())
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                2 => Style::default()
                    .fg(super::theme::md_header())
                    .add_modifier(Modifier::BOLD),
                3 => Style::default().fg(super::theme::md_header()),
                _ => Style::default().fg(super::theme::md_header_minor()),
            };
            let path_fg = super::theme::file_path_fg();
            for span in parse_inline_markdown(content_text) {
                // Preserve file-path fg so click detection still finds the span
                let merged = if span.style.fg == Some(path_fg) {
                    span.style.add_modifier(header_style.add_modifier)
                } else {
                    span.style.patch(header_style)
                };
                spans.push(Span::styled(span.content.to_string(), merged));
            }
        }

        BlockElement::Blockquote => {
            spans.push(Span::styled(
                "\u{2502} ".to_string(),
                Style::default().fg(super::theme::md_blockquote()),
            ));
            let path_fg = super::theme::file_path_fg();
            for span in parse_inline_markdown(content_text) {
                // Preserve file-path fg so click detection still finds the span
                let merged = if span.style.fg == Some(path_fg) {
                    span.style.add_modifier(Modifier::ITALIC)
                } else {
                    span.style
                        .add_modifier(Modifier::ITALIC)
                        .fg(super::theme::md_blockquote())
                };
                spans.push(Span::styled(span.content.to_string(), merged));
            }
        }

        BlockElement::TaskUnchecked => {
            spans.push(Span::styled(
                "\u{2610} ".to_string(), // ☐
                Style::default().fg(super::theme::md_list_bullet()),
            ));
            spans.extend(parse_inline_markdown(content_text));
        }

        BlockElement::TaskChecked => {
            spans.push(Span::styled(
                "\u{2611} ".to_string(), // ☑
                Style::default().fg(super::theme::md_task_done()),
            ));
            let path_fg = super::theme::file_path_fg();
            for span in parse_inline_markdown(content_text) {
                // Preserve file-path fg so click detection still finds the span
                let merged = if span.style.fg == Some(path_fg) {
                    span.style.add_modifier(Modifier::CROSSED_OUT)
                } else {
                    span.style
                        .add_modifier(Modifier::CROSSED_OUT)
                        .fg(super::theme::md_task_done())
                };
                spans.push(Span::styled(span.content.to_string(), merged));
            }
        }

        BlockElement::UnorderedList => {
            spans.push(Span::styled(
                "\u{2022} ".to_string(),
                Style::default().fg(super::theme::md_list_bullet()),
            ));
            spans.extend(parse_inline_markdown(content_text));
        }

        BlockElement::OrderedList(n) => {
            spans.push(Span::styled(
                format!("{}. ", n),
                Style::default().fg(super::theme::md_list_bullet()),
            ));
            spans.extend(parse_inline_markdown(content_text));
        }

        BlockElement::HorizontalRule => {
            spans.push(Span::styled(
                "\u{2500}".repeat(60),
                Style::default().fg(super::theme::md_hr()),
            ));
        }

        BlockElement::TableRow => {
            for part in content_text.split('|') {
                if !spans.is_empty() || content_text.starts_with('|') {
                    spans.push(Span::styled(
                        "\u{2502}".to_string(),
                        Style::default().fg(super::theme::md_table_border()),
                    ));
                }
                let trimmed = part.trim();
                if !trimmed.is_empty() {
                    spans.push(Span::raw(" ".to_string()));
                    spans.extend(parse_inline_markdown(trimmed));
                    spans.push(Span::raw(" ".to_string()));
                }
            }
        }

        BlockElement::TableSeparator => {
            return Line::default();
        }

        BlockElement::None => {
            spans.extend(parse_inline_markdown(content_text));
        }
    }

    Line::from(spans)
}

/// Render a complete table block with aligned columns.
///
/// Two-pass: first computes max column widths across all rows,
/// then renders each row with cells padded to those widths.
/// Separator rows render as horizontal box-drawing lines.
fn render_table_block(
    indent: &str,
    rows: &[(BlockElement, &str)],
    max_width: u16,
) -> Vec<Line<'static>> {
    let TableLayout {
        parsed_rows,
        mut col_widths,
    } = table_layout(rows);

    if table_needs_stacked_layout(indent, &col_widths, max_width) {
        return render_stacked_table(indent, &parsed_rows, max_width);
    }

    // Constrain column widths to fit within max_width.
    // Table structure: indent + │ + ( space + content + space + │ ) per column
    // Overhead = indent_len + num_cols + 1 (borders) + num_cols * 2 (cell padding)
    if !col_widths.is_empty() && max_width > 0 {
        let num_cols = col_widths.len();
        let overhead = indent.chars().count() + num_cols.saturating_mul(3) + 1;
        let available = (max_width as usize).saturating_sub(overhead);
        let total: usize = col_widths.iter().sum();

        if total > available && available > 0 {
            // The stacked-layout gate guarantees these readable minima fit.
            // Spend remaining cells on the column with the largest unmet need;
            // every increment stays within both the natural and total width.
            let mut new_widths: Vec<usize> = col_widths
                .iter()
                .map(|width| (*width).min(MIN_READABLE_TABLE_COLUMN_WIDTH))
                .collect();
            let mut remaining = available.saturating_sub(new_widths.iter().sum());
            while remaining > 0 {
                let Some((index, _)) = col_widths
                    .iter()
                    .enumerate()
                    .filter(|(index, width)| new_widths[*index] < **width)
                    .max_by_key(|(index, width)| **width - new_widths[*index])
                else {
                    break;
                };
                new_widths[index] += 1;
                remaining -= 1;
            }

            col_widths = new_widths;
        }
    }

    // Second pass: render rows. When cell content exceeds column width,
    // word-wrap within the cell and emit multiple output lines for that row.
    let mut lines = Vec::new();
    let border_style = Style::default().fg(super::theme::md_table_border());

    lines.push(table_border_line(
        indent,
        &col_widths,
        "┌",
        "┬",
        "┐",
        border_style,
    ));

    for (row_idx, (block, _)) in rows.iter().enumerate() {
        if *block == BlockElement::TableSeparator {
            lines.push(table_border_line(
                indent,
                &col_widths,
                "├",
                "┼",
                "┤",
                border_style,
            ));
            continue;
        }

        let cells = &parsed_rows[row_idx];

        // Word-wrap each cell's content to fit column width. Uses rendered width
        // (excluding markdown delimiters like ** and `) for accurate wrapping.
        let mut wrapped_cells: Vec<Vec<String>> = Vec::new();
        let mut max_lines = 1usize;

        for (i, &w) in col_widths.iter().enumerate() {
            let cell_text = cells.get(i).copied().unwrap_or("");
            let rendered_width: usize = parse_inline_markdown(cell_text)
                .iter()
                .map(|s| s.content.chars().count())
                .sum();

            if w == 0 || rendered_width <= w {
                wrapped_cells.push(vec![cell_text.to_string()]);
            } else {
                // Wrap at word boundaries using rendered width.
                // First flatten to plain text for wrapping decisions,
                // then map break positions back to the raw markdown text.
                let cell_lines = wrap_cell_text(cell_text, w);
                if cell_lines.len() > max_lines {
                    max_lines = cell_lines.len();
                }
                wrapped_cells.push(cell_lines);
            }
        }

        // Emit one output Line per wrapped line, padding shorter cells with blanks.
        for line_idx in 0..max_lines {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if !indent.is_empty() {
                spans.push(Span::raw(indent.to_string()));
            }
            spans.push(Span::styled("│".to_string(), border_style));

            for (i, &w) in col_widths.iter().enumerate() {
                let chunk = wrapped_cells
                    .get(i)
                    .and_then(|cell_lines| cell_lines.get(line_idx))
                    .map(|s| s.as_str())
                    .unwrap_or("");

                let cell_spans = parse_inline_markdown(chunk);
                let actual_width: usize =
                    cell_spans.iter().map(|s| s.content.chars().count()).sum();
                let padding = w.saturating_sub(actual_width);

                spans.push(Span::raw(" ".to_string()));
                spans.extend(cell_spans);
                if padding > 0 {
                    spans.push(Span::raw(" ".repeat(padding)));
                }
                spans.push(Span::raw(" ".to_string()));
                spans.push(Span::styled("│".to_string(), border_style));
            }
            lines.push(Line::from(spans));
        }
    }

    lines.push(table_border_line(
        indent,
        &col_widths,
        "└",
        "┴",
        "┘",
        border_style,
    ));

    lines
}

const MIN_READABLE_TABLE_COLUMN_WIDTH: usize = 8;

fn table_needs_stacked_layout(indent: &str, col_widths: &[usize], max_width: u16) -> bool {
    if col_widths.is_empty() || max_width == 0 {
        return false;
    }

    let natural_width = table_line_width(indent, col_widths);
    let readable_content_width: usize = col_widths
        .iter()
        .map(|width| (*width).min(MIN_READABLE_TABLE_COLUMN_WIDTH))
        .sum();
    let readable_width =
        indent.chars().count() + readable_content_width + col_widths.len().saturating_mul(3) + 1;

    natural_width > max_width as usize && readable_width > max_width as usize
}

/// Render dense tables vertically when the pane cannot preserve readable
/// columns. Header-only tables become a column list; data rows become labeled
/// records. This avoids four-character columns and mid-word header fragments.
fn render_stacked_table(
    indent: &str,
    parsed_rows: &[Vec<&str>],
    max_width: u16,
) -> Vec<Line<'static>> {
    let Some(headers) = parsed_rows.first() else {
        return Vec::new();
    };
    let body_rows: Vec<&Vec<&str>> = parsed_rows.iter().skip(2).collect();
    let content_width = (max_width as usize)
        .saturating_sub(indent.chars().count())
        .max(1);
    let mut lines = Vec::new();

    if body_rows.is_empty() {
        lines.push(Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled("Columns", Style::default().add_modifier(Modifier::BOLD)),
        ]));
        for header in headers {
            let chunks = word_wrap(header, content_width.saturating_sub(2).max(1));
            for (index, chunk) in chunks.iter().enumerate() {
                let prefix = if index == 0 { "• " } else { "  " };
                let mut spans = vec![Span::raw(format!("{indent}{prefix}"))];
                spans.extend(parse_inline_markdown(chunk));
                lines.push(Line::from(spans));
            }
        }
        return lines;
    }

    for (row_index, row) in body_rows.iter().enumerate() {
        if body_rows.len() > 1 {
            lines.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(
                    format!("Row {}", row_index + 1),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        for column_index in 0..headers.len().max(row.len()) {
            let fallback_header = format!("Column {}", column_index + 1);
            let header = headers
                .get(column_index)
                .copied()
                .filter(|header| !header.is_empty())
                .unwrap_or(&fallback_header);
            let value = row.get(column_index).copied().unwrap_or("");
            let text = format!("{header}: {value}");
            for chunk in word_wrap(&text, content_width) {
                let mut spans = vec![Span::raw(indent.to_string())];
                spans.extend(parse_inline_markdown(&chunk));
                lines.push(Line::from(spans));
            }
        }
        if row_index + 1 < body_rows.len() {
            lines.push(Line::default());
        }
    }

    lines
}

/// Parsed table rows and the natural width of each column.
struct TableLayout<'a> {
    parsed_rows: Vec<Vec<&'a str>>,
    col_widths: Vec<usize>,
}

/// Parse table cells and measure their rendered (not Markdown-source) widths.
///
/// This is shared by rendering and layout selection so that a table never
/// requests a width different from the one its renderer considers natural.
fn table_layout<'a>(rows: &[(BlockElement, &'a str)]) -> TableLayout<'a> {
    let mut parsed_rows: Vec<Vec<&str>> = Vec::new();
    let mut col_widths: Vec<usize> = Vec::new();

    for (block, content) in rows {
        if *block == BlockElement::TableSeparator {
            parsed_rows.push(Vec::new()); // placeholder
            continue;
        }
        let trimmed = pipe_cells(content);
        for (i, cell) in trimmed.iter().enumerate() {
            let rendered_width: usize = parse_inline_markdown(cell)
                .iter()
                .map(|s| s.content.chars().count())
                .sum();
            if i >= col_widths.len() {
                col_widths.push(rendered_width);
            } else if rendered_width > col_widths[i] {
                col_widths[i] = rendered_width;
            }
        }
        parsed_rows.push(trimmed);
    }

    TableLayout {
        parsed_rows,
        col_widths,
    }
}

fn is_valid_table_block(rows: &[(BlockElement, &str)]) -> bool {
    if rows.len() < 2
        || rows[0].0 != BlockElement::TableRow
        || rows[1].0 != BlockElement::TableSeparator
    {
        return false;
    }

    let header_columns = pipe_cells(rows[0].1).len();
    header_columns > 0 && table_separator_cells(rows[1].1).len() == header_columns
}

/// Width of a complete box-drawn table line, including indent, cell padding,
/// and borders.
fn table_line_width(indent: &str, col_widths: &[usize]) -> usize {
    indent.chars().count()
        + col_widths.iter().sum::<usize>()
        + col_widths.len().saturating_mul(3)
        + 1
}

/// Return the widest natural table line in rendered Markdown text.
fn markdown_text_table_width(text: &str, indent: &str) -> Option<usize> {
    let mut table_buf: Vec<(BlockElement, &str)> = Vec::new();
    let mut widest: Option<usize> = None;

    for content_line in text.lines() {
        let (block, block_content) = detect_block_element(content_line);
        match block {
            BlockElement::TableRow | BlockElement::TableSeparator => {
                table_buf.push((block, block_content));
            }
            _ => {
                if !table_buf.is_empty() {
                    if is_valid_table_block(&table_buf) {
                        let layout = table_layout(&table_buf);
                        widest = Some(
                            widest
                                .unwrap_or_default()
                                .max(table_line_width(indent, &layout.col_widths)),
                        );
                    }
                    table_buf.clear();
                }
            }
        }
    }

    if !table_buf.is_empty() {
        if is_valid_table_block(&table_buf) {
            let layout = table_layout(&table_buf);
            widest = Some(
                widest
                    .unwrap_or_default()
                    .max(table_line_width(indent, &layout.col_widths)),
            );
        }
    }

    widest
}

/// Return the natural line width of the widest Markdown table rendered for an
/// event. Only text and explicitly Markdown-fenced code use the Markdown table
/// renderer, matching `build_event_lines_with_interaction_meta` exactly.
pub(crate) fn event_markdown_table_width(event: &ConversationEvent) -> Option<u16> {
    let indent = markdown_event_indent(event.event_type);
    let mut widest: Option<usize> = None;

    for segment in parse_content(&event.content) {
        let table_width = match segment {
            ContentSegment::Text(text) => markdown_text_table_width(text, indent),
            ContentSegment::Code {
                language: Some(language),
                content,
            } if is_markdown_fence_language(language) => markdown_text_table_width(content, indent),
            ContentSegment::Code { .. } => None,
        };
        if let Some(table_width) = table_width {
            widest = Some(widest.unwrap_or_default().max(table_width));
        }
    }

    widest.map(|width| u16::try_from(width).unwrap_or(u16::MAX))
}

fn table_border_line(
    indent: &str,
    col_widths: &[usize],
    left: &'static str,
    join: &'static str,
    right: &'static str,
    border_style: Style,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !indent.is_empty() {
        spans.push(Span::raw(indent.to_string()));
    }
    spans.push(Span::styled(left, border_style));
    for (i, &w) in col_widths.iter().enumerate() {
        spans.push(Span::styled("─".repeat(w + 2), border_style));
        if i + 1 < col_widths.len() {
            spans.push(Span::styled(join, border_style));
        }
    }
    spans.push(Span::styled(right, border_style));
    Line::from(spans)
}

/// Word-wrap cell text for table rendering, breaking at word boundaries based
/// on **rendered** width (excluding markdown delimiters like `**` and `` ` ``).
///
/// Standard `word_wrap` operates on raw text bytes, which overcounts width when
/// markdown formatting is present. This function splits on spaces, measures each
/// word's rendered width, and accumulates words per line until the column width
/// is reached.
fn wrap_cell_text(raw_text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![raw_text.to_string()];
    }

    let words: Vec<&str> = raw_text.split_whitespace().collect();
    if words.is_empty() {
        return vec![String::new()];
    }

    let mut result: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let mut current_rendered_width = 0usize;

    for word in &words {
        let word_rendered_width: usize = parse_inline_markdown(word)
            .iter()
            .map(|s| s.content.chars().count())
            .sum();

        if current_line.is_empty() {
            if word_rendered_width > max_width && max_width >= 1 {
                // Word exceeds column width (e.g. a long file path with no spaces).
                // Hard-break it at max_width chars so the table stays within bounds.
                let mut remaining_word = *word;
                while !remaining_word.is_empty() {
                    let remaining_rendered: usize = parse_inline_markdown(remaining_word)
                        .iter()
                        .map(|s| s.content.chars().count())
                        .sum();
                    if remaining_rendered <= max_width {
                        current_line.push_str(remaining_word);
                        current_rendered_width = remaining_rendered;
                        break;
                    }
                    // Slice at max_width chars (char_indices gives byte positions safely)
                    let byte_end = remaining_word
                        .char_indices()
                        .nth(max_width)
                        .map(|(i, _)| i)
                        .unwrap_or(remaining_word.len());
                    result.push(remaining_word[..byte_end].to_string());
                    remaining_word = &remaining_word[byte_end..];
                }
            } else {
                // First word fits — place it on the current line.
                current_line.push_str(word);
                current_rendered_width = word_rendered_width;
            }
        } else if current_rendered_width + 1 + word_rendered_width <= max_width {
            // Fits with a space separator
            current_line.push(' ');
            current_line.push_str(word);
            current_rendered_width += 1 + word_rendered_width;
        } else {
            // Doesn't fit — start a new line
            result.push(current_line);
            current_line = word.to_string();
            current_rendered_width = word_rendered_width;
        }
    }

    if !current_line.is_empty() {
        result.push(current_line);
    }

    if result.is_empty() {
        result.push(String::new());
    }

    result
}

/// Check if a docregblock content string is a question indicator (`?`).
///
/// Question indicators are non-executable yellow pills that signal
/// the session is waiting for user input.
pub fn is_question_indicator(content: &str) -> bool {
    content.trim() == "?"
}

/// Derive a display label from docregblock content.
///
/// Extracts the command name (stripping leading `/`). Falls back to generic label.
/// Question indicators (`?`) render as "Questions" instead of a command.
///
/// Examples:
///   "/implement @thoughts/shared/plans/foo.md" → "▸ implement"
///   "/research @thoughts/shared/research/bar.md"    → "▸ research"
///   "?"                                             → " Questions "
///   "some arbitrary text"                           → "▸ Execute Block"
fn docregblock_label(content: &str) -> String {
    let trimmed = content.trim();

    if is_question_indicator(trimmed) {
        return " Questions ".to_string();
    }

    // Extract command name (first word, possibly starting with /)
    let command = trimmed.split_whitespace().next().unwrap_or("");
    let command_clean = command.strip_prefix('/').unwrap_or(command);

    if !command_clean.is_empty() && command.starts_with('/') {
        return format!(" \u{25B8} {} ", command_clean);
    }

    " \u{25B8} Execute Block ".to_string()
}

/// Derives the next pipeline command from a pipeline artifact path.
/// The directory determines the command; the path is the argument.
///
/// - `thoughts/shared/research/*.md` → `/plan @path`
/// - `thoughts/shared/plans/*.md` → `/implement @path`
/// - `thoughts/shared/handoffs/**/*.md` → `/resume_handoff path`
pub fn pipeline_artifact_to_command(path: &str) -> Option<String> {
    if path.contains("thoughts/shared/research/") {
        Some(format!("/plan @{}", path))
    } else if path.contains("thoughts/shared/plans/") {
        Some(format!("/implement @{}", path))
    } else if path.contains("thoughts/shared/handoffs/") {
        Some(format!("/resume_handoff {}", path))
    } else {
        None
    }
}

/// Generate a compact pill label for session list display.
///
/// Abbreviates known commands to 4-char short forms.
/// Returns `None` if no docregblock contents.
pub fn docregblock_pill_label(contents: &[String]) -> Option<String> {
    let first = contents.first()?;
    let trimmed = first.trim();

    if is_question_indicator(trimmed) {
        return Some("?".to_string());
    }

    let command = trimmed.split_whitespace().next().unwrap_or("");
    let command_clean = command.strip_prefix('/').unwrap_or(command);

    let abbrev = if command.starts_with('/') && !command_clean.is_empty() {
        match command_clean {
            "implement" => "impl",
            "plan" => "plan",
            "iterate_plan" => "iter",
            "research" => "rsch",
            "commit" | "ci_commit" => "cmit",
            "review_plan" | "local_review" | "code_review" => "rvew",
            "validate_plan" => "vald",
            "merge_ready" => "mrg",
            "describe_pr" | "ci_describe_pr" => "pr",
            "debug" => "dbug",
            "oneshot" | "oneshot_plan" => "1sht",
            other => {
                // Take first 4 chars of unknown commands
                let end = other.floor_char_boundary(4.min(other.len()));
                &other[..end]
            }
        }
    } else {
        "exec"
    };

    Some(abbrev.to_string())
}

/// Build a minimal pill span for a docregblock label.
///
/// Returns a single span: just the label on a colored background.
/// No caps or padding — tightest possible rendering.
pub fn build_docregblock_pill_spans(label: &str) -> Vec<Span<'static>> {
    let (fg, bg) = if label == "?" {
        (
            super::theme::question_indicator_fg(),
            super::theme::question_indicator_bg(),
        )
    } else {
        (
            super::theme::docregblock_fg(),
            super::theme::docregblock_bg(),
        )
    };
    vec![Span::styled(
        label.to_string(),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )]
}

/// Parse inline markdown in a single line, returning styled spans.
///
/// Handles: **bold**, *italic*, `inline code`, [text](url), <docregblock> tags.
/// Single-pass, left-to-right. Outermost delimiter wins (no nesting).
/// Unmatched delimiters are emitted as plain text.
pub fn parse_inline_markdown(line: &str) -> Vec<Span<'static>> {
    if line.is_empty() {
        return Vec::new();
    }

    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain_start = 0;
    let mut i = 0;

    while i < chars.len() {
        // ** bold **
        if i + 1 < chars.len()
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && let Some(end) = find_closing(&chars, i + 2, &['*', '*'])
        {
            flush_plain(&chars, plain_start, i, &mut spans);
            let content: String = chars[i + 2..end].iter().collect();
            // Recurse so file paths inside bold retain their fg+underline styling
            for mut span in parse_inline_markdown(&content) {
                span.style = span.style.add_modifier(Modifier::BOLD);
                spans.push(span);
            }
            i = end + 2;
            plain_start = i;
            continue;
        }

        // __ bold __ (underscore variant, word-boundary guarded)
        if i + 1 < chars.len()
            && chars[i] == '_'
            && chars[i + 1] == '_'
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && let Some(end) = find_closing(&chars, i + 2, &['_', '_'])
            && (end + 2 >= chars.len() || !chars[end + 2].is_alphanumeric())
        {
            flush_plain(&chars, plain_start, i, &mut spans);
            let content: String = chars[i + 2..end].iter().collect();
            // Recurse so file paths inside bold retain their fg+underline styling
            for mut span in parse_inline_markdown(&content) {
                span.style = span.style.add_modifier(Modifier::BOLD);
                spans.push(span);
            }
            i = end + 2;
            plain_start = i;
            continue;
        }

        // * italic * (but not **)
        if chars[i] == '*'
            && (i + 1 >= chars.len() || chars[i + 1] != '*')
            && let Some(end) = find_closing_single(&chars, i + 1, '*')
        {
            flush_plain(&chars, plain_start, i, &mut spans);
            let content: String = chars[i + 1..end].iter().collect();
            // Recurse so file paths inside italic retain their fg+underline styling
            for mut span in parse_inline_markdown(&content) {
                span.style = span.style.add_modifier(Modifier::ITALIC);
                spans.push(span);
            }
            i = end + 1;
            plain_start = i;
            continue;
        }

        // _ italic _ (underscore variant, word-boundary guarded, not __)
        if chars[i] == '_'
            && (i + 1 >= chars.len() || chars[i + 1] != '_')
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && let Some(end) = find_closing_single(&chars, i + 1, '_')
            && (end + 1 >= chars.len() || !chars[end + 1].is_alphanumeric())
        {
            flush_plain(&chars, plain_start, i, &mut spans);
            let content: String = chars[i + 1..end].iter().collect();
            // Recurse so file paths inside italic retain their fg+underline styling
            for mut span in parse_inline_markdown(&content) {
                span.style = span.style.add_modifier(Modifier::ITALIC);
                spans.push(span);
            }
            i = end + 1;
            plain_start = i;
            continue;
        }

        // ~~ strikethrough ~~
        if i + 1 < chars.len()
            && chars[i] == '~'
            && chars[i + 1] == '~'
            && let Some(end) = find_closing(&chars, i + 2, &['~', '~'])
        {
            flush_plain(&chars, plain_start, i, &mut spans);
            let content: String = chars[i + 2..end].iter().collect();
            spans.push(Span::styled(
                content,
                Style::default()
                    .add_modifier(Modifier::CROSSED_OUT)
                    .fg(super::theme::md_strikethrough()),
            ));
            i = end + 2;
            plain_start = i;
            continue;
        }

        // ` inline code ` — with path-aware styling
        if chars[i] == '`'
            && let Some(end) = find_closing_single(&chars, i + 1, '`')
        {
            flush_plain(&chars, plain_start, i, &mut spans);
            let content: String = chars[i + 1..end].iter().collect();
            let style = if looks_like_filepath(&content) {
                // File path inside backticks: sapphire + underline (clickable)
                Style::default()
                    .fg(super::theme::file_path_fg())
                    .bg(super::theme::md_inline_code_bg())
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                // Regular inline code
                Style::default()
                    .fg(super::theme::md_inline_code_fg())
                    .bg(super::theme::md_inline_code_bg())
            };
            spans.push(Span::styled(content, style));
            i = end + 1;
            plain_start = i;
            continue;
        }

        // [text](url)
        if chars[i] == '['
            && let Some((text, url, end_pos)) = parse_link(&chars, i)
        {
            flush_plain(&chars, plain_start, i, &mut spans);
            let is_file_link = looks_like_filepath(&url);
            // Keep control sequences out of ratatui Span content. Ratatui
            // 0.29 measures Span width from the raw string, so embedding an
            // OSC 8 opener/closer here makes its reflow split the escape
            // sequence and leak fragments such as `8;` into the terminal.
            // The styled label keeps links readable; external links also get a
            // visible URL fallback, while local file targets use render
            // metadata for click-to-copy/open.
            spans.push(Span::styled(
                text,
                if is_file_link {
                    Style::default()
                        .fg(super::theme::file_path_fg())
                        .add_modifier(Modifier::UNDERLINED)
                } else {
                    Style::default()
                        .fg(super::theme::md_link_text())
                        .add_modifier(Modifier::UNDERLINED)
                },
            ));
            // External links retain a visible URL fallback. Local file links
            // show only their short label; their target is carried separately
            // in the render metadata used by click-to-copy/open.
            if !is_file_link {
                spans.push(Span::raw(" ("));
                spans.push(Span::styled(
                    url,
                    Style::default().fg(super::theme::md_link_url()),
                ));
                spans.push(Span::raw(")"));
            }
            i = end_pos;
            plain_start = i;
            continue;
        }

        // <docregblock>...</docregblock>
        if chars[i] == '<' {
            let tag_open = "<docregblock>";
            let tag_close = "</docregblock>";
            let remaining: String = chars[i..].iter().collect();
            if remaining.starts_with(tag_open)
                && let Some(close_pos) = remaining.find(tag_close)
            {
                flush_plain(&chars, plain_start, i, &mut spans);
                let inner = &remaining[tag_open.len()..close_pos];
                let label = docregblock_label(inner);
                let (fg, bg) = if is_question_indicator(inner) {
                    (
                        super::theme::question_indicator_fg(),
                        super::theme::question_indicator_bg(),
                    )
                } else {
                    (
                        super::theme::docregblock_fg(),
                        super::theme::docregblock_bg(),
                    )
                };
                spans.push(Span::styled(
                    label,
                    Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
                ));
                // Advance past the entire tag
                let total_tag_len = close_pos + tag_close.len();
                // Count chars consumed (not bytes)
                let chars_consumed = remaining[..total_tag_len].chars().count();
                i += chars_consumed;
                plain_start = i;
                continue;
            }
        }

        // Bare URL: http://... or https://... (not already inside [text](url) or `code`)
        if chars[i] == 'h'
            && let Some((url, end)) = try_parse_bare_url(&chars, i)
        {
            flush_plain(&chars, plain_start, i, &mut spans);
            spans.push(Span::styled(
                url,
                Style::default()
                    .fg(super::theme::md_link_text())
                    .add_modifier(Modifier::UNDERLINED),
            ));
            i = end;
            plain_start = i;
            continue;
        }

        // File path: /absolute/path.ext, ~/path.ext, or relative/path.ext
        if (chars[i] == '/'
            || chars[i] == '~'
            || chars[i] == '.'
            || chars[i].is_alphanumeric()
            || chars[i] == '_')
            && let Some((path, end)) = try_parse_filepath(&chars, i)
        {
            flush_plain(&chars, plain_start, i, &mut spans);
            spans.push(Span::styled(
                path,
                Style::default()
                    .fg(super::theme::file_path_fg())
                    .add_modifier(Modifier::UNDERLINED),
            ));
            i = end;
            plain_start = i;
            continue;
        }

        i += 1;
    }

    // Flush remaining plain text
    flush_plain(&chars, plain_start, chars.len(), &mut spans);
    spans
}

/// Flush accumulated plain text as an unstyled span.
fn flush_plain(chars: &[char], start: usize, end: usize, spans: &mut Vec<Span<'static>>) {
    if start < end {
        let text: String = chars[start..end].iter().collect();
        spans.push(Span::raw(text));
    }
}

/// Find closing double-char delimiter (e.g., **).
/// Returns the index of the first char of the closing delimiter.
fn find_closing(chars: &[char], start: usize, delim: &[char; 2]) -> Option<usize> {
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == delim[0] && chars[i + 1] == delim[1] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Find closing single-char delimiter.
/// Returns the index of the closing delimiter.
fn find_closing_single(chars: &[char], start: usize, delim: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == delim {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse a markdown link: [text](url). Returns (text, url, end_position).
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    // start is at '['
    let text_end = find_closing_single(chars, start + 1, ']')?;
    // Must be followed by '('
    if text_end + 1 >= chars.len() || chars[text_end + 1] != '(' {
        return None;
    }
    let url_end = find_closing_single(chars, text_end + 2, ')')?;
    let text: String = chars[start + 1..text_end].iter().collect();
    let url: String = chars[text_end + 2..url_end].iter().collect();
    Some((text, url, url_end + 1))
}

/// Try to parse a file path starting at `start` in `chars`.
///
/// Matches:
/// - Absolute paths (`/foo/bar.rs`)
/// - Tilde paths (`~/foo/bar.rs`)
/// - Relative paths (`crates/foo/bar.rs`, `src/main.rs`, `./config.toml`)
///
/// The path must:
/// - Be preceded by a word boundary (space, tab, comma, quote, bracket, colon, or start)
/// - Contain at least one `/`
/// - Have a file extension (`.{1..=12 alnum}`) optionally followed by `:N` or `:N-M`
///
/// Returns `(path_string, end_index_exclusive)` if matched; `None` otherwise.
fn try_parse_filepath(chars: &[char], start: usize) -> Option<(String, usize)> {
    // Must be at a word boundary
    if start > 0
        && !matches!(
            chars[start - 1],
            ' ' | '\t' | ',' | '"' | '\'' | '(' | '[' | ':'
        )
    {
        return None;
    }

    // Must start with /, ~/, ./, or an alphanumeric/underscore (for relative paths)
    let ok_start = chars[start] == '/'
        || (chars[start] == '~' && chars.get(start + 1) == Some(&'/'))
        || (chars[start] == '.' && chars.get(start + 1) == Some(&'/'))
        || chars[start].is_alphanumeric()
        || chars[start] == '_';
    if !ok_start {
        return None;
    }

    // Collect path chars: stop at whitespace or prose-terminating punctuation
    let mut end = start;
    while end < chars.len() {
        match chars[end] {
            ' ' | '\t' | '\n' | ',' | '"' | '\'' | ')' | ']' | '>' | '<' | ';' => break,
            _ => end += 1,
        }
    }

    if end == start {
        return None;
    }

    // Build owned string, stripping trailing punctuation attached in prose
    let mut path: String = chars[start..end].iter().collect();
    while path.ends_with(|c: char| matches!(c, '.' | ':' | '!' | '?')) {
        path.pop();
        end -= 1;
    }

    // Minimum length check
    if path.len() < 2 {
        return None;
    }

    // Must contain at least one slash (filters out plain words)
    if !path.contains('/') {
        return None;
    }

    // Must have a file extension: strip any :N or :N-M line-number suffix first
    let base = path.split(':').next().unwrap_or(&path);
    let has_extension = base.rfind('.').is_some_and(|dot| {
        let ext = &base[dot + 1..];
        !ext.is_empty() && ext.len() <= 12 && ext.chars().all(|c| c.is_alphanumeric() || c == '_')
    });

    if !has_extension {
        return None;
    }

    Some((path, end))
}

/// Try to parse a bare (non-markdown-link) URL starting at `start` in `chars`.
///
/// Matches `http://...` or `https://...`, stopping at whitespace or
/// prose-terminating punctuation, with trailing sentence punctuation
/// stripped so a period ending a sentence doesn't become part of the URL.
///
/// Returns `(url_string, end_index_exclusive)` if matched; `None` otherwise.
fn try_parse_bare_url(chars: &[char], start: usize) -> Option<(String, usize)> {
    // Must be at a word boundary, same rule as `try_parse_filepath`.
    if start > 0
        && !matches!(
            chars[start - 1],
            ' ' | '\t' | ',' | '"' | '\'' | '(' | '[' | ':'
        )
    {
        return None;
    }

    let remaining: String = chars[start..].iter().collect();
    let prefix_len = if remaining.starts_with("https://") {
        8
    } else if remaining.starts_with("http://") {
        7
    } else {
        return None;
    };

    let mut end = start + prefix_len;
    while end < chars.len() {
        match chars[end] {
            ' ' | '\t' | '\n' | ',' | '"' | '\'' | ')' | ']' | '>' | '<' | ';' | '`' | '*' => {
                break;
            }
            _ => end += 1,
        }
    }

    // Must have something after the scheme (a host).
    if end <= start + prefix_len {
        return None;
    }

    let mut url: String = chars[start..end].iter().collect();
    while url.ends_with(|c: char| matches!(c, '.' | ':' | '!' | '?')) {
        url.pop();
        end -= 1;
    }

    if end <= start + prefix_len {
        return None;
    }

    Some((url, end))
}

/// Check if a string looks like a file path (for backtick-interior detection).
///
/// Lighter check than `try_parse_filepath` — used to decide if backtick content
/// should be styled as a path rather than generic inline code.
fn looks_like_filepath(text: &str) -> bool {
    if text.len() < 2 {
        return false;
    }
    if !text.contains('/') && !text.contains('.') {
        return false;
    }
    // Reject URLs
    if text.starts_with("http://") || text.starts_with("https://") || text.starts_with("ftp://") {
        return false;
    }
    // Must have a file extension: strip optional :N suffix
    let base = text.split(':').next().unwrap_or(text);
    base.rfind('.').is_some_and(|dot| {
        let ext = &base[dot + 1..];
        !ext.is_empty() && ext.len() <= 12 && ext.chars().all(|c| c.is_alphanumeric() || c == '_')
    })
}

/// Build styled button `Line`s from pipeline command strings.
///
/// Takes pre-derived command strings (from `pipeline_artifact` or legacy extraction)
/// and renders one styled button `Line` per command.
/// Returns an empty vec if no commands are provided.
pub fn build_docregblock_button_lines(commands: &[String]) -> Vec<Line<'static>> {
    commands
        .iter()
        .map(|inner| {
            let label = docregblock_label(inner);
            let (fg, bg) = if is_question_indicator(inner) {
                (
                    super::theme::question_indicator_fg(),
                    super::theme::question_indicator_bg(),
                )
            } else {
                (
                    super::theme::docregblock_fg(),
                    super::theme::docregblock_bg(),
                )
            };
            Line::from(Span::styled(
                label,
                Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
            ))
        })
        .collect()
}

/// Rendering context for building event lines.
#[derive(Debug, Clone)]
pub struct EventRenderContext {
    pub is_collapsed: bool,
    pub is_expanded: bool,
    pub is_cursor: bool,
    pub is_last_event: bool,
    /// Short model name for this event (e.g. "Opus 4.6"), looked up from ModelSegments.
    pub model_name: Option<String>,
    /// Pipeline commands derived from session's pipeline_artifact.
    /// Only used when is_last_event is true.
    pub pipeline_commands: Vec<String>,
    /// Available content width in columns. Used to constrain table rendering
    /// so wide tables are truncated to fit rather than wrapping.
    pub max_width: u16,
}

/// Look up which model produced an event by searching model segments for the
/// segment whose range covers the given sequence number. Falls back to the
/// session model when no segment has been recorded yet.
pub fn model_for_sequence(
    segments: &[rsi_common::types::ModelSegment],
    sequence: i32,
    fallback_model: Option<&str>,
) -> Option<String> {
    segments
        .iter()
        .rev()
        .find(|seg| {
            sequence >= seg.from_sequence && seg.to_sequence.map_or(true, |to| sequence <= to)
        })
        .map(|seg| seg.model_id.as_str())
        .or(fallback_model)
        .map(crate::ui::session::abbreviate_model_name)
}

/// Build the `Vec<Line>` for a conversation event, including headers, metadata,
/// markdown content, docregblock buttons, truncation, and cursor highlighting.
pub fn build_event_lines(
    event: &ConversationEvent,
    ctx: &EventRenderContext,
) -> Vec<Line<'static>> {
    build_event_lines_with_meta(event, ctx).0
}

pub fn build_event_lines_with_meta(
    event: &ConversationEvent,
    ctx: &EventRenderContext,
) -> (Vec<Line<'static>>, Vec<crate::types::CodeBlockRange>) {
    let (lines, code_blocks, _, _) = build_event_lines_with_interaction_meta(event, ctx);
    (lines, code_blocks)
}

pub(crate) fn build_event_lines_with_interaction_meta(
    event: &ConversationEvent,
    ctx: &EventRenderContext,
) -> (
    Vec<Line<'static>>,
    Vec<crate::types::CodeBlockRange>,
    Vec<crate::types::FileLinkTarget>,
    Vec<crate::types::WebLinkTarget>,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut code_blocks: Vec<crate::types::CodeBlockRange> = Vec::new();
    let mut file_links: Vec<crate::types::FileLinkTarget> = Vec::new();
    let is_message = event.event_type == EventType::Message;
    let mut web_links: Vec<crate::types::WebLinkTarget> = Vec::new();

    let role_color = match event.role {
        Some(Role::Assistant) => super::theme::assistant_role(),
        Some(Role::User) => super::theme::user_role(),
        None => super::theme::overlay1(),
        Some(_) => super::theme::overlay1(),
    };

    if ctx.is_collapsed {
        let summary = match event.event_type {
            EventType::ToolUse => {
                let tool = event.tool_name.as_deref().unwrap_or("unknown");
                format!("\u{25B6} {tool}")
            }
            EventType::ToolResult => "\u{25B6} \u{2190} Result".to_string(),
            _ => format!("#{}", event.sequence),
        };
        lines.push(Line::default()); // top padding row
        lines.push(Line::from(Span::styled(
            summary,
            Style::default().fg(super::theme::fold_indicator()),
        )));
        lines.push(Line::default());
        lines.push(Line::default()); // bottom padding row
        return (lines, code_blocks, file_links, web_links);
    }

    let role_label = match event.role {
        Some(Role::Assistant) => ctx.model_name.as_deref().unwrap_or("Assistant"),
        Some(Role::User) => "You",
        None => "",
        Some(_) => "",
    };

    let time_str = event
        .created_at
        .with_timezone(&chrono::Local)
        .format("%H:%M")
        .to_string();
    let date_str = event
        .created_at
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string();

    if !is_message {
        lines.push(Line::default()); // top padding row for bordered event cards
    }

    let mut header_spans = vec![];
    header_spans.push(Span::styled(
        format!("#{}", event.sequence),
        Style::default().fg(super::theme::seq_and_time()),
    ));
    if !role_label.is_empty() {
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(
            role_label.to_string(),
            Style::default().fg(role_color).add_modifier(Modifier::BOLD),
        ));
    }
    header_spans.push(Span::styled(
        format!("  {time_str}  {date_str}"),
        Style::default().fg(super::theme::seq_and_time()),
    ));
    lines.push(Line::from(header_spans));

    match event.event_type {
        EventType::ToolUse => {
            let tool_display = if let Some(ref name) = event.tool_name {
                format!("\u{1F527} {}", name)
            } else {
                "\u{1F527} (unknown)".to_string()
            };
            lines.push(Line::from(Span::styled(
                tool_display,
                Style::default()
                    .fg(super::theme::tool_name())
                    .add_modifier(Modifier::BOLD),
            )));
            if let Some(ref input) = event.tool_input {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                // Limit display to viewport width minus "  " prefix (2 cols).
                // Falls back to 120 if max_width is too small to be useful.
                let display_limit = if ctx.max_width > 5 {
                    (ctx.max_width as usize).saturating_sub(2)
                } else {
                    120
                };
                let truncated = if input_str.chars().count() > display_limit {
                    let byte_end = input_str
                        .char_indices()
                        .nth(display_limit.saturating_sub(1))
                        .map(|(i, _)| i)
                        .unwrap_or(input_str.len());
                    format!("  {}\u{2026}", &input_str[..byte_end])
                } else {
                    format!("  {}", input_str)
                };
                lines.push(Line::from(Span::styled(
                    truncated,
                    Style::default().fg(super::theme::tool_input()),
                )));
            }
        }
        EventType::ToolResult => {
            lines.push(Line::from(Span::styled(
                "\u{2190} Result",
                Style::default().fg(super::theme::tool_result()),
            )));
        }
        EventType::System => {
            lines.push(Line::from(Span::styled(
                "\u{2139} System",
                Style::default().fg(super::theme::system_event()),
            )));
        }
        EventType::Message => {}
        EventType::Thinking => {
            lines.push(Line::from(Span::styled(
                "\u{1F4AD} Thinking",
                Style::default()
                    .fg(super::theme::fold_indicator())
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        EventType::Compressed => {
            lines.push(Line::from(Span::styled(
                "\u{1F4AC} Compressed",
                Style::default()
                    .fg(super::theme::fold_indicator())
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        _ => {}
    }

    let indent = markdown_event_indent(event.event_type);

    let content_start = lines.len();

    let segments = parse_content(&event.content);
    for segment in &segments {
        match segment {
            ContentSegment::Text(text) => {
                append_markdown_text_lines(
                    &mut lines,
                    &mut file_links,
                    &mut web_links,
                    text,
                    indent,
                    ctx.max_width,
                );
            }
            ContentSegment::Code {
                language,
                content: code,
            } => {
                if language.is_some_and(is_markdown_fence_language) {
                    append_markdown_text_lines(
                        &mut lines,
                        &mut file_links,
                        &mut web_links,
                        code,
                        indent,
                        ctx.max_width,
                    );
                    continue;
                }

                let start_line = lines.len();
                if let Some(lang) = language {
                    lines.push(Line::from(vec![Span::styled(
                        format!(
                            "{}\u{2500}\u{2500}\u{2500} {} \u{2500}\u{2500}\u{2500}",
                            indent, lang
                        ),
                        Style::default().fg(super::theme::code_fence_lang()),
                    )]));
                }
                let highlighted = super::highlight::highlight_code(code, *language);
                for mut hl_line in highlighted {
                    // Prepend indent if needed
                    if !indent.is_empty() {
                        let mut spans = vec![Span::raw(indent.to_string())];
                        spans.append(&mut hl_line.spans);
                        hl_line = Line::from(spans);
                    }
                    // Apply code block background to all spans
                    let code_bg = super::theme::code_block_bg();
                    let bg_spans: Vec<Span<'static>> = hl_line
                        .spans
                        .into_iter()
                        .map(|s| {
                            if s.style.bg.is_none() {
                                Span::styled(s.content, s.style.bg(code_bg))
                            } else {
                                s
                            }
                        })
                        .collect();
                    lines.push(Line::from(bg_spans));
                }
                let end_line = lines.len();
                code_blocks.push(crate::types::CodeBlockRange {
                    start_line,
                    end_line,
                    content: code.to_string(),
                });
            }
        }
    }

    if ctx.is_last_event {
        let button_lines = build_docregblock_button_lines(&ctx.pipeline_commands);
        for btn_line in button_lines {
            lines.push(btn_line);
        }
    }

    let content_lines_count = lines.len().saturating_sub(content_start);
    let mut truncated = false;
    if !ctx.is_expanded && ctx.is_collapsed && content_lines_count > MAX_CONTENT_LINES {
        let remaining = content_lines_count - MAX_CONTENT_LINES;
        lines.truncate(content_start + MAX_CONTENT_LINES);
        lines.push(Line::from(Span::styled(
            format!("{}  \u{2026} {} more lines", indent, remaining),
            Style::default()
                .fg(super::theme::fold_indicator())
                .add_modifier(Modifier::ITALIC),
        )));
        truncated = true;
    }

    if truncated {
        let limit = content_start + MAX_CONTENT_LINES;
        code_blocks.retain_mut(|cb| {
            if cb.start_line >= limit {
                false
            } else {
                if cb.end_line > limit {
                    cb.end_line = limit;
                }
                true
            }
        });
    }

    if !is_message {
        lines.push(Line::default()); // bottom padding row for bordered event cards
    }

    (lines, code_blocks, file_links, web_links)
}

fn markdown_event_indent(event_type: EventType) -> &'static str {
    match event_type {
        EventType::ToolUse
        | EventType::ToolResult
        | EventType::System
        | EventType::Thinking
        | EventType::Compressed => "  ",
        EventType::Message => "",
        _ => "",
    }
}

fn is_markdown_fence_language(language: &str) -> bool {
    matches!(
        language
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "markdown" | "md" | "mdx"
    )
}

fn append_markdown_text_lines(
    lines: &mut Vec<Line<'static>>,
    file_links: &mut Vec<crate::types::FileLinkTarget>,
    web_links: &mut Vec<crate::types::WebLinkTarget>,
    text: &str,
    indent: &str,
    max_width: u16,
) {
    let mut table_buf: Vec<(BlockElement, &str)> = Vec::new();
    for content_line in text.lines() {
        let leading_spaces = content_line.len() - content_line.trim_start().len();
        let nest_level = leading_spaces / 2;
        let (block, block_content) = detect_block_element(content_line);
        match block {
            BlockElement::TableRow | BlockElement::TableSeparator => {
                table_buf.push((block, block_content));
            }
            _ => {
                if !table_buf.is_empty() {
                    flush_markdown_table_buffer(
                        lines,
                        file_links,
                        web_links,
                        &mut table_buf,
                        indent,
                        max_width,
                    );
                }

                if matches!(block, BlockElement::Header(_) | BlockElement::Blockquote)
                    && !lines.is_empty()
                    && lines.last().is_some_and(|l| !l.spans.is_empty())
                {
                    lines.push(Line::default());
                }

                let effective_indent = if nest_level > 0
                    && matches!(
                        block,
                        BlockElement::UnorderedList
                            | BlockElement::OrderedList(_)
                            | BlockElement::TaskUnchecked
                            | BlockElement::TaskChecked
                    ) {
                    format!("{}{}", indent, "  ".repeat(nest_level))
                } else {
                    indent.to_string()
                };

                let hanging_width: usize = match &block {
                    BlockElement::UnorderedList
                    | BlockElement::TaskUnchecked
                    | BlockElement::TaskChecked
                    | BlockElement::Blockquote => 2,
                    BlockElement::OrderedList(n) => format!("{}. ", n).chars().count(),
                    _ => 0,
                };

                let effective_wrap_width = if max_width > 0 {
                    let available = (max_width as usize)
                        .saturating_sub(effective_indent.chars().count())
                        .saturating_sub(hanging_width)
                        .max(20);
                    available.min(MAX_PROSE_WIDTH)
                } else {
                    MAX_PROSE_WIDTH
                };

                let hanging_indent = " ".repeat(hanging_width);
                let cont_indent = format!("{}{}", effective_indent, hanging_indent);

                // HorizontalRule: render width-aware ─── line instead of hardcoded 60-char one.
                if matches!(block, BlockElement::HorizontalRule) {
                    let indent_cols = effective_indent.chars().count();
                    let hr_width = if max_width > 0 {
                        (max_width as usize).saturating_sub(indent_cols).max(3)
                    } else {
                        60usize.saturating_sub(indent_cols).max(3)
                    };
                    let mut hr_spans: Vec<Span<'static>> = Vec::new();
                    if !effective_indent.is_empty() {
                        hr_spans.push(Span::raw(effective_indent.to_string()));
                    }
                    hr_spans.push(Span::styled(
                        "\u{2500}".repeat(hr_width),
                        Style::default().fg(super::theme::md_hr()),
                    ));
                    lines.push(Line::from(hr_spans));
                    continue;
                }

                let wrapped = word_wrap(block_content, effective_wrap_width);
                for (i, chunk) in wrapped.iter().enumerate() {
                    let line = lines.len();
                    let (file_targets, web_targets) = scan_link_targets(chunk);
                    file_links.extend(
                        file_targets
                            .into_iter()
                            .map(|target| crate::types::FileLinkTarget { line, target }),
                    );
                    web_links.extend(
                        web_targets
                            .into_iter()
                            .map(|target| crate::types::WebLinkTarget { line, target }),
                    );
                    if i == 0 {
                        lines.push(render_markdown_line(&effective_indent, &block, chunk));
                    } else {
                        lines.push(render_markdown_line(
                            &cont_indent,
                            &BlockElement::None,
                            chunk,
                        ));
                    }
                }
            }
        }
    }

    if !table_buf.is_empty() {
        flush_markdown_table_buffer(
            lines,
            file_links,
            web_links,
            &mut table_buf,
            indent,
            max_width,
        );
    }
}

fn flush_markdown_table_buffer(
    lines: &mut Vec<Line<'static>>,
    file_links: &mut Vec<crate::types::FileLinkTarget>,
    web_links: &mut Vec<crate::types::WebLinkTarget>,
    table_buf: &mut Vec<(BlockElement, &str)>,
    indent: &str,
    max_width: u16,
) {
    if is_valid_table_block(table_buf) {
        lines.extend(render_table_block(indent, table_buf, max_width));
        table_buf.clear();
        return;
    }

    let wrap_width = if max_width > 0 {
        (max_width as usize)
            .saturating_sub(indent.chars().count())
            .max(1)
    } else {
        MAX_PROSE_WIDTH
    };
    for (_, source_line) in table_buf.iter() {
        for chunk in word_wrap(source_line, wrap_width) {
            let line = lines.len();
            let (file_targets, web_targets) = scan_link_targets(&chunk);
            file_links.extend(
                file_targets
                    .into_iter()
                    .map(|target| crate::types::FileLinkTarget { line, target }),
            );
            web_links.extend(
                web_targets
                    .into_iter()
                    .map(|target| crate::types::WebLinkTarget { line, target }),
            );
            lines.push(render_markdown_line(indent, &BlockElement::None, &chunk));
        }
    }
    table_buf.clear();
}

/// Scan wrapped text for file-path and web-link click targets, to record as
/// click-time metadata (`FileLinkTarget`/`WebLinkTarget`) alongside the
/// visible spans `parse_inline_markdown` renders. Mirrors that function's
/// construct detection (backtick spans, `[text](url)` links, bare URLs) —
/// same classification rule (`looks_like_filepath`) so metadata always
/// agrees with what's actually styled as clickable on screen — but only
/// needs the target strings, not spans.
///
/// Returns `(file_targets, web_targets)`.
fn scan_link_targets(text: &str) -> (Vec<String>, Vec<String>) {
    let chars: Vec<char> = text.chars().collect();
    let mut file_targets = Vec::new();
    let mut web_targets = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`'
            && let Some(end) = find_closing_single(&chars, i + 1, '`')
        {
            i = end + 1;
        } else if chars[i] == '['
            && let Some((_, url, end)) = parse_link(&chars, i)
        {
            if looks_like_filepath(&url) {
                file_targets.push(url);
            } else {
                web_targets.push(url);
            }
            i = end;
        } else if chars[i] == 'h'
            && let Some((url, end)) = try_parse_bare_url(&chars, i)
        {
            web_targets.push(url);
            i = end;
        } else {
            i += 1;
        }
    }
    (file_targets, web_targets)
}

/// Maximum width for prose text lines (not code or tables).
/// Prose wraps at this width to improve readability on wide terminals.
/// 120 chars is the sweet spot — wide enough for technical prose but
/// narrow enough to prevent eye-scanning fatigue on ultrawide monitors.
pub const MAX_PROSE_WIDTH: usize = 100;

/// Return the byte ranges of inline Markdown constructs that should not be
/// split at a whitespace boundary.
///
/// `word_wrap` runs before inline Markdown is parsed into ratatui spans. If a
/// link such as `[slice board](...)` is split at the space in its label, the
/// two resulting lines can no longer be parsed as one link. Keep the inline
/// constructs whole while choosing a word-wrap boundary; an oversized
/// construct is emitted as one logical line and left to ratatui's renderer to
/// clip/wrap visually.
fn markdown_atomic_ranges(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let offsets: Vec<usize> = text
        .char_indices()
        .map(|(byte_offset, _)| byte_offset)
        .chain(std::iter::once(text.len()))
        .collect();
    let mut ranges = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let end = if chars[i] == '[' {
            parse_link(&chars, i).map(|(_, _, end)| end)
        } else if chars[i] == '`' {
            find_closing_single(&chars, i + 1, '`').map(|end| end + 1)
        } else if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            find_closing(&chars, i + 2, &['*', '*']).map(|end| end + 2)
        } else if i + 1 < chars.len() && chars[i] == '_' && chars[i + 1] == '_' {
            find_closing(&chars, i + 2, &['_', '_']).map(|end| end + 2)
        } else if i + 1 < chars.len() && chars[i] == '~' && chars[i + 1] == '~' {
            find_closing(&chars, i + 2, &['~', '~']).map(|end| end + 2)
        } else if chars[i] == '*' || chars[i] == '_' {
            find_closing_single(&chars, i + 1, chars[i]).map(|end| end + 1)
        } else {
            None
        };

        if let Some(end) = end {
            ranges.push((offsets[i], offsets[end]));
            i = end;
        } else {
            i += 1;
        }
    }

    ranges
}

/// Split text into whitespace-separated words without breaking inside an
/// inline Markdown construct. The returned slices retain their original
/// Markdown syntax for the later styling pass.
fn markdown_words(text: &str) -> Vec<&str> {
    let ranges = markdown_atomic_ranges(text);
    let mut words = Vec::new();
    let mut word_start = 0;

    for (offset, ch) in text.char_indices() {
        if ch == ' '
            && !ranges
                .iter()
                .any(|(start, end)| offset >= *start && offset < *end)
        {
            if word_start < offset {
                words.push(&text[word_start..offset]);
            }
            word_start = offset + 1;
        }
    }

    if word_start < text.len() {
        words.push(&text[word_start..]);
    }

    words
}

/// Measure the text that will actually be visible after inline Markdown is
/// parsed. In particular, local file-link targets are intentionally absent
/// from the rendered spans and must not consume wrapping width.
fn markdown_display_width(text: &str) -> usize {
    parse_inline_markdown(text)
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

fn is_atomic_markdown_word(word: &str) -> bool {
    markdown_atomic_ranges(word)
        .iter()
        .any(|(start, end)| *start == 0 && *end == word.len())
}

fn hard_break_word(word: &str, max_width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut remaining = word;
    while remaining.chars().count() > max_width {
        let byte_end = remaining
            .char_indices()
            .nth(max_width)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        chunks.push(remaining[..byte_end].to_string());
        remaining = &remaining[byte_end..];
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }
    chunks
}

/// Word-wrap a string to fit within `max_width` **characters** (Unicode scalar values).
/// Breaks at word boundaries (spaces) when possible, hard-breaks otherwise.
///
/// All comparisons use `chars().count()` so multi-byte Unicode (box-drawing chars,
/// em-dashes, arrows, etc.) is measured by visual column count, not byte length.
pub fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || markdown_display_width(text) <= max_width {
        return vec![text.to_string()];
    }

    let words = markdown_words(text);
    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for word in words {
        let word_width = markdown_display_width(word);

        if current.is_empty() {
            if word_width > max_width && !is_atomic_markdown_word(word) {
                let chunks = hard_break_word(word, max_width);
                for chunk in chunks {
                    if current.is_empty() {
                        current_width = markdown_display_width(&chunk);
                        current = chunk;
                    } else {
                        result.push(std::mem::take(&mut current));
                        current_width = 0;
                        current = chunk;
                    }
                }
            } else {
                current_width = word_width;
                current.push_str(word);
            }
        } else if current_width + 1 + word_width <= max_width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            result.push(std::mem::take(&mut current));
            current_width = word_width;
            current.push_str(word);
        }
    }

    if !current.is_empty() {
        result.push(current);
    }

    if result.is_empty() {
        result.push(String::new());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    // --- parse_inline_markdown tests ---

    #[test]
    fn test_parse_inline_plain_text() {
        let spans = parse_inline_markdown("hello world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "hello world");
    }

    #[test]
    fn test_parse_inline_bold() {
        let spans = parse_inline_markdown("before **bold** after");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), "before ");
        assert_eq!(spans[1].content.as_ref(), "bold");
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].content.as_ref(), " after");
    }

    #[test]
    fn test_parse_inline_italic() {
        let spans = parse_inline_markdown("before *italic* after");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content.as_ref(), "italic");
        assert!(spans[1].style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn test_parse_inline_code() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let spans = parse_inline_markdown("before `code` after");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content.as_ref(), "code");
        assert_eq!(
            spans[1].style.fg,
            Some(super::super::theme::md_inline_code_fg())
        );
        assert_eq!(
            spans[1].style.bg,
            Some(super::super::theme::md_inline_code_bg())
        );
    }

    #[test]
    fn test_parse_inline_link() {
        let spans = parse_inline_markdown("see [docs](https://example.com) here");
        // Expected: "see " + styled "docs" + " (https://example.com)" + " here"
        assert_eq!(spans.len(), 6);
        assert_eq!(spans[0].content.as_ref(), "see ");
        assert_eq!(spans[1].content.as_ref(), "docs");
        assert!(!spans[1].content.contains('\x1b'));
        // spans[3] is the URL in parens
        assert!(spans[3].content.contains("https://example.com"));
        assert_eq!(spans[5].content.as_ref(), " here");
    }

    #[test]
    fn test_parse_inline_file_link_shows_only_label() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let spans = parse_inline_markdown("see [slice board](/tmp/pending-slices-board.md:267)");
        let label_span = spans
            .iter()
            .find(|span| span.content == "slice board")
            .expect("file-link label should be rendered");

        assert_eq!(label_span.style.fg, Some(crate::ui::theme::file_path_fg()));
        assert!(label_span.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(
            !spans
                .iter()
                .any(|span| span.content.contains("pending-slices-board.md"))
        );
        assert!(spans.iter().all(|span| !span.content.contains('\x1b')));
    }

    #[test]
    fn test_file_link_metadata_preserves_hidden_target() {
        let event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: "See [slice board](/tmp/pending-slices-board.md:267).".to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let ctx = EventRenderContext {
            is_collapsed: false,
            is_expanded: false,
            is_cursor: false,
            is_last_event: false,
            model_name: None,
            pipeline_commands: Vec::new(),
            max_width: 100,
        };

        let (lines, _, file_links, _web_links) =
            build_event_lines_with_interaction_meta(&event, &ctx);
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .all(|span| !span.content.contains("pending-slices-board.md"))
        );
        assert_eq!(file_links.len(), 1);
        assert_eq!(file_links[0].line, 1);
        assert_eq!(file_links[0].target, "/tmp/pending-slices-board.md:267");
    }

    #[test]
    fn test_web_link_metadata_records_markdown_link_target() {
        let event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: "See [PR #51](https://github.com/jakedevar/rsi/pull/51) for detail."
                .to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let ctx = EventRenderContext {
            is_collapsed: false,
            is_expanded: false,
            is_cursor: false,
            is_last_event: false,
            model_name: None,
            pipeline_commands: Vec::new(),
            max_width: 100,
        };

        let (_, _, file_links, web_links) = build_event_lines_with_interaction_meta(&event, &ctx);
        assert!(file_links.is_empty());
        assert_eq!(web_links.len(), 1);
        assert_eq!(
            web_links[0].target,
            "https://github.com/jakedevar/rsi/pull/51"
        );
    }

    #[test]
    fn test_web_link_metadata_records_bare_url() {
        let event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: "Docs are at https://example.com/docs/page.html, check it out.".to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let ctx = EventRenderContext {
            is_collapsed: false,
            is_expanded: false,
            is_cursor: false,
            is_last_event: false,
            model_name: None,
            pipeline_commands: Vec::new(),
            max_width: 100,
        };

        let (_, _, file_links, web_links) = build_event_lines_with_interaction_meta(&event, &ctx);
        assert!(file_links.is_empty());
        assert_eq!(web_links.len(), 1);
        // Trailing comma must not be swallowed into the recorded target.
        assert_eq!(web_links[0].target, "https://example.com/docs/page.html");
    }

    #[test]
    fn test_parse_inline_bare_url_is_styled_and_underlined() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let spans = parse_inline_markdown("check https://example.com/foo for details");
        let url_span = spans
            .iter()
            .find(|span| span.content.as_ref() == "https://example.com/foo")
            .expect("bare URL should be rendered as its own span");
        assert_eq!(url_span.style.fg, Some(crate::ui::theme::md_link_text()));
        assert!(url_span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_parse_inline_bare_url_strips_trailing_sentence_punctuation() {
        let spans = parse_inline_markdown("see https://example.com/page.");
        let url_span = spans
            .iter()
            .find(|span| span.content.as_ref().starts_with("https://"))
            .expect("bare URL should be rendered as its own span");
        assert_eq!(url_span.content.as_ref(), "https://example.com/page");
        // The trailing period stays in the text as a separate plain span.
        assert!(spans.iter().any(|span| span.content.as_ref() == "."));
    }

    #[test]
    fn test_parse_inline_unmatched_delimiter() {
        // Unmatched ** should be passed through as plain text
        let spans = parse_inline_markdown("before **unmatched");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "before **unmatched");
    }

    #[test]
    fn test_parse_inline_outermost_wins() {
        // Nested: bold arm recurses, so inner backtick renders as inline code with BOLD applied
        let spans = parse_inline_markdown("**bold `code` bold**");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), "bold ");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].content.as_ref(), "code");
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].content.as_ref(), " bold");
        assert!(spans[2].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_parse_inline_empty() {
        let spans = parse_inline_markdown("");
        assert!(spans.is_empty());
    }

    #[test]
    fn test_parse_inline_adjacent_elements() {
        let spans = parse_inline_markdown("**bold***italic*");
        assert_eq!(spans.len(), 2);
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(spans[1].style.add_modifier.contains(Modifier::ITALIC));
    }

    // --- file path detection tests ---

    #[test]
    fn test_parse_inline_absolute_path() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let spans = parse_inline_markdown("see /home/jake/file.rs for details");
        // Should be: "see " | "/home/jake/file.rs" (styled) | " for details"
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content.as_ref(), "/home/jake/file.rs");
        assert!(spans[1].style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(spans[1].style.fg, Some(crate::ui::theme::file_path_fg()));
    }

    #[test]
    fn test_parse_inline_tilde_path() {
        let spans = parse_inline_markdown("saved to ~/.fractal/state.json");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content.as_ref(), "~/.fractal/state.json");
        assert!(spans[1].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_parse_inline_path_with_line_number() {
        let spans = parse_inline_markdown("error at /src/main.rs:42");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content.as_ref(), "/src/main.rs:42");
    }

    #[test]
    fn test_parse_inline_path_no_false_positive_division() {
        // "a/b" without leading / should NOT be detected
        let spans = parse_inline_markdown("input/output");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "input/output");
        assert_eq!(spans[0].style.fg, None); // no special styling
    }

    #[test]
    fn test_parse_inline_path_no_false_positive_no_extension() {
        // /usr/local/bin has no extension — should not be detected
        let spans = parse_inline_markdown("/usr/local/bin");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, None);
    }

    #[test]
    fn test_parse_inline_path_with_trailing_comma() {
        // Comma after path should be stripped from the path itself
        let spans = parse_inline_markdown("/home/jake/a.rs, and more");
        let path_span = spans.iter().find(|s| s.content.contains(".rs")).unwrap();
        assert_eq!(path_span.content.as_ref(), "/home/jake/a.rs");
    }

    #[test]
    fn test_parse_inline_path_inside_backtick_styled_as_path() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        // Paths inside backticks should be styled as paths (sapphire + underline)
        let spans = parse_inline_markdown("`/home/jake/file.rs`");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "/home/jake/file.rs");
        assert_eq!(spans[0].style.fg, Some(crate::ui::theme::file_path_fg()));
        assert!(spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
        // Should also keep code bg for visual consistency
        assert_eq!(
            spans[0].style.bg,
            Some(crate::ui::theme::md_inline_code_bg())
        );
    }

    #[test]
    fn test_parse_inline_backtick_relative_path_styled_as_path() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let spans = parse_inline_markdown("`crates/fractal/src/main.rs`");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "crates/fractal/src/main.rs");
        assert_eq!(spans[0].style.fg, Some(crate::ui::theme::file_path_fg()));
        assert!(spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_parse_inline_backtick_non_path_not_styled_as_path() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        // Non-path backtick content should remain code-styled
        let spans = parse_inline_markdown("`some_variable`");
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].style.fg,
            Some(crate::ui::theme::md_inline_code_fg())
        );
        assert!(!spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_parse_inline_relative_path_bare() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        // Bare relative paths should be detected
        let spans = parse_inline_markdown("see crates/fractal/src/main.rs for details");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content.as_ref(), "crates/fractal/src/main.rs");
        assert_eq!(spans[1].style.fg, Some(crate::ui::theme::file_path_fg()));
        assert!(spans[1].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_parse_inline_relative_path_dot_slash() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let spans = parse_inline_markdown("edit ./config.toml");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content.as_ref(), "./config.toml");
        assert_eq!(spans[1].style.fg, Some(crate::ui::theme::file_path_fg()));
    }

    #[test]
    fn test_parse_inline_url_not_detected_as_path() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        // URLs inside backticks should NOT be styled as paths
        let spans = parse_inline_markdown("`https://example.com/path.html`");
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].style.fg,
            Some(crate::ui::theme::md_inline_code_fg())
        );
        assert!(!spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    // --- detect_block_element tests ---

    #[test]
    fn test_detect_header() {
        let (block, content) = detect_block_element("## Hello World");
        assert_eq!(block, BlockElement::Header(2));
        assert_eq!(content, "Hello World");
    }

    #[test]
    fn test_detect_blockquote() {
        let (block, content) = detect_block_element("> quoted text");
        assert_eq!(block, BlockElement::Blockquote);
        assert_eq!(content, "quoted text");
    }

    #[test]
    fn test_detect_unordered_list() {
        let (block, content) = detect_block_element("- list item");
        assert_eq!(block, BlockElement::UnorderedList);
        assert_eq!(content, "list item");
    }

    #[test]
    fn test_detect_ordered_list() {
        let (block, content) = detect_block_element("3. third item");
        assert_eq!(block, BlockElement::OrderedList(3));
        assert_eq!(content, "third item");
    }

    #[test]
    fn test_detect_hr() {
        let (block, _) = detect_block_element("---");
        assert_eq!(block, BlockElement::HorizontalRule);
        let (block2, _) = detect_block_element("***");
        assert_eq!(block2, BlockElement::HorizontalRule);
        let (block3, _) = detect_block_element("___");
        assert_eq!(block3, BlockElement::HorizontalRule);
    }

    #[test]
    fn test_detect_table_row() {
        let (block, content) = detect_block_element("| col1 | col2 |");
        assert_eq!(block, BlockElement::TableRow);
        assert_eq!(content, "| col1 | col2 |");
    }

    #[test]
    fn test_detect_table_separator() {
        let (block, content) = detect_block_element("|---|:---:|");
        assert_eq!(block, BlockElement::TableSeparator);
        assert_eq!(content, "|---|:---:|");

        let (short_rule, content) = detect_block_element("|-|--|");
        assert_eq!(short_rule, BlockElement::TableSeparator);
        assert_eq!(content, "|-|--|");

        let (no_trailing_pipe, content) = detect_block_element("| --- | ---");
        assert_eq!(no_trailing_pipe, BlockElement::TableSeparator);
        assert_eq!(content, "| --- | ---");

        let (trailing_spaces, content) = detect_block_element("| --- | --- |  ");
        assert_eq!(trailing_spaces, BlockElement::TableSeparator);
        assert_eq!(content, "| --- | --- |  ");
    }

    #[test]
    fn test_detect_plain() {
        let (block, content) = detect_block_element("just plain text");
        assert_eq!(block, BlockElement::None);
        assert_eq!(content, "just plain text");
    }

    // --- parse_content tests ---

    #[test]
    fn test_plain_text_only() {
        let input = "Hello world\nSecond line";
        let segments = parse_content(input);
        assert_eq!(
            segments,
            vec![ContentSegment::Text("Hello world\nSecond line")]
        );
    }

    #[test]
    fn test_single_code_block() {
        let input = "Before\n```rust\nfn main() {}\n```\nAfter";
        let segments = parse_content(input);
        assert_eq!(
            segments,
            vec![
                ContentSegment::Text("Before"),
                ContentSegment::Code {
                    language: Some("rust"),
                    content: "fn main() {}"
                },
                ContentSegment::Text("After"),
            ]
        );
    }

    #[test]
    fn test_code_block_no_language() {
        let input = "```\nsome code\n```";
        let segments = parse_content(input);
        assert_eq!(
            segments,
            vec![ContentSegment::Code {
                language: None,
                content: "some code"
            },]
        );
    }

    #[test]
    fn test_unclosed_code_block() {
        let input = "Text\n```python\nprint('hi')";
        let segments = parse_content(input);
        assert_eq!(
            segments,
            vec![
                ContentSegment::Text("Text"),
                ContentSegment::Code {
                    language: Some("python"),
                    content: "print('hi')"
                },
            ]
        );
    }

    #[test]
    fn test_multiple_code_blocks() {
        let input = "A\n```js\nlet x = 1;\n```\nB\n```py\ny = 2\n```\nC";
        let segments = parse_content(input);
        assert_eq!(segments.len(), 5);
    }

    #[test]
    fn test_empty_content() {
        let segments = parse_content("");
        assert!(segments.is_empty());
    }

    #[test]
    fn test_word_wrap_multibyte_utf8() {
        // Regression: word_wrap panicked on multi-byte chars (─ is 3 bytes)
        // when max_width fell inside a multi-byte sequence.
        let input = "★ Insight ─────────────────────────────────────";
        let result = word_wrap(input, 80);
        assert!(!result.is_empty());
        // Should not panic — that's the main assertion
    }

    #[test]
    fn test_word_wrap_multibyte_exact_boundary() {
        // All 3-byte chars, max_width=4 falls inside the 2nd char (bytes 3..6)
        let input = "────";
        let result = word_wrap(input, 4);
        assert!(!result.is_empty());
        for chunk in &result {
            assert!(chunk.is_char_boundary(chunk.len()));
        }
    }

    #[test]
    fn test_word_wrap_preserves_markdown_links_with_spaced_labels() {
        let input = "The [slice board](/home/jakedevar/rsi/thoughts/shared/orchestration/2026-07-02-pending-slices-board.md:267) and [ledger](/home/jakedevar/rsi/thoughts/shared/projects/local-issue-tracker/ledger.md:49)";
        let wrapped = word_wrap(input, 80);

        // The hidden file targets must not force each short label onto its
        // own line; the visible content fits comfortably on one line.
        assert_eq!(wrapped.len(), 1);
        assert!(wrapped[0].starts_with("The [slice board]("));
        assert!(wrapped[0].contains(") and [ledger]("));

        // A link label containing spaces must stay in one logical line. If it
        // is split before inline parsing, the second fragment is misread as a
        // bare file path and the Markdown link is displayed incorrectly.
        assert!(
            wrapped
                .iter()
                .any(|line| line.contains("[slice board](") && line.contains(":267)"))
        );
        assert!(
            wrapped
                .iter()
                .any(|line| line.contains("[ledger](") && line.contains(":49)"))
        );

        for line in wrapped {
            let spans = parse_inline_markdown(&line);
            assert!(
                !spans.iter().any(|span| span.content.contains("](/home/")),
                "wrapped Markdown was left as raw link syntax: {line:?}"
            );
        }
    }

    // --- docregblock tests ---

    #[test]
    fn test_parse_inline_docregblock() {
        let spans = parse_inline_markdown(
            "before <docregblock>/implement @thoughts/shared/plans/foo.md</docregblock> after",
        );
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), "before ");
        assert!(spans[1].content.contains("implement"));
        assert!(!spans[1].content.contains("plans/foo.md"));
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].content.as_ref(), " after");
    }

    #[test]
    fn test_parse_inline_docregblock_research_path() {
        let spans = parse_inline_markdown(
            "<docregblock>/research @thoughts/shared/research/bar.md</docregblock>",
        );
        assert_eq!(spans.len(), 1);
        assert!(spans[0].content.contains("research"));
        assert!(!spans[0].content.contains("research/bar.md"));
    }

    #[test]
    fn test_parse_inline_docregblock_fallback_label() {
        let spans = parse_inline_markdown("<docregblock>some arbitrary text</docregblock>");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].content.contains("Execute Block"));
    }

    #[test]
    fn test_parse_inline_docregblock_unclosed() {
        // Unclosed tag — should be plain text
        let spans = parse_inline_markdown("before <docregblock>unclosed text");
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].content.as_ref(),
            "before <docregblock>unclosed text"
        );
    }

    #[test]
    fn test_docregblock_label_plans_path() {
        let label = docregblock_label("/implement @thoughts/shared/plans/2026-02-09-foo.md");
        assert!(label.contains("implement"));
        assert!(!label.contains("plans/2026-02-09-foo.md"));
    }

    #[test]
    fn test_docregblock_label_research_path() {
        let label = docregblock_label("/research @thoughts/shared/research/bar.md");
        assert!(label.contains("research"));
        assert!(!label.contains("research/bar.md"));
    }

    #[test]
    fn test_docregblock_label_slash_command_only() {
        let label = docregblock_label("/my_command");
        assert!(label.contains("my_command"));
    }

    #[test]
    fn test_docregblock_label_generic_fallback() {
        let label = docregblock_label("some random text");
        assert!(label.contains("Execute Block"));
    }

    #[test]
    fn test_empty_code_block() {
        let input = "```\n```";
        let segments = parse_content(input);
        assert_eq!(
            segments,
            vec![ContentSegment::Code {
                language: None,
                content: ""
            },]
        );
    }

    #[test]
    fn test_docregblock_pill_label_implement() {
        let contents = vec!["/implement @thoughts/shared/plans/foo.md".to_string()];
        assert_eq!(docregblock_pill_label(&contents), Some("impl".to_string()));
    }

    #[test]
    fn test_docregblock_pill_label_plan() {
        let contents = vec!["/plan @thoughts/shared/plans/foo.md".to_string()];
        assert_eq!(docregblock_pill_label(&contents), Some("plan".to_string()));
    }

    #[test]
    fn test_docregblock_pill_label_unknown_command() {
        let contents = vec!["/my_custom_command arg1".to_string()];
        let label = docregblock_pill_label(&contents).unwrap();
        assert_eq!(label, "my_c");
    }

    #[test]
    fn test_docregblock_pill_label_generic() {
        let contents = vec!["some random text".to_string()];
        assert_eq!(docregblock_pill_label(&contents), Some("exec".to_string()));
    }

    #[test]
    fn test_docregblock_pill_label_empty() {
        let contents: Vec<String> = vec![];
        assert_eq!(docregblock_pill_label(&contents), None);
    }

    #[test]
    fn test_docregblock_pill_label_merge_ready() {
        let contents = vec!["/merge_ready".to_string()];
        assert_eq!(docregblock_pill_label(&contents), Some("mrg".to_string()));
    }

    #[test]
    fn test_build_docregblock_pill_spans_count() {
        let spans = build_docregblock_pill_spans("impl");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].content.contains("impl"));
    }

    // --- render_table_block tests ---

    #[test]
    fn test_render_table_block_alignment() {
        let rows = vec![
            (BlockElement::TableRow, "| Name | Age |"),
            (BlockElement::TableSeparator, "|------|-----|"),
            (BlockElement::TableRow, "| Alice | 30 |"),
            (BlockElement::TableRow, "| Bob | 5 |"),
        ];
        let lines = render_table_block("", &rows, 200);
        assert_eq!(lines.len(), 6); // top + header + separator + 2 data rows + bottom

        // Extract text content from each line
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        // All lines should have the same char count (aligned columns)
        let widths: Vec<usize> = text.iter().map(|l| l.chars().count()).collect();
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "All table lines should have equal char width, got widths {:?} for: {:?}",
            widths,
            text
        );

        // Separator should use box-drawing characters
        assert!(text[0].starts_with('┌'));
        assert!(text[0].contains('┬'));
        assert!(text[0].ends_with('┐'));
        assert!(text[2].contains('─'));
        assert!(text[2].contains('┼'));
        assert!(text[2].starts_with('├'));
        assert!(text[2].ends_with('┤'));
        assert!(text[5].starts_with('└'));
        assert!(text[5].contains('┴'));
        assert!(text[5].ends_with('┘'));
    }

    #[test]
    fn test_render_table_block_with_indent() {
        let rows = vec![
            (BlockElement::TableRow, "| A | B |"),
            (BlockElement::TableRow, "| CC | D |"),
        ];
        let lines = render_table_block("  ", &rows, 200);
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text[0].starts_with("  ┌"));
        assert!(text[1].starts_with("  │"));
        assert!(text[2].starts_with("  │"));
        assert!(text[3].starts_with("  └"));
    }

    #[test]
    fn test_render_table_block_single_column() {
        let rows = vec![
            (BlockElement::TableRow, "| Solo |"),
            (BlockElement::TableSeparator, "|------|"),
            (BlockElement::TableRow, "| X |"),
        ];
        let lines = render_table_block("", &rows, 200);
        assert_eq!(lines.len(), 5);
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let widths: Vec<usize> = text.iter().map(|l| l.chars().count()).collect();
        assert!(widths.iter().all(|&w| w == widths[0]));
    }

    #[test]
    fn test_render_table_block_inline_markdown_in_cells() {
        let rows = vec![
            (BlockElement::TableRow, "| **bold** | normal |"),
            (BlockElement::TableRow, "| text | `code` |"),
        ];
        let lines = render_table_block("", &rows, 200);
        assert_eq!(lines.len(), 4);
        // Verify bold cell has BOLD modifier
        let bold_span = lines[1].spans.iter().find(|s| s.content.as_ref() == "bold");
        assert!(bold_span.is_some());
        assert!(
            bold_span
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn event_markdown_table_width_matches_rendered_markdown_and_indent() {
        let event = |content: &str, event_type| ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type,
            role: Some(Role::Assistant),
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let table = "| A | Longest |\n|---|---------|\n| mid | value |";

        assert_eq!(
            event_markdown_table_width(&event(table, EventType::Message)),
            Some(17),
            "natural width includes one border and three cells-per-column characters"
        );
        assert_eq!(
            event_markdown_table_width(&event(table, EventType::ToolResult)),
            Some(19),
            "tool-like events render their table with a two-column indent"
        );
        assert_eq!(
            event_markdown_table_width(&event(
                "```markdown\n| **wide** | value |\n|----------|-------|\n```",
                EventType::Message,
            )),
            Some(16),
            "inline Markdown delimiters do not count toward the rendered width"
        );
        assert_eq!(
            event_markdown_table_width(&event("```text\n| A | B |\n```", EventType::Message)),
            None,
            "non-Markdown fenced code does not use the table renderer"
        );
        assert_eq!(
            event_markdown_table_width(&event(
                "plain text\n\n| A | B |\n|---|---|\n\n| **wide** | Longest |\n|----------|---------|",
                EventType::Message,
            )),
            Some(18),
            "the widest table block drives the requested width"
        );
        assert_eq!(
            event_markdown_table_width(&event(
                "| Header one | Header two |\n\nFollowing prose",
                EventType::Message,
            )),
            None,
            "a pipe-delimited line without a separator is not a Markdown table"
        );
        assert_eq!(
            event_markdown_table_width(&event(
                "| A | B |\n| - | - |  \n| 1 | 2 |",
                EventType::Message,
            )),
            Some(9),
            "a separator may omit its trailing edge pipe"
        );
    }

    #[test]
    fn delimiterless_pipe_row_renders_as_literal_text() {
        let source = "| Content / category | Canonical source file | Injection code path |";
        let event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: source.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let ctx = EventRenderContext {
            is_collapsed: false,
            is_expanded: false,
            is_cursor: false,
            is_last_event: false,
            model_name: None,
            pipeline_commands: Vec::new(),
            max_width: 200,
        };

        let rendered: Vec<String> = build_event_lines(&event, &ctx)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(
            rendered.iter().any(|line| line == source),
            "delimiter-less pipe content stays visible as literal text: {rendered:?}"
        );
    }

    #[test]
    fn dense_valid_table_uses_stacked_label_value_layout() {
        let rows = vec![
            (
                BlockElement::TableRow,
                "| Content category | Canonical source | Injection path | Provider |",
            ),
            (
                BlockElement::TableSeparator,
                "| ---------------- | ---------------- | -------------- | -------- |",
            ),
            (
                BlockElement::TableRow,
                "| instructions | AGENTS.md | session launch | Codex | retained extra |",
            ),
        ];

        let rendered: Vec<String> = render_table_block("", &rows, 40)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        for expected in [
            "Content category: instructions",
            "Canonical source: AGENTS.md",
            "Injection path: session launch",
            "Provider: Codex",
            "Column 5: retained extra",
        ] {
            assert!(
                rendered.iter().any(|line| line == expected),
                "stacked table preserves labeled value {expected:?}: {rendered:?}"
            );
        }
    }

    #[test]
    fn test_build_event_lines_table_alignment_integration() {
        // Full integration: build_event_lines should produce aligned table
        let event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: "| Heading | Col2 |\n|---------|------|\n| Short | LongerValue |".to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let ctx = EventRenderContext {
            is_collapsed: false,
            is_expanded: false,
            is_cursor: false,
            is_last_event: false,
            model_name: None,
            pipeline_commands: Vec::new(),
            max_width: 200,
        };
        let lines = build_event_lines(&event, &ctx);
        // Find table lines (those containing table border characters)
        let table_lines: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .filter(|l| l.contains('│') || l.contains('├') || l.contains('┌') || l.contains('└'))
            .collect();
        assert_eq!(table_lines.len(), 5);
        // All table lines should have equal char width
        let widths: Vec<usize> = table_lines.iter().map(|l| l.chars().count()).collect();
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "Table lines should be aligned, got widths {:?} for: {:?}",
            widths,
            table_lines
        );
    }

    #[test]
    fn test_build_event_lines_renders_fenced_markdown_table() {
        let event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content:
                "```markdown\n| Heading | Col2 |\n|---------|------|\n| Short | LongerValue |\n```"
                    .to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let ctx = EventRenderContext {
            is_collapsed: false,
            is_expanded: false,
            is_cursor: false,
            is_last_event: false,
            model_name: None,
            pipeline_commands: Vec::new(),
            max_width: 200,
        };
        let (lines, code_blocks) = build_event_lines_with_meta(&event, &ctx);
        assert!(code_blocks.is_empty());

        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.iter().any(|l| l.starts_with('┌')));
        assert!(text.iter().any(|l| l.starts_with('└')));
        assert!(!text.iter().any(|l| l.contains("─── markdown ───")));
    }

    #[test]
    fn test_assistant_header_uses_model_name() {
        let event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 7,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: "Done".to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let ctx = EventRenderContext {
            is_collapsed: false,
            is_expanded: false,
            is_cursor: false,
            is_last_event: false,
            model_name: Some("GPT-5.5".to_string()),
            pipeline_commands: Vec::new(),
            max_width: 80,
        };
        let lines = build_event_lines(&event, &ctx);
        let header = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();

        assert!(header.starts_with("#7  GPT-5.5"));
        assert!(!header.contains("Claude"));
        assert!(!header.contains("ID"));
    }

    #[test]
    fn test_event_headers_render_sequence_first_with_local_time_and_full_date() {
        let user_created_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let unlabeled_created_at = chrono::DateTime::parse_from_rfc3339("2026-01-03T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let ctx = EventRenderContext {
            is_collapsed: false,
            is_expanded: false,
            is_cursor: false,
            is_last_event: false,
            model_name: None,
            pipeline_commands: Vec::new(),
            max_width: 80,
        };
        let user_event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::User),
            content: "First message".to_string(),
            tool_name: None,
            tool_input: None,
            created_at: user_created_at,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let unlabeled_event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 2,
            event_type: EventType::System,
            role: None,
            content: "Second event".to_string(),
            tool_name: None,
            tool_input: None,
            created_at: unlabeled_created_at,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let header_text = |event: &ConversationEvent| {
            let header_idx = usize::from(event.event_type != EventType::Message);
            build_event_lines(event, &ctx)[header_idx]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let user_stamp = user_event
            .created_at
            .with_timezone(&chrono::Local)
            .format("%H:%M  %Y-%m-%d")
            .to_string();
        let unlabeled_stamp = unlabeled_event
            .created_at
            .with_timezone(&chrono::Local)
            .format("%H:%M  %Y-%m-%d")
            .to_string();

        let user_header = header_text(&user_event);
        let unlabeled_header = header_text(&unlabeled_event);
        assert!(user_header.starts_with("#1  You"));
        assert!(user_header.contains(&user_stamp));
        assert!(unlabeled_header.starts_with("#2"));
        assert!(unlabeled_header.contains(&unlabeled_stamp));
    }

    #[test]
    fn test_model_for_sequence_falls_back_to_session_model() {
        assert_eq!(
            model_for_sequence(&[], 3, Some("gpt-5.5")),
            Some("GPT-5.5".to_string())
        );
    }

    #[test]
    fn test_render_header_has_bold_and_color_no_hashes() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let line = render_markdown_line("", &BlockElement::Header(2), "Hello World");
        // Should have 1 span: just the content (no ## prefix)
        assert_eq!(
            line.spans.len(),
            1,
            "Expected 1 span (no # prefix), got {}: {:?}",
            line.spans.len(),
            line.spans
        );

        // Content should have header color and bold
        assert_eq!(line.spans[0].content.as_ref(), "Hello World");
        assert_eq!(
            line.spans[0].style.fg,
            Some(super::super::theme::md_header())
        );
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_render_h1_is_underlined() {
        let line = render_markdown_line("", &BlockElement::Header(1), "Title");
        assert_eq!(line.spans[0].content.as_ref(), "Title");
        assert!(
            line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_detect_and_render_header_full_flow() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let input = "# CPU Usage Issues in Ratatui";
        let (block, content) = detect_block_element(input);
        assert_eq!(block, BlockElement::Header(1));
        assert_eq!(content, "CPU Usage Issues in Ratatui");

        let line = render_markdown_line("  ", &block, content);
        // First span: indent "  "
        assert_eq!(line.spans[0].content.as_ref(), "  ");
        // Second span: content with styling (no # prefix)
        assert_eq!(
            line.spans[1].content.as_ref(),
            "CPU Usage Issues in Ratatui"
        );
        assert_eq!(
            line.spans[1].style.fg,
            Some(super::super::theme::md_header())
        );
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert!(
            line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn test_pipeline_artifact_to_command() {
        // Research → plan
        assert_eq!(
            pipeline_artifact_to_command("thoughts/shared/research/2026-03-04-foo.md"),
            Some("/plan @thoughts/shared/research/2026-03-04-foo.md".to_string())
        );

        // Plans → implement
        assert_eq!(
            pipeline_artifact_to_command("thoughts/shared/plans/2026-03-04-bar.md"),
            Some("/implement @thoughts/shared/plans/2026-03-04-bar.md".to_string())
        );

        // Handoffs → resume_handoff
        assert_eq!(
            pipeline_artifact_to_command("thoughts/shared/handoffs/general/2026-03-04.md"),
            Some("/resume_handoff thoughts/shared/handoffs/general/2026-03-04.md".to_string())
        );

        // Unknown → None
        assert_eq!(
            pipeline_artifact_to_command("thoughts/shared/other/foo.md"),
            None
        );
        assert_eq!(pipeline_artifact_to_command("random/path.md"), None);
    }

    #[test]
    fn test_pipeline_artifact_labels_and_pills() {
        // Verify derived commands produce correct labels/pills
        let cmd =
            pipeline_artifact_to_command("thoughts/shared/research/2026-03-04-foo.md").unwrap();
        assert_eq!(docregblock_label(&cmd), " \u{25B8} plan ");
        assert_eq!(docregblock_pill_label(&[cmd]), Some("plan".to_string()));

        let cmd = pipeline_artifact_to_command("thoughts/shared/plans/2026-03-04-bar.md").unwrap();
        assert_eq!(docregblock_label(&cmd), " \u{25B8} implement ");
        assert_eq!(docregblock_pill_label(&[cmd]), Some("impl".to_string()));

        let cmd = "/merge_ready".to_string();
        assert_eq!(docregblock_label(&cmd), " \u{25B8} merge_ready ");
        assert_eq!(docregblock_pill_label(&[cmd]), Some("mrg".to_string()));
    }

    // --- underscore bold/italic tests ---

    #[test]
    fn test_parse_inline_underscore_bold() {
        let spans = parse_inline_markdown("before __bold__ after");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content.as_ref(), "bold");
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_parse_inline_underscore_italic() {
        let spans = parse_inline_markdown("before _italic_ after");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content.as_ref(), "italic");
        assert!(spans[1].style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn test_parse_inline_underscore_word_boundary() {
        // snake_case should NOT be styled
        let spans = parse_inline_markdown("some_variable_name");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "some_variable_name");
        assert!(!spans[0].style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn test_parse_inline_underscore_double_word_boundary() {
        // __init__ with spaces around it SHOULD match (word boundaries)
        let spans = parse_inline_markdown("Python's __init__ method");
        assert_eq!(spans.len(), 3);
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_parse_inline_strikethrough() {
        let spans = parse_inline_markdown("before ~~struck~~ after");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content.as_ref(), "struck");
        assert!(spans[1].style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn test_parse_inline_strikethrough_color() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let spans = parse_inline_markdown("~~deleted~~");
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].style.fg,
            Some(super::super::theme::md_strikethrough())
        );
    }

    // --- header hierarchy tests ---

    #[test]
    fn test_render_header_h3_no_bold() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let line = render_markdown_line("", &BlockElement::Header(3), "H3 Title");
        let header_span = &line.spans[0];
        assert!(!header_span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(header_span.style.fg, Some(super::super::theme::md_header()));
    }

    #[test]
    fn test_render_header_h4_minor_color() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let line = render_markdown_line("", &BlockElement::Header(4), "H4 Title");
        let header_span = &line.spans[0];
        assert!(!header_span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            header_span.style.fg,
            Some(super::super::theme::md_header_minor())
        );
    }

    // --- task list tests ---

    #[test]
    fn test_detect_task_unchecked() {
        let (block, content) = detect_block_element("- [ ] todo item");
        assert_eq!(block, BlockElement::TaskUnchecked);
        assert_eq!(content, "todo item");
    }

    #[test]
    fn test_detect_task_checked() {
        let (block, content) = detect_block_element("- [x] done item");
        assert_eq!(block, BlockElement::TaskChecked);
        assert_eq!(content, "done item");
    }

    #[test]
    fn test_detect_task_checked_uppercase() {
        let (block, content) = detect_block_element("- [X] done item");
        assert_eq!(block, BlockElement::TaskChecked);
        assert_eq!(content, "done item");
    }

    #[test]
    fn test_detect_task_star_prefix() {
        let (block, content) = detect_block_element("* [ ] star task");
        assert_eq!(block, BlockElement::TaskUnchecked);
        assert_eq!(content, "star task");
    }

    #[test]
    fn test_detect_nested_list() {
        // Indented lists still detected as list items after trimming
        let (block, content) = detect_block_element("  - nested item");
        assert_eq!(block, BlockElement::UnorderedList);
        assert_eq!(content, "nested item");
    }

    #[test]
    fn test_detect_nested_ordered_list() {
        let (block, content) = detect_block_element("    1. deep item");
        assert_eq!(block, BlockElement::OrderedList(1));
        assert_eq!(content, "deep item");
    }

    #[test]
    fn test_code_block_range_extraction() {
        let event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: "Some text\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\nMore text"
                .to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let ctx = EventRenderContext {
            is_collapsed: false,
            is_expanded: false,
            is_cursor: false,
            is_last_event: false,
            model_name: None,
            pipeline_commands: Vec::new(),
            max_width: 80,
        };
        let (lines, code_blocks) = build_event_lines_with_meta(&event, &ctx);
        assert_eq!(code_blocks.len(), 1);
        let cb = &code_blocks[0];
        assert_eq!(cb.content, "fn main() {\n    println!(\"hello\");\n}");

        // Let's assert that the lines between start_line and end_line exist and match
        assert!(cb.start_line < cb.end_line);
        assert!(cb.end_line <= lines.len());

        // Print the lines within range for debugging
        for idx in cb.start_line..cb.end_line {
            let line_str = lines[idx]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            println!("Line {}: {:?}", idx, line_str);
        }
    }
}
