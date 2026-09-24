//! Reusable vim-modal text editing surface.
//!
//! Shared between input bar, prompt overlay, and input modal.
//! Provides the struct, key handler, and suggestion management.
//! Callers provide chrome (borders, mode pill, CWD line, hint bar)
//! and handle caller-specific concerns (grammar correction triggering,
//! clipboard paste, submit routing, draft persistence).

use crate::suggestions::{self, CommandSuggestion, SuggestionMode};
use crate::types::PopupMode;
use crate::vim_textarea::{self, VimState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::cell::Cell;
use std::path::Path;
use tui_textarea::CursorMove;

/// Full context from a prompt compilation, preserved for persistence.
#[derive(Debug, Clone)]
pub struct CompileContext {
    pub original_input: String,
    pub contract_status: String,
    pub layer_semantic: bool,
    pub layer_syntactic: bool,
    pub layer_deictic: bool,
    pub layer_discourse: bool,
    pub layer_pragmatic: bool,
}

/// Reusable vim-modal text editing surface.
/// Shared between input bar, prompt overlay, and new input modal.
#[derive(Debug, Clone)]
pub struct InputSurface {
    /// The textarea widget holding draft text.
    pub textarea: Box<tui_textarea::TextArea<'static>>,
    /// Current vim mode (Insert or Normal).
    pub mode: PopupMode,
    /// Vim editing state (operator-pending, visual mode, char search, etc.).
    pub vim_state: VimState,
    /// Indices into available commands matching current filter.
    pub filtered_indices: Vec<usize>,
    /// Currently highlighted suggestion index.
    pub selected_suggestion: usize,
    /// Whether suggestion dropdown is visible.
    pub suggestions_visible: bool,
    /// Which kind of suggestion dropdown is active (Command, File, or None).
    pub suggestion_mode: SuggestionMode,
    /// Cached file paths for `@` file suggestions (populated on-demand).
    pub file_paths: Vec<String>,
    /// True while a prompt correction request is in-flight.
    pub correction_in_flight: bool,
    /// Corrected text awaiting user acceptance.
    pub corrected_preview: Option<String>,
    /// Full compile context preserved alongside corrected_preview for persistence.
    pub corrected_compile_context: Option<CompileContext>,
    /// Render width of the textarea area, set each frame by the render function.
    /// Used by insert-mode auto-wrap to break long lines into real buffer lines.
    pub wrap_width: Cell<usize>,
}

impl Default for InputSurface {
    fn default() -> Self {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        textarea.set_block(ratatui::widgets::Block::default());
        Self {
            textarea: Box::new(textarea),
            mode: PopupMode::Normal,
            vim_state: VimState::default(),
            filtered_indices: Vec::new(),
            selected_suggestion: 0,
            suggestions_visible: false,
            suggestion_mode: SuggestionMode::None,
            file_paths: Vec::new(),
            correction_in_flight: false,
            corrected_preview: None,
            corrected_compile_context: None,
            wrap_width: Cell::new(0),
        }
    }
}

impl InputSurface {
    /// Create a surface that starts in insert mode (for overlays).
    pub fn new_insert() -> Self {
        Self {
            mode: PopupMode::Insert,
            ..Self::default()
        }
    }

    /// Create a surface starting in insert mode with pre-filled content.
    pub fn new_insert_with_content(lines: Vec<String>) -> Self {
        let mut textarea = tui_textarea::TextArea::new(lines);
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        textarea.set_block(ratatui::widgets::Block::default());
        Self {
            textarea: Box::new(textarea),
            mode: PopupMode::Insert,
            ..Self::default()
        }
    }

    /// Get the text content as a single string.
    pub fn content(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Get the trimmed text content.
    pub fn content_trimmed(&self) -> String {
        self.content().trim().to_string()
    }

    /// Get text content for sending, with visual line-wraps collapsed.
    ///
    /// `auto_wrap_if_needed` and `wrap_all_lines` insert real `\n` characters
    /// into the textarea buffer for display wrapping. This method applies the
    /// same normalization as `normalize_pasted_text` — single `\n` boundaries
    /// (from wrapping) become spaces, while `\n\n` paragraph breaks survive —
    /// then collapses runs of spaces left by `word_boundary_break` trailing
    /// spaces, so the sent text matches what the user intended to type.
    pub fn content_for_send(&self) -> String {
        let raw = self.content();
        let normalized = normalize_pasted_text(&raw);
        // Collapse runs of spaces left by word_boundary_break trailing spaces
        // joining with the normalization-inserted space.
        let mut result = String::with_capacity(normalized.len());
        let mut prev_space = false;
        for ch in normalized.chars() {
            if ch == ' ' {
                if !prev_space {
                    result.push(ch);
                }
                prev_space = true;
            } else {
                result.push(ch);
                prev_space = false;
            }
        }
        result.trim().to_string()
    }

    /// Whether the textarea has any non-empty content.
    pub fn has_content(&self) -> bool {
        self.textarea.lines().iter().any(|l| !l.is_empty())
    }

    /// Reset the textarea to empty and return to normal mode.
    pub fn clear(&mut self) {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        textarea.set_block(ratatui::widgets::Block::default());
        *self.textarea = textarea;
        self.mode = PopupMode::Normal;
        self.vim_state.reset_transient();
        self.suggestions_visible = false;
        self.filtered_indices.clear();
        self.suggestion_mode = SuggestionMode::None;
        self.file_paths.clear();
    }

    /// Enter insert mode with vim-style cursor positioning.
    pub fn enter_insert(&mut self, style: crate::modalkit_types::InsertStyle) {
        match style {
            crate::modalkit_types::InsertStyle::Insert => {}
            crate::modalkit_types::InsertStyle::Append => {
                self.textarea.move_cursor(CursorMove::Forward);
            }
            crate::modalkit_types::InsertStyle::OpenBelow => {
                self.textarea.move_cursor(CursorMove::End);
                self.textarea.insert_newline();
            }
            crate::modalkit_types::InsertStyle::OpenAbove => {
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.insert_newline();
                self.textarea.move_cursor(CursorMove::Up);
            }
        }
        self.mode = PopupMode::Insert;
        self.vim_state.reset_transient();
    }

    /// Insert clipboard text as data, without routing its characters through
    /// vim or textarea key bindings.
    pub(crate) fn insert_pasted_text(&mut self, text: &str) {
        if self.mode != PopupMode::Insert {
            self.enter_insert(crate::modalkit_types::InsertStyle::Insert);
        }
        if self.vim_state.insert_start_snapshot.is_none() {
            let content = self.content();
            self.vim_state.snapshot_for_insert(&content);
        }
        self.textarea.insert_str(text);
    }
}

/// Result of processing a key through an InputSurface.
#[derive(Debug)]
pub enum InputAction {
    /// Key was consumed by the surface (mode change, text edit, suggestion nav, etc.).
    Consumed,
    /// User submitted the content (Ctrl+Enter from any mode).
    Submit(String),
    /// User requested to close without submitting (q in normal mode for overlays).
    Close,
    /// Key was not handled — caller should process it (only returned in pass-through mode).
    Passthrough(KeyEvent),
    /// User accepted or denied a compiled prompt. Carries persistence data.
    CompileDecision {
        accepted: bool,
        context: CompileContext,
        compiled_output: String,
    },
}

/// Configuration for how an InputSurface behaves in a specific context.
pub struct InputSurfaceConfig<'a> {
    /// If true, unhandled normal-mode keys return Passthrough instead of Consumed.
    /// Used by the input bar to let keys fall through to session detail.
    pub pass_through_unhandled: bool,
    /// Available commands for suggestion autocomplete.
    pub available_commands: &'a [CommandSuggestion],
    /// Working directory for `@` file suggestions. `None` disables file completion.
    pub working_dir: Option<&'a Path>,
    /// When true, plain Enter in insert mode submits; Shift+Enter inserts a newline.
    /// When false, only Ctrl+Enter submits and plain Enter inserts a newline (legacy).
    /// Ctrl+Enter from any mode always submits regardless of this flag.
    pub submit_on_enter: bool,
}

/// Process a key event through the InputSurface.
///
/// Handles: Ctrl+Enter (submit), correction preview accept/discard,
/// insert mode editing + suggestions, normal mode vim commands.
///
/// Does NOT handle: grammar correction triggering (caller-specific),
/// clipboard paste (needs app context), Ctrl+T/Ctrl+S overlay-specific submits,
/// Ctrl+M model selector (overlay-specific).
pub fn handle_key(
    surface: &mut InputSurface,
    key: KeyEvent,
    config: &InputSurfaceConfig<'_>,
) -> InputAction {
    // 1. Ctrl+Enter submits from any mode (even if empty — caller decides behavior)
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Enter {
        let content = surface.content_for_send();
        return InputAction::Submit(content);
    }

    // 2. Correction preview intercept (before mode-specific handling)
    if let Some(ref preview) = surface.corrected_preview {
        let is_clarify = preview.starts_with("CLARIFY:");
        match key.code {
            KeyCode::Char('a') if !is_clarify => {
                let preview = surface.corrected_preview.take().unwrap();
                let context = surface.corrected_compile_context.take();
                let lines: Vec<String> = preview.lines().map(str::to_string).collect();
                let mut new_ta = tui_textarea::TextArea::new(lines.clone());
                new_ta.set_cursor_line_style(ratatui::style::Style::default());
                new_ta.set_block(ratatui::widgets::Block::default());
                let compiled_output = lines.join("\n");
                *surface.textarea = new_ta;
                surface.mode = PopupMode::Insert;
                return match context {
                    Some(ctx) => InputAction::CompileDecision {
                        accepted: true,
                        context: ctx,
                        compiled_output,
                    },
                    None => InputAction::Consumed,
                };
            }
            KeyCode::Char('d') | KeyCode::Esc => {
                let compiled_output = surface.corrected_preview.take().unwrap_or_default();
                let context = surface.corrected_compile_context.take();
                return match context {
                    Some(ctx) => InputAction::CompileDecision {
                        accepted: false,
                        context: ctx,
                        compiled_output,
                    },
                    None => InputAction::Consumed,
                };
            }
            _ => {}
        }
    }

    // 3. Mode-specific handling
    match surface.mode {
        PopupMode::Insert => handle_insert_mode(surface, key, config),
        PopupMode::Normal => handle_normal_mode(surface, key, config),
    }
}

/// Handle keys in insert mode.
fn handle_insert_mode(
    surface: &mut InputSurface,
    key: KeyEvent,
    config: &InputSurfaceConfig<'_>,
) -> InputAction {
    // Non-pass-through: Ctrl+Q closes even while inserting (the new-session
    // modal must be quittable from insert mode). Pass-through surfaces (the
    // session input bar) keep feeding unhandled chords to their caller.
    if !config.pass_through_unhandled
        && key.code == KeyCode::Char('q')
        && key.modifiers == KeyModifiers::CONTROL
    {
        return InputAction::Close;
    }

    // Suggestion navigation when visible. Plain Enter follows the surface's
    // primary action (submit/newline); Tab is the explicit accept chord.
    if surface.suggestions_visible {
        match key.code {
            KeyCode::Esc => {
                surface.suggestions_visible = false;
                return InputAction::Consumed;
            }
            KeyCode::Tab => {
                accept_suggestion(surface, config.available_commands, config.working_dir);
                return InputAction::Consumed;
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                move_suggestion_selection(surface, 1);
                return InputAction::Consumed;
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                move_suggestion_selection(surface, -1);
                return InputAction::Consumed;
            }
            KeyCode::Down => {
                move_suggestion_selection(surface, 1);
                return InputAction::Consumed;
            }
            KeyCode::Up => {
                move_suggestion_selection(surface, -1);
                return InputAction::Consumed;
            }
            _ => {} // Fall through to normal input
        }
    }

    // Plain Enter submits when submit_on_enter is true (and suggestion popup is
    // not handling it — that branch returns above). Explicit modifier equality
    // ensures Shift+Enter and Alt+Enter still fall through to native textarea
    // input (Shift+Enter → newline; Alt+Enter → ignored).
    if config.submit_on_enter && key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
        let content = surface.content_for_send();
        return InputAction::Submit(content);
    }

    // Esc → normal mode
    if key.code == KeyCode::Esc {
        surface.mode = PopupMode::Normal;
        surface.suggestions_visible = false;
        let content = surface.content();
        surface.vim_state.finalize_insert_from_snapshot(&content);
        return InputAction::Consumed;
    }

    // Pass-through mode: Up/Down pass through for session detail scroll
    if config.pass_through_unhandled && (key.code == KeyCode::Up || key.code == KeyCode::Down) {
        return InputAction::Passthrough(key);
    }

    // Arrow keys for cursor movement
    match key.code {
        KeyCode::Left => {
            surface.textarea.move_cursor(CursorMove::Back);
        }
        KeyCode::Right => {
            surface.textarea.move_cursor(CursorMove::Forward);
        }
        KeyCode::Up => {
            let wrap_width = surface.wrap_width.get();
            crate::vim_textarea::move_vertical_with_curswant(
                &mut surface.textarea,
                &mut surface.vim_state,
                -1,
                Some(wrap_width),
                true,
            );
        }
        KeyCode::Down => {
            let wrap_width = surface.wrap_width.get();
            crate::vim_textarea::move_vertical_with_curswant(
                &mut surface.textarea,
                &mut surface.vim_state,
                1,
                Some(wrap_width),
                true,
            );
        }
        _ => {
            surface.textarea.input_without_shortcuts(key);
        }
    }

    // Update suggestions after input
    update_suggestions(surface, config.available_commands, config.working_dir);

    InputAction::Consumed
}

