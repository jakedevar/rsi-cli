//! Canonical harness-version hash computation.
//!
//! SHA256 over: optional kind discriminant + 0x00 separator + system_prompt + 0x00 + query.
//! Used at session launch AND at rotation-child launch so every Session.harness_version_hash
//! reflects the actual harness inputs seen by the provider subprocess.

use rsi_common::types::SessionKind;
use sha2::{Digest, Sha256};

/// Compute the harness-version hash for a session's launch/rotation inputs.
///
/// Input schema:
///   `SHA256( ["kind:" || Debug(kind) || 0x00]?  || system_prompt_bytes? || 0x00 || query_bytes )`
///
/// The optional kind discriminant prefix guarantees that two kinds producing
/// coincidentally identical resolved system_prompts still hash distinctly.
/// Pass `None` for the legacy hash schema (used by existing call sites that
/// haven't yet adopted kind tagging, and for backwards-compat in tests).
///
/// Rationale for the 0x00 separator: prevents ambiguity between an empty
/// system prompt with a long query vs. a prompt whose trailing bytes match the
/// start of the query.
///
/// Rationale for `Option<&str>` on system_prompt: launch path may have no system
/// prompt configured (passes `None`); rotation-child path always has one (passes
/// `Some`). Returning the hex digest as `String` matches the storage type
/// (`Session.harness_version_hash: Option<String>`).
pub fn compute_harness_version_hash(
    system_prompt: Option<&str>,
    query: &str,
    kind: Option<SessionKind>,
) -> String {
    let mut hasher = Sha256::new();
    if let Some(k) = kind {
        // Tag prefix: "kind:<Variant>\x00". Cheap salt that guarantees two kinds
        // with coincidentally identical resolved system_prompts still produce
        // distinct hashes.
        hasher.update(b"kind:");
        hasher.update(format!("{:?}", k).as_bytes());
        hasher.update(b"\x00");
    }
    if let Some(prompt) = system_prompt {
        hasher.update(prompt.as_bytes());
    }
    hasher.update(b"\x00");
    hasher.update(query.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_same_inputs_produce_same_hash() {
        let a = compute_harness_version_hash(Some("prompt"), "query", None);
        let b = compute_harness_version_hash(Some("prompt"), "query", None);
        assert_eq!(a, b);
    }

    #[test]
    fn different_prompts_produce_different_hashes() {
        let a = compute_harness_version_hash(Some("prompt_a"), "query", None);
        let b = compute_harness_version_hash(Some("prompt_b"), "query", None);
        assert_ne!(a, b);
    }

    #[test]
    fn different_queries_produce_different_hashes() {
        let a = compute_harness_version_hash(Some("prompt"), "query_a", None);
        let b = compute_harness_version_hash(Some("prompt"), "query_b", None);
        assert_ne!(a, b);
    }

    #[test]
    fn none_prompt_differs_from_empty_prompt() {
        // Both should produce valid digests, and they should be identical
        // because the 0x00 separator is always hashed. Documents the choice:
        // None and Some("") are equivalent in this hash scheme.
        let a = compute_harness_version_hash(None, "query", None);
        let b = compute_harness_version_hash(Some(""), "query", None);
        assert_eq!(a, b);
    }

    #[test]
    fn separator_prevents_boundary_collision() {
        // "ab" + "c" must differ from "a" + "bc" thanks to the 0x00 separator.
        let a = compute_harness_version_hash(Some("ab"), "c", None);
        let b = compute_harness_version_hash(Some("a"), "bc", None);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_lowercase_hex_64_chars() {
        let hash = compute_harness_version_hash(Some("prompt"), "query", None);
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn different_kinds_produce_different_hashes() {
        // RSI-012: two kinds with identical (system_prompt, query) must hash
        // distinctly. The kind discriminant prefix guarantees this even if
        // the resolved prompts happened to collide.
        let bug_hash = compute_harness_version_hash(
            Some("identical prompt"),
            "identical query",
            Some(SessionKind::Bug),
        );
        let feature_hash = compute_harness_version_hash(
            Some("identical prompt"),
            "identical query",
            Some(SessionKind::Feature),
        );
        let refactor_hash = compute_harness_version_hash(
            Some("identical prompt"),
            "identical query",
            Some(SessionKind::Refactor),
        );
        let research_hash = compute_harness_version_hash(
            Some("identical prompt"),
            "identical query",
            Some(SessionKind::Research),
        );
        assert_ne!(bug_hash, feature_hash);
        assert_ne!(bug_hash, refactor_hash);
        assert_ne!(bug_hash, research_hash);
        assert_ne!(feature_hash, refactor_hash);
        assert_ne!(feature_hash, research_hash);
        assert_ne!(refactor_hash, research_hash);
    }

    #[test]
    fn none_kind_matches_legacy_behavior() {
        // The pre-RSI-012 hash contract — kind=None — must remain stable
        // for old test fixtures and backwards compatibility.
        let pre = compute_harness_version_hash(Some("p"), "q", None);
        // Reconstruct the legacy hash explicitly (no kind discriminant).
        let mut hasher = Sha256::new();
        hasher.update(b"p");
        hasher.update(b"\x00");
        hasher.update(b"q");
        let legacy = format!("{:x}", hasher.finalize());
        assert_eq!(pre, legacy);
    }
}
