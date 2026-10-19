//! Session operations (hub #3000 gap sweep G2): pin, testing-needed,
//! auto-rotation, cancel retry, rotate and landing a session branch. Each
//! command validates its input and forwards exactly one daemon method, with
//! the params the TUI client sends (`crates/rsi/src/client.rs`): `TogglePin`,
//! `ToggleTestingNeeded`, `ToggleRotationDisabled`, `CancelRetry`,
//! `RotateSession`, `EnqueueRollingQueueSource` and `GetRollingQueue`
//! (`entry_ids`, the land watch's read). Commit and push is the existing
//! `send_message` (`ContinueSession` with `/ci_commit`).

use std::time::Duration;

use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward, parse_uuid};

pub const METHOD_TOGGLE_PIN: &str = "TogglePin";
pub const METHOD_TOGGLE_TESTING: &str = "ToggleTestingNeeded";
pub const METHOD_TOGGLE_ROTATION: &str = "ToggleRotationDisabled";
pub const METHOD_CANCEL_RETRY: &str = "CancelRetry";
pub const METHOD_ROTATE: &str = "RotateSession";
pub const METHOD_ENQUEUE: &str = "EnqueueRollingQueueSource";
pub const METHOD_QUEUE: &str = "GetRollingQueue";

/// The daemon answers `RotateSession` once the rotation is under way, which
/// can take longer than an ordinary read (the TUI fires and forgets it).
pub const ROTATE_DEADLINE: Duration = Duration::from_secs(120);
/// Entries one queue read may name (`ROLLING_QUEUE_WATCH_IDS_MAX`).
pub const MAX_WATCH_IDS: usize = 64;
/// Idempotency keys are short ASCII (`operator-land-<uuid>`).
pub const MAX_KEY_BYTES: usize = 128;

pub fn session_params(session_id: &str) -> Result<Value, String> {
    Ok(json!({ "session_id": parse_uuid("session_id", session_id)? }))
}

/// A full lowercase git object id (40 or 64 hex digits), as the queue requires.
fn commit_param(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(c) = raw.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    let hex = c
        .chars()
        .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch));
    if (c.len() == 40 || c.len() == 64) && hex {
        Ok(Some(c.to_string()))
    } else {
        Err("commit must be a full lowercase commit id".into())
    }
}

fn key_param(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(k) = raw.map(str::trim).filter(|k| !k.is_empty()) else {
        return Ok(None);
    };
    if k.len() > MAX_KEY_BYTES
        || !k
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("idempotency key must be short ASCII letters, digits, - or _".into());
    }
    Ok(Some(k.to_string()))
}

/// `EnqueueRollingQueueSource` for a session (portable_cli::land_params): a
/// dry run resolves and shows the commit; the real call queues exactly that
/// commit under a fresh key, with the filters the queue derives.
pub fn land_params(
    session_id: &str,
    commit: Option<&str>,
    idempotency_key: Option<&str>,
    dry_run: bool,
) -> Result<Value, String> {
    let commit = commit_param(commit)?;
    let key = key_param(idempotency_key)?;
    if !dry_run && key.is_none() {
        return Err("a landing needs an idempotency key".into());
    }
    if !dry_run && commit.is_none() {
        return Err("a landing names the commit the dry run showed".into());
    }
    Ok(json!({
        "session_id": parse_uuid("session_id", session_id)?,
        "branch": Value::Null,
        "commit": commit,
        "derive_filters": true,
        "test_filters": [],
        "idempotency_key": key,
        "dry_run": dry_run,
    }))
}

/// `GetRollingQueue` by entry id (client.rs get_rolling_queue_entries).
pub fn queue_entries_params(entry_ids: &[String]) -> Result<Value, String> {
    if entry_ids.is_empty() || entry_ids.len() > MAX_WATCH_IDS {
        return Err(format!("name 1 to {MAX_WATCH_IDS} queue entries"));
    }
    let ids = entry_ids
        .iter()
        .map(|id| parse_uuid("entry id", id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "entry_ids": ids }))
}

#[tauri::command]
pub async fn toggle_pin(state: State<'_, AppState>, session_id: String) -> CmdResult {
    forward(&state, METHOD_TOGGLE_PIN, session_params(&session_id)?).await
}

#[tauri::command]
pub async fn toggle_testing_needed(state: State<'_, AppState>, session_id: String) -> CmdResult {
    forward(&state, METHOD_TOGGLE_TESTING, session_params(&session_id)?).await
}

#[tauri::command]
pub async fn toggle_rotation_disabled(state: State<'_, AppState>, session_id: String) -> CmdResult {
    forward(&state, METHOD_TOGGLE_ROTATION, session_params(&session_id)?).await
}

