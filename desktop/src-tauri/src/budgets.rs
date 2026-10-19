//! Settings › Safety & Spend › Budgets and the budget policy form (gap sweep
//! G4, hub #3000; crates/rsi/src/overlay/budget_policy_form.rs and
//! action_handler/daemon_config.rs `submit_model_budget_policy`).
//!
//! `budget_policies` reads `GetModelControlStatus` (the TUI's
//! `cached_model_control_status`). A save or a delete re-reads it, checks the
//! row being edited is still the one the operator saw, and sends one
//! `UpdateModelControlPolicy` with the current breaker mode unchanged,
//! `interrupt_active: false` and `replace_policies: true` carrying the full
//! list, exactly as the TUI does. The policy is built with the TUI's rules: a
//! scope id unless Global (whose id is "global"), the purpose parsed by serde,
//! digit-only limits, a ratio in (0, 1], and at least one counted limit on the
//! merged policy (hidden fields of the original are kept).

use rsi_common::model_control::{
    BudgetScopeKind, ModelBudgetPolicy, ModelControlMode, ModelControlStatusReport,
    ModelInvocationPurpose, ModelTier,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward};

pub const METHOD_STATUS: &str = "GetModelControlStatus";
pub const METHOD_UPDATE: &str = "UpdateModelControlPolicy";

/// The form's scope kinds in its cycle order (`model_control_budgets::SCOPE_KINDS`).
pub const SCOPE_KINDS: &[&str] = &[
    "global",
    "provider",
    "project",
    "session",
    "tree",
    "workflow",
    "subsystem",
    "retry",
    "scheduled_job",
    "issue_tracker",
    "recursive_graph",
    "operator",
];

/// The form's model tiers (`MODEL_TIERS`); "any" is no tier.
pub const MODEL_TIERS: &[&str] = &["any", "local", "standard", "premium"];

/// The form's text fields, as typed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetForm {
    pub scope_kind: String,
    #[serde(default)]
    pub scope_id: String,
    #[serde(default)]
    pub purpose: String,
    pub model_tier: String,
    #[serde(default)]
    pub max_total_tokens: String,
    #[serde(default)]
    pub max_concurrency: String,
    #[serde(default)]
    pub max_calls_per_window: String,
    #[serde(default)]
    pub rate_window_seconds: String,
    #[serde(default)]
    pub alert_threshold_ratio: String,
}

fn enum_of<T: serde::de::DeserializeOwned>(raw: &str) -> Option<T> {
    serde_json::from_value(Value::String(raw.to_string())).ok()
}

fn optional_u64(raw: &str, field: &str) -> Result<Option<u64>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if !t.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("{field} must be a whole number"));
    }
    t.parse::<u64>()
        .map(Some)
        .map_err(|_| format!("{field} is too large"))
}

fn optional_u32(raw: &str, field: &str) -> Result<Option<u32>, String> {
    optional_u64(raw, field)?
        .map(|v| u32::try_from(v).map_err(|_| format!("{field} is too large")))
        .transpose()
}

/// A policy with every field unset (`all_none_policy`).
pub fn empty_policy() -> ModelBudgetPolicy {
    ModelBudgetPolicy {
        scope_kind: BudgetScopeKind::Global,
        scope_id: None,
        purpose: None,
        model_tier: None,
        effort: None,
        ceiling_model_tier: None,
        ceiling_effort: None,
        max_calls: None,
        max_total_tokens: None,
        max_input_tokens: None,
        max_output_tokens: None,
        max_cache_creation_tokens: None,
        max_cache_read_tokens: None,
        max_reasoning_tokens: None,
        max_embedding_inputs: None,
        max_wall_time_ms: None,
        max_concurrency: None,
        max_retries: None,
        max_calls_per_window: None,
        rate_window_seconds: None,
        alert_threshold_ratio: None,
    }
}

