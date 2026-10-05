use crate::app::App;
use crate::modalkit_types::LcAction;
use rsi_common::model_control::{
    BudgetScopeKind, ModelControlMode, ModelInvocationPurpose, ModelInvocationStatus,
    ModelInvocationView,
};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatsGroup {
    Overview,
    Safety,
    Active,
    Alerts,
    Denied,
    ModelSpend,
    Tokens,
    Recent,
    Efficiency,
}

impl StatsGroup {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Overview => "LIFETIME USAGE",
            Self::Safety => "SPENDING CONTROLS",
            Self::Active => "LIVE MODEL WORK",
            Self::Alerts => "WARNINGS & LIMITS",
            Self::Denied => "BLOCKED REQUESTS",
            Self::ModelSpend => "SPEND BY MODEL",
            Self::Tokens => "TOKEN BREAKDOWN",
            Self::Recent => "RECENT ACTIVITY",
            Self::Efficiency => "DELIVERY EFFICIENCY · TODAY (UTC)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatsTone {
    Neutral,
    Positive,
    Warning,
    Error,
    Muted,
    Accent,
}

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
    pub group: StatsGroup,
    pub tone: StatsTone,
    pub status: Option<ModelInvocationStatus>,
    pub description: String,
    /// Full diagnostics belong in the selected row's info pane, never the list.
    pub details: Vec<(String, String)>,
}

impl StatsRow {
    pub(crate) fn info(
        group: StatsGroup,
        label: impl Into<String>,
        value: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        let value = value.into();
        let tone = if value.contains("loading") || value.contains("unknown") {
            StatsTone::Muted
        } else {
            StatsTone::Neutral
        };
        Self {
            label: label.into(),
            value,
            group,
            tone,
            description: description.into(),
            action: None,
            status: None,
            details: Vec::new(),
        }
    }
}

pub(crate) fn stats_rows(app: &App) -> Vec<StatsRow> {
    let mut rows = overview_rows(app);
    append_safety_rows(&mut rows, app);
    if let Some(control) = &app.cached_model_control_status {
        let mut seen = HashSet::new();
        for invocation in &control.active_invocations {
            if seen.insert(invocation.record.id) {
                rows.push(invocation_row(app, StatsGroup::Active, invocation));
            }
        }
        if control.active_invocations.is_empty() {
            rows.push(StatsRow::info(
                StatsGroup::Active,
                "No live model work",
                "Idle",
                "No model invocations are currently running. Press R to refresh.",
            ));
        }
        for alert in &control.recent_budget_alerts {
            let mut row = StatsRow::info(
                StatsGroup::Alerts,
                scope_name(app, alert.scope_kind, alert.scope_id.as_deref()),
                format!(
                    "{}: {} left of {}",
                    humanize(&alert.metric),
                    alert.remaining,
                    alert.limit
                ),
                "Budget approaching its limit. Review the matching policy in the Budgets tab.",
            );
            row.tone = StatsTone::Warning;
            row.details = vec![
                ("Warning threshold".into(), alert.threshold.to_string()),
                ("Invocation ID".into(), alert.invocation_id.to_string()),
                (
                    "Scope".into(),
                    format_scope(alert.scope_kind, alert.scope_id.as_deref()),
                ),
            ];
            rows.push(row);
        }
        for circuit in &control.circuits {
            let mut row = StatsRow::info(
                StatsGroup::Alerts,
                scope_name(app, circuit.scope_kind, circuit.scope_id.as_deref()),
                circuit_label(&circuit.state),
                circuit.reason.clone(),
            );
            row.tone = circuit_tone(&circuit.state);
            row.details = vec![
                ("Circuit state".into(), circuit.state.clone()),
                ("Source".into(), circuit.source.clone()),
                (
                    "Scope".into(),
                    format_scope(circuit.scope_kind, circuit.scope_id.as_deref()),
                ),
            ];
            if let Some(error) = &circuit.error_class {
                row.details.push(("Error".into(), error.clone()));
            }
            rows.push(row);
        }
        for invocation in &control.recent_denials {
            if seen.insert(invocation.record.id) {
                rows.push(invocation_row(app, StatsGroup::Denied, invocation));
            }
        }
    } else {
        rows.push(StatsRow::info(
            StatsGroup::Active,
            "Live model work",
            "(loading…)",
            "Waiting for model-control telemetry. Press R to refresh.",
        ));
    }
    append_model_rows(&mut rows, app);
    append_token_rows(&mut rows, app);
    if let Some(control) = &app.cached_model_control_status {
        let shown: HashSet<_> = control
            .active_invocations
            .iter()
            .chain(&control.recent_denials)
            .map(|invocation| invocation.record.id)
            .collect();
        let mut recent_seen = shown;
        for invocation in &control.recent_invocations {
            if recent_seen.insert(invocation.record.id) {
                rows.push(invocation_row(app, StatsGroup::Recent, invocation));
            }
        }
    }
    rows.extend(crate::efficiency_stats::efficiency_rows(
        app.cached_efficiency_metrics.as_ref(),
    ));
    rows
}

