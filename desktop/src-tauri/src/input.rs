//! Gap sweep G1 (hub #3000): the input bar's daemon and file surface.
//!
//! - Operator messages: a follow-up for a busy session is queued
//!   (`QueueOperatorMessage`) and `:pending` lists, edits and withdraws them
//!   (`ListOperatorMessages`, `EditOperatorMessage`, `WithdrawOperatorMessage`);
//!   `:drain` asks for a restart after turns settle (`RestartDaemonDrain`,
//!   `GetDrainRestartStatus`). Same requests as crates/rsi/src/client.rs.
//! - The correction preview: `CompilePrompt` (its result arrives as a
//!   `compile_prompt_completed` / `_failed` bus event, so a subscription is
//!   opened first, as crates/rsi/src/prompt_processor.rs does) and
//!   `GenerateText` for grammar fixes and questions about the draft. The
//!   daemon picks the configured prompt-compiler model; no credential passes
//!   through the desktop.
//! - A pasted image goes to rsid (`UploadPasteImage`, hub #3044), which
//!   checks its type and stores it under a content-addressed name in its own
//!   paste directory; the returned path is the `@path` reference.
//! - Slash-command suggestions come from rsid (`ListSlashCommands`) for a
//!   project or session root, through the TUI's discovery.
//! - Settings › Claude Skills lists the daemon user's skills (`ListSkills`
//!   `user: true`) and previews one `SKILL.md` (`GetSkill`, hub #3062),
//!   through the same `rsi_common::input_assets` reads the TUI uses, and
//!   its `e` / `d` enable, disable or delete one (`SetSkillEnabled`,
//!   `DeleteSkill`, #97; rsid does the rename or removal).
//!
//! This module touches no file itself (hub #3044): the desktop must work as a
//! pure rsid frontend off the daemon host, and a test below pins that.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tauri::State;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{AppState, CmdResult, check_text, forward, parse_uuid};

/// How long a compile may take before the desktop stops waiting.
const COMPILE_DEADLINE: Duration = Duration::from_secs(180);
/// Largest event line read from the compile subscription.
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// Largest pasted image rsid stores (`rsi_common::input_assets::MAX_PASTE_IMAGE_BYTES`).
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// Most slash commands asked for (rsid's default).
const MAX_COMMANDS: u64 = 500;
pub const METHOD_UPLOAD_PASTE_IMAGE: &str = "UploadPasteImage";
pub const METHOD_LIST_SLASH_COMMANDS: &str = "ListSlashCommands";
pub const METHOD_LIST_SKILLS: &str = "ListSkills";
pub const METHOD_GET_SKILL: &str = "GetSkill";
pub const METHOD_SET_SKILL_ENABLED: &str = "SetSkillEnabled";
pub const METHOD_DELETE_SKILL: &str = "DeleteSkill";
/// Most skills asked for (rsid's `DEFAULT_SKILLS`).
const MAX_SKILLS: u64 = 500;
/// Longest skill name rsid accepts (`MAX_SKILL_NAME_BYTES`).
const MAX_SKILL_NAME_BYTES: usize = 255;
/// The image types rsid accepts (`PASTE_IMAGE_MIME_ALLOWLIST`).
const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// Largest draft sent to the prompt processor.
const MAX_DRAFT_BYTES: usize = 256 * 1024;

pub const GRAMMAR_SYSTEM_PROMPT: &str = "\
Fix all spelling and grammar errors in the following text. \
Preserve the original meaning, tone, structure, and formatting. \
Do not add, remove, or rephrase content \u{2014} only correct errors. \
If the text has no errors, return it unchanged. \
Return the corrected text only. No explanation. No wrapper text.";

pub const AI_CHAT_SYSTEM_PROMPT: &str = "\
You are a concise text analysis assistant. The user will provide source text \
and ask questions about it. Answer questions directly and briefly. \
Keep responses under 3 sentences unless the question requires more detail.";

// ---------- operator messages ----------

