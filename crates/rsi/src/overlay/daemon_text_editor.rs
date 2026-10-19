//! Single-field text editor overlay for daemon string settings (#2393; the
//! cold-storage archive directory was the first, #2384 / #2438).
//!
//! The settings page has no inline text field, so a row of a free-text daemon
//! field opens this overlay (the pattern of the text-area background editor).
//! Enter writes the value through `UpdateDaemonConfig`; the daemon is the
//! authority on what is acceptable (an absolute path outside every sandbox root,
//! an http(s) URL), and its refusal is shown here with the draft kept. An empty
//! value selects the field's default.
//!
//! A new text field is one [`DaemonTextField`] entry in [`DAEMON_TEXT_FIELDS`]
//! plus a `Display` feature entry and an `Action` registry row.

use crate::app::App;
use crate::settings::DaemonFeatureValue;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

/// One daemon string field the editor can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonTextField {
    /// The `UpdateDaemonConfig` field name.
    pub field: &'static str,
    /// Overlay title and the subject of the save notice.
    pub title: &'static str,
    /// The editor's label line, before the "empty = default" part.
    pub prompt: &'static str,
    /// The key-hint prefix, e.g. "type a path".
    pub typing_hint: &'static str,
    /// What the page (and the editor) shows for an empty value.
    pub default_label: &'static str,
    /// The page's one-line action description.
    pub row_action: &'static str,
}

/// The daemon field of the cold-storage archive directory.
pub const COLD_ARCHIVE_DIR_FIELD: &str = "cold_archive_dir";

/// The daemon field of the SearXNG base URL (#2315).
pub const SEARXNG_URL_FIELD: &str = "harness_searxng_url";

/// The daemon field of the operator provider launch overrides (hub #2765).
pub const PROVIDER_LAUNCH_OVERRIDES_FIELD: &str = rsi_common::provider_launch_overrides::FIELD;

/// The daemon field of the Claude subagent model (#2763).
pub const CLAUDE_SUBAGENT_MODEL_FIELD: &str = "claude_subagent_model";

/// The daemon field of the Codex subagent model (#2763).
pub const CODEX_SUBAGENT_MODEL_FIELD: &str = "codex_subagent_model";

/// The daemon field of the Claude `--fallback-model` chain (#2762).
pub const CLAUDE_FALLBACK_MODELS_FIELD: &str = "claude_fallback_models";

/// The daemon field of the Claude `--max-budget-usd` cap (#2762).
pub const CLAUDE_MAX_BUDGET_USD_FIELD: &str = "claude_max_budget_usd";

/// The fields edited as free text. Others (the `*_base_url` fields) can adopt
/// the editor by adding an entry here.
pub const DAEMON_TEXT_FIELDS: &[DaemonTextField] = &[
    DaemonTextField {
        field: COLD_ARCHIVE_DIR_FIELD,
        title: "Cold-storage directory",
        prompt: "Absolute path for archive files",
        typing_hint: "type a path",
        default_label: "default (<data dir>/archive)",
        row_action: "edit the archive directory",
    },
    DaemonTextField {
        field: SEARXNG_URL_FIELD,
        title: "Harness SearXNG URL",
        prompt: "Base URL of the SearXNG instance, e.g. http://127.0.0.1:8888",
        typing_hint: "type a URL",
        default_label: "default (unset)",
        row_action: "edit the SearXNG URL",
    },
    DaemonTextField {
        field: PROVIDER_LAUNCH_OVERRIDES_FIELD,
        title: "Provider launch overrides",
        prompt: "JSON, e.g. {\"codex\":[\"model_reasoning_summary=concise\"]}",
        typing_hint: "type JSON",
        default_label: "default (none)",
        row_action: "edit the provider launch overrides",
    },
    DaemonTextField {
        field: CLAUDE_SUBAGENT_MODEL_FIELD,
        title: "Claude subagent model",
        prompt: "Model for Claude subagents, e.g. haiku or a full model id",
        typing_hint: "type a model",
        default_label: "default (subagents inherit the parent's model)",
        row_action: "edit the Claude subagent model",
    },
    DaemonTextField {
        field: CODEX_SUBAGENT_MODEL_FIELD,
        title: "Codex subagent model",
        prompt: "Model for Codex subagents, e.g. a Codex model slug",
        typing_hint: "type a model",
        default_label: "default (Codex's own default)",
        row_action: "edit the Codex subagent model",
    },
    DaemonTextField {
        field: CLAUDE_FALLBACK_MODELS_FIELD,
        title: "Claude fallback models",
        prompt: "Models to try in order when the primary is overloaded or unavailable, comma-separated, at most three",
        typing_hint: "type model names",
        default_label: "default (no fallback)",
        row_action: "edit the fallback models",
    },
    DaemonTextField {
        field: CLAUDE_MAX_BUDGET_USD_FIELD,
        title: "Claude spend cap",
        prompt: "USD at which a new Claude session stops, e.g. 5 or 2.50",
        typing_hint: "type a USD amount",
        default_label: "default (0, off)",
        row_action: "edit the spend cap",
    },
];

