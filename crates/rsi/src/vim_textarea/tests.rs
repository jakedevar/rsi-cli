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
    assert_eq!(ta.lines().join(""), "world");
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
