//! Action handlers for daemon config (feature flags).

use crate::app::App;
use crate::client::DaemonClient;
use crate::settings::{
    DaemonFeatureEntry, DaemonFeatureValue, SystemPromptPreset, model_control_mode_slug,
};
use crate::settings_staging::{SYSTEM_PROMPT_FIELD, StagedValue, Step, row_value_of_entry};
use rsi_common::daemon_config_catalog::ApplyClass;
use rsi_common::model_control::ModelControlMode;
use rsi_common::operator_config::CustomProviderRole;
use rsi_common::sandbox_storage::SandboxBuildCacheReclaimReport;

/// The restart-badge suffix for a saved-value toast (Epic M design D.5): the
/// unconditional success toast becomes explicit about when the change takes
/// effect for every non-`Live` apply class. Returns `None` for `Live` (no
/// suffix needed — the toast already reads as immediate) and fields with no
/// catalog entry (e.g. rows with no persisted daemon field, such as the
/// classifier model's picker path, which has its own toast).
fn restart_toast_suffix(field: &str) -> Option<&'static str> {
    let spec = rsi_common::daemon_config_catalog::daemon_field_spec(field)?;
    match spec.apply {
        ApplyClass::Live => None,
        ApplyClass::DaemonRestart => Some(" · applies after daemon restart"),
        ApplyClass::LiveOffRestartOn => Some(" · off now, on after daemon restart"),
        ApplyClass::PartialLive => Some(" · new launches now, resumed sessions after restart"),
        ApplyClass::NextSpawn => Some(" · applies at next spawn"),
        ApplyClass::NotApplied => Some(" · stored, not applied (see the row's summary)"),
    }
}

/// How recent a settled storage preview must be for `refresh_daemon_features`
/// to reuse it instead of asking the daemon again.
const STORAGE_STATUS_EVENT_REFRESH_MIN_AGE: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Refresh the cached daemon feature list from the daemon.
///
/// Also synchronizes the `UserSettings.system_prompt_preset` cache from
/// the daemon response (RSI-026). Launch-path read sites
/// (`overlay/prompt.rs`, `app/session_actions.rs`) depend on this cache
/// being warm before the user can launch a session.
pub async fn refresh_daemon_features(app: &mut App) {
    // Settings refreshes are optional work. They deliberately do not restart
    // bootstrap or replace its primary client/push stream: a completed
    // handshake remains launch-ready while this short-lived refresh runs.
    app.request_daemon_config_refresh();
    // Duplicate automatic refreshes coalesce in the bootstrap coordinator. A
    // preview settled within the last 30 s is kept: this action also runs for
    // every model-invocation event while a Settings panel is open, and the
    // daemon's preview is a full scan of every sandbox target (#2604).
    app.request_storage_status_refresh_if_stale(STORAGE_STATUS_EVENT_REFRESH_MIN_AGE);
    // #2384: the Cold Storage tab shows the archive's own status.
    if app.settings_state.section == crate::settings_registry::SettingsSection::ColdStorage {
        crate::cold_storage::refresh_status(app).await;
    }
}

/// Read, set, or clear one `OpenRouter` model's route from the TUI command line.
/// The model portion is stored verbatim after the daemon's dotted prefix.
#[allow(clippy::future_not_send)] // App belongs to the single-threaded event loop.
pub async fn set_openrouter_model_route(app: &mut App, args: &str) {
    let mut parts = args.split_whitespace();
    let (Some(model), route, None) = (parts.next(), parts.next(), parts.next()) else {
        app.notify("Usage: :openrouter-route <model> [codex_cli|harness|default]");
        return;
    };
    if !require_authoritative_config(app) {
        return;
    }
    let field = format!("api_route.openrouter.{model}");
    let Some(route) = route else {
        match app.client.get_daemon_config().await {
            Ok(config) => {
                let current = config[field.as_str()].as_str().unwrap_or("default");
                app.notify(format!("OpenRouter route for {model}: {current}"));
            }
            Err(error) => app.notify(format!("Failed to read OpenRouter route: {error}")),
        }
        return;
    };
    let route_value = match route {
        "codex_cli" | "harness" => serde_json::json!(route),
        "default" => serde_json::Value::Null,
        _ => {
            app.notify("Route must be codex_cli, harness, or default");
            return;
        }
    };
    match app.client.update_daemon_config(&field, route_value).await {
        Ok(()) => app.notify_success(format!("OpenRouter route for {model}: {route}")),
        Err(error) => app.notify(format!("Failed to update OpenRouter route: {error}")),
    }
}

/// #1005: read, set, or clear one provider or model override of the
/// coordinator context cap from the TUI command line. The key is `<Provider>`
/// or `<Provider>/<model>`; the daemon validates it and the percentage or token range.
#[allow(clippy::future_not_send)] // App belongs to the single-threaded event loop.
pub async fn set_coordinator_context_cap(app: &mut App, args: &str) {
    let mut parts = args.split_whitespace();
    let (Some(key), tokens, None) = (parts.next(), parts.next(), parts.next()) else {
        app.notify("Usage: :context-cap <Provider[/model]> [pct|tokens|0|default]");
        return;
    };
    if !require_authoritative_config(app) {
        return;
    }
    let field = format!("coordinator_context_cap.{key}");
    let Some(tokens) = tokens else {
        match app.client.get_daemon_config().await {
            Ok(config) => {
                let current = config[field.as_str()]
                    .as_u64()
                    .map_or_else(|| "default".to_string(), coordinator_cap_display);
                app.notify(format!("Coordinator context cap for {key}: {current}"));
            }
            Err(error) => app.notify(format!("Failed to read coordinator context cap: {error}")),
        }
        return;
    };
    let value = if tokens == "default" {
        serde_json::Value::Null
    } else if let Ok(n) = tokens.parse::<u64>() {
        serde_json::json!(n)
    } else {
        app.notify("Cap must be a percentage (1-100), a token count, 0 (off), or default");
        return;
    };
    let display = value
        .as_u64()
        .map_or_else(|| "default".to_string(), coordinator_cap_display);
    match app.client.update_daemon_config(&field, value).await {
        Ok(()) => app.notify_success(format!("Coordinator context cap for {key}: {display}")),
        Err(error) => app.notify(format!("Failed to update coordinator context cap: {error}")),
    }
}

fn coordinator_cap_display(value: u64) -> String {
    match value {
        1..=100 => format!("{value}%"),
        _ => value.to_string(),
    }
}

/// #1254: read, set, or clear one provider or model override of the worker
/// baton cap. The value is `0` (off), a percentage `1..=100` of the session's
/// context window, or an absolute token count; the daemon validates it.
#[allow(clippy::future_not_send)] // App belongs to the single-threaded event loop.
pub async fn set_worker_context_cap(app: &mut App, args: &str) {
    let mut parts = args.split_whitespace();
    let (Some(key), value, None) = (parts.next(), parts.next(), parts.next()) else {
        app.notify("Usage: :worker-context-cap <Provider[/model]> [pct|tokens|0|default]");
        return;
    };
    if !require_authoritative_config(app) {
        return;
    }
    let field = format!("worker_context_cap.{key}");
    let Some(value) = value else {
        match app.client.get_daemon_config().await {
            Ok(config) => {
                let current = config[field.as_str()]
                    .as_u64()
                    .map_or_else(|| "default".to_string(), |n| n.to_string());
                app.notify(format!("Worker context cap for {key}: {current}"));
            }
            Err(error) => app.notify(format!("Failed to read worker context cap: {error}")),
        }
        return;
    };
    let json = if value == "default" {
        serde_json::Value::Null
    } else if let Ok(n) = value.parse::<u64>() {
        serde_json::json!(n)
    } else {
        app.notify("Cap must be a percentage (1-100), a token count, 0 (off), or default");
        return;
    };
    match app.client.update_daemon_config(&field, json).await {
        Ok(()) => app.notify_success(format!("Worker context cap for {key}: {value}")),
        Err(error) => app.notify(format!("Failed to update worker context cap: {error}")),
    }
}

