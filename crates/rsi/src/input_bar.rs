//! Input bar key handling for session detail view.
//!
//! Thin wrapper around [`InputSurface`](crate::input_surface) that:
//! - Intercepts caller-specific keys (grammar correction, clipboard paste)
//! - Delegates all editing to the shared `handle_key()` handler
//! - Routes `Submit`/`Close`/`Passthrough` actions back to the app

use crate::app::App;
use crate::input_surface::{self, InputAction, InputSurface, InputSurfaceConfig};
use crate::modalkit_types::InsertStyle;
use crate::types::{OverlayState, Pane, PopupMode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Handle a key event for the input bar.
/// Returns `true` if the key was consumed, `false` if it should be handled normally.
pub async fn handle_input_bar_key(app: &mut App, key: KeyEvent) -> bool {
    // Only handle keys when focused on a session detail pane
    let session_id = match app.focused_pane().cloned() {
        Some(Pane::SessionDetail { session_id }) => session_id,
        _ => return false,
    };

    // Get input bar state
    let Some(state) = app.sessions.get(&session_id) else {
        return false;
    };

    // Standard editing (#1628) has no Normal mode: the composer is always typing.
    let standard = app.standard_editing();
    let mode = if standard {
        PopupMode::Insert
    } else {
        state.input_bar.surface.mode
    };
    let vim_idle = state.input_bar.surface.vim_state.is_idle();

    // Ctrl+Enter submits from any mode (insert or normal)
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Enter {
        submit_input_bar(app, session_id).await;
        return true;
    }

    // Ctrl+Shift+A opens AI chat (Q&A about the text via local model).
    if key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
    {
        if app.prompt_processor.is_none() {
            app.notify("AI assistant requires prompt processor (check settings)");
            return true;
        }
        let source_text = {
            let Some(s) = app.sessions.get(&session_id) else {
                return true;
            };
            s.input_bar.surface.content_for_send()
        };
        if source_text.is_empty() {
            app.notify("No text to chat about");
            return true;
        }
        let source = crate::types::AiAssistantSource::InputBar(session_id);
        app.overlay = OverlayState::AiChat {
            messages: Vec::new(),
            input: String::new(),
            source_text,
            source,
            in_flight: false,
            scroll_offset: 0,
        };
        app.mark_dirty();
        return true;
    }

    // Ctrl+A opens AI command input (text transformation via local model).
    // In Standard mode Ctrl+A is select-all, so the AI command moves to
    // Ctrl+Alt+A (#1628).
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('a')
        && (!standard || key.modifiers.contains(KeyModifiers::ALT))
    {
        if app.prompt_processor.is_none() {
            app.notify("AI assistant requires prompt processor (check settings)");
            return true;
        }
        let source_text = {
            let Some(s) = app.sessions.get(&session_id) else {
                return true;
            };
            s.input_bar.surface.content_for_send()
        };
        if source_text.is_empty() {
            app.notify("No text to transform");
            return true;
        }
        let source = crate::types::AiAssistantSource::InputBar(session_id);
        app.overlay = OverlayState::AiCommand {
            command: String::new(),
            source_text,
            source,
            in_flight: false,
        };
        app.mark_dirty();
        return true;
    }

    // Ctrl+Y triggers prompt compilation (caller-specific — not in shared handler)
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
        // Guard: prevent double-trigger while in-flight
        if app
            .sessions
            .get(&session_id)
            .is_some_and(|s| s.input_bar.surface.correction_in_flight)
            || app.input_bar_compile_rx.is_some()
        {
            return true;
        }
        // Guard: require a configured processor
        if app.prompt_processor.is_none() {
            app.notify("No prompt processor configured (set settings.prompt_processor.enabled)");
            return true;
        }
        let input = {
            let Some(s) = app.sessions.get(&session_id) else {
                return true;
            };
            s.input_bar.surface.content_trimmed()
        };
        if input.is_empty() {
            return true;
        }
        if let Some(s) = app.sessions.get_mut(&session_id) {
            s.input_bar.surface.correction_in_flight = true;
        }
        let config = app.settings.prompt_processor.clone();
        let original_input = input.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.input_bar_compile_rx = Some((session_id, original_input, rx));
        tokio::spawn(async move {
            let result = match crate::prompt_processor::build_processor(&config) {
                Some(p) => p.compile(&input).await.map_err(|e| e.to_string()),
                None => Err("Processor disabled".to_string()),
            };
            let _ = tx.send(result);
        });
        app.mark_dirty();
        return true;
    }

    // Ctrl+Shift+G triggers grammar/spelling correction (no restructuring, just fix errors)
    if key
        .modifiers
        .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
    {
        // Guard: prevent double-trigger while in-flight
        if app
            .sessions
            .get(&session_id)
            .is_some_and(|s| s.input_bar.surface.correction_in_flight)
            || app.input_bar_compile_rx.is_some()
        {
            return true;
        }
        // Guard: require a configured processor
        if app.prompt_processor.is_none() {
            app.notify("No prompt processor configured (set settings.prompt_processor.enabled)");
            return true;
        }
        let input = {
            let Some(s) = app.sessions.get(&session_id) else {
                return true;
            };
            s.input_bar.surface.content_trimmed()
        };
        if input.is_empty() {
            return true;
        }
        if let Some(s) = app.sessions.get_mut(&session_id) {
            s.input_bar.surface.correction_in_flight = true;
        }
        let config = app.settings.prompt_processor.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.input_bar_compile_rx = Some((session_id, String::new(), rx));
        tokio::spawn(async move {
            let result = match crate::prompt_processor::build_processor(&config) {
                Some(p) => p
                    .send(crate::prompt_processor::GRAMMAR_SYSTEM_PROMPT, &input)
                    .await
                    .map(|text| crate::prompt_processor::CompileResult {
                        compiled: text,
                        contract: crate::prompt_processor::OutputContract::Complete,
                        layer_validation: crate::prompt_processor::LayerValidation {
                            semantic: true,
                            syntactic: true,
                            deictic: true,
                            discourse: true,
                            pragmatic: true,
                        },
                    })
                    .map_err(|e| e.to_string()),
                None => Err("Processor disabled".to_string()),
            };
            let _ = tx.send(result);
        });
        app.mark_dirty();
        return true;
    }

    // Ctrl+G opens the input modal overlay (quarter-size centered popup)
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
        open_input_modal(app, session_id);
        return true;
    }

    // Ctrl+V pastes from system clipboard (caller-specific — needs app context)
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
    {
        try_paste_input_bar(app);
        return true;
    }

    // Up/Down arrows always scroll the session detail message container,
    // never the input bar — regardless of insert/normal mode.
    // In Standard mode a multi-line draft moves its cursor first and the
    // surface hands the arrow back at the draft's first/last row.
    if !standard
        && key.modifiers == KeyModifiers::NONE
        && matches!(key.code, KeyCode::Up | KeyCode::Down)
    {
        return false;
    }

    // When the vim machine is mid-sequence (e.g., Space was just pressed and it's waiting
    // for the next key), bypass the input bar entirely so leader sequences like Space+g
    // reach the vim machine even when the input bar has content in normal mode.
    if app.vim_machine_pending && mode == PopupMode::Normal && vim_idle {
        return false;
    }

    // An idle Space starts a global leader sequence even with a draft (an idle
    // `g` does too, but only on an empty draft, which the surface passes
    // through; with a draft it is the surface's own `gg`). A pending surface
    // command owns its next key: f<Space>, fg, r<Space>, dgg, etc.
    if mode == PopupMode::Normal
        && vim_idle
        && key.modifiers == KeyModifiers::NONE
        && key.code == KeyCode::Char(' ')
    {
        return false;
    }

    if handle_session_surface_key(app, session_id, key).await {
        return true;
    }
    // Standard editing (#1628): Esc with no selection and no open completion
    // is handed back by the surface. There is no Normal mode to fall into, and
    // Ctrl-H needs a kitty-protocol terminal, so Esc is the plain-terminal way
    // back to the session list.
    if standard && key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        app.back_to_list();
        app.mark_dirty();
        return true;
    }
    false
}

