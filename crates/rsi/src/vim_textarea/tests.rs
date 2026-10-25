//! Tests for vim textarea emulation.

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::{CursorMove, TextArea};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn make_textarea(text: &str) -> TextArea<'static> {
    let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    if lines.is_empty() {
        TextArea::default()
    } else {
        TextArea::new(lines)
    }
}

fn text_content(ta: &TextArea<'_>) -> String {
    ta.lines().join("\n")
}

fn surface_key(surface: &mut crate::input_surface::InputSurface, code: KeyCode) {
    let config = crate::input_surface::InputSurfaceConfig {
        pass_through_unhandled: false,
        available_commands: &[],
        working_dir: None,
        submit_on_enter: false,
        standard_editing: false,
    };
    assert!(matches!(
        crate::input_surface::handle_key(surface, key(code), &config),
        crate::input_surface::InputAction::Consumed
    ));
}

fn change_and_escape(text: &str, col: usize, command: &str) -> String {
    let mut surface = crate::input_surface::InputSurface::default();
    *surface.textarea = make_textarea(text);
    motions::move_cursor_to(&mut surface.textarea, 0, col);
    for ch in command.chars() {
        surface_key(&mut surface, KeyCode::Char(ch));
    }
    assert_eq!(surface.mode, crate::types::PopupMode::Insert);
    surface_key(&mut surface, KeyCode::Char('X'));
    surface_key(&mut surface, KeyCode::Esc);
    assert_eq!(surface.mode, crate::types::PopupMode::Normal);
    text_content(&surface.textarea)
}

// ========================
// Existing Phase 1 tests
// ========================

#[test]
fn test_motions_consumed() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('l')), None),
        VimAction::Consumed
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None),
        VimAction::Consumed
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('b')), None),
        VimAction::Consumed
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('$')), None),
        VimAction::Consumed
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('0')), None),
        VimAction::Consumed
    );
}

#[test]
fn test_insert_mode_transitions() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('i')), None),
        VimAction::EnteredInsert
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('a')), None),
        VimAction::EnteredInsert
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('A')), None),
        VimAction::EnteredInsert
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('I')), None),
        VimAction::EnteredInsert
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('o')), None),
        VimAction::EnteredInsert
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('O')), None),
        VimAction::EnteredInsert
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('C')), None),
        VimAction::EnteredInsert
    );
}

#[test]
fn test_unhandled_keys() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('q')), None),
        VimAction::Unhandled
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('z')), None),
        VimAction::Unhandled
    );
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Esc), None),
        VimAction::Unhandled
    );
}

#[test]
fn test_dd_deletes_line() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();

    // 'd' starts operator-pending
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None),
        VimAction::Consumed
    );
    assert_eq!(state.pending_operator, Some('d'));

    // second 'd' completes dd
    assert_eq!(
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None),
        VimAction::Consumed
    );
    assert_eq!(state.pending_operator, None);
    assert!(ta.lines().join("").is_empty());
}

#[test]
fn test_dd_cursor_moves_up_to_previous_line() {
    // dd on a non-first line should shift cursor up to the line above
    let mut ta = make_textarea("line1\nline2\nline3");
    let mut state = VimState::default();

    // Move to line2 (row 1)
    ta.move_cursor(CursorMove::Down);
    assert_eq!(ta.cursor().0, 1);

    // dd — delete "line2", cursor should move up to line1 (row 0)
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);

    assert_eq!(text_content(&ta), "line1\nline3");
    assert_eq!(
        ta.cursor().0,
        0,
        "cursor should move up to line above after dd"
    );
}

#[test]
fn test_dd_cursor_stays_at_top_when_on_first_line() {
    // dd on the first line should keep cursor at row 0 (can't go higher)
    let mut ta = make_textarea("line1\nline2\nline3");
    let mut state = VimState::default();

    // dd — delete "line1", cursor should stay at row 0
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);

    assert_eq!(text_content(&ta), "line2\nline3");
    assert_eq!(
        ta.cursor().0,
        0,
        "cursor should stay at row 0 when deleting first line"
    );
}

#[test]
fn test_dd_last_line_cursor_moves_up() {
    // dd on the last line should move cursor up
    let mut ta = make_textarea("line1\nline2\nline3");
    let mut state = VimState::default();

    // Move to line3 (row 2)
    ta.move_cursor(CursorMove::Down);
    ta.move_cursor(CursorMove::Down);
    assert_eq!(ta.cursor().0, 2);

    // dd — delete "line3", cursor should move up to line2 (row 1)
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);

    // tui_textarea leaves a trailing empty line when deleting the last line
    // (selection goes to End, not through a newline), so we get "line1\nline2\n"
    assert_eq!(ta.lines().len(), 3); // line1, line2, empty
    assert_eq!(ta.lines()[0], "line1");
    assert_eq!(ta.lines()[1], "line2");
    assert_eq!(ta.lines()[2], "");
    assert_eq!(
        ta.cursor().0,
        1,
        "cursor should move up after deleting last line"
    );
}

#[test]
fn test_dw_deletes_word() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();

    // Move to start
    ta.move_cursor(CursorMove::Head);

    // dw
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    assert_eq!(ta.lines().join(""), "world");
}

#[test]
fn test_cw_deletes_word_and_enters_insert() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('c')), None);
    let action = handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    assert_eq!(action, VimAction::EnteredInsert);
    assert_eq!(ta.lines().join(""), " world");
}