/// Read, set, or clear the operator launch-model allowlist from the TUI command
/// line (Issue #692). The daemon validates and enforces it on every launch path;
/// an empty list means unrestricted.
#[allow(clippy::future_not_send)] // App belongs to the single-threaded event loop.
pub async fn set_launch_model_allowlist(app: &mut App, args: &str) {
    const FIELD: &str = rsi_common::launch_allowlist::LAUNCH_MODEL_ALLOWLIST_FIELD;
    if !require_authoritative_config(app) {
        return;
    }
    let args = args.trim();
    if args.is_empty() {
        match app.client.get_daemon_config().await {
            Ok(config) => {
                let current: Vec<&str> = config[FIELD]
                    .as_array()
                    .map(|items| items.iter().filter_map(|item| item.as_str()).collect())
                    .unwrap_or_default();
                if current.is_empty() {
                    app.notify("Launch model allowlist: unrestricted");
                } else {
                    app.notify(format!("Launch model allowlist: {}", current.join(", ")));
                }
            }
            Err(error) => app.notify(format!("Failed to read launch model allowlist: {error}")),
        }
        return;
    }
    // `clear` (or none/off) is a daemon-side spelling of the empty list.
    match app
        .client
        .update_daemon_config("launch_model_allowlist", serde_json::json!(args))
        .await
    {
        Ok(()) => app.notify_success(format!("Launch model allowlist updated: {args}")),
        Err(error) => app.notify(format!("Failed to update launch model allowlist: {error}")),
    }
}

/// Write a free-text daemon field (the cold-storage archive directory,
/// #2384 / #2438, the SearXNG URL, #2393) through `UpdateDaemonConfig` and
/// mirror it into the cached feature entry. The daemon validates the value
/// (`update_field`); its refusal comes back as `Err` with the daemon's reason.
/// `""` selects the field's default.
pub async fn write_daemon_text_field(
    app: &mut App,
    field: &str,
    value: &str,
) -> Result<(), String> {
    if !app.authoritative_config_ready() {
        return Err(app.daemon_config_unavailable_reason());
    }
    app.client
        .update_daemon_config(field, serde_json::json!(value))
        .await
        .map_err(|error| error.to_string())?;
    let json = serde_json::json!({ field: value });
    DaemonFeatureEntry::update_from_json(&mut app.daemon_features, &json);
    Ok(())
}

/// Refresh the cached lifetime usage aggregate for the Settings -> Stats
/// category (T8; 1:1 clone of `refresh_daemon_features`). Tab-scoped
/// per-project filter (D1): reads `app.current_project_id` at fetch time,
/// so switching tabs and reopening Stats reflects the new tab's project.
pub async fn refresh_usage_stats(app: &mut App) {
    app.request_usage_stats_refresh();
}

/// Cycle `system_prompt_preset` to the next value and dispatch
/// `UpdateDaemonConfig` (RSI-026). On RPC success, updates the local
/// `UserSettings` cache and the `DaemonFeatureEntry` mirror so launch-path
/// reads (overlay/prompt.rs, app/session_actions.rs) and the
/// Daemon Features overlay both reflect the new value without an extra
/// `GetDaemonConfig` roundtrip.
///
/// On RPC failure: cache is unchanged, a notification fires, and the daemon's
/// stored value remains authoritative on the next `refresh_daemon_features`
/// pass.
pub async fn cycle_system_prompt_preset(app: &mut App) {
    if !require_authoritative_config(app) {
        return;
    }
    use crate::settings::cycle_preset;
    let next = cycle_preset(app.settings.system_prompt_preset);
    let slug = next.slug();

    match app
        .client
        .update_daemon_config("system_prompt_preset", serde_json::json!(slug))
        .await
    {
        Ok(()) => {
            app.settings.system_prompt_preset = next;
            // Mirror the change into the DaemonFeatures overlay so the
            // alternate cycle path reflects the new value without a
            // follow-up GetDaemonConfig.
            let json = serde_json::json!({ "system_prompt_preset": slug });
            DaemonFeatureEntry::update_from_json(&mut app.daemon_features, &json);
            app.notify_success(&format!("System prompt: {}", next.label()));
        }
        Err(e) => {
            tracing::warn!("Failed to cycle system_prompt_preset: {}", e);
            app.notify(&format!("Failed to update system prompt preset: {}", e));
        }
    }
}

/// Sync title model configuration from UserSettings to daemon RuntimeConfig.
pub async fn sync_title_model_config(app: &mut App) {
    if !app.authoritative_config_ready() {
        return;
    }
    let local = app.settings.title_model_local.clone();
    let fallback = app.settings.title_model_fallback.clone();
    let provider = app.settings.title_model_provider;
    let custom = live_custom_provider_id(app, app.settings.title_model_custom_provider_id);

    if let Err(e) = app
        .client
        .update_daemon_config("title_model_local", serde_json::json!(local))
        .await
    {
        tracing::warn!("Failed to sync title_model_local to daemon: {}", e);
    }
    if let Err(e) = app
        .client
        .update_daemon_config("title_model_fallback", serde_json::json!(fallback))
        .await
    {
        tracing::warn!("Failed to sync title_model_fallback to daemon: {}", e);
    }
    sync_config_field(app, "title_model_provider", serde_json::json!(provider)).await;
    sync_custom_provider_route(app, CustomProviderRole::TitleModel, custom).await;
}

/// Sync TUI-owned memory/dream model choices to daemon `RuntimeConfig`.
pub async fn sync_memory_model_config(app: &mut App) {
    if !app.authoritative_config_ready() {
        return;
    }
    for (field, value) in memory_model_sync_fields(&app.settings) {
        if let Err(e) = app.client.update_daemon_config(field, value).await {
            tracing::warn!("Failed to sync {} to daemon: {}", field, e);
        }
    }
    let memory =
        live_custom_provider_id(app, app.settings.memory_model_fallback_custom_provider_id);
    let dream = live_custom_provider_id(app, app.settings.dream_model_custom_provider_id);
    sync_custom_provider_route(app, CustomProviderRole::MemoryModelFallback, memory).await;
    sync_custom_provider_route(app, CustomProviderRole::DreamModel, dream).await;
}

/// The model fields the TUI owns. A custom provider's base URL and key are
/// not among them: `SetCustomProviderRoute` makes the daemon resolve both
/// (hub #3039).
fn memory_model_sync_fields(
    settings: &crate::settings::UserSettings,
) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (
            "memory_model_local",
            serde_json::json!(settings.memory_model_local),
        ),
        (
            "memory_model_fallback",
            serde_json::json!(settings.memory_model_fallback),
        ),
        (
            "memory_model_fallback_provider",
            serde_json::json!(settings.memory_model_fallback_provider),
        ),
        ("dream_model", serde_json::json!(settings.dream_model)),
        (
            "dream_model_provider",
            serde_json::json!(settings.dream_model_provider),
        ),
    ]
}

pub async fn sync_prompt_processor_config(app: &mut App) {
    if !app.authoritative_config_ready() {
        return;
    }
    let model = app.settings.prompt_processor.model.clone();
    let provider = app.settings.prompt_processor.provider;
    let custom = live_custom_provider_id(app, app.settings.prompt_processor.custom_provider_id);
    sync_config_field(app, "prompt_compile_model_local", serde_json::json!(model)).await;
    sync_config_field(
        app,
        "prompt_compile_model_provider",
        serde_json::json!(provider),
    )
    .await;
    sync_custom_provider_route(app, CustomProviderRole::PromptCompileModel, custom).await;
}

/// Point every daemon model role at the custom provider the settings select
/// (after the provider list loads at connect). The daemon keeps the
/// assignment and reapplies it itself after a restart.
pub(super) async fn sync_custom_provider_routes(app: &mut App) {
    let routes = [
        (
            CustomProviderRole::TitleModel,
            app.settings.title_model_custom_provider_id,
        ),
        (
            CustomProviderRole::MemoryModelFallback,
            app.settings.memory_model_fallback_custom_provider_id,
        ),
        (
            CustomProviderRole::DreamModel,
            app.settings.dream_model_custom_provider_id,
        ),
        (
            CustomProviderRole::PromptCompileModel,
            app.settings.prompt_processor.custom_provider_id,
        ),
    ];
    for (role, id) in routes {
        let id = live_custom_provider_id(app, id);
        sync_custom_provider_route(app, role, id).await;
    }
}

