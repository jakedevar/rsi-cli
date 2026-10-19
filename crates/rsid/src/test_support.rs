//! Shared fixtures for rsid unit tests.
//!
//! Sandbox execution scratch refuses a sandbox root on tmpfs/ramfs, and the
//! rolling lander runs its test gates with `TMPDIR` on `/dev/shm`. A fixture
//! built on `TempDir::new()` (which honours `TMPDIR`) therefore passes on a
//! developer host with a disk-backed `TMPDIR` and fails the merge-queue gate
//! (#1182). Any fixture that allocates a sandbox, or otherwise needs a real
//! disk path, uses [`disk_backed_tempdir`].

use tempfile::TempDir;

/// Deterministic synthetic credential with the production mint's wire shape.
/// Labels remain readable in fixtures; they are never accepted as bearers.
pub(crate) fn agent_token(label: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(label.as_bytes()))
}

/// A fresh temporary directory under `$CARGO_TARGET_DIR` (or the workspace
/// `target/`), canonicalized, never under `$TMPDIR`. `label` becomes the
/// directory-name prefix.
pub(crate) fn disk_backed_tempdir(label: &str) -> TempDir {
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(rsi_common::manifest_dir!()).join("../../target"))
        .join("rsid-test-fixtures");
    std::fs::create_dir_all(&base).expect("create disk-backed test fixture root");
    // Sandbox allocation refuses a non-canonical base, and the fallback above
    // contains `..` whenever CARGO_TARGET_DIR is unset (daemon test jobs).
    let base = std::fs::canonicalize(&base).expect("canonicalize disk-backed test fixture root");
    tempfile::Builder::new()
        .prefix(label)
        .tempdir_in(base)
        .expect("create disk-backed test fixture")
}

/// A fresh test directory for a satellite-link unix socket: short enough for
/// `sun_path` (the agent harness `TMPDIR` is long) and with every component
/// canonical. The link custody check refuses a socket path that contains a
/// symlink, and on macOS `/tmp` is itself a symlink to `/private/tmp`, so the
/// bare `/tmp` prefix fails there with `satellite socket path contains a
/// symlink`. Canonicalising the base keeps one portable fixture (hub #2999).
pub(crate) fn socket_tempdir(prefix: &str) -> TempDir {
    let base = std::fs::canonicalize("/tmp").expect("canonicalize /tmp for socket fixtures");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create socket fixture directory")
}
