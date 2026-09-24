//! Normal mode key dispatch for vim emulation.

use super::motions;
use super::motions::{
    execute_char_search, handle_char_search_target, join_lines, move_paragraph_backward,
    move_paragraph_forward, move_to_first_non_blank, move_to_matching_bracket, toggle_case,
};
use super::operators::{handle_operator_g_prefix, handle_operator_pending, handle_textobj_key};
use super::state::{CharSearch, CharSearchDir, VimAction, VimState, VisualMode};
use super::visual::handle_visual_mode;
use crate::types::IndentStyle;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::{CursorMove, TextArea};

/// Optional file-editor context passed to normal mode handler.
/// When present, enables file-editor-specific behaviors (auto-indent, etc.)
pub struct FileEditorCtx<'a> {
    pub indent_style: IndentStyle,
    /// Read-only snapshot of lines for inspecting adjacent lines.
    pub lines: &'a [String],
    pub wrap_width: Option<usize>,
}

/// Block-opener characters that trigger an extra indent level.
const BLOCK_OPENERS: &[char] = &['{', ':', '(', '['];

/// Compute the indentation string for `o` (open line below).
/// Reference line = the line the cursor is currently on.
fn compute_open_below_indent(textarea: &TextArea<'static>, ctx: &FileEditorCtx<'_>) -> String {
    let (row, _) = textarea.cursor();
    let ref_line = ctx.lines.get(row).map(String::as_str).unwrap_or("");
    let base_indent = leading_whitespace(ref_line);
    let trimmed = ref_line.trim_end();

    if trimmed.ends_with(|c: char| BLOCK_OPENERS.contains(&c)) {
        // Add one level of indentation
        format!("{}{}", base_indent, indent_unit(ctx.indent_style))
    } else {
        base_indent.to_string()
    }
}

/// Compute the indentation string for `O` (open line above).
/// Reference line = the current line (which will shift down).
fn compute_open_above_indent(textarea: &TextArea<'static>, ctx: &FileEditorCtx<'_>) -> String {
    let (row, _) = textarea.cursor();
    let ref_line = ctx.lines.get(row).map(String::as_str).unwrap_or("");
    leading_whitespace(ref_line).to_string()
    // No block-opener heuristic for O -- the line above hasn't ended yet
}

/// Extract leading whitespace from a line.
fn leading_whitespace(line: &str) -> &str {
    let trimmed_start = line.len() - line.trim_start().len();
    &line[..trimmed_start]
}

/// One indent unit as a string given the detected style.
fn indent_unit(style: IndentStyle) -> String {
    if style.use_tabs {
        "\t".to_string()
    } else {
        " ".repeat(style.width as usize)
    }
}