pub(crate) fn stats_row_count(app: &App) -> usize {
    stats_rows(app).len()
}

pub(crate) fn stats_row_action(app: &App, idx: usize) -> Option<LcAction> {
    match stats_rows(app).get(idx).and_then(|row| row.action.clone()) {
        Some(StatsRowAction::EmergencyStopAll) => Some(LcAction::EmergencyStopAll),
        Some(StatsRowAction::CancelInvocation(id)) => Some(LcAction::CancelModelInvocation(id)),
        None => None,
    }
}

fn overview_rows(app: &App) -> Vec<StatsRow> {
    let stats = app.cached_usage_stats.as_ref();
    let mut spend = StatsRow::info(
        StatsGroup::Overview,
        "Total spend",
        stats
            .map(|s| format!("${:.2}", s.total_cost_usd))
            .unwrap_or_else(loading),
        "Lifetime recorded model cost across chats. Provider billing may differ from recorded usage.",
    );
    if stats.is_some() {
        spend.tone = StatsTone::Accent;
    }
    vec![
        spend,
        StatsRow::info(
            StatsGroup::Overview,
            "Chats",
            stats
                .map(|s| s.lifetime_chats.to_string())
                .unwrap_or_else(loading),
            "Total recorded chats across providers.",
        ),
        StatsRow::info(
            StatsGroup::Overview,
            "Work time",
            stats
                .map(|s| crate::ui::session::format_work_time_ms(s.total_work_time_ms))
                .unwrap_or_else(loading),
            "Accumulated session work time, including idle time while a session is Running. This is not compute time.",
        ),
    ]
}

fn append_safety_rows(rows: &mut Vec<StatsRow>, app: &App) {
    let control = app.cached_model_control_status.as_ref();
    let mut mode = StatsRow::info(
        StatsGroup::Safety,
        "Allowed work",
        control
            .map(|s| {
                match s.mode {
                    ModelControlMode::Normal => "Normal · budgets apply",
                    ModelControlMode::PauseBackground => "Background work paused",
                    ModelControlMode::DenyPaid => "Paid work blocked",
                    ModelControlMode::LocalOnly => "Local models only",
                    ModelControlMode::StopAll => "All model work stopped",
                }
                .to_string()
            })
            .unwrap_or_else(loading),
        "Current model admission mode. Change it in the Model Control tab; budget rules still apply.",
    );
    mode.tone = control
        .map(|s| {
            if s.mode == ModelControlMode::Normal {
                StatsTone::Positive
            } else {
                StatsTone::Warning
            }
        })
        .unwrap_or(StatsTone::Muted);
    rows.push(mode);
    let mut circuit = StatsRow::info(
        StatsGroup::Safety,
        "Safety gate",
        control
            .map(|s| circuit_label(&s.circuit_state))
            .unwrap_or_else(loading),
        control
            .map(|s| s.circuit_reason.as_str())
            .unwrap_or("Waiting for safety-gate telemetry."),
    );
    circuit.tone = control
        .map(|s| circuit_tone(&s.circuit_state))
        .unwrap_or(StatsTone::Muted);
    if let Some(control) = control {
        circuit
            .details
            .push(("Circuit state".into(), control.circuit_state.clone()));
    }
    rows.push(circuit);
    rows.push(StatsRow::info(StatsGroup::Safety, "Budget policies",
        control.map(|s| format!("{} configured · edit in Budgets", s.policies.len())).unwrap_or_else(loading),
        "Explicit budget policies limit model calls, tokens, or concurrency for their scope. Zero policies does not mean unrestricted work: admission mode and built-in rules still apply."));
    let mut stop = StatsRow::info(
        StatsGroup::Safety,
        "Emergency stop",
        "Stops all model work",
        "Stops running model work and blocks new model invocations. To allow work again, change the mode in Model Control.",
    );
    stop.action = Some(StatsRowAction::EmergencyStopAll);
    stop.tone = StatsTone::Error;
    rows.push(stop);
}