/// `QueueOperatorMessage`: a follow-up for a busy session, delivered at its next safe turn boundary.
pub fn queue_params(session_id: &str, content: &str, key: &str) -> Result<Value, String> {
    let id = parse_uuid("session_id", session_id)?;
    check_text("message", content)?;
    let key = parse_uuid("idempotency_key", key)?;
    Ok(json!({ "session_id": id, "content": content, "idempotency_key": key }))
}

#[tauri::command]
pub async fn queue_operator_message(
    state: State<'_, AppState>,
    session_id: String,
    content: String,
) -> CmdResult {
    let key = uuid::Uuid::new_v4().to_string();
    forward(
        &state,
        "QueueOperatorMessage",
        queue_params(&session_id, &content, &key)?,
    )
    .await
}

#[tauri::command]
pub async fn list_operator_messages(state: State<'_, AppState>, session_id: String) -> CmdResult {
    let id = parse_uuid("session_id", &session_id)?;
    forward(&state, "ListOperatorMessages", json!({ "session_id": id })).await
}

#[tauri::command]
pub async fn edit_operator_message(
    state: State<'_, AppState>,
    message_id: String,
    content: String,
) -> CmdResult {
    let id = parse_uuid("message_id", &message_id)?;
    check_text("message", &content)?;
    forward(
        &state,
        "EditOperatorMessage",
        json!({ "message_id": id, "content": content }),
    )
    .await
}

#[tauri::command]
pub async fn withdraw_operator_message(
    state: State<'_, AppState>,
    message_id: String,
) -> CmdResult {
    let id = parse_uuid("message_id", &message_id)?;
    forward(
        &state,
        "WithdrawOperatorMessage",
        json!({ "message_id": id }),
    )
    .await
}

#[tauri::command]
pub async fn restart_daemon_drain(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "RestartDaemonDrain", json!({})).await
}

#[tauri::command]
pub async fn get_drain_restart_status(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "GetDrainRestartStatus", json!({})).await
}

// ---------- prompt processor ----------

fn check_draft(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("the draft is empty".into());
    }
    if text.len() > MAX_DRAFT_BYTES {
        return Err(format!("the draft exceeds {MAX_DRAFT_BYTES} bytes"));
    }
    Ok(())
}

/// `GenerateText` for a grammar fix (`question` None) or a question about the draft.
pub fn generate_params(draft: &str, question: Option<&str>) -> Result<Value, String> {
    check_draft(draft)?;
    match question.map(str::trim) {
        None => Ok(json!({ "system": GRAMMAR_SYSTEM_PROMPT, "prompt": draft })),
        Some("") => Err("the question is empty".into()),
        Some(q) => {
            check_draft(q)?;
            Ok(
                json!({ "system": AI_CHAT_SYSTEM_PROMPT, "prompt": format!("Text:\n{draft}\n\nQuestion: {q}") }),
            )
        }
    }
}

#[tauri::command]
pub async fn generate_text(
    state: State<'_, AppState>,
    draft: String,
    question: Option<String>,
) -> CmdResult {
    let params = generate_params(&draft, question.as_deref())?;
    let client = state.client.clone().with_deadline(COMPILE_DEADLINE);
    let out = client
        .call("GenerateText", params)
        .await
        .map_err(|e| e.to_string())?;
    let text = out
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("Empty response from model".into());
    }
    Ok(json!({ "text": text }))
}

/// One desktop process is one compile caller: a second compile supersedes the first.
fn caller_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

#[derive(Debug, PartialEq, Eq)]
enum Line {
    Data(Vec<u8>),
    Oversize,
    Eof,
}

async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R, max: usize) -> std::io::Result<Line> {
    let mut buf = Vec::new();
    let mut over = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if buf.is_empty() && !over {
                Line::Eof
            } else if over {
                Line::Oversize
            } else {
                Line::Data(buf)
            });
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !over {
            if buf.len() + take > max {
                over = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        r.consume(take);
        if done {
            return Ok(if over {
                Line::Oversize
            } else {
                Line::Data(buf)
            });
        }
    }
}

