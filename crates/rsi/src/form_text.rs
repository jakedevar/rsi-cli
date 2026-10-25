//! Vim (or Standard) text editing for the text fields of a form (#3421).
//!
//! The Issue editor and the project form keep each field's text in a plain
//! `String` that the rest of the code reads (submit, validation, derived
//! values). Editing those strings by pushing and popping characters at the
//! end left the operator unable to fix a typo and let long text run off the
//! edge. A [`FormText`] puts the shared [`InputSurface`] (the vim textarea the
//! prompt and the input modal use) behind the *focused* field: the surface
//! loads the field's text, the key goes through the same vim emulation as
//! everywhere else, and the owner copies the resulting text back into its
//! `String`. Nothing else about the form changes.
//!
//! One surface serves whichever field has focus. It is rebuilt (insert mode,
//! cursor at the end) when focus moves to another field or when the owner
//! changed the field's text behind its back (a reload, a rebase), so an
//! out-of-date cursor can never edit text it was not made for.

use crate::input_surface::{self, InputAction, InputSurface, InputSurfaceConfig};
use crate::types::PopupMode;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::CursorMove;

/// The longest Issue title the TUI editor accepts, in characters. The daemon
/// and the agent RPCs have their own, larger limits; this one only keeps the
/// operator's titles scannable in a table row.
pub const ISSUE_TITLE_LIMIT: usize = 200;

/// The title counter shows from this many characters.
pub const ISSUE_TITLE_COUNTER_FROM: usize = 150;

/// The hint shown when the title limit refuses input.
#[must_use]
pub fn title_limit_hint() -> String {
    format!("TITLE LIMIT — titles are cut at {ISSUE_TITLE_LIMIT} characters; input refused")
}

/// How a field behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    /// May hold several lines (an Issue body). A single-line field wraps on
    /// screen but never holds a newline.
    pub multiline: bool,
    /// The most characters the field holds; input past it is refused.
    pub max_chars: Option<usize>,
}

impl FieldSpec {
    /// A one-line field with no length limit.
    pub const SINGLE_LINE: Self = Self {
        multiline: false,
        max_chars: None,
    };
    /// A multi-line field with no length limit.
    pub const MULTILINE: Self = Self {
        multiline: true,
        max_chars: None,
    };
}

/// What a key did to the focused field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKey {
    /// Not a text key: the owner handles it (or nothing is loaded).
    Ignored,
    /// The cursor, mode or a selection changed; the text did not.
    Moved,
    /// The text changed; the owner copies it back.
    Edited,
    /// The key would have passed the field's length limit and was dropped.
    Refused,
}

/// The buffer and cursor at one point of a field's history.
type Snapshot = (Vec<String>, (usize, usize));

#[derive(Debug, Clone)]
struct Slot {
    field: usize,
    surface: InputSurface,
    /// One entry per change: a whole insert session or a single normal-mode
    /// command (`ciwx<Esc>` is one entry), so `u` takes back what the operator
    /// just did and not one keystroke of it.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Typing in the current insert session is already covered by an entry.
    group_open: bool,
}

fn snapshot(surface: &InputSurface) -> Snapshot {
    (surface.textarea.lines().to_vec(), surface.textarea.cursor())
}

fn restore(surface: &mut InputSurface, (lines, (row, col)): Snapshot) {
    let mut textarea = tui_textarea::TextArea::new(lines);
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    textarea.set_block(ratatui::widgets::Block::default());
    textarea.move_cursor(CursorMove::Jump(row as u16, col as u16));
    *surface.textarea = textarea;
}

/// The editing surface behind a form's focused text field.
///
/// It is view state derived from the form's fields, so two forms compare equal
/// whatever their surfaces hold.
#[derive(Debug, Clone, Default)]
pub struct FormText {
    slot: Option<Box<Slot>>,
}

