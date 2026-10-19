//! Project file commands (slice 10B and #113, hub #3012): the file explorer,
//! the fuzzy file picker and the file viewer/editor reach files only through
//! rsid's project-root-scoped methods: the reads (`ListProjectDirectory`,
//! `ListProjectFiles`, `ReadProjectFile`, `GetProjectFileGitGutter`) and the
//! operator-only writes (`WriteProjectFile` with its version fence and force,
//! `CreateProjectEntry`, `MoveProjectEntry`, `CopyProjectEntry`,
//! `TrashProjectEntry`, `UndoProjectFileOperation`); wire types in
//! `crates/rsi-common/src/project_files.rs`. The desktop never touches the
//! filesystem itself: every write command below is a validated forwarder, and
//! rsid owns the confinement, the stale-version fence and the undo log.
//!
//! Each command checks the request's shape (exactly one of `project_id` or
//! `session_id` as a UUID, a relative `path` string, the caps in range, no
//! unknown field) and forwards exactly one daemon method. rsid owns the scope:
//! it canonicalizes the path and refuses one that leaves the root
//! (`project_file_outside_root`), so this module does not judge `..` or
//! symlinks. The request builders are pure and unit-tested, and the tests
//! below also send each one to a fake daemon to pin the bytes on the wire.

use serde_json::{Map, Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward, parse_uuid};

pub const METHOD_LIST_DIRECTORY: &str = "ListProjectDirectory";
pub const METHOD_LIST_FILES: &str = "ListProjectFiles";
pub const METHOD_READ_FILE: &str = "ReadProjectFile";
pub const METHOD_GIT_GUTTER: &str = "GetProjectFileGitGutter";
pub const METHOD_WRITE_FILE: &str = "WriteProjectFile";
pub const METHOD_CREATE_ENTRY: &str = "CreateProjectEntry";
pub const METHOD_MOVE_ENTRY: &str = "MoveProjectEntry";
pub const METHOD_COPY_ENTRY: &str = "CopyProjectEntry";
pub const METHOD_TRASH_ENTRY: &str = "TrashProjectEntry";
pub const METHOD_UNDO: &str = "UndoProjectFileOperation";

/// Ceilings rsid clamps to (`rsi_common::project_files`); a larger ask is a
/// frontend bug, so it is refused here rather than silently clamped.
pub const MAX_DIRECTORY_ENTRIES: u64 = 10_000;
pub const MAX_PICKER_PATHS: u64 = 100_000;
pub const MAX_READ_BYTES: u64 = 4 << 20;
/// Longest relative path the desktop sends (bytes).
pub const MAX_PATH_BYTES: usize = 4_096;
/// Largest `WriteProjectFile.content` (`rsi_common::project_files::MAX_WRITE_BYTES`).
pub const MAX_WRITE_BYTES: usize = 8 << 20;
/// Longest `expected_version` token (`v1:<size>:<mtime nanos>:<sha256 hex>` is under 120 bytes).
pub const MAX_VERSION_BYTES: usize = 256;

type Built = Result<Value, String>;

fn object<'a>(v: &'a Value) -> Result<&'a Map<String, Value>, String> {
    v.as_object()
        .ok_or_else(|| "request must be an object".to_string())
}

/// Copy the root (exactly one of `project_id` / `session_id`) into `out`.
fn root(o: &Map<String, Value>, out: &mut Map<String, Value>) -> Result<(), String> {
    let pick = |field: &str| -> Result<Option<String>, String> {
        match o.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => parse_uuid(field, s).map(Some),
            Some(_) => Err(format!("{field} must be a UUID string")),
        }
    };
    match (pick("project_id")?, pick("session_id")?) {
        (Some(p), None) => {
            out.insert("project_id".into(), json!(p));
        }
        (None, Some(s)) => {
            out.insert("session_id".into(), json!(s));
        }
        (Some(_), Some(_)) => {
            return Err("name exactly one of project_id and session_id, not both".into());
        }
        (None, None) => return Err("name a project_id or a session_id".into()),
    }
    Ok(())
}

