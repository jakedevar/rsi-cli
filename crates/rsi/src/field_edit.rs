//! Standard (non-modal) editing for single-line `String` fields (#1628).
//!
//! Search boxes, palettes, forms and small editors keep their text in plain
//! `String`s that grew only at the end. In Standard mode those fields get a
//! real cursor and selection without changing their type: the cursor and
//! anchor live in one [`FieldEdit`] owned by the `App` (only one field has
//! focus at a time) together with a snapshot of the text it last saw. When
//! the field's text differs from that snapshot something else changed it
//! (a paste, a field switch, a reset), and the cursor falls back to the end,
//! which is exactly where the field behaved before.
//!
//! Vim mode never reaches this module: owners call it only when the operator
//! chose Standard editing, so Vim-mode behaviour is untouched.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// What a key did to a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKey {
    /// Not a field-editing key; the owner handles it as before.
    Ignored,
    /// The cursor or selection moved (or text was copied); the text is unchanged.
    Moved,
    /// The text changed.
    Edited,
}

/// Cursor and selection for the focused single-line field.
#[derive(Debug, Clone, Default)]
pub struct FieldEdit {
    /// Cursor position in characters.
    cursor: usize,
    /// The other end of the selection, in characters.
    anchor: Option<usize>,
    /// The text the cursor was last valid for.
    seen: String,
    /// Which overlay the cursor belongs to; a different overlay starts over.
    owner: Option<std::mem::Discriminant<crate::types::OverlayState>>,
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn byte_at(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Start of the word before `cursor` (skips spaces, then one run of word or
/// punctuation characters).
fn word_back(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut i = cursor.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i > 0 {
        let word = is_word(chars[i - 1]);
        while i > 0 && !chars[i - 1].is_whitespace() && is_word(chars[i - 1]) == word {
            i -= 1;
        }
    }
    i
}

/// End of the word after `cursor`.
fn word_forward(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut i = cursor.min(chars.len());
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if i < chars.len() {
        let word = is_word(chars[i]);
        while i < chars.len() && !chars[i].is_whitespace() && is_word(chars[i]) == word {
            i += 1;
        }
    }
    i
}

impl FieldEdit {
    /// Bring the cursor back in step with `text`: if the field changed behind
    /// our back, the cursor goes to the end and any selection is dropped.
    pub fn sync(&mut self, text: &str) {
        if self.seen != text {
            self.cursor = char_len(text);
            self.anchor = None;
            self.seen = text.to_string();
        }
        self.cursor = self.cursor.min(char_len(text));
    }

    /// Drop the cursor when the focused overlay is not the one it was
    /// tracking, so a stale selection from a closed overlay cannot claim keys.
    pub fn follow_overlay(&mut self, overlay: &crate::types::OverlayState) {
        let kind = std::mem::discriminant(overlay);
        if self.owner != Some(kind) {
            *self = Self {
                owner: Some(kind),
                ..Self::default()
            };
        }
    }

    /// Whether a non-empty selection is active (Ctrl-C then copies it).
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.anchor.is_some_and(|anchor| anchor != self.cursor)
    }

    /// The cursor and selection for drawing `text`, without mutating.
    #[must_use]
    pub fn view(&self, text: &str) -> (usize, Option<(usize, usize)>) {
        if self.seen != text {
            return (char_len(text), None);
        }
        let selection = self
            .anchor
            .filter(|anchor| *anchor != self.cursor)
            .map(|anchor| (anchor.min(self.cursor), anchor.max(self.cursor)));
        (self.cursor.min(char_len(text)), selection)
    }

    /// The selected text, if any.
    #[must_use]
    pub fn selected<'a>(&self, text: &'a str) -> Option<&'a str> {
        let (_, selection) = self.view(text);
        let (start, end) = selection?;
        Some(&text[byte_at(text, start)..byte_at(text, end)])
    }

    fn delete_range(&mut self, text: &mut String, start: usize, end: usize) {
        let (from, to) = (byte_at(text, start), byte_at(text, end));
        text.replace_range(from..to, "");
        self.cursor = start;
        self.anchor = None;
    }