#[test]
fn test_cw_last_word_preserves_line_break() {
    for first_line in ["hello world", "hello a", "a", "hello café"] {
        let col = first_line
            .chars()
            .position(|ch| ch == ' ')
            .map_or(0, |c| c + 1);
        let prefix: String = first_line.chars().take(col).collect();
        for suffix in ["", "\nsecond line"] {
            let text = format!("{first_line}{suffix}");
            assert_eq!(
                change_and_escape(&text, col, "cw"),
                format!("{prefix}X{suffix}"),
                "cw on {text:?}"
            );
        }
    }
}

#[test]
fn test_cw_keeps_following_spaces_and_word_boundaries() {
    for (text, col, expected) in [
        ("hello world  \nsecond line", 6, "hello X  \nsecond line"),
        ("hello world", 0, "X world"),
        ("hello world", 4, "hellX world"),
        ("one   two\nsecond line", 3, "oneXtwo\nsecond line"),
        ("one_two.rest\nsecond line", 0, "X.rest\nsecond line"),
        ("one...rest\nsecond line", 3, "oneXrest\nsecond line"),
    ] {
        assert_eq!(change_and_escape(text, col, "cw"), expected, "{text:?}");
    }
}

#[test]
fn test_counted_cw_changes_through_last_word_end() {
    for command in ["2cw", "c2w"] {
        assert_eq!(
            change_and_escape("one two three\nsecond line", 0, command),
            "X three\nsecond line"
        );
        assert_eq!(
            change_and_escape("one two\nsecond line", 0, command),
            "X\nsecond line"
        );
    }
}

#[test]
fn test_cw_and_word_change_share_line_end_boundaries() {
    for suffix in ["", "\n  second.line"] {
        for command in ["$cw", "$cW"] {
            assert_eq!(
                change_and_escape(&format!("one two.three{suffix}"), 0, command),
                format!("one two.threX{suffix}"),
                "{command} with suffix {suffix:?}"
            );
        }
        for command in ["2cW", "c2W"] {
            assert_eq!(
                change_and_escape(&format!("one.two three.four  {suffix}"), 0, command),
                format!("X  {suffix}"),
                "{command} with suffix {suffix:?}"
            );
        }
    }
}

#[test]
fn test_dollar_stops_on_last_character_and_x_deletes_it() {
    for first_line in ["hello world", "hello a", "a", "hello café", ""] {
        let len = first_line.chars().count();
        let shortened: String = first_line.chars().take(len.saturating_sub(1)).collect();
        for suffix in ["", "\nsecond line"] {
            let mut ta = make_textarea(&format!("{first_line}{suffix}"));
            let mut state = VimState::default();
            for _ in 0..2 {
                handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('$')), None);
                assert_eq!(ta.cursor(), (0, len.saturating_sub(1)), "{first_line:?}");
            }
            // x on an empty line is a separate motion edge case; this test pins $ there.
            if len > 0 {
                handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('x')), None);
                assert_eq!(text_content(&ta), format!("{shortened}{suffix}"));
            }
        }
    }
}

#[test]
fn test_operator_dollar_still_includes_last_character() {
    for suffix in ["", "\nsecond line"] {
        let text = format!("hello world{suffix}");
        assert_eq!(
            change_and_escape(&text, 6, "c$"),
            format!("hello X{suffix}")
        );
        let mut ta = make_textarea(&text);
        let mut state = VimState::default();
        motions::move_cursor_to(&mut ta, 0, 6);
        for ch in ['d', '$'] {
            handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(ch)), None);
        }
        assert_eq!(text_content(&ta), format!("hello {suffix}"));
    }
}

#[test]
fn test_ce_retains_inclusive_word_end_semantics() {
    for suffix in ["", "\nsecond line"] {
        assert_eq!(
            change_and_escape(&format!("hello world{suffix}"), 6, "ce"),
            format!("hello X{suffix}")
        );
        assert_eq!(
            change_and_escape(&format!("hello a{suffix}"), 6, "ce"),
            if suffix.is_empty() {
                "hello X"
            } else {
                "hello X line"
            }
        );
    }
}

#[test]
fn test_append_end_keeps_insert_position_after_last_character() {
    for first_line in ["hello world", "hello a", "a", "hello café", ""] {
        for suffix in ["", "\nsecond line"] {
            assert_eq!(
                change_and_escape(&format!("{first_line}{suffix}"), 0, "A"),
                format!("{first_line}X{suffix}")
            );
        }
    }
}

#[test]
fn test_esc_cancels_pending_operator() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    assert_eq!(state.pending_operator, Some('d'));

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Esc), None);
    assert_eq!(state.pending_operator, None);
    // Text should be unchanged
    assert_eq!(ta.lines().join(""), "hello");
}

#[test]
fn test_delete_to_end() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);
    // Move to 'w' in "world"
    ta.move_cursor(CursorMove::WordForward);

    let action = handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('D')), None);
    assert_eq!(action, VimAction::Consumed);
    assert_eq!(ta.lines().join(""), "hello ");
}

#[test]
fn test_change_to_end_enters_insert() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();

    let action = handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('C')), None);
    assert_eq!(action, VimAction::EnteredInsert);
}

#[test]
fn test_undo_redo() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();

    // Delete char
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('x')), None);
    // Undo
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('u')), None);
    assert_eq!(ta.lines().join(""), "hello");

    // Redo (Ctrl+R)
    handle_vim_normal(&mut ta, &mut state, ctrl_key(KeyCode::Char('r')), None);
}

// ========================
// Phase 2: Count tests
// ========================

#[test]
fn test_count_motion_3w() {
    let mut ta = make_textarea("one two three four five");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // 3w = move forward 3 words
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('3')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    // Should be at "four"
    assert_eq!(ta.cursor().1, 14); // "one two three " = 14 chars
}

