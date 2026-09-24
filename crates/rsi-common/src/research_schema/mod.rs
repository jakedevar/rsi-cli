//! v1 JSON-schema validator for `<doc>.json` research sidecars (RSI-014).
//!
//! ## Scope
//!
//! `<doc>.json` is the machine-readable companion to `<doc>.md` emitted by
//! `/research` and `/team_research`. The markdown remains
//! the human artifact; the JSON exists so planner skills (`/plan`,
//! `/team_plan`) and downstream daemon consumers (RSI-007 Dreamer)
//! parse a typed contract instead of grepping markdown.
//!
//! ## Public surface
//!
//! - [`validate_research_json`] — strict-mode wrapper, preserved for
//!   backwards compatibility with RSI-014's original API.
//! - [`validate_research_json_with_mode`] — explicit-mode validator.
//!   `Lenient` enforces only the four near-universal rules (parse,
//!   version pin, `research_question` non-empty, `areas` non-empty);
//!   `Strict` enforces every v1 rule. RSI-021 added the lenient mode.
//! - [`ValidationMode`] — strictness selector. The CLI defaults to
//!   `Lenient` (resume-time tolerance for legacy docs); `--strict` opts
//!   into the write-time gate.
//! - [`RESEARCH_SCHEMA_VERSION`] — the current schema version pin.
//! - Type re-exports ([`ResearchDoc`], [`Finding`], [`FileRef`],
//!   [`OpenQuestion`], [`Confidence`]) so daemon-side consumers can
//!   deserialize without re-declaring the schema.
//!
//! ## Strict vs. lenient mode
//!
//! The two modes share the four near-universal "lenient floor" rules:
//! parse, version pin, `research_question` non-empty, `areas` non-empty.
//! These are the invariants every research doc — historical or new —
//! must satisfy for any downstream consumer to extract useful signal.
//!
//! Strict mode additionally fires per-finding/per-question hygiene rules
//! (`WordCap` 25-word cap on summaries, `FormatRegex` on `findings[i].file_ref`,
//! `RangeCheck` on `file_refs[i].lines`, and on PRESENT `findings[i].id`
//! values: a default `Presence`-style rejection for empty/whitespace-only ids
//! plus `UniqueID` for duplicates). These are write-time-only — legacy
//! docs predate them, so a resume-time validator that fired them would
//! reject the bulk of the historical corpus.
//!
//! Mirrors `handoff_schema`'s lenient/strict split (RSI-013).
//!
//! ## Versioning
//!
//! Current pin is **v2** ([`RESEARCH_SCHEMA_VERSION`] `= 2`). v2 is a strict
//! superset of v1: the sole schema delta is the optional `Finding.id`
//! provenance join key
//! (`#[serde(default, skip_serializing_if = "Option::is_none")]`). A v1 doc
//! omits `id`, deserializes to `None`, and re-serializes byte-stable, so every
//! pre-existing v1 sidecar validates green under the v2 pin — zero forced
//! migration. Strict mode does require that any PRESENT `id` be non-empty and
//! unique within the document (rule `UniqueID`); because v1 docs never carry
//! the key, that rule never fires on legacy output. `#[serde(deny_unknown_fields)]`
//! on every struct still guarantees that a *future* v3 field (any other unknown
//! key) is hard-rejected by a v2 binary — exit 2 → consumer falls back to
//! markdown.

pub mod error;
pub mod rules;
pub mod schema;

pub use error::{Validation, ValidationError, ValidationMode};
pub use rules::{
    RESEARCH_SCHEMA_VERSION, validate_research_json, validate_research_json_with_mode,
};
pub use schema::{Confidence, FileRef, Finding, OpenQuestion, ResearchDoc};

use std::path::{Path, PathBuf};