    fn delete_selection(&mut self, text: &mut String) -> bool {
        let (_, selection) = self.view(text);
        if let Some((start, end)) = selection {
            self.delete_range(text, start, end);
            true
        } else {
            self.anchor = None;
            false
        }
    }

    fn insert(&mut self, text: &mut String, inserted: char) {
        self.delete_selection(text);
        let at = byte_at(text, self.cursor);
        text.insert(at, inserted);
        self.cursor += 1;
    }

    fn move_to(&mut self, target: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = target;
    }

    /// Apply `key` to `text`. Only chords that edit or move inside the field
    /// are taken; Enter, Esc, Tab and Up/Down are `Ignored` for the owner.
    pub fn handle_key(&mut self, text: &mut String, key: KeyEvent) -> FieldKey {
        self.handle_key_filtered(text, key, |_| true)
    }

    /// Like [`handle_key`](Self::handle_key), but typed characters must pass
    /// `accept` (numeric fields); a rejected character is swallowed.
    pub fn handle_key_filtered(
        &mut self,
        text: &mut String,
        key: KeyEvent,
        accept: impl Fn(char) -> bool,
    ) -> FieldKey {
        self.handle_key_capped(text, key, accept, usize::MAX)
    }

    /// Like [`handle_key_filtered`](Self::handle_key_filtered) for fields with
    /// a length limit: a typed character that would take the text past
    /// `max_chars` (after replacing any selection) is swallowed.
    pub fn handle_key_capped(
        &mut self,
        text: &mut String,
        key: KeyEvent,
        accept: impl Fn(char) -> bool,
        max_chars: usize,
    ) -> FieldKey {
        self.sync(text);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let word = ctrl || alt;
        let len = char_len(text);
        let result = match key.code {
            KeyCode::Char(c) if ctrl && !alt => match c.to_ascii_lowercase() {
                'a' => {
                    self.anchor = Some(0);
                    self.cursor = len;
                    FieldKey::Moved
                }
                'c' => match self.selected(text) {
                    Some(selected) => {
                        crate::clipboard::osc52_copy(selected);
                        FieldKey::Moved
                    }
                    None => FieldKey::Ignored,
                },
                'x' => match self.selected(text).map(str::to_string) {
                    Some(selected) => {
                        crate::clipboard::osc52_copy(&selected);
                        self.delete_selection(text);
                        FieldKey::Edited
                    }
                    None => FieldKey::Ignored,
                },
                _ => FieldKey::Ignored,
            },
            KeyCode::Char(c) if !ctrl && !alt => {
                let selected_len = self.view(text).1.map_or(0, |(start, end)| end - start);
                if accept(c) && len - selected_len < max_chars {
                    self.insert(text, c);
                    FieldKey::Edited
                } else {
                    FieldKey::Moved
                }
            }
            KeyCode::Backspace => {
                if self.delete_selection(text) {
                } else if word {
                    let start = word_back(text, self.cursor);
                    self.delete_range(text, start, self.cursor);
                } else if self.cursor > 0 {
                    self.delete_range(text, self.cursor - 1, self.cursor);
                } else {
                    return FieldKey::Moved;
                }
                FieldKey::Edited
            }
            KeyCode::Delete => {
                if self.delete_selection(text) {
                } else if word {
                    let end = word_forward(text, self.cursor);
                    self.delete_range(text, self.cursor, end);
                } else if self.cursor < len {
                    self.delete_range(text, self.cursor, self.cursor + 1);
                } else {
                    return FieldKey::Moved;
                }
                FieldKey::Edited
            }
            KeyCode::Left | KeyCode::Right => {
                let forward = key.code == KeyCode::Right;
                let selection = self.view(text).1;
                match (selection, word, shift) {
                    // A plain arrow collapses a selection to its near edge.
                    (Some((start, end)), false, false) => {
                        self.cursor = if forward { end } else { start };
                        self.anchor = None;
                    }
                    _ => {
                        let target = match (forward, word) {
                            (true, true) => word_forward(text, self.cursor),
                            (false, true) => word_back(text, self.cursor),
                            (true, false) => (self.cursor + 1).min(len),
                            (false, false) => self.cursor.saturating_sub(1),
                        };
                        self.move_to(target, shift);
                    }
                }
                FieldKey::Moved
            }
            KeyCode::Home => {
                self.move_to(0, shift);
                FieldKey::Moved
            }
            KeyCode::End => {
                self.move_to(len, shift);
                FieldKey::Moved
            }
            _ => FieldKey::Ignored,
        };
        self.seen.clone_from(text);
        result
    }

