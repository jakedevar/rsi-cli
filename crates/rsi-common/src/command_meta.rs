//! Command-frontmatter parser for `.claude/commands/*.md` (RSI-010).
//!
//! Pure parser — no I/O beyond `parse_command_file`, no registry state. Lives in
//! `rsi-common` so the daemon (`rsid::command_frontmatter::CommandRegistry`),
//! the TUI (`crates/rsi`), and the `rsi-diag` CLI can all invoke the same
//! YAML front-matter shape.
//!
//! Shape mirrors `project_workflow::parse_rsi_md` in rsid: `---` fence,
//! YAML-serde into a struct with all fields optional, unknown fields ignored
//! for forward compatibility.

use crate::types::CapabilityClass;
use serde::Deserialize;
use std::path::Path;

/// Parsed YAML frontmatter from a `.claude/commands/*.md` file.
/// Every field is optional — absent fields map to `None`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CommandMeta {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub capability_class: Option<CapabilityClass>,
}

/// Extract YAML front matter from command file content.
/// Returns `(yaml_str, body)`. Returns `None` if no `---`-fenced frontmatter
/// is present (matching `rsid::project_workflow::split_front_matter` semantics).
fn split_front_matter(content: &str) -> Option<(&str, &str)> {
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

/// Parse a command file's full content into a `CommandMeta`.
///
/// - No front-matter fence → returns `Ok(default)` (no class declared).
/// - Invalid YAML → returns `Err(message)` so the caller can log+skip.
/// - Unknown fields are ignored (forward compatibility).
pub fn parse_command_content(content: &str) -> Result<CommandMeta, String> {
    match split_front_matter(content) {
        Some((yaml, _body)) => serde_yaml_ng::from_str::<CommandMeta>(yaml)
            .map_err(|e| format!("YAML parse error: {}", e)),
        None => Ok(CommandMeta::default()),
    }
}

/// Plan-spec alias (plan §1.2 — `parse_command_frontmatter`). Thin wrapper
/// that treats any parse failure as "no frontmatter" so TUI-facing callers get
/// an infallible `Option<CommandMeta>` view.
pub fn parse_command_frontmatter(content: &str) -> Option<CommandMeta> {
    parse_command_content(content).ok()
}

/// Parse a single command file from disk.
pub fn parse_command_file(path: &Path) -> Result<CommandMeta, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("read error: {}", e))?;
    parse_command_content(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_frontmatter_reads_capability_class() {
        let content =
            "---\ndescription: foo\nmodel: opus\ncapability_class: architect\n---\n# Body";
        let fm = parse_command_frontmatter(content).unwrap();
        assert_eq!(fm.description.as_deref(), Some("foo"));
        assert_eq!(fm.model.as_deref(), Some("opus"));
        assert_eq!(fm.capability_class, Some(CapabilityClass::Architect));
    }

    #[test]
    fn parse_command_frontmatter_missing_fence_returns_default() {
        let content = "no frontmatter at all, just body";
        let fm = parse_command_frontmatter(content).unwrap();
        assert!(fm.description.is_none());
        assert!(fm.capability_class.is_none());
    }

    #[test]
    fn parse_command_frontmatter_missing_class() {
        let content = "---\ndescription: bar\n---\nBody";
        let fm = parse_command_frontmatter(content).unwrap();
        assert_eq!(fm.description.as_deref(), Some("bar"));
        assert!(fm.capability_class.is_none());
    }

    #[test]
    fn parse_command_frontmatter_malformed_yaml_returns_none() {
        let content = "---\n: invalid: yaml: [\n---\nBody.";
        assert!(parse_command_frontmatter(content).is_none());
    }

    #[test]
    fn parse_command_content_snake_case_variants() {
        for (yaml, expected) in [
            ("architect", CapabilityClass::Architect),
            ("implementer", CapabilityClass::Implementer),
            ("lookup_fast", CapabilityClass::LookupFast),
        ] {
            let content = format!("---\ncapability_class: {}\n---\nbody", yaml);
            let fm = parse_command_content(&content).unwrap();
            assert_eq!(fm.capability_class, Some(expected));
        }
    }

    #[test]
    fn parse_command_content_unknown_fields_ignored() {
        let content = "---\ncapability_class: lookup_fast\nunknown_field: value\n---\nBody.";
        let fm = parse_command_content(content).unwrap();
        assert_eq!(fm.capability_class, Some(CapabilityClass::LookupFast));
    }
}
