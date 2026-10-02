//! Integration test for the variant-missing-falls-back-to-base path.
//!
//! The project part is read from the session's own tree, so the test points
//! the loader at a temp project that ships only the base file.

use rsi_common::types::SessionKind;
use rsid::session::preamble;
use tempfile::TempDir;

#[test]
fn variant_missing_falls_back_to_base() {
    // Build a tempdir layout that contains ONLY the base file:
    //   <tmp>/.claude/commands/_shared/worker_preamble.md
    let tmp = TempDir::new().expect("tempdir");
    let shared = tmp.path().join(".claude/commands/_shared");
    std::fs::create_dir_all(&shared).expect("mkdir -p shared");
    let base_path = shared.join("worker_preamble.md");
    let base_content = "BASE PREAMBLE FOR FALLBACK TEST";
    std::fs::write(&base_path, base_content).expect("write base file");

    // `load()` appends the binary-embedded agent-discovery nudge to the
    // disk-resolved preamble, so the returned content starts with the selected
    // disk file and carries the nudge tail. The fallback assertion is therefore
    // "starts with base + contains nudge".
    let nudge = preamble::agent_discovery_nudge();

    // Bug has a variant file in the rsi repo, but the temp project does NOT
    // — so loading should fall back to the base content we just wrote.
    let content =
        preamble::load(SessionKind::Bug, tmp.path()).expect("base fallback should yield content");
    assert!(
        content.starts_with(base_content),
        "Bug variant missing should fall back to base content from the session project"
    );
    assert!(
        content.contains(nudge),
        "load() must append the agent nudge"
    );

    // Same for Feature/Refactor/Research — none have files in our tempdir.
    for kind in [
        SessionKind::Feature,
        SessionKind::Refactor,
        SessionKind::Research,
    ] {
        let content = preamble::load(kind, tmp.path())
            .expect("base fallback should yield content for all kinds");
        assert!(
            content.starts_with(base_content),
            "{:?} variant missing should fall back to base",
            kind
        );
        assert!(
            content.contains(nudge),
            "load() must append the agent nudge"
        );
    }

    // Standard has no variant file by design — also returns base (+ nudge).
    let std_content =
        preamble::load(SessionKind::Standard, tmp.path()).expect("base preamble for Standard");
    assert!(std_content.starts_with(base_content));
    assert!(std_content.contains(nudge));
}