#[test]
fn test_count_motion_5j() {
    let mut ta = make_textarea("line0\nline1\nline2\nline3\nline4\nline5\nline6");
    let mut state = VimState::default();

    // 5j = move down 5 lines
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('5')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('j')), None);

    assert_eq!(ta.cursor().0, 5);
}

#[test]
fn test_count_2dd() {
    let mut ta = make_textarea("line1\nline2\nline3");
    let mut state = VimState::default();

    // 2dd = delete 2 lines
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('2')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);

    assert_eq!(text_content(&ta), "line3");
}

#[test]
fn test_count_3x() {
    let mut ta = make_textarea("abcdef");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // 3x = delete 3 chars
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('3')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('x')), None);

    assert_eq!(ta.lines().join(""), "def");
}

#[test]
fn test_count_0_as_head_motion() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::End);

    // 0 alone = go to line start
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('0')), None);
    assert_eq!(ta.cursor().1, 0);
}

#[test]
fn test_count_10j() {
    // Multi-digit count
    let mut ta = make_textarea("0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11");
    let mut state = VimState::default();

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('1')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('0')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('j')), None);

    assert_eq!(ta.cursor().0, 10);
}

// ========================
// Phase 2: gg
// ========================

#[test]
fn test_gg_goes_to_top() {
    let mut ta = make_textarea("line1\nline2\nline3");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Bottom);
    assert_eq!(ta.cursor().0, 2);

    // gg
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('g')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('g')), None);

    assert_eq!(ta.cursor().0, 0);
    assert_eq!(ta.cursor().1, 0);
}

#[test]
fn test_gg_on_first_line() {
    let mut ta = make_textarea("only line");
    let mut state = VimState::default();

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('g')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('g')), None);

    assert_eq!(ta.cursor().0, 0);
}

#[test]
fn test_dgg_deletes_through_first_line_linewise() {
    let mut ta = make_textarea("first\nsecond\nthird\nfourth");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Bottom);
    ta.move_cursor(CursorMove::Up);
    for ch in ['d', 'g', 'g'] {
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(ch)), None);
    }
    assert_eq!(text_content(&ta), "fourth");
    assert_eq!(state.pending_operator, None);
}

#[test]
fn test_cgg_changes_through_first_line_linewise() {
    let mut ta = make_textarea("first\nsecond\nthird\nfourth");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Bottom);
    ta.move_cursor(CursorMove::Up);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('c')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('g')), None);
    let action = handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('g')), None);
    assert_eq!(action, VimAction::EnteredInsert);
    assert_eq!(text_content(&ta), "fourth");
    assert_eq!(state.pending_operator, None);
}

#[test]
fn test_ygg_yanks_through_first_line_linewise() {
    let mut ta = make_textarea("first\nsecond\nthird\nfourth");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Bottom);
    ta.move_cursor(CursorMove::Up);
    for ch in ['y', 'g', 'g'] {
        handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(ch)), None);
    }
    assert_eq!(ta.yank_text(), "first\nsecond\nthird\n");
    assert_eq!(text_content(&ta), "first\nsecond\nthird\nfourth");
    assert_eq!(state.pending_operator, None);
}

// ========================
// Phase 2: ^ motion
// ========================

#[test]
fn test_caret_first_non_blank() {
    let mut ta = make_textarea("   hello");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::End);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('^')), None);
    assert_eq!(ta.cursor().1, 3); // after 3 spaces
}

#[test]
fn test_caret_no_indent() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::End);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('^')), None);
    assert_eq!(ta.cursor().1, 0);
}

// ========================
// Phase 2: s (substitute)
// ========================

#[test]
fn test_s_substitute() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    let action = handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('s')), None);
    assert_eq!(action, VimAction::EnteredInsert);
    assert_eq!(ta.lines().join(""), "ello");
}

// ========================
// Phase 2: J (join lines)
// ========================

#[test]
fn test_join_lines() {
    let mut ta = make_textarea("hello\nworld");
    let mut state = VimState::default();

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('J')), None);
    // Should be joined on one line
    assert_eq!(ta.lines().len(), 1);
}

#[test]
fn test_join_last_line_noop() {
    let mut ta = make_textarea("only");
    let mut state = VimState::default();

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('J')), None);
    assert_eq!(text_content(&ta), "only");
}

// ========================
// Phase 2: ~ (toggle case)
// ========================

#[test]
fn test_toggle_case() {
    let mut ta = make_textarea("Hello");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // ~ on 'H' → 'h', cursor advances
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('~')), None);
    assert_eq!(ta.lines()[0], "hello");
    assert_eq!(ta.cursor().1, 1);

    // ~ on 'e' → 'E'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('~')), None);
    assert_eq!(ta.lines()[0], "hEllo");
}

#[test]
fn test_3_tilde() {
    let mut ta = make_textarea("abc");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('3')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('~')), None);
    assert_eq!(ta.lines()[0], "ABC");
}

// ========================
// Phase 2: r (replace)
// ========================

#[test]
fn test_replace_char() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // r + X should replace 'h' with 'X'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('r')), None);
    assert!(state.pending_replace);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('X')), None);
    assert_eq!(ta.lines()[0], "Xello");
    assert_eq!(ta.cursor().1, 0); // cursor stays
}

#[test]
fn test_replace_char_esc_cancels() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('r')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Esc), None);
    assert_eq!(ta.lines()[0], "hello"); // unchanged
}

// ========================
// Phase 2: { / } (paragraph)
// ========================

#[test]
fn test_paragraph_forward() {
    let mut ta = make_textarea("line1\nline2\n\nline4\nline5");
    let mut state = VimState::default();

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('}')), None);
    assert_eq!(ta.cursor().0, 2); // blank line
}

