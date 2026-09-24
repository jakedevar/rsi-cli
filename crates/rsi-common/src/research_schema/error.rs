//! Error types for the research-doc JSON validator.
//!
//! `ValidationError` represents a single rule violation; `Validation` is the
//! aggregate result returned by [`super::rules::validate_research_json`].
//!
//! Both types serialize to stable JSON for the CLI's stdout contract.

use serde::Serialize;

/// One rule violation in a research-doc JSON file.
///
/// `field` uses dotted/indexed path notation (e.g., `findings[2].summary`).
/// `rule` is one of: `Parse`, `VersionPin`, `Presence`, `WordCap`,
/// `FormatRegex`, `RangeCheck`, `UniqueID`.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("Field {field} failed rule {rule}: {message}")]
pub struct ValidationError {
    pub field: String,
    pub rule: String,
    pub message: String,
}

/// Aggregate result of validating a research-doc JSON.
///
/// `valid` is `true` iff `errors.is_empty()`. `schema_version` reports the
/// `version` field from the parsed doc — `0` if the parse failed before the
/// version could be read. `mode` records which strictness gate produced this
/// result so consumers (the CLI, downstream telemetry) can correlate.
#[derive(Debug, Clone, Serialize)]
pub struct Validation {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
    pub schema_version: u32,
    pub mode: ValidationMode,
}

/// Validation strictness selector for research-doc JSON.
///
/// Mirrors `handoff_schema::ValidationMode` (RSI-013). The two modules keep
/// independent enum copies — they validate different schemas — but share the
/// same Strict/Lenient policy semantics so the CLI flag UX is identical.
///
/// - `Strict` — every rule in the v1 schema fires (write-time gate). This is
///   the original RSI-014 behavior; it remains the only mode in which a
///   research doc can be confidently used as a typed input to downstream
///   skills.
/// - `Lenient` — only the four near-universal rules fire: parse, version pin,
///   `research_question` non-empty, `areas` non-empty (resume-time tolerance
///   for the historical corpus pre-RSI-021). Rules that gate on per-finding
///   formatting (WordCap, FormatRegex, RangeCheck) defer in this mode.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ValidationMode {
    Strict,
    Lenient,
}

impl ValidationMode {
    pub fn is_strict(self) -> bool {
        matches!(self, ValidationMode::Strict)
    }
}
