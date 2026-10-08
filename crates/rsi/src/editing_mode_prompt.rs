//! First-start editing-mode prompt (Issue #1628).
//!
//! When the daemon's `editing_mode` setting is `unset` (and this is not an
//! existing install, see [`App::resolve_editing_mode_on_start`]) a modal
//! appears before anything else and must be answered: **Standard** on the
//! left (recommended unless the operator is experienced with Vim), **Vim** on
//! the right (the mode RSI was built around). The modal is deliberately not an
//! `OverlayState`: it owns every key and click until a choice is made, and has
//! no dismissal. The only key that escapes it is the global quit chord.
//!
//! Buttons are chosen by mouse click, by Left/Right/Tab then Enter, or
//! directly with `s` / `v`.

use crate::app::App;
use crate::settings::DaemonFeatureEntry;
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use rsi_common::editing_mode::{EDITING_MODE_FIELD, EditingMode};

/// Prompt state: the highlighted button and where the last frame drew them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditingModePrompt {
    pub selected: EditingMode,
    /// Button hit areas from the last render, `[Standard, Vim]`.
    pub buttons: [Rect; 2],
}

impl Default for EditingModePrompt {
    fn default() -> Self {
        Self {
            selected: EditingMode::Standard,
            buttons: [Rect::default(); 2],
        }
    }
}

/// What a key or click did to the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOutcome {
    /// Consumed; the selection may have moved.
    Consumed,
    /// The operator chose a mode.
    Choose(EditingMode),
}

impl EditingModePrompt {
    fn toggle(&mut self) {
        self.selected = match self.selected {
            EditingMode::Standard => EditingMode::Vim,
            EditingMode::Vim => EditingMode::Standard,
        };
    }

    /// Apply one key press. Every key is consumed (there is no dismissal).
    pub fn on_key(&mut self, key: KeyEvent) -> PromptOutcome {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Left => {
                self.selected = EditingMode::Standard;
                PromptOutcome::Consumed
            }
            KeyCode::Right => {
                self.selected = EditingMode::Vim;
                PromptOutcome::Consumed
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle();
                PromptOutcome::Consumed
            }
            KeyCode::Enter => PromptOutcome::Choose(self.selected),
            KeyCode::Char('s' | 'S') if plain => PromptOutcome::Choose(EditingMode::Standard),
            KeyCode::Char('v' | 'V') if plain => PromptOutcome::Choose(EditingMode::Vim),
            _ => PromptOutcome::Consumed,
        }
    }

    /// Apply a left click at a terminal cell.
    pub fn on_click(&mut self, col: u16, row: u16) -> PromptOutcome {
        for (mode, rect) in EditingMode::ALL.into_iter().zip(self.buttons) {
            if rect_contains(rect, col, row) {
                self.selected = mode;
                return PromptOutcome::Choose(mode);
            }
        }
        PromptOutcome::Consumed
    }
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Key entry point (the caller has already filtered to key presses and let
/// the quit chord through). Always consumes the key.
pub async fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(prompt) = app.editing_mode_prompt.as_mut() else {
        return;
    };
    if let PromptOutcome::Choose(mode) = prompt.on_key(key) {
        commit(app, mode).await;
    }
    app.mark_dirty();
}

/// Left-click entry point. Returns whether the prompt owned the click.
pub async fn handle_click(app: &mut App, col: u16, row: u16) -> bool {
    let Some(prompt) = app.editing_mode_prompt.as_mut() else {
        return false;
    };
    if let PromptOutcome::Choose(mode) = prompt.on_click(col, row) {
        commit(app, mode).await;
    }
    app.mark_dirty();
    true
}

/// Run the first-start rule and, for a detected existing install, write the
/// Vim default. Called after every bootstrap event; a no-op once resolved.
pub async fn settle_on_start(app: &mut App) {
    app.resolve_editing_mode_on_start();
    if let Some(mode) = app.pending_editing_mode_default.take()
        && let Err(error) = write_mode(app, mode).await
    {
        tracing::warn!(%error, "could not keep Vim for an existing install");
    }
}