/// Process a key event in vim normal mode.
///
/// Handles:
/// - Count accumulation (1-9 start, 0 continues)
/// - g-prefix (gg), r-prefix (replace char)
/// - Operator-pending mode (d/c/y + motions, dd/cc/yy)
/// - Motions (h/j/k/l/w/b/e/0/$/G/gg/^/{/}/%)
/// - Direct editing (x, D, C, p, u, Ctrl+R, s, J, ~)
/// - Mode transitions (i, a, A, I, o, O)
/// - Operator triggers (d, c, y → start selection, set pending)
///
/// When `editor_ctx` is `Some`, file-editor-specific behaviors are enabled
/// (auto-indent on o/O). All existing callers pass `None`.
///
/// Returns `VimAction` so the caller can handle side effects.
pub fn handle_vim_normal(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    key: KeyEvent,
    editor_ctx: Option<&FileEditorCtx<'_>>,
) -> VimAction {
    // --- Phase 0: g-prefix resolution ---
    if state.pending_g {
        state.pending_g = false;
        if let Some(op) = state.pending_operator {
            return handle_operator_g_prefix(textarea, state, op, key);
        }
        return handle_g_prefix(textarea, state, key);
    }

    // --- Phase 0b: r-prefix resolution ---
    if state.pending_replace {
        state.pending_replace = false;
        return handle_replace_char(textarea, state, key);
    }

    // --- Phase 0c: text object prefix resolution (i/a + object key) ---
    if let Some(prefix) = state.pending_textobj_prefix.take() {
        return handle_textobj_key(textarea, state, prefix, key);
    }

    // --- Phase 0d: f/t/F/T char search target resolution ---
    if let Some(dir) = state.pending_char_search_dir.take() {
        return handle_char_search_target(textarea, state, dir, key);
    }

    // --- Phase 1: Count accumulation ---
    if let KeyCode::Char(ch @ '1'..='9') = key.code {
        let digit = ch as usize - '0' as usize;
        state.pending_count = Some(state.pending_count.unwrap_or(0) * 10 + digit);
        return VimAction::Consumed;
    }
    #[allow(clippy::collapsible_if)]
    if let KeyCode::Char('0') = key.code {
        if let Some(count) = state.pending_count {
            state.pending_count = Some(count * 10);
            return VimAction::Consumed;
        }
        // else fall through to Head motion
    }

    let count = state.pending_count.take().unwrap_or(1);

    // --- Phase 2: Operator-pending mode ---
    if let Some(op) = state.pending_operator {
        return handle_operator_pending(textarea, state, op, key, count);
    }

    // --- Phase 2b: Visual mode handling ---
    if state.visual.is_some() {
        return handle_visual_mode(textarea, state, key, count);
    }

    // --- Phase 3: Normal key dispatch ---
    let result = match key.code {
        // Visual mode entry
        KeyCode::Char('v') => {
            state.visual = Some(VisualMode::Char);
            let (row, col) = textarea.cursor();
            state.visual_anchor = Some((row, col));
            textarea.start_selection();
            VimAction::Consumed
        }
        KeyCode::Char('V') => {
            state.visual = Some(VisualMode::Line);
            let (row, _) = textarea.cursor();
            state.visual_anchor = Some((row, 0));
            textarea.move_cursor(CursorMove::Head);
            textarea.start_selection();
            textarea.move_cursor(CursorMove::End);
            VimAction::Consumed
        }

        // Cursor movement
        KeyCode::Char('h') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Back);
            }
            VimAction::Consumed
        }
        KeyCode::Char('j') => {
            let ww = editor_ctx.and_then(|c| c.wrap_width);
            motions::move_vertical_with_curswant(textarea, state, count as isize, ww, false);
            return VimAction::Consumed; // early return: preserve desired_col
        }
        KeyCode::Char('k') => {
            let ww = editor_ctx.and_then(|c| c.wrap_width);
            motions::move_vertical_with_curswant(textarea, state, -(count as isize), ww, false);
            return VimAction::Consumed; // early return: preserve desired_col
        }
        KeyCode::Char('l') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Forward);
            }
            VimAction::Consumed
        }

        // Word movement
        KeyCode::Char('w') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::WordForward);
            }
            VimAction::Consumed
        }
        KeyCode::Char('b') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::WordBack);
            }
            VimAction::Consumed
        }
        KeyCode::Char('e') => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::WordEnd);
            }
            VimAction::Consumed
        }

        // Line movement
        KeyCode::Char('0') => {
            textarea.move_cursor(CursorMove::Head);
            VimAction::Consumed
        }
        KeyCode::Char('$') => {
            textarea.move_cursor(CursorMove::End);
            VimAction::Consumed
        }
        KeyCode::Char('^') => {
            move_to_first_non_blank(textarea);
            VimAction::Consumed
        }

        // Buffer movement
        KeyCode::Char('G') => {
            textarea.move_cursor(CursorMove::Bottom);
            VimAction::Consumed
        }
        KeyCode::Char('g') => {
            state.pending_g = true;
            VimAction::Consumed
        }

        // Paragraph movement
        KeyCode::Char('{') => {
            for _ in 0..count {
                move_paragraph_backward(textarea);
            }
            VimAction::Consumed
        }
        KeyCode::Char('}') => {
            for _ in 0..count {
                move_paragraph_forward(textarea);
            }
            VimAction::Consumed
        }

        // Matching bracket
        KeyCode::Char('%') => {
            move_to_matching_bracket(textarea);
            VimAction::Consumed
        }

        // Direct editing
        KeyCode::Char('x') => {
            state.start_recording(key);
            for _ in 0..count {
                textarea.delete_next_char();
            }
            state.finalize_change();
            VimAction::Consumed
        }
        KeyCode::Char('D') => {
            state.start_recording(key);
            textarea.delete_line_by_end();
            state.finalize_change();
            VimAction::Consumed
        }
        KeyCode::Char('C') => {
            state.start_recording(key);
            textarea.delete_line_by_end();
            // Don't finalize — caller will finalize on insert exit
            VimAction::EnteredInsert
        }
        KeyCode::Char('p') => {
            textarea.paste();
            VimAction::Consumed
        }
        KeyCode::Char('s') => {
            state.start_recording(key);
            for _ in 0..count {
                textarea.delete_next_char();
            }
            // Don't finalize — caller will finalize on insert exit
            VimAction::EnteredInsert
        }
        KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.start_recording(key);
            state.pending_replace = true;
            VimAction::Consumed
        }
        KeyCode::Char('J') => {
            state.start_recording(key);
            for _ in 0..count {
                join_lines(textarea);
            }
            state.finalize_change();
            VimAction::Consumed
        }
        KeyCode::Char('~') => {
            state.start_recording(key);
            for _ in 0..count {
                toggle_case(textarea);
            }
            state.finalize_change();
            VimAction::Consumed
        }

        // Undo / redo
        KeyCode::Char('u') => {
            textarea.undo();
            VimAction::Consumed
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            textarea.redo();
            VimAction::Consumed
        }

        // Mode transitions
        KeyCode::Char('i') => VimAction::EnteredInsert,
        KeyCode::Char('a') => {
            textarea.move_cursor(CursorMove::Forward);
            VimAction::EnteredInsert
        }
        KeyCode::Char('A') => {
            textarea.move_cursor(CursorMove::End);
            VimAction::EnteredInsert
        }
        KeyCode::Char('I') => {
            move_to_first_non_blank(textarea);
            VimAction::EnteredInsert
        }
        KeyCode::Char('o') => {
            let indent = if let Some(ctx) = editor_ctx {
                compute_open_below_indent(textarea, ctx)
            } else {
                String::new()
            };
            textarea.move_cursor(CursorMove::End);
            textarea.insert_newline();
            if !indent.is_empty() {
                textarea.insert_str(&indent);
            }
            VimAction::EnteredInsert
        }
        KeyCode::Char('O') => {
            let indent = if let Some(ctx) = editor_ctx {
                compute_open_above_indent(textarea, ctx)
            } else {
                String::new()
            };
            textarea.move_cursor(CursorMove::Head);
            textarea.insert_newline();
            textarea.move_cursor(CursorMove::Up);
            if !indent.is_empty() {
                textarea.insert_str(&indent);
            }
            VimAction::EnteredInsert
        }

        // f/t/F/T char search
        KeyCode::Char(ch @ ('f' | 't')) => {
            let dir = if ch == 'f' {
                CharSearchDir::ForwardTo
            } else {
                CharSearchDir::ForwardTill
            };
            state.pending_char_search_dir = Some(dir);
            if count > 1 {
                state.pending_count = Some(count);
            }
            VimAction::Consumed
        }
        KeyCode::Char('F') => {
            state.pending_char_search_dir = Some(CharSearchDir::BackwardTo);
            if count > 1 {
                state.pending_count = Some(count);
            }
            VimAction::Consumed
        }
        KeyCode::Char('T') => {
            state.pending_char_search_dir = Some(CharSearchDir::BackwardTill);
            if count > 1 {
                state.pending_count = Some(count);
            }
            VimAction::Consumed
        }

        // ; repeats last f/t/F/T, , reverses it
        KeyCode::Char(';') => {
            if let Some(ref search) = state.last_char_search {
                for _ in 0..count {
                    execute_char_search(textarea, search, true);
                }
            }
            VimAction::Consumed
        }
        KeyCode::Char(',') => {
            if let Some(ref search) = state.last_char_search {
                let reversed = CharSearch {
                    direction: search.direction.reverse(),
                    ch: search.ch,
                };
                for _ in 0..count {
                    execute_char_search(textarea, &reversed, true);
                }
            }
            VimAction::Consumed
        }

        // Dot repeat
        KeyCode::Char('.') => {
            if let Some(change) = state.last_change.clone() {
                state.replaying = true;
                for k in &change.keys {
                    handle_vim_normal(textarea, state, *k, editor_ctx);
                }
                if let Some(ref text) = change.inserted_text {
                    textarea.insert_str(text);
                }
                state.replaying = false;
                // Restore the last_change (replay may have overwritten it)
                state.last_change = Some(change);
            }
            VimAction::Consumed
        }

        // Operator-pending triggers
        KeyCode::Char(op @ ('d' | 'c' | 'y')) => {
            state.start_recording(key);
            textarea.start_selection();
            state.pending_operator = Some(op);
            // Store count for operator use (e.g., 2dw means dw twice)
            if count > 1 {
                state.pending_count = Some(count);
            }
            VimAction::Consumed
        }

        _ => VimAction::Unhandled,
    };

    // Any key that didn't early-return (i.e., not j/k vertical motion) clears curswant.
    state.desired_col = None;
    result
}

/// Handle the second key after 'g' prefix.
fn handle_g_prefix(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    key: KeyEvent,
) -> VimAction {
    match key.code {
        KeyCode::Char('g') => {
            // gg = go to top
            textarea.move_cursor(CursorMove::Top);
            textarea.move_cursor(CursorMove::Head);
            VimAction::Consumed
        }
        _ => {
            // Unknown g-sequence; cancel and pass through
            state.pending_count = None;
            VimAction::Consumed
        }
    }
}

/// Handle character replacement after 'r' prefix.
fn handle_replace_char(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    key: KeyEvent,
) -> VimAction {
    match key.code {
        KeyCode::Char(ch) => {
            state.record_key(key);
            let (row, col) = textarea.cursor();
            let can_replace = col < textarea.lines()[row].len();
            if can_replace {
                textarea.delete_next_char();
                textarea.insert_char(ch);
                textarea.move_cursor(CursorMove::Back);
            }
            state.finalize_change();
            VimAction::Consumed
        }
        KeyCode::Esc => {
            state.recording_change = None;
            VimAction::Consumed
        }
        _ => {
            state.recording_change = None;
            VimAction::Consumed
        }
    }
}
