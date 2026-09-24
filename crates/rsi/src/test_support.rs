//! Test-only helpers shared across the `rsi` crate's Unix-socket-based
//! tests.
//!
//! `UnixListener::bind` fails with `"path must be shorter than SUN_LEN"`
//! (the kernel's ~100-byte `sockaddr_un.sun_path` limit) once the
//! containing directory is long enough — e.g. when `TMPDIR`/
//! `std::env::temp_dir()` points at a sandbox-scoped directory instead of
//! `/tmp`. These helpers bind explicitly under `/tmp`, ignoring the ambient
//! `TMPDIR`, so test socket paths stay short regardless of where the test
//! process itself is sandboxed (#679).

use std::path::PathBuf;

/// A fresh short-lived directory for a test socket, rooted at `/tmp`
/// (not `env::temp_dir()`/`TMPDIR`). Keep the returned `TempDir` alive for
/// as long as a socket under it is in use — dropping it removes the
/// directory.
pub fn short_socket_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .expect("create short-path temp dir for test socket")
}

/// A fresh short-lived socket path directly under `/tmp` (not
/// `env::temp_dir()`/`TMPDIR`), for call sites that build a path without
/// holding a `TempDir` handle. Uniqueness comes from a UUID in the file
/// name, matching the pre-existing convention at these call sites.
pub fn short_socket_path(label: &str) -> PathBuf {
    PathBuf::from("/tmp").join(format!("rsi-test-{label}-{}.sock", uuid::Uuid::new_v4()))
}
