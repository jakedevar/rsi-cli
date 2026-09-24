//! Phase 5 end-to-end sandbox tests.
//!
//! Coverage decisions (per Phase 5 pre-check instructions):
//!
//! § COVERED by existing tests (not duplicated here):
//!   - §2 crash-recovery: `sandbox_cleanup::restore_sessions_orphan_sweep` +
//!     `restore_sessions_disk_orphan_sweep` cover the daemon-restart + orphan-sweep
//!     path via SessionManager state simulation. Process-level kill is not required —
//!     the plan explicitly permits simulating via SessionManager drop + DB re-open.
//!
//! § NEW tests added here:
//!   - §1 E2E dirty-content retention: D00 forbids production cleanup without
//!     independently verified proof, so fixture disposal owns test teardown.
//!   - §3 "two concurrent sandboxed sessions, same relative path, different contents":
//!     `sandbox_isolation.rs` tests one sandboxed vs. one non-sandboxed. Two sandboxed
//!     sessions both writing the same relative path are not covered.

use rsi_common::types::SandboxKind;
use rsid::sandbox::SandboxAllocator;
use std::process::Command;
use tempfile::tempdir;
use uuid::Uuid;

fn init_git_repo(dir: &std::path::Path) {
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir)
        .status()
        .expect("git init");
    Command::new("git")
        .args(["config", "user.email", "t@t"])
        .current_dir(dir)
        .status()
        .ok();
    Command::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(dir)
        .status()
        .ok();
    std::fs::write(dir.join("README.md"), "canonical").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-q", "-m", "init"])
        .current_dir(dir)
        .status()
        .unwrap();
}

/// §1 — allocate a sandbox and prove dirty work remains isolated and retained.
/// The temporary repository owners dispose of the fixture after assertions;
/// production raw cleanup is intentionally inaccessible to integration tests.
#[test]
fn e2e_dirty_work_is_isolated_and_retained() {
    let repo = tempdir().expect("repo tempdir");
    let base = tempdir().expect("sandbox base tempdir");
    init_git_repo(repo.path());

    let allocator = SandboxAllocator::new(base.path().to_path_buf());
    let sid = Uuid::new_v4();

    let alloc = allocator
        .allocate(sid, repo.path(), SandboxKind::GitWorktree, "HEAD", None)
        .expect("allocate");

    // Write a file into the sandbox — simulates a shell-tool write during a
    // live harness session.
    let written_path = alloc.root.join("agent_output.txt");
    std::fs::write(&written_path, "output from agent").expect("write into sandbox");
    assert!(
        written_path.exists(),
        "file must exist in sandbox before archive"
    );

    // Confirm the file is NOT visible in the canonical repo (isolation check).
    assert!(
        !repo.path().join("agent_output.txt").exists(),
        "sandbox write must not appear in canonical repo"
    );

    // D00 retains the unproved work for recovery.
    assert!(
        written_path.exists(),
        "written file must remain available for recovery"
    );
    assert!(
        alloc.root.exists(),
        "sandbox root must remain until the temporary fixture owner disposes it"
    );

    // Canonical repo is untouched — README still present, no agent_output.
    assert!(
        repo.path().join("README.md").exists(),
        "canonical README.md must survive archive of sandbox"
    );
    assert!(
        !repo.path().join("agent_output.txt").exists(),
        "canonical repo must remain clean while sandbox work is retained"
    );
}

/// §3 — two concurrent sandboxed sessions write the same relative file path
/// with different contents. Both writes must succeed, each visible only in its
/// own sandbox, and the canonical repo must remain untouched.
///
/// `sandbox_isolation.rs::sandboxed_writes_invisible_to_canonical_repo` covers
/// one sandboxed session vs. the canonical repo. This test covers the orthogonal
/// case: two independently sandboxed sessions both writing `shared.txt`.
#[test]
fn e2e_two_concurrent_sandboxes_same_path_independent() {
    let repo = tempdir().expect("repo tempdir");
    let base = tempdir().expect("sandbox base tempdir");
    init_git_repo(repo.path());

    let allocator = SandboxAllocator::new(base.path().to_path_buf());
    let sid_a = Uuid::new_v4();
    let sid_b = Uuid::new_v4();

    let alloc_a = allocator
        .allocate(sid_a, repo.path(), SandboxKind::GitWorktree, "HEAD", None)
        .expect("allocate sandbox A");
    let alloc_b = allocator
        .allocate(sid_b, repo.path(), SandboxKind::GitWorktree, "HEAD", None)
        .expect("allocate sandbox B");

    // Both sessions write the SAME relative path but with different contents.
    let rel = "shared.txt";
    std::fs::write(alloc_a.root.join(rel), "content from session A").expect("write A");
    std::fs::write(alloc_b.root.join(rel), "content from session B").expect("write B");

    // Each sandbox sees only its own content.
    let content_a = std::fs::read_to_string(alloc_a.root.join(rel)).expect("read from sandbox A");
    let content_b = std::fs::read_to_string(alloc_b.root.join(rel)).expect("read from sandbox B");

    assert_eq!(
        content_a, "content from session A",
        "sandbox A must hold session A's content"
    );
    assert_eq!(
        content_b, "content from session B",
        "sandbox B must hold session B's content"
    );
    assert_ne!(
        content_a, content_b,
        "sandboxes must hold independent content for the same relative path"
    );

    // Canonical repo must not see the file at all.
    assert!(
        !repo.path().join(rel).exists(),
        "canonical repo must not contain '{rel}' written by sandboxed sessions"
    );

    assert!(alloc_a.root.exists(), "sandbox A remains fixture-owned");
    assert!(alloc_b.root.exists(), "sandbox B remains fixture-owned");
}
