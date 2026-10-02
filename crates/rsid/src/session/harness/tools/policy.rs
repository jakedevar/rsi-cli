//! Runtime enforcement of the per-session Harness tool policy (#792).
//!
//! One `ToolPolicyRuntime` is shared by the registry (native tools) and the
//! agent loop (provider-hosted web tools), so the catalog build and the
//! execution gate consult the same rules and the same budget counters.

use crate::session::harness::types::ToolResult;
use rsi_common::harness_tool_policy::{
    HOSTED_SEARCH_COST_USD_MICROS, HarnessToolPolicy, TOOL_BUDGET_EXHAUSTED, TOOL_POLICY_DENIED,
    WEB_FETCH_TOOL, WEB_SEARCH_TOOL, WebAccessMode,
};
use std::sync::Mutex;

/// A typed denial or budget exhaustion; always surfaces as a settled
/// tool-error row, never as a session failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError {
    pub code: &'static str,
    pub message: String,
}

impl PolicyError {
    fn denied(message: impl Into<String>) -> Self {
        Self {
            code: TOOL_POLICY_DENIED,
            message: message.into(),
        }
    }

    fn exhausted(message: impl Into<String>) -> Self {
        Self {
            code: TOOL_BUDGET_EXHAUSTED,
            message: message.into(),
        }
    }

