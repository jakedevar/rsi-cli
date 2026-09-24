//! Validation error and result types for handoff schema validation.
//!
//! `Validation` is the top-level result returned by `validate()`; it contains
//! a list of `ValidationError` records (one per failing rule). Both types
//! serialize to JSON for the CLI's stdout protocol.

use serde::Serialize;

/// One rule failure against one field. The `field` is human-readable
/// (e.g. "Immediate Next Action" or "frontmatter.status"); `rule` is a
/// short tag (`"Presence"`, `"WordCap(20)"`, etc.) suitable for telemetry.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{field}: {message}")]
pub struct ValidationError {
    pub field: String,
    pub rule: String,
    pub message: String,
}

/// Top-level validation result. Always serializable; the CLI dumps this
/// to stdout regardless of pass/fail.
#[derive(Debug, Clone, Serialize)]
pub struct Validation {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
    pub schema_version: u32,
    pub mode: ValidationMode,
}

/// Validation strictness selector.
///
/// - `Strict` — every field declared in the v1 schema (write-time gate).
/// - `Lenient` — only the four near-universal sections + frontmatter
///   sanity (resume-time tolerance for legacy docs pre-April-2026).
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
