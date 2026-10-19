//! The Issues workspace's Dispatched and Sync tabs (hub #3000 gap sweep G2):
//! the issue tracker's dispatch records, its polling status and the manual
//! poll (`P`). Each command forwards one daemon method with the params the
//! TUI client sends (`crates/rsi/src/client.rs` list_dispatched_issues,
//! get_issue_tracker_status, trigger_issue_tracker_poll): none.

use serde_json::Value;
use tauri::State;

use crate::{AppState, CmdResult, forward};

pub const METHOD_DISPATCHED: &str = "ListDispatchedIssues";
pub const METHOD_STATUS: &str = "GetIssueTrackerStatus";
pub const METHOD_POLL: &str = "TriggerIssueTrackerPoll";

#[tauri::command]
pub async fn dispatched_issues(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_DISPATCHED, Value::Null).await
}

#[tauri::command]
pub async fn issue_tracker_status(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_STATUS, Value::Null).await
}

/// One manual tracker poll (`P` on the Sync tab).
#[tauri::command]
pub async fn trigger_issue_poll(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_POLL, Value::Null).await
}

// The only test drives a fake daemon on a Unix socket (rsid is Unix-only).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::daemon::DaemonClient;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn each_tracker_read_sends_its_method_with_no_params() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                BufReader::new(r).read_line(&mut line).await.unwrap();
                seen.push(serde_json::from_str::<Value>(&line).unwrap());
                w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":[]}\n")
                    .await
                    .unwrap();
            }
            seen
        });
        let client = DaemonClient::new(&sock);
        for method in [METHOD_DISPATCHED, METHOD_STATUS, METHOD_POLL] {
            client.call(method, Value::Null).await.unwrap();
        }
        let seen = server.await.unwrap();
        let methods: Vec<&str> = seen.iter().map(|r| r["method"].as_str().unwrap()).collect();
        assert_eq!(
            methods,
            [
                "ListDispatchedIssues",
                "GetIssueTrackerStatus",
                "TriggerIssueTrackerPoll"
            ]
        );
        assert!(seen.iter().all(|r| r["params"].is_null()));
    }
}
