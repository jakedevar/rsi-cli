//! Integration test: when the configured harness root contains no
//! `.claude/` tree at all, `preamble::load` still returns the binary-embedded
//! RSI control policy gracefully (logs a warn but does NOT panic, error, or
//! abort the launch).
//!
//! This is the worst-case fallback rung; the daemon must remain
//! launch-capable even if the harness directory has been deleted or
//! moved out from under it.

use rsi_common::types::SessionKind;
use rsid::session::preamble;
use tempfile::TempDir;

#[test]
fn missing_harness_root_returns_embedded_policy_without_panic() {
    let tmp = TempDir::new().expect("tempdir");
    // Tempdir is empty — no .claude/ subtree.
    // Setting RSI_HARNESS_ROOT here only takes effect if it's the first
    // call to preamble::harness_root() in this test binary. Because the
    // env var resolution checks for a base preamble at the supplied path
    // and FAILS (file not found), the discovery will fall through to
    // walk_up_for_claude(cwd) and walk_up_for_claude(current_exe) — both
    // of which will succeed in the cargo test environment. So we cannot
    // reliably assert `None` here without controlling the cwd.
    //
    // Instead: directly probe the public surface that does NOT consult
    // the cached harness_root: walk_up_for_claude is private, so we
    // assert the FALLBACK behavior at a level we can control —
    // load(kind) under a non-existent harness root that ALSO sits
    // outside any repo (the system temp dir typically does NOT have a
    // .claude/ ancestor). If load(kind) returns Some content, the test
    // is inconclusive (env contamination) but still must not panic.
    unsafe {
        std::env::set_var("RSI_HARNESS_ROOT", tmp.path());
    }

    // Should not panic regardless of disk-discovery outcome. The embedded
    // policy must be present even when the project-local preamble is absent or
    // discovery falls through to another harness root.
    for kind in [SessionKind::Bug, SessionKind::Standard] {
        let content = preamble::load(kind).expect("embedded RSI policy should always load");
        assert!(
            content.contains(preamble::AGENT_DISCOVERY_NUDGE),
            "missing agent discovery nudge for {kind:?}"
        );
        assert!(
            content.contains(preamble::RSI_BACKEND_POLICY),
            "missing RSI backend policy for {kind:?}"
        );
    }
}
