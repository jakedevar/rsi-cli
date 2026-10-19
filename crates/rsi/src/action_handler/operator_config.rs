//! hub #3039: custom providers, Claude hooks and message bridges through the
//! daemon's operator RPCs. The TUI never writes those files itself and never
//! holds a custom provider's key after submitting it.

use crate::app::App;
use crate::claude_config::{HookEdit, PendingHookSave};
use crate::settings::{CustomProviderEntry, MessageBridgeKind, MessageBridgeSettings};
use crate::types::OverlayState;
use rsi_common::claude_hooks::ClaudeHooks;
use rsi_common::operator_config::PutCustomProviderParams;

use super::daemon_config::require_authoritative_config;

/// Replace the provider list with the daemon's and keep every selection index
/// that pointed into it in range.
fn store_custom_providers(app: &mut App, providers: Vec<CustomProviderEntry>) {
    app.settings.custom_providers = providers;
    let len = app.settings.custom_providers.len();
    if app.custom_provider_index.is_some_and(|index| index >= len) {
        app.custom_provider_index = None;
    }
    if app.settings_state.section == crate::types::SettingsSection::ApiProviders {
        let max = len.saturating_sub(1);
        if app.settings_state.selected_index > max {
            app.settings_state.selected_index = max;
        }
    }
    app.needs_model_refresh = true;
    app.mark_dirty();
}

#[allow(clippy::future_not_send)]
async fn reload_custom_providers(app: &mut App) -> bool {
    match app.client.list_custom_providers().await {
        Ok(result) => {
            let providers = result
                .providers
                .into_iter()
                .map(CustomProviderEntry::from)
                .collect();
            store_custom_providers(app, providers);
            true
        }
        Err(error) => {
            tracing::warn!("Failed to list custom providers: {}", error);
            app.notify_error(format!("Failed to load custom providers: {error}"));
            false
        }
    }
}

/// `LcAction::RefreshCustomProviders`: load the list, then point each daemon
/// model role at the provider the settings select (the daemon resolves the
/// key; a role whose provider is gone is detached).
#[allow(clippy::future_not_send)]
pub async fn refresh_custom_providers(app: &mut App) {
    if !require_authoritative_config(app) {
        return;
    }
    if reload_custom_providers(app).await {
        super::daemon_config::sync_custom_provider_routes(app).await;
    }
}

#[allow(clippy::future_not_send)]
pub async fn put_custom_provider(app: &mut App, params: PutCustomProviderParams) {
    if !require_authoritative_config(app) {
        return;
    }
    let editing = params.id.is_some();
    match app.client.put_custom_provider(params).await {
        Ok(summary) => {
            if matches!(app.overlay, OverlayState::ProviderForm { .. }) {
                app.overlay = OverlayState::None;
            }
            let verb = if editing { "Updated" } else { "Added" };
            app.notify_success(format!("{verb} provider: {}", summary.name));
            reload_custom_providers(app).await;
        }
        Err(error) => {
            tracing::warn!("Failed to save custom provider: {}", error);
            app.notify_error(format!("Failed to save provider: {error}"));
        }
    }
}

#[allow(clippy::future_not_send)]
pub async fn delete_custom_provider(app: &mut App, id: uuid::Uuid) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.delete_custom_provider(id).await {
        Ok(_) => {
            app.notify_success("Deleted provider and its stored key");
            reload_custom_providers(app).await;
        }
        Err(error) => {
            tracing::warn!("Failed to delete custom provider: {}", error);
            app.notify_error(format!("Failed to delete provider: {error}"));
        }
    }
}

#[allow(clippy::future_not_send)]
pub async fn refresh_claude_hooks(app: &mut App) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.list_claude_hooks().await {
        Ok(view) => app.set_claude_hooks(view),
        Err(error) => {
            // `claude_hooks_requested` stays set, so render does not retry in
            // a loop; a reload or a save asks again.
            tracing::warn!("Failed to read Claude hooks: {}", error);
            app.notify_error(format!(
                "Failed to read ~/.claude/settings.json hooks: {error}"
            ));
        }
    }
}

/// `LcAction::SaveClaudeHooks`: write through the daemon. A stale revision
/// replays `edit` on the current hooks and opens the Overwrite / Reload /
/// Cancel prompt, as the TUI's own write used to.
#[allow(clippy::future_not_send)]
pub async fn save_claude_hooks(
    app: &mut App,
    expected_revision: String,
    hooks: ClaudeHooks,
    edit: HookEdit,
) {
    if !require_authoritative_config(app) {
        return;
    }
    let deleting = matches!(edit, HookEdit::Delete { .. });
    match app.client.put_claude_hooks(expected_revision, hooks).await {
        Ok(Ok(view)) => {
            app.set_claude_hooks(view);
            if matches!(
                app.overlay,
                OverlayState::HookForm { .. } | OverlayState::HookConflictPrompt { .. }
            ) {
                app.overlay = OverlayState::None;
            }
            let max = app.cached_hook_rows.len().saturating_sub(1);
            if app.settings_state.section == crate::types::SettingsSection::ClaudeHooks
                && app.settings_state.selected_index > max
            {
                app.settings_state.selected_index = max;
            }
            app.notify_success(if deleting {
                "Deleted hook. Applies to new Claude sessions."
            } else {
                "Saved. Applies to new Claude sessions."
            });
        }
        Ok(Err(_current_revision)) => match app.client.list_claude_hooks().await {
            Ok(current) => {
                let hooks = edit.apply(current.hooks.clone());
                let revision = current.revision.clone();
                app.set_claude_hooks(current);
                app.overlay = OverlayState::HookConflictPrompt {
                    pending: Box::new(PendingHookSave {
                        edit,
                        revision,
                        hooks,
                    }),
                };
            }
            Err(error) => {
                app.notify_error(format!(
                    "~/.claude/settings.json changed and could not be re-read: {error}"
                ));
            }
        },
        Err(error) => {
            tracing::warn!("Failed to save Claude hooks: {}", error);
            app.notify_error(format!("Failed to save: {error}"));
        }
    }
}