/// A selected custom provider id that the daemon still lists. A stale id
/// (deleted elsewhere) detaches the role instead of failing the sync.
fn live_custom_provider_id(app: &App, id: Option<uuid::Uuid>) -> Option<uuid::Uuid> {
    id.filter(|id| {
        app.settings
            .custom_providers
            .iter()
            .any(|entry| entry.id == *id)
    })
}

/// Point a daemon model role at a custom provider (or detach it). The daemon
/// writes the role's base URL and key from its store and vault.
async fn sync_custom_provider_route(
    app: &mut App,
    role: CustomProviderRole,
    id: Option<uuid::Uuid>,
) {
    if !app.authoritative_config_ready() {
        return;
    }
    if let Err(e) = app.client.set_custom_provider_route(role, id).await {
        tracing::warn!(
            "Failed to sync the {} provider to daemon: {}",
            role.as_str(),
            e
        );
    }
}

async fn sync_config_field(app: &mut App, field: &str, value: serde_json::Value) {
    if !app.authoritative_config_ready() {
        return;
    }
    if let Err(e) = app.client.update_daemon_config(field, value).await {
        tracing::warn!("Failed to sync {} to daemon: {}", field, e);
    }
}

/// Start one response-bearing stall-classifier mutation. The dropdown and
/// daemon-owned mirror stay unchanged until the generation-matched response is
/// applied on the main task.
pub fn sync_classifier_model_config(app: &mut App, model_id: String) -> bool {
    if !require_authoritative_config(app) {
        return false;
    }
    if app.classifier_config_pending.is_some() {
        app.notify("Classifier update is awaiting daemon acceptance");
        return false;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        app.notify_error("Classifier update failed: async runtime unavailable");
        return false;
    };

    let mut restore_dropdown = app.settings_state.model_dropdown.clone();
    restore_dropdown.open = true;

    app.classifier_config_generation = app.classifier_config_generation.saturating_add(1);
    let generation = app.classifier_config_generation;
    app.classifier_config_pending = Some(crate::app::PendingClassifierConfig {
        generation,
        model_id: model_id.clone(),
        restore_dropdown,
    });
    let socket_path = app.client.socket_path().to_path_buf();
    let tx = app.classifier_config_tx.clone();
    app.classifier_config_handle = Some(runtime.spawn(async move {
        let accepted = async {
            let mut client = DaemonClient::new(socket_path);
            client.connect().await.map_err(|error| error.to_string())?;
            client
                .update_daemon_config(
                    "stall_classifier_model",
                    serde_json::json!(model_id.clone()),
                )
                .await
                .map_err(|error| error.to_string())
        }
        .await;
        let _ = tx
            .send(crate::app::ClassifierConfigResult {
                generation,
                model_id,
                accepted,
            })
            .await;
    }));
    true
}

impl App {
    pub(crate) fn apply_classifier_config_result(
        &mut self,
        result: crate::app::ClassifierConfigResult,
    ) -> bool {
        let Some(pending) = self.classifier_config_pending.as_ref() else {
            return false;
        };
        if pending.generation != result.generation || pending.model_id != result.model_id {
            return false;
        }
        self.classifier_config_handle = None;
        let pending = self
            .classifier_config_pending
            .take()
            .expect("generation-matched classifier update");
        match result.accepted {
            Ok(()) => {
                if let Some(entry) = self
                    .daemon_features
                    .iter_mut()
                    .find(|entry| entry.field == "stall_classifier_model")
                {
                    entry.value = DaemonFeatureValue::Display(result.model_id.clone());
                }
                self.settings_state.model_dropdown.close();
                self.settings_state.active_dropdown_item = None;
                self.notify_success(format!("Classifier model: {}", result.model_id));
            }
            Err(error) => {
                self.settings_state.model_dropdown = pending.restore_dropdown;
                self.settings_state.active_dropdown_item = Some(5);
                self.notify_error(format!(
                    "Classifier update failed: {error}; previous choice preserved"
                ));
            }
        }
        true
    }
}

/// Step the daemon feature at the given index forward and send it at once.
///
/// The Settings panel no longer calls this for its switches and choices: those
/// stage and go through [`apply_pending_settings`] (#2241). It remains for the
/// action buttons (emergency stop, cache preview and reclaim) and for callers
/// outside the panel that commit a single value on the spot.
/// - `Bool` features flip their flag via RPC and update local state.
/// - `Cycle` features advance one option with the shared stepping function
///   (numeric lists clamp at their ends, enum lists wrap) and dispatch
///   `UpdateDaemonConfig` with the new JSON value.
/// - `Display` features are read-only and no-op.
pub async fn toggle_daemon_feature(app: &mut App, idx: usize) {
    if !require_authoritative_config(app) {
        return;
    }
    if let Some(entry) = app.daemon_features.get(idx) {
        match entry.field.as_str() {
            "model_control_mode" => {
                return cycle_model_control_mode(app, idx).await;
            }
            "model_control_stop_all" => {
                return emergency_stop_all(app).await;
            }
            "sandbox_build_cache_dry_run" => {
                return run_sandbox_build_cache_reclaim(app, true).await;
            }
            "sandbox_build_cache_reclaim_now" => {
                return run_sandbox_build_cache_reclaim(app, false).await;
            }
            // #2384: the Cold Storage tab's rows. Enter on the status rows
            // refreshes them and announces the state; the run row starts a pass.
            crate::cold_storage::RUN_NOW_FIELD => {
                return crate::cold_storage::run_pass(app, "stage").await;
            }
            crate::cold_storage::STATUS_FIELD | crate::cold_storage::LAST_RUN_FIELD => {
                return crate::cold_storage::cold_archive_command(app, None).await;
            }
            _ => {}
        }
    }

    // Display-only rows (and empty choice lists) have no value to step.
    let Some(live) = app.daemon_features.get(idx).and_then(row_value_of_entry) else {
        return;
    };
    let next = live.stepped(Step::Activate);
    match write_daemon_value(app, idx, next.staged()).await {
        Ok(message) => app.notify_success(&message),
        Err(error) => app.notify(&format!("Failed to toggle feature: {error}")),
    }
}

/// The text of option `staged` of the Cycle row at `idx`.
fn daemon_choice_text(app: &App, idx: usize, staged: StagedValue) -> Result<String, String> {
    let entry = app
        .daemon_features
        .get(idx)
        .ok_or("the setting is no longer listed")?;
    match (&entry.value, staged) {
        (DaemonFeatureValue::Cycle { options, .. }, StagedValue::Choice(index)) => options
            .get(index)
            .cloned()
            .ok_or_else(|| "the staged choice is no longer offered".to_string()),
        _ => Err("the staged value no longer fits this setting".to_string()),
    }
}

/// A daemon row write ready to send: what the RPC carries, and the row value
/// to cache once the daemon has accepted it.
struct PreparedWrite {
    idx: usize,
    field: String,
    label: String,
    value_json: serde_json::Value,
    new_value: DaemonFeatureValue,
}

/// Resolve `value` for the daemon feature at `idx` into its RPC form.
fn prepare_daemon_write(
    app: &App,
    idx: usize,
    value: StagedValue,
) -> Result<PreparedWrite, String> {
    let entry = app
        .daemon_features
        .get(idx)
        .ok_or("the setting is no longer listed")?;
    let (value_json, new_value) = match (&entry.value, value) {
        (DaemonFeatureValue::Bool(_), StagedValue::Bool(on)) => {
            (serde_json::json!(on), DaemonFeatureValue::Bool(on))
        }
        (DaemonFeatureValue::Cycle { options, .. }, StagedValue::Choice(index))
            if index < options.len() =>
        {
            (
                daemon_cycle_value_json(&options[index]),
                DaemonFeatureValue::Cycle {
                    options: options.clone(),
                    current: index,
                },
            )
        }
        _ => return Err("the staged value no longer fits this setting".to_string()),
    };
    Ok(PreparedWrite {
        idx,
        field: entry.field.clone(),
        label: entry.label.clone(),
        value_json,
        new_value,
    })
}

