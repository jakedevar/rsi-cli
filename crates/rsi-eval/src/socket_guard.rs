//! Default-socket guard.
//!
//! `rsi-eval` writes synthetic eval-replay rows to whatever daemon it talks
//! to. Hitting the user's `~/.rsi/daemon.sock` would pollute the user's
//! production analytics with eval rows. This guard fails closed: if the
//! resolved socket equals the user-default and `--allow-default-socket` was
//! not passed, refuse to run. Mirrors the V1.1 verifier-daemon pattern at
//! `thoughts/shared/projects/verification-pipeline/INDEX.md:30-39`.

use crate::errors::{EvalError, Result};
use std::path::{Path, PathBuf};

/// Resolve the socket path the eval driver would use, honoring the
/// `RSI_DAEMON_SOCKET_PATH` override.
pub fn resolved_socket() -> PathBuf {
    rsi_common::identity::resolve_socket_path()
}

/// The user-default socket location (`~/.rsi/daemon.sock` or `/tmp` fallback).
pub fn user_default_socket() -> PathBuf {
    rsi_common::identity::data_path("daemon.sock", "daemon.sock")
}

/// Refuse to run if the resolved socket equals the user-default and
/// `allow_default` is false. Returns `Ok(socket_path)` when allowed,
/// `Err(EvalError::SocketGuard)` otherwise.
pub fn check(allow_default: bool) -> Result<PathBuf> {
    let socket = resolved_socket();
    let user_default = user_default_socket();
    if !allow_default && paths_equal(&socket, &user_default) {
        return Err(EvalError::SocketGuard(format!(
            "rsi-eval refuses to run against the user default socket {}. \
             Set RSI_DAEMON_SOCKET_PATH to an isolated daemon, or pass \
             --allow-default-socket to override.",
            socket.display()
        )));
    }
    Ok(socket)
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    // Canonicalize when both exist; otherwise fall back to lexical equality.
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All env-var tests in this module use `temp_env::with_var*` which
    /// serializes env-var mutations across the whole test binary, making
    /// these safe to run alongside other parallel tests in the workspace.
    #[test]
    fn refuses_user_default() {
        temp_env::with_var_unset("RSI_DAEMON_SOCKET_PATH", || {
            let result = check(false);
            assert!(result.is_err(), "default socket must be refused");
            if let Err(EvalError::SocketGuard(msg)) = result {
                assert!(msg.contains("refuses to run"));
            } else {
                panic!("expected SocketGuard error");
            }
        });
    }

    #[test]
    fn allows_isolated_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("d.sock");
        temp_env::with_var(
            "RSI_DAEMON_SOCKET_PATH",
            Some(socket_path.to_str().unwrap()),
            || {
                let result = check(false);
                assert!(
                    result.is_ok(),
                    "isolated socket must be allowed: {result:?}"
                );
            },
        );
    }

    #[test]
    fn allows_default_with_override_flag() {
        temp_env::with_var_unset("RSI_DAEMON_SOCKET_PATH", || {
            let result = check(true);
            assert!(
                result.is_ok(),
                "--allow-default-socket must override: {result:?}"
            );
        });
    }
}