#[test]
fn test_paragraph_backward() {
    let mut ta = make_textarea("line1\n\nline3\nline4");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Bottom);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('{')), None);
    assert_eq!(ta.cursor().0, 1); // blank line
}

// ========================
// Phase 2: % (matching bracket)
// ========================

#[test]
fn test_matching_bracket_forward() {
    let mut ta = make_textarea("(hello)");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('%')), None);
    assert_eq!(ta.cursor().1, 6); // on ')'
}

#[test]
fn test_matching_bracket_backward() {
    let mut ta = make_textarea("(hello)");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::End);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('%')), None);
    assert_eq!(ta.cursor().1, 0); // on '('
}

#[test]
fn test_matching_bracket_nested() {
    let mut ta = make_textarea("(a (b) c)");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('%')), None);
    assert_eq!(ta.cursor().1, 8); // outer ')'
}

// ========================
// Phase 2: count + operator
// ========================

#[test]
fn test_2dw() {
    let mut ta = make_textarea("one two three four");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // 2dw = delete 2 words (using count on operator)
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('2')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    // "one two " should be deleted, leaving "three four"
    // Wait — actually 2dw: the count goes to the operator.
    // With our implementation, count is consumed before operator is set,
    // then passed to operator-pending. The operator stores count and
    // applies it to the motion.
    // dw deletes "one ", 2dw should delete "one two "
    assert_eq!(ta.lines().join(""), "three four");
}

// ========================
// Phase 3: Text objects with operators
// ========================

#[test]
fn test_diw_deletes_word() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);
    // Move to 'e' in "hello"
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('l')), None);

    // diw
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('i')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    assert_eq!(text_content(&ta), " world");
}

#[test]
fn test_daw_deletes_word_with_space() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // daw — "hello " deleted
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('a')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    assert_eq!(text_content(&ta), "world");
}

#[test]
fn test_ciw_changes_word_enters_insert() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // ciw
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('c')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('i')), None);
    let action = handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    assert_eq!(action, VimAction::EnteredInsert);
    assert_eq!(text_content(&ta), " world");
}

#[test]
fn test_di_quote_deletes_inside_quotes() {
    let mut ta = make_textarea(r#"say "hello" now"#);
    let mut state = VimState::default();
    // Position cursor on 'h' at col 5
    ta.move_cursor(CursorMove::Head);
    for _ in 0..5 {
        ta.move_cursor(CursorMove::Forward);
    }

    // di"
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('i')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('"')), None);

    assert_eq!(text_content(&ta), r#"say "" now"#);
}

#[test]
fn test_da_quote_deletes_including_quotes() {
    let mut ta = make_textarea(r#"say "hello" now"#);
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);
    for _ in 0..5 {
        ta.move_cursor(CursorMove::Forward);
    }

    // da"
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('a')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('"')), None);

    assert_eq!(text_content(&ta), "say  now");
}

#[test]
fn test_ci_paren_changes_inside_parens() {
    let mut ta = make_textarea("fn(body)");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);
    for _ in 0..3 {
        ta.move_cursor(CursorMove::Forward);
    }

    // ci(
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('c')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('i')), None);
    let action = handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('(')), None);

    assert_eq!(action, VimAction::EnteredInsert);
    assert_eq!(text_content(&ta), "fn()");
}

#[test]
fn test_yiw_yanks_word() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // yiw
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('y')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('i')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    // Text unchanged after yank
    assert_eq!(text_content(&ta), "hello world");
    // Paste should give "hello"
    ta.move_cursor(CursorMove::End);
    ta.paste();
    assert!(ta.lines().join("").contains("hello"));
}

#[test]
fn test_textobj_no_match_cancels() {
    let mut ta = make_textarea("no quotes here");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // di" with no quotes — should cancel, text unchanged
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('i')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('"')), None);

    assert_eq!(text_content(&ta), "no quotes here");
    assert_eq!(state.pending_operator, None);
}

// ========================
// Phase 5: f/t/F/T char search
// ========================

#[test]
fn test_f_forward_to() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // fo — move to 'o' in "hello"
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('f')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('o')), None);

    assert_eq!(ta.cursor().1, 4); // 'o' in "hello" at col 4
}

#[test]
fn test_f_forward_to_second() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // fw — move to 'w' in "world"
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('f')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    assert_eq!(ta.cursor().1, 6);
}

#[test]
fn test_t_forward_till() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // tw — move to char before 'w'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('t')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    assert_eq!(ta.cursor().1, 5); // space before 'w'
}

#[test]
fn test_big_f_backward() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::End);

    // Fh — backward to 'h'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('F')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('h')), None);

    assert_eq!(ta.cursor().1, 0);
}

#[test]
fn test_big_t_backward_till() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::End);

    // Th — move to char after 'h'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('T')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('h')), None);

    assert_eq!(ta.cursor().1, 1); // char after 'h'
}

#[test]
fn test_t_forward_till_two_chars_away() {
    // Regression: t was landing 2 chars before target instead of 1
    let mut ta = make_textarea("abcdef");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // tc — target 'c' at index 2, should land on index 1 ('b')
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('t')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('c')), None);

    assert_eq!(ta.cursor().1, 1); // 'b', one char before 'c'
}

#[test]
fn test_t_forward_till_three_chars_away() {
    let mut ta = make_textarea("abcdef");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // td — target 'd' at index 3, should land on index 2 ('c')
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('t')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);

    assert_eq!(ta.cursor().1, 2);
}

#[test]
fn test_t_forward_till_adjacent_is_noop() {
    let mut ta = make_textarea("abcdef");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // tb — target 'b' is at index 1, landing would be index 0 (same as cursor), no-op
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('t')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('b')), None);

    assert_eq!(ta.cursor().1, 0); // stays put
}

