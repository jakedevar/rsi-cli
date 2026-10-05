//! Operator-attention commands: answer a pending question, archive, pause.
//!
//! Each command validates its input and forwards exactly one daemon method,
//! mirroring the TUI client (`crates/rsi/src/client.rs`): `AnswerQuestion`,
//! `ArchiveSession`, `InterruptSession` (with `pause_level`), `GetOperatorPause`
//! and `SetOperatorPause`.

use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, check_text, forward, parse_uuid};

/// Strengths the operator may request when pausing a running session.
const INTERRUPT_LEVELS: &[&str] = &["soft", "hard"];
/// `SetOperatorPause` only downgrades or clears a persisted marker.
const SET_LEVELS: &[&str] = &["none", "soft"];

fn level_param(levels: &[&str], level: &str) -> Result<String, String> {
    let level = level.trim().to_ascii_lowercase();
    if levels.contains(&level.as_str()) {
        Ok(level)
    } else {
        Err(format!("pause level must be one of: {}", levels.join(", ")))
    }
}

pub fn answer_params(session_id: &str, response_text: &str) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    check_text("answer", response_text)?;
    Ok(json!({ "session_id": id, "response_text": response_text }))
}

pub fn pause_params(session_id: &str, level: &str) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    let level = level_param(INTERRUPT_LEVELS, level)?;
    Ok(json!({ "session_id": id, "pause_level": level }))
}

pub fn set_pause_params(session_id: &str, level: &str) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    let level = level_param(SET_LEVELS, level)?;
    Ok(json!({ "session_id": id, "pause_level": level }))
}

/// Answer the session's pending operator question (or decline it).
#[tauri::command]
pub async fn answer_question(
    state: State<'_, AppState>,
    session_id: String,
    response_text: String,
) -> CmdResult {
    let params = answer_params(&session_id, &response_text)?;
    forward(&state, "AnswerQuestion", params).await
}

/// Archive (soft delete) a session; the frontend confirms first.
#[tauri::command]
pub async fn archive_session(state: State<'_, AppState>, session_id: String) -> CmdResult {
    let id = parse_uuid("session_id", &session_id)?;
    forward(&state, "ArchiveSession", json!({ "session_id": id })).await
}

/// Interrupt the running turn and persist a soft or hard operator pause.
#[tauri::command]
pub async fn pause_session(
    state: State<'_, AppState>,
    session_id: String,
    level: String,
) -> CmdResult {
    let params = pause_params(&session_id, &level)?;
    forward(&state, "InterruptSession", params).await
}

#[tauri::command]
pub async fn get_operator_pause(state: State<'_, AppState>, session_id: String) -> CmdResult {
    let id = parse_uuid("session_id", &session_id)?;
    forward(&state, "GetOperatorPause", json!({ "session_id": id })).await
}

/// Clear (`none`) or downgrade (`soft`) the persisted operator pause.
#[tauri::command]
pub async fn set_operator_pause(
    state: State<'_, AppState>,
    session_id: String,
    level: String,
) -> CmdResult {
    let params = set_pause_params(&session_id, &level)?;
    forward(&state, "SetOperatorPause", params).await
}

/// Reflect the number of sessions needing the operator in the window title.
/// Purely local: forwards nothing to the daemon.
#[tauri::command]
pub fn set_attention_count(window: tauri::WebviewWindow, count: u32) -> Result<(), String> {
    window
        .set_title(&attention_title(count))
        .map_err(|e| e.to_string())
}

pub fn attention_title(count: u32) -> String {
    if count == 0 {
        "RSI".to_string()
    } else {
        format!("({count}) RSI")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";

    #[test]
    fn answer_params_validate() {
        let p = answer_params(ID, "Yes").unwrap();
        assert_eq!(p["session_id"], "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae");
        assert_eq!(p["response_text"], "Yes");
        assert!(answer_params(ID, "  ").is_err());
        assert!(answer_params("nope", "Yes").is_err());
    }

    #[test]
    fn pause_levels_are_closed() {
        assert_eq!(pause_params(ID, "Hard").unwrap()["pause_level"], "hard");
        assert_eq!(pause_params(ID, "soft").unwrap()["pause_level"], "soft");
        assert!(pause_params(ID, "none").is_err());
        assert_eq!(set_pause_params(ID, "none").unwrap()["pause_level"], "none");
        assert!(set_pause_params(ID, "hard").is_err());
        assert!(set_pause_params("x", "none").is_err());
    }

    #[test]
    fn title_shows_count() {
        assert_eq!(attention_title(0), "RSI");
        assert_eq!(attention_title(3), "(3) RSI");
    }
}