/// Result of probing for a JSON sidecar next to a markdown research doc.
///
/// Consumer skills (`/plan`, `/team_plan`) and daemon-side
/// consumers (RSI-007 Dreamer) call [`probe_or_fallback`] to decide whether
/// to read the typed JSON or fall back to markdown Read+Grep.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ResearchSource {
    /// JSON sidecar exists and validates against the v1 schema.
    JsonValid(ResearchDoc),
    /// JSON sidecar is missing, unreadable, or invalid. Consumer should fall
    /// back to parsing the markdown at the contained path.
    MarkdownFallback(PathBuf),
}

/// Probe for a `<doc>.json` sibling next to `md_path` and return the
/// strongest available source.
///
/// Decision tree:
/// 1. If `<doc>.json` does not exist → `MarkdownFallback`.
/// 2. If reading the JSON fails (I/O) → `MarkdownFallback`.
/// 3. If the JSON does not validate (parse error or schema violation) →
///    `MarkdownFallback`.
/// 4. Otherwise → `JsonValid(ResearchDoc)`.
///
/// The helper is silent on the fallback path — it never logs, panics, or
/// returns an error. The caller decides whether to surface the reason.
/// Callers wanting a structured reason should call [`validate_research_json`]
/// directly.
pub fn probe_or_fallback(md_path: &Path) -> ResearchSource {
    let json_path = md_path.with_extension("json");
    if !json_path.exists() {
        return ResearchSource::MarkdownFallback(md_path.to_path_buf());
    }
    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(_) => return ResearchSource::MarkdownFallback(md_path.to_path_buf()),
    };
    let v = validate_research_json(&content);
    if !v.valid {
        return ResearchSource::MarkdownFallback(md_path.to_path_buf());
    }
    match serde_json::from_str::<ResearchDoc>(&content) {
        Ok(doc) => ResearchSource::JsonValid(doc),
        Err(_) => ResearchSource::MarkdownFallback(md_path.to_path_buf()),
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use std::fs;

    fn write_md(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, "# Research: stub\n\n## Research Question\nQ?\n").unwrap();
        p
    }

    #[test]
    fn no_json_sibling_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let md = write_md(dir.path(), "stub.md");
        match probe_or_fallback(&md) {
            ResearchSource::MarkdownFallback(p) => assert_eq!(p, md),
            other => panic!("expected fallback, got {:?}", other),
        }
    }

    #[test]
    fn valid_json_yields_research_doc() {
        let dir = tempfile::tempdir().unwrap();
        let md = write_md(dir.path(), "stub.md");
        let json = md.with_extension("json");
        fs::write(
            &json,
            r#"{
                "version": 1,
                "research_question": "Q?",
                "areas": ["A"],
                "findings": [],
                "file_refs": [],
                "open_questions": []
            }"#,
        )
        .unwrap();
        match probe_or_fallback(&md) {
            ResearchSource::JsonValid(doc) => {
                assert_eq!(doc.research_question, "Q?");
                assert_eq!(doc.version, 1);
            }
            other => panic!("expected JsonValid, got {:?}", other),
        }
    }

    #[test]
    fn invalid_json_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let md = write_md(dir.path(), "stub.md");
        let json = md.with_extension("json");
        // version: 99 is rejected by VersionPin.
        fs::write(
            &json,
            r#"{
                "version": 99,
                "research_question": "Q?",
                "areas": ["A"],
                "findings": [],
                "file_refs": [],
                "open_questions": []
            }"#,
        )
        .unwrap();
        match probe_or_fallback(&md) {
            ResearchSource::MarkdownFallback(p) => assert_eq!(p, md),
            other => panic!("expected fallback, got {:?}", other),
        }
    }

    #[test]
    fn malformed_json_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let md = write_md(dir.path(), "stub.md");
        let json = md.with_extension("json");
        fs::write(&json, "{not json").unwrap();
        match probe_or_fallback(&md) {
            ResearchSource::MarkdownFallback(p) => assert_eq!(p, md),
            other => panic!("expected fallback, got {:?}", other),
        }
    }
}