/// What a bus event line means for the compile with `request_id`.
pub fn compile_event(line: &[u8], request_id: &str) -> Option<Result<Value, String>> {
    let ev: Value = serde_json::from_slice(line).ok()?;
    let data = ev.get("data")?;
    if data.get("request_id").and_then(Value::as_str) != Some(request_id) {
        return None;
    }
    match ev.get("event_type").and_then(Value::as_str)? {
        "compile_prompt_completed" => Some(Ok(data.get("result").cloned().unwrap_or(Value::Null))),
        "compile_prompt_failed" => Some(Err(data
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown compile failure")
            .to_string())),
        _ => None,
    }
}

async fn compile(socket: &Path, client: &crate::daemon::DaemonClient, input: &str) -> CmdResult {
    let stream = crate::daemon::connect_socket(socket)
        .await
        .map_err(|e| format!("cannot reach rsid: {e}"))?;
    let (read, mut write) = tokio::io::split(stream);
    let mut sub = serde_json::to_vec(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "Subscribe",
        "params": { "event_types": ["compile_prompt_completed", "compile_prompt_failed"], "session_id": null },
    }))
    .map_err(|e| e.to_string())?;
    sub.push(b'\n');
    write.write_all(&sub).await.map_err(|e| e.to_string())?;
    write.flush().await.map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(read);
    match read_line(&mut reader, MAX_EVENT_BYTES)
        .await
        .map_err(|e| e.to_string())?
    {
        Line::Data(ack) => {
            crate::daemon::parse_response(&ack).map_err(|e| format!("subscribe refused: {e}"))?;
        }
        _ => return Err("closed before the subscribe ack".into()),
    }
    let resp = client
        .call(
            "CompilePrompt",
            json!({ "input": input, "caller_id": caller_id() }),
        )
        .await
        .map_err(|e| e.to_string())?;
    if let Some(cached) = resp.get("cached").filter(|v| !v.is_null()) {
        return Ok(cached.clone());
    }
    let request_id = resp
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or("CompilePrompt returned no request_id")?
        .to_string();
    loop {
        match read_line(&mut reader, MAX_EVENT_BYTES)
            .await
            .map_err(|e| e.to_string())?
        {
            Line::Eof => return Err("subscribe stream closed before the compile finished".into()),
            Line::Oversize => {}
            Line::Data(line) => {
                if let Some(done) = compile_event(&line, &request_id) {
                    return done;
                }
            }
        }
    }
}

#[tauri::command]
pub async fn compile_prompt(state: State<'_, AppState>, input: String) -> CmdResult {
    check_draft(&input)?;
    let client = state.client.clone();
    let socket = client.socket().to_path_buf();
    tokio::time::timeout(COMPILE_DEADLINE, compile(&socket, &client, &input))
        .await
        .map_err(|_| "prompt compilation timed out".to_string())?
}

// ---------- pasted images ----------

/// `UploadPasteImage`: `{mime, data_base64}`. The type and size are checked
/// here so an obvious mistake never crosses the socket; rsid re-checks both
/// and sniffs the magic bytes.
pub fn upload_params(request: &Value) -> Result<Value, String> {
    let o = request
        .as_object()
        .ok_or_else(|| "request must be an object".to_string())?;
    if let Some(k) = o
        .keys()
        .find(|k| !["mime", "data_base64"].contains(&k.as_str()))
    {
        return Err(format!("unknown field {k}"));
    }
    let mime = o
        .get("mime")
        .and_then(Value::as_str)
        .map(|m| m.trim().to_ascii_lowercase())
        .ok_or_else(|| "mime is required".to_string())?;
    if !IMAGE_MIMES.contains(&mime.as_str()) {
        return Err(format!("unsupported image type {mime}"));
    }
    let data = o
        .get("data_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "data_base64 is required".to_string())?;
    if data.is_empty() {
        return Err("the clipboard image is empty".into());
    }
    if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(format!(
            "the clipboard image exceeds {MAX_IMAGE_BYTES} bytes"
        ));
    }
    Ok(json!({ "mime": mime, "data_base64": data }))
}

