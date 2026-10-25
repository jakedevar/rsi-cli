//! The per-run adapter socket (#3178 item 2).
//!
//! A daemon-owned unix socket `~/.rsi/bench/<run>/adapter.sock` (directory
//! 0700, socket 0600), bound only while the run is active and bound again after
//! a daemon restart. It speaks the daemon's own framing (one JSON-RPC 2.0
//! request per line, one response per line) but serves only the five Bench
//! methods, scoped to the run the socket belongs to: the run comes from the
//! socket, never from the request, and every parameter type refuses unknown
//! fields, so a request cannot name another run. Any other method, including
//! every daemon method, is refused `bench_method_not_allowed`.

use crate::bench::{BenchError, BenchService};
use rsi_common::bench::{
    BENCH_METHOD_NOT_ALLOWED, BENCH_METHODS, BenchTaskStartParams, BenchTraceReadParams,
    BenchTrialParams, METHOD_TASK_CANCEL, METHOD_TASK_FINISH, METHOD_TASK_START,
    METHOD_TASK_STATUS, METHOD_TRACE_READ,
};
use rsi_common::rpc::{
    INVALID_PARAMS, METHOD_NOT_FOUND, RpcError, RpcErrorData, RpcRequest, RpcResponse,
};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The longest request line accepted (an instruction is capped at 512 KiB; JSON
/// escaping can inflate it).
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Connections served at once per run.
pub const MAX_CONNECTIONS: usize = 16;
/// A connection idle this long is closed.
pub const IDLE_SECS: u64 = 600;

/// A bound listener: closing it stops accepting and removes the socket file.
pub struct ListenerHandle {
    cancel: CancellationToken,
    #[cfg_attr(not(unix), allow(dead_code))]
    path: std::path::PathBuf,
}

impl ListenerHandle {
    /// Stop accepting and remove the socket file.
    pub fn close(&self) {
        self.cancel.cancel();
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn error_response(id: Option<serde_json::Value>, code: i32, error: &BenchError) -> RpcResponse {
    RpcResponse::error(
        id,
        RpcError {
            code,
            message: error.message(),
            data: serde_json::to_value(RpcErrorData {
                code: error.code.clone(),
                resource_type: None,
                resource_id: None,
                retryable: error.retryable,
                details: None,
            })
            .ok(),
        },
    )
}

fn params<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T, BenchError> {
    serde_json::from_value(request.params.clone()).map_err(|e| {
        BenchError::new(
            rsi_common::bench::BENCH_INVALID_REQUEST,
            format!("{}:params: {e}", rsi_common::bench::BENCH_INVALID_REQUEST),
        )
    })
}

fn result<T: serde::Serialize>(
    id: Option<serde_json::Value>,
    outcome: Result<T, BenchError>,
) -> RpcResponse {
    match outcome {
        Ok(value) => match serde_json::to_value(value) {
            Ok(value) => RpcResponse::success(id, value),
            Err(e) => error_response(
                id,
                rsi_common::rpc::INTERNAL_ERROR,
                &BenchError::new("bench_internal_error", e.to_string()),
            ),
        },
        Err(error) => error_response(id, INVALID_PARAMS, &error),
    }
}

/// Answer one request on `run_id`'s socket.
pub async fn dispatch(
    service: &Arc<BenchService>,
    run_id: Uuid,
    request: RpcRequest,
) -> RpcResponse {
    let id = request.id.clone();
    if !BENCH_METHODS.contains(&request.method.as_str()) {
        return error_response(
            id,
            METHOD_NOT_FOUND,
            &BenchError::new(
                BENCH_METHOD_NOT_ALLOWED,
                format!(
                    "{BENCH_METHOD_NOT_ALLOWED}: {} is not served on a bench adapter socket",
                    request.method
                ),
            ),
        );
    }
    match request.method.as_str() {
        METHOD_TASK_START => match params::<BenchTaskStartParams>(&request) {
            Ok(p) => result(id, service.start_task(run_id, p).await),
            Err(e) => result::<()>(id, Err(e)),
        },
        METHOD_TASK_STATUS => match params::<BenchTrialParams>(&request) {
            Ok(p) => result(id, service.task_status(run_id, &p.trial).await),
            Err(e) => result::<()>(id, Err(e)),
        },
        METHOD_TASK_CANCEL => match params::<BenchTrialParams>(&request) {
            Ok(p) => result(id, service.cancel_task(run_id, &p.trial).await),
            Err(e) => result::<()>(id, Err(e)),
        },
        METHOD_TASK_FINISH => match params::<BenchTrialParams>(&request) {
            Ok(p) => result(id, service.finish_task(run_id, &p.trial).await),
            Err(e) => result::<()>(id, Err(e)),
        },
        METHOD_TRACE_READ => match params::<BenchTraceReadParams>(&request) {
            Ok(p) => result(id, service.read_trace(run_id, p).await),
            Err(e) => result::<()>(id, Err(e)),
        },
        _ => result::<()>(
            id,
            Err(BenchError::new(BENCH_METHOD_NOT_ALLOWED, request.method)),
        ),
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    /// Read one line of at most [`MAX_LINE_BYTES`]. `None` at end of input.
    async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
        reader: &mut R,
        line: &mut Vec<u8>,
    ) -> std::io::Result<Option<bool>> {
        line.clear();
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(if line.is_empty() { None } else { Some(true) });
            }
            let (take, done) = match available.iter().position(|b| *b == b'\n') {
                Some(at) => (at + 1, true),
                None => (available.len(), false),
            };
            if line.len() + take > MAX_LINE_BYTES {
                reader.consume(take);
                return Ok(Some(false));
            }
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            if done {
                return Ok(Some(true));
            }
        }
    }

    async fn serve(service: Arc<BenchService>, run_id: Uuid, stream: UnixStream) {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(IDLE_SECS),
                read_line(&mut reader, &mut line),
            )
            .await;
            let response = match read {
                Ok(Ok(Some(true))) => {
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    match serde_json::from_slice::<RpcRequest>(&line) {
                        Ok(request) => dispatch(&service, run_id, request).await,
                        Err(e) => error_response(
                            None,
                            INVALID_PARAMS,
                            &BenchError::new(
                                rsi_common::bench::BENCH_INVALID_REQUEST,
                                format!(
                                    "{}:request: {e}",
                                    rsi_common::bench::BENCH_INVALID_REQUEST
                                ),
                            ),
                        ),
                    }
                }
                Ok(Ok(Some(false))) => error_response(
                    None,
                    INVALID_PARAMS,
                    &BenchError::invalid("request_too_large"),
                ),
                Ok(Ok(None) | Err(_)) | Err(_) => return,
            };
            let Ok(mut text) = serde_json::to_string(&response) else {
                return;
            };
            text.push('\n');
            if writer.write_all(text.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                return;
            }
        }
    }

    pub(super) fn bind(
        service: Arc<BenchService>,
        run_id: Uuid,
        path: &Path,
    ) -> std::io::Result<ListenerHandle> {
        use std::os::unix::fs::PermissionsExt;
        let dir = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path has no parent",
            )
        })?;
        if let Some(root) = dir.parent() {
            crate::bench::private_dir(root)?;
        }
        crate::bench::private_dir(dir)?;
        // A stale socket file from a previous daemon: this path is
        // daemon-owned, so it is removed and bound afresh.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let cancel = CancellationToken::new();
        let accept_cancel = cancel.clone();
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = accept_cancel.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else {
                    continue;
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    // Over the connection cap: drop the connection.
                    continue;
                };
                let service = Arc::clone(&service);
                let conn_cancel = accept_cancel.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    tokio::select! {
                        () = conn_cancel.cancelled() => {}
                        () = serve(service, run_id, stream) => {}
                    }
                });
            }
        });
        Ok(ListenerHandle {
            cancel,
            path: path.to_path_buf(),
        })
    }
}

/// Bind `run_id`'s adapter socket at `path`.
///
/// # Errors
/// The directory or socket cannot be created, or the platform has no unix sockets.
pub fn bind(
    service: Arc<BenchService>,
    run_id: Uuid,
    path: &Path,
) -> std::io::Result<ListenerHandle> {
    #[cfg(unix)]
    {
        imp::bind(service, run_id, path)
    }
    #[cfg(not(unix))]
    {
        let _ = (service, run_id, path);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the bench adapter socket needs unix sockets",
        ))
    }
}
