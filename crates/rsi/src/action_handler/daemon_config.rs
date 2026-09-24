//! Action handlers for daemon config (feature flags).

use crate::app::App;
use crate::client::DaemonClient;
use crate::settings::{
    DaemonFeatureEntry, DaemonFeatureValue, SystemPromptPreset, model_control_mode_slug,
};
use rsi_common::daemon_config_catalog::ApplyClass;
use rsi_common::model_control::ModelControlMode;
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
    // A fresh preview keeps its existing RPC semantics. Duplicate automatic
    // refreshes coalesce in the bootstrap coordinator.
    app.request_storage_status_refresh();
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
    let custom = app
        .settings
        .title_model_custom_provider_id
        .and_then(|id| {
            app.settings
                .custom_providers
                .iter()
                .find(|entry| entry.id == id)
        })
        .map(|entry| (entry.base_url.clone(), entry.api_key.clone()));

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
    sync_config_field(
        app,
        "title_model_base_url",
        serde_json::json!(custom.as_ref().map(|entry| entry.0.clone())),
    )
    .await;
    sync_config_field(
        app,
        "title_model_api_key",
        serde_json::json!(custom.as_ref().map(|entry| entry.1.clone())),
    )
    .await;
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
}

fn memory_model_sync_fields(
    settings: &crate::settings::UserSettings,
) -> Vec<(&'static str, serde_json::Value)> {
    let memory_custom = settings
        .memory_model_fallback_custom_provider_id
        .and_then(|id| {
            settings
                .custom_providers
                .iter()
                .find(|entry| entry.id == id)
        });
    let dream_custom = settings.dream_model_custom_provider_id.and_then(|id| {
        settings
            .custom_providers
            .iter()
            .find(|entry| entry.id == id)
    });
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
        (
            "memory_model_fallback_base_url",
            serde_json::json!(memory_custom.map(|entry| entry.base_url.clone())),
        ),
        (
            "memory_model_fallback_api_key",
            serde_json::json!(memory_custom.map(|entry| entry.api_key.clone())),
        ),
        ("dream_model", serde_json::json!(settings.dream_model)),
        (
            "dream_model_provider",
            serde_json::json!(settings.dream_model_provider),
        ),
        (
            "dream_model_base_url",
            serde_json::json!(dream_custom.map(|entry| entry.base_url.clone())),
        ),
        (
            "dream_model_api_key",
            serde_json::json!(dream_custom.map(|entry| entry.api_key.clone())),
        ),
    ]
}