#[tauri::command]
pub async fn upload_paste_image(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_UPLOAD_PASTE_IMAGE, upload_params(&request)?).await
}

// ---------- slash commands ----------

/// `ListSlashCommands`: `{project_id | session_id}`, exactly one.
pub fn slash_params(request: &Value) -> Result<Value, String> {
    let o = request
        .as_object()
        .ok_or_else(|| "request must be an object".to_string())?;
    if let Some(k) = o
        .keys()
        .find(|k| !["project_id", "session_id"].contains(&k.as_str()))
    {
        return Err(format!("unknown field {k}"));
    }
    let pick = |field: &str| match o.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => parse_uuid(field, s).map(Some),
        Some(_) => Err(format!("{field} must be a UUID string")),
    };
    match (pick("project_id")?, pick("session_id")?) {
        (Some(p), None) => Ok(json!({ "project_id": p, "max_entries": MAX_COMMANDS })),
        (None, Some(s)) => Ok(json!({ "session_id": s, "max_entries": MAX_COMMANDS })),
        (Some(_), Some(_)) => Err("name exactly one of project_id and session_id, not both".into()),
        (None, None) => Err("name a project_id or a session_id".into()),
    }
}

#[tauri::command]
pub async fn list_slash_commands(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_LIST_SLASH_COMMANDS, slash_params(&request)?).await
}

// ---------- skills ----------

/// `ListSkills` for the daemon user's `~/.claude/skills` (the TUI's
/// Settings › Claude Skills). No argument: the desktop names no path.
pub fn list_skills_params() -> Value {
    json!({ "user": true, "max_entries": MAX_SKILLS })
}

/// `GetSkill` for one of the daemon user's skills: `name` is one path
/// component (rsid checks it too and keeps the read inside the directory).
pub fn get_skill_params(name: &str) -> Result<Value, String> {
    plain_skill_name(name)?;
    Ok(json!({ "user": true, "name": name }))
}

/// A skill name is one path component (rsid checks it too).
fn plain_skill_name(name: &str) -> Result<(), String> {
    let plain = !name.is_empty()
        && name.len() <= MAX_SKILL_NAME_BYTES
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0']);
    if !plain {
        return Err(format!("'{}' is not a skill name", name.escape_debug()));
    }
    Ok(())
}

/// `SetSkillEnabled` (#97): the TUI's Settings › Skills `e` on one of the
/// daemon user's skills (rsid renames `<name>` <-> `<name>.disabled`).
pub fn set_skill_enabled_params(name: &str, enabled: bool) -> Result<Value, String> {
    plain_skill_name(name)?;
    Ok(json!({ "name": name, "enabled": enabled }))
}

/// `DeleteSkill` (#97): the TUI's `d`; rsid removes the skill directory. The
/// desktop asks for a second `d` before it calls this.
pub fn delete_skill_params(name: &str) -> Result<Value, String> {
    plain_skill_name(name)?;
    Ok(json!({ "name": name }))
}

#[tauri::command]
pub async fn list_skills(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_LIST_SKILLS, list_skills_params()).await
}

#[tauri::command]
pub async fn get_skill(state: State<'_, AppState>, name: String) -> CmdResult {
    forward(&state, METHOD_GET_SKILL, get_skill_params(&name)?).await
}

#[tauri::command]
pub async fn set_skill_enabled(
    state: State<'_, AppState>,
    name: String,
    enabled: bool,
) -> CmdResult {
    forward(
        &state,
        METHOD_SET_SKILL_ENABLED,
        set_skill_enabled_params(&name, enabled)?,
    )
    .await
}