/// Handle keys in normal mode.
fn handle_normal_mode(
    surface: &mut InputSurface,
    key: KeyEvent,
    config: &InputSurfaceConfig<'_>,
) -> InputAction {
    // Pass-through mode with empty content: all keys pass through
    if config.pass_through_unhandled && !surface.has_content() {
        return InputAction::Passthrough(key);
    }

    // Non-pass-through: Ctrl+Q closes.
    if !config.pass_through_unhandled
        && key.code == KeyCode::Char('q')
        && key.modifiers == KeyModifiers::CONTROL
    {
        return InputAction::Close;
    }

    // Delegate to shared vim normal-mode handler
    let lines_snapshot: Vec<String> = surface.textarea.lines().to_vec();
    let ctx = crate::vim_textarea::FileEditorCtx {
        indent_style: crate::types::IndentStyle::default(),
        lines: &lines_snapshot,
        wrap_width: Some(surface.wrap_width.get()),
    };
    let action = vim_textarea::handle_vim_normal(
        &mut surface.textarea,
        &mut surface.vim_state,
        key,
        Some(&ctx),
    );

    match action {
        vim_textarea::VimAction::EnteredInsert => {
            surface.mode = PopupMode::Insert;
            let content = surface.content();
            surface.vim_state.snapshot_for_insert(&content);
            update_suggestions(surface, config.available_commands, config.working_dir);
            InputAction::Consumed
        }
        vim_textarea::VimAction::Consumed => InputAction::Consumed,
        vim_textarea::VimAction::Unhandled => {
            if config.pass_through_unhandled {
                InputAction::Passthrough(key)
            } else {
                InputAction::Consumed
            }
        }
    }
}

