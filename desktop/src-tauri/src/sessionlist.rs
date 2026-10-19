//! Session-list management commands: projects, labels, project and parent
//! reassignment. Each validates its input and forwards exactly one daemon
//! method, mirroring the TUI client (`crates/rsi/src/client.rs`):
//! `CreateProject`, `UpdateProject`, `DeleteProject`, `ListLabels`,
//! `CreateLabel`, `UpdateLabel`, `DeleteLabel`, `UpdateSessionLabel`,
//! `UpdateSessionProject`, `SetSessionParent`. Unarchive is slice 6's
//! `lifecycle::unarchive_session`.

use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward, parse_uuid};

/// Names, paths and descriptions are short operator text.
const MAX_FIELD_BYTES: usize = 1024;

fn text_field(field: &str, raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.len() > MAX_FIELD_BYTES {
        return Err(format!("{field} exceeds {MAX_FIELD_BYTES} bytes"));
    }
    if t.chars().any(|c| c.is_control()) {
        return Err(format!("{field} contains a control character"));
    }
    Ok(t.to_string())
}

fn required(field: &str, raw: &str) -> Result<String, String> {
    let t = text_field(field, raw)?;
    if t.is_empty() {
        return Err(format!("{field} is empty"));
    }
    Ok(t)
}

/// `Some(text)` for non-empty text, `None` to leave the daemon default.
fn optional(field: &str, raw: Option<&str>) -> Result<Option<String>, String> {
    match raw {
        None => Ok(None),
        Some(r) => {
            let t = text_field(field, r)?;
            Ok(if t.is_empty() { None } else { Some(t) })
        }
    }
}

/// `#rrggbb` only: the daemon stores the string and the TUI parses it.
fn color_param(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = optional("color", raw)? else {
        return Ok(None);
    };
    let ok =
        raw.len() == 7 && raw.starts_with('#') && raw[1..].chars().all(|c| c.is_ascii_hexdigit());
    if ok {
        Ok(Some(raw.to_ascii_lowercase()))
    } else {
        Err("color must look like #89b4fa".to_string())
    }
}

pub fn project_create_params(
    name: &str,
    path: Option<&str>,
    color: Option<&str>,
) -> Result<Value, String> {
    Ok(json!({
        "name": required("name", name)?,
        "path": optional("path", path)?,
        "description": Value::Null,
        "color": color_param(color)?,
    }))
}

pub fn project_update_params(
    id: &str,
    name: &str,
    path: Option<&str>,
    color: Option<&str>,
) -> Result<Value, String> {
    Ok(json!({
        "id": parse_uuid("id", id)?,
        "name": required("name", name)?,
        "path": optional("path", path)?,
        "description": Value::Null,
        "color": color_param(color)?,
    }))
}

pub fn label_create_params(
    name: &str,
    description: Option<&str>,
    color: Option<&str>,
) -> Result<Value, String> {
    Ok(json!({
        "name": required("name", name)?,
        "description": optional("description", description)?,
        "project_id": Value::Null,
        "color": color_param(color)?,
    }))
}

pub fn label_update_params(
    id: &str,
    name: &str,
    description: Option<&str>,
    color: Option<&str>,
) -> Result<Value, String> {
    Ok(json!({
        "id": parse_uuid("id", id)?,
        "name": required("name", name)?,
        "description": optional("description", description)?,
        "color": color_param(color)?,
    }))
}

/// `null` / empty clears the assignment; anything else must be a UUID.
fn optional_uuid(field: &str, raw: Option<&str>) -> Result<Value, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(Value::Null),
        Some(r) => Ok(Value::String(parse_uuid(field, r)?)),
    }
}

pub fn session_label_params(session_id: &str, group_id: Option<&str>) -> Result<Value, String> {
    Ok(json!({
        "session_id": parse_uuid("session_id", session_id)?,
        "group_id": optional_uuid("group_id", group_id)?,
    }))
}

pub fn session_project_params(session_id: &str, project_id: Option<&str>) -> Result<Value, String> {
    Ok(json!({
        "session_id": parse_uuid("session_id", session_id)?,
        "project_id": optional_uuid("project_id", project_id)?,
    }))
}

pub fn session_parent_params(
    session_id: &str,
    new_parent_id: Option<&str>,
) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    let parent = optional_uuid("new_parent_id", new_parent_id)?;
    if parent.as_str() == Some(id.as_str()) {
        return Err("a session cannot be its own parent".to_string());
    }
    Ok(json!({ "session_id": id, "new_parent_id": parent }))
}

#[tauri::command]
pub async fn create_project(
    state: State<'_, AppState>,
    name: String,
    path: Option<String>,
    color: Option<String>,
) -> CmdResult {
    let p = project_create_params(&name, path.as_deref(), color.as_deref())?;
    forward(&state, "CreateProject", p).await
}

#[tauri::command]
pub async fn update_project(
    state: State<'_, AppState>,
    id: String,
    name: String,
    path: Option<String>,
    color: Option<String>,
) -> CmdResult {
    let p = project_update_params(&id, &name, path.as_deref(), color.as_deref())?;
    forward(&state, "UpdateProject", p).await
}

