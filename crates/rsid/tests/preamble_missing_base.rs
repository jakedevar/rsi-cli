//! Integration test: a session in a project with no `.claude/` tree at all
//! gets the binary-embedded RSI control policy and nothing that names a file
//! the project does not have. Launch is never aborted.

use rsi_common::types::SessionKind;
use rsid::session::preamble;
use tempfile::TempDir;

#[test]
fn project_without_claude_tree_gets_embedded_policy_and_no_dangling_paths() {
    let tmp = TempDir::new().expect("tempdir");
    std::fs::create_dir(tmp.path().join(".git")).expect("git marker");

    for kind in [SessionKind::Bug, SessionKind::Standard] {
        let content =
            preamble::load(kind, tmp.path()).expect("embedded RSI policy should always load");
        assert!(
            content.contains(preamble::agent_discovery_nudge()),
            "missing agent discovery nudge for {kind:?}"
        );
        assert!(
            content.contains(preamble::RSI_BACKEND_POLICY),
            "missing RSI backend policy for {kind:?}"
        );
        assert!(
            content.contains(preamble::DAEMON_MESSAGE_CONVENTION),
            "missing daemon message convention for {kind:?}"
        );
        // Exactly the generic part: no project preamble, no thoughts policy.
        assert_eq!(content, preamble::load_generic(Some(tmp.path())));
    }
    assert_eq!(preamble::load_orchestration_router(tmp.path()), None);
}
