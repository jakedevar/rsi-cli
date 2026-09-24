//! Cursor movement primitives and character search for vim emulation.

use super::operators::apply_operator;
use super::state::{CharSearch, CharSearchDir, VimAction, VimState};
use crossterm::event::{KeyCode, KeyEvent};
use tui_textarea::{CursorMove, TextArea};

/// Move cursor to first non-blank character on the current line (^ motion).
pub(super) fn move_to_first_non_blank(textarea: &mut TextArea<'static>) {
    textarea.move_cursor(CursorMove::Head);
    let (row, _) = textarea.cursor();
    let indent = textarea.lines()[row]
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    for _ in 0..indent {
        textarea.move_cursor(CursorMove::Forward);
    }
}

/// Join current line with next line (J motion).
pub(super) fn join_lines(textarea: &mut TextArea<'static>) {
    let (row, _) = textarea.cursor();
    if row + 1 >= textarea.lines().len() {
        return;
    }
    textarea.move_cursor(CursorMove::End);
    textarea.delete_next_char(); // deletes the newline
    // Check if space is needed between joined content
    let needs_space = {
        let (new_row, new_col) = textarea.cursor();
        let line = &textarea.lines()[new_row];
        new_col > 0
            && line
                .chars()
                .nth(new_col)
                .is_some_and(|c| !c.is_whitespace())
            && line
                .chars()
                .nth(new_col - 1)
                .is_some_and(|c| !c.is_whitespace())
    };
    if needs_space {
        textarea.insert_char(' ');
    }
}

/// Toggle case of character under cursor and advance (~).
pub(super) fn toggle_case(textarea: &mut TextArea<'static>) {
    let (row, col) = textarea.cursor();
    let ch = {
        let line = &textarea.lines()[row];
        line.chars().nth(col)
    };
    let Some(ch) = ch else {
        return;
    };
    textarea.delete_next_char();
    let toggled = if ch.is_uppercase() {
        ch.to_lowercase().to_string()
    } else {
        ch.to_uppercase().to_string()
    };
    textarea.insert_str(&toggled);
    // Cursor is now after the inserted char — that's correct (~ advances)
}

/// Move to previous blank line (paragraph backward).
pub(super) fn move_paragraph_backward(textarea: &mut TextArea<'static>) {
    let (row, _) = textarea.cursor();

    if row == 0 {
        textarea.move_cursor(CursorMove::Head);
        return;
    }

    // Compute target row from line content (read-only scan)
    #[allow(clippy::needless_range_loop)]
    let target = {
        let lines = textarea.lines();
        let mut t = 0;
        let mut in_content = !lines[row].trim().is_empty();
        for i in (0..row).rev() {
            if lines[i].trim().is_empty() {
                if in_content {
                    t = i;
                    break;
                }
            } else {
                in_content = true;
            }
        }
        t
    };

    let delta = row - target;
    for _ in 0..delta {
        textarea.move_cursor(CursorMove::Up);
    }
    textarea.move_cursor(CursorMove::Head);
}

/// Move to next blank line (paragraph forward).
pub(super) fn move_paragraph_forward(textarea: &mut TextArea<'static>) {
    let (row, _) = textarea.cursor();

    let (target, line_count) = {
        let lines = textarea.lines();
        let lc = lines.len();
        if row + 1 >= lc {
            return textarea.move_cursor(CursorMove::End);
        }
        let mut t = lc - 1;
        let mut in_content = !lines[row].trim().is_empty();
        for (i, line) in lines.iter().enumerate().skip(row + 1) {
            if line.trim().is_empty() {
                if in_content {
                    t = i;
                    break;
                }
            } else {
                in_content = true;
            }
        }
        (t, lc)
    };
    let _ = line_count; // suppress unused warning

    let delta = target - row;
    for _ in 0..delta {
        textarea.move_cursor(CursorMove::Down);
    }
    textarea.move_cursor(CursorMove::Head);
}

