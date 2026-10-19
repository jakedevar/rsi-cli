//! The operator to-do panel and pop-up (hub #3015 slice 13B, TUI #1747/#2950).
//!
//! Three commands, each forwarding one operator-only daemon method with the
//! request the TUI client sends (`list_operator_todos`, `close_operator_todo`,
//! `reopen_operator_todo` in `crates/rsi/src/client.rs`):
//!
//! - `operator_todo_list` → `OperatorTodoList {closed_since}` (the open items
//!   plus those closed at or after the catch-up baseline);
//! - `operator_todo_close` → `OperatorTodoClose {todo_id, status, note: null}`,
//!   status `done` or `dismissed` only (never `open`);
//! - `operator_todo_reopen` → `OperatorTodoReopen {todo_id}`, the undo of the
//!   operator's own close inside the daemon's window.
//!
//! The daemon validates again (`deny_unknown_fields`, the undo window, scope).

use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward, parse_uuid};

/// Statuses the close may set (`OperatorTodoStatusV1` serde names).
pub const CLOSE_STATUSES: &[&str] = &["done", "dismissed"];

/// An RFC 3339 instant as the webview writes it (`Date.toISOString()`); the
/// daemon parses it, this only refuses anything that is clearly not one.
fn instant(field: &str, raw: &str) -> Result<String, String> {
    let s = raw.trim();
    let shaped = (20..=40).contains(&s.len())
        && s.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        && s.contains('T')
        && s.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'T' | 'Z' | ':' | '.' | '+' | '-'));
    if shaped {
        Ok(s.to_string())
    } else {
        Err(format!("{field} is not an RFC 3339 time"))
    }
}

/// `OperatorTodoList` params.
pub fn list_params(closed_since: Option<&str>) -> Result<Value, String> {
    let since = match closed_since.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Value::String(instant("closed_since", s)?),
        None => Value::Null,
    };
    Ok(json!({ "closed_since": since }))
}

/// `OperatorTodoClose` params.
pub fn close_params(todo_id: &str, status: &str) -> Result<Value, String> {
    let id = parse_uuid("todo_id", todo_id)?;
    if !CLOSE_STATUSES.contains(&status) {
        return Err("status must be done or dismissed".into());
    }
    Ok(json!({ "todo_id": id, "status": status, "note": null }))
}

/// `OperatorTodoReopen` params.
pub fn reopen_params(todo_id: &str) -> Result<Value, String> {
    Ok(json!({ "todo_id": parse_uuid("todo_id", todo_id)? }))
}

#[tauri::command]
pub async fn operator_todo_list(
    state: State<'_, AppState>,
    closed_since: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "OperatorTodoList",
        list_params(closed_since.as_deref())?,
    )
    .await
}

#[tauri::command]
pub async fn operator_todo_close(
    state: State<'_, AppState>,
    todo_id: String,
    status: String,
) -> CmdResult {
    forward(
        &state,
        "OperatorTodoClose",
        close_params(&todo_id, &status)?,
    )
    .await
}

#[tauri::command]
pub async fn operator_todo_reopen(state: State<'_, AppState>, todo_id: String) -> CmdResult {
    forward(&state, "OperatorTodoReopen", reopen_params(&todo_id)?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "11111111-1111-4111-8111-11111111ABCD";
    const ID_LOWER: &str = "11111111-1111-4111-8111-11111111abcd";

    #[test]
    fn the_list_asks_for_items_closed_since_the_baseline() {
        assert_eq!(list_params(None).unwrap(), json!({ "closed_since": null }));
        assert_eq!(
            list_params(Some(" ")).unwrap(),
            json!({ "closed_since": null })
        );
        assert_eq!(
            list_params(Some("2026-10-18T10:00:00.000Z")).unwrap(),
            json!({ "closed_since": "2026-10-18T10:00:00.000Z" })
        );
        assert!(list_params(Some("yesterday")).is_err());
        assert!(list_params(Some("2026-10-18T10:00:00Z; DROP")).is_err());
    }

    #[test]
    fn a_close_is_done_or_dismissed_with_no_note() {
        assert_eq!(
            close_params(ID, "done").unwrap(),
            json!({ "todo_id": ID_LOWER, "status": "done", "note": null })
        );
        assert_eq!(
            close_params(ID, "dismissed").unwrap()["status"],
            "dismissed"
        );
        assert!(close_params(ID, "open").is_err());
        assert!(close_params(ID, "Done").is_err());
        assert!(close_params("x", "done").is_err());
    }

    #[test]
    fn a_reopen_names_only_the_item() {
        assert_eq!(reopen_params(ID).unwrap(), json!({ "todo_id": ID_LOWER }));
        assert!(reopen_params("").is_err());
    }
}
