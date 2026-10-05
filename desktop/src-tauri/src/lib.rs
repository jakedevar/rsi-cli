//! RSI desktop backend.
//!
//! The webview never touches the daemon directly: it calls the closed set of
//! Tauri commands below, each of which validates its input and forwards
//! exactly one daemon JSON-RPC method over `~/.rsi/daemon.sock`. There is no
//! generic passthrough, so the frontend cannot reach any method not listed
//! here.

pub mod approvals;
pub mod daemon;
pub mod markdown;
pub mod live;

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::{Emitter, State};

use daemon::{DaemonClient, default_socket};

/// Providers the launch form may name; matches `SessionProvider` serde names.
pub const PROVIDERS: &[&str] = &[
    "Claude",
    "Codex",
    "Pioneer",
    "OpenRouter",
    "Bedrock",
    "Local",
    "Antigravity",
    "CodexAppServer",
    "Harness",
];

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const MAX_MESSAGE_BYTES: usize = 256 * 1024;

pub struct AppState {
    client: DaemonClient,
}

pub(crate) type CmdResult = Result<Value, String>;

pub(crate) fn parse_uuid(field: &str, raw: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(raw.trim())
        .map(|u| u.to_string())
        .map_err(|_| format!("{field} is not a valid UUID"))
}

pub(crate) fn check_text(field: &str, text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(format!("{field} exceeds {MAX_MESSAGE_BYTES} bytes"));
    }
    Ok(())
}

pub(crate) async fn forward(state: &AppState, method: &str, params: Value) -> CmdResult {
    state
        .client
        .call(method, params)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn daemon_info(state: State<'_, AppState>) -> CmdResult {
    let socket = state.client.socket().display().to_string();
    match state.client.call("GetHealthStatus", Value::Null).await {
        Ok(health) => Ok(json!({ "socket": socket, "connected": true, "health": health })),
        Err(e) => Ok(json!({ "socket": socket, "connected": false, "error": e.to_string() })),
    }
}

/// Render message Markdown to sanitized HTML (no daemon call). Output is safe
/// for `innerHTML`; it is the only daemon-derived text the webview may insert so.
#[tauri::command]
async fn render_markdown(texts: Vec<String>) -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(move || markdown::render_batch(&texts))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn list_projects(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "ListProjects", Value::Null).await
}

#[tauri::command]
async fn list_sessions(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "ListSessions", Value::Null).await
}

/// Archived sessions (read-only history). The daemon returns the full list;
/// the optional project filter is validated and forwarded.
pub fn archived_params(project_id: Option<&str>) -> Result<Value, String> {
    match project_id.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Ok(json!({ "project_id": parse_uuid("project_id", p)? })),
        None => Ok(json!({})),
    }
}

#[tauri::command]
async fn list_archived_sessions(
    state: State<'_, AppState>,
    project_id: Option<String>,
) -> CmdResult {
    let params = archived_params(project_id.as_deref())?;
    forward(&state, "ListArchivedSessions", params).await
}

#[tauri::command]
async fn get_session(state: State<'_, AppState>, session_id: String) -> CmdResult {
    let id = parse_uuid("session_id", &session_id)?;
    forward(&state, "GetSession", json!({ "session_id": id })).await
}

#[tauri::command]
async fn get_conversation(
    state: State<'_, AppState>,
    session_id: String,
    since_sequence: Option<i32>,
) -> CmdResult {
    let id = parse_uuid("session_id", &session_id)?;
    let mut params = json!({ "session_id": id });
    if let Some(seq) = since_sequence {
        params["since_sequence"] = json!(seq);
    }
    forward(&state, "GetConversation", params).await
}

/// Send operator text to a session. The daemon queues it when a turn is
/// active (#1049) and otherwise starts a continuation turn.
#[tauri::command]
async fn send_message(state: State<'_, AppState>, session_id: String, text: String) -> CmdResult {
    let id = parse_uuid("session_id", &session_id)?;
    check_text("message", &text)?;
    forward(
        &state,
        "ContinueSession",
        json!({ "session_id": id, "query": text }),
    )
    .await
}

