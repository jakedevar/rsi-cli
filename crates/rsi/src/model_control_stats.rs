use crate::app::App;
use crate::modalkit_types::LcAction;
use crate::settings::model_control_mode_slug;
use rsi_common::model_control::{
    BudgetScopeKind, ModelBudgetHeadroom, ModelInvocationStatus, ModelInvocationView,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatsRowAction {
    EmergencyStopAll,
    CancelInvocation(uuid::Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatsRow {
    pub label: String,
    pub value: String,
    pub action: Option<StatsRowAction>,
}

pub(crate) fn stats_rows(app: &App) -> Vec<StatsRow> {
    let mut rows = summary_rows(app);
    let Some(control) = app.cached_model_control_status.as_ref() else {
        return rows;
    };

    for (index, circuit) in control.circuits.iter().enumerate() {
        let scope = format_scope(circuit.scope_kind, circuit.scope_id.as_deref());
        let error = circuit
            .error_class
            .as_deref()
            .map(|error| format!(" err:{error}"))
            .unwrap_or_default();
        rows.push(StatsRow {
            label: format!("Circuit {}", index + 1),
            value: truncate_value(&format!(
                "{} {} {}{} src:{}",
                scope, circuit.state, circuit.reason, error, circuit.source
            )),
            action: None,
        });
    }

    append_invocation_rows(&mut rows, "Active", &control.active_invocations);
    append_invocation_rows(&mut rows, "Recent", &control.recent_invocations);
    append_invocation_rows(&mut rows, "Denied", &control.recent_denials);

    for (index, alert) in control.recent_budget_alerts.iter().enumerate() {
        rows.push(StatsRow {
            label: format!("Alert {}", index + 1),
            value: truncate_value(&format!(
                "{} rem:{} lim:{} thr:{} {}",
                alert.metric,
                alert.remaining,
                alert.limit,
                alert.threshold,
                format_scope(alert.scope_kind, alert.scope_id.as_deref())
            )),
            action: None,
        });
    }

    rows
}

pub(crate) fn stats_row_count(app: &App) -> usize {
    stats_rows(app).len()
}

pub(crate) fn stats_row_label_value(app: &App, idx: usize) -> (String, String) {
    stats_rows(app)
        .get(idx)
        .map(|row| (row.label.clone(), row.value.clone()))
        .unwrap_or_else(|| ("?".to_string(), "?".to_string()))
}

pub(crate) fn stats_row_action(app: &App, idx: usize) -> Option<LcAction> {
    match stats_rows(app).get(idx).and_then(|row| row.action.clone()) {
        Some(StatsRowAction::EmergencyStopAll) => Some(LcAction::EmergencyStopAll),
        Some(StatsRowAction::CancelInvocation(invocation_id)) => {
            Some(LcAction::CancelModelInvocation(invocation_id))
        }
        None => None,
    }
}

fn summary_rows(app: &App) -> Vec<StatsRow> {
    let stats = app.cached_usage_stats.as_ref();
    let control = app.cached_model_control_status.as_ref();
    let token_max = stats
        .map(|stats| {
            stats
                .total_input_tokens
                .max(stats.total_output_tokens)
                .max(stats.total_cache_creation_tokens)
                .max(stats.total_cache_read_tokens)
        })
        .unwrap_or(0);
    const BAR_WIDTH: usize = 10;

    vec![
        StatsRow {
            label: "Chats".to_string(),
            value: stats
                .map(|stats| stats.lifetime_chats.to_string())
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Spend".to_string(),
            value: stats
                .map(|stats| format!("${:.2}", stats.total_cost_usd))
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Input tokens".to_string(),
            value: stats
                .map(|stats| {
                    format!(
                        "{}  {}",
                        compact_count(stats.total_input_tokens),
                        usage_bar(stats.total_input_tokens, token_max, BAR_WIDTH)
                    )
                })
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Output tokens".to_string(),
            value: stats
                .map(|stats| {
                    format!(
                        "{}  {}",
                        compact_count(stats.total_output_tokens),
                        usage_bar(stats.total_output_tokens, token_max, BAR_WIDTH)
                    )
                })
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Cache create".to_string(),
            value: stats
                .map(|stats| {
                    format!(
                        "{}  {}",
                        compact_count(stats.total_cache_creation_tokens),
                        usage_bar(stats.total_cache_creation_tokens, token_max, BAR_WIDTH)
                    )
                })
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Cache read".to_string(),
            value: stats
                .map(|stats| {
                    format!(
                        "{}  {}",
                        compact_count(stats.total_cache_read_tokens),
                        usage_bar(stats.total_cache_read_tokens, token_max, BAR_WIDTH)
                    )
                })
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Work time".to_string(),
            value: stats
                .map(|stats| crate::ui::session::format_work_time_ms(stats.total_work_time_ms))
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Per-model".to_string(),
            value: stats
                .map(|stats| per_model_summary(&stats.per_model))
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Control mode".to_string(),
            value: control
                .map(|status| model_control_mode_slug(status.mode).to_string())
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Circuit".to_string(),
            value: control
                .map(|status| format!("{} {}", status.circuit_state, status.circuit_reason))
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Policies".to_string(),
            value: control
                .map(|status| format!("{} configured", status.policies.len()))
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Active".to_string(),
            value: control
                .map(|status| format!("{} live", status.active_invocations.len()))
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Recent".to_string(),
            value: control
                .map(|status| format!("{} tracked", status.recent_invocations.len()))
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Denials".to_string(),
            value: control
                .map(|status| format!("{} recent", status.recent_denials.len()))
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Alerts".to_string(),
            value: control
                .map(|status| format!("{} recent", status.recent_budget_alerts.len()))
                .unwrap_or_else(|| "(loading…)".to_string()),
            action: None,
        },
        StatsRow {
            label: "Stop now".to_string(),
            value: control
                .map(|status| format!("Enter to stop all ({})", status.circuit_state))
                .unwrap_or_else(|| "Enter to stop all".to_string()),
            action: Some(StatsRowAction::EmergencyStopAll),
        },
    ]
}

fn append_invocation_rows(
    rows: &mut Vec<StatsRow>,
    prefix: &str,
    invocations: &[ModelInvocationView],
) {
    for (index, invocation) in invocations.iter().enumerate() {
        rows.push(StatsRow {
            label: format!("{} {}", prefix, index + 1),
            value: truncate_value(&format!(
                "{} {} {}",
                status_label(invocation.record.status),
                invocation.record.purpose.as_str(),
                invocation.owner_summary
            )),
            action: None,
        });
        rows.push(StatsRow {
            label: format!("{}{} model", short_prefix(prefix), index + 1),
            value: truncate_value(&invocation_model_summary(invocation)),
            action: None,
        });
        rows.push(StatsRow {
            label: format!("{}{} auth", short_prefix(prefix), index + 1),
            value: truncate_value(&invocation_authorization_summary(invocation)),
            action: None,
        });
        rows.push(StatsRow {
            label: format!("{}{} scope", short_prefix(prefix), index + 1),
            value: truncate_value(&format!(
                "{} line:{}",
                invocation.scope_summary, invocation.lineage_summary
            )),
            action: None,
        });
        rows.push(StatsRow {
            label: format!("{}{} stop", short_prefix(prefix), index + 1),
            value: truncate_value(&invocation_stop_summary(invocation)),
            action: cancellable_action(invocation),
        });
    }
}

fn short_prefix(prefix: &str) -> char {
    prefix.chars().next().unwrap_or('?')
}

fn invocation_model_summary(invocation: &ModelInvocationView) -> String {
    let provider = invocation.record.provider.as_deref().unwrap_or("?");
    let model = invocation.record.model.as_deref().unwrap_or("?");
    let tier = invocation
        .record
        .model_tier
        .map(|tier| format!("{tier:?}").to_ascii_lowercase())
        .unwrap_or_else(|| "unknown".to_string());
    let effort = invocation.record.effort.as_deref().unwrap_or("default");
    let confidence = format!("{:?}", invocation.record.usage.confidence).to_ascii_lowercase();
    let mut parts = vec![format!("{provider}/{model}")];
    parts.push(format!("tier:{tier}"));
    parts.push(format!("eff:{effort}"));
    parts.push(format!("conf:{confidence}"));
    if let Some(source) = &invocation.record.escalation_source {
        let reason = invocation
            .record
            .escalation_reason
            .as_deref()
            .unwrap_or("unspecified");
        parts.push(format!("esc:{source}:{reason}"));
    }
    parts.join(" ")
}

fn invocation_authorization_summary(invocation: &ModelInvocationView) -> String {
    let auth = if invocation.record.policy_authorized {
        "allowed"
    } else {
        "blocked"
    };
    let reason = invocation
        .record
        .authorization_reason
        .as_deref()
        .unwrap_or(&invocation.record.raw_admission_status);
    let terminal = invocation
        .denial_reason
        .as_deref()
        .or(invocation.cancellation_reason.as_deref())
        .unwrap_or(&invocation.record.raw_status);
    format!(
        "auth:{auth} policy:{}:{} {}",
        invocation.record.policy_snapshot_status,
        reason,
        invocation_budget_summary(&invocation.budget, terminal)
    )
}

fn invocation_budget_summary(budget: &[ModelBudgetHeadroom], terminal: &str) -> String {
    let Some(headroom) = budget.first() else {
        return format!("status:{terminal} headroom:(none)");
    };
    let mut parts = vec![format!(
        "status:{terminal} headroom:{}:{}",
        headroom.policy_status,
        if headroom.authorized { "ok" } else { "deny" }
    )];
    if let Some(remaining) = headroom.remaining_calls {
        parts.push(format!("calls:{remaining}"));
    }
    if let Some(remaining) = headroom.remaining_active {
        parts.push(format!("active:{remaining}"));
    }
    if let Some(remaining) = headroom.remaining_total_tokens {
        parts.push(format!("tok:{}", compact_count(remaining.max(0) as u64)));
    }
    if let Some(remaining) = headroom.remaining_input_tokens {
        parts.push(format!("in:{}", compact_count(remaining.max(0) as u64)));
    }
    if let Some(remaining) = headroom.remaining_output_tokens {
        parts.push(format!("out:{}", compact_count(remaining.max(0) as u64)));
    }
    parts.push(headroom.source.clone());
    parts.join(" ")
}

fn invocation_stop_summary(invocation: &ModelInvocationView) -> String {
    if cancellable_action(invocation).is_some() {
        format!("Enter to cancel via {}", invocation.stop_mechanism)
    } else {
        format!(
            "{} via {}",
            status_label(invocation.record.status),
            invocation.stop_mechanism
        )
    }
}

fn cancellable_action(invocation: &ModelInvocationView) -> Option<StatsRowAction> {
    matches!(invocation.record.status, ModelInvocationStatus::Running)
        .then_some(StatsRowAction::CancelInvocation(invocation.record.id))
}

fn status_label(status: ModelInvocationStatus) -> &'static str {
    match status {
        ModelInvocationStatus::Running => "running",
        ModelInvocationStatus::CancellationRequested => "cancellation_requested",
        ModelInvocationStatus::Completed => "completed",
        ModelInvocationStatus::Failed => "failed",
        ModelInvocationStatus::Cancelled => "cancelled",
        ModelInvocationStatus::Denied => "denied",
        ModelInvocationStatus::Unknown => "unknown",
    }
}

fn format_scope(kind: BudgetScopeKind, scope_id: Option<&str>) -> String {
    let kind = format!("{kind:?}").to_ascii_lowercase();
    match scope_id {
        Some(scope_id) => format!("{kind}:{scope_id}"),
        None => kind,
    }
}

fn usage_bar(value: u64, max: u64, total_bars: usize) -> String {
    let filled = if max == 0 {
        0
    } else {
        (((value as f64 / max as f64) * total_bars as f64).round() as usize).min(total_bars)
    };
    let mut bar = String::with_capacity(total_bars);
    for i in 0..total_bars {
        bar.push(if i < filled { '▮' } else { '▯' });
    }
    bar
}

fn compact_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn per_model_summary(models: &[rsi_common::types::ModelUsage]) -> String {
    if models.is_empty() {
        return "(no data)".to_string();
    }
    let joined = models
        .iter()
        .map(|m| format!("{}: ${:.2}", m.model, m.cost_usd))
        .collect::<Vec<_>>()
        .join(", ");
    truncate_value(&joined)
}

fn truncate_value(s: &str) -> String {
    const MAX: usize = 90;
    let mut out = String::with_capacity(MAX + 1);
    let mut count = 0;
    for ch in s.chars() {
        if count + 1 >= MAX {
            out.push('…');
            return out;
        }
        out.push(if ch == '\n' || ch == '\r' { ' ' } else { ch });
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use rsi_common::model_control::{
        AdmissionStatus, InvocationForeground, InvocationOwner, ModelBudgetAlert,
        ModelBudgetPolicy, ModelControlMode, ModelControlStatusReport, ModelInvocationKind,
        ModelInvocationPurpose, ModelInvocationRecord, ModelInvocationUsage, ModelTier,
        ModelUsageConfidence, PaidRisk,
    };
    use std::path::PathBuf;

    fn test_app() -> App {
        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from(
            "/tmp/test-model-control-stats.sock",
        )))
    }

    fn fixture_invocation(status: ModelInvocationStatus) -> ModelInvocationView {
        ModelInvocationView {
            record: ModelInvocationRecord {
                id: uuid::Uuid::from_u128(7),
                purpose: ModelInvocationPurpose::SessionLaunchFresh,
                kind: ModelInvocationKind::SessionLifecycle,
                foreground: InvocationForeground::Foreground,
                paid_risk: PaidRisk::PaidCapable,
                status,
                provider: Some("Claude".to_string()),
                model: Some("claude-sonnet-5".to_string()),
                backend: Some("Claude".to_string()),
                model_tier: Some(ModelTier::Premium),
                effort: Some("medium".to_string()),
                trigger: "launch_session".to_string(),
                owner: InvocationOwner {
                    session_id: Some(uuid::Uuid::from_u128(1)),
                    ..Default::default()
                },
                owner_scopes: Vec::new(),
                dedup_key: None,
                request_fingerprint: None,
                parent_invocation_id: None,
                retry_of_invocation_id: None,
                raw_admission_status: "admitted".to_string(),
                raw_status: status_label(status).to_string(),
                admission_status: if matches!(status, ModelInvocationStatus::Denied) {
                    AdmissionStatus::Denied
                } else {
                    AdmissionStatus::Admitted
                },
                usage: ModelInvocationUsage {
                    input_tokens: Some(1200),
                    output_tokens: Some(400),
                    cache_creation_tokens: Some(0),
                    cache_read_tokens: Some(0),
                    reasoning_tokens: Some(50),
                    embedding_input_count: Some(0),
                    wall_time_ms: Some(35_000),
                    estimated_cost_usd: Some(0.42),
                    confidence: ModelUsageConfidence::Measured,
                },
                baseline_usage: ModelInvocationUsage::default(),
                error_class: None,
                cancellation_requested_at: None,
                cancellation_reason: None,
                cancellation_mechanism: None,
                authorization_reason: Some("policy_session".to_string()),
                policy_authorized: !matches!(status, ModelInvocationStatus::Denied),
                escalation_source: Some("operator".to_string()),
                escalation_reason: Some("manual".to_string()),
                policy_snapshot: None,
                policy_snapshot_status: "valid".to_string(),
                policy_snapshot_error: None,
                created_at: "2026-07-15T00:00:00Z".to_string(),
                started_at: Some("2026-07-15T00:00:01Z".to_string()),
                completed_at: None,
            },
            owner_summary: "session 00000001".to_string(),
            lineage_summary: "root".to_string(),
            scope_summary: "session:0001 | project:0002".to_string(),
            denial_reason: matches!(status, ModelInvocationStatus::Denied)
                .then_some("provider_circuit_open".to_string()),
            stop_mechanism: "interrupt_session".to_string(),
            stop_target: Some(uuid::Uuid::from_u128(1).to_string()),
            cancellation_reason: None,
            budget: vec![ModelBudgetHeadroom {
                scope_kind: BudgetScopeKind::Session,
                scope_id: Some(uuid::Uuid::from_u128(1).to_string()),
                purpose: Some(ModelInvocationPurpose::SessionLaunchFresh),
                model_tier: Some(ModelTier::Premium),
                effort: Some("medium".to_string()),
                source: "policy:session/test".to_string(),
                authorized: !matches!(status, ModelInvocationStatus::Denied),
                policy_status: "configured".to_string(),
                remaining_calls: Some(2),
                remaining_active: Some(0),
                remaining_total_tokens: Some(20_000),
                remaining_input_tokens: Some(15_000),
                remaining_output_tokens: Some(5_000),
                remaining_embedding_inputs: None,
                remaining_wall_time_ms: Some(120_000),
            }],
        }
    }

    #[test]
    fn stats_rows_include_cancel_action_for_running_invocation() {
        let mut app = test_app();
        app.cached_model_control_status = Some(ModelControlStatusReport {
            mode: ModelControlMode::Normal,
            mode_updated_at: Some("2026-07-15T00:00:00Z".to_string()),
            restart_required_fields: Vec::new(),
            circuit_state: "closed".to_string(),
            circuit_reason: "budget-governed".to_string(),
            circuits: Vec::new(),
            policies: vec![ModelBudgetPolicy {
                scope_kind: BudgetScopeKind::Session,
                scope_id: Some(uuid::Uuid::from_u128(1).to_string()),
                purpose: Some(ModelInvocationPurpose::SessionLaunchFresh),
                model_tier: Some(ModelTier::Premium),
                effort: Some("medium".to_string()),
                ceiling_model_tier: None,
                ceiling_effort: None,
                max_calls: Some(3),
                max_total_tokens: Some(20_000),
                max_input_tokens: None,
                max_output_tokens: None,
                max_cache_creation_tokens: None,
                max_cache_read_tokens: None,
                max_reasoning_tokens: None,
                max_embedding_inputs: None,
                max_wall_time_ms: None,
                max_concurrency: Some(1),
                max_retries: Some(0),
                max_calls_per_window: None,
                rate_window_seconds: None,
                alert_threshold_ratio: Some(0.25),
            }],
            active_invocations: vec![fixture_invocation(ModelInvocationStatus::Running)],
            recent_invocations: Vec::new(),
            recent_denials: Vec::new(),
            recent_budget_alerts: Vec::new(),
        });

        let rows = stats_rows(&app);
        assert!(rows.iter().any(|row| row.label == "Active 1"));
        assert!(rows.iter().any(|row| {
            row.label == "A1 stop"
                && row.action == Some(StatsRowAction::CancelInvocation(uuid::Uuid::from_u128(7)))
        }));
    }

    #[test]
    fn stats_rows_render_denials_and_alerts_visibly() {
        let mut app = test_app();
        app.cached_model_control_status = Some(ModelControlStatusReport {
            mode: ModelControlMode::DenyPaid,
            mode_updated_at: Some("2026-07-15T00:00:00Z".to_string()),
            restart_required_fields: Vec::new(),
            circuit_state: "open".to_string(),
            circuit_reason: "paid background denied".to_string(),
            circuits: Vec::new(),
            policies: Vec::new(),
            active_invocations: Vec::new(),
            recent_invocations: Vec::new(),
            recent_denials: vec![fixture_invocation(ModelInvocationStatus::Denied)],
            recent_budget_alerts: vec![ModelBudgetAlert {
                invocation_id: uuid::Uuid::from_u128(7),
                scope_kind: BudgetScopeKind::Session,
                scope_id: Some(uuid::Uuid::from_u128(1).to_string()),
                purpose: Some(ModelInvocationPurpose::SessionLaunchFresh),
                metric: "output_tokens".to_string(),
                remaining: 5,
                limit: 20,
                threshold: 5,
            }],
        });

        let rows = stats_rows(&app);
        assert!(
            rows.iter()
                .any(|row| row.label == "Denials" && row.value == "1 recent")
        );
        assert!(rows.iter().any(|row| {
            row.label == "Denied 1" && row.value.contains("denied session.launch.fresh")
        }));
        assert!(rows.iter().any(|row| {
            row.label == "Alert 1" && row.value.contains("output_tokens rem:5 lim:20 thr:5")
        }));
    }

    #[test]
    fn stats_rows_render_cancellation_requested_without_repeat_cancel_action() {
        let mut app = test_app();
        let mut invocation = fixture_invocation(ModelInvocationStatus::CancellationRequested);
        invocation.cancellation_reason = Some("operator_cancelled".to_string());
        app.cached_model_control_status = Some(ModelControlStatusReport {
            mode: ModelControlMode::StopAll,
            mode_updated_at: Some("2026-07-15T00:00:00Z".to_string()),
            restart_required_fields: Vec::new(),
            circuit_state: "open:stop_all".to_string(),
            circuit_reason: "all model work stopped".to_string(),
            circuits: Vec::new(),
            policies: Vec::new(),
            active_invocations: vec![invocation],
            recent_invocations: Vec::new(),
            recent_denials: Vec::new(),
            recent_budget_alerts: Vec::new(),
        });

        let rows = stats_rows(&app);
        assert!(rows.iter().any(|row| {
            row.label == "A1 stop"
                && row
                    .value
                    .contains("cancellation_requested via interrupt_session")
                && row.action.is_none()
        }));
        assert!(rows.iter().any(|row| {
            row.label == "A1 auth" && row.value.contains("status:operator_cancelled")
        }));
    }
}