impl PartialEq for FormText {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for FormText {}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn new_surface(value: &str, spec: FieldSpec, standard: bool) -> InputSurface {
    let lines: Vec<String> = if spec.multiline {
        value.split('\n').map(str::to_string).collect()
    } else {
        vec![value.replace(['\r', '\n'], " ")]
    };
    let mut surface = InputSurface::new_insert_with_content(lines);
    surface.textarea.move_cursor(CursorMove::Bottom);
    surface.textarea.move_cursor(CursorMove::End);
    let content = surface.content();
    surface.vim_state.snapshot_for_insert(&content);
    if standard {
        input_surface::enter_standard(&mut surface);
    }
    surface
}

/// Join every line of a one-line field into the first, keeping the cursor on
/// the same character. Vim commands such as `J`, `p` of a multi-line yank, or
/// a stray Ctrl-M can leave a second line behind; a one-line field never keeps
/// it.
fn flatten_to_one_line(surface: &mut InputSurface) {
    if surface.textarea.lines().len() <= 1 {
        return;
    }
    let (row, col) = surface.textarea.cursor();
    let offset: usize = surface.textarea.lines()[..row]
        .iter()
        .map(|line| char_count(line))
        .sum::<usize>()
        + col;
    while surface.textarea.lines().len() > 1 {
        surface.textarea.move_cursor(CursorMove::Top);
        surface.textarea.move_cursor(CursorMove::End);
        surface.textarea.delete_next_char();
    }
    surface.textarea.move_cursor(CursorMove::Head);
    for _ in 0..offset.min(char_count(&surface.textarea.lines()[0])) {
        surface.textarea.move_cursor(CursorMove::Forward);
    }
}

/// A key as the one-line field should see it: vim's `o` and `O` open a new
/// line, which the field refuses, so they append and insert at the line edge.
fn single_line_key(key: KeyEvent, surface: &InputSurface, standard: bool) -> KeyEvent {
    if standard
        || surface.mode != PopupMode::Normal
        || !surface.vim_state.is_idle()
        || key.modifiers != KeyModifiers::NONE
    {
        return key;
    }
    match key.code {
        KeyCode::Char('o') => KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
        KeyCode::Char('O') => KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT),
        _ => key,
    }
}

impl FormText {
    /// Point the surface at `field` showing `value`. A surface that already
    /// shows exactly that keeps its cursor, mode and undo history; anything
    /// else starts over in insert mode with the cursor at the end of the text.
    pub fn load(&mut self, field: usize, value: &str, spec: FieldSpec, standard: bool) {
        let keep = self.slot.as_ref().is_some_and(|slot| {
            let shown = slot.surface.content();
            slot.field == field
                && if spec.multiline {
                    shown == value
                } else {
                    shown == value.replace(['\r', '\n'], " ")
                }
        });
        if keep {
            if let Some(slot) = self.slot.as_mut() {
                if standard {
                    input_surface::enter_standard(&mut slot.surface);
                } else {
                    slot.surface.standard_editing = false;
                }
            }
            return;
        }
        self.slot = Some(Box::new(Slot {
            field,
            surface: new_surface(value, spec, standard),
            undo: Vec::new(),
            redo: Vec::new(),
            group_open: false,
        }));
    }

    /// Drop the surface (the form closed or moved to a non-text field).
    pub fn clear(&mut self) {
        self.slot = None;
    }

    /// The surface, when it is showing `field`.
    #[must_use]
    pub fn surface_for(&self, field: usize) -> Option<&InputSurface> {
        self.slot
            .as_deref()
            .filter(|slot| slot.field == field)
            .map(|slot| &slot.surface)
    }

    /// The text the surface holds.
    #[must_use]
    pub fn content(&self) -> Option<String> {
        self.slot.as_ref().map(|slot| slot.surface.content())
    }

    /// Whether Esc belongs to the field instead of the form. In vim that is
    /// insert mode (Esc leaves it) and a half-typed command or visual
    /// selection (Esc abandons it); at rest in normal mode Esc cancels the form,
    /// as in the input modal. In Standard it only clears a selection.
    #[must_use]
    pub fn consumes_esc(&self, standard: bool) -> bool {
        let Some(slot) = self.slot.as_ref() else {
            return false;
        };
        if standard {
            return input_surface::has_selection(&slot.surface);
        }
        slot.surface.mode == PopupMode::Insert || !slot.surface.vim_state.is_idle()
    }

    /// Whether typed characters go into the text right now (insert mode, or
    /// Standard editing). In vim normal mode a character is a command.
    #[must_use]
    pub fn is_typing(&self, standard: bool) -> bool {
        standard
            || self
                .slot
                .as_ref()
                .is_some_and(|slot| slot.surface.mode == PopupMode::Insert)
    }