/// Soft operator pause: interrupt the running turn.
#[tauri::command]
async fn interrupt_session(state: State<'_, AppState>, session_id: String) -> CmdResult {
    let id = parse_uuid("session_id", &session_id)?;
    forward(
        &state,
        "InterruptSession",
        json!({ "session_id": id, "pause_level": "soft" }),
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct LaunchRequest {
    pub query: String,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

/// Validate a launch form and build the `LaunchSession` params.
pub fn launch_params(req: &LaunchRequest) -> Result<Value, String> {
    check_text("prompt", &req.query)?;
    if !PROVIDERS.contains(&req.provider.as_str()) {
        return Err(format!("unknown provider {}", req.provider));
    }
    let nonblank = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let effort = nonblank(&req.effort);
    if let Some(e) = &effort
        && !EFFORTS.contains(&e.as_str())
    {
        return Err(format!("unknown effort {e}"));
    }
    let project_id = nonblank(&req.project_id)
        .map(|p| parse_uuid("project_id", &p))
        .transpose()?;
    let working_dir = nonblank(&req.working_dir);
    if let Some(dir) = &working_dir
        && !std::path::Path::new(dir).is_absolute()
    {
        return Err("working directory must be an absolute path".into());
    }
    Ok(json!({
        "query": req.query,
        "provider": req.provider,
        "model": nonblank(&req.model),
        "effort": effort,
        "project_id": project_id,
        "working_dir": working_dir,
        "tags": [],
    }))
}

#[tauri::command]
async fn launch_session(state: State<'_, AppState>, request: LaunchRequest) -> CmdResult {
    let params = launch_params(&request)?;
    forward(&state, "LaunchSession", params).await
}

#[tauri::command]
fn providers() -> Vec<&'static str> {
    PROVIDERS.to_vec()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = AppState {
        client: DaemonClient::new(default_socket()),
    };
    let socket = default_socket();
    tauri::Builder::default()
        .manage(state)
        .setup(move |app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(live::run(
                socket,
                move |msg| match msg {
                    live::LiveMsg::Event(e) => {
                        let _ = handle.emit(live::EVENT_NAME, e);
                    }
                    live::LiveMsg::Link(up) => {
                        live::set_link(up);
                        let _ = handle.emit(live::LINK_EVENT_NAME, live::LinkEvent { up });
                    }
                },
                live::Backoff::default(),
            ));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            daemon_info,
            list_projects,
            list_sessions,
            list_archived_sessions,
            get_session,
            get_conversation,
            send_message,
            interrupt_session,
            launch_session,
            providers,
            render_markdown,
            live::live_link,
            approvals::answer_question,
            approvals::archive_session,
            approvals::pause_session,
            approvals::get_operator_pause,
            approvals::set_operator_pause,
            approvals::set_attention_count,
        ])
        .run(tauri::generate_context!())
        .expect("error while running rsi desktop");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(provider: &str) -> LaunchRequest {
        LaunchRequest {
            query: "do the thing".into(),
            provider: provider.into(),
            model: Some("  ".into()),
            effort: Some("high".into()),
            project_id: Some("E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE".into()),
            working_dir: None,
        }
    }

    #[test]
    fn launch_params_normalize_and_validate() {
        let p = launch_params(&req("Claude")).unwrap();
        assert_eq!(p["provider"], "Claude");
        assert_eq!(p["model"], Value::Null);
        assert_eq!(p["effort"], "high");
        // UUIDs are lowercase on the wire.
        assert_eq!(p["project_id"], "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae");
    }

    #[test]
    fn launch_params_refuse_bad_input() {
        assert!(launch_params(&req("Gpt")).is_err());
        let mut r = req("Claude");
        r.query = "   ".into();
        assert!(launch_params(&r).is_err());
        let mut r = req("Claude");
        r.effort = Some("ultra".into());
        assert!(launch_params(&r).is_err());
        let mut r = req("Claude");
        r.working_dir = Some("relative/dir".into());
        assert!(launch_params(&r).is_err());
        let mut r = req("Claude");
        r.project_id = Some("not-a-uuid".into());
        assert!(launch_params(&r).is_err());
    }

    #[test]
    fn archived_params_validate_project() {
        assert_eq!(archived_params(None).unwrap(), json!({}));
        assert_eq!(archived_params(Some("  ")).unwrap(), json!({}));
        assert_eq!(
            archived_params(Some("E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE")).unwrap(),
            json!({ "project_id": "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae" })
        );
        assert!(archived_params(Some("nope")).is_err());
    }

    #[test]
    fn uuid_and_text_checks() {
        assert!(parse_uuid("id", "nope").is_err());
        assert!(check_text("m", "").is_err());
        assert!(check_text("m", &"x".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
        assert!(check_text("m", "hi").is_ok());
    }
}
