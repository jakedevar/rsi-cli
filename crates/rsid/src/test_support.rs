//! Shared fixtures for rsid unit tests.
//!
//! Sandbox execution scratch refuses a sandbox root on tmpfs/ramfs, and the
//! rolling lander runs its test gates with `TMPDIR` on `/dev/shm`. A fixture
//! built on `TempDir::new()` (which honours `TMPDIR`) therefore passes on a
//! developer host with a disk-backed `TMPDIR` and fails the merge-queue gate
//! (#1182). Any fixture that allocates a sandbox, or otherwise needs a real
//! disk path, uses [`disk_backed_tempdir`].

use tempfile::TempDir;

/// A fresh temporary directory that is inside no Git work tree, canonical: the
/// fixture for a project root that must not look like a repository. `$TMPDIR`
/// and `$CARGO_TARGET_DIR` sit inside the sandbox's own checkout, so the first
/// candidate that `git` does not place inside a work tree wins.
pub(crate) fn non_repository_tempdir(label: &str) -> TempDir {
    let candidates = [std::path::PathBuf::from("/tmp"), std::env::temp_dir()];
    for base in candidates {
        let Ok(base) = std::fs::canonicalize(&base) else {
            continue;
        };
        let Ok(dir) = tempfile::Builder::new().prefix(label).tempdir_in(base) else {
            continue;
        };
        if !crate::sandbox::git_worktree::origin_admits_sandbox(dir.path()) {
            return dir;
        }
    }
    panic!("no temporary directory outside a Git work tree");
}

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
    rsid_store::test_support::disk_backed_tempdir(label)
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

/// Whether `pid` names a process that still runs. An unreaped zombie does not
/// count: a test that kills a process group orphans its grandchildren, and under
/// a PID namespace whose init never waits (the `unshare --pid --fork` lanes of
/// `scripts/pid-isolate.py`) nobody reaps them, so `kill(pid, 0)` and
/// `/proc/<pid>` keep answering for a process that is already dead (#3662).
#[cfg(target_os = "linux")]
pub(crate) fn process_is_running(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state is the first field after the `)` that closes the command name.
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.trim_start().chars().next());
    !matches!(state, Some('Z' | 'X' | 'x') | None)
}

/// Portable fallback: no `/proc`, so ask the kernel whether the pid exists.
#[cfg(not(target_os = "linux"))]
pub(crate) fn process_is_running(pid: i32) -> bool {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        None::<nix::sys::signal::Signal>,
    )
    .is_ok()
}