    /// Spans for drawing `text` with this cursor and selection, for the
    /// focused field in Standard mode. The caret is a reversed character (or
    /// a block at the end); a selection is reversed too.
    #[must_use]
    pub fn spans(&self, text: &str, style: Style) -> Vec<Span<'static>> {
        let (cursor, selection) = self.view(text);
        let chars: Vec<char> = text.chars().collect();
        let reversed = style.add_modifier(Modifier::REVERSED);
        let mut spans = Vec::new();
        let mut run = String::new();
        let mut run_selected = false;
        for (index, c) in chars.iter().enumerate() {
            let selected = selection.is_some_and(|(start, end)| index >= start && index < end);
            let caret = index == cursor && selection.is_none();
            if caret || selected != run_selected {
                if !run.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut run),
                        if run_selected { reversed } else { style },
                    ));
                }
                run_selected = selected;
            }
            if caret {
                spans.push(Span::styled(c.to_string(), reversed));
            } else {
                run.push(*c);
            }
        }
        if !run.is_empty() {
            spans.push(Span::styled(
                run,
                if run_selected { reversed } else { style },
            ));
        }
        if cursor >= chars.len() && selection.is_none() {
            spans.push(Span::styled("\u{2588}", style));
        }
        spans
    }
}

thread_local! {
    /// The Standard field cursor for the frame being drawn. Renderers read it
    /// instead of taking it as a parameter, so a field's draw code does not
    /// need the `App`. `None` outside a frame and in Vim mode.
    static FRAME_EDIT: std::cell::RefCell<Option<FieldEdit>> = const { std::cell::RefCell::new(None) };
}

/// Clears the frame's field cursor when the frame ends.
pub struct FrameGuard;

impl Drop for FrameGuard {
    fn drop(&mut self) {
        FRAME_EDIT.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Publish `edit` (Standard mode) to this frame's renderers until the guard drops.
#[must_use]
pub fn begin_frame(edit: Option<FieldEdit>) -> FrameGuard {
    FRAME_EDIT.with(|cell| *cell.borrow_mut() = edit);
    FrameGuard
}

/// Spans for the focused field's text and caret: the Standard cursor and
/// selection during a Standard frame, otherwise the text with the end-of-text
/// block cursor Vim mode has always drawn.
#[must_use]
pub fn draw(text: &str, style: Style) -> Vec<Span<'static>> {
    draw_with_caret(text, style, style)
}

/// [`draw`] where Vim mode's end-of-text block is styled differently from
/// the text.
#[must_use]
pub fn draw_with_caret(text: &str, style: Style, caret_style: Style) -> Vec<Span<'static>> {
    FRAME_EDIT.with(|cell| match cell.borrow().as_ref() {
        Some(edit) => edit.spans(text, style),
        None => vec![
            Span::styled(text.to_string(), style),
            Span::styled("\u{2588}", caret_style),
        ],
    })
}

/// [`draw`] scrolled horizontally so the caret stays inside `width` columns.
/// Vim mode gets the bare text unchanged.
#[must_use]
pub fn draw_fit(text: &str, style: Style, width: usize) -> Vec<Span<'static>> {
    let spans = draw(text, style);
    let skip = (frame_cursor(text) + 1).saturating_sub(width.max(1));
    if skip == 0 || !standard_frame() {
        return spans;
    }
    let mut remaining = skip;
    let mut out = Vec::new();
    for span in spans {
        let count = span.content.chars().count();
        if remaining >= count {
            remaining -= count;
            continue;
        }
        let kept: String = span.content.chars().skip(remaining).collect();
        remaining = 0;
        out.push(Span::styled(kept, span.style));
    }
    out
}

