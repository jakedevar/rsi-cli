//! Parser for the `<docregblock>/spawn_child …</docregblock>` directive.
//!
//! Mirrors the handoff-directive precedent in monitor.rs but produces a
//! richer struct (kind + optional model + optional effort + multiline query).
//!
//! Directive grammar:
//!
//! ```text
//! <docregblock>
//! /spawn_child kind=Task provider=Claude model=claude-opus-4-7 effort=high agent_role=Planner topology_node=plan_v1 iteration=2 tags=alpha,beta
//! QUERY:
//! <multiline body until close tag>
//! </docregblock>
//! ```
//!
//! - Header is single-line with `key=value` whitespace-separated fields.
//! - Required: `kind`. Optional: `provider`, `model`, `effort`, `agent_role`, `topology_node`, `iteration`, `tags`.
//! - Directive roles cannot contain spaces; use JSON/native transport for multi-word roles.
//! - `QUERY:` introduces the multiline body; body ends at `</docregblock>`.
//! - Unknown header keys are logged-and-ignored (forward compatibility).

use rsi_common::types::SessionKind;

/// A successfully parsed spawn-child directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnDirective {
    pub kind: SessionKind,
    /// Explicit child provider. `None` inherits the emitting lead's provider.
    pub provider: Option<rsi_common::types::SessionProvider>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent_role: Option<String>,
    pub query: String,
    /// Bound topology node id. `None` = unbound spawn.
    pub topology_node: Option<String>,
    /// Explicit iteration override. `None` = daemon auto-increments.
    pub iteration: Option<u32>,
    /// Tag override set. `None` = inherit emitter's tags. `Some(vec![])` is
    /// rejected at parse time (malformed).
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnDirectiveParseError {
    #[error("missing required key: {0}")]
    MissingKey(&'static str),
    #[error("unknown kind: {0}")]
    UnknownKind(String),
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    #[error("malformed: {0}")]
    Malformed(&'static str),
}

impl SpawnDirective {
    /// Parse a complete `<docregblock>…</docregblock>` block.
    ///
    /// Returns:
    /// - `Ok(Some(directive))` when the block is a well-formed `/spawn_child` directive.
    /// - `Ok(None)` when the block opens correctly but the header is some other
    ///   directive (e.g. `/resume_handoff`) — the caller should ignore the block.
    /// - `Err(_)` for `/spawn_child` blocks whose content is malformed.
    pub fn parse(block: &str) -> Result<Option<Self>, SpawnDirectiveParseError> {
        let block_trimmed = block.trim();
        if !block_trimmed.starts_with("<docregblock>") {
            return Err(SpawnDirectiveParseError::Malformed("missing open tag"));
        }
        if !block_trimmed.ends_with("</docregblock>") {
            return Err(SpawnDirectiveParseError::Malformed("missing close tag"));
        }

        // Strip tags
        let inner =
            &block_trimmed["<docregblock>".len()..block_trimmed.len() - "</docregblock>".len()];
        let inner_trimmed = inner.trim();

        if !inner_trimmed.starts_with("/spawn_child") {
            // A different directive (e.g. `/resume_handoff`). Caller ignores.
            return Ok(None);
        }

        // Find "QUERY:" marker
        let query_idx = inner_trimmed
            .find("QUERY:")
            .ok_or(SpawnDirectiveParseError::Malformed("missing QUERY: marker"))?;

        let header = inner_trimmed[..query_idx].trim();
        let body = inner_trimmed[query_idx + "QUERY:".len()..]
            .trim()
            .to_string();

        if header.is_empty() {
            return Err(SpawnDirectiveParseError::Malformed("missing header"));
        }

        if !header.starts_with("/spawn_child ") && header != "/spawn_child" {
            return Ok(None);
        }

        // Header fields: kind=Task provider=… model=… effort=…
        let header_rest = header.strip_prefix("/spawn_child").unwrap_or("").trim();
        let fields: std::collections::HashMap<&str, &str> = header_rest
            .split_whitespace()
            .filter_map(|tok| tok.split_once('='))
            .collect();
        let kind_str = fields
            .get("kind")
            .ok_or(SpawnDirectiveParseError::MissingKey("kind"))?;
        let kind = parse_kind(kind_str)
            .ok_or_else(|| SpawnDirectiveParseError::UnknownKind((*kind_str).to_string()))?;
        let provider = fields
            .get("provider")
            .map(|value| {
                serde_json::from_value(serde_json::Value::String((*value).to_string()))
                    .map_err(|_| SpawnDirectiveParseError::UnknownProvider((*value).to_string()))
            })
            .transpose()?;
        let model = fields.get("model").map(|s| (*s).to_string());
        let effort = fields.get("effort").map(|s| (*s).to_string());
        let agent_role =
            rsi_common::agent_coordination::normalize_agent_role(fields.get("agent_role").copied())
                .map_err(|_| SpawnDirectiveParseError::Malformed("invalid agent_role"))?;

        // Explicit extraction for P1.7 keys — before unknown-key fallthrough.
        let topology_node = match fields.get("topology_node") {
            None => None,
            Some(v) if v.is_empty() => {
                return Err(SpawnDirectiveParseError::Malformed("invalid topology_node"));
            }
            Some(v) if v.chars().all(|c| c.is_alphanumeric() || "-_/".contains(c)) => {
                Some((*v).to_string())
            }
            Some(_) => {
                return Err(SpawnDirectiveParseError::Malformed("invalid topology_node"));
            }
        };
        let iteration = match fields.get("iteration") {
            None => None,
            Some(v) => Some(
                v.parse::<u32>()
                    .map_err(|_| SpawnDirectiveParseError::Malformed("invalid iteration"))?,
            ),
        };
        let tags: Option<Vec<String>> = match fields.get("tags") {
            None => None,
            Some(v) if v.is_empty() => {
                return Err(SpawnDirectiveParseError::Malformed(
                    "invalid tags: must not be empty",
                ));
            }
            Some(v) => {
                let raw: Vec<&str> = v.split(',').collect();
                let mut normalized = Vec::with_capacity(raw.len());
                for t in raw {
                    match rsi_common::normalize_tag(t.trim()) {
                        Ok(n) => normalized.push(n),
                        Err(_) => {
                            return Err(SpawnDirectiveParseError::Malformed("invalid tags"));
                        }
                    }
                }
                if normalized.is_empty() {
                    return Err(SpawnDirectiveParseError::Malformed(
                        "invalid tags: must not be empty",
                    ));
                }
                Some(normalized)
            }
        };

        Ok(Some(SpawnDirective {
            kind,
            provider,
            model,
            effort,
            agent_role,
            query: body,
            topology_node,
            iteration,
            tags,
        }))
    }
}

fn parse_kind(s: &str) -> Option<SessionKind> {
    match s {
        "Story" => Some(SessionKind::Story),
        "Task" => Some(SessionKind::Task),
        "Bug" => Some(SessionKind::Bug),
        "Feature" => Some(SessionKind::Feature),
        "Refactor" => Some(SessionKind::Refactor),
        "Research" => Some(SessionKind::Research),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal() {
        let block = "<docregblock>\n/spawn_child kind=Task\nQUERY:\nimplement X\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.kind, SessionKind::Task);
        assert_eq!(d.query, "implement X");
        assert!(d.provider.is_none());
        assert!(d.model.is_none());
        assert!(d.effort.is_none());
        assert!(d.agent_role.is_none());
        assert!(d.topology_node.is_none());
        assert!(d.iteration.is_none());
        assert!(d.tags.is_none());
    }

    #[test]
    fn parses_with_optional_fields() {
        let block = "<docregblock>\n/spawn_child kind=Story provider=Claude model=claude-opus-4-7 effort=high agent_role=Planner\nQUERY:\nL1\nL2\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.kind, SessionKind::Story);
        assert_eq!(d.provider, Some(rsi_common::types::SessionProvider::Claude));
        assert_eq!(d.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(d.effort.as_deref(), Some("high"));
        assert_eq!(d.agent_role.as_deref(), Some("Planner"));
        assert_eq!(d.query, "L1\nL2");
    }

    #[test]
    fn parses_gemini_provider_alias() {
        let block =
            "<docregblock>\n/spawn_child kind=Task provider=Gemini\nQUERY:\nx\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(
            d.provider,
            Some(rsi_common::types::SessionProvider::Antigravity)
        );
    }

    #[test]
    fn parses_all_leaf_kinds() {
        for (s, expected) in [
            ("Story", SessionKind::Story),
            ("Task", SessionKind::Task),
            ("Bug", SessionKind::Bug),
            ("Feature", SessionKind::Feature),
            ("Refactor", SessionKind::Refactor),
            ("Research", SessionKind::Research),
        ] {
            let block = format!("<docregblock>\n/spawn_child kind={s}\nQUERY:\nx\n</docregblock>");
            let d = SpawnDirective::parse(&block).unwrap().unwrap();
            assert_eq!(d.kind, expected);
        }
    }

    #[test]
    fn returns_none_for_other_directive() {
        let block = "<docregblock>\n/resume_handoff /tmp/x.md\nQUERY:\n\n</docregblock>";
        // Header is not /spawn_child → Ok(None).
        let parsed = SpawnDirective::parse(block).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn rejects_unknown_kind() {
        let block = "<docregblock>\n/spawn_child kind=Sandwich\nQUERY:\nx\n</docregblock>";
        match SpawnDirective::parse(block) {
            Err(SpawnDirectiveParseError::UnknownKind(k)) => assert_eq!(k, "Sandwich"),
            other => panic!("expected UnknownKind, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_kind() {
        let block = "<docregblock>\n/spawn_child model=foo\nQUERY:\nx\n</docregblock>";
        assert!(matches!(
            SpawnDirective::parse(block),
            Err(SpawnDirectiveParseError::MissingKey("kind"))
        ));
    }

    #[test]
    fn rejects_unknown_provider() {
        let block = "<docregblock>\n/spawn_child kind=Task provider=not-a-provider\nQUERY:\nx\n</docregblock>";
        assert!(matches!(
            SpawnDirective::parse(block),
            Err(SpawnDirectiveParseError::UnknownProvider(provider)) if provider == "not-a-provider"
        ));
    }

    #[test]
    fn rejects_missing_query_marker() {
        let block = "<docregblock>\n/spawn_child kind=Task\nbody\n</docregblock>";
        assert!(matches!(
            SpawnDirective::parse(block),
            Err(SpawnDirectiveParseError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_missing_close_tag() {
        let block = "<docregblock>\n/spawn_child kind=Task\nQUERY:\nbody";
        assert!(matches!(
            SpawnDirective::parse(block),
            Err(SpawnDirectiveParseError::Malformed("missing close tag"))
        ));
    }

    #[test]
    fn rejects_missing_open_tag() {
        let block = "/spawn_child kind=Task\nQUERY:\nbody\n</docregblock>";
        assert!(matches!(
            SpawnDirective::parse(block),
            Err(SpawnDirectiveParseError::Malformed("missing open tag"))
        ));
    }

    #[test]
    fn ignores_unknown_header_keys() {
        let block =
            "<docregblock>\n/spawn_child kind=Task hyperflux=42 model=m\nQUERY:\nx\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.kind, SessionKind::Task);
        assert_eq!(d.model.as_deref(), Some("m"));
    }

    #[test]
    fn preserves_multiline_body_with_blank_lines() {
        let block =
            "<docregblock>\n/spawn_child kind=Task\nQUERY:\nLine 1\n\nLine 3\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.query, "Line 1\n\nLine 3");
    }

    #[test]
    fn query_body_excludes_docregblock_wrapper_and_header() {
        let block = "<docregblock>\n/spawn_child kind=Task model=gpt-5-codex\nQUERY:\nOnly this body becomes the child user prompt.\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.query, "Only this body becomes the child user prompt.");
        assert!(!d.query.contains("<docregblock>"));
        assert!(!d.query.contains("/spawn_child"));
        assert!(!d.query.contains("QUERY:"));
        assert!(!d.query.contains("</docregblock>"));
    }

    #[test]
    fn regex_extracts_block_from_surrounding_text() {
        use crate::session::types::SPAWN_DIRECTIVE_RE;
        let text = "Before noise\n<docregblock>\n/spawn_child kind=Task\nQUERY:\nimplement Y\n</docregblock>\nAfter noise";
        let blocks: Vec<&str> = SPAWN_DIRECTIVE_RE
            .captures_iter(text)
            .map(|c| c.get(0).unwrap().as_str())
            .collect();
        assert_eq!(blocks.len(), 1);
        let d = SpawnDirective::parse(blocks[0]).unwrap().unwrap();
        assert_eq!(d.kind, SessionKind::Task);
        assert_eq!(d.query, "implement Y");
    }

    #[test]
    fn regex_finds_multiple_blocks() {
        use crate::session::types::SPAWN_DIRECTIVE_RE;
        let text = "<docregblock>\n/spawn_child kind=Task\nQUERY:\nA\n</docregblock>\nstuff\n<docregblock>\n/spawn_child kind=Story\nQUERY:\nB\n</docregblock>";
        let blocks: Vec<_> = SPAWN_DIRECTIVE_RE.captures_iter(text).collect();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn regex_ignores_non_anchored_directive() {
        use crate::session::types::SPAWN_DIRECTIVE_RE;
        // Indented (not at line start) — must NOT match.
        let text = "    <docregblock>\n/spawn_child kind=Task\nQUERY:\nx\n</docregblock>";
        let count = SPAWN_DIRECTIVE_RE.captures_iter(text).count();
        assert_eq!(count, 0);
    }

    // ─── P1.7 new-key tests ──────────────────────────────────────────────────

    #[test]
    fn parses_topology_node_iteration_tags() {
        let block = "<docregblock>\n/spawn_child kind=Research topology_node=plan_v1 iteration=2 tags=alpha,beta\nQUERY:\nresearch task\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.kind, SessionKind::Research);
        assert_eq!(d.topology_node.as_deref(), Some("plan_v1"));
        assert_eq!(d.iteration, Some(2));
        assert_eq!(
            d.tags.as_deref(),
            Some(vec!["alpha".to_string(), "beta".to_string()].as_slice())
        );
    }

    #[test]
    fn rejects_empty_topology_node() {
        let block =
            "<docregblock>\n/spawn_child kind=Task topology_node=\nQUERY:\nx\n</docregblock>";
        assert!(matches!(
            SpawnDirective::parse(block),
            Err(SpawnDirectiveParseError::Malformed("invalid topology_node"))
        ));
    }

    #[test]
    fn rejects_non_numeric_iteration() {
        let block =
            "<docregblock>\n/spawn_child kind=Task iteration=abc\nQUERY:\nx\n</docregblock>";
        assert!(matches!(
            SpawnDirective::parse(block),
            Err(SpawnDirectiveParseError::Malformed("invalid iteration"))
        ));
    }

    #[test]
    fn rejects_malformed_tags() {
        // Uppercase + special char fails normalize_tag.
        let block = "<docregblock>\n/spawn_child kind=Task tags=BAD!\nQUERY:\nx\n</docregblock>";
        assert!(matches!(
            SpawnDirective::parse(block),
            Err(SpawnDirectiveParseError::Malformed("invalid tags"))
        ));
    }

    #[test]
    fn rejects_empty_tags_csv() {
        // Empty value after `tags=` is rejected.
        let block = "<docregblock>\n/spawn_child kind=Task tags=\nQUERY:\nx\n</docregblock>";
        assert!(matches!(
            SpawnDirective::parse(block),
            Err(SpawnDirectiveParseError::Malformed(
                "invalid tags: must not be empty"
            ))
        ));
    }

    #[test]
    fn ignores_unknown_header_keys_with_hyperflux() {
        // Extend the existing forward-compat check with an extra unknown key (hyperflux)
        // plus all three P1.7 keys absent — proves neither set trips up the parser.
        let block = "<docregblock>\n/spawn_child kind=Task hyperflux=42 frob=99 model=m\nQUERY:\nx\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.kind, SessionKind::Task);
        assert_eq!(d.model.as_deref(), Some("m"));
        assert!(d.topology_node.is_none());
        assert!(d.iteration.is_none());
        assert!(d.tags.is_none());
    }

    #[test]
    fn new_keys_absent_parses_same_as_pre_p17() {
        // A directive identical to the pre-P1.7 baseline parses cleanly
        // with all three new keys defaulting to None.
        let block = "<docregblock>\n/spawn_child kind=Story model=claude-opus-4-7 effort=high\nQUERY:\nL1\nL2\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.kind, SessionKind::Story);
        assert!(d.provider.is_none());
        assert_eq!(d.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(d.effort.as_deref(), Some("high"));
        assert_eq!(d.query, "L1\nL2");
        assert!(
            d.topology_node.is_none(),
            "topology_node must default to None"
        );
        assert!(d.iteration.is_none(), "iteration must default to None");
        assert!(d.tags.is_none(), "tags must default to None");
    }

    #[test]
    fn parses_inline_and_same_line_layouts() {
        use crate::session::types::SPAWN_DIRECTIVE_RE;

        // Single-line layout
        let block = "<docregblock> /spawn_child kind=Task model=gemini-3.5-flash-high QUERY: Run cargo check. </docregblock>";
        let blocks: Vec<&str> = SPAWN_DIRECTIVE_RE
            .captures_iter(block)
            .map(|c| c.get(0).unwrap().as_str())
            .collect();
        assert_eq!(blocks, vec![block]);
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.kind, SessionKind::Task);
        assert_eq!(d.model.as_deref(), Some("gemini-3.5-flash-high"));
        assert_eq!(d.query, "Run cargo check.");

        // Inline tag start layout
        let block =
            "<docregblock>/spawn_child kind=Feature\nQUERY:\nPIPELINE MODE: true\n</docregblock>";
        let d = SpawnDirective::parse(block).unwrap().unwrap();
        assert_eq!(d.kind, SessionKind::Feature);
        assert_eq!(d.query, "PIPELINE MODE: true");
    }

    #[test]
    fn halt_regex_accepts_inline_and_same_line_layouts() {
        use crate::session::types::HALT_DIRECTIVE_RE;

        assert!(HALT_DIRECTIVE_RE.is_match("<docregblock>/halt</docregblock>"));
        assert!(HALT_DIRECTIVE_RE.is_match("<docregblock> /halt </docregblock>"));
        assert!(HALT_DIRECTIVE_RE.is_match("<docregblock>\n/halt\n</docregblock>"));
    }
}
