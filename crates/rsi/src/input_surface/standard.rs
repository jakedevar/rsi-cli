//! Standard (non-modal) editing for [`InputSurface`] (#1628).
//!
//! Typing inserts text directly; there is no Normal mode. Esc clears a
//! selection; with nothing to clear the composer's caller returns to the list.
//! Everything an operator expects from an ordinary text box works: arrows,
//! Home/End, Ctrl/Alt word moves, Shift selection, Backspace/Delete,
//! Ctrl-A select all, Ctrl-C/X copy/cut, Ctrl-Z undo, Enter/Shift-Enter.
//!
//! The surface stays in `PopupMode::Insert` so every renderer and caller that
//! reads the mode sees a text-entry surface. Keys that do not edit (an empty
//! draft's arrows, Esc with nothing to dismiss) are returned as
//! [`InputAction::Passthrough`] when the caller asked for pass-through.

use super::{
    InputAction, InputSurface, InputSurfaceConfig, handle_suggestion_keys, update_suggestions,
};
use crate::types::PopupMode;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::CursorMove;

/// The selected text, when the selection is non-empty.
pub(crate) fn selected_text(surface: &InputSurface) -> Option<String> {
    let ((start_row, start_col), (end_row, end_col)) = surface.textarea.selection_range()?;
    if (start_row, start_col) == (end_row, end_col) {
        return None;
    }
    let lines = surface.textarea.lines();
    let mut out = String::new();
    for row in start_row..=end_row {
        let line = lines.get(row)?;
        let from = if row == start_row { start_col } else { 0 };
        let to = if row == end_row {
            end_col
        } else {
            line.chars().count()
        };
        out.extend(line.chars().skip(from).take(to.saturating_sub(from)));
        if row != end_row {
            out.push('\n');
        }
    }
    Some(out)
}

/// Whether the surface has a non-empty selection (Ctrl-C copies only then).
pub(crate) fn has_selection(surface: &InputSurface) -> bool {
    selected_text(surface).is_some()
}

/// Put the surface in the state Standard editing expects: always inserting,
/// with no half-typed vim command. Idempotent; this is what makes a live
/// Vim to Standard switch take effect on the next key.
pub(crate) fn enter_standard(surface: &mut InputSurface) {
    surface.standard_editing = true;
    if surface.mode != PopupMode::Insert || !surface.vim_state.is_idle() {
        surface.mode = PopupMode::Insert;
        surface.vim_state.reset_transient();
        surface.textarea.cancel_selection();
    }
}

/// Copy the selection into the textarea's yank buffer and the system
/// clipboard. Returns whether anything was copied.
fn copy_selection(surface: &mut InputSurface) -> bool {
    // Set the yank buffer by hand: `TextArea::copy` would drop the selection,
    // and a copy leaves it in place in an ordinary editor.
    let Some(text) = selected_text(surface) else {
        return false;
    };
    surface.textarea.set_yank_text(text.clone());
    crate::clipboard::osc52_copy(&text);
    true
}

fn passthrough_or_consumed(key: KeyEvent, config: &InputSurfaceConfig<'_>) -> InputAction {
    if config.pass_through_unhandled {
        InputAction::Passthrough(key)
    } else {
        InputAction::Consumed
    }
}

/// Move the cursor, extending the selection when `extend` and otherwise
/// dropping it.
fn move_cursor(surface: &mut InputSurface, movement: CursorMove, extend: bool) {
    if extend {
        if !surface.textarea.is_selecting() {
            surface.textarea.start_selection();
        }
    } else {
        surface.textarea.cancel_selection();
    }
    surface.textarea.move_cursor(movement);
}

/// Vertical move by one wrapped row. Returns whether the cursor moved.
fn move_vertical(surface: &mut InputSurface, delta: isize, extend: bool) -> bool {
    let before = surface.textarea.cursor();
    if extend {
        if !surface.textarea.is_selecting() {
            surface.textarea.start_selection();
        }
    } else {
        surface.textarea.cancel_selection();
    }
    let wrap_width = surface.wrap_width.get();
    crate::vim_textarea::move_vertical_with_curswant(
        &mut surface.textarea,
        &mut surface.vim_state,
        delta,
        Some(wrap_width),
        true,
    );
    surface.textarea.cursor() != before
}