/// `submit_budget_policy_form`: the form over the original (or an empty) policy.
pub fn build_policy(
    form: &BudgetForm,
    original: Option<ModelBudgetPolicy>,
) -> Result<ModelBudgetPolicy, String> {
    let scope_kind: BudgetScopeKind = enum_of(form.scope_kind.trim())
        .filter(|_| SCOPE_KINDS.contains(&form.scope_kind.trim()))
        .ok_or_else(|| format!("unknown scope kind {}", form.scope_kind))?;
    let model_tier: Option<ModelTier> = match form.model_tier.trim() {
        "any" => None,
        t if MODEL_TIERS.contains(&t) => Some(enum_of(t).ok_or("unknown model tier")?),
        t => return Err(format!("unknown model tier {t}")),
    };
    let scope_id = form.scope_id.trim().to_string();
    if scope_kind != BudgetScopeKind::Global && scope_id.is_empty() {
        return Err(format!(
            "Budget policy requires scope_id for {}",
            form.scope_kind.trim()
        ));
    }
    let purpose_raw = form.purpose.trim();
    let purpose: Option<ModelInvocationPurpose> = if purpose_raw.is_empty() {
        None
    } else {
        Some(enum_of(purpose_raw).ok_or_else(|| format!("Invalid purpose: {purpose_raw}"))?)
    };
    let ratio = form.alert_threshold_ratio.trim();
    let alert_threshold_ratio = if ratio.is_empty() {
        None
    } else {
        match ratio.parse::<f64>() {
            Ok(v) if v > 0.0 && v <= 1.0 => Some(v),
            _ => return Err("alert_threshold_ratio must be in (0, 1]".into()),
        }
    };
    let mut p = original.unwrap_or_else(empty_policy);
    p.scope_kind = scope_kind;
    p.scope_id = Some(if scope_kind == BudgetScopeKind::Global {
        "global".to_string()
    } else {
        scope_id
    });
    p.purpose = purpose;
    p.model_tier = model_tier;
    p.max_total_tokens = optional_u64(&form.max_total_tokens, "max_total_tokens")?;
    p.max_concurrency = optional_u32(&form.max_concurrency, "max_concurrency")?;
    p.max_calls_per_window = optional_u64(&form.max_calls_per_window, "max_calls_per_window")?;
    p.rate_window_seconds = optional_u64(&form.rate_window_seconds, "rate_window_seconds")?;
    p.alert_threshold_ratio = alert_threshold_ratio;
    // rsid's validate_model_budget_policy "at least one limit" union, exactly.
    let has_limit = p.max_calls.is_some()
        || p.max_total_tokens.is_some()
        || p.max_input_tokens.is_some()
        || p.max_output_tokens.is_some()
        || p.max_embedding_inputs.is_some()
        || p.max_wall_time_ms.is_some()
        || p.max_concurrency.is_some()
        || p.max_retries.is_some();
    if !has_limit {
        return Err("Budget policy must set at least one limit (tokens, concurrency, calls, retries, or wall time)".into());
    }
    Ok(p)
}

/// The row the operator is editing or deleting must still be at `index`.
pub fn still_there(
    policies: &[ModelBudgetPolicy],
    index: usize,
    seen: &Value,
) -> Result<(), String> {
    let now = policies
        .get(index)
        .ok_or("That policy is gone; refresh (R)")?;
    if serde_json::to_value(now).map_err(|e| e.to_string())? != *seen {
        return Err("That policy changed since you opened it; refresh (R)".into());
    }
    Ok(())
}

pub fn update_params(mode: ModelControlMode, policies: &[ModelBudgetPolicy]) -> Value {
    json!({
        "mode": mode,
        "interrupt_active": false,
        "replace_policies": true,
        "policies": policies,
    })
}

async fn status(state: &AppState) -> Result<ModelControlStatusReport, String> {
    let v = forward(state, METHOD_STATUS, json!({ "recent_limit": 0 })).await?;
    serde_json::from_value(v).map_err(|e| format!("unexpected model-control status: {e}"))
}

#[tauri::command]
pub async fn budget_policies(state: State<'_, AppState>) -> CmdResult {
    let s = status(&state).await?;
    Ok(json!({ "mode": s.mode, "policies": s.policies }))
}

