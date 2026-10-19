//! Settings rows backed by daemon state RPCs (hub #3009, local #125): the
//! model-control mode, the usage and model-control telemetry, the sandbox
//! storage and warm-pool status, and the sandbox cache reclaim. Each command
//! forwards exactly the operator method the TUI client
//! (`crates/rsi/src/client.rs`) calls from the same Settings row, with the
//! request the TUI builds: `update_model_control_policy`, `get_usage_stats`,
//! `get_model_control_status`, `cancel_model_invocation`,
//! `get_sandbox_storage_status`, `get_target_pool_status` and
//! `run_sandbox_build_cache_reclaim`. None carries a credential; paths in the
//! answers still pass the URL user-info mask on the way to the webview.

use serde_json::{Value, json};
use tauri::State;

use crate::admin_ops::mask_strings;
use crate::{AppState, CmdResult, forward, parse_uuid};

pub const METHOD_MODEL_CONTROL_STATUS: &str = "GetModelControlStatus";
pub const METHOD_MODEL_CONTROL_UPDATE: &str = "UpdateModelControlPolicy";
pub const METHOD_USAGE_STATS: &str = "GetUsageStats";
pub const METHOD_CANCEL_INVOCATION: &str = "CancelModelInvocation";
pub const METHOD_STORAGE_STATUS: &str = "GetSandboxStorageStatus";
pub const METHOD_POOL_STATUS: &str = "GetTargetPoolStatus";
pub const METHOD_RECLAIM: &str = "RunSandboxBuildCacheReclaim";

/// The model-control mode slugs the TUI row cycles through
/// (`DaemonFeatureEntry::defaults`), and the wire name each one maps to.
const MODES: [(&str, &str); 5] = [
    ("normal", "normal"),
    ("pause-background", "pause_background"),
    ("deny-paid", "deny_paid"),
    ("local-only", "local_only"),
    ("stop-all", "stop_all"),
];

/// `{ "mode": <wire name>, "interrupt_active": false }`: the TUI's mode
/// change (`Client::update_model_control_policy(mode, false)`). It never
/// interrupts running sessions and never touches the budget policies.
pub fn mode_params(slug: &str) -> Result<Value, String> {
    let wire = MODES
        .iter()
        .find(|(s, _)| *s == slug.trim())
        .map(|(_, w)| *w)
        .ok_or_else(|| {
            format!(
                "unknown model control mode '{}': use one of {}",
                slug.trim(),
                MODES.iter().map(|(s, _)| *s).collect::<Vec<_>>().join(", ")
            )
        })?;
    Ok(json!({ "mode": wire, "interrupt_active": false }))
}

/// `{ "recent_limit": 8 }`: the TUI's `get_model_control_status(None)`.
pub fn status_params() -> Value {
    json!({ "recent_limit": 8 })
}

/// `{ "dry_run": <bool> }`.
pub fn reclaim_params(dry_run: bool) -> Value {
    json!({ "dry_run": dry_run })
}

/// `{ "invocation_id": <uuid> }`.
pub fn cancel_params(invocation_id: &str) -> Result<Value, String> {
    Ok(json!({ "invocation_id": parse_uuid("invocation id", invocation_id)? }))
}

/// The breaker mode, the live and recent model work and the budget alerts.
#[tauri::command]
pub async fn model_control_overview(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_MODEL_CONTROL_STATUS, status_params())
        .await
        .map(mask_strings)
}

/// Set the model-control mode (the Settings row's Apply). The answer is the
/// daemon's `ModelControlPolicyUpdateReport`.
#[tauri::command]
pub async fn set_model_control_mode(state: State<'_, AppState>, mode: String) -> CmdResult {
    forward(&state, METHOD_MODEL_CONTROL_UPDATE, mode_params(&mode)?).await
}

/// Lifetime usage (`project_id: null`, the whole install).
#[tauri::command]
pub async fn usage_stats(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_USAGE_STATS, json!({ "project_id": null })).await
}

/// Cancel one live model invocation. The frontend asks first.
#[tauri::command]
pub async fn cancel_model_invocation(
    state: State<'_, AppState>,
    invocation_id: String,
) -> CmdResult {
    forward(
        &state,
        METHOD_CANCEL_INVOCATION,
        cancel_params(&invocation_id)?,
    )
    .await
}

/// The dry-run storage report (`GetSandboxStorageStatus`).
#[tauri::command]
pub async fn sandbox_storage_status(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_STORAGE_STATUS, json!({}))
        .await
        .map(mask_strings)
}

/// The warm build pools (`GetTargetPoolStatus`).
#[tauri::command]
pub async fn target_pool_status(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_POOL_STATUS, json!({}))
        .await
        .map(mask_strings)
}

/// One bounded sandbox cache pass: `dry_run` previews, otherwise it reclaims.
/// The frontend asks before a real pass.
#[tauri::command]
pub async fn sandbox_cache_reclaim(state: State<'_, AppState>, dry_run: bool) -> CmdResult {
    forward(&state, METHOD_RECLAIM, reclaim_params(dry_run))
        .await
        .map(mask_strings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_methods_are_the_ones_the_tui_calls() {
        assert_eq!(METHOD_MODEL_CONTROL_UPDATE, "UpdateModelControlPolicy");
        assert_eq!(METHOD_MODEL_CONTROL_STATUS, "GetModelControlStatus");
        assert_eq!(METHOD_USAGE_STATS, "GetUsageStats");
        assert_eq!(METHOD_CANCEL_INVOCATION, "CancelModelInvocation");
        assert_eq!(METHOD_STORAGE_STATUS, "GetSandboxStorageStatus");
        assert_eq!(METHOD_POOL_STATUS, "GetTargetPoolStatus");
        assert_eq!(METHOD_RECLAIM, "RunSandboxBuildCacheReclaim");
    }

    #[test]
    fn every_tui_mode_maps_to_its_wire_name_and_never_interrupts() {
        for (slug, wire) in MODES {
            let p = mode_params(slug).unwrap();
            assert_eq!(p["mode"], wire);
            assert_eq!(p["interrupt_active"], false);
            assert!(p.get("replace_policies").is_none());
            assert!(p.get("policies").is_none());
        }
        assert_eq!(
            mode_params("pause-background").unwrap()["mode"],
            "pause_background"
        );
    }

    #[test]
    fn an_unknown_mode_is_refused_before_any_request() {
        let e = mode_params("everything").unwrap_err();
        assert!(e.contains("unknown model control mode"), "{e}");
        assert!(
            mode_params("stop_all").is_err(),
            "the TUI slug is hyphenated"
        );
        assert!(mode_params("").is_err());
    }

    #[test]
    fn the_reclaim_and_cancel_requests_match_the_tui() {
        assert_eq!(reclaim_params(true), json!({ "dry_run": true }));
        assert_eq!(reclaim_params(false), json!({ "dry_run": false }));
        assert_eq!(status_params(), json!({ "recent_limit": 8 }));
        let id = "6f0c5b1e-9a53-4d1f-8f55-0d7a3c1b2e44";
        assert_eq!(cancel_params(id).unwrap(), json!({ "invocation_id": id }));
        assert!(cancel_params("not-a-uuid").is_err());
    }
}
