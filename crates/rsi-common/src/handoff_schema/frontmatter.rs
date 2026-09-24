//! Typed YAML frontmatter for handoff documents (v1).
//!
//! `HandoffFrontmatter` mirrors the contract declared in
//! `.claude/commands/create_handoff.md:55-69`. `#[serde(deny_unknown_fields)]`
//! catches drift — any new key surfaces as a structured error rather than
//! silently passing.
//!
//! Some fields are `Option<String>` because the legacy corpus (pre-April
//! 2026 refactor) did not always carry every key. `validate()` enforces
//! presence post-deserialize; rejecting unknown keys is the deserializer's
//! sole job.

use serde::Deserialize;

/// All frontmatter keys defined for handoff v1.
///
/// The strict variant (`HandoffFrontmatterStrict`) carries
/// `#[serde(deny_unknown_fields)]` and is used at write-time. The lenient
/// variant (this struct) tolerates unknown keys AND non-canonical status
/// strings (legacy corpus uses `in-progress`, `wip`, etc.) — `status` is
/// captured as a free string and re-parsed into [`HandoffStatus`] via
/// [`HandoffFrontmatter::parsed_status`] when needed.
#[derive(Debug, Clone, Deserialize)]
pub struct HandoffFrontmatter {
    pub date: Option<String>,
    pub researcher: Option<String>,
    pub git_commit: Option<String>,
    pub branch: Option<String>,
    pub repository: Option<String>,
    pub topic: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Free string in lenient mode (legacy corpus uses non-canonical
    /// values). Use `parsed_status()` to coerce into the canonical enum.
    pub status: Option<String>,
    pub last_updated: Option<String>,
    pub last_updated_by: Option<String>,
    /// `type` is a Rust keyword; serde maps the YAML `type:` key here.
    #[serde(rename = "type")]
    pub doc_type: Option<String>,
    /// Defaults to v1 (`Some(1)`) when absent during validation; the
    /// deserializer keeps `None` so the validator can distinguish "missing"
    /// from "explicit 1".
    pub schema_version: Option<u32>,
}

impl HandoffFrontmatter {
    /// Re-parse the status string into the canonical enum. Returns
    /// `None` for legacy values (`in-progress`, `wip`, etc.) — callers
    /// that need a strict enum should use [`HandoffFrontmatterStrict`].
    pub fn parsed_status(&self) -> Option<HandoffStatus> {
        match self.status.as_deref()? {
            "complete" => Some(HandoffStatus::Complete),
            "paused" => Some(HandoffStatus::Paused),
            "blocked" => Some(HandoffStatus::Blocked),
            _ => None,
        }
    }
}

/// Strict variant — adds `#[serde(deny_unknown_fields)]`. Write-time
/// validation parses with this struct and surfaces any drift as a
/// frontmatter Schema error. Resume-time validation uses
/// [`HandoffFrontmatter`] directly (extra keys allowed).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffFrontmatterStrict {
    pub date: Option<String>,
    pub researcher: Option<String>,
    pub git_commit: Option<String>,
    pub branch: Option<String>,
    pub repository: Option<String>,
    pub topic: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub status: Option<HandoffStatus>,
    pub last_updated: Option<String>,
    pub last_updated_by: Option<String>,
    #[serde(rename = "type")]
    pub doc_type: Option<String>,
    pub schema_version: Option<u32>,
}

/// Status enum mirrors `<handoff_contract>` in `create_handoff.md`. Lowercase
/// rename matches the on-disk wire format (`status: complete`).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum HandoffStatus {
    Complete,
    Paused,
    Blocked,
}

impl HandoffStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HandoffStatus::Complete => "complete",
            HandoffStatus::Paused => "paused",
            HandoffStatus::Blocked => "blocked",
        }
    }
}

/// Splits a markdown document into `(yaml_str, body)` if the document
/// opens with `---` and contains a closing `---` fence on its own line.
///
/// Mirrors the canonical `split_front_matter` pattern used in
/// `crates/rsid/src/project_workflow.rs`. Returning `None` means "no
/// frontmatter detected"; callers should treat this as a hard validation
/// error (handoffs MUST have frontmatter).
pub fn split_front_matter(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = &trimmed[3..];
    let close_pos = after_first.find("\n---")?;
    let yaml = after_first[..close_pos].trim();
    let body = after_first[close_pos + 4..].trim_start_matches(['\n', '\r']);
    Some((yaml, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frontmatter(yaml: &str) -> Result<HandoffFrontmatter, serde_yaml_ng::Error> {
        serde_yaml_ng::from_str::<HandoffFrontmatter>(yaml)
    }

    #[test]
    fn lenient_variant_tolerates_unknown_fields() {
        let yaml = r#"
date: "2026-04-25"
researcher: jake
status: complete
foo: "drift!"
"#;
        let fm = frontmatter(yaml).expect("lenient variant must accept extra keys");
        assert_eq!(fm.researcher.as_deref(), Some("jake"));
    }

    #[test]
    fn strict_variant_denies_unknown_fields() {
        let yaml = r#"
date: "2026-04-25"
researcher: jake
status: complete
foo: "drift!"
"#;
        let err = serde_yaml_ng::from_str::<HandoffFrontmatterStrict>(yaml)
            .expect_err("unknown field must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("foo"),
            "expected error to name unknown field, got: {msg}"
        );
    }

    #[test]
    fn optional_schema_version_defaults_to_none() {
        let yaml = r#"
date: "2026-04-25"
researcher: jake
status: complete
"#;
        let fm = frontmatter(yaml).expect("must parse");
        assert_eq!(fm.schema_version, None);
    }

    #[test]
    fn explicit_schema_version_one_parses() {
        let yaml = r#"
schema_version: 1
status: blocked
"#;
        let fm = frontmatter(yaml).expect("must parse");
        assert_eq!(fm.schema_version, Some(1));
        assert_eq!(fm.parsed_status(), Some(HandoffStatus::Blocked));
    }

    #[test]
    fn lenient_status_tolerates_legacy_values() {
        let yaml = r#"status: in-progress"#;
        let fm = frontmatter(yaml).expect("must parse");
        // Free string captured.
        assert_eq!(fm.status.as_deref(), Some("in-progress"));
        // Canonical enum coercion returns None for legacy value.
        assert_eq!(fm.parsed_status(), None);
    }

    #[test]
    fn type_keyword_is_renamed() {
        let yaml = r#"
type: "implementation_strategy"
status: complete
"#;
        let fm = frontmatter(yaml).expect("must parse");
        assert_eq!(fm.doc_type.as_deref(), Some("implementation_strategy"));
    }

    #[test]
    fn split_front_matter_extracts_yaml_and_body() {
        let doc = "---\nfoo: bar\n---\n\n## Heading\n\nbody text\n";
        let (yaml, body) = split_front_matter(doc).expect("frontmatter present");
        assert_eq!(yaml, "foo: bar");
        assert!(body.starts_with("## Heading"));
    }

    #[test]
    fn split_front_matter_returns_none_without_fence() {
        assert!(split_front_matter("# just a heading\n").is_none());
    }

    #[test]
    fn strict_variant_rejects_invalid_status() {
        let yaml = r#"status: "in-progress""#;
        let res = serde_yaml_ng::from_str::<HandoffFrontmatterStrict>(yaml);
        assert!(
            res.is_err(),
            "strict variant must reject non-canonical status"
        );
    }
}