fn append_model_rows(rows: &mut Vec<StatsRow>, app: &App) {
    let Some(stats) = &app.cached_usage_stats else {
        rows.push(StatsRow::info(
            StatsGroup::ModelSpend,
            "Model costs",
            loading(),
            "Lifetime cost grouped by model.",
        ));
        return;
    };
    if stats.per_model.is_empty() {
        rows.push(StatsRow::info(
            StatsGroup::ModelSpend,
            "No model usage yet",
            "No data",
            "Model costs appear when recorded usage becomes available.",
        ));
    }
    let mut models: Vec<_> = stats.per_model.iter().collect();
    models.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then_with(|| a.model.cmp(&b.model))
    });
    for model in models {
        let mut row = StatsRow::info(
            StatsGroup::ModelSpend,
            single_line(&model.model),
            format!("${:.2} · {} chats", model.cost_usd, model.chats),
            "Lifetime recorded spend for this model, ordered by highest cost first.",
        );
        row.tone = StatsTone::Accent;
        row.details = vec![
            ("Model".into(), model.model.clone()),
            ("Input tokens".into(), model.input_tokens.to_string()),
            ("Output tokens".into(), model.output_tokens.to_string()),
            (
                "Cache written".into(),
                model.cache_creation_tokens.to_string(),
            ),
            ("Cache reused".into(), model.cache_read_tokens.to_string()),
        ];
        rows.push(row);
    }
}

fn append_token_rows(rows: &mut Vec<StatsRow>, app: &App) {
    let stats = app.cached_usage_stats.as_ref();
    let max = stats
        .map(|s| {
            s.total_input_tokens
                .max(s.total_output_tokens)
                .max(s.total_cache_creation_tokens)
                .max(s.total_cache_read_tokens)
        })
        .unwrap_or(0);
    for (label, value, description) in [
        (
            "Input tokens",
            stats.map(|s| s.total_input_tokens),
            "Recorded input tokens sent to models.",
        ),
        (
            "Output tokens",
            stats.map(|s| s.total_output_tokens),
            "Recorded response tokens generated by models.",
        ),
        (
            "Cache written",
            stats.map(|s| s.total_cache_creation_tokens),
            "Tokens recorded as written into provider prompt caches.",
        ),
        (
            "Cache reused",
            stats.map(|s| s.total_cache_read_tokens),
            "Tokens recorded as read from provider prompt caches.",
        ),
    ] {
        let mut row = StatsRow::info(
            StatsGroup::Tokens,
            label,
            value
                .map(|n| format!("{}  {}", compact_count(n), usage_bar(n, max, 10)))
                .unwrap_or_else(loading),
            format!(
                "{description} Bar compares this category to the largest token category, not a budget limit."
            ),
        );
        if let Some(value) = value {
            row.tone = if label == "Cache reused" {
                StatsTone::Positive
            } else {
                StatsTone::Accent
            };
            row.details
                .push(("Exact token count".into(), value.to_string()));
        }
        rows.push(row);
    }
}