/// Move the cached row and its mirrors to a value the daemon accepted. Returns
/// the success message (with the restart suffix when the field needs one).
fn finish_daemon_write(app: &mut App, write: PreparedWrite) -> String {
    let PreparedWrite {
        idx,
        field,
        label,
        new_value,
        ..
    } = write;
    let value_str = match &new_value {
        DaemonFeatureValue::Bool(on) => if *on { "enabled" } else { "disabled" }.to_string(),
        DaemonFeatureValue::Cycle { options, current } => options
            .get(*current)
            .cloned()
            .unwrap_or_else(|| "?".to_string()),
        DaemonFeatureValue::Display(_) => String::new(),
    };
    if field == SYSTEM_PROMPT_FIELD {
        app.settings.system_prompt_preset = SystemPromptPreset::from_slug(&value_str);
    }
    if let Some(entry) = app.daemon_features.get_mut(idx) {
        entry.value = new_value;
    }
    if field == rsi_common::provider_profile::PROVIDER_PROFILE_FIELD {
        app.sync_provider_profile();
    }
    let suffix = restart_toast_suffix(&field).unwrap_or("");
    format!("{label} → {value_str}{suffix}")
}

/// Send `value` for the daemon feature at `idx` through `UpdateDaemonConfig`
/// and, once the daemon accepts it, move the cached row and its mirrors to it.
/// Returns the success message (with the restart suffix when the field needs
/// one); a rejected or failed update leaves the cache unchanged.
async fn write_daemon_value(
    app: &mut App,
    idx: usize,
    value: StagedValue,
) -> Result<String, String> {
    let write = prepare_daemon_write(app, idx, value)?;
    if let Err(error) = app
        .client
        .update_daemon_config(&write.field, write.value_json.clone())
        .await
    {
        tracing::warn!(
            "Failed to update daemon config field '{}': {}",
            write.field,
            error
        );
        return Err(error.to_string());
    }
    Ok(finish_daemon_write(app, write))
}

/// Apply every pending Settings edit (#2241, #2236). The staged local
/// preferences are committed and saved. The changed daemon rows go to the
/// daemon as ONE atomic `UpdateDaemonConfig` carrying every final value: the
/// daemon validates the set, persists it in one transaction and applies it in
/// one pass, so either every row takes effect or none does and all stay
/// pending, with the daemon's refusal (which names the field) in the error
/// toast. The model-control mode is a policy, not a config field, so it keeps
/// its own RPC.
pub async fn apply_pending_settings(app: &mut App) {
    let local = crate::settings_keys::commit_pending_local(app);
    if local > 0 {
        crate::state::PersistedState::capture(app).save();
    }
    let changes = app
        .settings_state
        .pending
        .daemon_changes(&app.daemon_features);
    app.mark_dirty();
    if changes.is_empty() {
        app.settings_state.pending.clear();
        if local == 0 {
            app.notify("No pending settings changes");
        } else {
            app.notify_success(applied_message(local, &[]));
        }
        return;
    }
    if !require_authoritative_config(app) {
        if local > 0 {
            app.notify_success(applied_message(local, &[]));
        }
        return;
    }

    let total = changes.len();
    let mut applied = local;
    let mut messages = Vec::new();
    let mut failures = Vec::new();
    let mut batch: Vec<PreparedWrite> = Vec::new();
    let mut mode_change: Option<(usize, StagedValue)> = None;
    for (idx, staged) in changes {
        if app
            .daemon_features
            .get(idx)
            .is_some_and(|entry| entry.field == "model_control_mode")
        {
            mode_change = Some((idx, staged));
            continue;
        }
        match prepare_daemon_write(app, idx, staged) {
            Ok(write) => batch.push(write),
            Err(error) => {
                let label = app
                    .daemon_features
                    .get(idx)
                    .map(|entry| entry.label.clone())
                    .unwrap_or_default();
                failures.push(format!("{label}: {error}"));
            }
        }
    }

    if !batch.is_empty() {
        let updates: Vec<(String, serde_json::Value)> = batch
            .iter()
            .map(|write| (write.field.clone(), write.value_json.clone()))
            .collect();
        match app.client.update_daemon_config_batch(&updates).await {
            Ok(()) => {
                for write in batch {
                    let field = write.field.clone();
                    messages.push(finish_daemon_write(app, write));
                    app.settings_state.pending.settle_daemon(&field);
                    applied += 1;
                }
            }
            Err(error) => {
                tracing::warn!("Failed to update daemon config fields: {error}");
                let labels: Vec<&str> = batch.iter().map(|write| write.label.as_str()).collect();
                failures.push(format!(
                    "none of {} were applied: {error}",
                    labels.join(", ")
                ));
            }
        }
    }
    if let Some((idx, staged)) = mode_change {
        let (field, label) = app
            .daemon_features
            .get(idx)
            .map(|entry| (entry.field.clone(), entry.label.clone()))
            .unwrap_or_default();
        match apply_model_control_mode(app, idx, staged).await {
            Ok(message) => {
                app.settings_state.pending.settle_daemon(&field);
                applied += 1;
                messages.push(message);
            }
            Err(error) => failures.push(format!("{label}: {error}")),
        }
    }
    app.settings_state
        .pending
        .prune_settled_daemon(&app.daemon_features);
    app.mark_dirty();
    if !messages.is_empty() || local > 0 {
        app.notify_success(applied_message(applied, &messages));
    }
    if !failures.is_empty() {
        app.notify_error(format!(
            "{} of {} daemon settings were not applied and stay pending: {}",
            total.saturating_sub(applied.saturating_sub(local)),
            total,
            failures.join("; ")
        ));
    }
}

/// The accepted model-control mode row of an apply.
async fn apply_model_control_mode(
    app: &mut App,
    idx: usize,
    staged: StagedValue,
) -> Result<String, String> {
    let slug = daemon_choice_text(app, idx, staged)?;
    let message = set_model_control_mode(app, &slug).await?;
    // Keep the row on the accepted mode even when no status snapshot is cached
    // to refresh it from.
    if let Some(entry) = app.daemon_features.get_mut(idx)
        && let (DaemonFeatureValue::Cycle { current, .. }, StagedValue::Choice(index)) =
            (&mut entry.value, staged)
    {
        *current = index;
    }
    Ok(message)
}

/// "Applied N settings" with the per-row messages when there are any; a single
/// daemon row reads as that row's own message.
fn applied_message(applied: usize, messages: &[String]) -> String {
    match (applied, messages) {
        (1, [only]) => only.clone(),
        (_, []) => format!(
            "Applied {applied} setting{}",
            if applied == 1 { "" } else { "s" }
        ),
        _ => format!("Applied {applied} settings: {}", messages.join("; ")),
    }
}

async fn run_sandbox_build_cache_reclaim(app: &mut App, dry_run: bool) {
    tracing::info!(dry_run, "Running operator sandbox build-cache action");
    let storage_generation = app.begin_direct_storage_refresh();
    // Use one short-lived connection for the action and its post-actual
    // refresh. The main TUI connection carries polling and fire-and-forget
    // traffic; isolating this operator transaction prevents an unrelated
    // pending response from delaying its truthful notification/current state.
    let mut client = DaemonClient::new(app.client.socket_path().to_path_buf());
    if let Err(error) = client.connect().await {
        app.finish_direct_storage_refresh(storage_generation, Err(error.to_string()));
        DaemonFeatureEntry::set_sandbox_storage_action_failed(&mut app.daemon_features, dry_run);
        app.notify(sandbox_cache_reclaim_failure_message(&error.to_string()));
        return;
    }
    match client.run_sandbox_build_cache_reclaim(dry_run).await {
        Ok(report) => {
            let success = sandbox_cache_reclaim_success_message(report.report(), dry_run);
            DaemonFeatureEntry::update_sandbox_storage_action(
                &mut app.daemon_features,
                report.report(),
                dry_run,
                report.v2().map(|v2| &v2.candidate_sweep),
            );
            if dry_run {
                app.finish_direct_storage_refresh(storage_generation, Ok(report.report().clone()));
                app.notify_success(&success);
                return;
            }
            match client.get_sandbox_storage_status().await {
                Ok(current) => {
                    app.finish_direct_storage_refresh(
                        storage_generation,
                        Ok(current.report().clone()),
                    );
                    app.notify_success(&success);
                }
                Err(error) => {
                    app.finish_direct_storage_refresh(storage_generation, Err(error.to_string()));
                    app.notify(&format!(
                        "{success}; current status refresh failed: {error}"
                    ));
                }
            }
        }
        Err(error) => {
            app.finish_direct_storage_refresh(storage_generation, Err(error.to_string()));
            DaemonFeatureEntry::set_sandbox_storage_action_failed(
                &mut app.daemon_features,
                dry_run,
            );
            tracing::warn!(dry_run, %error, "Sandbox build-cache reclaim RPC failed");
            app.notify(sandbox_cache_reclaim_failure_message(&error.to_string()));
        }
    }
}

