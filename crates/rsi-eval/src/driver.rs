//! Polling driver for the eval/replay harness.
//!
//! For each corpus ticket: launch a session via JSON-RPC `LaunchSession`,
//! poll `GetSession` every 2s until the session reaches a terminal status
//! (or the per-replay timeout fires), then capture the resulting `Session`
//! row. Phase 4 ships the launch + poll loop; Phase 5 layers on metrics
//! aggregation and baseline comparison.

use crate::corpus::CorpusTicket;
use crate::errors::{EvalError, Result};
use rsi_common::rpc::{LaunchSessionParams, RpcRequest, RpcResponse};
use rsi_common::types::{Session, SessionStatus};
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

/// Default poll interval. Plan locks 2s.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Default per-replay timeout (5 minutes). Configurable via the CLI.
pub const DEFAULT_REPLAY_TIMEOUT: Duration = Duration::from_secs(300);

/// Default total wall-time budget (30 minutes). The plan's success criterion.
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(1800);

/// Per-ticket replay outcome.
#[derive(Debug, Clone)]
pub struct ReplayOutcome {
    pub ticket_id: String,
    /// Final session row. `None` when the launch RPC itself failed.
    pub session: Option<Session>,
    /// Whether the per-replay timeout fired.
    pub timed_out: bool,
}

/// Drive one ticket end-to-end: launch, poll, return final `Session`.
pub async fn replay_ticket(
    socket: &Path,
    ticket: &CorpusTicket,
    working_dir: PathBuf,
    replay_timeout: Duration,
) -> Result<ReplayOutcome> {
    let session_id = launch(socket, ticket, working_dir.clone()).await?;

    let deadline = tokio::time::Instant::now() + replay_timeout;

    loop {
        let session = get_session(socket, session_id).await?;
        let last_status = session.status;
        if session.status.is_terminal() {
            return Ok(ReplayOutcome {
                ticket_id: ticket.id.clone(),
                session: Some(session),
                timed_out: false,
            });
        }

        if tokio::time::Instant::now() >= deadline {
            // Best-effort interrupt; even if it fails, we still report timeout.
            let _ = interrupt(socket, session_id).await;
            // Capture final state once after the interrupt.
            let session = get_session(socket, session_id).await.ok();
            return Ok(ReplayOutcome {
                ticket_id: ticket.id.clone(),
                session,
                timed_out: true,
            });
        }

        tokio::time::sleep(POLL_INTERVAL).await;

        // Guard against pathological "Starting forever" — log every 30s of
        // sustained non-terminal status so the operator can see progress.
        if matches!(last_status, SessionStatus::Starting) {
            tracing::debug!(session_id = %session_id, "still in Starting after poll");
        }
    }
}

async fn launch(socket: &Path, ticket: &CorpusTicket, working_dir: PathBuf) -> Result<Uuid> {
    let params = LaunchSessionParams {
        query: ticket.prompt.clone(),
        title: None,
        working_dir: Some(working_dir),
        provider: None,
        model: None,
        configured_context_window: None,
        system_prompt: ticket.system_prompt.clone(),
        session_kind: Some(ticket.kind),
        project_id: None,
        continued_from: None,
        parent_id: None,
        openai_base_url: None,
        openai_api_key: None,
        workflow_id: None,
        max_retries: None,
        group_id: None,
        effort: None,
        sandbox: None,
        is_eval: Some(true),
        skip_context_pipeline: Some(true),
        tags: vec!["eval".to_string()],
        workflow_id_override: None,
    };
    let request = RpcRequest::new(
        "LaunchSession",
        serde_json::to_value(&params).map_err(EvalError::from)?,
    );
    let response = send(socket, &request).await?;
    let result = response_result(response)?;
    let session_id = result
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| EvalError::Rpc("LaunchSession response missing session_id".to_string()))?;
    Uuid::parse_str(session_id).map_err(|e| EvalError::Rpc(format!("invalid session_id: {e}")))
}

async fn get_session(socket: &Path, session_id: Uuid) -> Result<Session> {
    let request = RpcRequest::new(
        "GetSession",
        serde_json::json!({ "session_id": session_id }),
    );
    let response = send(socket, &request).await?;
    let result = response_result(response)?;
    serde_json::from_value(result).map_err(EvalError::from)
}

async fn interrupt(socket: &Path, session_id: Uuid) -> Result<()> {
    let request = RpcRequest::new(
        "InterruptSession",
        serde_json::json!({ "session_id": session_id }),
    );
    let _ = send(socket, &request).await?;
    Ok(())
}

/// Send a single JSON-RPC request over a fresh Unix-domain stream and
/// receive one newline-framed response. Mirrors the framing protocol of
/// `crates/rsi-common/src/bin/rsi-rpc.rs` so the eval driver speaks the
/// daemon's documented wire format.
async fn send(socket: &Path, request: &RpcRequest) -> Result<RpcResponse> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| EvalError::Rpc(format!("connect {} failed: {}", socket.display(), e)))?;
    let mut payload = serde_json::to_vec(request).map_err(EvalError::from)?;
    payload.push(b'\n');
    stream.write_all(&payload).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(EvalError::Rpc("daemon closed connection".to_string()));
    }
    serde_json::from_str(&line).map_err(EvalError::from)
}

fn response_result(response: RpcResponse) -> Result<Value> {
    if let Some(err) = response.error {
        return Err(EvalError::Rpc(format!(
            "daemon error {}: {}",
            err.code, err.message
        )));
    }
    response
        .result
        .ok_or_else(|| EvalError::Rpc("missing result in JSON-RPC response".to_string()))
}