#[tauri::command]
pub async fn cancel_retry(state: State<'_, AppState>, session_id: String) -> CmdResult {
    forward(&state, METHOD_CANCEL_RETRY, session_params(&session_id)?).await
}

#[tauri::command]
pub async fn rotate_session(state: State<'_, AppState>, session_id: String) -> CmdResult {
    let params = session_params(&session_id)?;
    state
        .client
        .clone()
        .with_deadline(ROTATE_DEADLINE)
        .call(METHOD_ROTATE, params)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn land_session(
    state: State<'_, AppState>,
    session_id: String,
    commit: Option<String>,
    idempotency_key: Option<String>,
    dry_run: bool,
) -> CmdResult {
    let p = land_params(
        &session_id,
        commit.as_deref(),
        idempotency_key.as_deref(),
        dry_run,
    )?;
    forward(&state, METHOD_ENQUEUE, p).await
}

#[tauri::command]
pub async fn rolling_queue_entries(
    state: State<'_, AppState>,
    entry_ids: Vec<String>,
) -> CmdResult {
    forward(&state, METHOD_QUEUE, queue_entries_params(&entry_ids)?).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::daemon::DaemonClient;
    #[cfg(unix)]
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::sync::{Arc, Mutex};
    #[cfg(unix)]
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    #[cfg(unix)]
    use tokio::net::UnixListener;

    const S: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const S_LOWER: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";

    #[test]
    fn session_params_need_a_uuid() {
        assert_eq!(session_params(S).unwrap(), json!({ "session_id": S_LOWER }));
        assert!(session_params("nope").is_err());
    }

    #[test]
    fn a_dry_run_names_the_session_and_derives_filters() {
        let p = land_params(S, None, None, true).unwrap();
        assert_eq!(
            p,
            json!({
                "session_id": S_LOWER, "branch": null, "commit": null, "derive_filters": true,
                "test_filters": [], "idempotency_key": null, "dry_run": true,
            })
        );
    }

    #[test]
    fn a_landing_needs_the_shown_commit_and_a_key() {
        let c = "c".repeat(40);
        let p = land_params(S, Some(&c), Some("operator-land-1"), false).unwrap();
        assert_eq!(p["commit"], c);
        assert_eq!(p["idempotency_key"], "operator-land-1");
        assert_eq!(p["dry_run"], false);
        assert!(land_params(S, Some(&c), None, false).is_err());
        assert!(land_params(S, None, Some("k"), false).is_err());
        assert!(land_params(S, Some("abc"), Some("k"), false).is_err());
        assert!(land_params(S, Some(&"C".repeat(40)), Some("k"), false).is_err());
        assert!(land_params(S, Some(&c), Some("bad key"), false).is_err());
        assert!(land_params(S, Some(&"d".repeat(64)), Some("k"), false).is_ok());
    }

    #[test]
    fn queue_reads_name_one_to_sixty_four_entries() {
        let p = queue_entries_params(&[S.to_string()]).unwrap();
        assert_eq!(p, json!({ "entry_ids": [S_LOWER] }));
        assert!(queue_entries_params(&[]).is_err());
        assert!(queue_entries_params(&vec![S.to_string(); 65]).is_err());
        assert!(queue_entries_params(&["x".to_string()]).is_err());
    }

    #[cfg(unix)]
    fn fake_daemon(dir: &Path, replies: Vec<&'static str>) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
        let sock = dir.join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        tokio::spawn(async move {
            for reply in replies {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                BufReader::new(r).read_line(&mut line).await.unwrap();
                seen2.lock().unwrap().push(line);
                w.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (sock, seen)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn each_command_sends_its_method_and_params_to_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let ok = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
        let (sock, seen) = fake_daemon(dir.path(), vec![ok; 7]);
        let client = DaemonClient::new(&sock);
        let c = "a".repeat(40);
        let calls = [
            (METHOD_TOGGLE_PIN, session_params(S).unwrap()),
            (METHOD_TOGGLE_TESTING, session_params(S).unwrap()),
            (METHOD_TOGGLE_ROTATION, session_params(S).unwrap()),
            (METHOD_CANCEL_RETRY, session_params(S).unwrap()),
            (METHOD_ROTATE, session_params(S).unwrap()),
            (
                METHOD_ENQUEUE,
                land_params(S, Some(&c), Some("operator-land-x"), false).unwrap(),
            ),
            (
                METHOD_QUEUE,
                queue_entries_params(&[S.to_string()]).unwrap(),
            ),
        ];
        for (method, params) in &calls {
            client.call(method, params.clone()).await.unwrap();
        }
        let sent: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(sent.len(), calls.len());
        for (i, (method, params)) in calls.iter().enumerate() {
            assert_eq!(sent[i]["method"], *method);
            assert_eq!(sent[i]["params"], *params);
        }
        assert!(sent.iter().all(|r| r.get("session_token").is_none()));
    }
}
