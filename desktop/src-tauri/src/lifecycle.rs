//! Session-lifecycle commands: delete to trash, unarchive, and the trash
//! browser's list, restore and purge.
//!
//! Each command validates its input and forwards exactly one daemon method,
//! mirroring the TUI client (`crates/rsi/src/client.rs`): `DeleteSession`
//! (moves the session to the trash), `UnarchiveSession`, `ListDeletedSessions`,
//! `UndeleteSession` and `PurgeSession` (a hard delete). The frontend asks the
//! operator before a delete or a purge, the way the TUI's `DD` and the trash
//! browser's `D` do; the daemon enforces the status rules.

use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward, parse_uuid};

/// `{ "session_id": <lowercase uuid> }` for the commands that take only a session.
pub fn session_params(session_id: &str) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    Ok(json!({ "session_id": id }))
}

/// The optional project filter of `ListDeletedSessions`.
pub fn deleted_params(project_id: Option<&str>) -> Result<Value, String> {
    match project_id.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Ok(json!({ "project_id": parse_uuid("project_id", p)? })),
        None => Ok(json!({})),
    }
}

/// Move a session to the trash (a logical delete; the trash browser restores it).
#[tauri::command]
pub async fn delete_session(state: State<'_, AppState>, session_id: String) -> CmdResult {
    forward(&state, "DeleteSession", session_params(&session_id)?).await
}

/// Restore an archived session to the active list.
#[tauri::command]
pub async fn unarchive_session(state: State<'_, AppState>, session_id: String) -> CmdResult {
    forward(&state, "UnarchiveSession", session_params(&session_id)?).await
}

/// Sessions in the trash, newest first.
#[tauri::command]
pub async fn list_deleted_sessions(
    state: State<'_, AppState>,
    project_id: Option<String>,
) -> CmdResult {
    let params = deleted_params(project_id.as_deref())?;
    forward(&state, "ListDeletedSessions", params).await
}

/// Restore a session from the trash.
#[tauri::command]
pub async fn undelete_session(state: State<'_, AppState>, session_id: String) -> CmdResult {
    forward(&state, "UndeleteSession", session_params(&session_id)?).await
}

/// Permanently delete a session that is in the trash. The frontend asks first.
#[tauri::command]
pub async fn purge_session(state: State<'_, AppState>, session_id: String) -> CmdResult {
    forward(&state, "PurgeSession", session_params(&session_id)?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";

    #[test]
    fn session_params_lowercase_the_uuid() {
        assert_eq!(
            session_params(ID).unwrap(),
            json!({ "session_id": "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae" })
        );
        assert_eq!(
            session_params("  e2cae1d4-1833-48eb-a7e8-6f1dd480fdae ").unwrap()["session_id"],
            "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae"
        );
    }

    #[test]
    fn session_params_refuse_a_bad_id() {
        assert!(session_params("").is_err());
        assert!(session_params("nope").is_err());
        assert!(session_params("../etc").is_err());
    }

    #[test]
    fn deleted_params_validate_the_project_filter() {
        assert_eq!(deleted_params(None).unwrap(), json!({}));
        assert_eq!(deleted_params(Some("  ")).unwrap(), json!({}));
        assert_eq!(
            deleted_params(Some(ID)).unwrap(),
            json!({ "project_id": "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae" })
        );
        assert!(deleted_params(Some("nope")).is_err());
    }
}