/// Update suggestion visibility and filtering based on current textarea content.
///
/// Checks `/` command trigger first (takes priority), then `@` file trigger.
fn update_suggestions(
    surface: &mut InputSurface,
    commands: &[CommandSuggestion],
    working_dir: Option<&Path>,
) {
    // Try slash-command first (line must start with `/`)
    if let Some(query) = suggestions::extract_slash_query(&surface.textarea) {
        let scored = suggestions::filter_suggestions(commands, &query);
        surface.filtered_indices = scored.into_iter().map(|s| s.index).collect();
        surface.selected_suggestion = 0;
        surface.suggestions_visible = !surface.filtered_indices.is_empty();
        surface.suggestion_mode = SuggestionMode::Command;
        return;
    }

    // Try `@` file trigger (requires working_dir)
    if let Some(query) = suggestions::extract_at_query(&surface.textarea)
        && let Some(dir) = working_dir
    {
        // Populate file list on first trigger (cleared when suggestions dismiss)
        if surface.file_paths.is_empty() {
            surface.file_paths = crate::file_utils::walk_files(dir, false)
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
        }

        let scored = suggestions::filter_file_suggestions(&surface.file_paths, &query, 7);
        surface.filtered_indices = scored.into_iter().map(|s| s.index).collect();
        surface.selected_suggestion = 0;
        surface.suggestions_visible = !surface.filtered_indices.is_empty();
        surface.suggestion_mode = SuggestionMode::File;
        return;
    }

    // No trigger active — clear all suggestion state
    surface.suggestions_visible = false;
    surface.filtered_indices.clear();
    surface.selected_suggestion = 0;
    surface.suggestion_mode = SuggestionMode::None;
    surface.file_paths.clear();
}