#[allow(clippy::future_not_send)]
pub async fn open_message_bridge_form(app: &mut App, bridge: MessageBridgeKind) {
    if !require_authoritative_config(app) {
        return;
    }
    let settings = match app.client.get_message_bridge(bridge.wire()).await {
        Ok(view) => match view.config {
            Some(config) => MessageBridgeSettings::from_wire(config),
            None => app.settings.message_bridges.get(bridge).clone(),
        },
        Err(error) => {
            app.notify(format!("Using saved settings; {error}"));
            app.settings.message_bridges.get(bridge).clone()
        }
    };
    crate::overlay::show_message_bridge_form(app, bridge, settings);
}

#[allow(clippy::future_not_send)]
pub async fn put_message_bridge(
    app: &mut App,
    bridge: MessageBridgeKind,
    settings: MessageBridgeSettings,
) {
    if !require_authoritative_config(app) {
        return;
    }
    match app
        .client
        .put_message_bridge(bridge.wire(), settings.to_wire())
        .await
    {
        Ok(view) => {
            if matches!(app.overlay, OverlayState::MessageBridgeForm { .. }) {
                app.overlay = OverlayState::None;
            }
            app.notify_success(format!(
                "Saved {} config: {} · bridge restart required",
                bridge.label(),
                view.path
            ));
        }
        Err(error) => app.notify(format!(
            "Failed to save {} config: {}",
            bridge.label(),
            error
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_config::{HookCommand, HookEvent, KnownHookEvent};
    use rsi_common::operator_config::{CustomProviderKeyChange, CustomProviderRole};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const FAKE_KEY: &str = "fake-key-3039-tui-not-a-real-credential";

    type Log = Arc<Mutex<Vec<(String, serde_json::Value)>>>;
    type Reply = std::result::Result<serde_json::Value, serde_json::Value>;

    /// A fake daemon on a short socket path (macOS caps socket paths at 104
    /// bytes) that records each request and answers from `reply`.
    async fn app_with_daemon(
        reply: impl Fn(&str, &serde_json::Value) -> Reply + Send + Sync + 'static,
    ) -> (tempfile::TempDir, Log, App) {
        let directory = tempfile::Builder::new()
            .prefix("rsi-oc")
            .tempdir_in("/tmp")
            .expect("temp dir");
        let socket = directory.path().join("d.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind fake daemon");
        let log = Log::default();
        let recorded = Arc::clone(&log);
        let reply = Arc::new(reply);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                let reply = Arc::clone(&reply);
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                        let method = request["method"].as_str().unwrap_or_default().to_owned();
                        let params = request["params"].clone();
                        recorded
                            .lock()
                            .unwrap()
                            .push((method.clone(), params.clone()));
                        let body = match reply(&method, &params) {
                            Ok(result) => {
                                serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                            }
                            Err(data) => serde_json::json!({"jsonrpc": "2.0", "id": request["id"],
                                "error": {"code": -32602, "message": "refused", "data": data}}),
                        };
                        let _ = write.write_all(format!("{body}\n").as_bytes()).await;
                    }
                });
            }
        });
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = crate::client::DaemonClient::new(socket);
        app.client.connect().await.expect("connect fake daemon");
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        (directory, log, app)
    }

    fn summary(id: uuid::Uuid, name: &str, key_set: bool) -> serde_json::Value {
        serde_json::json!({
            "id": id, "name": name, "base_url": "https://api.example.invalid/v1",
            "default_model": "m",
            "api_key": if key_set {
                serde_json::json!({"state": "set", "fingerprint": "abc123"})
            } else {
                serde_json::json!({"state": "absent"})
            },
        })
    }

    /// Acceptance: the TUI's provider list is the daemon's `ListCustomProviders`,
    /// and the model roles are pointed at providers by id only.
    #[tokio::test]
    async fn the_tui_reads_the_daemon_provider_list_and_routes_roles_by_id() {
        let (groq, local) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let providers = serde_json::json!({"providers": [summary(groq, "Groq", true), summary(local, "Local", false)]});
        let (_dir, log, mut app) = app_with_daemon(move |method, params| match method {
            "ListCustomProviders" => Ok(providers.clone()),
            "SetCustomProviderRoute" => Ok(serde_json::json!({
                "role": params["role"], "id": params["id"], "api_key_set": !params["id"].is_null()
            })),
            other => panic!("unexpected {other}"),
        })
        .await;
        app.settings.title_model_custom_provider_id = Some(groq);
        app.settings.dream_model_custom_provider_id = Some(uuid::Uuid::new_v4()); // gone

        refresh_custom_providers(&mut app).await;

        let listed: Vec<_> = app
            .settings
            .custom_providers
            .iter()
            .map(|entry| (entry.id, entry.name.as_str(), entry.api_key_set))
            .collect();
        assert_eq!(listed, [(groq, "Groq", true), (local, "Local", false)]);
        assert_eq!(
            app.settings.custom_providers[0]
                .api_key_fingerprint
                .as_deref(),
            Some("abc123")
        );
        let routes: Vec<_> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "SetCustomProviderRoute")
            .map(|(_, params)| {
                (
                    params["role"].as_str().unwrap().to_owned(),
                    params["id"].clone(),
                )
            })
            .collect();
        assert_eq!(
            routes,
            [
                (
                    CustomProviderRole::TitleModel.as_str().to_owned(),
                    serde_json::json!(groq)
                ),
                ("memory_model_fallback".to_owned(), serde_json::Value::Null),
                ("dream_model".to_owned(), serde_json::Value::Null),
                ("prompt_compile_model".to_owned(), serde_json::Value::Null),
            ]
        );
    }

    /// The key typed into the form reaches the daemon once and the TUI keeps
    /// no copy: not in its provider list and not in what it persists.
    #[tokio::test]
    async fn a_submitted_key_goes_to_the_daemon_once_and_the_tui_keeps_none() {
        let id = uuid::Uuid::new_v4();
        let created = summary(id, "Groq", true);
        let list = serde_json::json!({"providers": [created.clone()]});
        let (_dir, log, mut app) = app_with_daemon(move |method, _| match method {
            "PutCustomProvider" => Ok(created.clone()),
            "ListCustomProviders" => Ok(list.clone()),
            other => panic!("unexpected {other}"),
        })
        .await;

        put_custom_provider(
            &mut app,
            PutCustomProviderParams {
                id: None,
                name: "Groq".into(),
                base_url: "https://api.example.invalid/v1".into(),
                default_model: "m".into(),
                api_key: CustomProviderKeyChange::Set {
                    secret: FAKE_KEY.into(),
                },
            },
        )
        .await;

        let requests = log.lock().unwrap().clone();
        assert_eq!(requests[0].0, "PutCustomProvider");
        assert_eq!(requests[0].1["api_key"]["secret"], FAKE_KEY);
        assert_eq!(
            requests
                .iter()
                .filter(|(_, params)| params.to_string().contains(FAKE_KEY))
                .count(),
            1
        );
        assert_eq!(app.settings.custom_providers[0].id, id);
        assert!(app.settings.custom_providers[0].api_key_set);
        let held = format!("{:?}", app.settings.custom_providers);
        let persisted = serde_json::to_string(&app.settings).unwrap();
        assert_eq!(
            held.matches(FAKE_KEY).count() + persisted.matches(FAKE_KEY).count(),
            0
        );
    }

    /// A stale revision replays the edit on the daemon's current hooks and
    /// opens the conflict prompt, which an overwrite answers with that
    /// revision.
    #[tokio::test]
    async fn a_stale_hooks_revision_replays_the_edit_and_opens_the_conflict_prompt() {
        let current = serde_json::json!({
            "path": "/home/op/.claude/settings.json", "exists": true, "revision": "r2",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "theirs.sh"}]}]},
        });
        let (_dir, log, mut app) = app_with_daemon(move |method, params| match method {
            "PutClaudeHooks" if params["expected_revision"] == "r1" => Err(serde_json::json!({
                "code": "claude_hooks_conflict", "current_revision": "r2"
            })),
            "ListClaudeHooks" => Ok(current.clone()),
            other => panic!("unexpected {other} {params}"),
        })
        .await;
        let edit = HookEdit::Upsert {
            editing: None,
            event: HookEvent::Known(KnownHookEvent::SessionStart),
            matcher: None,
            command: HookCommand {
                kind: "command".into(),
                command: "mine.sh".into(),
                timeout: 60,
            },
        };
        let hooks = edit.apply(ClaudeHooks::new());

        save_claude_hooks(&mut app, "r1".into(), hooks, edit.clone()).await;

        let OverlayState::HookConflictPrompt { pending } = &app.overlay else {
            panic!("expected the conflict prompt");
        };
        assert_eq!(pending.revision, "r2");
        assert_eq!(pending.edit, edit);
        let commands: Vec<_> = crate::claude_config::flatten_hooks(&pending.hooks)
            .into_iter()
            .map(|row| row.command)
            .collect();
        assert_eq!(commands, ["mine.sh", "theirs.sh"]);
        assert_eq!(app.cached_claude_hooks.as_ref().unwrap().revision, "r2");
        assert_eq!(log.lock().unwrap()[1].0, "ListClaudeHooks");
    }
}