pub(super) fn handle_standard_key(
    surface: &mut InputSurface,
    key: KeyEvent,
    config: &InputSurfaceConfig<'_>,
) -> InputAction {
    enter_standard(surface);

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Non-pass-through surfaces stay quittable with Ctrl+Q, as in vim insert.
    if !config.pass_through_unhandled
        && key.code == KeyCode::Char('q')
        && key.modifiers == KeyModifiers::CONTROL
    {
        return InputAction::Close;
    }

    if let Some(action) = handle_suggestion_keys(surface, key, config) {
        return action;
    }

    // Plain Enter follows the surface's primary action; Shift+Enter (and
    // plain Enter on a legacy surface) inserts a newline.
    if key.code == KeyCode::Enter {
        if config.submit_on_enter && key.modifiers == KeyModifiers::NONE {
            return InputAction::Submit(surface.content_for_send());
        }
        if !alt {
            surface.textarea.insert_newline();
            update_suggestions(surface, config.available_commands, config.working_dir);
        }
        return InputAction::Consumed;
    }

    // Esc dismisses a selection. With nothing to dismiss there is no Normal
    // mode to fall back to: a pass-through surface (the composer) hands the key
    // to its caller, and a closable overlay surface closes.
    if key.code == KeyCode::Esc {
        if surface.textarea.is_selecting() {
            surface.textarea.cancel_selection();
            return InputAction::Consumed;
        }
        if config.pass_through_unhandled {
            return InputAction::Passthrough(key);
        }
        return InputAction::Close;
    }

    let empty = !surface.has_content();
    let word = ctrl || alt;
    let mut consumed_edit = true;

    match key.code {
        // Select all.
        KeyCode::Char('a' | 'A') if ctrl && !alt && !shift => {
            surface.textarea.cancel_selection();
            surface.textarea.select_all();
        }
        // Copy: a no-op without a selection (the global Ctrl-C meaning is
        // decided one layer up, before this key reaches the surface).
        KeyCode::Char('c' | 'C') if ctrl && !alt => {
            if !copy_selection(surface) {
                return passthrough_or_consumed(key, config);
            }
            return InputAction::Consumed;
        }
        KeyCode::Char('x' | 'X') if ctrl && !alt => {
            if copy_selection(surface) {
                surface.textarea.cut();
            }
        }
        // Paste is owned by the caller (it needs the app clipboard path).
        KeyCode::Char('v' | 'V') if ctrl => return InputAction::Consumed,
        KeyCode::Char('z' | 'Z') if ctrl && !alt => {
            if shift {
                surface.textarea.redo();
            } else {
                surface.textarea.undo();
            }
        }
        KeyCode::Left | KeyCode::Right => {
            let forward = key.code == KeyCode::Right;
            // Plain Left/Right on an empty draft belong to session navigation.
            if empty && !word {
                return passthrough_or_consumed(key, config);
            }
            let selection = surface.textarea.selection_range();
            match (selection, word, shift) {
                // Collapse a selection to its near edge, like any editor.
                (Some((start, end)), false, false) if start != end => {
                    surface.textarea.cancel_selection();
                    let (row, col) = if forward { end } else { start };
                    surface
                        .textarea
                        .move_cursor(CursorMove::Jump(row as u16, col as u16));
                }
                _ => {
                    let movement = match (forward, word) {
                        (true, true) => CursorMove::WordForward,
                        (false, true) => CursorMove::WordBack,
                        (true, false) => CursorMove::Forward,
                        (false, false) => CursorMove::Back,
                    };
                    move_cursor(surface, movement, shift);
                }
            }
            surface.vim_state.desired_col = None;
        }
        KeyCode::Home => {
            let movement = if ctrl {
                CursorMove::Top
            } else {
                CursorMove::Head
            };
            move_cursor(surface, movement, shift);
            surface.vim_state.desired_col = None;
        }
        KeyCode::End => {
            let movement = if ctrl {
                CursorMove::Bottom
            } else {
                CursorMove::End
            };
            move_cursor(surface, movement, shift);
            surface.vim_state.desired_col = None;
        }
        KeyCode::Up | KeyCode::Down => {
            let delta = if key.code == KeyCode::Down { 1 } else { -1 };
            // On an empty draft, or already at the first/last row, the arrow
            // belongs to the transcript scroll.
            if empty && !shift {
                return passthrough_or_consumed(key, config);
            }
            let moved = move_vertical(surface, delta, shift);
            if !moved && !shift {
                return passthrough_or_consumed(key, config);
            }
        }
        KeyCode::Backspace if ctrl && !alt => {
            if surface.textarea.is_selecting() {
                surface.textarea.delete_char();
            } else {
                surface.textarea.delete_word();
            }
        }
        KeyCode::Delete if ctrl && !alt => {
            if surface.textarea.is_selecting() {
                surface.textarea.delete_next_char();
            } else {
                surface.textarea.delete_next_word();
            }
        }
        _ => {
            consumed_edit = surface.textarea.input(key);
        }
    }
    let _ = consumed_edit;

    update_suggestions(surface, config.available_commands, config.working_dir);
    InputAction::Consumed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn config(pass_through: bool) -> InputSurfaceConfig<'static> {
        InputSurfaceConfig {
            pass_through_unhandled: pass_through,
            available_commands: &[],
            working_dir: None,
            submit_on_enter: true,
            standard_editing: true,
        }
    }

    fn surface(text: &str) -> InputSurface {
        let mut s =
            InputSurface::new_insert_with_content(text.split('\n').map(str::to_string).collect());
        s.standard_editing = true;
        s.textarea.move_cursor(CursorMove::Bottom);
        s.textarea.move_cursor(CursorMove::End);
        s
    }

    fn press(s: &mut InputSurface, code: KeyCode, mods: KeyModifiers) -> InputAction {
        super::super::handle_key(s, KeyEvent::new(code, mods), &config(true))
    }

    fn typed(s: &mut InputSurface, text: &str) {
        for ch in text.chars() {
            press(s, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    #[test]
    fn typing_inserts_directly_and_never_leaves_insert() {
        let mut s = InputSurface::default(); // starts in Normal mode
        s.standard_editing = true;
        typed(&mut s, "jk hello");
        assert_eq!(s.content(), "jk hello");
        assert_eq!(s.mode, PopupMode::Insert);
        // Esc with nothing to dismiss goes to the caller, not to Normal mode.
        assert!(matches!(
            press(&mut s, KeyCode::Esc, KeyModifiers::NONE),
            InputAction::Passthrough(_)
        ));
        assert_eq!(s.mode, PopupMode::Insert);
    }

    #[test]
    fn arrows_home_end_and_edits() {
        let mut s = surface("hello");
        press(&mut s, KeyCode::Left, KeyModifiers::NONE);
        press(&mut s, KeyCode::Left, KeyModifiers::NONE);
        typed(&mut s, "X");
        assert_eq!(s.content(), "helXlo");
        press(&mut s, KeyCode::Home, KeyModifiers::NONE);
        typed(&mut s, ">");
        assert_eq!(s.content(), ">helXlo");
        press(&mut s, KeyCode::End, KeyModifiers::NONE);
        press(&mut s, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(s.content(), ">helXl");
        press(&mut s, KeyCode::Home, KeyModifiers::NONE);
        press(&mut s, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(s.content(), "helXl");
    }

    #[test]
    fn ctrl_and_alt_arrows_move_by_word() {
        for mods in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            let mut s = surface("alpha beta gamma");
            press(&mut s, KeyCode::Left, mods);
            assert_eq!(s.textarea.cursor(), (0, 11), "{mods:?} back one word");
            press(&mut s, KeyCode::Left, mods);
            assert_eq!(s.textarea.cursor(), (0, 6));
            press(&mut s, KeyCode::Right, mods);
            assert_eq!(s.textarea.cursor().0, 0);
            assert!(s.textarea.cursor().1 > 6, "{mods:?} forward a word");
        }
    }

    #[test]
    fn ctrl_backspace_and_delete_remove_a_word() {
        let mut s = surface("alpha beta");
        press(&mut s, KeyCode::Backspace, KeyModifiers::CONTROL);
        assert_eq!(s.content(), "alpha ");
        press(&mut s, KeyCode::Home, KeyModifiers::NONE);
        press(&mut s, KeyCode::Delete, KeyModifiers::CONTROL);
        assert_eq!(s.content(), " ");
    }

    #[test]
    fn ctrl_a_selects_all_and_typing_replaces_it() {
        let mut s = surface("one\ntwo");
        press(&mut s, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(selected_text(&s).as_deref(), Some("one\ntwo"));
        typed(&mut s, "x");
        assert_eq!(s.content(), "x");
        assert!(!has_selection(&s));
    }

    #[test]
    fn shift_arrows_select_and_plain_arrows_collapse() {
        let mut s = surface("hello world");
        press(&mut s, KeyCode::Left, KeyModifiers::SHIFT);
        press(&mut s, KeyCode::Left, KeyModifiers::SHIFT);
        assert_eq!(selected_text(&s).as_deref(), Some("ld"));
        press(&mut s, KeyCode::Left, KeyModifiers::NONE);
        assert!(!has_selection(&s));
        assert_eq!(s.textarea.cursor(), (0, 9), "Left collapses to the start");
        press(
            &mut s,
            KeyCode::Right,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(has_selection(&s));
        press(&mut s, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(s.content(), "hello wor");
    }

    #[test]
    fn ctrl_c_copies_only_a_selection_and_ctrl_x_cuts() {
        let mut s = surface("hello world");
        // No selection: nothing is copied and the key goes to the caller.
        assert!(matches!(
            press(&mut s, KeyCode::Char('c'), KeyModifiers::CONTROL),
            InputAction::Passthrough(_)
        ));
        press(&mut s, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert!(matches!(
            press(&mut s, KeyCode::Char('c'), KeyModifiers::CONTROL),
            InputAction::Consumed
        ));
        assert_eq!(s.textarea.yank_text(), "hello world");
        assert_eq!(s.content(), "hello world");
        press(&mut s, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(s.content(), "");
        press(&mut s, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(s.content(), "hello world");
    }

    #[test]
    fn enter_submits_and_shift_enter_inserts_a_newline() {
        let mut s = surface("a");
        press(&mut s, KeyCode::Enter, KeyModifiers::SHIFT);
        typed(&mut s, "b");
        assert_eq!(s.textarea.lines(), &["a", "b"]);
        match press(&mut s, KeyCode::Enter, KeyModifiers::NONE) {
            InputAction::Submit(text) => assert_eq!(text, "a b"),
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn vertical_arrows_move_within_a_draft_and_scroll_at_its_edges() {
        let mut s = surface("one\ntwo");
        assert!(matches!(
            press(&mut s, KeyCode::Down, KeyModifiers::NONE),
            InputAction::Passthrough(_)
        ));
        assert!(matches!(
            press(&mut s, KeyCode::Up, KeyModifiers::NONE),
            InputAction::Consumed
        ));
        assert_eq!(s.textarea.cursor().0, 0);
        assert!(matches!(
            press(&mut s, KeyCode::Up, KeyModifiers::NONE),
            InputAction::Passthrough(_)
        ));
    }

    #[test]
    fn empty_draft_left_right_pass_through_to_session_navigation() {
        let mut s = surface("");
        for code in [KeyCode::Left, KeyCode::Right, KeyCode::Up, KeyCode::Down] {
            assert!(matches!(
                press(&mut s, code, KeyModifiers::NONE),
                InputAction::Passthrough(_)
            ));
        }
    }

    #[test]
    fn a_vim_surface_switched_to_standard_leaves_normal_mode() {
        let mut s = InputSurface::default(); // Normal mode, vim state
        s.textarea.insert_str("abc");
        s.standard_editing = true;
        typed(&mut s, "d");
        assert_eq!(s.content(), "abcd");
        assert_eq!(s.mode, PopupMode::Insert);
    }
}
