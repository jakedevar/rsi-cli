//! "Budgets" settings category — read-only row derivation.
//!
//! Reads over `App::cached_model_control_status.policies`
//! (`rsi_common::model_control::ModelBudgetPolicy`), mirroring
//! `model_control_stats.rs`'s shape. The write side (add/edit/delete) lives
//! in `crate::overlay::budget_policy_form` and
//! `crate::action_handler::daemon_config`; this module owns only the shared
//! `SCOPE_KINDS`/`MODEL_TIERS` display tables and the read/derive helpers so
//! the settings-pane row list and the edit form never drift from each other.

use crate::app::App;
use rsi_common::model_control::{BudgetScopeKind, ModelBudgetPolicy, ModelTier};

/// Display label <-> `BudgetScopeKind`, covering all 12 variants. Shared by
/// the read-only row list (this module) and the edit form's cyclable
/// scope-kind picker (`crate::overlay::budget_policy_form`) so the two never
/// drift from each other.
pub(crate) const SCOPE_KINDS: &[(BudgetScopeKind, &str)] = &[
    (BudgetScopeKind::Global, "global"),
    (BudgetScopeKind::Provider, "provider"),
    (BudgetScopeKind::Project, "project"),
    (BudgetScopeKind::Session, "session"),
    (BudgetScopeKind::Tree, "tree"),
    (BudgetScopeKind::Workflow, "workflow"),
    (BudgetScopeKind::Subsystem, "subsystem"),
    (BudgetScopeKind::Retry, "retry"),
    (BudgetScopeKind::ScheduledJob, "scheduled_job"),
    (BudgetScopeKind::IssueTracker, "issue_tracker"),
    (BudgetScopeKind::RecursiveGraph, "recursive_graph"),
    (BudgetScopeKind::Operator, "operator"),
];

/// Display label <-> `Option<ModelTier>`; `None` = "any tier". Shared with
/// the edit form's cyclable model-tier picker.
pub(crate) const MODEL_TIERS: &[(Option<ModelTier>, &str)] = &[
    (None, "any"),
    (Some(ModelTier::Local), "local"),
    (Some(ModelTier::Standard), "standard"),
    (Some(ModelTier::Premium), "premium"),
];

pub(crate) fn scope_kind_label(kind: BudgetScopeKind) -> &'static str {
    SCOPE_KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or("?", |(_, label)| *label)
}

pub(crate) fn scope_kind_index(kind: BudgetScopeKind) -> usize {
    SCOPE_KINDS
        .iter()
        .position(|(k, _)| *k == kind)
        .unwrap_or(0)
}

pub(crate) fn model_tier_label(tier: Option<ModelTier>) -> &'static str {
    MODEL_TIERS
        .iter()
        .find(|(t, _)| *t == tier)
        .map_or("?", |(_, label)| *label)
}

pub(crate) fn model_tier_index(tier: Option<ModelTier>) -> usize {
    MODEL_TIERS
        .iter()
        .position(|(t, _)| *t == tier)
        .unwrap_or(0)
}

/// One (label, value) row per configured budget policy. Empty list renders a
/// single synthetic row: an empty Budgets list does NOT mean "unlimited" —
/// the daemon enforces hardcoded per-scope defaults whenever no explicit
/// policy exists for a scope (see `crates/rsid/src/store/model_control.rs`).
pub(crate) fn budget_rows(app: &App) -> Vec<(String, String)> {
    let policies: &[ModelBudgetPolicy] = app
        .cached_model_control_status
        .as_ref()
        .map_or(&[], |status| status.policies.as_slice());

    if policies.is_empty() {
        return vec![(
            "(no budget policies)".to_string(),
            "press 'a' to add — hardcoded defaults apply until then".to_string(),
        )];
    }

    policies
        .iter()
        .map(|policy| {
            let label = format!(
                "{}/{}",
                scope_kind_label(policy.scope_kind),
                policy.scope_id.as_deref().unwrap_or("*")
            );
            (label, format_policy_value(policy))
        })
        .collect()
}