/// Feed `key` to `session_id`'s input bar editor and act on the result:
/// submit sends a continue, close clears the draft. Returns `false` when the
/// caller should handle the key. Shared by the detail pane and the global
/// manager workspace's conversation pane (#1231), which targets a session
/// that is not the focused pane.
pub(crate) async fn handle_session_surface_key(
    app: &mut App,
    session_id: uuid::Uuid,
    key: KeyEvent,
) -> bool {
    // Clone working_dir before mutable borrow to avoid borrow conflicts.
    let working_dir = app
        .sessions
        .get(&session_id)
        .map(|s| s.session.working_dir.clone());

    // The surface follows the operator's live editing mode on every key.
    let standard = app.standard_editing();

    // Delegate to shared InputSurface handler
    let config = InputSurfaceConfig {
        pass_through_unhandled: true,
        available_commands: &app.available_commands,
        working_dir: working_dir.as_deref(),
        submit_on_enter: app.settings.submit_on_enter,
        standard_editing: standard,
    };

    // We need to extract the surface, call handle_key, then put it back.
    // This avoids borrow conflicts with app.
    let Some(state) = app.sessions.get_mut(&session_id) else {
        return false;
    };
    let action = input_surface::handle_key(&mut state.input_bar.surface, key, &config);

    match action {
        InputAction::Consumed => {
            app.mark_dirty();
            true
        }
        InputAction::Submit(content) => {
            submit_input_bar_with_content(app, session_id, &content).await;
            true
        }
        InputAction::Close => {
            // Clear input bar content
            if let Some(state) = app.sessions.get_mut(&session_id) {
                state.input_bar.surface.clear();
            }
            app.mark_dirty();
            true
        }
        // In normal mode with no content, or an unhandled vim key: the caller
        // handles it.
        InputAction::Passthrough(_) => false,
        InputAction::CompileDecision {
            accepted,
            context,
            compiled_output,
        } => {
            let params = rsi_common::rpc::SaveCompiledPromptParams {
                session_id: Some(session_id),
                original_input: context.original_input,
                compiled_output,
                contract_status: context.contract_status,
                layer_semantic: context.layer_semantic,
                layer_syntactic: context.layer_syntactic,
                layer_deictic: context.layer_deictic,
                layer_discourse: context.layer_discourse,
                layer_pragmatic: context.layer_pragmatic,
                accepted,
            };
            let _ = app.client.save_compiled_prompt(params).await;
            app.mark_dirty();
            true
        }
    }
}

