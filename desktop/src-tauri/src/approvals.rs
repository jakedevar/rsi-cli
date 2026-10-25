//! Operator-attention commands: answer a pending question, archive, pause.
//!
//! Each command validates its input and forwards exactly one daemon method,
//! mirroring the TUI client (`crates/rsi/src/client.rs`): `AnswerQuestion`,
//! `ArchiveSession`, `MarkPendingArchive`, `InterruptSession` (with `pause_level`), `GetOperatorPause`
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

/// `MarkPendingArchive` takes the wanted state explicitly: the TUI sends `!session.pending_archive`
/// (crates/rsi/src/client.rs `mark_pending_archive`), so one command both sets and clears it.
pub fn pending_archive_params(session_id: &str, pending: bool) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    Ok(json!({ "session_id": id, "pending": pending }))
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

/// Mark a Starting, Running or WaitingApproval session to archive itself when it finishes (`pending: true`), or clear the
/// mark. rsid refuses `ArchiveSession` for an active session, so the frontend picks this for those.
/// A write: it is not on the read-only allowlist.
#[tauri::command]
pub async fn mark_pending_archive(
    state: State<'_, AppState>,
    session_id: String,
    pending: bool,
) -> CmdResult {
    let params = pending_archive_params(&session_id, pending)?;
    forward(&state, "MarkPendingArchive", params).await
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

/// Reflect the number of sessions needing the operator in the window title and the Dock badge
/// (local #294). Purely local: forwards nothing to the daemon.
#[tauri::command]
pub fn set_attention_count(window: tauri::WebviewWindow, count: u32) -> Result<(), String> {
    // The badge is best effort: Windows has no count badge and Linux shows one only on some desktops.
    let _ = window.set_badge_count(badge_count(count));
    window
        .set_title(&attention_title(count))
        .map_err(|e| e.to_string())
}

/// The Dock badge for `count` sessions waiting: none (cleared) at zero.
pub fn badge_count(count: u32) -> Option<i64> {
    (count > 0).then_some(i64::from(count))
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
    fn pending_archive_params_carry_the_wanted_state() {
        let on = pending_archive_params(ID, true).unwrap();
        assert_eq!(on["session_id"], "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae");
        assert_eq!(on["pending"], true);
        assert_eq!(pending_archive_params(ID, false).unwrap()["pending"], false);
        assert!(pending_archive_params("nope", true).is_err());
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
    fn badge_clears_at_zero() {
        assert_eq!(badge_count(0), None);
        assert_eq!(badge_count(2), Some(2));
    }

    #[test]
    fn title_shows_count() {
        assert_eq!(attention_title(0), "RSI");
        assert_eq!(attention_title(3), "(3) RSI");
    }
}
