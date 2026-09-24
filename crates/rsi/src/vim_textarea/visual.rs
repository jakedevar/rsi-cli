//! Visual mode handling for vim emulation.

use super::motions::{
    execute_char_search, move_cursor_to, move_paragraph_backward, move_paragraph_forward,
    move_to_first_non_blank, move_to_matching_bracket,
};
use super::state::{CharSearch, CharSearchDir, VimAction, VimState, VisualMode};
use crossterm::event::{KeyCode, KeyEvent};
use tui_textarea::{CursorMove, TextArea};

/// Handle keys while in visual mode (v or V).
/// Motions extend the selection. Operators act on the selection immediately.
pub(super) fn handle_visual_mode(
    textarea: &mut TextArea<'static>,
    state: &mut VimState,
    key: KeyEvent,
    count: usize,
) -> VimAction {
    let exit_visual = |state: &mut VimState, textarea: &mut TextArea<'static>| {
        state.visual = None;
        state.visual_anchor = None;
        textarea.cancel_selection();
    };

    match key.code {
        // Operators act on the visual selection immediately
        KeyCode::Char('d') | KeyCode::Char('x') => {
            textarea.cut();
            state.visual = None;
            state.visual_anchor = None;
            VimAction::Consumed
        }
        KeyCode::Char('c') | KeyCode::Char('s') => {
            textarea.cut();
            state.visual = None;
            state.visual_anchor = None;
            VimAction::EnteredInsert
        }
        KeyCode::Char('y') => {
            textarea.copy();
            crate::clipboard::osc52_copy(&textarea.yank_text());
            exit_visual(state, textarea);
            VimAction::Consumed
        }

        // Esc exits visual mode
        KeyCode::Esc => {
            exit_visual(state, textarea);
            VimAction::Consumed
        }

        // v toggles: if already in char visual, exit; if in line visual, switch to char
        KeyCode::Char('v') => {
            match state.visual {
                Some(VisualMode::Char) => {
                    exit_visual(state, textarea);
                }
                Some(VisualMode::Line) => {
                    state.visual = Some(VisualMode::Char);
                    // Re-start selection from anchor for char mode
                    textarea.cancel_selection();
                    if let Some((ar, ac)) = state.visual_anchor {
                        move_cursor_to(textarea, ar, ac);
                    }
                    textarea.start_selection();
                }
                None => unreachable!(),
            }
            VimAction::Consumed
        }
        // V toggles: if already in line visual, exit; if in char visual, switch to line
        KeyCode::Char('V') => {
            match state.visual {
                Some(VisualMode::Line) => {
                    exit_visual(state, textarea);
                }
                Some(VisualMode::Char) => {
                    state.visual = Some(VisualMode::Line);
                    // Extend selection to full lines
                }
                None => unreachable!(),
            }
            VimAction::Consumed
        }

        // Text objects in visual mode (viw, va", etc.)
        KeyCode::Char(prefix @ ('i' | 'a')) => {
            state.pending_textobj_prefix = Some(prefix);
            // Set a temporary operator so the text object handler knows to apply
            // For visual mode text objects, we re-select the range
            VimAction::Consumed
        }

        // Motions extend the selection (selection is already active)
        KeyCode::Char('h') | KeyCode::Left => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Back);
            }
            VimAction::Consumed
        }
        KeyCode::Char('j') | KeyCode::Down => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Down);
            }
            VimAction::Consumed
        }
        KeyCode::Char('k') | KeyCode::Up => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Up);
            }
            VimAction::Consumed
        }
        KeyCode::Char('l') | KeyCode::Right => {
            for _ in 0..count {
                textarea.move_cursor(CursorMove::Forward);
            }
            VimAction::Consumed
        }
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
        KeyCode::Char('G') => {
            textarea.move_cursor(CursorMove::Bottom);
            VimAction::Consumed
        }
        KeyCode::Char('g') => {
            state.pending_g = true;
            VimAction::Consumed
        }
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
        KeyCode::Char('%') => {
            move_to_matching_bracket(textarea);
            VimAction::Consumed
        }
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

        // Any other key — consume in visual mode
        _ => VimAction::Consumed,
    }
}