/// Submit the input bar content as a continue_session query.
async fn submit_input_bar(app: &mut App, session_id: uuid::Uuid) {
    // Guard: prevent submission for archived (read-only) sessions
    if let Some(state) = app.sessions.get(&session_id)
        && state.session.status == rsi_common::types::SessionStatus::Archived
    {
        app.notify("Read-only: unarchive first (U)");
        return;
    }

    let query = {
        let Some(state) = app.sessions.get(&session_id) else {
            return;
        };
        state.input_bar.surface.content_for_send()
    };

    if query.is_empty() {
        return;
    }

    submit_input_bar_with_content(app, session_id, &query).await;
}

/// Submit with already-extracted content (called from shared handler's Submit action).
async fn submit_input_bar_with_content(app: &mut App, session_id: uuid::Uuid, query: &str) {
    if query.is_empty() {
        return;
    }

    // Guard: prevent submission for archived (read-only) sessions
    if let Some(state) = app.sessions.get(&session_id)
        && state.session.status == rsi_common::types::SessionStatus::Archived
    {
        app.notify("Read-only: unarchive first (U)");
        return;
    }

    // Snapshot the raw lines before clearing so a rejected continue can restore
    // exactly what was typed. `query` is `content_for_send` output, which has
    // already collapsed visual wraps into spaces.
    let typed: Vec<String> = app
        .sessions
        .get(&session_id)
        .map(|s| s.input_bar.surface.textarea.lines().to_vec())
        .unwrap_or_default();

    // Clear the textarea and return to normal mode
    if let Some(state) = app.sessions.get_mut(&session_id) {
        state.input_bar.surface.clear();
        // Anchor detail view to bottom so user sees the new prompt immediately
        state.follow_tail = true;
    }

    // Send the query. On rejection put the text back — the surface was cleared
    // above, so without this the user's prompt is destroyed by a transient
    // daemon-side teardown timeout.
    if !app.continue_session(session_id, query).await {
        app.restore_continue_lines(session_id, typed);
    }
}