pub fn require_authoritative_config(app: &mut App) -> bool {
    if app.authoritative_config_ready() {
        true
    } else {
        app.notify(app.daemon_config_unavailable_reason());
        false
    }
}

fn sandbox_cache_reclaim_success_message(
    report: &SandboxBuildCacheReclaimReport,
    dry_run: bool,
) -> String {
    if dry_run {
        return format!(
            "Preview: {} eligible of {} checked; capacity estimate unavailable",
            report.eligible_candidates, report.candidates_considered
        );
    }
    format!(
        "Reclaim: {} removed, {} staged, {} pending, {} measured free",
        report.fully_removed_count,
        report.newly_staged_count,
        report.pending_count,
        compact_binary_bytes(report.reclaimed_bytes)
    )
}

fn sandbox_cache_reclaim_failure_message(error: &str) -> String {
    format!("Sandbox cache reclaim failed: {error}")
}

fn compact_binary_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} B")
    }
}

async fn cycle_model_control_mode(app: &mut App, idx: usize) {
    let Some(live) = app.daemon_features.get(idx).and_then(row_value_of_entry) else {
        return;
    };
    let Ok(next_slug) = daemon_choice_text(app, idx, live.stepped(Step::Activate).staged()) else {
        return;
    };
    match set_model_control_mode(app, &next_slug).await {
        Ok(message) => app.notify_success(&message),
        Err(error) => app.notify(&format!("Failed to update model control mode: {error}")),
    }
}

/// Set the model-control mode through its own RPC and refresh the cached
/// status and the Settings row from the daemon's report.
async fn set_model_control_mode(app: &mut App, slug: &str) -> Result<String, String> {
    let mode = match slug {
        "pause-background" => ModelControlMode::PauseBackground,
        "deny-paid" => ModelControlMode::DenyPaid,
        "local-only" => ModelControlMode::LocalOnly,
        "stop-all" => ModelControlMode::StopAll,
        _ => ModelControlMode::Normal,
    };

    match app.client.update_model_control_policy(mode, false).await {
        Ok(report) => {
            if let Some(status) = app.cached_model_control_status.as_mut() {
                status.mode = report.current_mode;
                status.mode_updated_at = Some(report.updated_at.clone());
                status.circuit_state = model_control_mode_slug(report.current_mode).to_string();
            }
            if let Some(status) = app.cached_model_control_status.as_ref() {
                DaemonFeatureEntry::update_model_control(&mut app.daemon_features, status);
            }
            refresh_usage_stats(app).await;
            Ok(format!(
                "Model control mode: {}",
                model_control_mode_slug(report.current_mode)
            ))
        }
        Err(e) => {
            tracing::warn!("Failed to update model control mode: {}", e);
            Err(e.to_string())
        }
    }
}

pub(crate) async fn emergency_stop_all(app: &mut App) {
    match app
        .client
        .update_model_control_policy(ModelControlMode::StopAll, true)
        .await
    {
        Ok(report) => {
            let cancelled = report.cancelled_invocations.len();
            let interrupted = report.interrupted_sessions.len();
            for session_id in &report.interrupted_sessions {
                app.operator_pauses
                    .insert(*session_id, crate::client::OperatorPauseLevel::Hard);
            }
            app.needs_redraw = true;
            app.notify_success(&format!(
                "Emergency stop applied: {} cancelled, {} interrupted",
                cancelled, interrupted
            ));
            refresh_usage_stats(app).await;
        }
        Err(e) => {
            tracing::warn!("Failed to apply emergency stop: {}", e);
            app.notify(&format!("Failed to apply emergency stop: {}", e));
        }
    }
}

pub(crate) async fn cancel_model_invocation(app: &mut App, invocation_id: uuid::Uuid) {
    match app.client.cancel_model_invocation(invocation_id).await {
        Ok(report) => {
            let short_id = &report.invocation_id.to_string()[..8];
            if report.cancelled {
                app.notify_success(&format!(
                    "Invocation {} cancelled via {}",
                    short_id, report.mechanism
                ));
            } else {
                app.notify_success(&format!(
                    "Invocation {} unchanged: {}",
                    short_id, report.message
                ));
            }
            refresh_usage_stats(app).await;
        }
        Err(e) => {
            tracing::warn!("Failed to cancel model invocation {}: {}", invocation_id, e);
            app.notify(&format!("Failed to cancel model invocation: {}", e));
        }
    }
}

/// Delete the budget policy at `idx` in the current cached snapshot. Sends
/// the REMAINING policies list with `replace_policies: true` — the daemon's
/// `UpdateModelControlPolicy` RPC has no single-policy-delete verb, so
/// sending the authoritative full remaining list is the only way to
/// actually remove one.
pub(crate) async fn delete_model_budget_policy(app: &mut App, idx: usize) {
    let mode = app
        .cached_model_control_status
        .as_ref()
        .map_or(ModelControlMode::Normal, |status| status.mode);
    let mut policies = app
        .cached_model_control_status
        .as_ref()
        .map(|status| status.policies.clone())
        .unwrap_or_default();

    if idx >= policies.len() {
        tracing::warn!(
            "delete_model_budget_policy: index {} out of bounds ({} policies) — cache stale, no-op",
            idx,
            policies.len()
        );
        app.notify("Budget policy list changed — please retry");
        return;
    }
    let removed = policies.remove(idx);

    match app
        .client
        .update_model_budget_policies(mode, policies)
        .await
    {
        Ok(_report) => {
            refresh_usage_stats(app).await;
            let scope_kind = format!("{:?}", removed.scope_kind).to_ascii_lowercase();
            let scope_id = removed.scope_id.as_deref().unwrap_or("*");
            app.notify_success(format!("Deleted budget policy: {scope_kind}/{scope_id}"));
        }
        Err(e) => {
            tracing::warn!("Failed to delete budget policy: {}", e);
            app.notify(format!("Failed to delete budget policy: {e}"));
        }
    }
}

/// Add or update a budget policy against the current cached snapshot, then
/// push the full updated list with `replace_policies: true`.
pub(crate) async fn submit_model_budget_policy(
    app: &mut App,
    policy: rsi_common::model_control::ModelBudgetPolicy,
    editing_index: Option<usize>,
) {
    let mode = app
        .cached_model_control_status
        .as_ref()
        .map_or(ModelControlMode::Normal, |status| status.mode);
    let mut policies = app
        .cached_model_control_status
        .as_ref()
        .map(|status| status.policies.clone())
        .unwrap_or_default();

    let scope_summary = format!(
        "{}/{}",
        format!("{:?}", policy.scope_kind).to_ascii_lowercase(),
        policy.scope_id.as_deref().unwrap_or("*")
    );

    match editing_index {
        Some(idx) if idx < policies.len() => {
            policies[idx] = policy;
        }
        Some(idx) => {
            tracing::warn!(
                "submit_model_budget_policy: editing index {} out of bounds ({} policies) — appending instead",
                idx,
                policies.len()
            );
            policies.push(policy);
        }
        None => {
            policies.push(policy);
        }
    }

    match app
        .client
        .update_model_budget_policies(mode, policies)
        .await
    {
        Ok(_report) => {
            refresh_usage_stats(app).await;
            app.notify_success(format!("Budget policy saved: {scope_summary}"));
        }
        Err(e) => {
            tracing::warn!("Failed to save budget policy: {}", e);
            app.notify(format!("Failed to save budget policy: {e}"));
        }
    }
}