fn invocation_row(app: &App, group: StatsGroup, invocation: &ModelInvocationView) -> StatsRow {
    let record = &invocation.record;
    let purpose = purpose_label(record.purpose);
    let mut row = StatsRow::info(
        group,
        invocation_owner_name(app, invocation),
        format!(
            "{} · {purpose}",
            single_line(record.model.as_deref().unwrap_or("Model unknown"))
        ),
        format!("{purpose}. {}", invocation_outcome(invocation)),
    );
    row.status = Some(record.status);
    row.tone = status_tone(record.status);
    row.action = matches!(record.status, ModelInvocationStatus::Running)
        .then_some(StatsRowAction::CancelInvocation(record.id));
    row.details = vec![
        ("Work".into(), row.label.clone()),
        ("Status".into(), status_label(record.status).into()),
        (
            "Provider".into(),
            record.provider.clone().unwrap_or_else(|| "Unknown".into()),
        ),
        (
            "Model".into(),
            record.model.clone().unwrap_or_else(|| "Unknown".into()),
        ),
        (
            "Effort".into(),
            record
                .effort
                .clone()
                .unwrap_or_else(|| "Provider default".into()),
        ),
        (
            "Policy decision".into(),
            if record.policy_authorized {
                "Allowed"
            } else {
                "Blocked"
            }
            .into(),
        ),
        (
            "Policy reason".into(),
            record
                .authorization_reason
                .clone()
                .unwrap_or_else(|| record.raw_admission_status.clone()),
        ),
        (
            "Policy snapshot".into(),
            record.policy_snapshot_status.clone(),
        ),
        (
            "Usage confidence".into(),
            humanize(&format!("{:?}", record.usage.confidence)),
        ),
        (
            "Stop behavior".into(),
            stop_label(&invocation.stop_mechanism).into(),
        ),
        ("Created".into(), record.created_at.clone()),
    ];
    if let Some(cost) = record.usage.estimated_cost_usd {
        row.details
            .push(("Recorded cost".into(), format!("${cost:.2}")));
    }
    if let Some(error) = &record.error_class {
        row.details.push(("Error".into(), error.clone()));
    }
    if let Some(reason) = invocation
        .denial_reason
        .as_ref()
        .or(invocation.cancellation_reason.as_ref())
    {
        row.details
            .push(("Outcome reason".into(), humanize(reason)));
    }
    if let Some(source) = &record.escalation_source {
        row.details.push((
            "Escalation".into(),
            format!(
                "{} · {}",
                source,
                record
                    .escalation_reason
                    .as_deref()
                    .unwrap_or("Reason unavailable")
            ),
        ));
    }
    for budget in &invocation.budget {
        let mut parts = vec![
            if budget.authorized {
                "Allowed"
            } else {
                "Blocked"
            }
            .to_string(),
        ];
        for (label, remaining) in [
            ("calls", budget.remaining_calls),
            ("active slots", budget.remaining_active),
            ("total tokens", budget.remaining_total_tokens),
            ("input tokens", budget.remaining_input_tokens),
            ("output tokens", budget.remaining_output_tokens),
            ("embedding inputs", budget.remaining_embedding_inputs),
            ("work milliseconds", budget.remaining_wall_time_ms),
        ] {
            if let Some(remaining) = remaining {
                parts.push(format!("{remaining} {label} left"));
            }
        }
        parts.push(format!("{} · {}", budget.policy_status, budget.source));
        row.details.push((
            format!(
                "Budget · {}",
                scope_name(app, budget.scope_kind, budget.scope_id.as_deref())
            ),
            parts.join(" · "),
        ));
    }
    row.details.extend([
        ("Invocation ID".into(), record.id.to_string()),
        ("Owner IDs".into(), invocation.owner_summary.clone()),
        ("Scope IDs".into(), invocation.scope_summary.clone()),
        ("Lineage IDs".into(), invocation.lineage_summary.clone()),
        ("Purpose code".into(), record.purpose.as_str().into()),
        ("Stop mechanism".into(), invocation.stop_mechanism.clone()),
    ]);
    if let Some(id) = record.owner.session_id {
        row.details.push(("Session ID".into(), id.to_string()));
    }
    if let Some(id) = record.owner.project_id {
        row.details.push(("Project ID".into(), id.to_string()));
    }
    row
}

