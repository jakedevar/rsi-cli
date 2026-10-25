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
//!
//! The same overlay types a number into a numeric daemon row (#3468): a row
//! whose choices are all numbers (a spend cap, a slot count) is a slider on
//! `h`/`l` over its presets and the live value, and Enter opens this editor so
//! any value can be typed. The range is the daemon's own (`bounded_u64` in its
//! `update_field`): the editor sends a JSON number and shows the daemon's
//! refusal with the draft kept, never a silent no-op.

use crate::app::App;
use crate::settings::{DaemonFeatureEntry, DaemonFeatureValue};
use crate::settings_staging::{StepEdge, edge_for_options};
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

/// The daemon field of the benchmark docker host allowlist (#3176).
pub const BENCH_DOCKER_HOSTS_FIELD: &str = rsi_common::exec_target::BENCH_DOCKER_HOSTS_FIELD;

/// The daemon field of the benchmark laptop's ssh host (#3181).
pub const BENCH_LAPTOP_HOST_FIELD: &str = rsi_common::bench_settings::BENCH_LAPTOP_HOST_FIELD;

/// The daemon field of the Claude `--max-budget-usd` cap (#2762).
pub const CLAUDE_MAX_BUDGET_USD_FIELD: &str = "claude_max_budget_usd";

/// The daemon field of the OpenRouter spend ceiling (#3154).
pub const OPENROUTER_SPEND_CAP_USD_FIELD: &str = "openrouter_spend_cap_usd";

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
        field: BENCH_DOCKER_HOSTS_FIELD,
        title: "Benchmark docker hosts",
        prompt: "Docker hosts a benchmark container may run on, comma-separated, e.g. ssh://arch-laptop",
        typing_hint: "type docker hosts",
        default_label: "default (none: every container target refused)",
        row_action: "edit the benchmark docker hosts",
    },
    DaemonTextField {
        field: BENCH_LAPTOP_HOST_FIELD,
        title: "Benchmark laptop host",
        prompt: "The ssh host that runs Harbor for a benchmark job, e.g. arch-laptop or user@arch-laptop",
        typing_hint: "type the ssh host",
        default_label: "default (arch-laptop)",
        row_action: "edit the benchmark laptop host",
    },
    DaemonTextField {
        field: CLAUDE_MAX_BUDGET_USD_FIELD,
        title: "Claude spend cap",
        prompt: "USD at which a new Claude session stops, e.g. 5 or 2.50",
        typing_hint: "type a USD amount",
        default_label: "default (0, off)",
        row_action: "edit the spend cap",
    },
    DaemonTextField {
        field: OPENROUTER_SPEND_CAP_USD_FIELD,
        title: "OpenRouter spend ceiling",
        prompt: "USD above the account usage at this moment at which OpenRouter-billed work stops, e.g. 100 (0 turns it off)",
        typing_hint: "type a USD amount",
        default_label: "default (0, off)",
        row_action: "arm the OpenRouter spend ceiling",
    },
];

/// The text-edit spec of a daemon field, `None` for any other field.
#[must_use]
pub fn daemon_text_field(field: &str) -> Option<&'static DaemonTextField> {
    DAEMON_TEXT_FIELDS.iter().find(|spec| spec.field == field)
}

/// True for a numeric daemon row (#3468): its choices are all numbers (a
/// leading `Default` is allowed), so Settings steps it like a slider and Enter
/// types a value. Free-text rows and enum choices are not.
#[must_use]
pub fn daemon_number_entry(entry: &DaemonFeatureEntry) -> bool {
    match &entry.value {
        DaemonFeatureValue::Cycle { options, .. } => {
            daemon_text_field(&entry.field).is_none()
                && edge_for_options(options) == StepEdge::Clamp
        }
        _ => false,
    }
}

/// The largest value the daemon accepts for a numeric `field`, when the TUI
/// can name it from the daemon's own constant. `None` means the daemon alone
/// judges the range and its refusal is shown.
#[must_use]
pub fn daemon_number_max(field: &str) -> Option<u64> {
    match field {
        "cloud_spend_stop_line_usd" | "cloud_spend_daily_cap_usd" => {
            Some(rsi_common::cloud_spend::MAX_CAP_USD)
        }
        _ => None,
    }
}

