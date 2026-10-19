//! Command Mode rows (hub #3000 gap sweep G3): `:manual`, `:rotation-abandon`,
//! `:manager-reseat`, `:cold-restore`, `:cold-archive` and `:context`. Each
//! daemon command validates its input and forwards exactly one method with the
//! params the TUI client sends (`crates/rsi/src/client.rs`):
//! `AbandonBlockedRotation`, `ReseatHarnessManager`, `RestoreColdSession`,
//! `GetColdArchiveStatus`, `RunColdArchiveNow` and `UpdateActiveTask`. These
//! are operator RPCs the TUI already calls; nothing here touches the agent verb
//! lists. `manual_sections` is a no-daemon helper: the generated manual
//! (`docs/manual/rsi-manual.md`, kept current by the TUI's drift tests) is
//! compiled in and rendered with the transcript's sanitizer.

use rsi_common::manager_reseat::ReseatHarnessManagerParams;
use rsi_common::rpc::AbandonBlockedRotationParams;
use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, PROVIDERS, forward, parse_uuid};

pub const METHOD_ABANDON: &str = "AbandonBlockedRotation";
pub const METHOD_RESEAT: &str = "ReseatHarnessManager";
pub const METHOD_COLD_RESTORE: &str = "RestoreColdSession";
pub const METHOD_COLD_STATUS: &str = "GetColdArchiveStatus";
pub const METHOD_COLD_RUN: &str = "RunColdArchiveNow";
pub const METHOD_ACTIVE_TASK: &str = "UpdateActiveTask";

/// The modes `RunColdArchiveNow` takes (`:cold-archive run` is `stage`).
pub const COLD_MODES: &[&str] = &["stage", "move", "verify", "compact"];
/// `:context` caps the active task at 500 bytes (commands.rs `ActionId::Context`).
pub const ACTIVE_TASK_MAX_BYTES: usize = 500;

/// The generated manual, as the TUI's `:manual` renders it.
const MANUAL: &str = include_str!("../../../docs/manual/rsi-manual.md");

fn provider(raw: &str) -> Result<rsi_common::types::SessionProvider, String> {
    if !PROVIDERS.contains(&raw) {
        return Err(format!("unknown provider {raw}"));
    }
    serde_json::from_value(Value::String(raw.to_string())).map_err(|e| e.to_string())
}

fn model(raw: Option<&str>) -> Result<Option<String>, String> {
    match raw.map(str::trim) {
        None => Ok(None),
        Some("") => Err("model is empty".into()),
        Some(m) if m.len() > 256 || m.contains('\0') => Err("model is malformed".into()),
        Some(m) => Ok(Some(m.to_string())),
    }
}

fn key(raw: &str) -> Result<String, String> {
    if raw.trim().is_empty() || raw.len() > 128 || raw.contains('\0') {
        return Err("idempotency_key must be 1..=128 bytes without NUL".into());
    }
    Ok(raw.to_string())
}

/// `AbandonBlockedRotation {session_id, provider?, model?, idempotency_key}`:
/// no provider keeps the blocked holder's provider and model.
pub fn abandon_params(
    session_id: &str,
    provider_name: Option<&str>,
    model_name: Option<&str>,
    idempotency_key: &str,
) -> Result<Value, String> {
    let session_id = parse_uuid("session_id", session_id)?;
    let provider = provider_name.map(provider).transpose()?;
    let model = model(model_name)?;
    if provider.is_none() && model.is_some() {
        return Err("a model needs its provider (provider/model)".into());
    }
    let params = AbandonBlockedRotationParams {
        session_id: uuid::Uuid::parse_str(&session_id).map_err(|e| e.to_string())?,
        provider,
        model,
        idempotency_key: key(idempotency_key)?,
    };
    serde_json::to_value(params).map_err(|e| e.to_string())
}

/// `ReseatHarnessManager` as `:manager-reseat` sends it: no effort, and the
/// operator's request asserts the seat's provider is down.
pub fn reseat_params(
    project_id: &str,
    provider_name: &str,
    model_name: Option<&str>,
    reason: &str,
) -> Result<Value, String> {
    let params = ReseatHarnessManagerParams {
        project_id: uuid::Uuid::parse_str(&parse_uuid("project_id", project_id)?)
            .map_err(|e| e.to_string())?,
        provider: provider(provider_name)?,
        model: model(model_name)?,
        effort: None,
        reason: reason.to_string(),
        provider_unavailable: true,
    }
    .normalized()
    .map_err(|code| format!("{code}: the reason must be 1..=512 bytes"))?;
    serde_json::to_value(params).map_err(|e| e.to_string())
}