fn invocation_owner_name(app: &App, invocation: &ModelInvocationView) -> String {
    let owner = &invocation.record.owner;
    let session = owner.session_id.and_then(|id| app.sessions.get(&id));
    let name = if let Some(session) = session {
        crate::types::resolve_session_display_identity(&session.session, &app.sessions)
            .effective_title
    } else if let Some(id) = owner.session_id {
        format!("Session {} (name unavailable)", &id.to_string()[..8])
    } else if let Some(issue) = &owner.issue_identifier {
        format!("Issue {issue}")
    } else if owner.scheduled_job_id.is_some() {
        "Scheduled job".into()
    } else if owner.workflow_id.is_some() {
        "Workflow".into()
    } else if let Some(operator) = &owner.operator {
        format!("Operator {operator}")
    } else {
        "Background helper".into()
    };
    let project_id = owner
        .project_id
        .or_else(|| session.and_then(|s| s.session.project_id));
    match project_id.and_then(|id| app.projects.iter().find(|project| project.id == id)) {
        Some(project) => format!("{} · {}", single_line(&name), single_line(&project.name)),
        None => single_line(&name),
    }
}

fn scope_name(app: &App, kind: BudgetScopeKind, id: Option<&str>) -> String {
    let uuid = id.and_then(|id| uuid::Uuid::parse_str(id).ok());
    match kind {
        BudgetScopeKind::Session => uuid.and_then(|id| app.sessions.get(&id)).map(|s| {
            single_line(
                &crate::types::resolve_session_display_identity(&s.session, &app.sessions)
                    .effective_title,
            )
        }),
        BudgetScopeKind::Project => uuid
            .and_then(|id| app.projects.iter().find(|p| p.id == id))
            .map(|p| single_line(&p.name)),
        _ => None,
    }
    .unwrap_or_else(|| match id {
        Some(id) if uuid.is_some() => format!(
            "{} {} (name unavailable)",
            humanize(&format!("{kind:?}")),
            id.chars().take(8).collect::<String>()
        ),
        Some(id) => format!("{} · {}", humanize(&format!("{kind:?}")), single_line(id)),
        None => humanize(&format!("{kind:?}")),
    })
}

fn invocation_outcome(invocation: &ModelInvocationView) -> String {
    if let Some(reason) = invocation
        .denial_reason
        .as_ref()
        .or(invocation.cancellation_reason.as_ref())
    {
        return humanize(reason);
    }
    match invocation.record.status {
        ModelInvocationStatus::Running => "Select Stop to cancel this model work.".into(),
        ModelInvocationStatus::CancellationRequested => {
            "Stop requested; waiting for the provider to finish cancelling.".into()
        }
        ModelInvocationStatus::Completed => "Model work completed.".into(),
        ModelInvocationStatus::Failed => "Model work failed; inspect error details below.".into(),
        ModelInvocationStatus::Cancelled => "Model work was stopped.".into(),
        ModelInvocationStatus::Denied => "Request blocked by model-control rules.".into(),
        ModelInvocationStatus::Unknown => "Provider status unavailable.".into(),
    }
}

pub(crate) fn status_label(status: ModelInvocationStatus) -> &'static str {
    match status {
        ModelInvocationStatus::Running => "Running",
        ModelInvocationStatus::CancellationRequested => "Stopping",
        ModelInvocationStatus::Completed => "Completed",
        ModelInvocationStatus::Failed => "Failed",
        ModelInvocationStatus::Cancelled => "Stopped",
        ModelInvocationStatus::Denied => "Blocked",
        ModelInvocationStatus::Unknown => "Unknown",
    }
}

pub(crate) fn status_tone(status: ModelInvocationStatus) -> StatsTone {
    match status {
        ModelInvocationStatus::Running | ModelInvocationStatus::Completed => StatsTone::Positive,
        ModelInvocationStatus::CancellationRequested => StatsTone::Warning,
        ModelInvocationStatus::Failed | ModelInvocationStatus::Denied => StatsTone::Error,
        ModelInvocationStatus::Cancelled | ModelInvocationStatus::Unknown => StatsTone::Muted,
    }
}

