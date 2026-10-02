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

/// Request: `{}` for the whole manual, or `{"verb": "<name>"}` for just that
/// control's detail (schema, example, refusals) in the compact envelope. Caller identity is transport-bound, never here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGetAuthorityCatalogRequestV1 {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<AgentAuthorityControlDetailV1>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verb_accepts_method_native_and_claude_mcp_spellings() {
        for name in [
            "AgentSpawnChild",
            "rsi_control_spawn",
            "mcp__rsi-agent__rsi_control_spawn",
        ] {
            let request = AgentGetAuthorityCatalogRequestV1 {
                verb: Some(name.into()),
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