pub fn session_params(session_id: &str) -> Result<Value, String> {
    Ok(json!({ "session_id": parse_uuid("session_id", session_id)? }))
}

/// `RunColdArchiveNow {mode}`.
pub fn cold_run_params(mode: &str) -> Result<Value, String> {
    if !COLD_MODES.contains(&mode) {
        return Err(format!("mode must be one of {}", COLD_MODES.join(", ")));
    }
    Ok(json!({ "mode": mode }))
}

/// The longest prefix of `text` within `max` bytes that ends on a character.
fn cap_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `UpdateActiveTask {session_id, active_task}`: text sets it (capped at 500
/// bytes), none or blank clears it.
pub fn active_task_params(session_id: &str, active_task: Option<&str>) -> Result<Value, String> {
    let task = active_task
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| cap_bytes(t, ACTIVE_TASK_MAX_BYTES).to_string());
    if task.as_deref().is_some_and(|t| t.contains('\0')) {
        return Err("the active task contains NUL".into());
    }
    Ok(json!({ "session_id": parse_uuid("session_id", session_id)?, "active_task": task }))
}

/// The manual split at its `## ` headings: `(title, markdown)` per section.
pub fn manual_chunks(manual: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut fence = false;
    for line in manual.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
        }
        let heading = (!fence)
            .then(|| line.strip_prefix("## "))
            .flatten()
            .map(|h| h.trim().to_string());
        match heading {
            Some(title) => out.push((title, format!("{line}\n"))),
            None => match out.last_mut() {
                Some((_, body)) => {
                    body.push_str(line);
                    body.push('\n');
                }
                None => out.push(("Introduction".into(), format!("{line}\n"))),
            },
        }
    }
    out
}

// ---------- commands ----------

/// The manual's sections as `{title, html}` (sanitized like the transcript).
#[tauri::command]
pub fn manual_sections() -> Result<Value, String> {
    let chunks = manual_chunks(MANUAL);
    let texts: Vec<String> = chunks.iter().map(|(_, md)| md.clone()).collect();
    let html = crate::markdown::render_batch(&texts)?;
    Ok(Value::Array(
        chunks
            .into_iter()
            .zip(html)
            .map(|((title, _), html)| json!({ "title": title, "html": html }))
            .collect(),
    ))
}

#[tauri::command]
pub async fn abandon_rotation(
    state: State<'_, AppState>,
    session_id: String,
    provider: Option<String>,
    model: Option<String>,
    idempotency_key: String,
) -> CmdResult {
    let params = abandon_params(
        &session_id,
        provider.as_deref(),
        model.as_deref(),
        &idempotency_key,
    )?;
    forward(&state, METHOD_ABANDON, params).await
}

#[tauri::command]
pub async fn reseat_manager(
    state: State<'_, AppState>,
    project_id: String,
    provider: String,
    model: Option<String>,
    reason: String,
) -> CmdResult {
    let params = reseat_params(&project_id, &provider, model.as_deref(), &reason)?;
    forward(&state, METHOD_RESEAT, params).await
}

#[tauri::command]
pub async fn cold_restore(state: State<'_, AppState>, session_id: String) -> CmdResult {
    forward(&state, METHOD_COLD_RESTORE, session_params(&session_id)?).await
}

#[tauri::command]
pub async fn cold_archive_status(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_COLD_STATUS, json!({})).await
}

#[tauri::command]
pub async fn cold_archive_run(state: State<'_, AppState>, mode: String) -> CmdResult {
    forward(&state, METHOD_COLD_RUN, cold_run_params(&mode)?).await
}

