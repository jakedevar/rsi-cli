//! App-server approval response construction (moved down from
//! `codex_app_server`, which re-exports it).

use crate::error::{DaemonError, Result};
use crate::store_support::provider_settings::ApprovalDecision;
use serde_json::{Value, json};

/// Supported approval methods from the installed codex-cli 0.153.4 schemas.
/// Permissions, user input, elicitation and legacy approvals have different
/// response contracts and must stay unresolved until separately implemented.
pub(crate) const APPROVAL_METHODS: &[&str] = &[
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
];

/// Method-bound response construction shared by the direct ProviderSession and
/// detached writer paths. Request IDs remain JSON strings or signed integers;
/// no coercion, missing-ID fallback, legacy enum guess, or persistent grant is
/// introduced by an operator's single-request approve/deny answer.
pub(crate) fn approval_response(
    request_id: &Value,
    method: &str,
    params: &Value,
    decision: ApprovalDecision,
) -> Result<Value> {
    if !APPROVAL_METHODS.contains(&method) {
        return Err(DaemonError::InvalidParam(
            "unsupported_appserver_approval_method".into(),
        ));
    }
    if !(request_id.is_i64() || request_id.is_string()) {
        return Err(DaemonError::InvalidParam(
            "invalid_appserver_approval_request_id".into(),
        ));
    }
    if !["threadId", "turnId", "itemId"]
        .iter()
        .all(|key| params[*key].is_string())
        || !params["startedAtMs"].is_i64()
    {
        return Err(DaemonError::InvalidParam(
            "incomplete_appserver_approval_params".into(),
        ));
    }
    let decision = match (method, decision) {
        (
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval",
            ApprovalDecision::Approve,
        ) => "accept",
        (
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval",
            ApprovalDecision::Deny,
        ) => "decline",
        (
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval",
            ApprovalDecision::ApproveForSession,
        ) => "acceptForSession",
        _ => {
            return Err(DaemonError::InvalidParam(
                "unsupported_appserver_approval_decision".into(),
            ));
        }
    };
    if let Some(available) = params.get("availableDecisions").filter(|v| !v.is_null()) {
        if !available
            .as_array()
            .is_some_and(|values| values.iter().any(|v| v.as_str() == Some(decision)))
        {
            return Err(DaemonError::InvalidParam(
                "appserver_approval_decision_not_available".into(),
            ));
        }
    }
    Ok(json!({"jsonrpc":"2.0", "id":request_id, "result":{"decision":decision}}))
}