#[test]
fn test_big_t_backward_till_two_chars_away() {
    let mut ta = make_textarea("abcdef");
    let mut state = VimState::default();
    // Put cursor at 'd' (index 3)
    ta.move_cursor(CursorMove::Head);
    for _ in 0..3 {
        ta.move_cursor(CursorMove::Forward);
    }
    assert_eq!(ta.cursor().1, 3);

    // Tb — target 'b' at index 1, should land on index 2 ('c')
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('T')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('b')), None);

    assert_eq!(ta.cursor().1, 2); // 'c', one char after 'b'
}

#[test]
fn test_big_t_backward_till_adjacent_is_noop() {
    let mut ta = make_textarea("abcdef");
    let mut state = VimState::default();
    // Put cursor at 'b' (index 1)
    ta.move_cursor(CursorMove::Head);
    ta.move_cursor(CursorMove::Forward);
    assert_eq!(ta.cursor().1, 1);

    // Ta — target 'a' at index 0, landing would be index 1 (same as cursor), no-op
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('T')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('a')), None);

    assert_eq!(ta.cursor().1, 1); // stays put
}

#[test]
fn test_semicolon_repeats_t() {
    let mut ta = make_textarea("abcabcabc");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // ta — first 'a' after cursor is at index 3, land on 2
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('t')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('a')), None);
    assert_eq!(ta.cursor().1, 2);

    // ; — repeat t, next 'a' is at index 6, land on 5
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(';')), None);
    assert_eq!(ta.cursor().1, 5);
}

#[test]
fn test_semicolon_repeats_f() {
    let mut ta = make_textarea("abcabc");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // fa — move to first 'a' (which is at 0, so searches forward to next 'a')
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('f')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('b')), None);
    assert_eq!(ta.cursor().1, 1); // first 'b'

    // ; — repeat, move to next 'b'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(';')), None);
    assert_eq!(ta.cursor().1, 4); // second 'b'
}

#[test]
fn test_comma_reverses_f() {
    let mut ta = make_textarea("abcabc");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // fb — to first 'b'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('f')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('b')), None);
    assert_eq!(ta.cursor().1, 1);

    // ; — to second 'b'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(';')), None);
    assert_eq!(ta.cursor().1, 4);

    // , — reverse, back to first 'b'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(',')), None);
    assert_eq!(ta.cursor().1, 1);
}

#[test]
fn test_t_till_first_of_duplicate_adjacent() {
    let mut ta = make_textarea("abbc");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // tb — target 'b', should stay at 'a' (index 0) because index 0 is before the first 'b'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('t')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('b')), None);
    assert_eq!(ta.cursor().1, 0);

    // ; — repeat last search, should land on index 1 ('b') because that is the char before the second 'b'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(';')), None);
    assert_eq!(ta.cursor().1, 1);
}

#[test]
fn test_big_t_till_first_of_duplicate_adjacent() {
    let mut ta = make_textarea("aab");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::End); // cursor at index 2 ('b')

    // Ta — target 'a', should stay at 'b' (index 2) because index 2 is after the second 'a' (index 1)
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('T')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('a')), None);
    assert_eq!(ta.cursor().1, 2);

    // ; — repeat last search, should land on index 1 ('a') because that is the char after the first 'a' (index 0)
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(';')), None);
    assert_eq!(ta.cursor().1, 1);
}

#[test]
fn test_dt_deletes_till_char() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // dto — delete from 'h' till 'o' (i.e. 'h', 'e', 'l', 'l')
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('t')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('o')), None);

    assert_eq!(text_content(&ta), "o world");
}

#[test]
fn test_df_deletes_to_char() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // dfo — delete from 'h' to and including 'o'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('f')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('o')), None);

    // Should delete "hello" (h through o inclusive)
    assert_eq!(text_content(&ta), " world");
}

#[test]
fn test_f_no_match_noop() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // fz — no 'z' in "hello"
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('f')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('z')), None);

    assert_eq!(ta.cursor().1, 0); // didn't move
}

// ========================
// Phase 4: Visual mode
// ========================

#[test]
fn test_v_enters_visual_char() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('v')), None);
    assert_eq!(state.visual, Some(VisualMode::Char));
    assert_eq!(state.visual_anchor, Some((0, 0)));
}

#[test]
fn test_v_motion_d_deletes_selection() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // v + w + d = select a word then delete
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('v')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);

    assert_eq!(state.visual, None);
    // Should have deleted "hello " (selection from 0 to after w motion)
    // The exact result depends on tui-textarea's selection behavior
    assert!(!ta.lines().join("").contains("hello"));
}

#[test]
fn test_big_v_enters_visual_line() {
    let mut ta = make_textarea("hello\nworld");
    let mut state = VimState::default();

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('V')), None);
    assert_eq!(state.visual, Some(VisualMode::Line));
}

#[test]
fn test_v_esc_exits_visual() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('v')), None);
    assert!(state.visual.is_some());

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Esc), None);
    assert_eq!(state.visual, None);
    assert_eq!(state.visual_anchor, None);
}

#[test]
fn test_v_v_exits_visual() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('v')), None);
    assert!(state.visual.is_some());

    // Second v exits
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('v')), None);
    assert_eq!(state.visual, None);
}

#[test]
fn test_v_to_big_v_switches_mode() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('v')), None);
    assert_eq!(state.visual, Some(VisualMode::Char));

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('V')), None);
    assert_eq!(state.visual, Some(VisualMode::Line));
}

#[test]
fn test_vy_yanks_selection() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // vw → select word, y → yank
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('v')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('y')), None);

    // Text unchanged after yank
    assert_eq!(text_content(&ta), "hello world");
    assert_eq!(state.visual, None);
}