/// The relative path: text without NUL, at most [`MAX_PATH_BYTES`].
fn path(o: &Map<String, Value>, required: bool) -> Result<String, String> {
    named_path(o, "path", required)
}

/// A relative path in the field `field` (`path`, `from` or `to`).
fn named_path(o: &Map<String, Value>, field: &str, required: bool) -> Result<String, String> {
    let s = match o.get(field) {
        None | Some(Value::Null) if !required => return Ok(String::new()),
        None | Some(Value::Null) => return Err(format!("{field} is required")),
        Some(Value::String(s)) => s,
        Some(_) => return Err(format!("{field} must be text")),
    };
    if s.len() > MAX_PATH_BYTES || s.contains('\0') {
        return Err(format!(
            "{field} must be at most {MAX_PATH_BYTES} bytes without NUL"
        ));
    }
    if required && s.trim().is_empty() {
        return Err(format!("{field} names no file"));
    }
    Ok(s.clone())
}

fn bounded(field: &str, v: &Value, max: u64) -> Result<u64, String> {
    match v.as_u64() {
        Some(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(format!("{field} must be 1..={max}")),
    }
}

/// Refuse a field this request does not have.
fn closed(o: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match o.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("unknown field {k}")),
        None => Ok(()),
    }
}

/// `ListProjectDirectory`: `{project_id|session_id, path?, max_entries?}`.
pub fn list_directory_params(request: &Value) -> Built {
    let o = object(request)?;
    closed(o, &["project_id", "session_id", "path", "max_entries"])?;
    let mut out = Map::new();
    root(o, &mut out)?;
    out.insert("path".into(), json!(path(o, false)?));
    if let Some(v) = o.get("max_entries").filter(|v| !v.is_null()) {
        out.insert(
            "max_entries".into(),
            json!(bounded("max_entries", v, MAX_DIRECTORY_ENTRIES)?),
        );
    }
    Ok(Value::Object(out))
}

/// `ListProjectFiles`: `{project_id|session_id, path?, max_paths?}`.
pub fn list_files_params(request: &Value) -> Built {
    let o = object(request)?;
    closed(o, &["project_id", "session_id", "path", "max_paths"])?;
    let mut out = Map::new();
    root(o, &mut out)?;
    out.insert("path".into(), json!(path(o, false)?));
    if let Some(v) = o.get("max_paths").filter(|v| !v.is_null()) {
        out.insert(
            "max_paths".into(),
            json!(bounded("max_paths", v, MAX_PICKER_PATHS)?),
        );
    }
    Ok(Value::Object(out))
}

/// `ReadProjectFile`: `{project_id|session_id, path, max_bytes?, start_line?, end_line?}`.
pub fn read_file_params(request: &Value) -> Built {
    let o = object(request)?;
    closed(
        o,
        &[
            "project_id",
            "session_id",
            "path",
            "max_bytes",
            "start_line",
            "end_line",
        ],
    )?;
    let mut out = Map::new();
    root(o, &mut out)?;
    out.insert("path".into(), json!(path(o, true)?));
    if let Some(v) = o.get("max_bytes").filter(|v| !v.is_null()) {
        out.insert(
            "max_bytes".into(),
            json!(bounded("max_bytes", v, MAX_READ_BYTES)?),
        );
    }
    let line = |field: &str| -> Result<Option<u64>, String> {
        match o.get(field).filter(|v| !v.is_null()) {
            None => Ok(None),
            Some(v) => bounded(field, v, u64::MAX).map(Some),
        }
    };
    let (start, end) = (line("start_line")?, line("end_line")?);
    if let (Some(s), Some(e)) = (start, end)
        && s > e
    {
        return Err("start_line must not be after end_line".into());
    }
    if let Some(s) = start {
        out.insert("start_line".into(), json!(s));
    }
    if let Some(e) = end {
        out.insert("end_line".into(), json!(e));
    }
    Ok(Value::Object(out))
}

/// `GetProjectFileGitGutter`: `{project_id|session_id, path}`.
pub fn git_gutter_params(request: &Value) -> Built {
    let o = object(request)?;
    closed(o, &["project_id", "session_id", "path"])?;
    let mut out = Map::new();
    root(o, &mut out)?;
    out.insert("path".into(), json!(path(o, true)?));
    Ok(Value::Object(out))
}

