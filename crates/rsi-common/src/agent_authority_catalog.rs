//! `AgentGetAuthorityCatalog`: the agent's callable operator's manual.
//!
//! One read-only verb every RSI-managed session may call. The daemon resolves
//! the token-bound caller's current durable authority and answers with the
//! caller's roles, the guidance for those roles, and exactly the controls the
//! caller may use now. It is an advertisement, never a grant: every other
//! verb still runs its own guard, so a stale catalog can only under- or
//! over-describe, never authorize.

use crate::agent_control_schema::{AgentControlVerbV1, agent_control_catalog_v1};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Envelope version of [`AgentAuthorityCatalogV1`].
pub const AGENT_AUTHORITY_CATALOG_SCHEMA_VERSION_V1: u32 = 1;

/// Stable refusal for a malformed request.
pub const AUTHORITY_CATALOG_INVALID_REQUEST: &str = "authority_catalog_invalid_request";
/// Stable refusal for a `verb` that names no catalog control.
pub const AUTHORITY_CATALOG_UNKNOWN_VERB: &str = "authority_catalog_unknown_verb";

/// Prefix Claude Code puts on the `rsi-agent` MCP server's tools.
const CLAUDE_MCP_TOOL_PREFIX: &str = "mcp__rsi-agent__";

/// Longest one-line control description in the summary view, in characters.
pub const SUMMARY_DESCRIPTION_MAX_CHARS: usize = 100;

/// How much of each control's description a whole-manual response carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAuthorityCatalogFieldsV1 {
    /// Roles, guidance and every permitted control by name with a one-line
    /// description. Small enough for a provider to inline (#308).
    #[default]
    Summary,
    /// Each control's full canonical description (tooling; ~40 KB larger for a
    /// manager). The same text `rsi-rpc agent` and MCP `tools/list` show.
    Full,
}

/// Request: `{}` for the summary manual, `{"fields": "full"}` for the full
/// descriptions, or `{"verb": "<name>"}` for just that control's detail
/// (schema, example, refusals) in the compact envelope. Caller identity is
/// transport-bound, never here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGetAuthorityCatalogRequestV1 {
    /// `summary` (default) or `full`. Ignored when `verb` is set.
    #[serde(default)]
    pub fields: Option<AgentAuthorityCatalogFieldsV1>,
    /// An `Agent*` method (`AgentSpawnChild`), a native tool
    /// (`rsi_control_spawn`) or its Claude MCP spelling
    /// (`mcp__rsi-agent__rsi_control_spawn`).
    #[serde(default)]
    pub verb: Option<String>,
}

impl AgentGetAuthorityCatalogRequestV1 {
    /// Resolve the optional `verb` to one closed catalog entry.
    ///
    /// # Errors
    /// [`AUTHORITY_CATALOG_UNKNOWN_VERB`] when `verb` names no control.
    pub fn requested_verb(&self) -> Result<Option<AgentControlVerbV1>, &'static str> {
        let Some(name) = self.verb.as_deref() else {
            return Ok(None);
        };
        let name = name.strip_prefix(CLAUDE_MCP_TOOL_PREFIX).unwrap_or(name);
        agent_control_catalog_v1()
            .iter()
            .find(|descriptor| {
                descriptor.method == name
                    || descriptor
                        .native_tool
                        .is_some_and(|tool| tool.name() == name)
            })
            .map(|descriptor| Some(descriptor.verb))
            .ok_or(AUTHORITY_CATALOG_UNKNOWN_VERB)
    }

    /// # Errors
    /// See [`Self::requested_verb`].
    pub fn validate(&self) -> Result<(), &'static str> {
        self.requested_verb().map(drop)
    }
}

/// One control the caller may use now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAuthorityControlV1 {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tool: Option<String>,
    pub description: String,
}

/// One stable refusal a control returns and the caller's next step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAuthorityRefusalV1 {
    pub code: String,
    pub next_action: String,
}

/// The control named by the request's `verb`: its parameter schema, one
/// minimal valid example request and its stable refusal codes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentAuthorityControlDetailV1 {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tool: Option<String>,
    pub description: String,
    /// Whether the current snapshot lists this control for the caller.
    pub permitted: bool,
    pub parameters: Value,
    /// One minimal valid request for this control (never carries identity).
    #[serde(default)]
    pub example: Value,
    /// Stable refusal codes with their next step. Empty when the control has
    /// no closed typed refusal family yet.
    #[serde(default)]
    pub refusals: Vec<AgentAuthorityRefusalV1>,
}