#[test]
fn test_vc_changes_selection() {
    let mut ta = make_textarea("hello world");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('v')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('e')), None);
    let action = handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('c')), None);

    assert_eq!(action, VimAction::EnteredInsert);
    assert_eq!(state.visual, None);
}

#[test]
fn test_count_f() {
    let mut ta = make_textarea("abababab");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // 2fb — find second 'b'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('2')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('f')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('b')), None);

    assert_eq!(ta.cursor().1, 3); // second 'b'
}

// ========================
// Phase 5: Dot repeat
// ========================

#[test]
fn test_dot_repeats_dw() {
    let mut ta = make_textarea("one two three four");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // dw — delete "one "
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);
    assert_eq!(text_content(&ta), "two three four");

    // . — repeat dw, delete "two "
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('.')), None);
    assert_eq!(text_content(&ta), "three four");
}

#[test]
fn test_dot_repeats_dd() {
    let mut ta = make_textarea("line1\nline2\nline3");
    let mut state = VimState::default();

    // dd — delete "line1"
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    assert_eq!(text_content(&ta), "line2\nline3");

    // . — repeat dd
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('.')), None);
    assert_eq!(text_content(&ta), "line3");
}

#[test]
fn test_dot_repeats_x() {
    let mut ta = make_textarea("abcdef");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // x — delete 'a'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('x')), None);
    assert_eq!(text_content(&ta), "bcdef");

    // . — repeat x
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('.')), None);
    assert_eq!(text_content(&ta), "cdef");
}

#[test]
fn test_dot_repeats_3x() {
    let mut ta = make_textarea("abcdefgh");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // 3x — delete 3 chars
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('3')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('x')), None);
    assert_eq!(text_content(&ta), "defgh");

    // . — should delete 1 char (x without count)
    // Note: dot repeats the recorded command, which is just 'x' (count is consumed before)
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('.')), None);
    assert_eq!(text_content(&ta), "efgh");
}

#[test]
fn test_dot_repeats_r() {
    let mut ta = make_textarea("abc");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // rX — replace 'a' with 'X'
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('r')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('X')), None);
    assert_eq!(text_content(&ta), "Xbc");

    // Move to 'b' and dot repeat
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('l')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('.')), None);
    assert_eq!(text_content(&ta), "XXc");
}

#[test]
fn test_dot_repeats_diw() {
    let mut ta = make_textarea("hello world foo");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // diw — delete "hello"
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('d')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('i')), None);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    // Move past space to next word
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('w')), None);

    // . — repeat diw, should delete another word
    let before_dot = text_content(&ta);
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('.')), None);
    let after_dot = text_content(&ta);

    assert!(
        after_dot.len() < before_dot.len(),
        "dot repeat didn't delete: before={:?} after={:?}",
        before_dot,
        after_dot,
    );
}

#[test]
fn test_dot_noop_without_prior_change() {
    let mut ta = make_textarea("hello");
    let mut state = VimState::default();
    ta.move_cursor(CursorMove::Head);

    // . with no prior change — noop
    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('.')), None);
    assert_eq!(text_content(&ta), "hello");
}

// ========================
// Bracket matching edge cases
// ========================

#[test]
fn test_find_matching_bracket_backward_with_empty_lines() {
    // Regression: backward bracket search panicked on empty lines
    // (index out of bounds: len is 0 but index is 0)
    let lines = vec![
        "fn foo() {".to_string(),
        "".to_string(),
        "    let x = 1;".to_string(),
        "".to_string(),
        "}".to_string(),
    ];
    // Closing brace at (4, 0), should find opening brace at (0, 9)
    let result = find_matching_bracket_pos(&lines, 4, 0);
    assert_eq!(result, Some((0, 9)));
}

#[test]
fn test_find_matching_bracket_forward_with_empty_lines() {
    let lines = vec![
        "fn foo() {".to_string(),
        "".to_string(),
        "".to_string(),
        "}".to_string(),
    ];
    // Opening brace at (0, 9), should find closing brace at (3, 0)
    let result = find_matching_bracket_pos(&lines, 0, 9);
    assert_eq!(result, Some((3, 0)));
}

#[test]
fn test_find_matching_bracket_all_empty_lines_between() {
    let lines = vec![
        "(".to_string(),
        "".to_string(),
        "".to_string(),
        "".to_string(),
        ")".to_string(),
    ];
    let result = find_matching_bracket_pos(&lines, 4, 0);
    assert_eq!(result, Some((0, 0)));
    let result = find_matching_bracket_pos(&lines, 0, 0);
    assert_eq!(result, Some((4, 0)));
}

