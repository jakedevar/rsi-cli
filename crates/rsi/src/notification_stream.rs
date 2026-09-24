//! Background push notification stream from daemon.
//!
//! Opens a second Unix socket connection, sends a `Subscribe` RPC,
//! then reads `BusEvent` JSON lines and forwards them via an mpsc channel.
//! Any loss is reported once to the main event loop; the shared bootstrap
//! coordinator is the sole reconnect owner.

use rsi_common::rpc::{BusEvent, RpcRequest};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Handle to the background push notification task.
pub struct NotificationStream {
    rx: mpsc::UnboundedReceiver<NotificationStreamEvent>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Debug)]
pub(crate) enum NotificationStreamEvent {
    Bus(BusEvent),
    Lost(NotificationStreamLoss),
}

#[derive(Debug)]
pub(crate) enum NotificationStreamLoss {
    Connect(String),
    Subscribe(String),
    EstablishedEof,
    EstablishedRead(String),
}

impl NotificationStreamLoss {
    pub(crate) fn message(self) -> String {
        match self {
            Self::Connect(error) => format!("daemon notification stream connect failed: {error}"),
            Self::Subscribe(error) => {
                format!("daemon notification stream subscribe failed: {error}")
            }
            Self::EstablishedEof => "daemon notification stream closed".to_string(),
            Self::EstablishedRead(error) => {
                format!("daemon notification stream read failed: {error}")
            }
        }
    }
}

impl NotificationStream {
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
    pub(crate) async fn recv(&mut self) -> Option<NotificationStreamEvent> {
        self.rx.recv().await
    }
}

impl Drop for NotificationStream {
    fn drop(&mut self) {
        // Signal background task to shut down
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Background task: connect once, subscribe, and stream events. Transport loss
/// is handed to the main task exactly once so bootstrap owns reconnection.
async fn run_stream(
    socket_path: PathBuf,
    tx: mpsc::UnboundedSender<NotificationStreamEvent>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    if let StreamResult::Lost(loss) = connect_and_stream(&socket_path, &tx, &mut shutdown_rx).await
    {
        let _ = tx.send(NotificationStreamEvent::Lost(loss));
    }
}

enum StreamResult {
    /// Clean shutdown requested
    Shutdown,
    /// Connection or established stream lost — report to bootstrap once.
    Lost(NotificationStreamLoss),
}

async fn connect_and_stream(
    socket_path: &Path,
    tx: &mpsc::UnboundedSender<NotificationStreamEvent>,
    shutdown_rx: &mut tokio::sync::oneshot::Receiver<()>,
) -> StreamResult {
    // Connect
    let stream = match UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(error) => {
            return StreamResult::Lost(NotificationStreamLoss::Connect(error.to_string()));
        }
    };

    let (reader, mut writer) = stream.into_split();

    // Send Subscribe RPC
    let request = RpcRequest::new("Subscribe", serde_json::json!({}));
    let json = match serde_json::to_string(&request) {
        Ok(j) => j,
        Err(error) => {
            return StreamResult::Lost(NotificationStreamLoss::Subscribe(error.to_string()));
        }
    };
    if let Err(error) = writer.write_all(format!("{}\n", json).as_bytes()).await {
        return StreamResult::Lost(NotificationStreamLoss::Subscribe(error.to_string()));
    }

    let mut lines = BufReader::new(reader).lines();

    // Read ack response
    let ack_line = tokio::select! {
        result = lines.next_line() => result,
        _ = &mut *shutdown_rx => return StreamResult::Shutdown,
    };
    match ack_line {
        Ok(Some(_)) => {} // ack received, ignore content
        Ok(None) => {
            return StreamResult::Lost(NotificationStreamLoss::Subscribe(
                "connection closed before acknowledgement".to_string(),
            ));
        }
        Err(error) => {
            return StreamResult::Lost(NotificationStreamLoss::Subscribe(error.to_string()));
        }
    }

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
                if let Ok(event) = serde_json::from_str::<BusEvent>(&line) {
                    if tx.send(NotificationStreamEvent::Bus(event)).is_err() {
                        return StreamResult::Shutdown;
                    }
                }
            }
            Ok(None) => return StreamResult::Lost(NotificationStreamLoss::EstablishedEof),
            Err(error) => {
                return StreamResult::Lost(NotificationStreamLoss::EstablishedRead(
                    error.to_string(),
                ));
            }
        }
    }
}