fn format_policy_value(policy: &ModelBudgetPolicy) -> String {
    let purpose = policy
        .purpose
        .map_or_else(|| "*".to_string(), |p| p.as_str().to_string());
    let tier = policy.model_tier.map_or_else(
        || "*".to_string(),
        |t| model_tier_label(Some(t)).to_string(),
    );

    let mut parts = vec![purpose, tier];

    if let Some(v) = policy.max_total_tokens {
        parts.push(format!("tok:{v}"));
    }
    if let Some(v) = policy.max_concurrency {
        parts.push(format!("conc:{v}"));
    }
    match (policy.max_calls_per_window, policy.rate_window_seconds) {
        (Some(calls), Some(secs)) => parts.push(format!("{calls}/{secs}s")),
        (Some(calls), None) => parts.push(format!("{calls}/win")),
        (None, Some(secs)) => parts.push(format!("win:{secs}s")),
        (None, None) => {}
    }
    if let Some(v) = policy.alert_threshold_ratio {
        parts.push(format!("alert:{v:.2}"));
    }

    parts.join(" ")
}

pub(crate) fn budget_row_count(app: &App) -> usize {
    budget_rows(app).len()
}

pub(crate) fn budget_row_label_value(app: &App, idx: usize) -> (String, String) {
    budget_rows(app)
        .get(idx)
        .cloned()
        .unwrap_or_else(|| ("?".to_string(), "?".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use rsi_common::model_control::{ModelControlMode, ModelControlStatusReport};
    use std::path::PathBuf;

    fn test_app() -> App {
        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from(
            "/tmp/test-model-control-budgets.sock",
        )))
    }

    fn empty_report() -> ModelControlStatusReport {
        ModelControlStatusReport {
            mode: ModelControlMode::Normal,
            mode_updated_at: Some("2026-07-15T00:00:00Z".to_string()),
            restart_required_fields: Vec::new(),
            circuit_state: "closed".to_string(),
            circuit_reason: "budget-governed".to_string(),
            circuits: Vec::new(),
            policies: Vec::new(),
            active_invocations: Vec::new(),
            recent_invocations: Vec::new(),
            recent_denials: Vec::new(),
            recent_budget_alerts: Vec::new(),
        }
    }

    fn all_none_policy(scope_kind: BudgetScopeKind, scope_id: Option<&str>) -> ModelBudgetPolicy {
        ModelBudgetPolicy {
            scope_kind,
            scope_id: scope_id.map(str::to_string),
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

    #[test]
    fn budget_rows_shows_synthetic_row_when_empty() {
        let mut app = test_app();
        app.cached_model_control_status = Some(empty_report());

        let rows = budget_rows(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "(no budget policies)");
        assert!(rows[0].1.contains("hardcoded defaults apply"));
        assert_eq!(budget_row_count(&app), 1);
    }

    #[test]
    fn budget_rows_renders_all_none_policy_without_panicking() {
        let mut app = test_app();
        let mut report = empty_report();
        report.policies = vec![all_none_policy(BudgetScopeKind::Global, None)];
        app.cached_model_control_status = Some(report);

        let rows = budget_rows(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "global/*");
        assert_eq!(rows[0].1, "* *");
    }

    #[test]
    fn budget_row_count_matches_policy_len_when_populated() {
        let mut app = test_app();
        let mut report = empty_report();
        report.policies = vec![
            all_none_policy(BudgetScopeKind::Session, Some("s-1")),
            all_none_policy(BudgetScopeKind::Provider, Some("Claude")),
        ];
        app.cached_model_control_status = Some(report);

        assert_eq!(budget_row_count(&app), 2);
        let (label, _) = budget_row_label_value(&app, 1);
        assert_eq!(label, "provider/Claude");
    }
}