// WORD motions share the lowercase motion path, with punctuation kept in the
// same run. Check cursor positions in both normal and visual mode.
#[test]
fn test_word_motions_normal_and_visual() {
    let cases = [
        ("foo.bar baz", (0, 0), "W", (0, 8)),
        ("foo.bar baz", (0, 8), "B", (0, 0)),
        ("foo.bar baz", (0, 5), "B", (0, 0)),
        ("foo.bar baz", (0, 0), "E", (0, 6)),
        ("foo.bar baz", (0, 6), "E", (0, 10)),
        ("a-b  c", (0, 0), "W", (0, 5)),
        ("a-b  c", (0, 5), "B", (0, 0)),
        ("a-b  c", (0, 0), "E", (0, 2)),
        ("  foo.bar  baz  ", (0, 0), "W", (0, 2)),
        ("  foo.bar  baz  ", (0, 0), "E", (0, 8)),
        ("  foo.bar  baz  ", (0, 13), "B", (0, 11)),
        ("a-b\tc.d", (0, 0), "W", (0, 4)),
        ("é-猫 baz", (0, 0), "E", (0, 2)),
        ("é-猫 baz", (0, 0), "W", (0, 4)),
        ("foo.bar baz qux!", (0, 0), "2W", (0, 12)),
        ("foo.bar baz qux!", (0, 12), "2B", (0, 0)),
        ("foo.bar baz qux!", (0, 0), "2E", (0, 10)),
        ("foo.bar  \n  baz-qux", (0, 0), "W", (1, 2)),
        ("foo.bar  \n  baz-qux", (1, 2), "B", (0, 0)),
        ("foo.bar  \n  baz-qux", (0, 6), "E", (1, 8)),
        ("foo.bar\n\nbaz-qux", (0, 0), "W", (1, 0)),
        ("foo.bar\n\nbaz-qux", (0, 0), "2W", (2, 0)),
        ("foo.bar\n\nbaz-qux", (2, 0), "B", (1, 0)),
        ("foo.bar\n\nbaz-qux", (2, 0), "2B", (0, 0)),
        ("foo.bar\n\nbaz-qux", (0, 6), "E", (2, 6)),
        ("foo.bar", (0, 0), "W", (0, 6)),
        ("foo.bar", (0, 6), "3E", (0, 6)),
        ("foo.bar  ", (0, 6), "E", (0, 8)),
        ("foo.bar  ", (0, 6), "W", (0, 8)),
        ("foo.bar", (0, 0), "B", (0, 0)),
        ("", (0, 0), "W", (0, 0)),
        ("", (0, 0), "B", (0, 0)),
        ("", (0, 0), "E", (0, 0)),
        ("   ", (0, 0), "W", (0, 2)),
        ("   ", (0, 0), "E", (0, 2)),
        // Lowercase punctuation boundaries stay unchanged.
        ("foo.bar baz", (0, 0), "w", (0, 3)),
        ("foo.bar baz", (0, 6), "b", (0, 4)),
        ("foo.bar baz", (0, 0), "e", (0, 2)),
    ];
    for visual in [false, true] {
        for (text, start, keys, expected) in cases {
            let mut ta = make_textarea(text);
            let mut state = VimState::default();
            move_cursor_to(&mut ta, start.0, start.1);
            if visual {
                handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char('v')), None);
            }
            for ch in keys.chars() {
                assert_eq!(
                    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(ch)), None),
                    VimAction::Consumed,
                    "{text:?} {keys} visual={visual}",
                );
            }
            assert_eq!(ta.cursor(), expected, "{text:?} {keys} visual={visual}");
            assert_eq!(text_content(&ta), text);
            assert_eq!(state.visual, visual.then_some(VisualMode::Char));
        }
    }
}

#[test]
fn test_word_operator_changes_preserve_whitespace_and_line_ends() {
    let cases = [
        ("foo.bar baz", (0, 0), "cW", " baz", (0, 0)),
        ("foo.bar baz", (0, 3), "cW", "foo baz", (0, 3)),
        ("foo.bar baz", (0, 6), "cW", "foo.ba baz", (0, 6)),
        ("a-b  c", (0, 0), "cW", "  c", (0, 0)),
        ("a-b  c", (0, 2), "cW", "a-  c", (0, 2)),
        ("a-b  c", (0, 0), "cE", "  c", (0, 0)),
        ("a-b  c", (0, 2), "cE", "a-", (0, 2)),
        ("x  y", (0, 0), "cW", "  y", (0, 0)),
        ("  foo.bar baz", (0, 0), "cW", "foo.bar baz", (0, 0)),
        ("  foo.bar baz", (0, 0), "cE", " baz", (0, 0)),
        ("foo.bar  ", (0, 0), "cW", "  ", (0, 0)),
        ("foo.bar\nnext.line", (0, 0), "cW", "\nnext.line", (0, 0)),
        (
            "foo.bar\nnext.line",
            (0, 6),
            "cW",
            "foo.ba\nnext.line",
            (0, 6),
        ),
        (
            "foo.bar  \nnext.line",
            (0, 0),
            "cW",
            "  \nnext.line",
            (0, 0),
        ),
        ("foo.bar", (0, 0), "cW", "", (0, 0)),
        ("foo.bar", (0, 6), "cW", "foo.ba", (0, 6)),
        ("foo.bar", (0, 6), "cE", "foo.ba", (0, 6)),
        ("foo.bar baz qux!", (0, 0), "c2W", " qux!", (0, 0)),
        ("foo.bar baz qux!", (0, 0), "2cW", " qux!", (0, 0)),
        ("foo.bar baz qux!", (0, 6), "c2W", "foo.ba qux!", (0, 6)),
        ("foo.bar baz qux!", (0, 0), "c2E", " qux!", (0, 0)),
        ("foo.bar\n  baz-qux tail", (0, 0), "c2W", " tail", (0, 0)),
        ("foo.bar\n\nbaz-qux tail", (0, 0), "c2W", " tail", (0, 0)),
        ("foo.bar baz", (0, 8), "cB", "baz", (0, 0)),
        ("é-猫 baz", (0, 0), "cW", " baz", (0, 0)),
        ("", (0, 0), "cW", "", (0, 0)),
    ];
    for (text, start, keys, expected, cursor) in cases {
        let mut ta = make_textarea(text);
        let mut state = VimState::default();
        move_cursor_to(&mut ta, start.0, start.1);
        let mut action = VimAction::Consumed;
        for ch in keys.chars() {
            action = handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(ch)), None);
        }
        assert_eq!(action, VimAction::EnteredInsert, "{text:?} {keys}");
        assert_eq!(text_content(&ta), expected, "{text:?} {keys}");
        assert_eq!(ta.cursor(), cursor, "{text:?} {keys}");
        assert_eq!(state.pending_operator, None);
    }
}