/// Enter insert mode for a session's input bar with vim-style cursor positioning.
pub fn enter_insert_mode(app: &mut App, session_id: uuid::Uuid, style: InsertStyle) {
    if let Some(state) = app.sessions.get_mut(&session_id) {
        state.input_bar.surface.enter_insert(style);
    }
    // Track as last viewed session
    app.last_viewed_session = Some(session_id);
    app.push_jumplist(session_id);
}

/// Open the input modal overlay for longer-form input to the current session.
/// The modal is quarter-size, centered, and does NOT show CWD or model name.
/// Transfers any existing input bar text into the modal so they act as one surface.
fn open_input_modal(app: &mut App, session_id: uuid::Uuid) {
    // Transfer input bar content into the modal surface
    let mut surface = if let Some(state) = app.sessions.get_mut(&session_id) {
        let content = state.input_bar.surface.content_trimmed();
        // Clear the input bar since text is moving to the modal
        state.input_bar.surface.clear();
        if content.is_empty() {
            InputSurface::new_insert()
        } else {
            InputSurface::new_insert_with_content(content.lines().map(str::to_string).collect())
        }
    } else {
        InputSurface::new_insert()
    };
    surface.textarea.move_cursor(tui_textarea::CursorMove::Top);
    surface.textarea.move_cursor(tui_textarea::CursorMove::Head);
    app.overlay = OverlayState::InputModal {
        overlay_id: uuid::Uuid::new_v4(),
        surface,
        session_id,
    };
    app.mark_dirty();
}

/// Try to paste clipboard content into the input bar.
/// Automatically enters insert mode if the input bar is focused but in normal mode,
/// so the full paste always lands regardless of current mode.
/// Called from the event loop before the Press-only filter to handle terminals
/// (like Ghostty) that send Ctrl+V as a Release event.
pub fn try_paste_input_bar(app: &mut App) -> bool {
    let session_id = match input_bar_session_if_focused(app) {
        Some(id) => id,
        None => return false,
    };
    let needs_insert = app
        .sessions
        .get(&session_id)
        .map(|s| s.input_bar.surface.mode != PopupMode::Insert)
        .unwrap_or(false);
    if needs_insert {
        enter_insert_mode(app, session_id, InsertStyle::Insert);
    }
    paste_into_input_bar(app, session_id);
    true
}

/// Try to paste pre-read text into the input bar (for bracketed paste).
/// Automatically enters insert mode if the input bar is focused but in normal mode.
pub fn try_paste_text_input_bar(app: &mut App, text: &str) -> bool {
    let session_id = match input_bar_session_if_focused(app) {
        Some(id) => id,
        None => return false,
    };
    let needs_insert = app
        .sessions
        .get(&session_id)
        .map(|s| s.input_bar.surface.mode != PopupMode::Insert)
        .unwrap_or(false);
    if needs_insert {
        enter_insert_mode(app, session_id, InsertStyle::Insert);
    }
    paste_text_into_input_bar(app, session_id, text);
    true
}