fn purpose_label(purpose: ModelInvocationPurpose) -> &'static str {
    use ModelInvocationPurpose::*;
    match purpose {
        SessionLaunchFresh => "Start session",
        SessionContinueResume => "Continue session",
        SessionRetryAuto => "Retry session",
        SessionRotateChild => "Rotate session",
        SessionHarnessTurn | SessionCodexAppServerTurn | SessionOpenAiCompatibleTurn => {
            "Agent turn"
        }
        SessionHarnessCompaction => "Compact context",
        AgentSpawnChild => "Start child agent",
        AgentReserveSuccessor => "Prepare successor",
        WorkflowGraphNode => "Workflow step",
        WorkflowChainIteration => "Workflow iteration",
        RecursiveLiveTask => "Recursive task",
        ScheduledFresh | AgentScheduleWakeFresh => "Scheduled launch",
        ScheduledResumeWatch => "Scheduled resume",
        IssueTrackerDispatch => "Issue dispatch",
        PromptCompile => "Compile prompt",
        TextGenerateRpc => "Generate text",
        SessionTitle => "Generate session title",
        SessionSummary => "Summarize session",
        MemoryObservationExtract => "Extract memories",
        MemoryEmbeddingIndex => "Index memories",
        DreamConsolidation => "Consolidate memories",
        StallClassifier => "Check stalled agent",
        DialecticQuery => "Query dialectic",
        ModelDiscoveryClaudeProbe => "Discover models",
        QueueDeferredModelTask => "Deferred model task",
    }
}

fn circuit_label(state: &str) -> String {
    match state {
        "closed" => "Ready · budget rules apply".into(),
        state if state.starts_with("open") => "Blocking model work".into(),
        state if state.starts_with("half_open") => "Checking recovery".into(),
        state => humanize(state),
    }
}

fn circuit_tone(state: &str) -> StatsTone {
    if state == "closed" {
        StatsTone::Positive
    } else if state.starts_with("open") {
        StatsTone::Error
    } else {
        StatsTone::Warning
    }
}

fn stop_label(mechanism: &str) -> &str {
    match mechanism {
        "interrupt_session" => "Interrupt the owning session",
        "cancel_task" | "abort_task" => "Cancel the background task",
        "cooperative" => "Request cooperative cancellation",
        mechanism => mechanism,
    }
}

fn format_scope(kind: BudgetScopeKind, id: Option<&str>) -> String {
    match id {
        Some(id) => format!("{kind:?}:{id}"),
        None => format!("{kind:?}"),
    }
}

