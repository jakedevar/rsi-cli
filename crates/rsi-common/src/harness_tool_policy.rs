//! Per-session Harness tool policy (#792).
//!
//! Operator-only: the policy is set on `LaunchSessionParams.tool_policy` and
//! through the daemon-wide default settings. No agent verb carries or widens
//! it; a spawned child inherits its emitter's policy unchanged.
//!
//! The policy is enforced twice by rsid: when the tool catalog (including
//! provider-hosted web specs) is built, and again when a call executes.

use crate::egress_policy::EgressMode;
use serde::{Deserialize, Serialize};

pub const WEB_SEARCH_TOOL: &str = "web_search";
pub const WEB_FETCH_TOOL: &str = "web_fetch";
pub const TOOL_POLICY_MAX_NAMES: usize = 128;
pub const TOOL_POLICY_MAX_NAME_BYTES: usize = 64;
/// Estimated cost of one provider-hosted web search: providers bill searches
/// at about 10 USD per 1000 calls. Fetches add only token cost, which the
/// model-call ledger already meters.
pub const HOSTED_SEARCH_COST_USD_MICROS: u64 = 10_000;

/// Stable refusal codes, used as the whole error text (launch validation) or
/// the `Error: <code>: ...` prefix of a settled tool-error row.
pub const TOOL_POLICY_INVALID: &str = "tool_policy_invalid";
pub const TOOL_POLICY_UNSUPPORTED_PROVIDER: &str = "tool_policy_unsupported_provider";
pub const TOOL_POLICY_DENIED: &str = "tool_policy_denied";
pub const TOOL_BUDGET_EXHAUSTED: &str = "tool_budget_exhausted";

/// Providers whose sessions run the Harness loop, the only place a tool policy
/// (stored or daemon default) is enforced.
#[must_use]
pub const fn provider_runs_harness_loop(provider: crate::types::SessionProvider) -> bool {
    matches!(
        provider,
        crate::types::SessionProvider::Harness
            | crate::types::SessionProvider::OpenRouter
            | crate::types::SessionProvider::Bedrock
    )
}

/// Ordered from most to least restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAccessMode {
    Disabled,
    HostedOnly,
    Enabled,
}

impl WebAccessMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::HostedOnly => "hosted_only",
            Self::Disabled => "disabled",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "enabled" => Self::Enabled,
            "hosted_only" => Self::HostedOnly,
            "disabled" => Self::Disabled,
            _ => return None,
        })
    }
}

/// Per-session caps. `None` is unlimited; `Some(0)` allows none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolBudgets {
    #[serde(default)]
    pub max_search_calls: Option<u32>,
    #[serde(default)]
    pub max_fetch_calls: Option<u32>,
    /// Total bytes of tool output the session may consume.
    #[serde(default)]
    pub max_result_bytes: Option<u64>,
    /// Estimated web cost in micro-USD (1_000_000 = 1 USD).
    #[serde(default)]
    pub max_web_cost_usd_micros: Option<u64>,
}

impl ToolBudgets {
    #[must_use]
    pub const fn is_unlimited(&self) -> bool {
        self.max_search_calls.is_none()
            && self.max_fetch_calls.is_none()
            && self.max_result_bytes.is_none()
            && self.max_web_cost_usd_micros.is_none()
        // A budget is a ceiling: every field only ever narrows.
    }