    /// Whether a Standard selection is active (Ctrl-C copies it instead of quitting).
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.slot
            .as_ref()
            .is_some_and(|slot| input_surface::has_selection(&slot.surface))
    }

    /// The mode the focused field is in, for the form's border.
    #[must_use]
    pub fn mode_label(&self) -> &'static str {
        let Some(slot) = self.slot.as_ref() else {
            return "INSERT";
        };
        if slot.surface.standard_editing {
            return "EDIT";
        }
        match slot.surface.vim_state.visual {
            Some(crate::vim_textarea::VisualMode::Char) => return "VISUAL",
            Some(crate::vim_textarea::VisualMode::Line) => return "V-LINE",
            None => {}
        }
        match slot.surface.mode {
            PopupMode::Insert => "INSERT",
            PopupMode::Normal => "NORMAL",
        }
    }

    /// Route one key through the surface.
    pub fn handle_key(&mut self, key: KeyEvent, spec: FieldSpec, standard: bool) -> TextKey {
        let Some(slot) = self.slot.as_mut() else {
            return TextKey::Ignored;
        };
        let Slot {
            surface,
            undo,
            redo,
            group_open,
            ..
        } = &mut **slot;
        // Vim `u` and Ctrl-r walk the field's own history.
        if !standard
            && surface.mode == PopupMode::Normal
            && surface.vim_state.is_idle()
            && surface.vim_state.visual.is_none()
        {
            let step = match (key.code, key.modifiers) {
                (KeyCode::Char('u'), KeyModifiers::NONE) => Some((&mut *undo, &mut *redo)),
                (KeyCode::Char('r'), KeyModifiers::CONTROL) => Some((&mut *redo, &mut *undo)),
                _ => None,
            };
            if let Some((from, to)) = step {
                *group_open = false;
                let Some(target) = from.pop() else {
                    return TextKey::Moved;
                };
                let before_text = surface.content();
                to.push(snapshot(surface));
                restore(surface, target);
                return if surface.content() == before_text {
                    TextKey::Moved
                } else {
                    TextKey::Edited
                };
            }
        }
        let before = spec.max_chars.map(|_| surface.clone());
        let before_text = surface.content();
        let before_snapshot = snapshot(surface);
        let was_insert = surface.mode == PopupMode::Insert;
        let key = if spec.multiline {
            key
        } else {
            single_line_key(key, surface, standard)
        };
        let config = InputSurfaceConfig {
            pass_through_unhandled: false,
            available_commands: &[],
            working_dir: None,
            submit_on_enter: false,
            standard_editing: standard,
        };
        match input_surface::handle_key(surface, key, &config) {
            InputAction::Consumed => {}
            // Ctrl-Enter, Ctrl-Q and the compile-preview outcomes belong to the owner.
            _ => return TextKey::Ignored,
        }
        if !spec.multiline {
            flatten_to_one_line(surface);
        }
        if let (Some(max), Some(before)) = (spec.max_chars, before)
            && char_count(&surface.content().replace('\n', "")) > max
        {
            *surface = before;
            return TextKey::Refused;
        }
        if surface.content() == before_text {
            if surface.mode != PopupMode::Insert {
                *group_open = false;
            }
            TextKey::Moved
        } else {
            if !(was_insert && *group_open) {
                undo.push(before_snapshot);
                redo.clear();
            }
            *group_open = surface.mode == PopupMode::Insert;
            TextKey::Edited
        }
    }

    /// Insert pasted text at the cursor. A one-line field takes newlines as
    /// spaces; a field with a limit takes only what fits. Returns what happened
    /// and whether the text was cut.
    pub fn paste(&mut self, text: &str, spec: FieldSpec) -> (TextKey, bool) {
        let Some(slot) = self.slot.as_mut() else {
            return (TextKey::Ignored, false);
        };
        let surface = &mut slot.surface;
        let before_text = surface.content();
        let mut pasted = text.replace("\r\n", "\n").replace('\r', "\n");
        if !spec.multiline {
            pasted = pasted.replace('\n', " ");
        }
        let mut cut = false;
        if let Some(max) = spec.max_chars {
            let room = max.saturating_sub(char_count(&before_text));
            if char_count(&pasted) > room {
                pasted = pasted.chars().take(room).collect();
                cut = true;
            }
        }
        if pasted.is_empty() {
            return (
                if cut {
                    TextKey::Refused
                } else {
                    TextKey::Moved
                },
                cut,
            );
        }
        surface.insert_pasted_text(&pasted);
        if surface.content() == before_text {
            (TextKey::Moved, cut)
        } else {
            (TextKey::Edited, cut)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(text: &mut FormText, spec: FieldSpec, input: &str) {
        for c in input.chars() {
            text.handle_key(key(KeyCode::Char(c)), spec, false);
        }
    }

    fn loaded(value: &str, spec: FieldSpec) -> FormText {
        let mut text = FormText::default();
        text.load(0, value, spec, false);
        text
    }

    #[test]
    fn a_field_opens_in_insert_mode_at_the_end_of_its_text() {
        let spec = FieldSpec::SINGLE_LINE;
        let mut text = loaded("fix the bug", spec);
        type_text(&mut text, spec, "!");
        assert_eq!(text.content().as_deref(), Some("fix the bug!"));
        assert_eq!(text.mode_label(), "INSERT");
    }

    #[test]
    fn escape_gives_normal_mode_with_vim_motions_and_operators() {
        let spec = FieldSpec::SINGLE_LINE;
        let mut text = loaded("fix the bug", spec);
        assert!(text.consumes_esc(false), "insert mode keeps its Esc");
        text.handle_key(key(KeyCode::Esc), spec, false);
        assert_eq!(text.mode_label(), "NORMAL");
        assert!(
            !text.consumes_esc(false),
            "normal mode at rest gives Esc back"
        );
        // 0 w ciw: change the middle word.
        type_text(&mut text, spec, "0wciwa");
        text.handle_key(key(KeyCode::Esc), spec, false);
        assert_eq!(text.content().as_deref(), Some("fix a bug"));
        // x deletes under the cursor, u undoes it.
        type_text(&mut text, spec, "0x");
        assert_eq!(text.content().as_deref(), Some("ix a bug"));
        type_text(&mut text, spec, "u");
        assert_eq!(text.content().as_deref(), Some("fix a bug"));
        // dd clears the field but leaves one line.
        type_text(&mut text, spec, "dd");
        assert_eq!(text.content().as_deref(), Some(""));
    }

    #[test]
    fn a_one_line_field_never_keeps_a_newline() {
        let spec = FieldSpec::SINGLE_LINE;
        let mut text = loaded("abc", spec);
        text.handle_key(key(KeyCode::Esc), spec, false);
        // o would open a line below; the field appends instead.
        type_text(&mut text, spec, "oX");
        assert_eq!(text.content().as_deref(), Some("abcX"));
        // Shift-Enter in insert mode adds no line either.
        text.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            spec,
            false,
        );
        assert!(!text.content().unwrap_or_default().contains('\n'));
        let (result, cut) = text.paste("one\ntwo", spec);
        assert_eq!(result, TextKey::Edited);
        assert!(!cut);
        assert!(text.content().unwrap_or_default().ends_with("one two"));
    }

    #[test]
    fn a_multiline_field_takes_enter_as_a_newline_in_insert_mode() {
        let spec = FieldSpec::MULTILINE;
        let mut text = loaded("one", spec);
        text.handle_key(key(KeyCode::Enter), spec, false);
        type_text(&mut text, spec, "two");
        assert_eq!(text.content().as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn the_limit_refuses_typing_and_cuts_a_paste() {
        let spec = FieldSpec {
            multiline: false,
            max_chars: Some(5),
        };
        let mut text = loaded("abcd", spec);
        assert_eq!(
            text.handle_key(key(KeyCode::Char('e')), spec, false),
            TextKey::Edited
        );
        assert_eq!(
            text.handle_key(key(KeyCode::Char('f')), spec, false),
            TextKey::Refused
        );
        assert_eq!(text.content().as_deref(), Some("abcde"));
        let mut text = loaded("ab", spec);
        let (result, cut) = text.paste("cdefgh", spec);
        assert_eq!(result, TextKey::Edited);
        assert!(cut);
        assert_eq!(text.content().as_deref(), Some("abcde"));
        let (result, cut) = text.paste("zz", spec);
        assert_eq!(result, TextKey::Refused);
        assert!(cut);
    }

    #[test]
    fn a_surface_follows_its_field_and_rebuilds_when_the_text_changes_underneath() {
        let spec = FieldSpec::SINGLE_LINE;
        let mut text = loaded("abc", spec);
        text.handle_key(key(KeyCode::Esc), spec, false);
        text.load(0, "abc", spec, false);
        assert_eq!(text.mode_label(), "NORMAL", "same text keeps the mode");
        text.load(0, "reloaded", spec, false);
        assert_eq!(text.mode_label(), "INSERT", "new text starts over");
        assert_eq!(text.content().as_deref(), Some("reloaded"));
        text.load(1, "reloaded", spec, false);
        assert!(text.surface_for(0).is_none());
        assert!(text.surface_for(1).is_some());
    }
}