/// `WriteProjectFile`: `{project_id|session_id, path, content, expected_version?, force?}`.
/// `expected_version` is the opaque token `ReadProjectFile` answered; the
/// version fence and the force decision are rsid's.
pub fn write_file_params(request: &Value) -> Built {
    let o = object(request)?;
    closed(
        o,
        &[
            "project_id",
            "session_id",
            "path",
            "content",
            "expected_version",
            "force",
        ],
    )?;
    let mut out = Map::new();
    root(o, &mut out)?;
    out.insert("path".into(), json!(path(o, true)?));
    match o.get("content") {
        Some(Value::String(c)) if c.len() <= MAX_WRITE_BYTES => {
            out.insert("content".into(), json!(c));
        }
        Some(Value::String(_)) => {
            return Err(format!("content must be at most {MAX_WRITE_BYTES} bytes"));
        }
        _ => return Err("content must be text".into()),
    }
    match o.get("expected_version") {
        None | Some(Value::Null) => {}
        Some(Value::String(v)) if !v.is_empty() && v.len() <= MAX_VERSION_BYTES => {
            out.insert("expected_version".into(), json!(v));
        }
        Some(_) => {
            return Err(format!(
                "expected_version must be a non-empty token of at most {MAX_VERSION_BYTES} bytes"
            ));
        }
    }
    match o.get("force") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(f)) => {
            out.insert("force".into(), json!(f));
        }
        Some(_) => return Err("force must be true or false".into()),
    }
    Ok(Value::Object(out))
}

/// `CreateProjectEntry`: `{project_id|session_id, path, kind: "file"|"dir"}`.
pub fn create_entry_params(request: &Value) -> Built {
    let o = object(request)?;
    closed(o, &["project_id", "session_id", "path", "kind"])?;
    let mut out = Map::new();
    root(o, &mut out)?;
    out.insert("path".into(), json!(path(o, true)?));
    match o.get("kind").and_then(Value::as_str) {
        Some(k @ ("file" | "dir")) => {
            out.insert("kind".into(), json!(k));
        }
        _ => return Err("kind must be \"file\" or \"dir\"".into()),
    }
    Ok(Value::Object(out))
}

/// `MoveProjectEntry` / `CopyProjectEntry`: `{project_id|session_id, from, to}`.
pub fn transfer_entry_params(request: &Value) -> Built {
    let o = object(request)?;
    closed(o, &["project_id", "session_id", "from", "to"])?;
    let mut out = Map::new();
    root(o, &mut out)?;
    out.insert("from".into(), json!(named_path(o, "from", true)?));
    out.insert("to".into(), json!(named_path(o, "to", true)?));
    Ok(Value::Object(out))
}

/// `TrashProjectEntry`: `{project_id|session_id, path}`.
pub fn trash_entry_params(request: &Value) -> Built {
    let o = object(request)?;
    closed(o, &["project_id", "session_id", "path"])?;
    let mut out = Map::new();
    root(o, &mut out)?;
    out.insert("path".into(), json!(path(o, true)?));
    Ok(Value::Object(out))
}

/// `UndoProjectFileOperation`: `{project_id|session_id}` only.
pub fn undo_params(request: &Value) -> Built {
    let o = object(request)?;
    closed(o, &["project_id", "session_id"])?;
    let mut out = Map::new();
    root(o, &mut out)?;
    Ok(Value::Object(out))
}

#[tauri::command]
pub async fn list_project_directory(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(
        &state,
        METHOD_LIST_DIRECTORY,
        list_directory_params(&request)?,
    )
    .await
}

#[tauri::command]
pub async fn list_project_files(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_LIST_FILES, list_files_params(&request)?).await
}

#[tauri::command]
pub async fn read_project_file(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_READ_FILE, read_file_params(&request)?).await
}

#[tauri::command]
pub async fn get_project_file_git_gutter(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_GIT_GUTTER, git_gutter_params(&request)?).await
}

#[tauri::command]
pub async fn write_project_file(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_WRITE_FILE, write_file_params(&request)?).await
}

