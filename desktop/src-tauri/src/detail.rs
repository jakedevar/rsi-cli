//! Session-detail commands: rename, rate, switch model / effort, and the model
//! list the switch picker offers.
//!
//! Each command validates its input and forwards exactly one daemon method,
//! mirroring the TUI client (`crates/rsi/src/client.rs`): `UpdateSessionTitle`,
//! `UpdateSessionRating`, `GetSessionModelSwitchOptions`,
//! `QueueSessionModelUpdate` and `DiscoverModels`. The daemon enforces the
//! allowlist, the invocation fence and the provider rules; this layer only
//! rejects malformed input before it leaves the machine.

use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, PROVIDERS, forward, parse_uuid};

/// Longest session title the rename dialog may send.
pub const MAX_TITLE_CHARS: usize = 200;
/// Longest model id a switch may name.
const MAX_MODEL_CHARS: usize = 200;
/// Every effort level any switchable provider accepts (`session_switch_effort_levels`).
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

pub fn rename_params(session_id: &str, title: &str) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    let title = title.trim();
    if title.is_empty() {
        return Err("title is empty".into());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!("title exceeds {MAX_TITLE_CHARS} characters"));
    }
    if title.chars().any(char::is_control) {
        return Err("title must be a single line".into());
    }
    Ok(json!({ "session_id": id, "title": title }))
}

/// `rating: None` clears the stored rating.
pub fn rating_params(session_id: &str, rating: Option<i64>) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    if let Some(r) = rating
        && !(1..=10).contains(&r)
    {
        return Err("rating must be between 1 and 10".into());
    }
    Ok(json!({ "session_id": id, "rating": rating }))
}

pub fn switch_options_params(session_id: &str) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    Ok(json!({ "session_id": id }))
}

/// `new_effort: None` selects the model's own default effort. A fresh
/// idempotency key goes out with every call, like the TUI's.
pub fn switch_params(
    session_id: &str,
    expected_model_invocation_id: &str,
    new_model: &str,
    new_effort: Option<&str>,
    idempotency_key: &str,
) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    let fence = parse_uuid("expected_model_invocation_id", expected_model_invocation_id)?;
    let model = new_model.trim();
    if model.is_empty() {
        return Err("model is empty".into());
    }
    if model.chars().count() > MAX_MODEL_CHARS || model.chars().any(char::is_control) {
        return Err("model is not a valid model id".into());
    }
    let effort = new_effort.map(str::trim).filter(|e| !e.is_empty());
    if let Some(e) = effort
        && !EFFORTS.contains(&e)
    {
        return Err(format!("unknown effort {e}"));
    }
    Ok(json!({
        "session_id": id,
        "expected_model_invocation_id": fence,
        "new_model": model,
        "new_effort": effort,
        "idempotency_key": idempotency_key,
    }))
}

pub fn discover_params(provider: &str) -> Result<Value, String> {
    if !PROVIDERS.contains(&provider) {
        return Err(format!("unknown provider {provider}"));
    }
    Ok(json!({ "provider": provider, "include_capabilities": true }))
}

#[tauri::command]
pub async fn rename_session(
    state: State<'_, AppState>,
    session_id: String,
    title: String,
) -> CmdResult {
    let params = rename_params(&session_id, &title)?;
    forward(&state, "UpdateSessionTitle", params).await
}

#[tauri::command]
pub async fn rate_session(
    state: State<'_, AppState>,
    session_id: String,
    rating: Option<i64>,
) -> CmdResult {
    let params = rating_params(&session_id, rating)?;
    forward(&state, "UpdateSessionRating", params).await
}

#[tauri::command]
pub async fn get_session_model_switch_options(
    state: State<'_, AppState>,
    session_id: String,
) -> CmdResult {
    let params = switch_options_params(&session_id)?;
    forward(&state, "GetSessionModelSwitchOptions", params).await
}

