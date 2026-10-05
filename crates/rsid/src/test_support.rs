//! Shared fixtures for rsid unit tests.
//!
//! Sandbox execution scratch refuses a sandbox root on tmpfs/ramfs, and the
//! rolling lander runs its test gates with `TMPDIR` on `/dev/shm`. A fixture
//! built on `TempDir::new()` (which honours `TMPDIR`) therefore passes on a
//! developer host with a disk-backed `TMPDIR` and fails the merge-queue gate
//! (#1182). Any fixture that allocates a sandbox, or otherwise needs a real
//! disk path, uses [`disk_backed_tempdir`].

use tempfile::TempDir;

/// A fresh temporary directory under `$CARGO_TARGET_DIR` (or the workspace
/// `target/`), canonicalized, never under `$TMPDIR`. `label` becomes the
/// directory-name prefix.
pub(crate) fn disk_backed_tempdir(label: &str) -> TempDir {
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
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
