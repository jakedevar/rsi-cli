//! Operator-pending mode handling (d, c, y + motion/textobject).

use super::motions::move_cursor_to;
use super::state::{CharSearchDir, VimAction, VimState};
use crate::text_objects::{self, TextObjectKind, TextRange};
use crossterm::event::{KeyCode, KeyEvent};
use tui_textarea::{CursorMove, TextArea};

/// Handle a key while an operator is pending (d/c/y waiting for a motion).
pub(super) fn handle_operator_pending(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    op: char,
    key: KeyEvent,
    count: usize,
) -> VimAction {
    let count = if count > 0 { count } else { 1 };
    state.record_key(key);

    let motion_applied = match key.code {
        // Double-operator: dd/cc/yy → operate on entire line(s)
        KeyCode::Char(c) if c == op => {
            textarea.cancel_selection();
            let total_lines = textarea.lines().len();
            let (cur_row, _) = textarea.cursor();
            let last_target = (cur_row + count - 1).min(total_lines - 1);

            textarea.move_cursor(CursorMove::Head);
            textarea.start_selection();
            for _ in 0..count.saturating_sub(1) {
                textarea.move_cursor(CursorMove::Down);
            }

            // If there's a line after the target, select through the newline
            if last_target + 1 < total_lines {
                textarea.move_cursor(CursorMove::Down);
                textarea.move_cursor(CursorMove::Head);
            } else {
                // Last line(s) of buffer — select to end
                textarea.move_cursor(CursorMove::End);
            }

            // Apply the operator, then move cursor up for delete (dd)
            let result = apply_operator(textarea, state, op);
            // After dd, shift cursor up to the line above if not at the top
            if op == 'd' && cur_row > 0 {
                textarea.move_cursor(CursorMove::Up);
            }
            // Move to first non-blank character on the target line (vim behavior)
            let (row, _) = textarea.cursor();
            let line = textarea.lines().get(row).map(|s| s.as_str()).unwrap_or("");
            let first_non_blank = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);
            move_cursor_to(textarea, row, first_non_blank);
            return result;
        }
        // Motions
        KeyCode::Char('w') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::WordForward);
            }
            true
        }
        KeyCode::Char('b') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::WordBack);
            }
            true
        }
        KeyCode::Char('e') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::WordEnd);
            }
            textarea.move_cursor(CursorMove::Forward); // include char under cursor
            true
        }
        KeyCode::Char('l') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Forward);
            }
            true
        }
        KeyCode::Char('h') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Back);
            }
            true
        }
        KeyCode::Char('$') => {
            textarea.move_cursor(CursorMove::End);
            true
        }
        KeyCode::Char('0') => {
            textarea.move_cursor(CursorMove::Head);
            true
        }
        KeyCode::Char('G') => {
            textarea.move_cursor(CursorMove::Bottom);
            textarea.move_cursor(CursorMove::End);
            true
        }
        KeyCode::Char('g') => {
            // d/c/y + g → wait for the second g without resolving the operator.
            state.pending_g = true;
            return VimAction::Consumed;
        }
        KeyCode::Char('j') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Down);
            }
            textarea.move_cursor(CursorMove::End);
            true
        }
        KeyCode::Char('k') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Up);
            }
            true
        }
        // Text object prefix: i or a
        KeyCode::Char(prefix @ ('i' | 'a')) => {
            state.pending_textobj_prefix = Some(prefix);
            return VimAction::Consumed;
        }
        // f/t/F/T motions in operator-pending
        KeyCode::Char(ch @ ('f' | 't')) => {
            let dir = if ch == 'f' {
                CharSearchDir::ForwardTo
            } else {
                CharSearchDir::ForwardTill
            };
            state.pending_char_search_dir = Some(dir);
            return VimAction::Consumed;
        }
        KeyCode::Char(ch @ ('F' | 'T')) => {
            let dir = if ch == 'F' {
                CharSearchDir::BackwardTo
            } else {
                CharSearchDir::BackwardTill
            };
            state.pending_char_search_dir = Some(dir);
            return VimAction::Consumed;
        }
        // Esc or unknown key cancels operator
        _ => {
            textarea.cancel_selection();
            state.pending_operator = None;
            return VimAction::Consumed;
        }
    };

    if motion_applied {
        apply_operator(textarea, state, op)
    } else {
        VimAction::Consumed
    }
}

