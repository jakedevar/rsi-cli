//! Operator-local Remote read RPC dispatch. The public gateway remains closed;
//! it must establish its own configured project scope before forwarding reads.
use super::decision_targets::{parse_request, spawn_decision_targets_read};
use super::{
    ReadError, RemoteCursorSigner, RemoteReadCompleted, RemoteReadLimiter, spawn_decisions_read,
    spawn_history_read, spawn_info_read, spawn_projects_read, spawn_selected_session_read,
    spawn_sessions_read,
};
use crate::session::SessionManager;
use rsi_common::remote_read::{ReadRequestV1, WireDocumentV1};
use rsi_common::rpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError, RpcRequest, RpcResponse};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// The limiter every operator Remote read shares. Production uses the one
/// daemon-wide limiter: 250 ms from admission and a 50 ms Store transaction,
/// both of which a loaded host can exceed, so a read then answers `busy`
/// (#1168). A test that asserts the content of a dispatched read gets its own
/// limiter with generous deadlines, which also keeps it clear of the permits
/// other tests in the process hold on the global one.
#[cfg(not(test))]
fn operator_limiter() -> &'static RemoteReadLimiter {
    RemoteReadLimiter::global()
}

#[cfg(test)]
fn operator_limiter() -> &'static RemoteReadLimiter {
    static LIMITER: std::sync::OnceLock<RemoteReadLimiter> = std::sync::OnceLock::new();
    LIMITER.get_or_init(|| {
        let generous = std::time::Duration::from_secs(30);
        RemoteReadLimiter::with_deadline(generous, generous)
    })
}

pub fn is_operator_read_method(method: &str) -> bool {
    matches!(
        method,
        "RemoteGetInfoV1"
            | "RemoteListProjectsV1"
            | "RemoteListSessionsV1"
            | "RemoteGetSessionV1"
            | "RemoteGetHistoryPageV1"
            | "RemoteGetDecisionsV1"
            | "RemoteGetDecisionTargetsV1"
    )
}

/// This caller is the tokenless, operator-local Unix RPC boundary. The
/// configured project list and selected project are trusted only here; a
/// future gateway must authorize them before invoking these methods.
///
/// # Errors
///
/// Returns a socket error if the bounded response cannot be written or flushed.
#[allow(clippy::too_many_lines)] // Six fixed methods share one operator socket boundary.
pub async fn send_operator_read<W: AsyncWrite + Unpin>(
    manager: &Arc<SessionManager>,
    signer: &Arc<RemoteCursorSigner>,
    daemon_epoch: Uuid,
    rpc: &RpcRequest,
    writer: &mut W,
) -> io::Result<()> {
    let id = rpc.id.clone();
    if rpc.session_token.is_some() || !is_operator_read_method(&rpc.method) {
        return send_error(writer, id, ReadError::InvalidSource).await;
    }
    let limiter = operator_limiter();
    // This operator read is deliberately outside the six V1 observation
    // methods, so it is admitted before the closed `ReadRequestV1` parse.
    if rpc.method == "RemoteGetDecisionTargetsV1" {
        return match parse_request(rpc) {
            Ok(request) => {
                settle(
                    spawn_decision_targets_read(limiter, Arc::clone(manager.store()), request),
                    id,
                    writer,
                )
                .await
            }
            Err(error) => send_error(writer, id, error).await,
        };
    }
    let request = serde_json::from_value::<ReadRequestV1>(serde_json::json!({
        "method": rpc.method,
        "params": rpc.params,
    }))
    .map_err(|_| ReadError::InvalidSource)
    .and_then(|request| {
        rsi_common::remote_read::encode(&WireDocumentV1::Request(request.clone()))
            .map_err(|_| ReadError::InvalidSource)?;
        Ok(request)
    });
    let request = match request {
        Ok(request) if rpc.jsonrpc == "2.0" => request,
        _ => return send_error(writer, id, ReadError::InvalidSource).await,
    };
    match &request {
        ReadRequestV1::RemoteGetInfoV1(_) => {
            settle(spawn_info_read(limiter, &request, daemon_epoch), id, writer).await
        }
        ReadRequestV1::RemoteListProjectsV1(params) => {
            let configured = params
                .project_ids
                .iter()
                .map(|value| Uuid::parse_str(value.as_str()).map_err(|_| ReadError::InvalidSource))
                .collect::<super::Result<Vec<_>>>();
            let configured = match configured {
                Ok(configured) => configured,
                Err(error) => return send_error(writer, id, error).await,
            };
            let scope = policy_scope(&configured);
            settle(
                spawn_projects_read(
                    limiter,
                    Arc::clone(manager.store()),
                    Arc::clone(signer),
                    request.clone(),
                    configured,
                    scope,
                    daemon_epoch,
                ),
                id,
                writer,
            )
            .await
        }
        ReadRequestV1::RemoteListSessionsV1(params) => {
            let Ok(project) = Uuid::parse_str(params.project_id.as_str()) else {
                return send_error(writer, id, ReadError::InvalidSource).await;
            };
            settle(
                spawn_sessions_read(
                    limiter,
                    Arc::clone(manager.store()),
                    Arc::clone(manager),
                    Arc::clone(signer),
                    request.clone(),
                    project,
                    policy_scope(&[project]),
                    daemon_epoch,
                ),
                id,
                writer,
            )
            .await
        }
        ReadRequestV1::RemoteGetSessionV1(params) => {
            let Ok(project) = Uuid::parse_str(params.project_id.as_str()) else {
                return send_error(writer, id, ReadError::InvalidSource).await;
            };
            settle(
                spawn_selected_session_read(
                    limiter,
                    Arc::clone(manager.store()),
                    Arc::clone(manager),
                    &request,
                    project,
                    daemon_epoch,
                ),
                id,
                writer,
            )
            .await
        }
        ReadRequestV1::RemoteGetHistoryPageV1(params) => {
            let Ok(project) = Uuid::parse_str(params.project_id.as_str()) else {
                return send_error(writer, id, ReadError::InvalidSource).await;
            };
            settle(
                spawn_history_read(
                    limiter,
                    Arc::clone(manager.store()),
                    signer,
                    &request,
                    project,
                    policy_scope(&[project]),
                    daemon_epoch,
                ),
                id,
                writer,
            )
            .await
        }
        ReadRequestV1::RemoteGetDecisionsV1(params) => {
            let Ok(project) = Uuid::parse_str(params.project_id.as_str()) else {
                return send_error(writer, id, ReadError::InvalidSource).await;
            };
            settle(
                spawn_decisions_read(
                    limiter,
                    Arc::clone(manager.store()),
                    Arc::clone(manager),
                    Arc::clone(signer),
                    request.clone(),
                    project,
                    policy_scope(&[project]),
                    daemon_epoch,
                ),
                id,
                writer,
            )
            .await
        }
    }
}