#[tauri::command]
pub async fn queue_session_model_update(
    state: State<'_, AppState>,
    session_id: String,
    expected_model_invocation_id: String,
    new_model: String,
    new_effort: Option<String>,
) -> CmdResult {
    let key = uuid::Uuid::new_v4().to_string();
    let params = switch_params(
        &session_id,
        &expected_model_invocation_id,
        &new_model,
        new_effort.as_deref(),
        &key,
    )?;
    forward(&state, "QueueSessionModelUpdate", params).await
}

/// Models a provider offers, with its reported effort ladders.
#[tauri::command]
pub async fn discover_models(state: State<'_, AppState>, provider: String) -> CmdResult {
    let params = discover_params(&provider)?;
    forward(&state, "DiscoverModels", params).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const FENCE: &str = "0b2cf1d2-7f4b-4a0e-9d7c-2b3b6a7e8c11";

    #[test]
    fn rename_trims_and_bounds_the_title() {
        let p = rename_params(ID, "  Fix the build  ").unwrap();
        assert_eq!(p["session_id"], "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae");
        assert_eq!(p["title"], "Fix the build");
        assert!(rename_params(ID, "   ").is_err());
        assert!(rename_params(ID, "two\nlines").is_err());
        assert!(rename_params(ID, &"x".repeat(MAX_TITLE_CHARS + 1)).is_err());
        assert!(rename_params(ID, &"x".repeat(MAX_TITLE_CHARS)).is_ok());
        assert!(rename_params("nope", "ok").is_err());
    }

    #[test]
    fn rating_is_one_to_ten_or_clear() {
        assert_eq!(rating_params(ID, Some(7)).unwrap()["rating"], 7);
        assert_eq!(rating_params(ID, Some(1)).unwrap()["rating"], 1);
        assert_eq!(rating_params(ID, Some(10)).unwrap()["rating"], 10);
        assert_eq!(rating_params(ID, None).unwrap()["rating"], Value::Null);
        assert!(rating_params(ID, Some(0)).is_err());
        assert!(rating_params(ID, Some(11)).is_err());
        assert!(rating_params("nope", Some(5)).is_err());
    }

    #[test]
    fn switch_params_carry_the_fence_and_normalize() {
        let p = switch_params(ID, FENCE, " opus-5 ", Some("xhigh"), "k1").unwrap();
        assert_eq!(p["new_model"], "opus-5");
        assert_eq!(p["new_effort"], "xhigh");
        assert_eq!(p["expected_model_invocation_id"], FENCE);
        assert_eq!(p["idempotency_key"], "k1");
        let p = switch_params(ID, FENCE, "opus-5", Some("  "), "k1").unwrap();
        assert_eq!(p["new_effort"], Value::Null);
        let p = switch_params(ID, FENCE, "opus-5", None, "k1").unwrap();
        assert_eq!(p["new_effort"], Value::Null);
    }

    #[test]
    fn switch_params_refuse_bad_input() {
        assert!(switch_params(ID, FENCE, "  ", None, "k").is_err());
        assert!(switch_params(ID, FENCE, "m\nx", None, "k").is_err());
        assert!(switch_params(ID, FENCE, "m", Some("ultra-max"), "k").is_err());
        assert!(switch_params(ID, "not-a-uuid", "m", None, "k").is_err());
        assert!(switch_params("nope", FENCE, "m", None, "k").is_err());
        assert!(switch_params(ID, FENCE, &"m".repeat(MAX_MODEL_CHARS + 1), None, "k").is_err());
    }

    #[test]
    fn options_and_discovery_are_closed() {
        assert_eq!(
            switch_options_params(ID).unwrap(),
            json!({ "session_id": "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae" })
        );
        assert!(switch_options_params("nope").is_err());
        let p = discover_params("Claude").unwrap();
        assert_eq!(p["provider"], "Claude");
        assert_eq!(p["include_capabilities"], true);
        assert!(discover_params("Gpt").is_err());
    }
}