/// Persist the choice through the operator-only `UpdateDaemonConfig` RPC and
/// close the prompt. A failed write keeps the prompt open.
pub async fn commit(app: &mut App, mode: EditingMode) {
    match write_mode(app, mode).await {
        Ok(()) => app.editing_mode_prompt = None,
        Err(error) => app.notify_error(format!("Could not save editing mode: {error}")),
    }
}

/// Write `editing_mode` to the daemon and mirror it into the local cache.
pub async fn write_mode(app: &mut App, mode: EditingMode) -> Result<(), String> {
    let slug = mode.slug();
    app.client
        .update_daemon_config(EDITING_MODE_FIELD, serde_json::json!(slug))
        .await
        .map_err(|error| error.to_string())?;
    DaemonFeatureEntry::update_from_json(
        &mut app.daemon_features,
        &serde_json::json!({ EDITING_MODE_FIELD: slug }),
    );
    app.mark_dirty();
    Ok(())
}

/// Centered popup rect, clamped to the frame.
fn popup_rect(area: Rect) -> Rect {
    let width = 66.min(area.width);
    let height = 13.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

/// Draw the prompt over the whole frame and record the button hit areas.
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(mut prompt) = app.editing_mode_prompt else {
        return;
    };
    let popup = popup_rect(area);
    frame.render_widget(Clear, popup);
    let block = theme::overlay_block().title(Line::from(Span::styled(
        " Choose how you edit text ",
        Style::default()
            .fg(theme::overlay_title())
            .add_modifier(Modifier::BOLD),
    )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 7 || inner.width < 20 {
        return;
    }

    let intro = Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), 2);
    frame.render_widget(
        Paragraph::new("Pick one to start. You can change it any time in Settings > Editing Mode.")
            .style(Style::default().fg(theme::text()))
            .wrap(Wrap { trim: true }),
        intro,
    );

    let buttons_top = inner.y + 2;
    let buttons_height = inner.height.saturating_sub(3).min(7);
    let half = inner.width / 2;
    let rects = [
        Rect::new(inner.x, buttons_top, half, buttons_height),
        Rect::new(
            inner.x + half,
            buttons_top,
            inner.width - half,
            buttons_height,
        ),
    ];
    let copy: [&[&str]; 2] = [
        &[
            "Recommended if you are not",
            "experienced with Vim.",
            "",
            "Type directly; arrows and",
            "the mouse move the cursor.",
        ],
        &[
            "The mode RSI was built",
            "around.",
            "",
            "Modal editing: normal,",
            "insert and visual modes.",
        ],
    ];
    for ((mode, rect), text) in EditingMode::ALL.into_iter().zip(rects).zip(copy) {
        let selected = prompt.selected == mode;
        let border = if selected {
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::overlay_border())
        };
        let title = format!(
            " {}{} ",
            if selected { "> " } else { "" },
            match mode {
                EditingMode::Standard => "[s] Standard",
                EditingMode::Vim => "[v] Vim",
            }
        );
        let button = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .style(Style::default().bg(theme::overlay_bg()))
            .title(Line::from(Span::styled(
                title,
                if selected {
                    border.add_modifier(Modifier::REVERSED)
                } else {
                    border
                },
            )));
        let body = button.inner(rect);
        frame.render_widget(button, rect);
        frame.render_widget(
            Paragraph::new(
                text.iter()
                    .map(|line| Line::from(*line))
                    .collect::<Vec<_>>(),
            )
            .style(Style::default().fg(theme::text())),
            body,
        );
    }
    prompt.buttons = rects;
    app.editing_mode_prompt = Some(prompt);

    let hint_y = inner.y + inner.height - 1;
    frame.render_widget(
        Paragraph::new("Click, or: Left/Right/Tab then Enter  |  s Standard  |  v Vim")
            .style(Style::default().fg(theme::overlay_hint())),
        Rect::new(inner.x + 1, hint_y, inner.width.saturating_sub(2), 1),
    );
}