/// A `label: value` form row. In a Standard frame the active row's value is
/// drawn with the field cursor; otherwise (and for inactive rows) it is the
/// plain `prefix + value` text.
#[must_use]
pub fn field_row(prefix: &str, value: &str, style: Style, active: bool) -> Vec<Span<'static>> {
    if active && standard_frame() {
        let mut spans = vec![Span::styled(prefix.to_string(), style)];
        spans.extend(draw(value, style));
        spans
    } else {
        vec![Span::styled(format!("{prefix}{value}"), style)]
    }
}

const SEL_START: char = '\u{E000}';
const SEL_END: char = '\u{E001}';
const CARET: char = '\u{E002}';

/// `text` with private-use markers at the Standard cursor / selection, for
/// fields that are word-wrapped before drawing. Vim frames get the text
/// followed by `vim_caret`. Draw each wrapped row with [`marked_row`].
#[must_use]
pub fn mark(text: &str, vim_caret: &str) -> String {
    let (cursor, selection) = FRAME_EDIT.with(|cell| match cell.borrow().as_ref() {
        Some(edit) => {
            let (cursor, selection) = edit.view(text);
            (Some(cursor), selection)
        }
        None => (None, None),
    });
    let Some(cursor) = cursor else {
        return format!("{text}{vim_caret}");
    };
    let mut out = String::new();
    let count = text.chars().count();
    for (index, c) in text.chars().enumerate() {
        match selection {
            Some((start, _)) if index == start => out.push(SEL_START),
            Some((_, end)) if index == end => out.push(SEL_END),
            None if index == cursor => out.push(CARET),
            _ => {}
        }
        out.push(c);
    }
    match selection {
        Some((_, end)) if end >= count => out.push(SEL_END),
        None if cursor >= count => out.push(CARET),
        _ => {}
    }
    out
}

/// Spans for one wrapped row of [`mark`]ed text. `in_selection` carries the
/// selection state from one row to the next.
#[must_use]
pub fn marked_row(row: &str, in_selection: &mut bool, style: Style) -> Vec<Span<'static>> {
    let reversed = style.add_modifier(Modifier::REVERSED);
    let mut spans = Vec::new();
    let mut run = String::new();
    let flush = |run: &mut String, selected: bool, spans: &mut Vec<Span<'static>>| {
        if !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(run),
                if selected { reversed } else { style },
            ));
        }
    };
    for c in row.chars() {
        match c {
            SEL_START => {
                flush(&mut run, *in_selection, &mut spans);
                *in_selection = true;
            }
            SEL_END => {
                flush(&mut run, *in_selection, &mut spans);
                *in_selection = false;
            }
            CARET => {
                flush(&mut run, *in_selection, &mut spans);
                spans.push(Span::styled("\u{258f}", style));
            }
            _ => run.push(c),
        }
    }
    flush(&mut run, *in_selection, &mut spans);
    spans
}

/// Whether this frame draws Standard field cursors.
#[must_use]
pub fn standard_frame() -> bool {
    FRAME_EDIT.with(|cell| cell.borrow().is_some())
}

/// Like [`draw`], for fields that show their caret through the terminal
/// cursor in Vim mode: Vim gets the bare text, Standard the drawn caret.
#[must_use]
pub fn draw_inline(text: &str, style: Style) -> Vec<Span<'static>> {
    if standard_frame() {
        draw(text, style)
    } else {
        vec![Span::styled(text.to_string(), style)]
    }
}

