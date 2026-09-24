//! Token-bound Unix JSON-RPC transport shared by agent control clients.
//!
//! The session token is read only from the inherited environment.  It is not
//! part of a tool schema or caller-provided request payload.

use crate::identity;
use crate::rpc::{RpcRequest, RpcResponse};
use serde_json::Value;
use std::io::{BufRead as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn resolve_socket_path() -> PathBuf {
    std::env::var("RSI_DAEMON_SOCKET_PATH")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(identity::default_socket_path)
}

pub fn dispatch_from_env(method: &str, params: Value) -> std::io::Result<RpcResponse> {
    dispatch(&resolve_socket_path(), method, params)
}

pub fn dispatch(socket: &Path, method: &str, params: Value) -> std::io::Result<RpcResponse> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let request = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(Value::Number(1.into())),
        method: method.to_string(),
        params,
        session_token: std::env::var(identity::ENV_SESSION_TOKEN)
            .ok()
            .filter(|value| !value.is_empty()),
    };
    let request_json = serde_json::to_string(&request)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    writeln!(stream, "{request_json}")?;
    let mut response = String::new();
    let mut reader = std::io::BufReader::new(stream);
    if reader.read_line(&mut response)? == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "daemon closed connection without a response",
        ));
    }
    serde_json::from_str(response.trim())
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}