fn daemon_cycle_value_json(new_value: &str) -> serde_json::Value {
    if new_value == "Default" {
        serde_json::Value::Null
    } else if let Ok(n) = new_value.parse::<i64>() {
        serde_json::json!(n)
    } else if let Ok(f) = new_value.parse::<f64>() {
        serde_json::json!(f)
    } else {
        serde_json::json!(new_value)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn coordinator_cap_display_identifies_percentage_values() {
        assert_eq!(super::coordinator_cap_display(1), "1%");
        assert_eq!(super::coordinator_cap_display(60), "60%");
        assert_eq!(super::coordinator_cap_display(100), "100%");
        assert_eq!(super::coordinator_cap_display(0), "0");
        assert_eq!(super::coordinator_cap_display(200_000), "200000");
    }

    use super::{
        daemon_cycle_value_json, refresh_daemon_features, refresh_usage_stats,
        sandbox_cache_reclaim_failure_message, sandbox_cache_reclaim_success_message,
        set_launch_model_allowlist, toggle_daemon_feature,
    };
    use crate::settings::DaemonFeatureValue;
    use rsi_common::sandbox_storage::{
        SandboxBuildCacheReclaimConfig, SandboxBuildCacheReclaimReport,
        SandboxBuildCacheReclaimStopReason, SandboxFilesystemStats,
    };

    #[test]
    fn memory_model_sync_key_set_equals_tui_owned_model_fields() {
        let fields: std::collections::BTreeSet<_> =
            super::memory_model_sync_fields(&crate::settings::UserSettings::default())
                .into_iter()
                .map(|(field, _)| field)
                .collect();
        let expected: std::collections::BTreeSet<_> = [
            "memory_model_local",
            "memory_model_fallback",
            "memory_model_fallback_provider",
            "dream_model",
            "dream_model_provider",
        ]
        .into_iter()
        .collect();
        assert_eq!(fields, expected);
    }

    #[tokio::test]
    async fn codegraph_indexing_toggle_sends_bool_rpc_and_updates_row() {
        use crate::client::DaemonClient;
        use crate::settings::DaemonFeatureValue;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            for expected in [true, false] {
                let request: serde_json::Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(request["method"], "UpdateDaemonConfig");
                assert_eq!(request["params"]["field"], "codegraph_indexing_enabled");
                assert_eq!(request["params"]["value"], expected);
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": {"ok": true},
                });
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = DaemonClient::new(socket);
        app.client.connect().await.unwrap();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        let index = app
            .daemon_features
            .iter()
            .position(|entry| entry.field == "codegraph_indexing_enabled")
            .unwrap();
        for expected in [true, false] {
            toggle_daemon_feature(&mut app, index).await;
            assert!(matches!(
                app.daemon_features[index].value,
                DaemonFeatureValue::Bool(value) if value == expected
            ));
        }
        server.await.unwrap();
    }

    /// Shared fake-daemon harness for the two tests below: accepts one
    /// connection on the (already-bound) `listener`, reads exactly `count`
    /// JSON-RPC request lines, replies `{"ok": true}` to each, and returns
    /// the captured `(field, value)` pairs in request order. The caller
    /// binds `listener` synchronously before spawning this as a task and
    /// before calling `app.client.connect()`, so the socket file exists
    /// before the client dials it — binding inside this async fn would race
    /// the client against task scheduling. Factored out so the
    /// accept/read/reply plumbing exists once instead of once per test, and
    /// every fallible step uses a named `let-else` panic (the pattern
    /// `action_handler/session.rs`'s fake-daemon tests use) instead of
    /// `unwrap()`/`expect()`.
    async fn respond_to_update_daemon_config(
        listener: tokio::net::UnixListener,
        count: usize,
    ) -> Vec<(String, serde_json::Value)> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let Ok((stream, _)) = listener.accept().await else {
            panic!("fake daemon client never connected");
        };
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let mut captured = Vec::new();
        let mut seen = 0;
        while seen < count {
            let Ok(Some(line)) = lines.next_line().await else {
                break;
            };
            seen += 1;
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                panic!("fake daemon request is not valid JSON: {line}");
            };
            assert_eq!(request["method"], "UpdateDaemonConfig");
            // One request is either a single `field`/`value` or the atomic
            // `updates` list the Settings page applies with (#2236); both are
            // recorded field by field.
            if let Some(updates) = request["params"]["updates"].as_array() {
                for update in updates {
                    let Some(field) = update["field"].as_str() else {
                        panic!("every field update carries a string field: {request}");
                    };
                    captured.push((field.to_string(), update["value"].clone()));
                }
            } else {
                let Some(field) = request["params"]["field"].as_str() else {
                    panic!("UpdateDaemonConfig request carries a string field: {request}");
                };
                captured.push((field.to_string(), request["params"]["value"].clone()));
            }
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"ok": true},
            });
            let Ok(()) = write.write_all(format!("{response}\n").as_bytes()).await else {
                panic!("failed to write fake daemon response");
            };
        }
        captured
    }

    /// Issue #692: `:launch-allow <models>` writes the operator allowlist through
    /// `UpdateDaemonConfig`, and `clear` writes the empty-list spelling.
    #[tokio::test]
    async fn launch_allow_command_writes_the_allowlist_field() {
        use crate::client::DaemonClient;

        let Ok(directory) = tempfile::tempdir() else {
            panic!("failed to create temp dir for fake daemon socket");
        };
        let socket = directory.path().join("daemon.sock");
        let Ok(listener) = tokio::net::UnixListener::bind(&socket) else {
            panic!("failed to bind fake daemon socket at {socket:?}");
        };
        let server = tokio::spawn(respond_to_update_daemon_config(listener, 2));

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = DaemonClient::new(socket);
        let Ok(()) = app.client.connect().await else {
            panic!("fake daemon connect failed");
        };
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        set_launch_model_allowlist(&mut app, " gpt-6-sol,claude-opus-5-5 ").await;
        set_launch_model_allowlist(&mut app, "clear").await;

        let Ok(captured) = server.await else {
            panic!("fake daemon task failed");
        };
        assert_eq!(
            captured,
            vec![
                (
                    "launch_model_allowlist".to_string(),
                    serde_json::json!("gpt-6-sol,claude-opus-5-5")
                ),
                (
                    "launch_model_allowlist".to_string(),
                    serde_json::json!("clear")
                ),
            ]
        );
    }

    /// (c) acceptance: `issue35_fields_emit_update_daemon_config_with_their_field`.
    /// The 8 Issue #35 gap fields (design D.4) now have real settings rows,
    /// wired onto the same generic `toggle_daemon_feature` RPC path as every
    /// other daemon-features row. `retry_max_default` is intentionally
    /// excluded — review 39917a88 (slice (a) fix round 1) made it a
    /// `ReadOnly`/`NotApplied` row precisely so it is NOT toggleable; its
    /// read-only guarantee is covered by
    /// `settings_registry::tests::max_retries_row_states_default_launches_get_zero_retries`.
    #[tokio::test]
    async fn issue35_fields_emit_update_daemon_config_with_their_field() {
        use crate::client::DaemonClient;

        let fields = [
            "retry_max_backoff_ms",
            "dream_idle_secs",
            "recursive_dag_recovery_controls_enabled",
            "recursive_dag_scheduler_controls_enabled",
            "recursive_dag_cancellation_controls_enabled",
            "recursive_dag_live_scheduler_control_enabled",
            "recursive_dag_run_lease_ttl_ms",
            "recursive_dag_max_concurrent_graphs",
        ];

        let Ok(directory) = tempfile::tempdir() else {
            panic!("failed to create temp dir for fake daemon socket");
        };
        let socket = directory.path().join("daemon.sock");
        let Ok(listener) = tokio::net::UnixListener::bind(&socket) else {
            panic!("failed to bind fake daemon socket at {socket:?}");
        };
        let server = tokio::spawn(respond_to_update_daemon_config(listener, fields.len()));

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = DaemonClient::new(socket);
        let Ok(()) = app.client.connect().await else {
            panic!("failed to connect to fake daemon socket");
        };
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;

        for field in fields {
            let Some(index) = app
                .daemon_features
                .iter()
                .position(|entry| entry.field == field)
            else {
                panic!("{field} has a daemon_features entry");
            };
            toggle_daemon_feature(&mut app, index).await;
        }

        let Ok(captured) = server.await else {
            panic!("fake daemon server task panicked or was cancelled");
        };
        let captured_fields: Vec<&str> = captured.iter().map(|(f, _)| f.as_str()).collect();
        for field in fields {
            assert!(
                captured_fields.contains(&field),
                "{field} sent UpdateDaemonConfig"
            );
        }
    }

    /// #3141 (f): both per-provider route fallback rows toggle through the
    /// generic daemon-feature path and send their own field with the flipped
    /// bool (OpenRouter off -> on, Bedrock on -> off).
    #[tokio::test]
    async fn per_provider_route_fallback_rows_toggle_and_send_their_field() {
        use crate::client::DaemonClient;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(respond_to_update_daemon_config(listener, 2));
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = DaemonClient::new(socket);
        app.client.connect().await.unwrap();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        for field in [
            "api_route.fallback.openrouter",
            "api_route.fallback.bedrock",
        ] {
            let index = app
                .daemon_features
                .iter()
                .position(|entry| entry.field == field)
                .unwrap();
            toggle_daemon_feature(&mut app, index).await;
        }
        let captured = server.await.unwrap();
        assert_eq!(
            captured,
            vec![
                (
                    "api_route.fallback.openrouter".to_string(),
                    serde_json::json!(true)
                ),
                (
                    "api_route.fallback.bedrock".to_string(),
                    serde_json::json!(false)
                ),
            ]
        );
    }

    #[tokio::test]
    async fn completed_transcript_cache_setting_sends_zero_cap() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(respond_to_update_daemon_config(listener, 1));
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = crate::client::DaemonClient::new(socket);
        app.client.connect().await.unwrap();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        let index = app
            .daemon_features
            .iter()
            .position(|entry| entry.field == "completed_transcript_cache_max_bytes")
            .unwrap();
        // Step the cap down to its "0" option and apply: numeric rows clamp at
        // their ends, so zero is reached by stepping back rather than by a
        // wrap from the maximum.
        let DaemonFeatureValue::Cycle { options, current } = &mut app.daemon_features[index].value
        else {
            panic!("completed transcript cap has a cycle editor");
        };
        assert_eq!(options.first().map(String::as_str), Some("0"));
        *current = 1;
        app.settings_state.pending.stage_daemon(
            "completed_transcript_cache_max_bytes",
            crate::settings_staging::StagedValue::Choice(1),
            crate::settings_staging::StagedValue::Choice(0),
        );
        super::apply_pending_settings(&mut app).await;
        let captured = server.await.unwrap();
        assert_eq!(
            captured,
            vec![(
                "completed_transcript_cache_max_bytes".to_string(),
                serde_json::json!(0)
            )]
        );
    }

    /// (c) acceptance: `restart_update_shows_restart_toast`. A
    /// `DaemonRestart`-classed field's success toast names the restart
    /// requirement (D.5), instead of the unconditional "field → value" text
    /// every other field still gets.
    #[tokio::test]
    async fn restart_update_shows_restart_toast() {
        use crate::client::DaemonClient;

        let Ok(directory) = tempfile::tempdir() else {
            panic!("failed to create temp dir for fake daemon socket");
        };
        let socket = directory.path().join("daemon.sock");
        let Ok(listener) = tokio::net::UnixListener::bind(&socket) else {
            panic!("failed to bind fake daemon socket at {socket:?}");
        };
        let server = tokio::spawn(respond_to_update_daemon_config(listener, 1));

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = DaemonClient::new(socket);
        let Ok(()) = app.client.connect().await else {
            panic!("failed to connect to fake daemon socket");
        };
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        let Some(index) = app
            .daemon_features
            .iter()
            .position(|entry| entry.field == "queue_enabled")
        else {
            panic!("queue_enabled entry present");
        };

        toggle_daemon_feature(&mut app, index).await;
        let Ok(captured) = server.await else {
            panic!("fake daemon server task panicked or was cancelled");
        };
        assert_eq!(
            captured,
            vec![("queue_enabled".to_string(), serde_json::json!(true))]
        );

        assert!(
            app.notifications
                .back()
                .is_some_and(|n| n.message.contains("applies after daemon restart")),
            "toast names the restart requirement: {:?}",
            app.notifications.back()
        );
    }

    fn report() -> SandboxBuildCacheReclaimReport {
        let filesystem = SandboxFilesystemStats {
            total_bytes: 100,
            available_bytes: 40,
            used_bytes: 60,
            used_percent: 60,
        };
        SandboxBuildCacheReclaimReport {
            version: 1,
            dry_run: false,
            enabled: true,
            config: SandboxBuildCacheReclaimConfig {
                enabled: true,
                ttl_secs: 21_600,
                interval_secs: 3_600,
                high_watermark_pct: 85,
                low_watermark_pct: 75,
                max_candidates: 64,
            },
            pressure_active_before: false,
            pressure_active_after: false,
            filesystem_before: filesystem,
            filesystem_after: filesystem,
            candidates_considered: 3,
            eligible_candidates: 3,
            skip_counts: std::collections::BTreeMap::new(),
            would_reclaim_count: 0,
            would_reclaim_bytes: 0,
            staged_count: 2,
            staged_bytes: 2 * 1024 * 1024,
            newly_staged_count: 2,
            newly_staged_bytes: 2 * 1024 * 1024,
            recovered_count: 0,
            recovered_bytes: 0,
            pending_count: 0,
            pending_bytes: 0,
            fully_removed_count: 2,
            reclaimed_bytes: 1024 * 1024,
            stopped_at_low_watermark: false,
            candidate_budget_exhausted: false,
            stop_reason: SandboxBuildCacheReclaimStopReason::Completed,
        }
    }

    #[test]
    fn cycle_value_json_preserves_strings_for_enums() {
        assert_eq!(
            daemon_cycle_value_json("workspace-write"),
            serde_json::json!("workspace-write")
        );
    }

    #[test]
    fn cycle_value_json_parses_integer_numbers() {
        assert_eq!(daemon_cycle_value_json("600"), serde_json::json!(600));
    }

    #[test]
    fn cycle_value_json_parses_float_numbers() {
        assert_eq!(daemon_cycle_value_json("0.7"), serde_json::json!(0.7));
    }

    #[test]
    fn sandbox_build_cache_action_messages_distinguish_preview_success_and_failure() {
        let report = report();
        assert_eq!(
            sandbox_cache_reclaim_success_message(&report, true),
            "Preview: 3 eligible of 3 checked; capacity estimate unavailable"
        );
        assert_eq!(
            sandbox_cache_reclaim_success_message(&report, false),
            "Reclaim: 2 removed, 2 staged, 0 pending, 1.0 MiB measured free"
        );
        assert_eq!(
            sandbox_cache_reclaim_failure_message("transport unavailable"),
            "Sandbox cache reclaim failed: transport unavailable"
        );

        let mut partial = report;
        partial.fully_removed_count = 1;
        partial.pending_count = 1;
        partial.pending_bytes = 512 * 1024;
        partial.reclaimed_bytes = 0;
        assert_eq!(
            sandbox_cache_reclaim_success_message(&partial, false),
            "Reclaim: 1 removed, 2 staged, 1 pending, 0 B measured free"
        );

        let empty = SandboxBuildCacheReclaimReport {
            candidates_considered: 0,
            eligible_candidates: 0,
            ..partial
        };
        assert_eq!(
            sandbox_cache_reclaim_success_message(&empty, true),
            "Preview: 0 eligible of 0 checked; capacity estimate unavailable"
        );
    }

    #[tokio::test]
    async fn settings_refreshes_do_not_restart_or_withdraw_bootstrap_readiness() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;

        refresh_usage_stats(&mut app).await;
        refresh_daemon_features(&mut app).await;

        assert!(!app.bootstrap.handshake_in_flight());
        assert!(app.authoritative_config_ready());
        assert!(matches!(
            app.bootstrap.storage_status,
            crate::app::bootstrap::SandboxStorageStatus::RefreshingUnknown
        ));
        assert!(
            !app.storage_refresh_in_flight(),
            "storage remains automatically owned but waits for session admission"
        );
    }

    // -- #2241: edits stage locally and apply once -----------------------------

    type RecordedRequests = std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>;

    /// A fake daemon that records every `(method, params)` it is sent and
    /// answers `{"ok": true}`, until the client hangs up.
    async fn recording_daemon(listener: tokio::net::UnixListener, log: RecordedRequests) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                return;
            };
            if let Ok(mut log) = log.lock() {
                log.push((
                    request["method"].as_str().unwrap_or_default().to_string(),
                    request["params"].clone(),
                ));
            }
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"ok": true},
            });
            if write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    }

    fn requests(log: &RecordedRequests) -> Vec<(String, serde_json::Value)> {
        let Ok(log) = log.lock() else {
            panic!("request log lock");
        };
        log.clone()
    }

    /// A connected Settings app on the daemon row `field` of `section`, with a
    /// fake daemon recording what it is sent.
    async fn settings_app_with_recording_daemon(
        section: crate::settings_registry::SettingsSection,
        field: &str,
    ) -> (tempfile::TempDir, RecordedRequests, crate::app::App) {
        let Ok(directory) = tempfile::tempdir() else {
            panic!("failed to create temp dir for fake daemon socket");
        };
        let socket = directory.path().join("daemon.sock");
        let Ok(listener) = tokio::net::UnixListener::bind(&socket) else {
            panic!("failed to bind fake daemon socket at {socket:?}");
        };
        let log = RecordedRequests::default();
        tokio::spawn(recording_daemon(listener, log.clone()));

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = crate::client::DaemonClient::new(socket);
        let Ok(()) = app.client.connect().await else {
            panic!("fake daemon connect failed");
        };
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        let focused = app.active_tab().focused_pane;
        let Some(pane) = app.active_tab_mut().layout.find_pane_mut(focused) else {
            panic!("focused pane exists");
        };
        *pane = crate::types::Pane::Settings;
        select_daemon_row(&mut app, section, field);
        (directory, log, app)
    }

    fn select_daemon_row(
        app: &mut crate::app::App,
        section: crate::settings_registry::SettingsSection,
        field: &str,
    ) {
        let Some(row) = crate::settings_keys::daemon_feature_rows_for_section(app, section)
            .iter()
            .position(|(_, vec_idx)| app.daemon_features[*vec_idx].field == field)
        else {
            panic!("{field} is a row of {section:?}");
        };
        app.settings_state.section = section;
        app.settings_state.focus = crate::types::SettingsFocus::Items;
        app.settings_state.selected_index = row;
    }

    fn press_keys(app: &mut crate::app::App, keys: &[crossterm::event::KeyCode]) {
        for code in keys {
            let key = crossterm::event::KeyEvent::new(*code, crossterm::event::KeyModifiers::NONE);
            assert!(crate::settings_keys::handle_settings_key(app, key));
        }
    }

    /// Acceptance (b): N keypresses on a row produce zero daemon updates, and
    /// apply produces exactly one update carrying the final value.
    #[tokio::test]
    async fn many_keypresses_send_nothing_and_apply_sends_one_update_with_the_final_value() {
        use crossterm::event::KeyCode::{Char, Enter};

        let field = "sandbox_max_source_roots";
        let (_directory, log, mut app) = settings_app_with_recording_daemon(
            crate::settings_registry::SettingsSection::SandboxStorage,
            field,
        )
        .await;
        // The live row sits at 4096 (index 3 of 512..16384): 3 -> 0 (clamped),
        // then up to 1, 2, 3 and back down to 2 = 2048.
        press_keys(
            &mut app,
            &[
                Char('-'),
                Char('-'),
                Char('-'),
                Char('-'),
                Char('+'),
                Enter,
                Char('+'),
                Char('-'),
            ],
        );
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert!(
            app.pending_lc_actions.is_empty(),
            "no key queued a daemon action: {:?}",
            app.pending_lc_actions
        );
        assert!(
            requests(&log).is_empty(),
            "eight keypresses reached the daemon: {:?}",
            requests(&log)
        );
        assert_eq!(crate::settings_staging::pending_count(&app), 1);

        super::apply_pending_settings(&mut app).await;

        assert_eq!(
            requests(&log),
            vec![(
                "UpdateDaemonConfig".to_string(),
                serde_json::json!({"updates": [{"field": field, "value": 2048}]})
            )],
            "exactly one update, holding the final value after eight presses"
        );
        assert_eq!(crate::settings_staging::pending_count(&app), 0);
        assert!(app.settings_state.pending.is_empty());
        let current = app
            .daemon_features
            .iter()
            .find(|entry| entry.field == field)
            .map(|entry| entry.value.clone());
        assert!(
            matches!(current, Some(DaemonFeatureValue::Cycle { current: 2, .. })),
            "the live row moved to the applied value: {current:?}"
        );
    }

    /// Several rows are applied by one action: ONE daemon update carries every
    /// changed field with its final value, and nothing else is sent (#2236).
    #[tokio::test]
    async fn apply_sends_every_changed_field_in_one_daemon_update() {
        use crate::settings_registry::SettingsSection;
        use crossterm::event::KeyCode::{Char, Enter};

        let (_directory, log, mut app) = settings_app_with_recording_daemon(
            SettingsSection::SandboxStorage,
            "sandbox_max_source_roots",
        )
        .await;
        // 4096 -> 2048 -> 1024 -> 512, then back up one step: 1024.
        press_keys(&mut app, &[Char('-'), Char('-'), Char('-'), Char('+')]);
        select_daemon_row(&mut app, SettingsSection::RetriesRecovery, "retry_enabled");
        press_keys(&mut app, &[Enter, Enter, Enter]);
        // A third row that is stepped away and back is not an edit at all.
        select_daemon_row(&mut app, SettingsSection::RetriesRecovery, "retry_on_stall");
        press_keys(&mut app, &[Char('+'), Char('-')]);
        assert_eq!(crate::settings_staging::pending_count(&app), 2);
        assert!(requests(&log).is_empty());

        super::apply_pending_settings(&mut app).await;

        let sent = requests(&log);
        assert_eq!(
            sent.len(),
            1,
            "one daemon update for the whole apply: {sent:?}"
        );
        assert_eq!(sent[0].0, "UpdateDaemonConfig");
        let Some(listed) = sent[0].1["updates"].as_array() else {
            panic!("the update carries a list of field updates: {:?}", sent[0]);
        };
        let mut updates: Vec<(String, serde_json::Value)> = listed
            .iter()
            .map(|update| {
                (
                    update["field"].as_str().unwrap_or_default().to_string(),
                    update["value"].clone(),
                )
            })
            .collect();
        updates.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            updates,
            vec![
                ("retry_enabled".to_string(), serde_json::json!(true)),
                (
                    "sandbox_max_source_roots".to_string(),
                    serde_json::json!(1024)
                ),
            ]
        );
        assert!(
            app.notifications
                .back()
                .is_some_and(|n| n.message.starts_with("Applied 2 settings")),
            "one summary toast: {:?}",
            app.notifications.back()
        );
        assert_eq!(crate::settings_staging::pending_count(&app), 0);
    }

    /// Esc leaves the panel and drops every staged edit: nothing is sent, now
    /// or later.
    #[tokio::test]
    async fn escape_discards_staged_edits_so_nothing_is_ever_sent() {
        use crossterm::event::KeyCode::{Char, Esc};

        let field = "sandbox_max_source_roots";
        let (_directory, log, mut app) = settings_app_with_recording_daemon(
            crate::settings_registry::SettingsSection::SandboxStorage,
            field,
        )
        .await;
        press_keys(&mut app, &[Char('-'), Char('-')]);
        assert_eq!(crate::settings_staging::pending_count(&app), 1);
        press_keys(&mut app, &[Esc]);
        assert_eq!(crate::settings_staging::pending_count(&app), 0);

        super::apply_pending_settings(&mut app).await;
        assert!(requests(&log).is_empty(), "{:?}", requests(&log));
        assert!(
            app.notifications
                .back()
                .is_some_and(|n| n.message.contains("No pending settings changes"))
        );
    }

    /// A row the daemon cannot take stays pending while the others apply.
    #[tokio::test]
    async fn a_failed_row_stays_pending_and_is_named() {
        use crossterm::event::KeyCode::Char;

        let field = "sandbox_max_source_roots";
        let (_directory, _log, mut app) = settings_app_with_recording_daemon(
            crate::settings_registry::SettingsSection::SandboxStorage,
            field,
        )
        .await;
        press_keys(&mut app, &[Char('-')]);
        // The daemon goes away between staging and applying.
        app.client = crate::client::DaemonClient::new(std::path::PathBuf::from(
            "/nonexistent/rsi-daemon.sock",
        ));

        super::apply_pending_settings(&mut app).await;

        assert_eq!(
            crate::settings_staging::pending_count(&app),
            1,
            "the refused row is still pending"
        );
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("Maximum sandbox roots")),
            "the failure names the row: {:?}",
            app.notifications
        );
    }
}