/// The caller's operator's manual at one authority revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentAuthorityCatalogV1 {
    pub schema_version: u32,
    pub session_id: Uuid,
    /// Changes whenever role, policy, custody or review assignment changes.
    pub authority_revision: String,
    /// Initial role publication has not committed yet; call again shortly.
    pub pending: bool,
    /// Live coordinator policy; absent for workers and single-verb requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator_budget: Option<crate::coordinator_budget::CoordinatorBudgetV1>,
    /// `summary` when control descriptions are one-liners (the default); the
    /// full text is `fields: "full"`, one control's detail is `verb`. Omitted
    /// when the request named a `verb` or asked for `full`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub view: String,
    /// `worker` always; plus `epic_lead`, `manager`, `assigned_reviewer`.
    pub roles: Vec<String>,
    /// Operating rules for exactly these roles (Markdown). Omitted when the
    /// request named a `verb`: the detail response is the compact envelope.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guidance: String,
    /// Controls the caller may use now, in catalog order. Omitted when the
    /// request named a `verb`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<AgentAuthorityControlV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manager_update_variants: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manager_control_actions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manager_prepared_actions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegated_operator_methods: Vec<String>,
    /// #2060: how to call `delegated_operator_methods`. They are not RPC
    /// methods for a session caller (a direct call is refused); present
    /// exactly when the list is.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub delegated_operator_invocation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<AgentAuthorityControlDetailV1>,
}

/// Endings that do not close a sentence.
const ABBREVIATIONS: [&str; 4] = ["e.g.", "i.e.", "etc.", "vs."];

/// The first sentence of a control description, cut at a word boundary to at
/// most [`SUMMARY_DESCRIPTION_MAX_CHARS`] characters (`...` marks a cut).
#[must_use]
pub fn summary_description(description: &str) -> String {
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    // A sentence ends at `. ` past a short prefix, so "e.g. " and "i.e. " stay.
    let sentence_end = flat
        .match_indices(". ")
        .map(|(index, _)| index + 1)
        .find(|end| {
            *end >= 30
                && !ABBREVIATIONS
                    .iter()
                    .any(|abbr| flat[..*end].ends_with(abbr))
        });
    let first = sentence_end.map_or(flat.as_str(), |end| &flat[..end]);
    if first.chars().count() <= SUMMARY_DESCRIPTION_MAX_CHARS {
        return first.to_string();
    }
    let budget = SUMMARY_DESCRIPTION_MAX_CHARS - 3;
    let cut: String = first.chars().take(budget).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}...", cut.trim_end_matches([',', ';', ':', ' ']))
}

impl AgentAuthorityCatalogV1 {
    /// Shorten every control description to its one-line summary and mark the
    /// view. Names, guidance, roles and manager lists are untouched.
    pub fn summarize(&mut self) {
        for control in &mut self.controls {
            control.description = summary_description(&control.description);
        }
        self.view = "summary".to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_description_is_one_bounded_sentence() {
        assert_eq!(
            summary_description("Spawn a child agent session. Caller must lead its Epic."),
            "Spawn a child agent session. Caller must lead its Epic."
        );
        let long = "Read one bounded durable progress snapshot for the caller's child cohort or current manager's live scope. More text follows here.";
        let short = summary_description(long);
        assert!(
            short.chars().count() <= SUMMARY_DESCRIPTION_MAX_CHARS,
            "{short}"
        );
        assert!(short.ends_with("..."), "{short}");
        assert!(short.starts_with("Read one bounded durable progress snapshot"));
        let two = "Write the thing to the store for every caller, e.g. a worker. Then more words.";
        assert_eq!(
            summary_description(two),
            "Write the thing to the store for every caller, e.g. a worker."
        );
        assert_eq!(summary_description("Short."), "Short.");
    }

    #[test]
    fn request_fields_parse_and_reject_unknown_values() {
        let request: AgentGetAuthorityCatalogRequestV1 =
            serde_json::from_value(serde_json::json!({"fields": "full"})).unwrap();
        assert_eq!(request.fields, Some(AgentAuthorityCatalogFieldsV1::Full));
        assert_eq!(
            AgentGetAuthorityCatalogRequestV1::default()
                .fields
                .unwrap_or_default(),
            AgentAuthorityCatalogFieldsV1::Summary
        );
        assert!(
            serde_json::from_value::<AgentGetAuthorityCatalogRequestV1>(
                serde_json::json!({"fields": "everything"})
            )
            .is_err()
        );
    }

    #[test]
    fn verb_accepts_method_native_and_claude_mcp_spellings() {
        for name in [
            "AgentSpawnChild",
            "rsi_control_spawn",
            "mcp__rsi-agent__rsi_control_spawn",
        ] {
            let request = AgentGetAuthorityCatalogRequestV1 {
                verb: Some(name.into()),
                ..Default::default()
            };
            assert_eq!(
                request.requested_verb(),
                Ok(Some(AgentControlVerbV1::SpawnChild)),
                "{name}"
            );
        }
        assert_eq!(
            AgentGetAuthorityCatalogRequestV1::default().requested_verb(),
            Ok(None)
        );
        for name in [
            "GetSession",
            "agentspawnchild",
            "mcp__other__rsi_control_spawn",
        ] {
            let request = AgentGetAuthorityCatalogRequestV1 {
                verb: Some(name.into()),
                ..Default::default()
            };
            assert_eq!(
                request.validate(),
                Err(AUTHORITY_CATALOG_UNKNOWN_VERB),
                "{name}"
            );
        }
    }

    #[test]
    fn request_rejects_unknown_fields() {
        let error = serde_json::from_value::<AgentGetAuthorityCatalogRequestV1>(
            serde_json::json!({"session_id": Uuid::nil()}),
        );
        assert!(error.is_err());
    }
}
