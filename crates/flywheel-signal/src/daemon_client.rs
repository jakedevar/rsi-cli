//! JSON-RPC client for communicating with the RSI daemon.
//!
//! Wire-identical to `flywheel-imessage::daemon_client`; the daemon cares only
//! about the RPC method + params and is ignorant of the bridge provider.

use rsi_common::rpc::{RpcRequest, RpcResponse};
use rsi_common::types::{ConversationEvent, Session, SessionProvider};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Not connected to daemon")]
    NotConnected,

    #[error("RPC error ({code}): {message}")]
    Rpc { code: i32, message: String },

    #[error("Connection closed by daemon")]
    ConnectionClosed,
}

pub type Result<T> = std::result::Result<T, ClientError>;

/// Async client for communicating with rsid over Unix socket.
pub struct DaemonClient {
    socket_path: PathBuf,
    stream: Option<UnixStream>,
    next_id: AtomicI64,
}

impl DaemonClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            stream: None,
            next_id: AtomicI64::new(1),
        }
    }

    /// Default socket path: ~/.rsi/daemon.sock (env override: $RSI_SOCKET).
    pub fn default_socket_path() -> PathBuf {
        rsi_common::identity::env_with_legacy(
            "RSI_SOCKET",
            &["MOTHERSHIP_SOCKET", "FLYWHEEL_SOCKET"],
        )
        .map(PathBuf::from)
        .unwrap_or_else(|_| rsi_common::identity::data_path("daemon.sock", "daemon.sock"))
    }

    /// Connect to the daemon socket.
    pub async fn connect(&mut self) -> Result<()> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        self.stream = Some(stream);
        Ok(())
    }

    /// Socket path (for spawning independent connections like the push stream).
    #[allow(dead_code)]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Whether we have a live stream.
    #[allow(dead_code)]
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Drop the stream without closing it explicitly (tokio handles the fd).
    #[allow(dead_code)]
    pub fn disconnect(&mut self) {
        self.stream = None;
    }

    /// Connect with exponential backoff. Returns once connected.
    pub async fn connect_with_backoff(&mut self) {
        let mut backoff_ms: u64 = 1000;
        const MAX_BACKOFF_MS: u64 = 10_000;

        loop {
            match self.connect().await {
                Ok(()) => {
                    tracing::info!("Connected to daemon at {}", self.socket_path.display());
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to connect to daemon: {}. Retrying in {}ms...",
                        e,
                        backoff_ms
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                }
            }
        }
    }

    /// Send an RPC request and wait for the response.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let stream = self.stream.as_mut().ok_or(ClientError::NotConnected)?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::Number(id.into())),
            method: method.to_string(),
            params,
            session_token: None,
        };

        let json = serde_json::to_string(&request)?;
        stream.write_all(format!("{}\n", json).as_bytes()).await?;

        let mut buf = String::new();
        let mut reader = BufReader::new(&mut *stream);
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            self.stream = None;
            return Err(ClientError::ConnectionClosed);
        }

        let response: RpcResponse = serde_json::from_str(buf.trim())?;

        if let Some(error) = response.error {
            return Err(ClientError::Rpc {
                code: error.code,
                message: error.message,
            });
        }

        Ok(response.result.unwrap_or(Value::Null))
    }

    // --- Session operations ---

    pub async fn list_sessions(&mut self) -> Result<Vec<Session>> {
        let result = self.request("ListSessions", Value::Null).await?;
        let sessions: Vec<Session> = serde_json::from_value(result)?;
        Ok(sessions)
    }

    #[allow(dead_code)]
    pub async fn get_session(&mut self, session_id: Uuid) -> Result<Session> {
        let result = self
            .request(
                "GetSession",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        let session: Session = serde_json::from_value(result)?;
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn launch_session(
        &mut self,
        query: &str,
        working_dir: &Path,
        provider: SessionProvider,
        model: Option<&str>,
        project_id: Option<Uuid>,
    ) -> Result<Value> {
        let params = serde_json::json!({
            "query": query,
            "working_dir": working_dir,
            "provider": provider,
            "model": model,
            "project_id": project_id,
        });
        self.request("LaunchSession", params).await
    }

    pub async fn continue_session(&mut self, session_id: Uuid, query: &str) -> Result<()> {
        self.request(
            "ContinueSession",
            serde_json::json!({
                "session_id": session_id,
                "query": query,
            }),
        )
        .await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn answer_question(&mut self, session_id: Uuid, response_text: &str) -> Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "response_text": response_text,
        });
        self.request("AnswerQuestion", params).await?;
        Ok(())
    }

    pub async fn interrupt_session(&mut self, session_id: Uuid) -> Result<()> {
        self.request(
            "InterruptSession",
            serde_json::json!({ "session_id": session_id }),
        )
        .await?;
        Ok(())
    }

    pub async fn get_conversation(
        &mut self,
        session_id: Uuid,
        since_sequence: Option<i32>,
    ) -> Result<Vec<ConversationEvent>> {
        let mut params = serde_json::json!({ "session_id": session_id });
        if let Some(seq) = since_sequence {
            params["since_sequence"] = serde_json::json!(seq);
        }
        let result = self.request("GetConversation", params).await?;
        let events: Vec<ConversationEvent> = serde_json::from_value(result)?;
        Ok(events)
    }

    #[allow(dead_code)]
    pub async fn list_projects(&mut self) -> Result<Vec<rsi_common::types::Project>> {
        let result = self.request("ListProjects", Value::Null).await?;
        let projects: Vec<rsi_common::types::Project> = serde_json::from_value(result)?;
        Ok(projects)
    }

    #[allow(dead_code)]
    pub async fn get_health_status(&mut self) -> Result<rsi_common::rpc::HealthStatusResponse> {
        let result = self.request("GetHealthStatus", Value::Null).await?;
        let status: rsi_common::rpc::HealthStatusResponse = serde_json::from_value(result)?;
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_socket_path() {
        let path = DaemonClient::default_socket_path();
        assert!(path.to_string_lossy().contains("rsi"));
    }

    #[test]
    fn test_client_initially_disconnected() {
        let client = DaemonClient::new(PathBuf::from("/tmp/test.sock"));
        assert!(!client.is_connected());
    }

    #[test]
    fn test_client_disconnect() {
        let mut client = DaemonClient::new(PathBuf::from("/tmp/test.sock"));
        client.disconnect();
        assert!(!client.is_connected());
    }
}