#[tauri::command]
pub async fn update_active_task(
    state: State<'_, AppState>,
    session_id: String,
    active_task: Option<String>,
) -> CmdResult {
    let params = active_task_params(&session_id, active_task.as_deref())?;
    forward(&state, METHOD_ACTIVE_TASK, params).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";
    const P: &str = "0b7c8a52-5f63-4a9e-9d27-3c1a9a5b6e10";

    #[test]
    fn abandon_keeps_the_holder_launch_or_names_a_new_one() {
        assert_eq!(
            abandon_params(S, None, None, "k").unwrap(),
            json!({"session_id": S, "provider": null, "model": null, "idempotency_key": "k"})
        );
        assert_eq!(
            abandon_params(S, Some("Codex"), Some("gpt-6-astra"), "k").unwrap(),
            json!({"session_id": S, "provider": "Codex", "model": "gpt-6-astra", "idempotency_key": "k"})
        );
        assert!(abandon_params(S, Some("codex"), None, "k").is_err());
        assert!(abandon_params(S, None, Some("m"), "k").is_err());
        assert!(abandon_params(S, Some("Claude"), Some(" "), "k").is_err());
        assert!(abandon_params("x", None, None, "k").is_err());
        assert!(abandon_params(S, None, None, "").is_err());
    }

    #[test]
    fn reseat_asserts_the_provider_is_down_and_needs_a_reason() {
        assert_eq!(
            reseat_params(P, "Codex", Some("gpt-6-astra"), "  claude is down ").unwrap(),
            json!({
                "project_id": P, "provider": "Codex", "model": "gpt-6-astra", "effort": null,
                "reason": "claude is down", "provider_unavailable": true
            })
        );
        assert!(reseat_params(P, "Codex", None, "  ").is_err());
        assert!(reseat_params(P, "Nope", None, "r").is_err());
        assert!(reseat_params("x", "Codex", None, "r").is_err());
    }

    #[test]
    fn cold_runs_name_one_mode() {
        assert_eq!(cold_run_params("stage").unwrap(), json!({"mode": "stage"}));
        assert_eq!(
            cold_run_params("compact").unwrap(),
            json!({"mode": "compact"})
        );
        assert!(cold_run_params("run").is_err());
        assert_eq!(session_params(S).unwrap(), json!({"session_id": S}));
    }

    #[test]
    fn the_active_task_is_capped_on_a_character_and_blank_clears_it() {
        assert_eq!(
            active_task_params(S, Some(" ship it ")).unwrap(),
            json!({"session_id": S, "active_task": "ship it"})
        );
        assert_eq!(
            active_task_params(S, None).unwrap(),
            json!({"session_id": S, "active_task": null})
        );
        assert_eq!(
            active_task_params(S, Some("  ")).unwrap()["active_task"],
            Value::Null
        );
        let long = "é".repeat(400);
        let capped = active_task_params(S, Some(&long)).unwrap();
        let text = capped["active_task"].as_str().unwrap();
        assert_eq!(text.len(), 500);
        assert!(text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn the_manual_splits_at_its_sections_and_renders() {
        let chunks =
            manual_chunks("# T\nintro\n## One\na\n```\n## not a heading\n```\n## Two\nb\n");
        let titles: Vec<&str> = chunks.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(titles, ["Introduction", "One", "Two"]);
        assert!(chunks[1].1.contains("## not a heading"));
        let sections = manual_sections().unwrap();
        let sections = sections.as_array().unwrap();
        assert!(sections.len() > 3);
        assert!(
            sections
                .iter()
                .all(|s| !s["html"].as_str().unwrap().contains("<script"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn each_command_sends_its_method_and_params_to_the_daemon() {
        use crate::daemon::DaemonClient;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen2 = Arc::clone(&seen);
        let calls = vec![
            (
                METHOD_ABANDON,
                abandon_params(S, Some("Claude"), None, "k").unwrap(),
            ),
            (
                METHOD_RESEAT,
                reseat_params(P, "Codex", None, "down").unwrap(),
            ),
            (METHOD_COLD_RESTORE, session_params(S).unwrap()),
            (METHOD_COLD_STATUS, json!({})),
            (METHOD_COLD_RUN, cold_run_params("verify").unwrap()),
            (
                METHOD_ACTIVE_TASK,
                active_task_params(S, Some("t")).unwrap(),
            ),
        ];
        let n = calls.len();
        tokio::spawn(async move {
            for _ in 0..n {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                BufReader::new(r).read_line(&mut line).await.unwrap();
                seen2.lock().unwrap().push(line);
                w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
                    .await
                    .unwrap();
            }
        });
        let client = DaemonClient::new(&sock);
        for (method, params) in &calls {
            client.call(method, params.clone()).await.unwrap();
        }
        let sent: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(sent.len(), n);
        for (i, (method, params)) in calls.iter().enumerate() {
            assert_eq!(sent[i]["method"], *method);
            assert_eq!(sent[i]["params"], *params);
        }
    }
}