/// The cursor (in characters) the Standard frame has for `text`, or its end.
#[must_use]
pub fn frame_cursor(text: &str) -> usize {
    FRAME_EDIT.with(|cell| match cell.borrow().as_ref() {
        Some(edit) => edit.view(text).0,
        None => char_len(text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(
        edit: &mut FieldEdit,
        text: &mut String,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> FieldKey {
        edit.handle_key(text, KeyEvent::new(code, mods))
    }

    fn type_str(edit: &mut FieldEdit, text: &mut String, s: &str) {
        for c in s.chars() {
            press(edit, text, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[test]
    fn typing_inserts_at_the_cursor_and_arrows_move_it() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        type_str(&mut edit, &mut text, "helo");
        press(&mut edit, &mut text, KeyCode::Left, KeyModifiers::NONE);
        type_str(&mut edit, &mut text, "l");
        assert_eq!(text, "hello");
        press(&mut edit, &mut text, KeyCode::Home, KeyModifiers::NONE);
        type_str(&mut edit, &mut text, ">");
        assert_eq!(text, ">hello");
        press(&mut edit, &mut text, KeyCode::End, KeyModifiers::NONE);
        press(&mut edit, &mut text, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(text, ">hell");
        press(&mut edit, &mut text, KeyCode::Home, KeyModifiers::NONE);
        press(&mut edit, &mut text, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(text, "hell");
    }

    #[test]
    fn word_moves_and_word_deletes() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        type_str(&mut edit, &mut text, "one two three");
        press(&mut edit, &mut text, KeyCode::Left, KeyModifiers::CONTROL);
        press(&mut edit, &mut text, KeyCode::Left, KeyModifiers::CONTROL);
        type_str(&mut edit, &mut text, "X");
        assert_eq!(text, "one Xtwo three");
        press(
            &mut edit,
            &mut text,
            KeyCode::Backspace,
            KeyModifiers::CONTROL,
        );
        assert_eq!(text, "one two three");
        press(&mut edit, &mut text, KeyCode::Delete, KeyModifiers::CONTROL);
        assert_eq!(text, "one  three");
    }

    #[test]
    fn select_all_then_typing_replaces_and_shift_arrows_select() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        type_str(&mut edit, &mut text, "hello");
        press(
            &mut edit,
            &mut text,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(edit.selected(&text), Some("hello"));
        type_str(&mut edit, &mut text, "x");
        assert_eq!(text, "x");
        type_str(&mut edit, &mut text, "yz");
        press(&mut edit, &mut text, KeyCode::Left, KeyModifiers::SHIFT);
        press(&mut edit, &mut text, KeyCode::Left, KeyModifiers::SHIFT);
        assert_eq!(edit.selected(&text), Some("yz"));
        press(&mut edit, &mut text, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(text, "x");
    }

    #[test]
    fn cut_removes_the_selection_and_copy_keeps_it() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        type_str(&mut edit, &mut text, "abc");
        press(
            &mut edit,
            &mut text,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(
            press(
                &mut edit,
                &mut text,
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ),
            FieldKey::Moved
        );
        assert_eq!(text, "abc");
        assert_eq!(
            press(
                &mut edit,
                &mut text,
                KeyCode::Char('x'),
                KeyModifiers::CONTROL
            ),
            FieldKey::Edited
        );
        assert_eq!(text, "");
        assert_eq!(
            press(
                &mut edit,
                &mut text,
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ),
            FieldKey::Ignored,
            "no selection: Ctrl-C keeps its global meaning"
        );
    }

    #[test]
    fn keys_that_belong_to_the_owner_are_ignored() {
        let (mut edit, mut text) = (FieldEdit::default(), "abc".to_string());
        for code in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::Up,
            KeyCode::Down,
        ] {
            assert_eq!(
                press(&mut edit, &mut text, code, KeyModifiers::NONE),
                FieldKey::Ignored,
                "{code:?}"
            );
        }
        assert_eq!(
            press(
                &mut edit,
                &mut text,
                KeyCode::Char('u'),
                KeyModifiers::CONTROL
            ),
            FieldKey::Ignored
        );
        assert_eq!(text, "abc");
    }

    #[test]
    fn external_change_resets_the_cursor_to_the_end() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        type_str(&mut edit, &mut text, "abc");
        press(&mut edit, &mut text, KeyCode::Home, KeyModifiers::NONE);
        text.push_str("def"); // a paste appends
        press(&mut edit, &mut text, KeyCode::Char('!'), KeyModifiers::NONE);
        assert_eq!(text, "abcdef!");
    }

    #[test]
    fn filtered_fields_reject_other_characters() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        for c in "1a2".chars() {
            edit.handle_key_filtered(
                &mut text,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                |c| c.is_ascii_digit(),
            );
        }
        assert_eq!(text, "12");
    }

    #[test]
    fn multibyte_text_is_edited_by_character() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        type_str(&mut edit, &mut text, "héllo");
        press(&mut edit, &mut text, KeyCode::Left, KeyModifiers::NONE);
        press(&mut edit, &mut text, KeyCode::Left, KeyModifiers::NONE);
        press(&mut edit, &mut text, KeyCode::Left, KeyModifiers::NONE);
        press(&mut edit, &mut text, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(text, "hllo");
    }

    #[test]
    fn spans_draw_a_caret_and_a_selection() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        type_str(&mut edit, &mut text, "ab");
        let plain: String = edit
            .spans(&text, Style::default())
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(plain, "ab\u{2588}");
        press(&mut edit, &mut text, KeyCode::Left, KeyModifiers::NONE);
        let spans = edit.spans(&text, Style::default());
        let caret = spans
            .iter()
            .find(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .expect("caret span");
        assert_eq!(caret.content, "b");
        press(
            &mut edit,
            &mut text,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        let spans = edit.spans(&text, Style::default());
        assert!(
            spans
                .iter()
                .all(|s| s.style.add_modifier.contains(Modifier::REVERSED)),
            "a full selection is drawn reversed"
        );
    }

    #[test]
    fn wrapped_fields_mark_the_caret_and_selection() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        type_str(&mut edit, &mut text, "one two");
        press(&mut edit, &mut text, KeyCode::Home, KeyModifiers::NONE);
        {
            let _frame = begin_frame(Some(edit.clone()));
            let marked = mark(&text, "|");
            let mut in_selection = false;
            let spans = marked_row(&marked, &mut in_selection, Style::default());
            let drawn: String = spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(drawn, "\u{258f}one two", "caret drawn before the text");
        }
        press(
            &mut edit,
            &mut text,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        {
            let _frame = begin_frame(Some(edit.clone()));
            let marked = mark(&text, "|");
            let mut in_selection = false;
            let spans = marked_row(&marked, &mut in_selection, Style::default());
            assert_eq!(spans.len(), 1, "one selected run");
            assert_eq!(spans[0].content.as_ref(), "one two");
            assert!(spans[0].style.add_modifier.contains(Modifier::REVERSED));
            assert!(!in_selection, "selection closes within the row");
        }
        // Vim frames get the text and the end-of-text caret unchanged.
        assert_eq!(mark(&text, "|"), "one two|");
    }

    #[test]
    fn a_selection_carries_across_wrapped_rows() {
        let mut in_selection = false;
        let first = marked_row("a\u{E000}bc", &mut in_selection, Style::default());
        assert!(in_selection, "still selecting at the end of the row");
        assert_eq!(first.len(), 2);
        let second = marked_row("de\u{E001}f", &mut in_selection, Style::default());
        assert!(!in_selection);
        assert!(second[0].style.add_modifier.contains(Modifier::REVERSED));
        assert!(!second[1].style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn a_capped_field_swallows_characters_past_its_limit() {
        let (mut edit, mut text) = (FieldEdit::default(), String::new());
        for c in "abcde".chars() {
            edit.handle_key_capped(
                &mut text,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                |_| true,
                3,
            );
        }
        assert_eq!(text, "abc");
        press(
            &mut edit,
            &mut text,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        edit.handle_key_capped(
            &mut text,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            |_| true,
            3,
        );
        assert_eq!(text, "z", "typing over a selection stays within the cap");
    }
}