#[tauri::command]
pub async fn delete_skill(state: State<'_, AppState>, name: String) -> CmdResult {
    forward(&state, METHOD_DELETE_SKILL, delete_skill_params(&name)?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "5f0c6a52-6a43-4d5c-9a0e-3f3b8d1c2e71";

    #[test]
    fn queue_params_match_the_tui_request() {
        let p = queue_params(SID, "do it", "0b6f8e1c-4b5a-4f1e-8c2d-3a4b5c6d7e8f").unwrap();
        assert_eq!(p["session_id"], SID);
        assert_eq!(p["content"], "do it");
        assert_eq!(p["idempotency_key"], "0b6f8e1c-4b5a-4f1e-8c2d-3a4b5c6d7e8f");
        assert!(queue_params("nope", "x", SID).is_err());
        assert!(queue_params(SID, "  ", SID).is_err());
    }

    #[test]
    fn generate_params_pick_the_tui_system_prompts() {
        let g = generate_params("teh cat", None).unwrap();
        assert_eq!(g["system"], GRAMMAR_SYSTEM_PROMPT);
        assert_eq!(g["prompt"], "teh cat");
        let c = generate_params("draft", Some("why?")).unwrap();
        assert_eq!(c["system"], AI_CHAT_SYSTEM_PROMPT);
        assert_eq!(c["prompt"], "Text:\ndraft\n\nQuestion: why?");
        assert!(generate_params(" ", None).is_err());
        assert!(generate_params("x", Some(" ")).is_err());
    }

    #[test]
    fn compile_events_match_their_request() {
        let done = br#"{"event_type":"compile_prompt_completed","data":{"request_id":"r1","result":{"compiled":"x"}}}"#;
        assert_eq!(compile_event(done, "r1"), Some(Ok(json!({"compiled":"x"}))));
        assert_eq!(compile_event(done, "r2"), None);
        let failed = br#"{"event_type":"compile_prompt_failed","data":{"request_id":"r1","error":"Ambiguous intent: which?"}}"#;
        assert_eq!(
            compile_event(failed, "r1"),
            Some(Err("Ambiguous intent: which?".into()))
        );
        assert_eq!(compile_event(b"not json", "r1"), None);
    }

    #[tokio::test]
    async fn bounded_lines_skip_an_oversize_line() {
        let data: &[u8] = b"short\nthis line is far too long\nok\n";
        let mut r = BufReader::new(data);
        assert_eq!(
            read_line(&mut r, 8).await.unwrap(),
            Line::Data(b"short\n".to_vec())
        );
        assert_eq!(read_line(&mut r, 8).await.unwrap(), Line::Oversize);
        assert_eq!(
            read_line(&mut r, 8).await.unwrap(),
            Line::Data(b"ok\n".to_vec())
        );
        assert_eq!(read_line(&mut r, 8).await.unwrap(), Line::Eof);
    }

    const PID: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const PID_LOWER: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";

    #[test]
    fn upload_requests_carry_an_allowed_type_and_bounded_data() {
        let p = upload_params(&json!({"mime": " Image/PNG ", "data_base64": "iVBORw=="})).unwrap();
        assert_eq!(p, json!({"mime": "image/png", "data_base64": "iVBORw=="}));
        for bad in [
            json!({"mime": "image/svg+xml", "data_base64": "PHN2Zz4="}),
            json!({"mime": "image/png", "data_base64": ""}),
            json!({"mime": "image/png"}),
            json!({"data_base64": "iVBORw=="}),
            json!({"mime": "image/png", "data_base64": "iVBORw==", "path": "/etc/x"}),
            json!({"mime": "image/png", "data_base64": "A".repeat(MAX_IMAGE_BYTES.div_ceil(3) * 4 + 4)}),
            json!("image/png"),
        ] {
            assert!(upload_params(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn slash_requests_name_exactly_one_root() {
        assert_eq!(
            slash_params(&json!({"project_id": PID})).unwrap(),
            json!({"project_id": PID_LOWER, "max_entries": 500})
        );
        assert_eq!(
            slash_params(&json!({"session_id": SID, "project_id": null})).unwrap(),
            json!({"session_id": SID, "max_entries": 500})
        );
        for bad in [
            json!({}),
            json!({"project_id": PID, "session_id": SID}),
            json!({"working_dir": "/repo"}),
            json!({"session_id": "not-a-uuid"}),
        ] {
            assert!(slash_params(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn skill_requests_name_the_user_tier_by_plain_name() {
        assert_eq!(
            list_skills_params(),
            json!({"user": true, "max_entries": 500})
        );
        assert_eq!(
            get_skill_params("debug").unwrap(),
            json!({"user": true, "name": "debug"})
        );
        assert_eq!(
            set_skill_enabled_params("debug", false).unwrap(),
            json!({"name": "debug", "enabled": false})
        );
        assert_eq!(
            delete_skill_params("debug").unwrap(),
            json!({"name": "debug"})
        );
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "x\0"] {
            assert!(get_skill_params(bad).is_err(), "{bad:?}");
            assert!(set_skill_enabled_params(bad, true).is_err(), "{bad:?}");
            assert!(delete_skill_params(bad).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    /// Both commands are one daemon call each: the bytes on the wire are the
    /// method and the built params, and the daemon's answer comes back as is.
    #[tokio::test]
    async fn paste_and_slash_commands_go_to_the_daemon() {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let replies = [
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"path\":\"/h/.rsi/paste/rsid/ab.png\",\"name\":\"ab.png\",\"mime\":\"image/png\",\"bytes\":4,\"sha256\":\"ab\",\"reused\":false}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"commands\":[{\"name\":\"plan\",\"description\":\"Plan it\",\"source\":\"command\"}],\"truncated\":false,\"skipped_outside_root\":0}}\n",
        ];
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for reply in replies {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                BufReader::new(r).read_line(&mut line).await.unwrap();
                seen.push(serde_json::from_str::<Value>(&line).unwrap());
                w.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        let client = crate::daemon::DaemonClient::new(&sock);
        let upload =
            upload_params(&json!({"mime": "image/png", "data_base64": "iVBORw=="})).unwrap();
        let stored = client
            .call(METHOD_UPLOAD_PASTE_IMAGE, upload.clone())
            .await
            .unwrap();
        assert_eq!(stored["path"], "/h/.rsi/paste/rsid/ab.png");
        let slash = slash_params(&json!({"session_id": SID})).unwrap();
        let listing = client
            .call(METHOD_LIST_SLASH_COMMANDS, slash.clone())
            .await
            .unwrap();
        assert_eq!(listing["commands"][0]["name"], "plan");
        let seen = server.await.unwrap();
        assert_eq!(seen[0]["method"], METHOD_UPLOAD_PASTE_IMAGE);
        assert_eq!(seen[0]["params"], upload);
        assert_eq!(seen[1]["method"], METHOD_LIST_SLASH_COMMANDS);
        assert_eq!(seen[1]["params"], slash);
    }

    /// hub #3044: the backend no longer reads or writes the paste directory
    /// or `.claude` itself. This module's code (its tests aside) has no file
    /// API, and no backend module names either location. The needles are
    /// built with `concat!` so this test does not match itself.
    #[test]
    fn the_backend_touches_neither_the_paste_dir_nor_claude_files() {
        let code = |source: &str| -> String {
            source
                .split("#[cfg(test)]\nmod tests {")
                .next()
                .unwrap()
                .to_string()
        };
        let input = code(include_str!("input.rs"));
        for api in [
            concat!("std", "::fs"),
            concat!("tokio", "::fs"),
            concat!("File", "::"),
            concat!("read", "_dir"),
            concat!("create", "_dir"),
        ] {
            assert!(!input.contains(api), "input.rs uses {api}");
        }
        let src = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        let mut scanned = 0;
        for entry in std::fs::read_dir(src).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let body = code(&std::fs::read_to_string(&path).unwrap());
            for needle in [
                concat!("\".", "claude"),
                concat!("\"", "paste\""),
                concat!(".rsi/", "paste"),
                concat!("save_paste", "_image"),
                concat!("discover_slash", "_commands"),
            ] {
                assert!(!body.contains(needle), "{} names {needle}", path.display());
            }
            scanned += 1;
        }
        assert!(scanned > 10, "scanned {scanned} backend modules");
    }
}