#[tauri::command]
pub async fn create_project_entry(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_CREATE_ENTRY, create_entry_params(&request)?).await
}

#[tauri::command]
pub async fn move_project_entry(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_MOVE_ENTRY, transfer_entry_params(&request)?).await
}

#[tauri::command]
pub async fn copy_project_entry(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_COPY_ENTRY, transfer_entry_params(&request)?).await
}

#[tauri::command]
pub async fn trash_project_entry(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_TRASH_ENTRY, trash_entry_params(&request)?).await
}

#[tauri::command]
pub async fn undo_project_file_operation(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, METHOD_UNDO, undo_params(&request)?).await
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

    const P: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const P_LOWER: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";
    const S: &str = "11111111-1111-4111-8111-111111111111";

    #[test]
    fn directory_request_names_one_root_and_defaults_the_path() {
        let p = list_directory_params(&json!({"project_id": P})).unwrap();
        assert_eq!(p, json!({"project_id": P_LOWER, "path": ""}));
        let p =
            list_directory_params(&json!({"session_id": S, "path": "src/ui", "max_entries": 500}))
                .unwrap();
        assert_eq!(
            p,
            json!({"session_id": S, "path": "src/ui", "max_entries": 500})
        );
        let p = list_directory_params(
            &json!({"project_id": P, "session_id": null, "max_entries": null}),
        )
        .unwrap();
        assert_eq!(p, json!({"project_id": P_LOWER, "path": ""}));
    }

    #[test]
    fn requests_refuse_a_bad_root_unknown_fields_and_out_of_range_caps() {
        for bad in [
            json!({}),
            json!({"project_id": P, "session_id": S}),
            json!({"project_id": "nope"}),
            json!({"project_id": 7}),
            json!({"project_id": P, "extra": 1}),
            json!({"project_id": P, "path": 3}),
            json!({"project_id": P, "path": "a\u{0}b"}),
            json!({"project_id": P, "path": "x".repeat(MAX_PATH_BYTES + 1)}),
            json!({"project_id": P, "max_entries": 0}),
            json!({"project_id": P, "max_entries": MAX_DIRECTORY_ENTRIES + 1}),
            json!({"project_id": P, "max_entries": -1}),
            json!([]),
        ] {
            assert!(list_directory_params(&bad).is_err(), "{bad}");
        }
        assert!(
            list_files_params(&json!({"project_id": P, "max_paths": MAX_PICKER_PATHS + 1}))
                .is_err()
        );
        assert!(list_files_params(&json!({"project_id": P, "max_entries": 5})).is_err());
        assert!(git_gutter_params(&json!({"project_id": P, "path": "a", "max_bytes": 5})).is_err());
    }

    #[test]
    fn picker_request_keeps_its_cap() {
        let p = list_files_params(&json!({"session_id": S, "max_paths": 20_000})).unwrap();
        assert_eq!(p, json!({"session_id": S, "path": "", "max_paths": 20_000}));
    }

    #[test]
    fn read_request_needs_a_path_and_an_ordered_line_range() {
        let p = read_file_params(
            &json!({"project_id": P, "path": "README.md", "max_bytes": 1_048_576}),
        )
        .unwrap();
        assert_eq!(
            p,
            json!({"project_id": P_LOWER, "path": "README.md", "max_bytes": 1_048_576})
        );
        let p = read_file_params(
            &json!({"project_id": P, "path": "a.rs", "start_line": 10, "end_line": 20}),
        )
        .unwrap();
        assert_eq!(p["start_line"], 10);
        assert_eq!(p["end_line"], 20);
        for bad in [
            json!({"project_id": P}),
            json!({"project_id": P, "path": ""}),
            json!({"project_id": P, "path": "a", "max_bytes": MAX_READ_BYTES + 1}),
            json!({"project_id": P, "path": "a", "start_line": 0}),
            json!({"project_id": P, "path": "a", "start_line": 9, "end_line": 3}),
        ] {
            assert!(read_file_params(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scope_checks_stay_with_rsid() {
        // The desktop forwards `..` and absolute paths unchanged: rsid refuses
        // them with project_file_outside_root, which the viewer shows calmly.
        let p = git_gutter_params(&json!({"project_id": P, "path": "../etc/passwd"})).unwrap();
        assert_eq!(p["path"], "../etc/passwd");
        let p = read_file_params(&json!({"project_id": P, "path": "/etc/passwd"})).unwrap();
        assert_eq!(p["path"], "/etc/passwd");
    }

    #[cfg(unix)]
    /// A fake rsid on a Unix socket: answers each connection with the next
    /// reply and records the request lines it received.
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
        let (sock, seen) = fake_daemon(
            dir.path(),
            vec![
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"path\":\"\",\"entries\":[],\"truncated\":false}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"paths\":[\"a\"],\"truncated\":false,\"source\":\"git\"}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"path\":\"a\",\"size\":1,\"binary\":false,\"content\":\"x\",\"truncated\":false}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"path\":\"a\",\"status\":\"tracked\",\"markers\":[{\"line\":1,\"kind\":\"added\"}]}}\n",
            ],
        );
        let client = DaemonClient::new(&sock);
        let calls = [
            (
                METHOD_LIST_DIRECTORY,
                list_directory_params(&json!({"project_id": P, "path": "src"})).unwrap(),
            ),
            (
                METHOD_LIST_FILES,
                list_files_params(&json!({"session_id": S, "max_paths": 100})).unwrap(),
            ),
            (
                METHOD_READ_FILE,
                read_file_params(&json!({"project_id": P, "path": "a", "max_bytes": 1024}))
                    .unwrap(),
            ),
            (
                METHOD_GIT_GUTTER,
                git_gutter_params(&json!({"project_id": P, "path": "a"})).unwrap(),
            ),
        ];
        let mut results = Vec::new();
        for (method, params) in &calls {
            results.push(client.call(method, params.clone()).await.unwrap());
        }
        assert_eq!(results[1]["source"], "git");
        assert_eq!(results[3]["markers"][0]["kind"], "added");
        let sent: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(sent.len(), 4);
        for (i, (method, params)) in calls.iter().enumerate() {
            assert_eq!(sent[i]["method"], *method);
            assert_eq!(sent[i]["params"], *params);
            assert_eq!(sent[i]["jsonrpc"], "2.0");
        }
        assert_eq!(
            sent[0]["params"],
            json!({"project_id": P_LOWER, "path": "src"})
        );
        assert_eq!(
            sent[1]["params"],
            json!({"session_id": S, "path": "", "max_paths": 100})
        );
        assert_eq!(
            sent[2]["params"],
            json!({"project_id": P_LOWER, "path": "a", "max_bytes": 1024})
        );
        assert_eq!(
            sent[3]["params"],
            json!({"project_id": P_LOWER, "path": "a"})
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_scope_refusal_reaches_the_webview_with_its_code() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, _) = fake_daemon(
            dir.path(),
            vec![
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"Invalid params: project_file_outside_root: '../x' has a '..' component\"}}\n",
            ],
        );
        let err = DaemonClient::new(&sock)
            .call(
                METHOD_READ_FILE,
                read_file_params(&json!({"project_id": P, "path": "../x"})).unwrap(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("project_file_outside_root"), "{err}");
    }

    #[test]
    fn write_request_carries_the_observed_version_and_force_and_nothing_else() {
        let p = write_file_params(&json!({
            "project_id": P, "path": "src/a.rs", "content": "fn main() {}\n",
            "expected_version": "v1:13:1:abc", "force": false,
        }))
        .unwrap();
        assert_eq!(
            p,
            json!({
                "project_id": P_LOWER, "path": "src/a.rs", "content": "fn main() {}\n",
                "expected_version": "v1:13:1:abc", "force": false,
            })
        );
        // No version and no force: rsid counts that a stale save, so the desktop sends neither.
        let p = write_file_params(&json!({"session_id": S, "path": "a", "content": ""})).unwrap();
        assert_eq!(p, json!({"session_id": S, "path": "a", "content": ""}));
        let p = write_file_params(
            &json!({"session_id": S, "path": "a", "content": "x", "expected_version": null, "force": true}),
        )
        .unwrap();
        assert_eq!(
            p,
            json!({"session_id": S, "path": "a", "content": "x", "force": true})
        );
    }

    #[test]
    fn write_request_refuses_a_bad_shape_before_the_daemon_sees_it() {
        let big = "x".repeat(MAX_WRITE_BYTES + 1);
        for bad in [
            json!({"project_id": P, "path": "a"}),
            json!({"project_id": P, "path": "a", "content": 3}),
            json!({"project_id": P, "path": "a", "content": big}),
            json!({"project_id": P, "path": "", "content": "x"}),
            json!({"project_id": P, "path": "a", "content": "x", "expected_version": ""}),
            json!({"project_id": P, "path": "a", "content": "x", "expected_version": 7}),
            json!({"project_id": P, "path": "a", "content": "x", "expected_version": "v".repeat(MAX_VERSION_BYTES + 1)}),
            json!({"project_id": P, "path": "a", "content": "x", "force": "yes"}),
            json!({"project_id": P, "path": "a", "content": "x", "mode": 511}),
            json!({"path": "a", "content": "x"}),
            json!({"project_id": P, "session_id": S, "path": "a", "content": "x"}),
        ] {
            let shown = bad.to_string();
            assert!(
                write_file_params(&bad).is_err(),
                "{}",
                &shown[..shown.len().min(120)]
            );
        }
        let max = "x".repeat(MAX_WRITE_BYTES);
        assert!(write_file_params(&json!({"project_id": P, "path": "a", "content": max})).is_ok());
    }

    #[test]
    fn explorer_requests_name_their_fields_and_refuse_the_rest() {
        assert_eq!(
            create_entry_params(&json!({"project_id": P, "path": "docs/", "kind": "dir"})).unwrap(),
            json!({"project_id": P_LOWER, "path": "docs/", "kind": "dir"})
        );
        for bad in [
            json!({"project_id": P, "path": "a", "kind": "symlink"}),
            json!({"project_id": P, "path": "a"}),
            json!({"project_id": P, "path": " ", "kind": "file"}),
            json!({"project_id": P, "path": "a", "kind": "file", "mode": 1}),
        ] {
            assert!(create_entry_params(&bad).is_err(), "{bad}");
        }
        assert_eq!(
            transfer_entry_params(&json!({"session_id": S, "from": "a.rs", "to": "src/b.rs"}))
                .unwrap(),
            json!({"session_id": S, "from": "a.rs", "to": "src/b.rs"})
        );
        for bad in [
            json!({"session_id": S, "from": "a.rs"}),
            json!({"session_id": S, "to": "a.rs"}),
            json!({"session_id": S, "from": "a", "to": "  "}),
            json!({"session_id": S, "from": "a", "to": "b", "force": true}),
            json!({"session_id": S, "path": "a", "to": "b"}),
            json!({"session_id": S, "from": "a\u{0}", "to": "b"}),
        ] {
            assert!(transfer_entry_params(&bad).is_err(), "{bad}");
        }
        assert_eq!(
            trash_entry_params(&json!({"project_id": P, "path": "old.txt"})).unwrap(),
            json!({"project_id": P_LOWER, "path": "old.txt"})
        );
        assert!(trash_entry_params(&json!({"project_id": P, "path": ""})).is_err());
        assert_eq!(
            undo_params(&json!({"session_id": S})).unwrap(),
            json!({"session_id": S})
        );
        assert!(undo_params(&json!({"session_id": S, "path": "a"})).is_err());
        assert!(undo_params(&json!({})).is_err());
    }

    #[test]
    fn write_scope_checks_stay_with_rsid() {
        // `..` and absolute paths go through unchanged; rsid refuses them.
        let p = trash_entry_params(&json!({"project_id": P, "path": "../x"})).unwrap();
        assert_eq!(p["path"], "../x");
        let p =
            transfer_entry_params(&json!({"project_id": P, "from": "a", "to": "/etc/x"})).unwrap();
        assert_eq!(p["to"], "/etc/x");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn each_write_command_sends_its_method_and_params_to_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, seen) = fake_daemon(
            dir.path(),
            vec![
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"path\":\"a\",\"size\":2,\"version\":\"v1:2:1:ab\",\"forced\":false,\"created\":false}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"op\":\"created\",\"path\":\"n\",\"created_top\":\"n\",\"undo_depth\":1}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"op\":\"moved\",\"path\":\"a\",\"to\":\"b\",\"undo_depth\":2}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"op\":\"copied\",\"path\":\"a\",\"to\":\"c\",\"undo_depth\":3}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"op\":\"trashed\",\"path\":\"c\",\"undo_depth\":4}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"root\":\"/r\",\"undone\":\"trashed\",\"effect\":\"restored\",\"path\":\"c\",\"undo_depth\":3}}\n",
            ],
        );
        let client = DaemonClient::new(&sock);
        let calls = [
            (
                METHOD_WRITE_FILE,
                write_file_params(&json!({"project_id": P, "path": "a", "content": "hi", "expected_version": "v1:1:1:aa"})).unwrap(),
            ),
            (
                METHOD_CREATE_ENTRY,
                create_entry_params(&json!({"project_id": P, "path": "n", "kind": "file"})).unwrap(),
            ),
            (
                METHOD_MOVE_ENTRY,
                transfer_entry_params(&json!({"project_id": P, "from": "a", "to": "b"})).unwrap(),
            ),
            (
                METHOD_COPY_ENTRY,
                transfer_entry_params(&json!({"project_id": P, "from": "a", "to": "c"})).unwrap(),
            ),
            (
                METHOD_TRASH_ENTRY,
                trash_entry_params(&json!({"project_id": P, "path": "c"})).unwrap(),
            ),
            (METHOD_UNDO, undo_params(&json!({"project_id": P})).unwrap()),
        ];
        let mut results = Vec::new();
        for (method, params) in &calls {
            results.push(client.call(method, params.clone()).await.unwrap());
        }
        assert_eq!(results[0]["version"], "v1:2:1:ab");
        assert_eq!(results[2]["to"], "b");
        assert_eq!(results[5]["effect"], "restored");
        let sent: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(sent.len(), 6);
        let names = [
            "WriteProjectFile",
            "CreateProjectEntry",
            "MoveProjectEntry",
            "CopyProjectEntry",
            "TrashProjectEntry",
            "UndoProjectFileOperation",
        ];
        for (i, (method, params)) in calls.iter().enumerate() {
            assert_eq!(sent[i]["method"], names[i]);
            assert_eq!(sent[i]["method"], *method);
            assert_eq!(sent[i]["params"], *params);
        }
        assert_eq!(
            sent[0]["params"],
            json!({"project_id": P_LOWER, "path": "a", "content": "hi", "expected_version": "v1:1:1:aa"})
        );
        assert_eq!(sent[5]["params"], json!({"project_id": P_LOWER}));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_conflict_and_escape_refusals_reach_the_webview_with_their_codes() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, _) = fake_daemon(
            dir.path(),
            vec![
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"Invalid params: project_file_stale: a.rs changed on disk\"}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"Invalid params: project_file_conflict: a.rs changed while it was moved\"}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"Invalid params: project_file_outside_root: '../x' has a '..' component\"}}\n",
            ],
        );
        let client = DaemonClient::new(&sock);
        let stale = client
            .call(METHOD_WRITE_FILE, write_file_params(&json!({"project_id": P, "path": "a.rs", "content": "x", "expected_version": "v1:1:1:aa"})).unwrap())
            .await
            .unwrap_err()
            .to_string();
        assert!(stale.contains("project_file_stale"), "{stale}");
        let conflict = client
            .call(
                METHOD_MOVE_ENTRY,
                transfer_entry_params(&json!({"project_id": P, "from": "a.rs", "to": "b.rs"}))
                    .unwrap(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(conflict.contains("project_file_conflict"), "{conflict}");
        let escape = client
            .call(
                METHOD_TRASH_ENTRY,
                trash_entry_params(&json!({"project_id": P, "path": "../x"})).unwrap(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(escape.contains("project_file_outside_root"), "{escape}");
    }
}
