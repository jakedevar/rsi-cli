//! Push event subscriber for daemon notifications.
//!
//! Follows the TUI's `NotificationStream` pattern: opens a second Unix socket
//! connection, sends a `Subscribe` RPC, then reads `BusEvent` JSON lines.
//! Reconnects with exponential backoff on connection loss.

use rsi_common::rpc::{BusEvent, RpcRequest};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Handle to the background push notification task.
pub struct PushStream {
    rx: mpsc::UnboundedReceiver<BusEvent>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl PushStream {
    /// Spawn a background task that connects to the daemon and streams push events.
    pub fn spawn(socket_path: &Path) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let path = socket_path.to_path_buf();

        tokio::spawn(async move {
            run_stream(path, tx, shutdown_rx).await;
        });

        Self {
            rx,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// Receive the next push event. Returns None if the stream is closed.
    pub async fn recv(&mut self) -> Option<BusEvent> {
        self.rx.recv().await
    }
}

impl Drop for PushStream {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Background task: connect, subscribe, stream events. Reconnect on failure.
async fn run_stream(
    socket_path: PathBuf,
    tx: mpsc::UnboundedSender<BusEvent>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let mut backoff_ms: u64 = 1000;
    const MAX_BACKOFF_MS: u64 = 10_000;

    loop {
        match connect_and_stream(&socket_path, &tx, &mut shutdown_rx).await {
            StreamResult::Shutdown => return,
            StreamResult::Disconnected => {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)) => {}
                    _ = &mut shutdown_rx => return,
                }
                backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
            }
            StreamResult::Connected => {
                backoff_ms = 1000;
            }
        }
    }
}

enum StreamResult {
    Shutdown,
    Disconnected,
    Connected,
}

async fn connect_and_stream(
    socket_path: &Path,
    tx: &mpsc::UnboundedSender<BusEvent>,
    shutdown_rx: &mut tokio::sync::oneshot::Receiver<()>,
) -> StreamResult {
    let stream = match UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(_) => return StreamResult::Disconnected,
    };

    let (reader, mut writer) = stream.into_split();

    // Subscribe to relevant events only
    let request = RpcRequest::new(
        "Subscribe",
        serde_json::json!({
            "event_types": ["conversation_event", "session_status_changed"]
        }),
    );
    let json = match serde_json::to_string(&request) {
        Ok(j) => j,
        Err(_) => return StreamResult::Disconnected,
    };
    if writer
        .write_all(format!("{}\n", json).as_bytes())
        .await
        .is_err()
    {
        return StreamResult::Disconnected;
    }

    let mut lines = BufReader::new(reader).lines();

    // Read ack response
    let ack_line = tokio::select! {
        result = lines.next_line() => result,
        _ = &mut *shutdown_rx => return StreamResult::Shutdown,
    };
    match ack_line {
        Ok(Some(_)) => {} // ack received
        _ => return StreamResult::Disconnected,
    }

    tracing::info!("Push stream connected and subscribed");

    // Stream events
    loop {
        let line_result = tokio::select! {
            result = lines.next_line() => result,
            _ = &mut *shutdown_rx => return StreamResult::Shutdown,
        };

        match line_result {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(event) = serde_json::from_str::<BusEvent>(&line)
                    && tx.send(event).is_err()
                {
                    return StreamResult::Shutdown;
                }
            }
            Ok(None) => return StreamResult::Connected,
            Err(_) => return StreamResult::Connected,
        }
    }
}
