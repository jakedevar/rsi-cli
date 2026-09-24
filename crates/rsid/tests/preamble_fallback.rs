//! Integration test for the variant-missing-falls-back-to-base path.
//!
//! Because `preamble::harness_root()` uses `OnceLock`, the first call in
//! this test binary's process locks in whichever path discovery resolves.
//! We set `RSI_HARNESS_ROOT` BEFORE any test runs (via `ctor` is overkill;
//! we just set it at the top of the test function and rely on the fact
//! that no other test in this binary calls preamble::* first).
//!
//! Each integration test file in cargo is its own binary, so this binary's
//! `OnceLock` is independent of `preamble_variant.rs`'s OnceLock.

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

    // Point harness_root() at our tempdir BEFORE any preamble::* call.
    // Safe: this is the only test in this binary that touches preamble,
    // and it's the first call so the OnceLock initializer reads our env.
    // SAFETY: tests in this binary run serially by virtue of being the
    // only test that touches `preamble`; setting an env var in a single
    // test is the conventional pattern even though `set_var` is unsafe.
    unsafe {
        std::env::set_var("RSI_HARNESS_ROOT", tmp.path());
    }

    // `load()` appends the binary-embedded agent-discovery nudge to the
    // disk-resolved preamble, so the returned content starts with the selected
    // disk file and carries the nudge tail. The fallback assertion is therefore
    // "starts with base + contains nudge".
    let nudge = preamble::AGENT_DISCOVERY_NUDGE;

    // Bug has a variant file in the real repo, but our tempdir does NOT
    // — so loading should fall back to the base content we just wrote.
    let content = preamble::load(SessionKind::Bug).expect("base fallback should yield content");
    assert!(
        content.starts_with(base_content),
        "Bug variant missing should fall back to base content from RSI_HARNESS_ROOT"
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
        let content =
            preamble::load(kind).expect("base fallback should yield content for all kinds");
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
    let std_content = preamble::load(SessionKind::Standard).expect("base preamble for Standard");
    assert!(std_content.starts_with(base_content));
    assert!(std_content.contains(nudge));
}
