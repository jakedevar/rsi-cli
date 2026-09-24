//! Filesystem isolation test: sandbox writes must not appear in the
//! canonical working directory.
//!
//! This exercises the core security property of the sandbox allocator:
//! writes performed inside an allocated sandbox (simulating a second
//! sandboxed session) must be invisible to a concurrently running
//! non-sandboxed session against the same repo.
//!
//! The test creates a real git repo, allocates one sandbox, writes a
//! "secret" file from inside the sandbox, and verifies the canonical repo
//! has no trace of it. This directly models the
//! "launch two sessions against the same repo, one sandboxed, one not;
//! sandboxed writes do not appear in the canonical working_dir" criterion
//! from the plan.

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

#[test]
fn sandboxed_writes_invisible_to_canonical_repo() {
    let repo = tempdir().expect("repo tempdir");
    let base = tempdir().expect("sandbox base tempdir");
    init_git_repo(repo.path());

    let allocator = SandboxAllocator::new(base.path().to_path_buf());
    let sandboxed_sid = Uuid::new_v4();

    // Session A: NOT sandboxed — operates on repo directly (writes would
    // be seen in the canonical tree; this is the control).
    // We simulate by writing a file directly to repo.
    std::fs::write(repo.path().join("session_a.txt"), "from A").unwrap();

    // Session B: sandboxed — allocate a sandbox against the same repo.
    let allocation = allocator
        .allocate(
            sandboxed_sid,
            repo.path(),
            SandboxKind::GitWorktree,
            "HEAD",
            None,
        )
        .expect("allocate must succeed");

    // Simulate session B writing into its sandbox. This is the critical
    // write: it MUST NOT be visible in the canonical repo working tree
    // (the sandbox is an independent checkout of the same commit).
    let secret_path = allocation.root.join("session_b_secret.txt");
    std::fs::write(&secret_path, "from B — should be invisible").expect("write in sandbox");
    assert!(secret_path.exists(), "sandbox write must land in sandbox");

    // Canonical repo must not see the sandboxed write.
    let canonical_secret = repo.path().join("session_b_secret.txt");
    assert!(
        !canonical_secret.exists(),
        "sandbox write leaked into canonical repo at {}",
        canonical_secret.display()
    );

    // And the canonical write from Session A must NOT show up in the
    // sandbox (worktrees have independent working trees).
    let sandbox_view_of_a = allocation.root.join("session_a.txt");
    assert!(
        !sandbox_view_of_a.exists(),
        "canonical write leaked into sandbox at {}",
        sandbox_view_of_a.display()
    );

    // Raw destruction is sandbox-module-private under D00. The temporary
    // repository/base owners dispose of this isolated fixture after the
    // assertions, while production lifecycle callers remain clamped.
    assert!(
        allocation.root.exists(),
        "fixture-owned sandbox was retained"
    );
}
