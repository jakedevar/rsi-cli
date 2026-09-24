//! Error types for the eval driver.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("corpus error: {0}")]
    Corpus(String),

    #[error("socket guard error: {0}")]
    SocketGuard(String),

    #[error("daemon RPC error: {0}")]
    Rpc(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, EvalError>;