#[test]
fn test_word_delete_and_yank_operators() {
    let cases = [
        (
            "foo.bar baz",
            (0, 0),
            "W",
            "foo.bar ",
            "baz",
            (0, 0),
            (0, 8),
        ),
        (
            "foo.bar baz",
            (0, 0),
            "E",
            "foo.bar",
            " baz",
            (0, 0),
            (0, 7),
        ),
        ("a-b  c", (0, 0), "W", "a-b  ", "c", (0, 0), (0, 5)),
        ("a-b  c", (0, 5), "B", "a-b  ", "c", (0, 0), (0, 0)),
        (
            "foo.bar baz qux!",
            (0, 0),
            "2W",
            "foo.bar baz ",
            "qux!",
            (0, 0),
            (0, 12),
        ),
        (
            "foo.bar baz qux!",
            (0, 0),
            "2E",
            "foo.bar baz",
            " qux!",
            (0, 0),
            (0, 11),
        ),
        (
            "foo.bar baz qux!",
            (0, 12),
            "2B",
            "foo.bar baz ",
            "qux!",
            (0, 0),
            (0, 0),
        ),
        ("  foo.bar", (0, 0), "W", "  ", "foo.bar", (0, 0), (0, 2)),
        ("foo.bar  ", (0, 0), "W", "foo.bar  ", "", (0, 0), (0, 9)),
        ("foo.bar", (0, 0), "W", "foo.bar", "", (0, 0), (0, 7)),
        ("foo.bar", (0, 6), "E", "r", "foo.ba", (0, 6), (0, 7)),
        (
            "foo.bar\n  baz",
            (0, 0),
            "W",
            "foo.bar",
            "\n  baz",
            (0, 0),
            (0, 7),
        ),
        (
            "foo.bar\n  baz",
            (0, 6),
            "W",
            "r",
            "foo.ba\n  baz",
            (0, 6),
            (0, 7),
        ),
        (
            "foo.bar\n  baz",
            (0, 0),
            "2W",
            "foo.bar\n  baz",
            "",
            (0, 0),
            (1, 5),
        ),
        (
            "foo.bar\n  baz",
            (1, 2),
            "B",
            "foo.bar\n  ",
            "baz",
            (0, 0),
            (0, 0),
        ),
    ];
    for op in ['d', 'y'] {
        for (text, start, motion, yank, deleted, delete_cursor, yank_cursor) in cases {
            let mut ta = make_textarea(text);
            let mut state = VimState::default();
            move_cursor_to(&mut ta, start.0, start.1);
            for ch in std::iter::once(op).chain(motion.chars()) {
                assert_eq!(
                    handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(ch)), None),
                    VimAction::Consumed,
                );
            }
            assert_eq!(ta.yank_text(), yank, "{text:?} {op}{motion}");
            assert_eq!(text_content(&ta), if op == 'y' { text } else { deleted });
            assert_eq!(
                ta.cursor(),
                if op == 'y' {
                    yank_cursor
                } else {
                    delete_cursor
                }
            );
        }
    }
}

#[test]
fn test_visual_word_motion_extends_selection() {
    for (start, keys, expected, cursor) in [
        ((0, 0), "vWd", "baz qux!", (0, 0)),
        ((0, 0), "v2Wc", "qux!", (0, 0)),
        ((0, 12), "v2Bd", "qux!", (0, 0)),
    ] {
        let mut ta = make_textarea("foo.bar baz qux!");
        let mut state = VimState::default();
        move_cursor_to(&mut ta, start.0, start.1);
        for ch in keys.chars() {
            handle_vim_normal(&mut ta, &mut state, key(KeyCode::Char(ch)), None);
        }
        assert_eq!(text_content(&ta), expected);
        assert_eq!(ta.cursor(), cursor);
        assert_eq!(state.visual, None);
    }
}

#[test]
fn test_word_change_through_shared_input_surface() {
    use crate::input_surface::{InputAction, InputSurface, InputSurfaceConfig, handle_key};
    use crate::types::PopupMode;

    for pass_through_unhandled in [false, true] {
        let config = InputSurfaceConfig {
            pass_through_unhandled,
            available_commands: &[],
            working_dir: None,
            submit_on_enter: false,
            standard_editing: false,
        };
        for (text, start, command, expected, cursor) in [
            ("foo.bar baz", 0, "cW", "X baz", (0, 1)),
            ("foo.bar\nnext.line", 0, "cW", "X\nnext.line", (0, 1)),
            (
                "foo.bar  \nnext.line",
                6,
                "cW",
                "foo.baX  \nnext.line",
                (0, 7),
            ),
            ("foo.bar baz tail", 0, "c2W", "X tail", (0, 1)),
        ] {
            let mut surface = InputSurface::default();
            *surface.textarea = make_textarea(text);
            surface.mode = PopupMode::Normal;
            move_cursor_to(&mut surface.textarea, 0, start);
            for ch in command.chars() {
                assert!(matches!(
                    handle_key(&mut surface, key(KeyCode::Char(ch)), &config),
                    InputAction::Consumed,
                ));
            }
            assert_eq!(surface.mode, PopupMode::Insert);
            handle_key(&mut surface, key(KeyCode::Char('X')), &config);
            handle_key(&mut surface, key(KeyCode::Esc), &config);
            assert_eq!(surface.mode, PopupMode::Normal);
            assert_eq!(text_content(&surface.textarea), expected);
            assert_eq!(surface.textarea.cursor(), cursor);
        }
    }
}