#[tauri::command]
pub async fn delete_project(state: State<'_, AppState>, id: String) -> CmdResult {
    let id = parse_uuid("id", &id)?;
    forward(&state, "DeleteProject", json!({ "id": id })).await
}

#[tauri::command]
pub async fn list_labels(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "ListLabels", Value::Null).await
}

#[tauri::command]
pub async fn create_label(
    state: State<'_, AppState>,
    name: String,
    description: Option<String>,
    color: Option<String>,
) -> CmdResult {
    let p = label_create_params(&name, description.as_deref(), color.as_deref())?;
    forward(&state, "CreateLabel", p).await
}

#[tauri::command]
pub async fn update_label(
    state: State<'_, AppState>,
    id: String,
    name: String,
    description: Option<String>,
    color: Option<String>,
) -> CmdResult {
    let p = label_update_params(&id, &name, description.as_deref(), color.as_deref())?;
    forward(&state, "UpdateLabel", p).await
}

#[tauri::command]
pub async fn delete_label(state: State<'_, AppState>, id: String) -> CmdResult {
    let id = parse_uuid("id", &id)?;
    forward(&state, "DeleteLabel", json!({ "id": id })).await
}

/// Assign (or with no `group_id`, clear) the session's label.
#[tauri::command]
pub async fn update_session_label(
    state: State<'_, AppState>,
    session_id: String,
    group_id: Option<String>,
) -> CmdResult {
    let p = session_label_params(&session_id, group_id.as_deref())?;
    forward(&state, "UpdateSessionLabel", p).await
}

/// Move the session to a project (or with no `project_id`, to none).
#[tauri::command]
pub async fn update_session_project(
    state: State<'_, AppState>,
    session_id: String,
    project_id: Option<String>,
) -> CmdResult {
    let p = session_project_params(&session_id, project_id.as_deref())?;
    forward(&state, "UpdateSessionProject", p).await
}

/// Re-parent the session under a container (or with no parent, to the top level).
#[tauri::command]
pub async fn set_session_parent(
    state: State<'_, AppState>,
    session_id: String,
    new_parent_id: Option<String>,
) -> CmdResult {
    let p = session_parent_params(&session_id, new_parent_id.as_deref())?;
    forward(&state, "SetSessionParent", p).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const B: &str = "7b1b1d9e-0c7a-4c44-9a36-2f9c8f1a4e10";

    #[test]
    fn project_params_trim_and_validate() {
        let p = project_create_params("  Web  ", Some(" /srv/web "), Some("#89B4FA")).unwrap();
        assert_eq!(p["name"], "Web");
        assert_eq!(p["path"], "/srv/web");
        assert_eq!(p["color"], "#89b4fa");
        let p = project_create_params("Web", Some("  "), None).unwrap();
        assert!(p["path"].is_null());
        assert!(p["color"].is_null());
        assert!(project_create_params("  ", None, None).is_err());
        assert!(project_create_params("a\nb", None, None).is_err());
        assert!(project_create_params("Web", None, Some("blue")).is_err());
        assert!(project_create_params("Web", None, Some("#12345")).is_err());
        assert!(project_create_params("Web", None, Some("#12345g")).is_err());
    }

    #[test]
    fn project_update_needs_a_uuid_id() {
        let p = project_update_params(A, "Web", None, None).unwrap();
        assert_eq!(p["id"], "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae");
        assert!(project_update_params("nope", "Web", None, None).is_err());
        assert!(project_update_params(A, "", None, None).is_err());
    }

    #[test]
    fn label_params_keep_optional_description() {
        let p = label_create_params("Urgent", Some(" soon "), Some("#cba6f7")).unwrap();
        assert_eq!(p["description"], "soon");
        assert!(p["project_id"].is_null());
        let p = label_update_params(A, "Urgent", None, None).unwrap();
        assert!(p["description"].is_null());
        assert!(label_update_params("x", "Urgent", None, None).is_err());
        assert!(label_create_params(" ", None, None).is_err());
    }

    #[test]
    fn session_assignments_clear_with_null() {
        let p = session_label_params(A, Some(B)).unwrap();
        assert_eq!(p["group_id"], B);
        assert!(session_label_params(A, None).unwrap()["group_id"].is_null());
        assert!(session_label_params(A, Some("")).unwrap()["group_id"].is_null());
        assert!(session_label_params(A, Some("zzz")).is_err());
        assert!(session_project_params(A, None).unwrap()["project_id"].is_null());
        assert!(session_project_params("bad", Some(B)).is_err());
    }

    #[test]
    fn parent_refuses_self() {
        let p = session_parent_params(A, Some(B)).unwrap();
        assert_eq!(p["new_parent_id"], B);
        assert!(session_parent_params(A, None).unwrap()["new_parent_id"].is_null());
        assert!(session_parent_params(A, Some(&A.to_lowercase())).is_err());
    }
}