/// Move the suggestion selection up or down (wrapping).
fn move_suggestion_selection(surface: &mut InputSurface, delta: i32) {
    let len = surface.filtered_indices.len();
    if len == 0 {
        return;
    }
    let new = (surface.selected_suggestion as i32 + delta).rem_euclid(len as i32);
    surface.selected_suggestion = new as usize;
}

/// Accept the currently selected suggestion.
///
/// In Command mode: replaces the entire line with `/{command} {after_cursor}`.
/// In File mode: replaces only the `@query` segment with `@path `.
fn accept_suggestion(
    surface: &mut InputSurface,
    commands: &[CommandSuggestion],
    _working_dir: Option<&Path>,
) {
    if surface.filtered_indices.is_empty() {
        return;
    }

    let idx = surface.filtered_indices[surface.selected_suggestion];

    match surface.suggestion_mode {
        SuggestionMode::Command => {
            let command_name = &commands[idx].name;
            let (row, col) = surface.textarea.cursor();
            let line = surface.textarea.lines()[row].clone();
            // Cursor col is a char index; convert to bytes before slicing.
            let byte_col = crate::ui::session::byte_offset_of_col(&line, col);
            let after_cursor = &line[byte_col..];
            let new_line = format!("/{command_name} {after_cursor}");
            // CursorMove::Forward steps chars, so count chars (not bytes).
            let new_cursor_col = command_name.chars().count() + 2; // after "/{name} "

            surface.textarea.move_cursor(CursorMove::Head);
            surface.textarea.delete_line_by_end();
            surface.textarea.insert_str(&new_line);

            surface.textarea.move_cursor(CursorMove::Head);
            for _ in 0..new_cursor_col {
                surface.textarea.move_cursor(CursorMove::Forward);
            }
        }
        SuggestionMode::File => {
            let file_path = surface.file_paths[idx].clone();
            let (row, col) = surface.textarea.cursor();
            let line = surface.textarea.lines()[row].clone();
            // Cursor col is a char index; convert to bytes before slicing.
            let byte_col = crate::ui::session::byte_offset_of_col(&line, col);
            let before_cursor = &line[..byte_col];

            // Find the `@` trigger position (scan backward; `at_pos` is a
            // byte index from `rfind`, so byte slicing with it is safe)
            if let Some(at_pos) = before_cursor.rfind('@') {
                let before_at = &line[..at_pos];
                let after_cursor = &line[byte_col..];
                let new_line = format!("{before_at}@{file_path} {after_cursor}");
                // CursorMove::Forward steps chars, so count chars (not bytes).
                let new_cursor_col = before_at.chars().count() + 1 + file_path.chars().count() + 1; // after "@path "

                surface.textarea.move_cursor(CursorMove::Head);
                surface.textarea.delete_line_by_end();
                surface.textarea.insert_str(&new_line);

                surface.textarea.move_cursor(CursorMove::Head);
                for _ in 0..new_cursor_col {
                    surface.textarea.move_cursor(CursorMove::Forward);
                }
            }
        }
        SuggestionMode::None => {}
    }

    // Dismiss suggestions and clear file cache for next trigger
    surface.suggestions_visible = false;
    surface.filtered_indices.clear();
    surface.selected_suggestion = 0;
    surface.suggestion_mode = SuggestionMode::None;
    surface.file_paths.clear();
}