pub async fn sync_prompt_processor_config(app: &mut App) {
    if !app.authoritative_config_ready() {
        return;
    }
    let model = app.settings.prompt_processor.model.clone();
    let provider = app.settings.prompt_processor.provider;
    let base_url = app.settings.prompt_processor.custom_base_url.clone();
    let api_key = app.settings.prompt_processor.custom_api_key.clone();
    sync_config_field(app, "prompt_compile_model_local", serde_json::json!(model)).await;
    sync_config_field(
        app,
        "prompt_compile_model_provider",
        serde_json::json!(provider),
    )
    .await;
    sync_config_field(
        app,
        "prompt_compile_model_base_url",
        serde_json::json!(base_url),
    )
    .await;
    sync_config_field(
        app,
        "prompt_compile_model_api_key",
        serde_json::json!(api_key),
    )
    .await;
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

/// Toggle (or cycle) the daemon feature at the given index.
/// - `Bool` features flip their flag via RPC and update local state.
/// - `Cycle` features advance to the next option in their list (wrapping)
///   and dispatch `UpdateDaemonConfig` with the new JSON value.
/// - `Display` features are read-only and no-op on Enter.
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
            _ => {}
        }
    }

    /// The pending mutation, decided before the RPC call so the immutable
    /// borrow of `app.daemon_features[idx]` is released before we hit the
    /// async `app.client` borrow.
    enum Update {
        Bool(String, bool),
        Cycle {
            field: String,
            new_value: String,
            options: Vec<String>,
            new_current: usize,
        },
    }

    let update = {
        let entry = match app.daemon_features.get(idx) {
            Some(e) => e,
            None => return,
        };
        match &entry.value {
            DaemonFeatureValue::Bool(current) => Update::Bool(entry.field.clone(), !current),
            DaemonFeatureValue::Display(_) => return, // display-only, not toggleable
            DaemonFeatureValue::Cycle { options, current } => {
                if options.is_empty() {
                    return;
                }
                let new_current = (current + 1) % options.len();
                Update::Cycle {
                    field: entry.field.clone(),
                    new_value: options[new_current].clone(),
                    options: options.clone(),
                    new_current,
                }
            }
        }
    };

    let (field, value_json) = match &update {
        Update::Bool(f, b) => (f.clone(), serde_json::json!(b)),
        Update::Cycle {
            field, new_value, ..
        } => (field.clone(), daemon_cycle_value_json(new_value)),
    };
    let system_prompt_slug = match &update {
        Update::Cycle {
            field, new_value, ..
        } if field == "system_prompt_preset" => Some(new_value.clone()),
        _ => None,
    };

    match app.client.update_daemon_config(&field, value_json).await {
        Ok(()) => {
            // Update local cache optimistically.
            if let Some(entry) = app.daemon_features.get_mut(idx) {
                entry.value = match update {
                    Update::Bool(_, b) => DaemonFeatureValue::Bool(b),
                    Update::Cycle {
                        options,
                        new_current,
                        ..
                    } => DaemonFeatureValue::Cycle {
                        options,
                        current: new_current,
                    },
                };
            }
            if let Some(slug) = system_prompt_slug {
                app.settings.system_prompt_preset = SystemPromptPreset::from_slug(&slug);
            }
            let label = app
                .daemon_features
                .get(idx)
                .map(|e| e.label.as_str())
                .unwrap_or("feature")
                .to_string();
            let value_str = match app.daemon_features.get(idx).map(|e| &e.value) {
                Some(DaemonFeatureValue::Bool(b)) => {
                    if *b { "enabled" } else { "disabled" }.to_string()
                }
                Some(DaemonFeatureValue::Cycle { options, current }) => options
                    .get(*current)
                    .cloned()
                    .unwrap_or_else(|| "?".to_string()),
                _ => String::new(),
            };
            let suffix = restart_toast_suffix(&field).unwrap_or("");
            app.notify_success(&format!("{} → {}{}", label, value_str, suffix));
        }
        Err(e) => {
            tracing::warn!("Failed to update daemon config field '{}': {}", field, e);
            app.notify(&format!("Failed to toggle feature: {}", e));
        }
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

fn require_authoritative_config(app: &mut App) -> bool {
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
    let Some(entry) = app.daemon_features.get(idx) else {
        return;
    };
    let DaemonFeatureValue::Cycle { options, current } = &entry.value else {
        return;
    };
    if options.is_empty() {
        return;
    }
    let next_slug = options[(current + 1) % options.len()].clone();
    let mode = match next_slug.as_str() {
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
            app.notify_success(&format!(
                "Model control mode: {}",
                model_control_mode_slug(report.current_mode)
            ));
            refresh_usage_stats(app).await;
        }
        Err(e) => {
            tracing::warn!("Failed to update model control mode: {}", e);
            app.notify(&format!("Failed to update model control mode: {}", e));
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
    if let Ok(n) = new_value.parse::<i64>() {
        serde_json::json!(n)
    } else if let Ok(f) = new_value.parse::<f64>() {
        serde_json::json!(f)
    } else {
        serde_json::json!(new_value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        daemon_cycle_value_json, refresh_daemon_features, refresh_usage_stats,
        sandbox_cache_reclaim_failure_message, sandbox_cache_reclaim_success_message,
        toggle_daemon_feature,
    };
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
            "memory_model_fallback_base_url",
            "memory_model_fallback_api_key",
            "dream_model",
            "dream_model_provider",
            "dream_model_base_url",
            "dream_model_api_key",
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
            let Some(field) = request["params"]["field"].as_str() else {
                panic!("UpdateDaemonConfig request carries a string field: {request}");
            };
            captured.push((field.to_string(), request["params"]["value"].clone()));
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
        assert!(
            !captured_fields.contains(&"retry_max_default"),
            "retry_max_default stays read-only (NotApplied), never toggled"
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
            .position(|entry| entry.field == "stall_detection_enabled")
        else {
            panic!("stall_detection_enabled entry present");
        };

        toggle_daemon_feature(&mut app, index).await;
        let Ok(captured) = server.await else {
            panic!("fake daemon server task panicked or was cancelled");
        };
        assert_eq!(
            captured,
            vec![(
                "stall_detection_enabled".to_string(),
                serde_json::json!(true)
            )]
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
}