impl App {
    /// The daemon-owned editing mode, `None` while it is unset (or before the
    /// first authoritative config).
    #[must_use]
    pub fn editing_mode(&self) -> Option<EditingMode> {
        self.daemon_features
            .iter()
            .find(|entry| entry.field == EDITING_MODE_FIELD)
            .and_then(|entry| match &entry.value {
                crate::settings::DaemonFeatureValue::Cycle { options, current } => options
                    .get(*current)
                    .and_then(|v| EditingMode::from_slug(v)),
                _ => None,
            })
    }

    /// Whether text inputs use Standard (non-modal) editing. Vim is the
    /// behaviour for `vim` and while the choice is still unanswered.
    #[must_use]
    pub fn standard_editing(&self) -> bool {
        self.editing_mode() == Some(EditingMode::Standard)
    }

    /// Whether the focused composer is a Standard-mode text box, ready to
    /// take the editing chords that would otherwise be global (#1628). Used
    /// to let those chords through to the composer before the global table.
    #[must_use]
    pub fn standard_composer_claims(&self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        if !self.standard_editing() {
            return false;
        }
        // A selection in a focused form or search field is copied, not quit.
        let question_selection = matches!(
            &self.overlay,
            crate::types::OverlayState::QuestionModal { textarea, .. }
                if textarea.selection_range().is_some_and(|(start, end)| start != end)
        );
        if self.any_overlay_active()
            && (self.field_edit.has_selection() || question_selection)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
            && key.modifiers == KeyModifiers::CONTROL
        {
            return true;
        }
        // The issue editor's text field takes the same chords: a selection
        // is copied (not quit) and Ctrl-Left/Right move by word.
        if self.issue_editor_open() {
            let ctrl_only = key.modifiers == KeyModifiers::CONTROL;
            match key.code {
                KeyCode::Char('c' | 'C') if ctrl_only && self.field_edit.has_selection() => {
                    return true;
                }
                KeyCode::Left | KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return true;
                }
                _ => {}
            }
        }
        let Some(surface) = self.standard_claim_surface() else {
            return false;
        };
        let has_draft = surface.has_content();
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let shift = m.contains(KeyModifiers::SHIFT);
        match key.code {
            // Ctrl-C copies a selection; with none it keeps its global meaning.
            KeyCode::Char('c' | 'C') if m == KeyModifiers::CONTROL => {
                crate::input_surface::has_selection(surface)
            }
            // Word moves and word selection beat the sidebar/column/pane
            // chords; Ctrl-H / Ctrl-L still move pane focus.
            KeyCode::Left | KeyCode::Right if ctrl && !shift => true,
            // Selecting text beats the sidebar resize and event stepping, but
            // only when there is a draft to select in.
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if shift => has_draft,
            _ => false,
        }
    }

    /// Standard-mode editing for the focused single-line field of the
    /// current overlay. `pick` returns that field's text; a result other
    /// than `Ignored` means the key was taken and the overlay needs a
    /// redraw. Vim mode returns `Ignored`, so the owner's existing handling
    /// runs unchanged.
    pub fn edit_field(
        &mut self,
        key: crossterm::event::KeyEvent,
        pick: impl FnOnce(&mut crate::types::OverlayState) -> Option<&mut String>,
    ) -> crate::field_edit::FieldKey {
        self.edit_field_filtered(key, pick, |_| true)
    }

    /// [`edit_field`](Self::edit_field) for fields that only accept some
    /// characters (numeric fields).
    pub fn edit_field_filtered(
        &mut self,
        key: crossterm::event::KeyEvent,
        pick: impl FnOnce(&mut crate::types::OverlayState) -> Option<&mut String>,
        accept: impl Fn(char) -> bool,
    ) -> crate::field_edit::FieldKey {
        self.edit_field_capped(key, pick, accept, usize::MAX)
    }

    /// [`edit_field_filtered`](Self::edit_field_filtered) for fields with a
    /// length limit in characters.
    pub fn edit_field_capped(
        &mut self,
        key: crossterm::event::KeyEvent,
        pick: impl FnOnce(&mut crate::types::OverlayState) -> Option<&mut String>,
        accept: impl Fn(char) -> bool,
        max_chars: usize,
    ) -> crate::field_edit::FieldKey {
        use crate::field_edit::FieldKey;
        if !self.standard_editing() {
            return FieldKey::Ignored;
        }
        let Self {
            overlay,
            field_edit,
            ..
        } = self;
        let result = match pick(overlay) {
            Some(text) => field_edit.handle_key_capped(text, key, accept, max_chars),
            None => FieldKey::Ignored,
        };
        if result != FieldKey::Ignored {
            self.mark_dirty();
        }
        result
    }

    /// Point every overlay, file-viewer and prompt-editor text surface at the
    /// operator's live editing mode (#1628). Called once per frame so a
    /// settings change reaches surfaces that have not seen a key yet; Standard
    /// also drops any half-typed vim command and leaves Normal mode.
    pub fn sync_standard_surfaces(&mut self) {
        use crate::types::OverlayState;
        fn sync(surface: &mut crate::input_surface::InputSurface, standard: bool) {
            if standard {
                crate::input_surface::enter_standard(surface);
            } else {
                surface.standard_editing = false;
            }
        }
        fn sync_overlay(overlay: &mut OverlayState, standard: bool) {
            match overlay {
                OverlayState::Prompt { surface, .. } | OverlayState::InputModal { surface, .. } => {
                    sync(surface, standard);
                }
                OverlayState::QuestionModal { mode, .. } => {
                    // No Normal mode: the free-text box is always live.
                    if standard {
                        *mode = crate::types::PopupMode::Insert;
                    }
                }
                OverlayState::CreateEntityForm {
                    body,
                    focused_field,
                    insert_mode,
                    ..
                } => {
                    sync(body, standard);
                    // No insert/normal split: Name and Tag are typed into on focus.
                    if standard
                        && matches!(
                            focused_field,
                            crate::types::CreateEntityField::Name
                                | crate::types::CreateEntityField::Tag
                        )
                    {
                        *insert_mode = true;
                    }
                }
                OverlayState::ProviderForm {
                    name,
                    base_url,
                    api_key,
                    default_model,
                    ..
                } => {
                    for surface in [name, base_url, api_key, default_model] {
                        sync(surface, standard);
                    }
                }
                _ => {}
            }
        }
        let standard = self.standard_editing();
        self.field_edit.follow_overlay(&self.overlay);
        sync_overlay(&mut self.overlay, standard);
        for overlay in &mut self.input_overlays {
            sync_overlay(overlay, standard);
        }
        if let Some(viewer) = self.prompt_creator_viewer.as_mut() {
            sync(&mut viewer.surface, standard);
        }
        for state in self.sessions.values_mut() {
            if let Some(viewer) = state.file_viewer.as_mut() {
                sync(&mut viewer.surface, standard);
            }
        }
    }

    /// The text surface that currently owns typing, if any: the focused
    /// overlay's editor, the visible file viewer, or the session composer.
    fn standard_claim_surface(&self) -> Option<&crate::input_surface::InputSurface> {
        use crate::types::OverlayState;
        if self.any_overlay_active() {
            return match self.focused_input_overlay().unwrap_or(&self.overlay) {
                OverlayState::Prompt { surface, .. } | OverlayState::InputModal { surface, .. } => {
                    Some(surface)
                }
                OverlayState::CreateEntityForm {
                    focused_field: crate::types::CreateEntityField::Body,
                    body,
                    ..
                } => Some(body),
                overlay @ OverlayState::ProviderForm { .. } => {
                    crate::overlay::provider_form::focused_surface(overlay)
                }
                _ => None,
            };
        }
        if self.input_mode != crate::types::InputMode::Normal {
            return None;
        }
        if let Some(session_id) = crate::file_viewer::active_viewer_session(self) {
            return self
                .sessions
                .get(&session_id)
                .and_then(|s| s.file_viewer.as_ref())
                .map(|viewer| &viewer.surface);
        }
        let Some(crate::types::Pane::SessionDetail { session_id }) = self.focused_pane() else {
            return None;
        };
        self.sessions.get(session_id).map(|s| &s.input_bar.surface)
    }

    /// Resolve the first-start state once both the authoritative config and
    /// the first session snapshot have arrived.
    ///
    /// Rule (Issue #1628 AC4): if `editing_mode` is `unset` and the daemon
    /// already has sessions, this install predates the setting and has only
    /// ever had Vim editing, so Vim is kept silently. If it is `unset` and
    /// there are no sessions, the install is new: show the required prompt.
    /// Sessions are the signal, not `state.json`, because the TUI writes
    /// `state.json` on its first launch, before the operator can answer.
    pub(crate) fn resolve_editing_mode_on_start(&mut self) {
        if self.editing_mode_resolved || !self.authoritative_config_ready() {
            return;
        }
        if !matches!(
            self.bootstrap.sessions,
            crate::app::bootstrap::BootstrapComponentState::Ready
        ) {
            return;
        }
        self.editing_mode_resolved = true;
        if self.editing_mode().is_some() {
            return;
        }
        if self.sessions.is_empty() {
            self.editing_mode_prompt = Some(EditingModePrompt::default());
        } else {
            self.pending_editing_mode_default = Some(EditingMode::Vim);
        }
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn standard_is_left_and_selected_first() {
        let prompt = EditingModePrompt::default();
        assert_eq!(prompt.selected, EditingMode::Standard);
        assert_eq!(EditingMode::ALL[0], EditingMode::Standard);
        assert_eq!(EditingMode::ALL[1], EditingMode::Vim);
    }

    #[test]
    fn arrows_and_tab_move_then_enter_chooses() {
        let mut prompt = EditingModePrompt::default();
        assert_eq!(prompt.on_key(key(KeyCode::Right)), PromptOutcome::Consumed);
        assert_eq!(prompt.selected, EditingMode::Vim);
        assert_eq!(
            prompt.on_key(key(KeyCode::Enter)),
            PromptOutcome::Choose(EditingMode::Vim)
        );
        prompt.on_key(key(KeyCode::Left));
        assert_eq!(prompt.selected, EditingMode::Standard);
        prompt.on_key(key(KeyCode::Tab));
        assert_eq!(prompt.selected, EditingMode::Vim);
        prompt.on_key(key(KeyCode::Tab));
        assert_eq!(
            prompt.on_key(key(KeyCode::Enter)),
            PromptOutcome::Choose(EditingMode::Standard)
        );
    }

    #[test]
    fn s_and_v_choose_directly() {
        let mut prompt = EditingModePrompt::default();
        assert_eq!(
            prompt.on_key(key(KeyCode::Char('v'))),
            PromptOutcome::Choose(EditingMode::Vim)
        );
        assert_eq!(
            prompt.on_key(key(KeyCode::Char('S'))),
            PromptOutcome::Choose(EditingMode::Standard)
        );
    }

    #[test]
    fn escape_and_other_keys_never_dismiss() {
        let mut prompt = EditingModePrompt::default();
        for code in [
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Char('x'),
            KeyCode::Backspace,
        ] {
            assert_eq!(prompt.on_key(key(code)), PromptOutcome::Consumed);
        }
        assert_eq!(prompt.selected, EditingMode::Standard);
    }

    use crate::app::bootstrap::BootstrapComponentState;
    use crate::client::DaemonClient;

    fn ready_app(session_count: usize) -> App {
        let mut app = crate::app::app_test_helpers::with_session_list(session_count);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.bootstrap.sessions = BootstrapComponentState::Ready;
        app
    }

    /// Accepts one connection, answers `count` `UpdateDaemonConfig` calls with
    /// ok and returns the captured `(field, value)` pairs.
    async fn fake_daemon(
        listener: tokio::net::UnixListener,
        count: usize,
    ) -> Vec<(String, serde_json::Value)> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let Ok((stream, _)) = listener.accept().await else {
            panic!("client never connected");
        };
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let mut captured = Vec::new();
        while captured.len() < count {
            let Ok(Some(line)) = lines.next_line().await else {
                break;
            };
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                panic!("bad request: {line}");
            };
            captured.push((
                request["params"]["field"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                request["params"]["value"].clone(),
            ));
            let response = serde_json::json!({
                "jsonrpc": "2.0", "id": request["id"], "result": {"ok": true}
            });
            let Ok(()) = write.write_all(format!("{response}\n").as_bytes()).await else {
                panic!("write failed");
            };
        }
        captured
    }

    async fn connect(
        app: &mut App,
        count: usize,
    ) -> (
        tempfile::TempDir,
        tokio::task::JoinHandle<Vec<(String, serde_json::Value)>>,
    ) {
        let Ok(dir) = tempfile::tempdir() else {
            panic!("tempdir");
        };
        let socket = dir.path().join("daemon.sock");
        let Ok(listener) = tokio::net::UnixListener::bind(&socket) else {
            panic!("bind");
        };
        let server = tokio::spawn(fake_daemon(listener, count));
        app.client = DaemonClient::new(socket);
        let Ok(()) = app.client.connect().await else {
            panic!("connect");
        };
        (dir, server)
    }

    #[test]
    fn new_install_with_no_sessions_gets_the_prompt() {
        let mut app = ready_app(0);
        app.resolve_editing_mode_on_start();
        assert_eq!(app.editing_mode_prompt, Some(EditingModePrompt::default()));
        assert_eq!(app.pending_editing_mode_default, None);
    }

    #[test]
    fn existing_install_with_sessions_keeps_vim_without_a_prompt() {
        let mut app = ready_app(2);
        app.resolve_editing_mode_on_start();
        assert_eq!(app.editing_mode_prompt, None);
        assert_eq!(app.pending_editing_mode_default, Some(EditingMode::Vim));
    }

    #[test]
    fn a_stored_choice_is_never_prompted_again() {
        let mut app = ready_app(0);
        DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({ "editing_mode": "standard" }),
        );
        assert_eq!(app.editing_mode(), Some(EditingMode::Standard));
        app.resolve_editing_mode_on_start();
        assert_eq!(app.editing_mode_prompt, None);
        assert_eq!(app.pending_editing_mode_default, None);
    }

    #[test]
    fn the_rule_waits_for_config_and_sessions() {
        let mut app = ready_app(0);
        app.bootstrap.sessions = BootstrapComponentState::Loading;
        app.resolve_editing_mode_on_start();
        assert_eq!(app.editing_mode_prompt, None);
        assert!(!app.editing_mode_resolved);
        app.bootstrap.sessions = BootstrapComponentState::Ready;
        app.poll.authoritative_config_ready = false;
        app.resolve_editing_mode_on_start();
        assert_eq!(app.editing_mode_prompt, None);
        assert!(!app.editing_mode_resolved);
    }

    #[tokio::test]
    async fn settle_writes_vim_for_an_existing_install() {
        let mut app = ready_app(1);
        let (_dir, server) = connect(&mut app, 1).await;
        settle_on_start(&mut app).await;
        let Ok(captured) = server.await else {
            panic!("server task");
        };
        assert_eq!(
            captured,
            vec![("editing_mode".to_string(), serde_json::json!("vim"))]
        );
        assert_eq!(app.editing_mode(), Some(EditingMode::Vim));
        assert_eq!(app.editing_mode_prompt, None);
    }

    #[tokio::test]
    async fn pressing_v_in_the_event_loop_persists_vim_and_closes_the_prompt() {
        let mut app = ready_app(0);
        app.editing_mode_prompt = Some(EditingModePrompt::default());
        let (_dir, server) = connect(&mut app, 1).await;
        crate::event::step_once(&mut app, key(KeyCode::Char('v'))).await;
        let Ok(captured) = server.await else {
            panic!("server task");
        };
        assert_eq!(
            captured,
            vec![("editing_mode".to_string(), serde_json::json!("vim"))]
        );
        assert_eq!(app.editing_mode(), Some(EditingMode::Vim));
        assert_eq!(app.editing_mode_prompt, None);
    }

    #[tokio::test]
    async fn a_failed_write_keeps_the_prompt_open() {
        let mut app = ready_app(0);
        app.editing_mode_prompt = Some(EditingModePrompt::default());
        // No daemon connected: the RPC fails.
        crate::event::step_once(&mut app, key(KeyCode::Char('s'))).await;
        assert!(app.editing_mode_prompt.is_some());
        assert_eq!(app.editing_mode(), None);
    }

    #[tokio::test]
    async fn keys_do_not_reach_the_rest_of_the_app_while_the_prompt_is_open() {
        let mut app = ready_app(0);
        app.editing_mode_prompt = Some(EditingModePrompt::default());
        crate::event::step_once(&mut app, key(KeyCode::Esc)).await;
        crate::event::step_once(&mut app, key(KeyCode::Char('q'))).await;
        assert!(app.editing_mode_prompt.is_some());
        assert!(!app.quit);
    }

    #[tokio::test]
    async fn clicking_a_rendered_button_persists_that_mode() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = ready_app(0);
        app.editing_mode_prompt = Some(EditingModePrompt::default());
        let Ok(mut terminal) = Terminal::new(TestBackend::new(100, 30)) else {
            panic!("terminal");
        };
        let Ok(_) = terminal.draw(|frame| render(frame, frame.area(), &mut app)) else {
            panic!("draw");
        };
        let buttons = app
            .editing_mode_prompt
            .map(|p| p.buttons)
            .unwrap_or_default();
        assert!(
            buttons[0].x < buttons[1].x,
            "Standard is the left button, Vim the right"
        );
        let (_dir, server) = connect(&mut app, 1).await;
        let vim = buttons[1];
        assert!(handle_click(&mut app, vim.x + 2, vim.y + 1).await);
        let Ok(captured) = server.await else {
            panic!("server task");
        };
        assert_eq!(
            captured,
            vec![("editing_mode".to_string(), serde_json::json!("vim"))]
        );
        assert_eq!(app.editing_mode_prompt, None);
    }

    #[test]
    fn the_prompt_labels_standard_as_recommended_and_vim_as_the_native_mode() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = ready_app(0);
        app.editing_mode_prompt = Some(EditingModePrompt::default());
        let Ok(mut terminal) = Terminal::new(TestBackend::new(100, 30)) else {
            panic!("terminal");
        };
        let Ok(done) = terminal.draw(|frame| render(frame, frame.area(), &mut app)) else {
            panic!("draw");
        };
        let rows: Vec<String> = (0..done.buffer.area.height)
            .map(|y| {
                (0..done.buffer.area.width)
                    .map(|x| done.buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let text = rows.join("\n");
        assert!(text.contains("Recommended if you are not"));
        assert!(text.contains("experienced with Vim."));
        assert!(text.contains("The mode RSI was built"));
        let title_row = rows
            .iter()
            .find(|row| row.contains("Standard") && row.contains("Vim"))
            .map(String::as_str)
            .unwrap_or_default();
        assert!(title_row.find("Standard") < title_row.find("Vim"));
    }

    /// The settings surface: the Editing Mode row cycles the daemon field live.
    #[tokio::test]
    async fn the_settings_row_cycles_editing_mode_through_the_daemon() {
        let mut app = ready_app(0);
        let (_dir, server) = connect(&mut app, 2).await;
        let Some(index) = app
            .daemon_features
            .iter()
            .position(|entry| entry.field == "editing_mode")
        else {
            panic!("editing_mode has a daemon_features entry");
        };
        crate::action_handler::daemon_config::toggle_daemon_feature(&mut app, index).await;
        assert_eq!(app.editing_mode(), Some(EditingMode::Standard));
        crate::action_handler::daemon_config::toggle_daemon_feature(&mut app, index).await;
        assert_eq!(app.editing_mode(), Some(EditingMode::Vim));
        let Ok(captured) = server.await else {
            panic!("server task");
        };
        assert_eq!(
            captured,
            vec![
                ("editing_mode".to_string(), serde_json::json!("standard")),
                ("editing_mode".to_string(), serde_json::json!("vim")),
            ]
        );
    }

    #[test]
    fn click_on_a_button_chooses_it() {
        let mut prompt = EditingModePrompt::default();
        prompt.buttons = [Rect::new(0, 5, 10, 4), Rect::new(10, 5, 10, 4)];
        assert_eq!(
            prompt.on_click(12, 6),
            PromptOutcome::Choose(EditingMode::Vim)
        );
        assert_eq!(
            prompt.on_click(2, 8),
            PromptOutcome::Choose(EditingMode::Standard)
        );
        assert_eq!(prompt.on_click(30, 6), PromptOutcome::Consumed);
    }
}