/// Move to matching bracket (%).
#[allow(clippy::needless_range_loop)]
pub(super) fn move_to_matching_bracket(textarea: &mut TextArea<'static>) {
    let (row, col) = textarea.cursor();
    // Clone lines to avoid borrow conflict with textarea mutation
    let lines: Vec<String> = textarea.lines().iter().map(|s| s.to_string()).collect();
    let chars: Vec<char> = lines[row].chars().collect();

    // Find nearest bracket at or after cursor, then try backward
    let bracket_pos = (col..chars.len())
        .find(|&i| is_bracket(chars[i]))
        .or_else(|| (0..col).rev().find(|&i| is_bracket(chars[i])));
    let Some(pos) = bracket_pos else {
        return;
    };
    let bracket = chars[pos];

    // Move cursor to bracket position first
    if pos > col {
        for _ in 0..(pos - col) {
            textarea.move_cursor(CursorMove::Forward);
        }
    }

    let (open, close, forward) = match bracket {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return,
    };

    let mut depth: i32 = 0;
    if forward {
        for (r, line_str) in lines.iter().enumerate().skip(row) {
            let line_chars: Vec<char> = line_str.chars().collect();
            let start = if r == row { pos } else { 0 };
            for c in start..line_chars.len() {
                if line_chars[c] == open {
                    depth += 1;
                } else if line_chars[c] == close {
                    depth -= 1;
                    if depth == 0 {
                        move_cursor_to(textarea, r, c);
                        return;
                    }
                }
            }
        }
    } else {
        for r in (0..=row).rev() {
            let line_chars: Vec<char> = lines[r].chars().collect();
            let end = if r == row {
                pos
            } else {
                line_chars.len().saturating_sub(1)
            };
            for c in (0..=end).rev() {
                if line_chars[c] == close {
                    depth += 1;
                } else if line_chars[c] == open {
                    depth -= 1;
                    if depth == 0 {
                        move_cursor_to(textarea, r, c);
                        return;
                    }
                }
            }
        }
    }
}

pub fn is_bracket(ch: char) -> bool {
    matches!(ch, '(' | ')' | '[' | ']' | '{' | '}')
}

