//! Test-only helper shared by the library and the binary test modules (each
//! includes this file with `#[path]`; the binary cannot see lib `cfg(test)`).
//!
//! A unix socket path must fit `sun_path` (104 bytes on macOS, 108 on Linux).
//! The agent harness points `TMPDIR` at a long per-sandbox directory, so
//! `tempfile::tempdir()` plus a socket file name can exceed it. Production
//! socket paths are unaffected; only test sockets use this directory.

use std::path::PathBuf;
use tempfile::TempDir;

/// Longest socket file name a test binds (`ingress.sock`, `daemon.sock`)
/// plus the temp-dir's own `.tmpXXXXXX` component and separators.
const SOCKET_HEADROOM: usize = 32;
/// Stay under the smaller (macOS) `sun_path` limit of 104 bytes.
const MAX_SOCKET_PATH: usize = 100;

/// The base for socket test directories: the ordinary temp dir when a socket
/// path under it fits, else `/tmp`.
fn socket_base() -> PathBuf {
    let tmp = std::env::temp_dir();
    if tmp.as_os_str().len() + SOCKET_HEADROOM <= MAX_SOCKET_PATH {
        tmp
    } else {
        PathBuf::from("/tmp")
    }
}

/// A fresh test directory short enough to hold a unix socket; removed on drop.
pub fn short_tempdir() -> std::io::Result<TempDir> {
    tempfile::Builder::new()
        .prefix("rr-")
        .tempdir_in(socket_base())
}