/// Returns the focused session ID if the input bar pane is focused (regardless of mode).
fn input_bar_session_if_focused(app: &App) -> Option<uuid::Uuid> {
    let session_id = match app.focused_pane().cloned() {
        Some(Pane::SessionDetail { session_id }) => session_id,
        _ => return None,
    };
    app.sessions.get(&session_id)?;
    Some(session_id)
}

/// Paste clipboard content (image or text) into the input bar textarea.
fn paste_into_input_bar(app: &mut App, session_id: uuid::Uuid) {
    let paste_dir = app.paste_dir.clone();
    match crate::clipboard::read_clipboard(&paste_dir) {
        crate::clipboard::ClipboardContent::Image { reference, .. } => {
            if let Some(state) = app.sessions.get_mut(&session_id) {
                state.input_bar.surface.insert_pasted_text(&reference);
            }
            app.notify("Image pasted");
        }
        crate::clipboard::ClipboardContent::Text(text) => {
            paste_text_into_input_bar(app, session_id, &text);
        }
        crate::clipboard::ClipboardContent::Empty => {
            app.notify("Clipboard empty");
        }
    }
}

fn paste_text_into_input_bar(app: &mut App, session_id: uuid::Uuid, text: &str) {
    if let Some(state) = app.sessions.get_mut(&session_id) {
        let normalized = crate::input_surface::normalize_pasted_text(text);
        state.input_bar.surface.insert_pasted_text(&normalized);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::state::{DevState, PersistedState};
    use crate::types::{Pane, SessionState};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    fn test_app() -> App {
        DevState::clear();
        PersistedState::default().save();
        let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
        if let Some(crate::types::Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) = app.focused_pane_mut()
        {
            *selected_index = 0;
            *selected_session = None;
        }
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn ctrl_shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    }

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[tokio::test]
    async fn pending_vim_commands_capture_space_and_g_through_event_loop() {
        for (keys, start, expected, cursor) in [
            ("f ", 0, "abc def ghi", 3),
            ("t ", 0, "abc def ghi", 2),
            ("dt ", 0, " def ghi", 0),
            ("fg", 0, "abc def ghi", 8),
            ("r ", 0, " bc def ghi", 0),
            ("F ", 10, "abc def ghi", 7),
            ("T ", 10, "abc def ghi", 8),
            ("2f ", 0, "abc def ghi", 7),
            ("di ", 0, "abc def ghi", 0),
            ("dag", 0, "abc def ghi", 0),
        ] {
            let (mut app, id) = crate::app::app_test_helpers::with_session_detail();
            *app.focused_pane_mut().unwrap() = Pane::SessionDetail { session_id: id };
            let surface = &mut app.sessions.get_mut(&id).unwrap().input_bar.surface;
            surface.textarea.insert_str("abc def ghi");
            surface
                .textarea
                .move_cursor(tui_textarea::CursorMove::Jump(0, start));
            for ch in keys.chars() {
                crate::event::step_once(&mut app, key(KeyCode::Char(ch))).await;
            }
            let surface = &app.sessions[&id].input_bar.surface;
            assert_eq!(surface.content(), expected, "{keys:?}");
            assert_eq!(surface.textarea.cursor(), (0, cursor), "{keys:?}");
            assert!(surface.vim_state.is_idle(), "{keys:?}");
            assert!(!app.vim_machine_pending, "{keys:?}");
        }
    }

    #[tokio::test]
    async fn idle_g_with_a_draft_stays_the_surface_gg_prefix() {
        let (mut app, id) = crate::app::app_test_helpers::with_session_detail();
        *app.focused_pane_mut().unwrap() = Pane::SessionDetail { session_id: id };
        let surface = &mut app.sessions.get_mut(&id).unwrap().input_bar.surface;
        surface.textarea.insert_str("one\ntwo\nthree");
        for ch in "gg".chars() {
            crate::event::step_once(&mut app, key(KeyCode::Char(ch))).await;
        }
        let surface = &app.sessions[&id].input_bar.surface;
        assert_eq!(surface.textarea.cursor(), (0, 0));
        assert!(!app.vim_machine_pending);
        assert_eq!(app.vim_machine_prefix, None);
    }

    #[tokio::test]
    async fn pending_delete_captures_gg_in_input_bar() {
        let (mut app, id) = crate::app::app_test_helpers::with_session_detail();
        *app.focused_pane_mut().unwrap() = Pane::SessionDetail { session_id: id };
        let surface = &mut app.sessions.get_mut(&id).unwrap().input_bar.surface;
        surface.textarea.insert_str("one\ntwo\nthree");
        surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::Jump(1, 0));
        for ch in "dgg".chars() {
            assert!(handle_input_bar_key(&mut app, key(KeyCode::Char(ch))).await);
        }
        assert_eq!(app.sessions[&id].input_bar.surface.content(), "three");
    }

    #[tokio::test]
    async fn idle_input_bar_still_dispatches_space_and_g_leaders() {
        let (mut app, id) = crate::app::app_test_helpers::with_session_detail();
        *app.focused_pane_mut().unwrap() = Pane::SessionDetail { session_id: id };
        app.sessions
            .get_mut(&id)
            .unwrap()
            .input_bar
            .surface
            .textarea
            .insert_str("draft");
        crate::event::step_once(&mut app, key(KeyCode::Char(' '))).await;
        assert_eq!(app.vim_machine_prefix, Some(' '));
        crate::event::step_once(&mut app, key(KeyCode::Char(';'))).await;
        assert!(matches!(app.overlay, OverlayState::CommandPalette { .. }));
        assert_eq!(app.vim_machine_prefix, None);
        app.overlay = OverlayState::None;
        // `g` only reaches the global machine on an empty draft; with a draft
        // it is the surface's `gg` prefix.
        app.sessions.get_mut(&id).unwrap().input_bar.surface.clear();
        crate::event::step_once(&mut app, key(KeyCode::Char('g'))).await;
        assert_eq!(app.vim_machine_prefix, Some('g'));
        crate::event::step_once(&mut app, key(KeyCode::Char('j'))).await;
        assert!(matches!(
            app.focused_pane(),
            Some(Pane::SessionList {
                active_zone: crate::types::SessionListZone::Jobs,
                ..
            })
        ));
        assert_eq!(app.vim_machine_prefix, None);
    }

    /// Assert a Ctrl+Enter was handled as a *submit* whose continue was then
    /// rejected, and that the user's text survived the rejection.
    ///
    /// `test_app` points at a socket with no daemon behind it, so every submit
    /// in these tests takes the failure path. That makes this the regression
    /// guard for the teardown-timeout data loss: the submit path clears the
    /// surface before the RPC, so without restore-on-failure the typed prompt
    /// is destroyed by any rejected continue.
    ///
    /// Checking the notification alongside the content matters — content alone
    /// could not distinguish "submitted, rejected, restored" from "never
    /// submitted at all" (e.g. Ctrl+Enter silently regressing to a newline).
    fn assert_submitted_and_preserved(app: &App, id: uuid::Uuid, expected: &[&str]) {
        let state = app.sessions.get(&id).unwrap();
        assert_eq!(
            state.input_bar.surface.mode,
            PopupMode::Normal,
            "Ctrl+Enter must submit and leave normal mode"
        );
        assert!(
            !app.notifications.is_empty(),
            "a rejected continue must surface an error notification"
        );
        assert_eq!(
            state.input_bar.surface.textarea.lines(),
            expected,
            "a rejected continue must leave the user's typed prompt intact"
        );
    }

    /// Set up a session in detail view with insert mode active.
    fn setup_detail_view(app: &mut App) -> uuid::Uuid {
        let session = rsi_common::types::Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: rsi_common::types::SessionProvider::Claude,
            claude_session_id: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: rsi_common::types::SessionStatus::Completed,
            project_id: None,
            session_kind: rsi_common::types::SessionKind::Standard,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_prompt_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
            continued_from: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        };
        let id = session.id;
        app.session_order.push(id);
        app.sessions.insert(id, SessionState::new(session));

        // Switch focused pane to SessionDetail
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = Pane::SessionDetail { session_id: id };
        }

        id
    }

    #[tokio::test]
    async fn test_ctrl_enter_submits_from_insert_mode() {
        let mut app = test_app();
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        // Type some text
        handle_input_bar_key(&mut app, key(KeyCode::Char('h'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Char('i'))).await;

        // Submit with Ctrl+Enter
        handle_input_bar_key(&mut app, ctrl_key(KeyCode::Enter)).await;

        // Ctrl+Enter must be treated as submit (not a newline) and return to
        // normal mode. `test_app` points at a socket with no daemon behind it,
        // so the continue is REJECTED — which means the typed text must survive
        // rather than being silently eaten. See `assert_submitted_and_preserved`.
        assert_submitted_and_preserved(&app, id, &["hi"]);
    }

    #[tokio::test]
    async fn test_ctrl_enter_submits_from_normal_mode() {
        let mut app = test_app();
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        // Type text then go back to normal mode
        handle_input_bar_key(&mut app, key(KeyCode::Char('h'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Char('i'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Esc)).await;

        assert_eq!(
            app.sessions.get(&id).unwrap().input_bar.surface.mode,
            PopupMode::Normal
        );

        // Submit with Ctrl+Enter from normal mode
        handle_input_bar_key(&mut app, ctrl_key(KeyCode::Enter)).await;

        assert_submitted_and_preserved(&app, id, &["hi"]);
    }

    #[tokio::test]
    async fn test_plain_enter_submits_in_insert_mode_when_enabled() {
        let mut app = test_app();
        // The default configuration sends the session-detail input bar on Enter.
        assert!(app.settings.submit_on_enter);
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        handle_input_bar_key(&mut app, key(KeyCode::Char('h'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Char('i'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Enter)).await;

        assert_submitted_and_preserved(&app, id, &["hi"]);
    }

    #[tokio::test]
    async fn test_shift_enter_inserts_newline_in_insert_mode() {
        let mut app = test_app();
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        // Type "a", Shift+Enter, "b" — produces two lines, no submit.
        handle_input_bar_key(&mut app, key(KeyCode::Char('a'))).await;
        handle_input_bar_key(&mut app, shift_key(KeyCode::Enter)).await;
        handle_input_bar_key(&mut app, key(KeyCode::Char('b'))).await;

        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.input_bar.surface.mode, PopupMode::Insert);
        assert_eq!(state.input_bar.surface.textarea.lines(), &["a", "b"]);
    }

    #[tokio::test]
    async fn test_plain_enter_legacy_inserts_newline() {
        let mut app = test_app();
        // Turn the legacy escape hatch on — plain Enter should now insert a newline.
        app.settings.submit_on_enter = false;
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        handle_input_bar_key(&mut app, key(KeyCode::Char('a'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Enter)).await;
        handle_input_bar_key(&mut app, key(KeyCode::Char('b'))).await;

        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.input_bar.surface.mode, PopupMode::Insert);
        assert_eq!(state.input_bar.surface.textarea.lines(), &["a", "b"]);
    }

    /// Regression: continuing a session by typing a prompt intermittently
    /// failed (the daemon's interrupt-then-wait deadline sat below the teardown
    /// worst case), and the submit path cleared the input surface *before* the
    /// RPC — so the failure destroyed the user's prompt outright. Retrying via
    /// QuickContinue appeared to work only because it sends the literal
    /// "continue", silently dropping the real follow-up.
    ///
    /// Multi-line content is the interesting case: restoration rebuilds the
    /// surface from `str::lines()`, so a paragraph must come back with its line
    /// structure intact, not flattened into one line.
    #[tokio::test]
    async fn restores_multiline_prompt_on_failed_continue() {
        let mut app = test_app();
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        handle_input_bar_key(&mut app, key(KeyCode::Char('a'))).await;
        handle_input_bar_key(&mut app, shift_key(KeyCode::Enter)).await;
        handle_input_bar_key(&mut app, key(KeyCode::Char('b'))).await;
        handle_input_bar_key(&mut app, ctrl_key(KeyCode::Enter)).await;

        assert_submitted_and_preserved(&app, id, &["a", "b"]);
    }

    /// The restore is scoped to the session that failed — it must not leak the
    /// prompt into a different session's input bar.
    #[tokio::test]
    async fn restore_on_failed_continue_does_not_touch_other_sessions() {
        let mut app = test_app();
        let id = setup_detail_view(&mut app);
        let other = setup_detail_view(&mut app);
        assert_ne!(id, other);

        enter_insert_mode(&mut app, id, InsertStyle::Insert);
        handle_input_bar_key(&mut app, key(KeyCode::Char('h'))).await;
        handle_input_bar_key(&mut app, ctrl_key(KeyCode::Enter)).await;

        assert_eq!(
            app.sessions
                .get(&other)
                .unwrap()
                .input_bar
                .surface
                .textarea
                .lines(),
            &[""],
            "a failed continue must not write the prompt into another session"
        );
    }

    #[tokio::test]
    async fn test_ctrl_enter_legacy_still_submits() {
        let mut app = test_app();
        app.settings.submit_on_enter = false;
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        handle_input_bar_key(&mut app, key(KeyCode::Char('h'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Char('i'))).await;
        handle_input_bar_key(&mut app, ctrl_key(KeyCode::Enter)).await;

        assert_submitted_and_preserved(&app, id, &["hi"]);
    }

    #[tokio::test]
    async fn test_ctrl_enter_empty_does_not_submit() {
        let mut app = test_app();
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        // Submit empty content
        handle_input_bar_key(&mut app, ctrl_key(KeyCode::Enter)).await;

        // No notification (nothing was sent)
        assert!(app.notifications.is_empty());
    }

    #[tokio::test]
    async fn test_ctrl_y_triggers_prompt_compile_shortcut() {
        let mut app = test_app();
        app.prompt_processor = None;
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        handle_input_bar_key(&mut app, key(KeyCode::Char('h'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Char('i'))).await;
        handle_input_bar_key(&mut app, ctrl_key(KeyCode::Char('y'))).await;

        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("No prompt processor configured"))
        );
    }

    #[tokio::test]
    async fn ai_chat_uses_ctrl_shift_a_without_claiming_ctrl_b() {
        let mut app = test_app();
        app.prompt_processor = None;
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);
        handle_input_bar_key(&mut app, key(KeyCode::Char('x'))).await;

        handle_input_bar_key(&mut app, ctrl_key(KeyCode::Char('b'))).await;
        assert!(app.notifications.is_empty());

        handle_input_bar_key(&mut app, ctrl_shift_key(KeyCode::Char('a'))).await;
        assert!(
            app.notifications
                .iter()
                .any(|notification| notification.message.contains("AI assistant requires"))
        );
    }

    #[tokio::test]
    async fn test_ctrl_y_compile_reaches_normal_mode_input_bar_through_event_loop() {
        let mut app = test_app();
        app.prompt_processor = None;
        let id = setup_detail_view(&mut app);
        enter_insert_mode(&mut app, id, InsertStyle::Insert);

        handle_input_bar_key(&mut app, key(KeyCode::Char('h'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Char('i'))).await;
        handle_input_bar_key(&mut app, key(KeyCode::Esc)).await;

        assert_eq!(
            app.sessions.get(&id).unwrap().input_bar.surface.mode,
            PopupMode::Normal
        );

        crate::event::step_once(&mut app, ctrl_key(KeyCode::Char('y'))).await;

        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("No prompt processor configured"))
        );
    }
}