/// Normalize pasted text by collapsing single newlines into spaces while
/// preserving paragraph breaks (double newlines). This handles text from
/// dictation agents that pre-wrap at arbitrary widths (e.g., 25-30 chars).
///
/// Rules:
/// - `\n\n` (or more) → preserved as paragraph break (collapsed to `\n\n`)
/// - Single `\n` between non-empty content → replaced with space
/// - Leading/trailing whitespace on lines is preserved
pub(crate) fn normalize_pasted_text(text: &str) -> String {
    // Fast path: no newlines at all
    if !text.contains('\n') {
        return text.to_string();
    }

    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\n' {
            // Count consecutive newlines
            let mut newline_count = 1;
            while chars.peek() == Some(&'\n') {
                newline_count += 1;
                chars.next();
            }

            if newline_count >= 2 {
                // Paragraph break — preserve as double newline
                result.push('\n');
                result.push('\n');
            } else {
                // Single newline — replace with space (unless at start/end)
                if !result.is_empty() && chars.peek().is_some() {
                    result.push(' ');
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn passthrough_config() -> InputSurfaceConfig<'static> {
        InputSurfaceConfig {
            pass_through_unhandled: true,
            available_commands: &[],
            working_dir: None,
            submit_on_enter: true,
        }
    }

    fn overlay_config() -> InputSurfaceConfig<'static> {
        InputSurfaceConfig {
            pass_through_unhandled: false,
            available_commands: &[],
            working_dir: None,
            submit_on_enter: true,
        }
    }

    /// Legacy config for explicit `submit_on_enter == false` test coverage.
    fn legacy_config() -> InputSurfaceConfig<'static> {
        InputSurfaceConfig {
            pass_through_unhandled: false,
            available_commands: &[],
            working_dir: None,
            submit_on_enter: false,
        }
    }

    #[test]
    fn test_insert_mode_text_produces_consumed() {
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        let result = handle_key(&mut surface, key(KeyCode::Char('h')), &config);
        assert!(matches!(result, InputAction::Consumed));
        assert_eq!(surface.textarea.lines(), &["h"]);
    }

    #[test]
    fn test_ctrl_enter_produces_submit() {
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        // Type some text first
        handle_key(&mut surface, key(KeyCode::Char('h')), &config);
        handle_key(&mut surface, key(KeyCode::Char('i')), &config);

        let result = handle_key(&mut surface, ctrl_key(KeyCode::Enter), &config);
        match result {
            InputAction::Submit(content) => assert_eq!(content, "hi"),
            other => panic!("Expected Submit, got {:?}", other),
        }
    }

    #[test]
    fn test_ctrl_enter_empty_produces_submit_empty() {
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        let result = handle_key(&mut surface, ctrl_key(KeyCode::Enter), &config);
        match result {
            InputAction::Submit(content) => assert!(content.is_empty()),
            other => panic!("Expected Submit with empty content, got {:?}", other),
        }
    }

    #[test]
    fn test_normal_mode_ctrl_q_overlay_produces_close() {
        let mut surface = InputSurface::default(); // starts in Normal mode
        let config = overlay_config();
        let result = handle_key(&mut surface, ctrl_key(KeyCode::Char('q')), &config);
        assert!(matches!(result, InputAction::Close));
    }

    #[test]
    fn test_insert_mode_ctrl_q_overlay_produces_close() {
        let mut surface = InputSurface::new_insert(); // starts in Insert mode
        let config = overlay_config();
        let result = handle_key(&mut surface, ctrl_key(KeyCode::Char('q')), &config);
        assert!(matches!(result, InputAction::Close));
    }

    #[test]
    fn test_insert_mode_ctrl_q_passthrough_does_not_close() {
        // The session input bar is pass-through: Ctrl+Q must not close it.
        let mut surface = InputSurface::new_insert_with_content(vec!["draft".to_string()]);
        let config = passthrough_config();
        let result = handle_key(&mut surface, ctrl_key(KeyCode::Char('q')), &config);
        assert!(!matches!(result, InputAction::Close));
    }

    #[test]
    fn test_normal_mode_empty_passthrough_returns_passthrough() {
        let mut surface = InputSurface::default(); // Normal mode, empty
        let config = passthrough_config();
        let result = handle_key(&mut surface, key(KeyCode::Char('j')), &config);
        assert!(matches!(result, InputAction::Passthrough(_)));
    }

    #[test]
    fn test_normal_mode_with_content_unhandled_passthrough() {
        let mut surface = InputSurface::new_insert();
        let config = passthrough_config();
        // Type content, then go to normal mode
        handle_key(&mut surface, key(KeyCode::Char('h')), &config);
        handle_key(&mut surface, key(KeyCode::Esc), &config);
        assert_eq!(surface.mode, PopupMode::Normal);

        // 'q' is unhandled by vim handler → Passthrough in passthrough mode
        let result = handle_key(&mut surface, key(KeyCode::Char('q')), &config);
        assert!(matches!(result, InputAction::Passthrough(_)));
    }

    #[test]
    fn test_overlay_unhandled_key_consumed() {
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        // Type content, then go to normal mode
        handle_key(&mut surface, key(KeyCode::Char('h')), &config);
        handle_key(&mut surface, key(KeyCode::Esc), &config);

        // A key that vim doesn't handle → Consumed (not passthrough) in overlay mode
        let result = handle_key(&mut surface, key(KeyCode::Char('z')), &config);
        assert!(matches!(result, InputAction::Consumed));
    }

    #[test]
    fn test_esc_in_insert_switches_to_normal() {
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        handle_key(&mut surface, key(KeyCode::Char('h')), &config);
        let result = handle_key(&mut surface, key(KeyCode::Esc), &config);
        assert!(matches!(result, InputAction::Consumed));
        assert_eq!(surface.mode, PopupMode::Normal);
    }

    #[test]
    fn test_insert_up_down_passthrough_mode() {
        let mut surface = InputSurface::new_insert();
        let config = passthrough_config();
        let result = handle_key(&mut surface, key(KeyCode::Up), &config);
        assert!(matches!(result, InputAction::Passthrough(_)));
        let result = handle_key(&mut surface, key(KeyCode::Down), &config);
        assert!(matches!(result, InputAction::Passthrough(_)));
    }

    #[test]
    fn test_correction_preview_accept() {
        let mut surface = InputSurface::new_insert();
        surface.mode = PopupMode::Normal;
        surface.corrected_preview = Some("corrected text".to_string());

        let config = overlay_config();
        let result = handle_key(&mut surface, key(KeyCode::Char('a')), &config);
        assert!(matches!(result, InputAction::Consumed));
        assert!(surface.corrected_preview.is_none());
        assert_eq!(surface.textarea.lines(), &["corrected text"]);
        assert_eq!(surface.mode, PopupMode::Insert);
    }

    #[test]
    fn test_correction_preview_discard() {
        let mut surface = InputSurface::default();
        surface.corrected_preview = Some("corrected".to_string());

        let config = overlay_config();
        let result = handle_key(&mut surface, key(KeyCode::Char('d')), &config);
        assert!(matches!(result, InputAction::Consumed));
        assert!(surface.corrected_preview.is_none());
    }

    #[test]
    fn test_clarify_preview_no_accept() {
        let mut surface = InputSurface::default();
        surface.corrected_preview = Some("CLARIFY: what do you mean?".to_string());

        let config = overlay_config();
        // 'a' should NOT accept a CLARIFY preview
        let _result = handle_key(&mut surface, key(KeyCode::Char('a')), &config);
        // 'a' falls through (CLARIFY blocks accept), then vim handler processes it
        assert!(surface.corrected_preview.is_some()); // Still set (not accepted)
    }

    #[test]
    fn test_clear_resets_state() {
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        handle_key(&mut surface, key(KeyCode::Char('x')), &config);
        surface.correction_in_flight = true;

        surface.clear();
        assert_eq!(surface.mode, PopupMode::Normal);
        assert_eq!(surface.textarea.lines(), &[""]);
        assert!(!surface.suggestions_visible);
    }

    #[test]
    fn test_content_helpers() {
        let mut surface = InputSurface::new_insert();
        assert!(!surface.has_content());
        assert_eq!(surface.content_trimmed(), "");

        let config = overlay_config();
        handle_key(&mut surface, key(KeyCode::Char('h')), &config);
        handle_key(&mut surface, key(KeyCode::Char('i')), &config);
        assert!(surface.has_content());
        assert_eq!(surface.content_trimmed(), "hi");
    }

    #[test]
    fn test_enter_insert_styles() {
        // Test `i` — cursor stays
        let mut surface = InputSurface::default();
        surface.enter_insert(crate::modalkit_types::InsertStyle::Insert);
        assert_eq!(surface.mode, PopupMode::Insert);

        // Test `a` — cursor moves forward
        let mut surface = InputSurface::default();
        surface.enter_insert(crate::modalkit_types::InsertStyle::Append);
        assert_eq!(surface.mode, PopupMode::Insert);

        // Test `o` — new line below
        let mut surface = InputSurface::default();
        surface.enter_insert(crate::modalkit_types::InsertStyle::OpenBelow);
        assert_eq!(surface.mode, PopupMode::Insert);

        // Test `O` — new line above
        let mut surface = InputSurface::default();
        surface.enter_insert(crate::modalkit_types::InsertStyle::OpenAbove);
        assert_eq!(surface.mode, PopupMode::Insert);
    }

    // ── Auto-wrap tests ──────────────────────────────────────────────

    /// Helper: make a TextArea pre-populated with text, cursor at end.
    fn textarea_with(text: &str) -> tui_textarea::TextArea<'static> {
        let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        let mut ta = if lines.is_empty() {
            tui_textarea::TextArea::default()
        } else {
            tui_textarea::TextArea::new(lines)
        };
        ta.move_cursor(tui_textarea::CursorMove::Bottom);
        ta.move_cursor(tui_textarea::CursorMove::End);
        ta
    }

    #[test]
    fn test_normalize_pasted_text_no_newlines() {
        assert_eq!(normalize_pasted_text("hello world"), "hello world");
    }

    #[test]
    fn test_normalize_pasted_text_single_newlines_become_spaces() {
        assert_eq!(
            normalize_pasted_text("hello\nworld\nfoo"),
            "hello world foo"
        );
    }

    #[test]
    fn test_normalize_pasted_text_double_newlines_preserved() {
        assert_eq!(
            normalize_pasted_text("paragraph one\n\nparagraph two"),
            "paragraph one\n\nparagraph two"
        );
    }

    #[test]
    fn test_normalize_pasted_text_mixed() {
        assert_eq!(
            normalize_pasted_text("line one\nline two\n\nnew paragraph\nmore text"),
            "line one line two\n\nnew paragraph more text"
        );
    }

    #[test]
    fn test_normalize_pasted_text_triple_newlines_collapse_to_double() {
        assert_eq!(normalize_pasted_text("a\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn test_normalize_pasted_text_trailing_newline() {
        // Trailing single newline should be dropped (no content after)
        assert_eq!(normalize_pasted_text("hello\n"), "hello");
    }

    #[test]
    fn test_normalize_pasted_text_leading_newline() {
        // Leading single newline should be dropped (no content before)
        assert_eq!(normalize_pasted_text("\nhello"), "hello");
    }

    #[test]
    fn test_normalize_dictation_style_text() {
        // Simulates dictation agent wrapping at ~30 chars
        let dictation = "This is a bug that\nhappened earlier and I was\nwaiting for it to happen\nso I could show the\nreplication via image.";
        let expected = "This is a bug that happened earlier and I was waiting for it to happen so I could show the replication via image.";
        assert_eq!(normalize_pasted_text(dictation), expected);
    }

    // ── content_for_send tests ──────────────────────────────────────

    #[test]
    fn test_content_for_send_collapses_visual_wraps() {
        // Simulate auto_wrap_if_needed inserting real newlines into the buffer.
        // A long line that got wrapped at width 20 should be reassembled on send.
        let surface = InputSurface::new_insert_with_content(vec![
            "the quick brown fox ".to_string(), // trailing space from word_boundary_break
            "jumps over the lazy".to_string(),
            "dog".to_string(),
        ]);
        // content_trimmed() would preserve the wrapping newlines
        assert_eq!(
            surface.content_trimmed(),
            "the quick brown fox \njumps over the lazy\ndog"
        );
        // content_for_send() should collapse them
        assert_eq!(
            surface.content_for_send(),
            "the quick brown fox jumps over the lazy dog"
        );
    }

    #[test]
    fn test_content_for_send_preserves_paragraph_breaks() {
        // Double newlines (blank lines) are intentional paragraph breaks.
        let surface = InputSurface::new_insert_with_content(vec![
            "paragraph one".to_string(),
            "".to_string(),
            "paragraph two".to_string(),
        ]);
        assert_eq!(surface.content_for_send(), "paragraph one\n\nparagraph two");
    }

    #[test]
    fn test_content_for_send_mixed_wraps_and_paragraphs() {
        // Simulates the exact bug: long line wrapped + paragraph break + more text
        let surface = InputSurface::new_insert_with_content(vec![
            "/debug @/path/to/file.png the auto ".to_string(),
            "select".to_string(),
            "".to_string(),
            "dropdown does not overlay".to_string(),
        ]);
        assert_eq!(
            surface.content_for_send(),
            "/debug @/path/to/file.png the auto select\n\ndropdown does not overlay"
        );
    }

    #[test]
    fn test_content_for_send_empty() {
        let surface = InputSurface::new_insert();
        assert_eq!(surface.content_for_send(), "");
    }

    #[test]
    fn test_content_for_send_single_line() {
        let surface = InputSurface::new_insert_with_content(vec!["no wrapping here".to_string()]);
        assert_eq!(surface.content_for_send(), "no wrapping here");
    }

    // -- RSI-026: submit_on_enter dual-mode coverage --------------------------

    #[test]
    fn test_plain_enter_submits_when_submit_on_enter_true() {
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        handle_key(&mut surface, key(KeyCode::Char('h')), &config);
        handle_key(&mut surface, key(KeyCode::Char('i')), &config);

        let result = handle_key(&mut surface, key(KeyCode::Enter), &config);
        match result {
            InputAction::Submit(content) => assert_eq!(content, "hi"),
            other => panic!("Expected Submit, got {:?}", other),
        }
    }

    #[test]
    fn test_shift_enter_inserts_newline_when_submit_on_enter_true() {
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        handle_key(&mut surface, key(KeyCode::Char('a')), &config);
        handle_key(&mut surface, shift_key(KeyCode::Enter), &config);
        handle_key(&mut surface, key(KeyCode::Char('b')), &config);

        // Shift+Enter should NOT submit; it falls through to the textarea,
        // which inserts a literal newline.
        assert_eq!(surface.mode, PopupMode::Insert);
        assert_eq!(surface.textarea.lines(), &["a", "b"]);
    }

    #[test]
    fn test_plain_enter_inserts_newline_when_submit_on_enter_false() {
        let mut surface = InputSurface::new_insert();
        let config = legacy_config();
        handle_key(&mut surface, key(KeyCode::Char('a')), &config);
        let result = handle_key(&mut surface, key(KeyCode::Enter), &config);
        handle_key(&mut surface, key(KeyCode::Char('b')), &config);

        // Plain Enter in legacy mode produces Consumed + newline insertion.
        assert!(matches!(result, InputAction::Consumed));
        assert_eq!(surface.mode, PopupMode::Insert);
        assert_eq!(surface.textarea.lines(), &["a", "b"]);
    }

    #[test]
    fn test_ctrl_enter_still_submits_when_submit_on_enter_false() {
        let mut surface = InputSurface::new_insert();
        let config = legacy_config();
        handle_key(&mut surface, key(KeyCode::Char('h')), &config);
        handle_key(&mut surface, key(KeyCode::Char('i')), &config);

        let result = handle_key(&mut surface, ctrl_key(KeyCode::Enter), &config);
        match result {
            InputAction::Submit(content) => assert_eq!(content, "hi"),
            other => panic!("Expected Submit in legacy mode, got {:?}", other),
        }
    }

    #[test]
    fn test_suggestion_popup_enter_submits_when_submit_on_enter_true() {
        let mut surface = InputSurface::new_insert();
        surface.suggestions_visible = true;
        surface.selected_suggestion = 0;

        let config = overlay_config();
        let result = handle_key(&mut surface, key(KeyCode::Enter), &config);

        match result {
            InputAction::Submit(content) => assert!(content.is_empty()),
            other => panic!("Expected Submit, got {:?}", other),
        }
    }

    #[test]
    fn test_suggestion_popup_tab_still_accepts_when_submit_on_enter_true() {
        let mut surface = InputSurface::new_insert();
        surface.suggestions_visible = true;

        let commands = vec![CommandSuggestion {
            name: "debug".to_string(),
            description: "Debug command".to_string(),
            source: suggestions::CommandSource::CustomCommand,
        }];
        let config = InputSurfaceConfig {
            pass_through_unhandled: false,
            available_commands: &commands,
            working_dir: None,
            submit_on_enter: true,
        };

        handle_key(&mut surface, key(KeyCode::Char('/')), &config);
        assert!(surface.suggestions_visible);

        let result = handle_key(&mut surface, key(KeyCode::Tab), &config);

        assert!(matches!(result, InputAction::Consumed));
        assert_eq!(surface.content(), "/debug ");
        assert!(!surface.suggestions_visible);
    }

    #[test]
    fn test_plain_enter_in_normal_mode_does_not_submit() {
        // The new submit branch is gated on PopupMode::Insert only. In
        // normal mode plain Enter must not submit (preserves vim semantics).
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        // Type some content then exit to normal mode.
        handle_key(&mut surface, key(KeyCode::Char('h')), &config);
        handle_key(&mut surface, key(KeyCode::Esc), &config);
        assert_eq!(surface.mode, PopupMode::Normal);

        let result = handle_key(&mut surface, key(KeyCode::Enter), &config);
        // Normal-mode plain Enter is not the submit path. It must not
        // produce InputAction::Submit. (Exact action is normal-mode dependent
        // — typically Consumed or Passthrough.)
        assert!(!matches!(result, InputAction::Submit(_)));
    }

    // -- Multibyte regression tests (char cursor col vs byte index) ----------
    // Pre-fix, accept_suggestion and the suggestion extractors sliced lines
    // with the char-based cursor column used as a byte index, panicking
    // mid-char on multibyte content (the paste-crash trigger).

    #[test]
    fn test_accept_suggestion_command_arm_multibyte() {
        let mut surface = InputSurface::new_insert();
        surface.textarea.insert_str("/dé ✨tail");
        // Cursor after "é" (char col 3) — multibyte before AND after cursor.
        surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::Jump(0, 3));

        let commands = vec![CommandSuggestion {
            name: "debug".to_string(),
            description: "Debug command".to_string(),
            source: suggestions::CommandSource::CustomCommand,
        }];
        surface.suggestion_mode = SuggestionMode::Command;
        surface.filtered_indices = vec![0];
        surface.selected_suggestion = 0;
        surface.suggestions_visible = true;

        accept_suggestion(&mut surface, &commands, None);

        assert_eq!(surface.textarea.lines(), &["/debug  ✨tail"]);
        // Cursor sits exactly after "/debug " in CHARS (not bytes).
        assert_eq!(surface.textarea.cursor(), (0, 7));
        assert!(!surface.suggestions_visible);
    }

    #[test]
    fn test_accept_suggestion_file_arm_multibyte() {
        let mut surface = InputSurface::new_insert();
        // Multibyte before the `@`, in the query, and in the file path.
        // Pre-fix, `&line[..col]` landed mid-`ï` and panicked.
        surface.textarea.insert_str("héllo @fï✨");

        surface.suggestion_mode = SuggestionMode::File;
        surface.file_paths = vec!["döcs/fïle.md".to_string()];
        surface.filtered_indices = vec![0];
        surface.selected_suggestion = 0;
        surface.suggestions_visible = true;

        accept_suggestion(&mut surface, &[], None);

        assert_eq!(surface.textarea.lines(), &["héllo @döcs/fïle.md "]);
        // "héllo " (6) + "@" (1) + path (12 chars) + " " (1) = char col 20.
        assert_eq!(surface.textarea.cursor(), (0, 20));
        assert!(!surface.suggestions_visible);
    }

    #[test]
    fn test_paste_then_keystroke_slash_multibyte_no_panic() {
        // Mirrors the real paste crash path: insert_pasted_text is enter_insert
        // + insert_str (no suggestion update), then the FIRST
        // keystroke runs update_suggestions → extract_slash_query. Pre-fix,
        // the char cursor col landed mid-`✨` as a byte index and panicked.
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        surface.insert_pasted_text("héllo wörld\n\n/dèploy ✨");

        let result = handle_key(&mut surface, key(KeyCode::Char('x')), &config);

        assert!(matches!(result, InputAction::Consumed));
        assert_eq!(surface.textarea.lines()[2], "/dèploy ✨x");
        // Suggestion state updated (Command trigger recognized; no commands
        // configured so nothing is visible, but the extractor ran cleanly).
        assert_eq!(surface.suggestion_mode, SuggestionMode::Command);
        assert!(!surface.suggestions_visible);
    }

    #[test]
    fn test_paste_then_keystroke_at_multibyte_no_panic() {
        // `@`-trigger variant: pre-fix, extract_at_query's
        // `&line[..end]` landed mid-`✨` and panicked on the first
        // keystroke after the paste.
        let mut surface = InputSurface::new_insert();
        let config = overlay_config();
        surface.insert_pasted_text("héllo wörld\n\nsée @fi✨");

        let result = handle_key(&mut surface, key(KeyCode::Char('x')), &config);

        assert!(matches!(result, InputAction::Consumed));
        assert_eq!(surface.textarea.lines()[2], "sée @fi✨x");
        // No working_dir configured, so the file trigger clears state — the
        // regression is that the extractor no longer panics getting here.
        assert_eq!(surface.suggestion_mode, SuggestionMode::None);
        assert!(!surface.suggestions_visible);
    }

    #[test]
    fn pasted_text_is_literal_and_enters_insert_mode() {
        let mut surface = InputSurface::default();
        let pasted = r#"q / @ $ ` \ " ' [] {}"#;

        surface.insert_pasted_text(pasted);

        assert_eq!(surface.mode, PopupMode::Insert);
        assert_eq!(surface.content(), pasted);
        assert_eq!(surface.vim_state.insert_start_snapshot.as_deref(), Some(""));
    }
}