    fn or(self, defaults: Self) -> Self {
        Self {
            max_search_calls: self.max_search_calls.or(defaults.max_search_calls),
            max_fetch_calls: self.max_fetch_calls.or(defaults.max_fetch_calls),
            max_result_bytes: self.max_result_bytes.or(defaults.max_result_bytes),
            max_web_cost_usd_micros: self
                .max_web_cost_usd_micros
                .or(defaults.max_web_cost_usd_micros),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessToolPolicy {
    /// When set, only these tools (native and hosted) are advertised and run.
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    /// Tools that are never advertised or run; wins over `enabled_tools`.
    #[serde(default)]
    pub denied_tools: Vec<String>,
    /// `None` inherits the daemon default (enabled when that is unset too).
    #[serde(default)]
    pub web_access: Option<WebAccessMode>,
    /// Network egress mode for network-capable tools and the shell (#774).
    /// `None` inherits the daemon default (`deny_private` when unset too).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<EgressMode>,
    /// Server-side context editing (#1097 slice 4): the Anthropic API clears
    /// old tool results in bulk (`clear_tool_uses_20250919`). `None` inherits
    /// the default, which is on for Claude Harness sessions; it is never
    /// applied to other providers. Nothing is edited client-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_editing: Option<bool>,
    #[serde(default)]
    pub budgets: ToolBudgets,
}

impl HarnessToolPolicy {
    /// # Errors
    /// The stable `tool_policy_invalid` code for an unbounded or malformed set.
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid_set = |names: &[String]| {
            names.len() <= TOOL_POLICY_MAX_NAMES
                && names.iter().all(|name| {
                    !name.is_empty()
                        && name.len() <= TOOL_POLICY_MAX_NAME_BYTES
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                })
        };
        if !valid_set(&self.denied_tools)
            || !valid_set(self.enabled_tools.as_deref().unwrap_or(&[]))
        {
            return Err(TOOL_POLICY_INVALID);
        }
        Ok(())
    }

    /// Fill every field this policy leaves unspecified from `defaults`; deny
    /// sets union. Used to layer the daemon defaults under a session policy.
    #[must_use]
    pub fn or_defaults(&self, defaults: &Self) -> Self {
        let mut denied = self.denied_tools.clone();
        for name in &defaults.denied_tools {
            if !denied.contains(name) {
                denied.push(name.clone());
            }
        }
        Self {
            enabled_tools: self
                .enabled_tools
                .clone()
                .or_else(|| defaults.enabled_tools.clone()),
            denied_tools: denied,
            web_access: self.web_access.or(defaults.web_access),
            egress: self.egress.or(defaults.egress),
            context_editing: self.context_editing.or(defaults.context_editing),
            budgets: self.budgets.or(defaults.budgets),
        }
    }

    /// The most restrictive policy: no tools and no web. Used when a stored
    /// policy cannot be read, so a launch never silently drops a restriction.
    #[must_use]
    pub fn fail_closed() -> Self {
        Self {
            enabled_tools: Some(Vec::new()),
            denied_tools: Vec::new(),
            web_access: Some(WebAccessMode::Disabled),
            egress: Some(EgressMode::Offline),
            context_editing: None,
            budgets: ToolBudgets::default(),
        }
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Effective egress mode (`deny_private` when unspecified everywhere).
    #[must_use]
    pub fn egress_mode(&self) -> EgressMode {
        self.egress.unwrap_or_default()
    }

    /// Whether server-side context editing is requested (default on).
    #[must_use]
    pub fn context_editing_enabled(&self) -> bool {
        self.context_editing.unwrap_or(true)
    }

    /// Effective web mode (`enabled` when unspecified everywhere).
    #[must_use]
    pub fn web_mode(&self) -> WebAccessMode {
        self.web_access.unwrap_or(WebAccessMode::Enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_and_order_by_restriction() {
        for mode in [
            WebAccessMode::Enabled,
            WebAccessMode::HostedOnly,
            WebAccessMode::Disabled,
        ] {
            assert_eq!(WebAccessMode::parse(mode.as_str()), Some(mode));
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.as_str());
        }
        assert!(WebAccessMode::Disabled < WebAccessMode::HostedOnly);
        assert!(WebAccessMode::HostedOnly < WebAccessMode::Enabled);
    }

    #[test]
    fn unknown_fields_and_malformed_names_are_refused() {
        assert!(
            serde_json::from_value::<HarnessToolPolicy>(serde_json::json!({"web": "off"})).is_err()
        );
        let mut policy = HarnessToolPolicy::default();
        assert_eq!(policy.validate(), Ok(()));
        policy.denied_tools = vec!["Web Search".into()];
        assert_eq!(policy.validate(), Err(TOOL_POLICY_INVALID));
        policy.denied_tools = vec![String::new()];
        assert_eq!(policy.validate(), Err(TOOL_POLICY_INVALID));
        policy.denied_tools = vec!["shell".into()];
        policy.enabled_tools = Some(vec!["x".repeat(TOOL_POLICY_MAX_NAME_BYTES + 1)]);
        assert_eq!(policy.validate(), Err(TOOL_POLICY_INVALID));
    }

    #[test]
    fn defaults_fill_only_what_the_session_leaves_unset() {
        let defaults = HarnessToolPolicy {
            denied_tools: vec!["shell".into()],
            web_access: Some(WebAccessMode::HostedOnly),
            budgets: ToolBudgets {
                max_search_calls: Some(5),
                max_fetch_calls: Some(5),
                ..ToolBudgets::default()
            },
            ..HarnessToolPolicy::default()
        };
        let session = HarnessToolPolicy {
            denied_tools: vec!["git".into()],
            web_access: Some(WebAccessMode::Disabled),
            budgets: ToolBudgets {
                max_search_calls: Some(1),
                ..ToolBudgets::default()
            },
            ..HarnessToolPolicy::default()
        };
        let merged = session.or_defaults(&defaults);
        assert_eq!(merged.web_access, Some(WebAccessMode::Disabled));
        assert_eq!(merged.budgets.max_search_calls, Some(1));
        assert_eq!(merged.budgets.max_fetch_calls, Some(5));
        assert_eq!(
            merged.denied_tools,
            vec!["git".to_string(), "shell".to_string()]
        );
        assert_eq!(
            HarnessToolPolicy::default().web_mode(),
            WebAccessMode::Enabled
        );
    }

    #[test]
    fn egress_mode_layers_under_the_session_and_fails_closed() {
        let defaults = HarnessToolPolicy {
            egress: Some(EgressMode::Offline),
            ..HarnessToolPolicy::default()
        };
        let unset = HarnessToolPolicy::default();
        assert_eq!(
            unset.or_defaults(&defaults).egress_mode(),
            EgressMode::Offline
        );
        let explicit = HarnessToolPolicy {
            egress: Some(EgressMode::DenyPrivate),
            ..HarnessToolPolicy::default()
        };
        assert_eq!(
            explicit.or_defaults(&defaults).egress_mode(),
            EgressMode::DenyPrivate
        );
        assert_eq!(unset.egress_mode(), EgressMode::DenyPrivate);
        assert_eq!(
            HarnessToolPolicy::fail_closed().egress_mode(),
            EgressMode::Offline
        );
        // An unset field is not serialised, so stored policy rows are unchanged.
        assert!(!serde_json::to_string(&unset).unwrap().contains("egress"));
        let parsed: HarnessToolPolicy =
            serde_json::from_value(serde_json::json!({"egress": "offline"})).unwrap();
        assert_eq!(parsed.egress, Some(EgressMode::Offline));
    }

    #[test]
    fn context_editing_defaults_on_layers_and_stays_out_of_a_default_wire_form() {
        let unset = HarnessToolPolicy::default();
        assert!(unset.context_editing_enabled());
        assert!(
            !serde_json::to_string(&unset)
                .unwrap()
                .contains("context_editing")
        );
        assert!(unset.is_default());

        let off: HarnessToolPolicy =
            serde_json::from_value(serde_json::json!({"context_editing": false})).unwrap();
        assert!(!off.context_editing_enabled());
        assert!(!off.is_default());
        let layered = HarnessToolPolicy::default().or_defaults(&off);
        assert_eq!(layered.context_editing, Some(false));
        let session_on = HarnessToolPolicy {
            context_editing: Some(true),
            ..Default::default()
        };
        assert!(session_on.or_defaults(&off).context_editing_enabled());
    }
}