fn loading() -> String {
    "(loading…)".into()
}
fn single_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn humanize(s: &str) -> String {
    let s = s.replace(['_', '.'], " ");
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Unknown".into(),
    }
}
fn usage_bar(value: u64, max: u64, width: usize) -> String {
    let filled = if max == 0 {
        0
    } else {
        (((value as f64 / max as f64) * width as f64).round() as usize).min(width)
    };
    (0..width)
        .map(|i| if i < filled { '━' } else { '─' })
        .collect()
}
fn compact_count(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use rsi_common::model_control::{
        AdmissionStatus, InvocationForeground, InvocationOwner, ModelBudgetAlert,
        ModelBudgetHeadroom, ModelBudgetPolicy, ModelControlMode, ModelControlStatusReport,
        ModelInvocationKind, ModelInvocationPurpose, ModelInvocationRecord, ModelInvocationUsage,
        ModelTier, ModelUsageConfidence, PaidRisk,
    };
    use std::path::PathBuf;

    fn test_app() -> App {
        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from(
            "/tmp/test-model-control-stats.sock",
        )))
    }

    pub(crate) fn fixture_invocation(status: ModelInvocationStatus) -> ModelInvocationView {
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
        let row = rows
            .iter()
            .find(|row| row.group == StatsGroup::Active)
            .unwrap();
        assert_eq!(row.status, Some(ModelInvocationStatus::Running));
        assert_eq!(
            row.action,
            Some(StatsRowAction::CancelInvocation(uuid::Uuid::from_u128(7)))
        );
        let index = rows
            .iter()
            .position(|row| row.group == StatsGroup::Active)
            .unwrap();
        assert_eq!(
            stats_row_action(&app, index),
            Some(LcAction::CancelModelInvocation(uuid::Uuid::from_u128(7)))
        );
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
        let denial = rows
            .iter()
            .find(|row| row.group == StatsGroup::Denied)
            .unwrap();
        assert_eq!(denial.status, Some(ModelInvocationStatus::Denied));
        assert_eq!(denial.tone, StatsTone::Error);
        assert!(denial.value.contains("Start session"));
        assert!(denial.description.contains("Provider circuit open"));
        let alert = rows
            .iter()
            .find(|row| row.group == StatsGroup::Alerts)
            .unwrap();
        assert_eq!(alert.tone, StatsTone::Warning);
        assert_eq!(alert.value, "Output tokens: 5 left of 20");
        assert!(
            alert
                .details
                .contains(&("Warning threshold".into(), "5".into()))
        );
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
        let row = rows
            .iter()
            .find(|row| row.group == StatsGroup::Active)
            .unwrap();
        assert_eq!(
            row.status,
            Some(ModelInvocationStatus::CancellationRequested)
        );
        assert_eq!(row.action, None);
        assert_eq!(row.tone, StatsTone::Warning);
        assert!(
            row.details
                .contains(&("Outcome reason".into(), "Operator cancelled".into()))
        );
        assert!(row.details.contains(&(
            "Stop behavior".into(),
            "Interrupt the owning session".into()
        )));
    }

    #[test]
    fn stats_rows_show_efficiency_section_loading_then_cached() {
        use rsi_common::rpc::{
            EfficiencyMetricTargets, EfficiencyMetricValues, EfficiencyMetricsGroupBy,
            EfficiencyMetricsResponse, EfficiencyMetricsRow,
        };
        let mut app = test_app();
        let label = "Efficiency (today, UTC)";
        let find = |app: &App| {
            stats_rows(app)
                .into_iter()
                .find(|row| row.label == label)
                .map(|row| row.value)
        };
        assert_eq!(find(&app).as_deref(), Some("(loading…)"));
        let from = chrono::Utc::now();
        app.cached_efficiency_metrics = Some(EfficiencyMetricsResponse {
            from,
            to: from,
            group_by: EfficiencyMetricsGroupBy::Day,
            targets: EfficiencyMetricTargets::default(),
            rows: vec![EfficiencyMetricsRow {
                day: "2026-09-30".to_string(),
                epic_id: None,
                epic_title: None,
                values: EfficiencyMetricValues::default(),
            }],
        });
        assert_eq!(find(&app).as_deref(), Some("2026-09-30"));
        assert!(stats_rows(&app).iter().any(|row| row.label == "Landings"));
    }
    pub(crate) fn fixture_control() -> ModelControlStatusReport {
        ModelControlStatusReport {
            mode: ModelControlMode::Normal,
            mode_updated_at: None,
            restart_required_fields: Vec::new(),
            circuit_state: "closed".into(),
            circuit_reason: "Budget rules apply; paid background work denied by default".into(),
            circuits: Vec::new(),
            policies: Vec::new(),
            active_invocations: Vec::new(),
            recent_invocations: Vec::new(),
            recent_denials: Vec::new(),
            recent_budget_alerts: Vec::new(),
        }
    }

    pub(crate) fn named_work_app() -> App {
        use crate::app::app_test_helpers::baseline_session;
        use crate::types::SessionState;
        use rsi_common::types::{Project, SessionKind};
        let mut app = test_app();
        let project_id = uuid::Uuid::from_u128(2);
        let mut session = baseline_session(uuid::Uuid::from_u128(1), SessionKind::Task);
        session.title = Some("Fix settings navigation".into());
        session.project_id = Some(project_id);
        app.sessions.insert(session.id, SessionState::new(session));
        app.projects.push(Project {
            id: project_id,
            name: "RSI".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        });
        let mut control = fixture_control();
        control
            .active_invocations
            .push(fixture_invocation(ModelInvocationStatus::Running));
        app.cached_model_control_status = Some(control);
        app
    }

    #[test]
    fn live_work_uses_canonical_name_and_project_with_full_ids_in_details() {
        let app = named_work_app();
        let rows = stats_rows(&app);
        let row = rows
            .iter()
            .find(|row| row.group == StatsGroup::Active)
            .unwrap();
        assert_eq!(row.label, "Fix settings navigation · RSI");
        assert_eq!(row.value, "claude-sonnet-5 · Start session");
        assert!(
            row.details
                .contains(&("Session ID".into(), uuid::Uuid::from_u128(1).to_string()))
        );
        assert!(
            row.details
                .iter()
                .any(|(key, value)| key.starts_with("Budget") && value.contains("2 calls left"))
        );
    }

    #[test]
    fn live_and_blocked_work_appear_once_across_history() {
        let mut app = named_work_app();
        let control = app.cached_model_control_status.as_mut().unwrap();
        let live = control.active_invocations[0].clone();
        let mut blocked = fixture_invocation(ModelInvocationStatus::Denied);
        blocked.record.id = uuid::Uuid::from_u128(8);
        control.recent_denials.push(blocked.clone());
        control.recent_invocations = vec![live, blocked];
        let rows = stats_rows(&app);
        assert_eq!(rows.iter().filter(|row| row.status.is_some()).count(), 2);
        assert_eq!(
            rows.iter()
                .filter(|row| row.group == StatsGroup::Active)
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.group == StatsGroup::Denied)
                .count(),
            1
        );
    }

    #[test]
    fn unavailable_owner_has_explicit_fallback_and_preserves_full_id() {
        let app = test_app();
        let invocation = fixture_invocation(ModelInvocationStatus::Running);
        let row = invocation_row(&app, StatsGroup::Active, &invocation);
        assert_eq!(row.label, "Session 00000000 (name unavailable)");
        assert!(
            row.details
                .contains(&("Session ID".into(), uuid::Uuid::from_u128(1).to_string()))
        );
    }

    #[test]
    fn terminal_work_keeps_status_and_has_no_repeat_stop_action() {
        let app = test_app();
        for status in [
            ModelInvocationStatus::Completed,
            ModelInvocationStatus::Failed,
            ModelInvocationStatus::Cancelled,
            ModelInvocationStatus::Denied,
            ModelInvocationStatus::CancellationRequested,
            ModelInvocationStatus::Unknown,
        ] {
            let row = invocation_row(&app, StatsGroup::Recent, &fixture_invocation(status));
            assert_eq!(row.status, Some(status));
            assert_eq!(row.action, None);
        }
    }

    #[test]
    fn spend_by_model_is_sorted_and_large_token_counts_are_readable() {
        use rsi_common::types::{ModelUsage, UsageStats};
        let mut app = test_app();
        app.cached_usage_stats = Some(UsageStats {
            total_cache_read_tokens: 87_462_700_000,
            per_model: vec![
                ModelUsage {
                    model: "Local model".into(),
                    cost_usd: 0.0,
                    ..Default::default()
                },
                ModelUsage {
                    model: "Premium model".into(),
                    cost_usd: 125.25,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let rows = stats_rows(&app);
        let models: Vec<_> = rows
            .iter()
            .filter(|row| row.group == StatsGroup::ModelSpend)
            .collect();
        assert_eq!(models[0].label, "Premium model");
        assert_eq!(models[0].value, "$125.25 · 0 chats");
        assert_eq!(models[1].label, "Local model");
        let cache = rows.iter().find(|row| row.label == "Cache reused").unwrap();
        assert_eq!(cache.value, "87.5B  ━━━━━━━━━━");
        assert_eq!(cache.tone, StatsTone::Positive);
    }
}