/// Move cursor to an absolute (row, col) position.
pub fn move_cursor_to(textarea: &mut TextArea<'static>, row: usize, col: usize) {
    let (cur_row, _) = textarea.cursor();

    // Move to target row
    if row < cur_row {
        for _ in 0..(cur_row - row) {
            textarea.move_cursor(CursorMove::Up);
        }
    } else {
        for _ in 0..(row - cur_row) {
            textarea.move_cursor(CursorMove::Down);
        }
    }

    // Move to target col
    textarea.move_cursor(CursorMove::Head);
    for _ in 0..col {
        textarea.move_cursor(CursorMove::Forward);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VisualLine {
    pub logical_row: usize,
    pub char_range: (usize, usize),
    pub visual_indent: usize,
}

pub fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn word_boundary_break(s: &str, max_width: usize) -> usize {
    let char_limit = floor_char_boundary(s, max_width);
    if char_limit >= s.len() {
        return s.len();
    }
    match s[..char_limit].rfind(' ') {
        Some(space_pos) => space_pos + 1,
        None => char_limit,
    }
}

pub fn compute_visual_lines(lines: &[String], wrap_width: usize) -> Vec<VisualLine> {
    let mut visual_lines = Vec::new();
    if wrap_width == 0 {
        for (row, line) in lines.iter().enumerate() {
            visual_lines.push(VisualLine {
                logical_row: row,
                char_range: (0, line.chars().count()),
                visual_indent: 0,
            });
        }
        return visual_lines;
    }

    for (row, logical_line) in lines.iter().enumerate() {
        if logical_line.is_empty() {
            visual_lines.push(VisualLine {
                logical_row: row,
                char_range: (0, 0),
                visual_indent: 0,
            });
            continue;
        }

        let indent_char_len = logical_line.len() - logical_line.trim_start().len();
        let mut char_offset = 0;
        let mut remaining = logical_line.as_str();
        let mut is_first_chunk = true;

        while !remaining.is_empty() {
            let effective_width = if is_first_chunk {
                wrap_width
            } else {
                wrap_width.saturating_sub(indent_char_len).max(1)
            };

            let chunk_len = if remaining.len() <= effective_width {
                remaining.len()
            } else {
                word_boundary_break(remaining, effective_width)
            };

            if chunk_len == 0 {
                let first_char_len = remaining.chars().next().map_or(0, |c| c.len_utf8());
                if first_char_len == 0 {
                    break;
                }
                visual_lines.push(VisualLine {
                    logical_row: row,
                    char_range: (char_offset, char_offset + 1),
                    visual_indent: if is_first_chunk { 0 } else { indent_char_len },
                });
                char_offset += 1;
                remaining = &remaining[first_char_len..];
                is_first_chunk = false;
                continue;
            }

            let chunk = &remaining[..chunk_len];
            let chunk_chars = chunk.chars().count();

            visual_lines.push(VisualLine {
                logical_row: row,
                char_range: (char_offset, char_offset + chunk_chars),
                visual_indent: if is_first_chunk { 0 } else { indent_char_len },
            });

            char_offset += chunk_chars;
            remaining = &remaining[chunk_len..];
            is_first_chunk = false;
        }
    }
    visual_lines
}

pub fn find_current_visual_line(
    visual_lines: &[VisualLine],
    cursor_row: usize,
    cursor_col: usize,
) -> usize {
    for (i, vl) in visual_lines.iter().enumerate() {
        if vl.logical_row == cursor_row {
            if cursor_col >= vl.char_range.0 && cursor_col < vl.char_range.1 {
                return i;
            }
        }
    }
    let mut last_matching = 0;
    for (i, vl) in visual_lines.iter().enumerate() {
        if vl.logical_row == cursor_row {
            last_matching = i;
        }
    }
    last_matching
}

/// Move cursor vertically by `count` lines (positive = down, negative = up),
/// maintaining the desired column (vim's curswant) across empty lines.
pub fn move_vertical_with_curswant(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    count: isize,
    wrap_width: Option<usize>,
    is_insert_mode: bool,
) {
    let wrap_width = wrap_width.unwrap_or(0);
    if wrap_width == 0 {
        let (cur_row, cur_col) = textarea.cursor();
        let want_col = state.desired_col.unwrap_or(cur_col);
        state.desired_col = Some(want_col);

        let total_lines = textarea.lines().len();
        let target_row = if count >= 0 {
            (cur_row + count as usize).min(total_lines.saturating_sub(1))
        } else {
            cur_row.saturating_sub((-count) as usize)
        };

        if target_row < cur_row {
            for _ in 0..(cur_row - target_row) {
                textarea.move_cursor(CursorMove::Up);
            }
        } else {
            for _ in 0..(target_row - cur_row) {
                textarea.move_cursor(CursorMove::Down);
            }
        }

        let line_len = textarea.lines().get(target_row).map_or(0, |l| l.len());
        let max_col = if is_insert_mode {
            line_len
        } else {
            if line_len > 0 { line_len - 1 } else { 0 }
        };
        let target_col = want_col.min(max_col);

        textarea.move_cursor(CursorMove::Head);
        for _ in 0..target_col {
            textarea.move_cursor(CursorMove::Forward);
        }
        return;
    }

    let lines = textarea.lines();
    let visual_lines = compute_visual_lines(lines, wrap_width);
    if visual_lines.is_empty() {
        return;
    }

    let (cur_row, cur_col) = textarea.cursor();
    let current_v_idx = find_current_visual_line(&visual_lines, cur_row, cur_col);
    let current_vl = &visual_lines[current_v_idx];
    let col_in_vl = cur_col.saturating_sub(current_vl.char_range.0);
    let visual_col = col_in_vl + current_vl.visual_indent;

    let want_visual_col = state.desired_col.unwrap_or(visual_col);
    state.desired_col = Some(want_visual_col);

    let target_v_idx = if count >= 0 {
        (current_v_idx + count as usize).min(visual_lines.len().saturating_sub(1))
    } else {
        current_v_idx.saturating_sub((-count) as usize)
    };

    let target_vl = &visual_lines[target_v_idx];
    let rel_col = want_visual_col.saturating_sub(target_vl.visual_indent);
    let vl_len = target_vl.char_range.1 - target_vl.char_range.0;

    let max_rel_col = if is_insert_mode {
        vl_len
    } else {
        if vl_len > 0 { vl_len - 1 } else { 0 }
    };
    let target_rel_col = rel_col.min(max_rel_col);
    let target_logical_col = target_vl.char_range.0 + target_rel_col;

    move_cursor_to(textarea, target_vl.logical_row, target_logical_col);
}

/// Handle the target character for f/t/F/T search.
pub(super) fn handle_char_search_target(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    dir: CharSearchDir,
    key: KeyEvent,
) -> VimAction {
    let KeyCode::Char(target) = key.code else {
        // Non-char cancels the search
        return VimAction::Consumed;
    };

    let search = CharSearch {
        direction: dir,
        ch: target,
    };
    let count = state.pending_count.take().unwrap_or(1);

    // If operator is pending, this is a motion for the operator
    if state.pending_operator.is_some() {
        for _ in 0..count {
            execute_char_search(textarea, &search, false);
        }
        // For f/F/t, include the target char in the selection
        if matches!(dir, CharSearchDir::ForwardTo | CharSearchDir::ForwardTill) {
            // Forward motions: need to include char under cursor for operator
            textarea.move_cursor(CursorMove::Forward);
        }
        state.last_char_search = Some(search);
        return apply_operator(textarea, state, state.pending_operator.unwrap_or('d'));
    }

    for _ in 0..count {
        execute_char_search(textarea, &search, false);
    }
    state.last_char_search = Some(search);
    VimAction::Consumed
}

/// Execute a character search (f/t/F/T) on the current line.
pub(super) fn execute_char_search(
    textarea: &mut TextArea<'static>,
    search: &CharSearch,
    is_repeat: bool,
) {
    let (row, col) = textarea.cursor();
    let line: Vec<char> = textarea.lines()[row].chars().collect();

    match search.direction {
        CharSearchDir::ForwardTo | CharSearchDir::ForwardTill => {
            for (i, &ch) in line.iter().enumerate().skip(col + 1) {
                if ch == search.ch {
                    let target = if matches!(search.direction, CharSearchDir::ForwardTill) {
                        i.saturating_sub(1)
                    } else {
                        i
                    };
                    if is_repeat && target <= col {
                        // For t: char is adjacent so target == col (no-op).
                        // Continue searching for the next occurrence so ; repeat works.
                        continue;
                    }
                    for _ in 0..(target - col) {
                        textarea.move_cursor(CursorMove::Forward);
                    }
                    return;
                }
            }
        }
        CharSearchDir::BackwardTo | CharSearchDir::BackwardTill => {
            for i in (0..col).rev() {
                if line[i] == search.ch {
                    let target = if matches!(search.direction, CharSearchDir::BackwardTill) {
                        i + 1
                    } else {
                        i
                    };
                    if is_repeat && target >= col {
                        // For T: char is adjacent so target == col (no-op).
                        // Continue searching for the next occurrence so ; repeat works.
                        continue;
                    }
                    for _ in 0..(col - target) {
                        textarea.move_cursor(CursorMove::Back);
                    }
                    return;
                }
            }
        }
    }
}

/// Find the position of the bracket matching the one at (row, col),
/// without moving the cursor. Returns None if no bracket is at that position
/// or no matching bracket is found.
pub fn find_matching_bracket_pos(
    lines: &[String],
    row: usize,
    col: usize,
) -> Option<(usize, usize)> {
    let chars: Vec<char> = lines.get(row)?.chars().collect();
    if col >= chars.len() {
        return None;
    }
    let bracket = chars[col];
    if !is_bracket(bracket) {
        return None;
    }
    let (open, close, forward) = match bracket {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return None,
    };

    let mut depth: i32 = 0;
    if forward {
        for (r, line_str) in lines.iter().enumerate().skip(row) {
            let lchars: Vec<char> = line_str.chars().collect();
            let start = if r == row { col } else { 0 };
            for (c, &ch) in lchars.iter().enumerate().skip(start) {
                if ch == open {
                    depth += 1;
                } else if ch == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some((r, c));
                    }
                }
            }
        }
    } else {
        for r in (0..=row).rev() {
            let lchars: Vec<char> = lines[r].chars().collect();
            if lchars.is_empty() {
                continue;
            }
            let end = if r == row { col } else { lchars.len() - 1 };
            for c in (0..=end).rev() {
                let ch = lchars[c];
                if ch == close {
                    depth += 1;
                } else if ch == open {
                    depth -= 1;
                    if depth == 0 {
                        return Some((r, c));
                    }
                }
            }
        }
    }
    None
}