fn policy_scope(projects: &[Uuid]) -> [u8; 32] {
    let mut projects = projects.to_vec();
    projects.sort_unstable();
    let mut digest = Sha256::new();
    digest.update(b"rsi-remote-operator-local-scope-v1\0");
    for project in projects {
        digest.update(project.as_bytes());
    }
    digest.finalize().into()
}

async fn settle<T: Serialize, W: AsyncWrite + Unpin>(
    work: super::Result<JoinHandle<RemoteReadCompleted<T>>>,
    id: Option<serde_json::Value>,
    writer: &mut W,
) -> io::Result<()> {
    match work {
        Ok(handle) => match handle.await {
            Ok(completed) => {
                completed
                    .send_json_line(writer, |result| match result {
                        Ok(value) => match serde_json::to_value(value) {
                            Ok(value) => RpcResponse::success(id, value),
                            Err(_) => remote_error(id, &ReadError::SourceUnavailable),
                        },
                        Err(error) => remote_error(id, &error),
                    })
                    .await
            }
            Err(_) => send_error(writer, id, ReadError::SourceUnavailable).await,
        },
        Err(error) => send_error(writer, id, error).await,
    }
}

fn remote_error(id: Option<serde_json::Value>, error: &ReadError) -> RpcResponse {
    let (rpc_code, code) = match error {
        ReadError::InvalidSource => (INVALID_PARAMS, "invalid_request"),
        ReadError::NotFound => (INVALID_PARAMS, "not_found"),
        ReadError::StaleCursor => (INVALID_PARAMS, "stale_cursor"),
        ReadError::Admission => (INTERNAL_ERROR, "admission"),
        ReadError::Busy => (INTERNAL_ERROR, "busy"),
        ReadError::ResourceLimit => (INVALID_PARAMS, "resource_limit"),
        ReadError::SourceUnavailable | ReadError::Sql(_) | ReadError::Io(_) => {
            (INTERNAL_ERROR, "source_unavailable")
        }
    };
    RpcResponse::error(
        id,
        RpcError {
            code: rpc_code,
            message: code.into(),
            data: Some(serde_json::json!({"remote_code":code})),
        },
    )
}

async fn send_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: Option<serde_json::Value>,
    error: ReadError,
) -> io::Result<()> {
    let response = serde_json::to_vec(&remote_error(id, &error)).map_err(io::Error::other)?;
    writer.write_all(&response).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}