/// What the editor shows around the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorCopy {
    pub title: String,
    /// The label line above the draft.
    pub prompt: String,
    /// The key-hint prefix, e.g. "type a path".
    pub typing_hint: String,
}

/// The editor copy of `field`: a text field's spec or a numeric row's label.
#[must_use]
pub fn editor_copy(features: &[DaemonFeatureEntry], field: &str) -> Option<EditorCopy> {
    if let Some(spec) = daemon_text_field(field) {
        return Some(EditorCopy {
            title: spec.title.to_string(),
            prompt: format!("{} — empty = {}", spec.prompt, spec.default_label),
            typing_hint: spec.typing_hint.to_string(),
        });
    }
    let entry = features
        .iter()
        .find(|entry| entry.field == field && daemon_number_entry(entry))?;
    let range = match daemon_number_max(field) {
        Some(max) => format!("a whole number from 0 to {max}"),
        None => "a number; the daemon refuses a value outside its range".to_string(),
    };
    let (now, has_default) = match &entry.value {
        DaemonFeatureValue::Cycle { options, current } => (
            options.get(*current).cloned().unwrap_or_default(),
            options.iter().any(|option| option == "Default"),
        ),
        _ => (String::new(), false),
    };
    let default_hint = if has_default { " or \"default\"" } else { "" };
    Some(EditorCopy {
        title: entry.label.clone(),
        prompt: format!("Type {range}{default_hint}. Now: {now}"),
        typing_hint: "type a number".to_string(),
    })
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
    let Some(input) = initial_draft(app, field) else {
        return;
    };
    if !app.authoritative_config_ready() {
        let reason = app.daemon_config_unavailable_reason();
        app.notify(reason);
        return;
    }
    app.overlay = OverlayState::DaemonTextEditor {
        field: field.to_string(),
        input,
        error: None,
    };
}