/// The text-edit spec of a daemon field, `None` for any other field.
#[must_use]
pub fn daemon_text_field(field: &str) -> Option<&'static DaemonTextField> {
    DAEMON_TEXT_FIELDS.iter().find(|spec| spec.field == field)
}

/// The value the daemon reported for `field`, `""` for the default.
#[must_use]
pub fn current_value(app: &App, field: &str) -> String {
    app.daemon_features
        .iter()
        .find(|entry| entry.field == field)
        .and_then(|entry| match &entry.value {
            DaemonFeatureValue::Display(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Open the editor for `field`, pre-populated with the daemon's current value.
/// Refused with a notice while the daemon's configuration is not loaded: the
/// draft would have nothing to be checked against.
pub fn open_daemon_text_editor(app: &mut App, field: &str) {
    let Some(spec) = daemon_text_field(field) else {
        return;
    };
    if !app.authoritative_config_ready() {
        let reason = app.daemon_config_unavailable_reason();
        app.notify(reason);
        return;
    }
    let input = current_value(app, spec.field);
    app.overlay = OverlayState::DaemonTextEditor {
        field: spec.field,
        input,
        error: None,
    };
}

fn clear_error(app: &mut App) {
    if let OverlayState::DaemonTextEditor { error, .. } = &mut app.overlay {
        *error = None;
    }
}

/// Paste into the draft. Standard editing replaces the selection or inserts at
/// the cursor; Vim mode appends. A pasted value is one line, so line breaks are
/// dropped (the daemon would refuse them).
pub(super) fn paste_text(app: &mut App, text: &str) {
    let line = text.replace(['\n', '\r'], "");
    let pasted = app.paste_field(&line, |overlay| match overlay {
        OverlayState::DaemonTextEditor { input, .. } => Some(input),
        _ => None,
    });
    if !pasted {
        if let OverlayState::DaemonTextEditor { input, .. } = &mut app.overlay {
            input.push_str(&line);
        }
    }
    clear_error(app);
}

/// Handle a key event for the daemon text editor overlay.
pub(super) async fn handle_daemon_text_editor_key(app: &mut App, key: KeyEvent) {
    // Standard editing: a real cursor and selection in the text.
    match app.edit_field(key, |overlay| match overlay {
        OverlayState::DaemonTextEditor { input, .. } => Some(input),
        _ => None,
    }) {
        crate::field_edit::FieldKey::Edited => {
            clear_error(app);
            return;
        }
        crate::field_edit::FieldKey::Moved => return,
        crate::field_edit::FieldKey::Ignored => {}
    }
    match key.code {
        KeyCode::Esc => app.overlay = OverlayState::None,
        KeyCode::Enter => save(app).await,
        KeyCode::Delete => {
            if let OverlayState::DaemonTextEditor { input, error, .. } = &mut app.overlay {
                input.clear();
                *error = None;
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::DaemonTextEditor { input, error, .. } = &mut app.overlay {
                input.pop();
                *error = None;
            }
        }
        KeyCode::Char(c) if !c.is_control() => {
            if let OverlayState::DaemonTextEditor { input, error, .. } = &mut app.overlay {
                input.push(c);
                *error = None;
            }
        }
        _ => {}
    }
}

/// Send the draft to the daemon; close on success, keep the draft and show the
/// daemon's reason on refusal.
async fn save(app: &mut App) {
    let OverlayState::DaemonTextEditor { field, input, .. } = &app.overlay else {
        return;
    };
    let Some(spec) = daemon_text_field(field) else {
        return;
    };
    let value = input.trim().to_string();
    match crate::action_handler::daemon_config::write_daemon_text_field(app, spec.field, &value)
        .await
    {
        Ok(()) => {
            app.overlay = OverlayState::None;
            app.notify_success(if value.is_empty() {
                format!("{} reset to the {}", spec.title, spec.default_label)
            } else {
                format!("{} set to {value}", spec.title)
            });
        }
        Err(message) => {
            if let OverlayState::DaemonTextEditor { error, .. } = &mut app.overlay {
                *error = Some(message);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::event::step_once;
    use crate::settings_registry::{SettingId, SettingsSection};
    use crossterm::event::{KeyEvent, KeyModifiers};

    async fn press(app: &mut App, code: KeyCode) {
        step_once(app, KeyEvent::new(code, KeyModifiers::NONE)).await;
    }

    async fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c)).await;
        }
    }

    fn set_directory(app: &mut App, value: &str) {
        crate::settings::DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({ COLD_ARCHIVE_DIR_FIELD: value }),
        );
    }

    /// Selects the Settings > Cold Storage row of the archive directory.
    fn select_directory_row(app: &mut App) {
        use crate::settings_keys::daemon_feature_rows_for_section;
        let section = SettingsSection::ColdStorage;
        let position = daemon_feature_rows_for_section(app, section)
            .iter()
            .position(|(spec, _)| spec.id == SettingId::ColdArchiveDir)
            .expect("the archive directory is a Cold Storage row");
        app.settings_state.section = section;
        app.settings_state.focus = crate::types::SettingsFocus::Items;
        app.settings_state.selected_index = position;
    }

    fn enter(app: &mut App) {
        assert!(crate::settings_keys::handle_settings_key(
            app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        ));
    }

    #[test]
    fn the_settings_row_opens_the_editor_with_the_daemons_directory() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        set_directory(&mut app, "/mnt/cold/rsi");
        select_directory_row(&mut app);
        enter(&mut app);
        assert!(
            matches!(
                &app.overlay,
                OverlayState::DaemonTextEditor { input, error: None, .. } if input == "/mnt/cold/rsi"
            ),
            "Enter on the row opens the text editor holding the current value"
        );
    }

    fn set_editing_mode(app: &mut App, mode: &str) {
        crate::settings::DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({ "editing_mode": mode }),
        );
    }

    /// An app with the editor open on `/mnt/old` in the given editing mode.
    fn editor_on_old_directory(mode: &str) -> App {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        set_editing_mode(&mut app, mode);
        set_directory(&mut app, "/mnt/old");
        select_directory_row(&mut app);
        enter(&mut app);
        app
    }

    fn draft(app: &App) -> &str {
        match &app.overlay {
            OverlayState::DaemonTextEditor { input, .. } => input,
            _ => panic!("the editor is open"),
        }
    }

    /// The bracketed-paste path of `event.rs`.
    fn paste(app: &mut App, text: &str) {
        assert!(crate::overlay::try_paste_text_overlay(app, text));
    }

    #[tokio::test]
    async fn standard_select_all_then_paste_replaces_the_directory() {
        let mut app = editor_on_old_directory("standard");
        step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        )
        .await;
        paste(&mut app, "/mnt/new");
        assert_eq!(draft(&app), "/mnt/new");
    }

    #[tokio::test]
    async fn standard_paste_inserts_at_the_cursor() {
        let mut app = editor_on_old_directory("standard");
        for _ in 0..4 {
            press(&mut app, KeyCode::Left).await; // before "/old"
        }
        paste(&mut app, "/cold");
        assert_eq!(draft(&app), "/mnt/cold/old");
        // The cursor follows the pasted text, so typing continues after it.
        type_text(&mut app, "X").await;
        assert_eq!(draft(&app), "/mnt/coldX/old");
    }

    #[tokio::test]
    async fn standard_paste_drops_line_breaks_and_clears_a_stale_refusal() {
        let mut app = editor_on_old_directory("standard");
        if let OverlayState::DaemonTextEditor { error, .. } = &mut app.overlay {
            *error = Some("refused".to_string());
        }
        // Paste normalization turns a lone line break into a space; none survive.
        paste(&mut app, "/x\r\ny\n");
        assert!(matches!(
            &app.overlay,
            OverlayState::DaemonTextEditor { input, error: None, .. } if input == "/mnt/old/x y"
        ));
    }

    #[tokio::test]
    async fn vim_paste_still_appends() {
        let mut app = editor_on_old_directory("vim");
        paste(&mut app, "/more");
        assert_eq!(draft(&app), "/mnt/old/more");
    }

    #[test]
    fn the_editor_does_not_open_while_the_daemon_configuration_is_not_loaded() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.authoritative_config_ready = false;
        select_directory_row(&mut app);
        enter(&mut app);
        assert!(
            matches!(app.overlay, OverlayState::None),
            "nothing to check a draft against: no editor"
        );
    }

    #[tokio::test]
    async fn escape_cancels_without_telling_the_daemon() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        // No daemon connection: a write attempt would be refused, not silently dropped.
        open_daemon_text_editor(&mut app, COLD_ARCHIVE_DIR_FIELD);
        type_text(&mut app, "/mnt/cold").await;
        press(&mut app, KeyCode::Esc).await;
        assert!(matches!(app.overlay, OverlayState::None));
        assert_eq!(
            current_value(&app, COLD_ARCHIVE_DIR_FIELD),
            "",
            "the cached value is unchanged"
        );
    }

    /// A fake daemon that records each `UpdateDaemonConfig` and answers with
    /// `reply` (a JSON-RPC `result` or `error` object, merged into the response).
    #[cfg(unix)]
    async fn serve_updates(
        listener: tokio::net::UnixListener,
        count: usize,
        reply: serde_json::Value,
    ) -> Vec<(String, serde_json::Value)> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let Ok((stream, _)) = listener.accept().await else {
            panic!("the client never connected");
        };
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let mut captured = Vec::new();
        for _ in 0..count {
            let Ok(Some(line)) = lines.next_line().await else {
                break;
            };
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "UpdateDaemonConfig");
            captured.push((
                request["params"]["field"].as_str().unwrap().to_string(),
                request["params"]["value"].clone(),
            ));
            let mut response = serde_json::json!({ "jsonrpc": "2.0", "id": request["id"] });
            for (key, value) in reply.as_object().unwrap() {
                response[key] = value.clone();
            }
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
        captured
    }

    #[cfg(unix)]
    async fn connected_app(
        reply: serde_json::Value,
        count: usize,
    ) -> (
        App,
        tempfile::TempDir,
        tokio::task::JoinHandle<Vec<(String, serde_json::Value)>>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(serve_updates(listener, count, reply));
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = crate::client::DaemonClient::new(socket);
        app.client.connect().await.unwrap();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        (app, directory, server)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saving_writes_the_directory_through_update_daemon_config() {
        let (mut app, _socket_dir, server) =
            connected_app(serde_json::json!({ "result": { "ok": true } }), 1).await;
        select_directory_row(&mut app);
        enter(&mut app);
        type_text(&mut app, "/mnt/cold/rsi").await;
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(
            server.await.unwrap(),
            vec![(
                COLD_ARCHIVE_DIR_FIELD.to_string(),
                serde_json::json!("/mnt/cold/rsi")
            )]
        );
        assert!(matches!(app.overlay, OverlayState::None), "saved: closed");
        assert_eq!(
            current_value(&app, COLD_ARCHIVE_DIR_FIELD),
            "/mnt/cold/rsi",
            "the page shows the saved directory"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_then_enter_selects_the_default_directory() {
        let (mut app, _socket_dir, server) =
            connected_app(serde_json::json!({ "result": { "ok": true } }), 1).await;
        set_directory(&mut app, "/mnt/old");
        select_directory_row(&mut app);
        enter(&mut app);
        press(&mut app, KeyCode::Delete).await;
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(
            server.await.unwrap(),
            vec![(COLD_ARCHIVE_DIR_FIELD.to_string(), serde_json::json!(""))],
            "the empty value is the daemon's spelling of the default"
        );
        assert!(matches!(app.overlay, OverlayState::None));
        assert_eq!(current_value(&app, COLD_ARCHIVE_DIR_FIELD), "");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_daemon_refusal_keeps_the_draft_and_shows_its_reason() {
        let reason =
            "the archive directory resolves to /sbx/s1/archive, inside the sandbox root /sbx";
        let (mut app, _socket_dir, server) = connected_app(
            serde_json::json!({ "error": { "code": -32602, "message": reason } }),
            1,
        )
        .await;
        set_directory(&mut app, "/mnt/old");
        select_directory_row(&mut app);
        enter(&mut app);
        press(&mut app, KeyCode::Delete).await;
        type_text(&mut app, "/link/archive").await;
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(server.await.unwrap().len(), 1);
        match &app.overlay {
            OverlayState::DaemonTextEditor { input, error, .. } => {
                assert_eq!(input, "/link/archive", "the draft survives the refusal");
                assert!(
                    error.as_deref().is_some_and(|text| text.contains(reason)),
                    "the daemon's reason is shown: {error:?}"
                );
            }
            _ => panic!("the editor stays open after a refusal"),
        }
        assert_eq!(
            current_value(&app, COLD_ARCHIVE_DIR_FIELD),
            "/mnt/old",
            "a refused value never reaches the page"
        );
        // Editing clears the stale reason.
        type_text(&mut app, "x").await;
        assert!(matches!(
            &app.overlay,
            OverlayState::DaemonTextEditor { error: None, .. }
        ));
    }

    /// Selects the Settings > Harness tools row of the SearXNG URL.
    fn select_searxng_row(app: &mut App) {
        use crate::settings_keys::daemon_feature_rows_for_section;
        let section = SettingsSection::HarnessTools;
        let position = daemon_feature_rows_for_section(app, section)
            .iter()
            .position(|(spec, _)| spec.id == SettingId::HarnessSearxngUrl)
            .expect("the SearXNG URL is a Harness tools row");
        app.settings_state.section = section;
        app.settings_state.focus = crate::types::SettingsFocus::Items;
        app.settings_state.selected_index = position;
    }

    #[test]
    fn the_searxng_row_opens_the_editor_with_the_daemons_url() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        crate::settings::DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({ SEARXNG_URL_FIELD: "https://search.lan/searx" }),
        );
        select_searxng_row(&mut app);
        enter(&mut app);
        assert!(
            matches!(
                &app.overlay,
                OverlayState::DaemonTextEditor { field, input, error: None }
                    if *field == SEARXNG_URL_FIELD && input == "https://search.lan/searx"
            ),
            "Enter on the SearXNG row opens the text editor holding the current URL"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saving_a_searxng_url_writes_it_through_update_daemon_config() {
        let (mut app, _socket_dir, server) =
            connected_app(serde_json::json!({ "result": { "ok": true } }), 1).await;
        select_searxng_row(&mut app);
        enter(&mut app);
        type_text(&mut app, "http://127.0.0.1:8888").await;
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(
            server.await.unwrap(),
            vec![(
                SEARXNG_URL_FIELD.to_string(),
                serde_json::json!("http://127.0.0.1:8888")
            )]
        );
        assert!(matches!(app.overlay, OverlayState::None), "saved: closed");
        assert_eq!(
            current_value(&app, SEARXNG_URL_FIELD),
            "http://127.0.0.1:8888",
            "the page shows the saved URL"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_searxng_url_keeps_the_draft_and_shows_the_daemons_reason() {
        let reason = "expected an http(s) URL such as http://127.0.0.1:8888";
        let (mut app, _socket_dir, server) = connected_app(
            serde_json::json!({ "error": { "code": -32602, "message": reason } }),
            1,
        )
        .await;
        select_searxng_row(&mut app);
        enter(&mut app);
        type_text(&mut app, "not a url").await;
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(server.await.unwrap().len(), 1);
        match &app.overlay {
            OverlayState::DaemonTextEditor { input, error, .. } => {
                assert_eq!(input, "not a url", "the draft survives the refusal");
                assert!(
                    error.as_deref().is_some_and(|text| text.contains(reason)),
                    "the daemon's reason is shown: {error:?}"
                );
            }
            _ => panic!("the editor stays open after a refusal"),
        }
        assert_eq!(current_value(&app, SEARXNG_URL_FIELD), "");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clearing_the_searxng_url_selects_the_default() {
        let (mut app, _socket_dir, server) =
            connected_app(serde_json::json!({ "result": { "ok": true } }), 1).await;
        crate::settings::DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({ SEARXNG_URL_FIELD: "http://a.test" }),
        );
        select_searxng_row(&mut app);
        enter(&mut app);
        press(&mut app, KeyCode::Delete).await;
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(
            server.await.unwrap(),
            vec![(SEARXNG_URL_FIELD.to_string(), serde_json::json!(""))],
            "the daemon reads an empty URL as unset"
        );
        assert_eq!(current_value(&app, SEARXNG_URL_FIELD), "");
    }

    /// Selects the Settings > Provider Isolation row of the launch overrides.
    fn select_launch_overrides_row(app: &mut App) {
        use crate::settings_keys::daemon_feature_rows_for_section;
        let section = SettingsSection::ProviderIsolation;
        let position = daemon_feature_rows_for_section(app, section)
            .iter()
            .position(|(spec, _)| spec.id == SettingId::ProviderLaunchOverrides)
            .expect("the launch overrides are a Provider Isolation row");
        app.settings_state.section = section;
        app.settings_state.focus = crate::types::SettingsFocus::Items;
        app.settings_state.selected_index = position;
    }

    /// hub #2765: the row shows the daemon's canonical JSON ("" while empty)
    /// and Enter opens the editor holding it.
    #[test]
    fn the_launch_overrides_row_opens_the_editor_with_the_daemons_json() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        crate::settings::DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({ PROVIDER_LAUNCH_OVERRIDES_FIELD: {} }),
        );
        assert_eq!(current_value(&app, PROVIDER_LAUNCH_OVERRIDES_FIELD), "");
        let stored = serde_json::json!({"codex": ["model_reasoning_summary=concise"]});
        crate::settings::DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({ PROVIDER_LAUNCH_OVERRIDES_FIELD: stored.clone() }),
        );
        select_launch_overrides_row(&mut app);
        enter(&mut app);
        assert!(
            matches!(
                &app.overlay,
                OverlayState::DaemonTextEditor { field, input, error: None }
                    if *field == PROVIDER_LAUNCH_OVERRIDES_FIELD && *input == stored.to_string()
            ),
            "Enter opens the editor holding the current overrides"
        );
    }

    /// hub #2765: a denied key comes back as the daemon's typed refusal, which
    /// the editor shows while keeping the draft; an accepted value is written
    /// through `UpdateDaemonConfig`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_denied_launch_override_shows_the_typed_refusal_and_keeps_the_draft() {
        let refusal = rsi_common::provider_launch_overrides::normalize(
            &serde_json::json!({"codex": ["approval_policy=never"]}),
        )
        .unwrap_err();
        let reason =
            format!("Invalid value for field '{PROVIDER_LAUNCH_OVERRIDES_FIELD}': {refusal}");
        let (mut app, _socket_dir, server) = connected_app(
            serde_json::json!({ "error": { "code": -32602, "message": reason } }),
            1,
        )
        .await;
        select_launch_overrides_row(&mut app);
        enter(&mut app);
        let draft_text = r#"{"codex":["approval_policy=never"]}"#;
        type_text(&mut app, draft_text).await;
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(
            server.await.unwrap(),
            vec![(
                PROVIDER_LAUNCH_OVERRIDES_FIELD.to_string(),
                serde_json::json!(draft_text)
            )]
        );
        match &app.overlay {
            OverlayState::DaemonTextEditor { input, error, .. } => {
                assert_eq!(input, draft_text, "the draft survives the refusal");
                assert!(
                    error.as_deref().is_some_and(|text| {
                        text.contains("provider_launch_override_denied")
                            && text.contains("approval_policy")
                    }),
                    "the typed refusal is shown: {error:?}"
                );
            }
            _ => panic!("the editor stays open after a refusal"),
        }

        let (mut app, _socket_dir, server) =
            connected_app(serde_json::json!({ "result": { "ok": true } }), 1).await;
        select_launch_overrides_row(&mut app);
        enter(&mut app);
        let accepted = r#"{"codex":["model_reasoning_summary=concise"]}"#;
        type_text(&mut app, accepted).await;
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(
            server.await.unwrap(),
            vec![(
                PROVIDER_LAUNCH_OVERRIDES_FIELD.to_string(),
                serde_json::json!(accepted)
            )]
        );
        assert!(matches!(app.overlay, OverlayState::None), "saved: closed");
    }
}