    /// `code: message`, the text after the `Error: ` prefix of the row.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}: {}", self.code, self.message)
    }

    #[must_use]
    pub fn into_tool_result(self) -> ToolResult {
        ToolResult {
            success: false,
            output: String::new(),
            error_msg: Some(self.text()),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PolicyUsage {
    pub search_calls: u32,
    pub fetch_calls: u32,
    pub result_bytes: u64,
    pub web_cost_usd_micros: u64,
}

#[derive(Debug)]
pub struct ToolPolicyRuntime {
    policy: HarnessToolPolicy,
    usage: Mutex<PolicyUsage>,
}

fn is_web_tool(name: &str) -> bool {
    name == WEB_SEARCH_TOOL || name == WEB_FETCH_TOOL
}

fn call_cost(name: &str) -> u64 {
    if name == WEB_SEARCH_TOOL {
        HOSTED_SEARCH_COST_USD_MICROS
    } else {
        0
    }
}

impl ToolPolicyRuntime {
    #[must_use]
    pub fn new(policy: HarnessToolPolicy) -> Self {
        Self {
            policy,
            usage: Mutex::new(PolicyUsage::default()),
        }
    }

    #[must_use]
    pub fn policy(&self) -> &HarnessToolPolicy {
        &self.policy
    }

    /// The egress policy network-capable tools consult through `ToolContext`
    /// (#774). Unset resolves to the fail-closed `deny_private` default.
    #[must_use]
    pub fn egress_policy(&self) -> rsi_common::egress_policy::EgressPolicy {
        rsi_common::egress_policy::EgressPolicy::for_mode(self.policy.egress_mode())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    #[must_use]
    pub fn usage(&self) -> PolicyUsage {
        *self.lock()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PolicyUsage> {
        self.usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Static rules only: enablement sets and `web_access`. The catalog build
    /// and the execution gate both start here.
    ///
    /// # Errors
    /// `tool_policy_denied` naming the rule that refused the tool.
    pub fn tool_permitted(&self, name: &str, hosted: bool) -> Result<(), PolicyError> {
        if self.policy.denied_tools.iter().any(|denied| denied == name) {
            return Err(PolicyError::denied(format!(
                "{name} is denied by this session's tool policy"
            )));
        }
        if let Some(enabled) = &self.policy.enabled_tools
            && !enabled.iter().any(|allowed| allowed == name)
        {
            return Err(PolicyError::denied(format!(
                "{name} is not in this session's enabled tool set"
            )));
        }
        if !is_web_tool(name) {
            return Ok(());
        }
        match self.policy.web_mode() {
            WebAccessMode::Enabled => Ok(()),
            WebAccessMode::Disabled => Err(PolicyError::denied(format!(
                "{name} is unavailable: web_access is disabled for this session"
            ))),
            WebAccessMode::HostedOnly if !hosted => Err(PolicyError::denied(format!(
                "{name} is unavailable: web_access is hosted_only (provider-hosted web tools only)"
            ))),
            WebAccessMode::HostedOnly => Ok(()),
        }
    }

    fn budget_error(&self, name: &str, usage: &PolicyUsage) -> Option<PolicyError> {
        let budgets = &self.policy.budgets;
        if let Some(limit) = budgets.max_result_bytes
            && usage.result_bytes >= limit
        {
            return Some(PolicyError::exhausted(format!(
                "result_bytes budget of {limit} is used up"
            )));
        }
        if name == WEB_SEARCH_TOOL
            && let Some(limit) = budgets.max_search_calls
            && usage.search_calls >= limit
        {
            return Some(PolicyError::exhausted(format!(
                "search_calls budget of {limit} is used up"
            )));
        }
        if name == WEB_FETCH_TOOL
            && let Some(limit) = budgets.max_fetch_calls
            && usage.fetch_calls >= limit
        {
            return Some(PolicyError::exhausted(format!(
                "fetch_calls budget of {limit} is used up"
            )));
        }
        if let Some(limit) = budgets.max_web_cost_usd_micros
            && is_web_tool(name)
            && usage.web_cost_usd_micros.saturating_add(call_cost(name)) > limit
        {
            return Some(PolicyError::exhausted(format!(
                "web cost budget of {limit} micro-USD is used up"
            )));
        }
        None
    }

    /// Admit one call and, when admitted, charge its counters.
    ///
    /// # Errors
    /// `tool_policy_denied` or `tool_budget_exhausted`.
    pub fn admit_call(&self, name: &str, hosted: bool) -> Result<(), PolicyError> {
        self.tool_permitted(name, hosted)?;
        let mut usage = self.lock();
        if let Some(error) = self.budget_error(name, &usage) {
            return Err(error);
        }
        if name == WEB_SEARCH_TOOL {
            usage.search_calls = usage.search_calls.saturating_add(1);
        } else if name == WEB_FETCH_TOOL {
            usage.fetch_calls = usage.fetch_calls.saturating_add(1);
        }
        if is_web_tool(name) {
            usage.web_cost_usd_micros = usage.web_cost_usd_micros.saturating_add(call_cost(name));
        }
        Ok(())
    }

    pub fn record_result_bytes(&self, bytes: usize) {
        let mut usage = self.lock();
        usage.result_bytes = usage.result_bytes.saturating_add(bytes as u64);
    }

    /// Whether a hosted web spec may be sent on the next request: permitted,
    /// and its own budget not yet used up (nothing is charged here).
    #[must_use]
    pub fn hosted_spec_allowed(&self, name: &str) -> bool {
        if self.tool_permitted(name, true).is_err() {
            return false;
        }
        let usage = self.lock();
        self.budget_error(name, &usage).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::harness_tool_policy::ToolBudgets;

    fn runtime(policy: HarnessToolPolicy) -> ToolPolicyRuntime {
        ToolPolicyRuntime::new(policy)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn disabled_web_refuses_hosted_and_local_web_tools() {
        let rt = runtime(HarnessToolPolicy {
            web_access: Some(WebAccessMode::Disabled),
            ..HarnessToolPolicy::default()
        });
        for name in [WEB_SEARCH_TOOL, WEB_FETCH_TOOL] {
            for hosted in [true, false] {
                let error = rt.admit_call(name, hosted).unwrap_err();
                assert_eq!(error.code, TOOL_POLICY_DENIED);
            }
            assert!(!rt.hosted_spec_allowed(name));
        }
        assert!(rt.admit_call("read_file", false).is_ok());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn hosted_only_admits_hosted_web_but_not_a_local_web_tool() {
        let rt = runtime(HarnessToolPolicy {
            web_access: Some(WebAccessMode::HostedOnly),
            ..HarnessToolPolicy::default()
        });
        assert!(rt.hosted_spec_allowed(WEB_SEARCH_TOOL));
        assert!(rt.admit_call(WEB_SEARCH_TOOL, true).is_ok());
        assert_eq!(
            rt.admit_call(WEB_FETCH_TOOL, false).unwrap_err().code,
            TOOL_POLICY_DENIED
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn enabled_and_denied_sets_gate_every_tool_and_deny_wins() {
        let rt = runtime(HarnessToolPolicy {
            enabled_tools: Some(vec![
                "read_file".into(),
                "shell".into(),
                WEB_SEARCH_TOOL.into(),
            ]),
            denied_tools: vec!["shell".into()],
            ..HarnessToolPolicy::default()
        });
        assert!(rt.admit_call("read_file", false).is_ok());
        assert_eq!(
            rt.admit_call("shell", false).unwrap_err().code,
            TOOL_POLICY_DENIED
        );
        assert_eq!(
            rt.admit_call("write_file", false).unwrap_err().code,
            TOOL_POLICY_DENIED
        );
        assert!(rt.hosted_spec_allowed(WEB_SEARCH_TOOL));
        assert!(!rt.hosted_spec_allowed(WEB_FETCH_TOOL));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn search_and_fetch_budgets_exhaust_with_a_typed_error() {
        let rt = runtime(HarnessToolPolicy {
            budgets: ToolBudgets {
                max_search_calls: Some(2),
                max_fetch_calls: Some(0),
                ..ToolBudgets::default()
            },
            ..HarnessToolPolicy::default()
        });
        assert!(rt.hosted_spec_allowed(WEB_SEARCH_TOOL));
        assert!(rt.admit_call(WEB_SEARCH_TOOL, true).is_ok());
        assert!(rt.admit_call(WEB_SEARCH_TOOL, true).is_ok());
        assert!(!rt.hosted_spec_allowed(WEB_SEARCH_TOOL));
        let error = rt.admit_call(WEB_SEARCH_TOOL, true).unwrap_err();
        assert_eq!(error.code, TOOL_BUDGET_EXHAUSTED);
        assert!(
            error
                .text()
                .starts_with("tool_budget_exhausted: search_calls")
        );
        assert_eq!(rt.usage().search_calls, 2);
        assert_eq!(
            rt.admit_call(WEB_FETCH_TOOL, true).unwrap_err().code,
            TOOL_BUDGET_EXHAUSTED
        );
        assert!(rt.admit_call("read_file", false).is_ok());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn byte_and_cost_budgets_stop_further_calls_only() {
        let rt = runtime(HarnessToolPolicy {
            budgets: ToolBudgets {
                max_result_bytes: Some(10),
                max_web_cost_usd_micros: Some(HOSTED_SEARCH_COST_USD_MICROS),
                ..ToolBudgets::default()
            },
            ..HarnessToolPolicy::default()
        });
        assert!(rt.admit_call(WEB_SEARCH_TOOL, true).is_ok());
        assert_eq!(
            rt.admit_call(WEB_SEARCH_TOOL, true).unwrap_err().code,
            TOOL_BUDGET_EXHAUSTED
        );
        rt.record_result_bytes(10);
        assert_eq!(
            rt.admit_call("read_file", false).unwrap_err().code,
            TOOL_BUDGET_EXHAUSTED
        );
    }
}