#[tauri::command]
pub async fn budget_policy_save(
    state: State<'_, AppState>,
    form: BudgetForm,
    editing: Option<usize>,
    seen: Option<Value>,
) -> CmdResult {
    let s = status(&state).await?;
    let mut policies = s.policies;
    let original = match (editing, &seen) {
        (Some(i), Some(seen)) => {
            still_there(&policies, i, seen)?;
            Some(policies[i].clone())
        }
        (Some(_), None) => return Err("an edit needs the policy it replaces".into()),
        (None, _) => None,
    };
    let policy = build_policy(&form, original)?;
    match editing {
        Some(i) => policies[i] = policy,
        None => policies.push(policy),
    }
    forward(&state, METHOD_UPDATE, update_params(s.mode, &policies)).await
}

#[tauri::command]
pub async fn budget_policy_delete(
    state: State<'_, AppState>,
    index: usize,
    seen: Value,
) -> CmdResult {
    let s = status(&state).await?;
    let mut policies = s.policies;
    still_there(&policies, index, &seen)?;
    policies.remove(index);
    forward(&state, METHOD_UPDATE, update_params(s.mode, &policies)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> BudgetForm {
        BudgetForm {
            scope_kind: "project".into(),
            scope_id: " p1 ".into(),
            purpose: String::new(),
            model_tier: "premium".into(),
            max_total_tokens: "1000".into(),
            max_concurrency: String::new(),
            max_calls_per_window: String::new(),
            rate_window_seconds: String::new(),
            alert_threshold_ratio: "0.8".into(),
        }
    }

    #[test]
    fn a_form_builds_the_tui_policy() {
        let p = build_policy(&form(), None).unwrap();
        assert_eq!(p.scope_kind, BudgetScopeKind::Project);
        assert_eq!(p.scope_id.as_deref(), Some("p1"));
        assert_eq!(p.model_tier, Some(ModelTier::Premium));
        assert_eq!(p.max_total_tokens, Some(1000));
        assert_eq!(p.alert_threshold_ratio, Some(0.8));
        let mut g = form();
        g.scope_kind = "global".into();
        g.scope_id = String::new();
        g.model_tier = "any".into();
        let p = build_policy(&g, None).unwrap();
        assert_eq!(p.scope_id.as_deref(), Some("global"));
        assert_eq!(p.model_tier, None);
    }

    #[test]
    fn the_form_refuses_what_the_tui_refuses() {
        let mut f = form();
        f.scope_id = " ".into();
        assert!(
            build_policy(&f, None)
                .unwrap_err()
                .contains("requires scope_id")
        );
        let mut f = form();
        f.max_total_tokens = "12a".into();
        assert!(build_policy(&f, None).is_err());
        let mut f = form();
        f.alert_threshold_ratio = "1.5".into();
        assert!(build_policy(&f, None).is_err());
        let mut f = form();
        f.max_total_tokens = String::new();
        assert!(
            build_policy(&f, None)
                .unwrap_err()
                .contains("at least one limit")
        );
        let mut f = form();
        f.purpose = "not_a_purpose".into();
        assert!(
            build_policy(&f, None)
                .unwrap_err()
                .contains("Invalid purpose")
        );
        let mut f = form();
        f.scope_kind = "galaxy".into();
        assert!(build_policy(&f, None).is_err());
    }

    #[test]
    fn hidden_fields_of_the_original_count_and_are_kept() {
        let mut original = empty_policy();
        original.max_retries = Some(3);
        original.effort = Some("high".into());
        let mut f = form();
        f.max_total_tokens = String::new();
        let p = build_policy(&f, Some(original)).unwrap();
        assert_eq!(p.max_retries, Some(3), "a hidden limit satisfies the rule");
        assert_eq!(p.effort.as_deref(), Some("high"));
    }

    #[test]
    fn edits_are_fenced_on_the_row_the_operator_saw() {
        let a = build_policy(&form(), None).unwrap();
        let list = vec![a.clone()];
        let seen = serde_json::to_value(&a).unwrap();
        assert!(still_there(&list, 0, &seen).is_ok());
        assert!(still_there(&list, 1, &seen).is_err());
        let mut b = a.clone();
        b.max_total_tokens = Some(5);
        assert!(still_there(&[b], 0, &seen).is_err());
        let params = update_params(ModelControlMode::Normal, &list);
        assert_eq!(params["replace_policies"], true);
        assert_eq!(params["interrupt_active"], false);
        assert_eq!(params["mode"], "normal");
        assert_eq!(params["policies"].as_array().unwrap().len(), 1);
    }
}