/// Resolve the second key of an operator's gg motion. The motion is linewise:
/// it includes the current line and every line above it.
pub(super) fn handle_operator_g_prefix(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    op: char,
    key: KeyEvent,
) -> VimAction {
    if key.code != KeyCode::Char('g') {
        textarea.cancel_selection();
        state.pending_operator = None;
        state.recording_change = None;
        return VimAction::Consumed;
    }

    state.record_key(key);
    let current_row = textarea.cursor().0;
    let total_lines = textarea.lines().len();
    textarea.cancel_selection();
    textarea.move_cursor(CursorMove::Top);
    textarea.move_cursor(CursorMove::Head);
    textarea.start_selection();
    if current_row + 1 < total_lines {
        for _ in 0..=current_row {
            textarea.move_cursor(CursorMove::Down);
        }
        textarea.move_cursor(CursorMove::Head);
    } else {
        textarea.move_cursor(CursorMove::Bottom);
        textarea.move_cursor(CursorMove::End);
    }
    state.desired_col = None;
    apply_operator(textarea, state, op)
}

/// Apply the pending operator after a motion has been executed.
pub(super) fn apply_operator(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    op: char,
) -> VimAction {
    let entered_insert = match op {
        'd' => {
            textarea.cut();
            false
        }
        'c' => {
            textarea.cut();
            true
        }
        'y' => {
            textarea.copy();
            crate::clipboard::osc52_copy(&textarea.yank_text());
            textarea.cancel_selection();
            false
        }
        _ => false,
    };
    state.pending_operator = None;
    if entered_insert {
        // Don't finalize yet — caller will finalize on insert exit
        VimAction::EnteredInsert
    } else {
        state.finalize_change();
        VimAction::Consumed
    }
}

/// Handle the second key of a text object (after 'i' or 'a' prefix).
pub(super) fn handle_textobj_key(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    prefix: char,
    key: KeyEvent,
) -> VimAction {
    // Record the target key (prefix was already recorded in handle_operator_pending)
    state.record_key(key);

    let kind = match prefix {
        'i' => TextObjectKind::Inner,
        'a' => TextObjectKind::Around,
        _ => unreachable!(),
    };

    let (row, col) = textarea.cursor();
    let lines: Vec<String> = textarea.lines().iter().map(|s| s.to_string()).collect();

    let range = match key.code {
        KeyCode::Char('w') => text_objects::word_object(&lines, row, col, kind),
        KeyCode::Char('"') => text_objects::delimited_object(&lines, row, col, '"', '"', kind),
        KeyCode::Char('\'') => text_objects::delimited_object(&lines, row, col, '\'', '\'', kind),
        KeyCode::Char('(' | ')' | 'b') => {
            text_objects::delimited_object(&lines, row, col, '(', ')', kind)
        }
        KeyCode::Char('{' | '}' | 'B') => {
            text_objects::delimited_object(&lines, row, col, '{', '}', kind)
        }
        KeyCode::Char('[' | ']') => {
            text_objects::delimited_object(&lines, row, col, '[', ']', kind)
        }
        KeyCode::Char('s') => text_objects::sentence_object(&lines, row, col, kind),
        KeyCode::Char('p') => text_objects::paragraph_object(&lines, row, col, kind),
        _ => {
            // Invalid text object — cancel operator
            textarea.cancel_selection();
            state.pending_operator = None;
            return VimAction::Consumed;
        }
    };

    if let Some(range) = range {
        // In visual mode, re-select the text object range
        if state.visual.is_some() {
            textarea.cancel_selection();
            move_cursor_to(textarea, range.start_row, range.start_col);
            textarea.start_selection();
            move_cursor_to(textarea, range.end_row, range.end_col);
            return VimAction::Consumed;
        }
        apply_operator_to_range(textarea, state, range)
    } else {
        textarea.cancel_selection();
        state.pending_operator = None;
        VimAction::Consumed
    }
}

/// Apply the pending operator to a TextRange (from text object or visual selection).
fn apply_operator_to_range(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    range: TextRange,
) -> VimAction {
    // Cancel any existing selection, position to range start
    textarea.cancel_selection();
    move_cursor_to(textarea, range.start_row, range.start_col);
    textarea.start_selection();
    // Move one past end_col to make the selection inclusive of end_col
    move_cursor_to(textarea, range.end_row, range.end_col);
    textarea.move_cursor(CursorMove::Forward);

    let op = state.pending_operator.take().unwrap_or('d');
    match op {
        'd' => {
            textarea.cut();
            state.finalize_change();
            VimAction::Consumed
        }
        'c' => {
            textarea.cut();
            // Don't finalize — caller will finalize on insert exit
            VimAction::EnteredInsert
        }
        'y' => {
            textarea.copy();
            crate::clipboard::osc52_copy(&textarea.yank_text());
            textarea.cancel_selection();
            state.finalize_change();
            VimAction::Consumed
        }
        _ => VimAction::Consumed,
    }
}