/// The draft an editor of `field` opens with; `None` when `field` has no editor.
/// A number starts empty, so typing is the new value in either editing mode (the
/// prompt shows the live one); Esc cancels.
fn initial_draft(app: &App, field: &str) -> Option<String> {
    if daemon_text_field(field).is_some() {
        return Some(current_value(app, field));
    }
    app.daemon_features
        .iter()
        .any(|entry| entry.field == field && daemon_number_entry(entry))
        .then(String::new)
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
        let (field, value) = (field.clone(), input.trim().to_string());
        save_number(app, &field, &value).await;
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

/// Send a typed number to the daemon; close on success, keep the draft and show
/// the daemon's reason (or why the text is not a number) on refusal.
async fn save_number(app: &mut App, field: &str, value: &str) {
    let label = app
        .daemon_features
        .iter()
        .find(|entry| entry.field == field)
        .map(|entry| entry.label.clone())
        .unwrap_or_else(|| field.to_string());
    match crate::action_handler::daemon_config::write_daemon_number_field(app, field, value).await {
        Ok(shown) => {
            app.overlay = OverlayState::None;
            app.notify_success(format!("{label} set to {shown}"));
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
    pub(super) async fn serve_updates(
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
        while captured.len() < count {
            let Ok(Some(line)) = lines.next_line().await else {
                break;
            };
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            if request["method"] != "UpdateDaemonConfig" {
                // A section's own refresh request, not the write under test.
                let refusal = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": { "code": -32601, "message": "not under test" },
                });
                write
                    .write_all(format!("{refusal}\n").as_bytes())
                    .await
                    .unwrap();
                continue;
            }
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
        let directory = crate::test_support::socket_tempdir().unwrap();
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

/// The operator's own key path (#3430): `Space ,` opens Settings, the rail and
/// section tabs reach the section, `j` the row, `Enter` opens the editor, typed
/// keys fill it and `Enter` saves. Every key goes through `step_once`, so the
/// action registry sees each one exactly as it does live.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod real_key_path_tests {
    use super::*;
    use crate::event::step_once;
    use crate::settings_registry::{SETTINGS, SettingId, SettingsGroup, SettingsSection};
    use crate::types::SettingsFocus;
    use crossterm::event::{KeyEvent, KeyModifiers};

    async fn press(app: &mut App, code: KeyCode) {
        step_once(app, KeyEvent::new(code, KeyModifiers::NONE)).await;
    }

    async fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c)).await;
        }
    }

    fn set_editing_mode(app: &mut App, mode: &str) {
        crate::settings::DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({ "editing_mode": mode }),
        );
    }

    fn latest_notice(app: &App) -> String {
        app.notifications
            .back()
            .map(|notice| notice.message.clone())
            .unwrap_or_default()
    }

    /// Opens Settings and walks to the row of `id` with keys only.
    async fn reach_row(app: &mut App, id: SettingId) {
        let spec = SETTINGS.iter().find(|spec| spec.id == id).unwrap();
        let section = spec.section;
        press(app, KeyCode::Char(' ')).await;
        press(app, KeyCode::Char(',')).await;
        assert_eq!(app.settings_state.focus, SettingsFocus::Categories);
        // The rail lists the groups; `j` walks down it.
        for _ in 0..SettingsGroup::ALL.len() {
            if app.settings_state.section.group() == section.group() {
                break;
            }
            press(app, KeyCode::Char('j')).await;
        }
        assert_eq!(app.settings_state.section.group(), section.group());
        for _ in 0..SettingsSection::ALL.len() {
            if app.settings_state.section == section {
                break;
            }
            press(app, KeyCode::Tab).await;
        }
        assert_eq!(
            app.settings_state.section, section,
            "Tab reaches the section"
        );
        // A section tab lands on its rows; the group's first section is
        // entered from the rail with Enter.
        if app.settings_state.focus == SettingsFocus::Categories {
            press(app, KeyCode::Enter).await;
        }
        assert_eq!(app.settings_state.focus, SettingsFocus::Items);
        let position = crate::settings_keys::daemon_feature_rows_for_section(app, section)
            .iter()
            .position(|(row, _)| row.id == id)
            .expect("the row is a daemon feature row");
        for _ in 0..position {
            press(app, KeyCode::Char('j')).await;
        }
        assert_eq!(app.settings_state.selected_index, position);
    }

    fn ready_app(mode: &str) -> App {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        set_editing_mode(&mut app, mode);
        app
    }

    /// Opens the editor of `id` by keys, types `text`, saves, and returns what
    /// the daemon received plus the app.
    async fn edit_and_save(mode: &str, id: SettingId, field: &'static str, text: &str) {
        let directory = crate::test_support::socket_tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(super::tests::serve_updates(
            listener,
            1,
            serde_json::json!({ "result": { "ok": true } }),
        ));
        let mut app = ready_app(mode);
        app.client = crate::client::DaemonClient::new(socket);
        app.client.connect().await.unwrap();
        reach_row(&mut app, id).await;
        press(&mut app, KeyCode::Enter).await;
        assert!(
            matches!(
                &app.overlay,
                OverlayState::DaemonTextEditor { field: open, .. } if *open == field
            ),
            "{mode}: Enter on the row opens the {field} editor (notice: {})",
            latest_notice(&app)
        );
        type_text(&mut app, text).await;
        match &app.overlay {
            OverlayState::DaemonTextEditor { input, .. } => {
                assert_eq!(input, text, "{mode}: the editor takes the typed text");
            }
            _ => panic!("{mode}: typing closed the editor"),
        }
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(
            server.await.unwrap(),
            vec![(field.to_string(), serde_json::json!(text))],
            "{mode}: the daemon receives the value"
        );
        assert!(matches!(app.overlay, OverlayState::None), "{mode}: saved");
        let spec = daemon_text_field(field).unwrap();
        assert_eq!(
            latest_notice(&app),
            format!("{} set to {text}", spec.title),
            "{mode}: the green notice"
        );
        assert_eq!(current_value(&app, field), text);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn standard_benchmark_docker_hosts_takes_and_saves_the_value() {
        edit_and_save(
            "standard",
            SettingId::BenchDockerHosts,
            BENCH_DOCKER_HOSTS_FIELD,
            "ssh://arch-laptop",
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vim_benchmark_docker_hosts_takes_and_saves_the_value() {
        edit_and_save(
            "vim",
            SettingId::BenchDockerHosts,
            BENCH_DOCKER_HOSTS_FIELD,
            "ssh://arch-laptop",
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn standard_cold_storage_directory_takes_and_saves_the_value() {
        edit_and_save(
            "standard",
            SettingId::ColdArchiveDir,
            COLD_ARCHIVE_DIR_FIELD,
            "/mnt/cold/rsi",
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vim_cold_storage_directory_takes_and_saves_the_value() {
        edit_and_save(
            "vim",
            SettingId::ColdArchiveDir,
            COLD_ARCHIVE_DIR_FIELD,
            "/mnt/cold/rsi",
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn standard_searxng_url_takes_and_saves_the_value() {
        edit_and_save(
            "standard",
            SettingId::HarnessSearxngUrl,
            SEARXNG_URL_FIELD,
            "http://127.0.0.1:8888",
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vim_searxng_url_takes_and_saves_the_value() {
        edit_and_save(
            "vim",
            SettingId::HarnessSearxngUrl,
            SEARXNG_URL_FIELD,
            "http://127.0.0.1:8888",
        )
        .await;
    }

    /// Every free-text field has a Settings row whose Enter is available to the
    /// action registry; a row the registry refuses is a silent no-op.
    #[tokio::test]
    async fn enter_is_available_on_every_text_row() {
        for spec in SETTINGS {
            let Some(field) = crate::settings_keys::daemon_feature_field_for(spec.id) else {
                continue;
            };
            if daemon_text_field(field).is_none() {
                continue;
            }
            let mut app = ready_app("standard");
            reach_row(&mut app, spec.id).await;
            press(&mut app, KeyCode::Enter).await;
            assert!(
                matches!(
                    &app.overlay,
                    OverlayState::DaemonTextEditor { field: open, .. } if *open == field
                ),
                "Enter on {} opens its editor (notice: {})",
                spec.label,
                latest_notice(&app)
            );
        }
    }

    /// The text a numeric row shows for its live value.
    fn row_text(app: &App, field: &str) -> String {
        match &app
            .daemon_features
            .iter()
            .find(|entry| entry.field == field)
            .unwrap()
            .value
        {
            DaemonFeatureValue::Cycle { options, current } => options[*current].clone(),
            other => panic!("{field} is not a numeric row: {other:?}"),
        }
    }

    /// The option a staged slider step points at, `None` while nothing is staged.
    fn staged_text(app: &App, field: &str) -> Option<String> {
        let crate::settings_staging::StagedValue::Choice(index) =
            app.settings_state.pending.daemon_staged(field)?
        else {
            return None;
        };
        match &app
            .daemon_features
            .iter()
            .find(|entry| entry.field == field)?
            .value
        {
            DaemonFeatureValue::Cycle { options, .. } => options.get(index).cloned(),
            _ => None,
        }
    }

    type Updates = std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>;

    /// A fake daemon with state: `UpdateDaemonConfig` stores the field (or
    /// answers `refusal` when given) and `GetDaemonConfig` reports the stored
    /// fields, so a test reads back what the TUI wrote.
    #[cfg(unix)]
    async fn stateful_daemon(
        refusal: Option<&'static str>,
    ) -> (App, tempfile::TempDir, Updates, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let directory = crate::test_support::socket_tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let updates: Updates = Updates::default();
        let log = updates.clone();
        let server = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let mut stored = serde_json::Map::new();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let mut response = serde_json::json!({ "jsonrpc": "2.0", "id": request["id"] });
                match request["method"].as_str() {
                    Some("UpdateDaemonConfig") => {
                        log.lock().unwrap().push((
                            request["params"]["field"].as_str().unwrap().to_string(),
                            request["params"]["value"].clone(),
                        ));
                        if let Some(reason) = refusal {
                            response["error"] =
                                serde_json::json!({ "code": -32602, "message": reason });
                        } else {
                            stored.insert(
                                request["params"]["field"].as_str().unwrap().to_string(),
                                request["params"]["value"].clone(),
                            );
                            response["result"] = serde_json::json!({ "ok": true });
                        }
                    }
                    Some("GetDaemonConfig") => {
                        response["result"] = serde_json::Value::Object(stored.clone());
                    }
                    _ => {
                        response["error"] =
                            serde_json::json!({ "code": -32601, "message": "not under test" });
                    }
                }
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let mut app = ready_app("standard");
        app.client = crate::client::DaemonClient::new(socket);
        app.client.connect().await.unwrap();
        (app, directory, updates, server)
    }

    fn set_live(app: &mut App, field: &str, value: u64) {
        crate::settings::DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({ field: value }),
        );
    }

    const DAILY_CAP: &str = "cloud_spend_daily_cap_usd";
    const STOP_LINE: &str = "cloud_spend_stop_line_usd";

    /// #3468: the operator's $200 / 24 h for 7 days needs a daily cap of 190 and
    /// a stop line of 1330, both above the highest preset (100 and 200).
    #[cfg(unix)]
    async fn type_both_cloud_spend_values(mode: &str) {
        let (mut app, _dir, updates, server) = stateful_daemon(None).await;
        set_editing_mode(&mut app, mode);
        set_live(&mut app, DAILY_CAP, 15);
        set_live(&mut app, STOP_LINE, 90);
        for (id, field, text) in [
            (SettingId::CloudSpendDailyCap, DAILY_CAP, "190"),
            (SettingId::CloudSpendStopLine, STOP_LINE, "1330"),
        ] {
            reach_row(&mut app, id).await;
            press(&mut app, KeyCode::Enter).await;
            assert!(
                matches!(
                    &app.overlay,
                    OverlayState::DaemonTextEditor { field: open, .. } if open == field
                ),
                "{mode}: Enter on the {field} row opens the number editor (notice: {})",
                latest_notice(&app)
            );
            // The draft starts empty: typing is the new value.
            type_text(&mut app, text).await;
            press(&mut app, KeyCode::Enter).await;
            assert!(matches!(app.overlay, OverlayState::None), "{mode}: saved");
            assert_eq!(row_text(&app, field), text, "{mode}: the row shows {text}");
            // Close Settings; `reach_row` reopens it for the next row.
            crate::settings_keys::close_settings(&mut app);
            app.settings_state.focus = SettingsFocus::Categories;
            app.settings_state.selected_index = 0;
        }
        assert_eq!(
            updates.lock().unwrap().clone(),
            vec![
                (DAILY_CAP.to_string(), serde_json::json!(190)),
                (STOP_LINE.to_string(), serde_json::json!(1330)),
            ],
            "{mode}: the daemon receives JSON numbers"
        );
        let config = app.client.get_daemon_config().await.unwrap();
        assert_eq!(config[DAILY_CAP], serde_json::json!(190));
        assert_eq!(config[STOP_LINE], serde_json::json!(1330));
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn standard_cloud_spend_rows_take_190_and_1330() {
        type_both_cloud_spend_values("standard").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vim_cloud_spend_rows_take_190_and_1330() {
        type_both_cloud_spend_values("vim").await;
    }

    /// h/l and Left/Right step a numeric row like a slider (staged until `w`),
    /// through the presets and a live value no preset matches.
    async fn slide_through_values(mode: &str, back: KeyCode, forward: KeyCode) {
        let mut app = ready_app(mode);
        set_live(&mut app, DAILY_CAP, 190);
        assert_eq!(row_text(&app, DAILY_CAP), "190", "the live value is shown");
        reach_row(&mut app, SettingId::CloudSpendDailyCap).await;
        press(&mut app, back).await;
        assert_eq!(
            app.settings_state.focus,
            SettingsFocus::Items,
            "{mode}: stays on the rows"
        );
        assert_eq!(
            staged_text(&app, DAILY_CAP).as_deref(),
            Some("100"),
            "{mode}: one step down"
        );
        press(&mut app, back).await;
        assert_eq!(
            staged_text(&app, DAILY_CAP).as_deref(),
            Some("50"),
            "{mode}: two steps down"
        );
        press(&mut app, forward).await;
        press(&mut app, forward).await;
        assert_eq!(
            staged_text(&app, DAILY_CAP),
            None,
            "{mode}: back on the live 190, nothing is pending"
        );
        press(&mut app, forward).await;
        assert_eq!(
            staged_text(&app, DAILY_CAP),
            None,
            "{mode}: 190 is the top, a step stays put"
        );
        assert_eq!(
            row_text(&app, DAILY_CAP),
            "190",
            "{mode}: the live value is untouched"
        );
    }

    #[tokio::test]
    async fn standard_arrow_keys_slide_a_numeric_row() {
        slide_through_values("standard", KeyCode::Left, KeyCode::Right).await;
    }

    #[tokio::test]
    async fn vim_h_and_l_slide_a_numeric_row() {
        slide_through_values("vim", KeyCode::Char('h'), KeyCode::Char('l')).await;
    }

    /// A non-numeric choice row keeps `h` as "back to the rail".
    #[tokio::test]
    async fn h_still_returns_to_the_rail_on_a_choice_row() {
        let mut app = ready_app("vim");
        reach_row(&mut app, SettingId::HarnessWebAccess).await;
        press(&mut app, KeyCode::Char('h')).await;
        assert_eq!(app.settings_state.focus, SettingsFocus::Categories);
    }

    /// The daemon's refusal is on screen with the draft kept, and the row's
    /// value does not change.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_out_of_range_number_shows_the_daemons_refusal() {
        let reason = "Invalid value for field 'cloud_spend_daily_cap_usd': expected 0..=100000";
        let (mut app, _dir, updates, server) = stateful_daemon(Some(reason)).await;
        set_live(&mut app, DAILY_CAP, 15);
        reach_row(&mut app, SettingId::CloudSpendDailyCap).await;
        press(&mut app, KeyCode::Enter).await;
        type_text(&mut app, "999999").await;
        press(&mut app, KeyCode::Enter).await;
        assert_eq!(
            updates.lock().unwrap().clone(),
            vec![(DAILY_CAP.to_string(), serde_json::json!(999_999))]
        );
        match &app.overlay {
            OverlayState::DaemonTextEditor { input, error, .. } => {
                assert_eq!(input, "999999", "the draft survives the refusal");
                assert!(
                    error.as_deref().is_some_and(|text| text.contains(reason)),
                    "the daemon's reason is shown: {error:?}"
                );
            }
            _ => panic!("the editor stays open after a refusal"),
        }
        assert_eq!(
            row_text(&app, DAILY_CAP),
            "15",
            "a refused value never reaches the row"
        );
        server.abort();
    }

    /// Text that is not a number never reaches the daemon and says so.
    #[cfg(unix)]
    #[tokio::test]
    async fn text_that_is_not_a_number_is_refused_on_screen() {
        let (mut app, _dir, updates, server) = stateful_daemon(None).await;
        reach_row(&mut app, SettingId::CloudSpendDailyCap).await;
        press(&mut app, KeyCode::Enter).await;
        type_text(&mut app, "12abc").await;
        press(&mut app, KeyCode::Enter).await;
        assert!(updates.lock().unwrap().is_empty(), "nothing was sent");
        assert!(matches!(
            &app.overlay,
            OverlayState::DaemonTextEditor { error: Some(text), .. } if text.contains("12abc")
        ));
        server.abort();
    }

    /// Every numeric daemon row opens its number editor on Enter.
    #[tokio::test]
    async fn enter_opens_the_number_editor_on_every_numeric_row() {
        let mut checked = 0;
        for spec in SETTINGS {
            let Some(field) = crate::settings_keys::daemon_feature_field_for(spec.id) else {
                continue;
            };
            if !crate::settings_keys::DAEMON_FEATURE_SECTIONS.contains(&spec.section) {
                continue;
            }
            let app = ready_app("standard");
            let numeric = app
                .daemon_features
                .iter()
                .any(|entry| entry.field == field && daemon_number_entry(entry));
            if !numeric {
                continue;
            }
            let mut app = app;
            reach_row(&mut app, spec.id).await;
            press(&mut app, KeyCode::Enter).await;
            assert!(
                matches!(
                    &app.overlay,
                    OverlayState::DaemonTextEditor { field: open, .. } if open == field
                ),
                "Enter on {} opens its number editor (notice: {})",
                spec.label,
                latest_notice(&app)
            );
            checked += 1;
        }
        assert!(
            checked >= 50,
            "the audit covers the numeric rows ({checked})"
        );
    }

    #[tokio::test]
    async fn a_row_that_cannot_open_says_why_on_screen() {
        let mut app = ready_app("standard");
        reach_row(&mut app, SettingId::BenchDockerHosts).await;
        app.poll.authoritative_config_ready = false;
        press(&mut app, KeyCode::Enter).await;
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(
            !latest_notice(&app).is_empty(),
            "a refused Enter always leaves a notice"
        );
    }
}
