use super::Store;
use crate::error::{DaemonError, Result};
use crate::model_control::classify_model_tier;
use crate::model_control::registry::RegistryEntry;
use crate::model_control::{ExpectedUsage, InvocationCompletion, ModelAdmissionRequest};
use chrono::{SecondsFormat, TimeDelta, Utc};
use rsi_common::model_control::{
    AdmissionStatus, BudgetScopeKind, BudgetScopeRef, InvocationForeground, ModelBudgetAlert,
    ModelBudgetHeadroom, ModelBudgetPolicy, ModelCircuitStatus, ModelControlMode,
    ModelControlStatusReport, ModelInvocationKind, ModelInvocationList, ModelInvocationPurpose,
    ModelInvocationRecord, ModelInvocationStatus, ModelInvocationUsage, ModelInvocationView,
    ModelTier, ModelUsageConfidence, PaidRisk,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const KEY_MODEL_CONTROL_MODE: &str = "model_control_mode";
pub const KEY_MODEL_CONTROL_BREAKER_REASON: &str = "model_control_breaker_reason";
pub const KEY_MODEL_CONTROL_BREAKER_INVOCATION_ID: &str = "model_control_breaker_invocation_id";
pub const KEY_MODEL_CONTROL_CIRCUITS: &str = "model_control_circuits";

/// Operator-declared ceiling on the reasoning effort an `Orchestration` spawn
/// may request, held in the existing V48 `daemon_settings` key-value table
/// (issue #34).
///
/// Before this key existed, the effort ceiling was implicitly the tree root's
/// OWN effort. That conflated two unrelated quantities: how hard the
/// orchestrator should think (a property of coordination work, chosen by the
/// operator when launching the master) and how hard the hardest worker in the
/// tree may think (a property of the campaign). The practical consequence was
/// that a `high` master could not spawn an `xhigh` review child even at an
/// IDENTICAL model tier, and the only workaround — raising the master — lifted
/// the ceiling for every child in the tree, which is strictly more expensive
/// than the targeted spend actually wanted.
///
/// Semantics: when this key holds a recognized effort name, it REPLACES the
/// tree root's effort as the ceiling — so it can raise the limit for a
/// campaign, and equally can tighten it below the root. When the key is unset
/// (the default) the ceiling remains the tree root's own effort, so behaviour
/// is unchanged from before the key existed.
///
/// The model-TIER ceiling is deliberately NOT overridable by this key. Cost is
/// dominated by tier, and the runaway-escalation protection this guardrail was
/// built for (issue #2) is the tier half; effort within a fixed tier is a much
/// smaller delta. Decoupling effort therefore removes the observed pain while
/// leaving the guardrail that actually bounds spend fully intact.
///
/// Accepted values are the effort names ranked by [`effort_rank`]:
/// `low | medium | high | xhigh | max | ultra` (leading/trailing whitespace and
/// case are normalized), plus the explicit
/// [`rsi_common::model_control::ORCHESTRATION_MAX_CHILD_EFFORT_UNSET`]
/// sentinel. Any other value — a typo,
/// or a value of a SQLite storage class other than TEXT — is treated as unset
/// rather than as a rank-0 ceiling, because a rank-0 ceiling would deny every
/// child that named any effort at all: a malformed setting must not become a
/// tree-wide outage.
///
/// Since issue #35 this key has an operator surface and is no longer settable
/// only by hand-written SQL: it round-trips through `GetDaemonConfig` /
/// `UpdateDaemonConfig` (validated against
/// [`rsi_common::model_control::ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES`] on
/// write) and is presented in the TUI settings pane under Daemon features.
pub const KEY_ORCHESTRATION_MAX_CHILD_EFFORT: &str = "orchestration_max_child_effort";

const COUNTER_ALL: &str = "__all__";
const DEFAULT_CIRCUIT_COOLDOWN_SECS: u64 = 300;
const TRANSIENT_CIRCUIT_WINDOW_SECS: i64 = 120;
const TRANSIENT_CIRCUIT_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreAdmissionOutcome {
    Admitted(Uuid),
    Duplicate(Uuid),
    Denied {
        invocation_id: Uuid,
        inserted: bool,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CapacityStoreAdmissionOutcome {
    Admitted(Uuid),
    Duplicate(super::capacity_recovery::CapacityDeliveryReceipt),
    Denied {
        invocation_id: Uuid,
        inserted: bool,
        reason: String,
    },
}

enum InternalStoreAdmissionOutcome {
    Admitted(Uuid),
    Duplicate(Uuid),
    CapacityDuplicate(super::capacity_recovery::CapacityDeliveryReceipt),
    Denied {
        invocation_id: Uuid,
        inserted: bool,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreAdmissionChannel {
    Generic,
    ScheduledCapacity,
}

/// Why the orchestration tier/effort guardrail would refuse a not-yet-admitted
/// invocation. Produced only by [`Store::preview_orchestration_escalation`],
/// which asks `enforce_orchestration_tier_escalation` — the same authority
/// `admit_model_invocation` consults — and then reads the root row purely to
/// fill in reportable detail. These fields never participate in the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrchestrationEscalationDenial {
    /// Requested child tier, in `model_invocations.model_tier` spelling.
    pub requested_tier: String,
    /// Requested child effort, verbatim (`None` = unspecified).
    pub requested_effort: Option<String>,
    /// Tree-root tier that capped the request.
    pub root_tier: Option<String>,
    /// Tree-root effort that capped the request.
    pub root_effort: Option<String>,
    /// The authority's own `DaemonError::PolicyDenied` message, verbatim —
    /// this is what distinguishes a tier denial from an effort denial without
    /// re-implementing the ranking.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StoreCompletionOutcome {
    Missing,
    NoChange,
    Transitioned {
        record: ModelInvocationRecord,
        circuit_transition: Option<ModelCircuitStatus>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum StoreCancellationOutcome {
    Missing,
    NoChange(ModelInvocationRecord),
    Requested(ModelInvocationRecord),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelControlPolicyTransition {
    pub previous_mode: ModelControlMode,
    pub current_mode: ModelControlMode,
    pub updated_at: String,
    pub mode_changed: bool,
    pub circuits: Vec<ModelCircuitStatus>,
    pub circuit_transitions: Vec<ModelCircuitStatus>,
}

#[derive(Debug, Clone)]
struct ReservationUsage {
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    reasoning_tokens: i64,
    embedding_input_count: i64,
    wall_time_ms: i64,
}

#[derive(Debug, Clone, Copy)]
struct CounterDelta {
    calls: i64,
    active: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
    reasoning_tokens: i64,
    embedding_inputs: i64,
    wall_time_ms: i64,
}

#[derive(Debug, Clone)]
struct CounterPolicy {
    max_calls: Option<i64>,
    max_concurrency: Option<i64>,
    max_total_tokens: Option<i64>,
    max_input_tokens: Option<i64>,
    max_output_tokens: Option<i64>,
    max_cache_creation_tokens: Option<i64>,
    max_cache_read_tokens: Option<i64>,
    max_reasoning_tokens: Option<i64>,
    max_embedding_inputs: Option<i64>,
    max_wall_time_ms: Option<i64>,
    max_retries: Option<i64>,
    max_calls_per_window: Option<i64>,
    rate_window_seconds: Option<i64>,
    ceiling_model_tier: Option<String>,
    ceiling_effort: Option<String>,
    counter_purpose: String,
    counter_model_tier: String,
    counter_effort: String,
}

#[derive(Debug)]
struct ExistingDedupRow {
    id: Uuid,
    admission_status: String,
    /// Execution status, distinct from `admission_status`. An invocation can be
    /// `admitted` yet have terminally failed; the dedup short-circuit must not
    /// treat that as work already done.
    status: String,
    purpose: String,
    request_fingerprint: Option<String>,
    session_id: Option<String>,
    project_id: Option<String>,
    workflow_id: Option<String>,
    scheduled_job_id: Option<String>,
    issue_tracker_id: Option<String>,
    issue_identifier: Option<String>,
    topology_node_id: Option<String>,
    recursive_graph_id: Option<String>,
    recursive_task_id: Option<String>,
    recursive_attempt_id: Option<String>,
    operator: Option<String>,
}

impl Store {
    pub fn current_model_control_mode(&self) -> Result<ModelControlMode> {
        let Some(raw) = self.get_daemon_setting(KEY_MODEL_CONTROL_MODE)? else {
            return Ok(ModelControlMode::Normal);
        };
        parse_mode(&raw)
    }

    pub fn current_model_control_mode_with_updated_at(
        &self,
    ) -> Result<(ModelControlMode, Option<String>)> {
        let row = self
            .conn
            .query_row(
                "SELECT value, updated_at FROM daemon_settings WHERE key = ?1",
                params![KEY_MODEL_CONTROL_MODE],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        match row {
            Some((raw, updated_at)) => Ok((parse_mode(&raw)?, Some(updated_at))),
            None => Ok((ModelControlMode::Normal, None)),
        }
    }

    pub fn set_model_control_mode(
        &self,
        mode: ModelControlMode,
    ) -> Result<(ModelControlMode, String)> {
        let previous = self.current_model_control_mode()?;
        let updated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        self.conn.execute(
            "INSERT INTO daemon_settings (key, value, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![KEY_MODEL_CONTROL_MODE, to_mode(mode), updated_at],
        )?;
        Ok((previous, updated_at))
    }

    pub fn list_model_budget_policies(&self) -> Result<Vec<ModelBudgetPolicy>> {
        let mut stmt = self.conn.prepare(
            "SELECT scope_kind, scope_id, purpose, model_tier, effort,
                    max_calls, max_total_tokens, max_input_tokens, max_output_tokens,
                    max_embedding_inputs, max_wall_time_ms, max_concurrency, max_retries,
                    alert_threshold_ratio, ceiling_model_tier, ceiling_effort,
                    max_cache_creation_tokens, max_cache_read_tokens, max_reasoning_tokens,
                    max_calls_per_window, rate_window_seconds
             FROM model_budget_policies
             ORDER BY scope_kind, scope_id, purpose, model_tier, effort",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ModelBudgetPolicy {
                scope_kind: parse_budget_scope_kind(&row.get::<_, String>(0)?)
                    .map_err(err_to_sql_err)?,
                scope_id: normalize_counter_scope_id(row.get(1)?),
                purpose: row
                    .get::<_, Option<String>>(2)?
                    .map(|raw| parse_purpose(&raw).map_err(err_to_sql_err))
                    .transpose()?,
                model_tier: row
                    .get::<_, Option<String>>(3)?
                    .map(|raw| parse_model_tier(&raw).map_err(err_to_sql_err))
                    .transpose()?,
                effort: normalize_counter_scope_id(row.get(4)?),
                max_calls: row
                    .get::<_, Option<i64>>(5)?
                    .map(|value| value.max(0) as u64),
                max_total_tokens: row
                    .get::<_, Option<i64>>(6)?
                    .map(|value| value.max(0) as u64),
                max_input_tokens: row
                    .get::<_, Option<i64>>(7)?
                    .map(|value| value.max(0) as u64),
                max_output_tokens: row
                    .get::<_, Option<i64>>(8)?
                    .map(|value| value.max(0) as u64),
                max_embedding_inputs: row
                    .get::<_, Option<i64>>(9)?
                    .map(|value| value.max(0) as u64),
                max_wall_time_ms: row
                    .get::<_, Option<i64>>(10)?
                    .map(|value| value.max(0) as u64),
                max_concurrency: row
                    .get::<_, Option<i64>>(11)?
                    .map(|value| value.max(0) as u32),
                max_retries: row
                    .get::<_, Option<i64>>(12)?
                    .map(|value| value.max(0) as u32),
                alert_threshold_ratio: row.get(13)?,
                ceiling_model_tier: row
                    .get::<_, Option<String>>(14)?
                    .map(|raw| parse_model_tier(&raw).map_err(err_to_sql_err))
                    .transpose()?,
                ceiling_effort: normalize_counter_scope_id(row.get(15)?),
                max_cache_creation_tokens: row
                    .get::<_, Option<i64>>(16)?
                    .map(|value| value.max(0) as u64),
                max_cache_read_tokens: row
                    .get::<_, Option<i64>>(17)?
                    .map(|value| value.max(0) as u64),
                max_reasoning_tokens: row
                    .get::<_, Option<i64>>(18)?
                    .map(|value| value.max(0) as u64),
                max_calls_per_window: row
                    .get::<_, Option<i64>>(19)?
                    .map(|value| value.max(0) as u64),
                rate_window_seconds: row
                    .get::<_, Option<i64>>(20)?
                    .map(|value| value.max(0) as u64),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn list_model_circuits(&self) -> Result<Vec<ModelCircuitStatus>> {
        let Some(raw) = self.get_daemon_setting(KEY_MODEL_CONTROL_CIRCUITS)? else {
            return Ok(Vec::new());
        };
        serde_json::from_str::<Vec<ModelCircuitStatus>>(&raw).map_err(|error| {
            DaemonError::Store(format!("invalid model_control_circuits payload: {error}"))
        })
    }

    pub fn update_model_budget_policies(
        &self,
        policies: &[ModelBudgetPolicy],
        replace: bool,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        update_model_budget_policies_tx(&tx, policies, replace)?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_model_circuits(&self, circuits: &[ModelCircuitStatus]) -> Result<()> {
        for circuit in circuits {
            validate_model_circuit_status(circuit)?;
        }
        self.set_daemon_setting(
            KEY_MODEL_CONTROL_CIRCUITS,
            &serde_json::to_string(circuits).map_err(|error| {
                DaemonError::Store(format!("serialize model circuits: {error}"))
            })?,
        )
    }

    pub fn record_budget_alert_crossings(
        &self,
        invocation_id: Uuid,
    ) -> Result<Vec<ModelBudgetAlert>> {
        let Some(record) = self.load_model_invocation_record(invocation_id)? else {
            return Ok(Vec::new());
        };
        let alerts = self.list_budget_alerts_for_record(&record)?;
        let tx = self.conn.unchecked_transaction()?;
        let emitted_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let mut inserted = Vec::new();
        for alert in alerts {
            let changed = tx.execute(
                "INSERT INTO model_budget_alert_events (
                    invocation_id, scope_kind, scope_id, purpose, metric,
                    remaining, limit_value, threshold, emitted_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(invocation_id, scope_kind, scope_id, purpose, metric, threshold)
                DO NOTHING",
                params![
                    alert.invocation_id.to_string(),
                    to_budget_scope_kind(alert.scope_kind),
                    alert.scope_id.as_deref().unwrap_or(COUNTER_ALL),
                    alert
                        .purpose
                        .map(ModelInvocationPurpose::as_str)
                        .unwrap_or(COUNTER_ALL),
                    alert.metric,
                    alert.remaining,
                    alert.limit,
                    alert.threshold,
                    emitted_at,
                ],
            )?;
            if changed != 0 {
                inserted.push(alert);
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn list_recent_budget_alert_events(&self, limit: u32) -> Result<Vec<ModelBudgetAlert>> {
        let mut stmt = self.conn.prepare(
            "SELECT invocation_id, scope_kind, scope_id, purpose, metric,
                    remaining, limit_value, threshold
             FROM model_budget_alert_events
             ORDER BY emitted_at DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![i64::from(limit)], parse_budget_alert_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn list_budget_alert_events_for_invocation(
        &self,
        invocation_id: Uuid,
    ) -> Result<Vec<ModelBudgetAlert>> {
        let mut stmt = self.conn.prepare(
            "SELECT invocation_id, scope_kind, scope_id, purpose, metric,
                    remaining, limit_value, threshold
             FROM model_budget_alert_events
             WHERE invocation_id = ?1
             ORDER BY emitted_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![invocation_id.to_string()], parse_budget_alert_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn load_model_invocation_record(
        &self,
        invocation_id: Uuid,
    ) -> Result<Option<ModelInvocationRecord>> {
        self.conn
            .query_row(
                MODEL_INVOCATION_SELECT_BY_ID,
                params![invocation_id.to_string()],
                parse_model_invocation_record_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_model_invocation_records(
        &self,
        limit: u32,
        active_only: bool,
        purpose: Option<ModelInvocationPurpose>,
        session_id: Option<Uuid>,
    ) -> Result<Vec<ModelInvocationRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                provider, model, backend, model_tier, effort, trigger_source,
                session_id, project_id, workflow_id, scheduled_job_id, issue_tracker_id,
                issue_identifier, topology_node_id, recursive_graph_id, recursive_task_id,
                recursive_attempt_id, operator, parent_invocation_id, retry_of_invocation_id,
                dedup_key, request_fingerprint, policy_snapshot_json, error_class,
                cancellation_requested_at, cancellation_reason, cancellation_mechanism,
                created_at, started_at, completed_at,
                input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                reasoning_tokens, embedding_input_count, wall_time_ms, estimated_cost_usd,
                usage_confidence, baseline_input_tokens, baseline_output_tokens,
                baseline_cache_creation_tokens, baseline_cache_read_tokens,
                baseline_reasoning_tokens, baseline_embedding_input_count, baseline_wall_time_ms
             FROM model_invocations
             WHERE (?1 = 0 OR status IN ('running', 'cancellation_requested'))
               AND (?2 IS NULL OR purpose = ?2)
               AND (?3 IS NULL OR session_id = ?3)
             ORDER BY
               CASE WHEN status IN ('running', 'cancellation_requested') THEN 0 ELSE 1 END,
               created_at DESC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                if active_only { 1 } else { 0 },
                purpose.map(ModelInvocationPurpose::as_str),
                session_id.map(|id| id.to_string()),
                i64::from(limit),
            ],
            parse_model_invocation_record_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn list_cancelable_model_invocation_records_batch(
        &self,
        limit: u32,
        after_id: Option<&str>,
    ) -> Result<Vec<ModelInvocationRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                provider, model, backend, model_tier, effort, trigger_source,
                session_id, project_id, workflow_id, scheduled_job_id, issue_tracker_id,
                issue_identifier, topology_node_id, recursive_graph_id, recursive_task_id,
                recursive_attempt_id, operator, parent_invocation_id, retry_of_invocation_id,
                dedup_key, request_fingerprint, policy_snapshot_json, error_class,
                cancellation_requested_at, cancellation_reason, cancellation_mechanism,
                created_at, started_at, completed_at,
                input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                reasoning_tokens, embedding_input_count, wall_time_ms, estimated_cost_usd,
                usage_confidence, baseline_input_tokens, baseline_output_tokens,
                baseline_cache_creation_tokens, baseline_cache_read_tokens,
                baseline_reasoning_tokens, baseline_embedding_input_count, baseline_wall_time_ms
             FROM model_invocations
             WHERE status IN ('running', 'cancellation_requested')
               AND (?1 IS NULL OR id > ?1)
             ORDER BY id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![after_id, i64::from(limit)],
            parse_model_invocation_record_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn apply_model_control_policy_transition(
        &self,
        mode: ModelControlMode,
        replace_policies: bool,
        policies: &[ModelBudgetPolicy],
        circuit_updates: &[rsi_common::rpc::ModelCircuitUpdate],
    ) -> Result<ModelControlPolicyTransition> {
        let tx = self.conn.unchecked_transaction()?;
        if replace_policies || !policies.is_empty() {
            update_model_budget_policies_tx(&tx, policies, replace_policies)?;
        }

        let previous_mode = current_model_control_mode_tx(&tx)?;
        let updated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let mode_changed = previous_mode != mode;
        set_model_control_mode_tx(&tx, mode, &updated_at)?;

        let mut circuits = load_model_circuits_tx(&tx)?;
        let mut circuit_transitions = Vec::new();
        for update in circuit_updates {
            let scope_id = match update.scope_kind {
                BudgetScopeKind::Global => None,
                _ => update.scope_id.clone(),
            };
            let key_matches = |circuit: &ModelCircuitStatus| {
                circuit.scope_kind == update.scope_kind && circuit.scope_id == scope_id
            };
            let state = update.state.trim();
            if state != "open" && state != "closed" {
                return Err(DaemonError::InvalidParam(format!(
                    "unsupported circuit state update: {state}"
                )));
            }
            let entry = if let Some(index) = circuits.iter().position(key_matches) {
                &mut circuits[index]
            } else {
                circuits.push(ModelCircuitStatus {
                    scope_kind: update.scope_kind,
                    scope_id: scope_id.clone(),
                    state: "closed".to_string(),
                    reason: "healthy".to_string(),
                    error_class: None,
                    source: "operator_reset".to_string(),
                    opened_at: None,
                    updated_at: updated_at.clone(),
                    reset_at: Some(updated_at.clone()),
                    cooldown_secs: update.cooldown_secs.or(Some(DEFAULT_CIRCUIT_COOLDOWN_SECS)),
                    probe_after: None,
                    trip_count: 0,
                    transient_failure_count: 0,
                    transient_window_started_at: None,
                    probe_invocation_id: None,
                    probe_lease_started_at: None,
                });
                circuits.last_mut().expect("pushed circuit entry exists")
            };
            if state == "closed" {
                close_circuit_entry(
                    entry,
                    &updated_at,
                    if update.reason.trim().is_empty() {
                        "operator_reset"
                    } else {
                        &update.reason
                    },
                    "operator_reset",
                );
            } else {
                entry.cooldown_secs = update.cooldown_secs.or(entry.cooldown_secs);
                reopen_circuit_entry(
                    entry,
                    &updated_at,
                    &update.reason,
                    update.error_class.as_deref(),
                    "operator_manual_open",
                );
            }
            circuit_transitions.push(entry.clone());
        }
        set_model_circuits_tx(&tx, &circuits)?;
        tx.commit()?;
        Ok(ModelControlPolicyTransition {
            previous_mode,
            current_mode: mode,
            updated_at,
            mode_changed,
            circuits,
            circuit_transitions,
        })
    }

    pub fn request_model_invocation_cancellation(
        &self,
        invocation_id: Uuid,
        reason: &str,
        mechanism: &str,
    ) -> Result<StoreCancellationOutcome> {
        let tx = self.conn.unchecked_transaction()?;
        let record = tx
            .query_row(
                MODEL_INVOCATION_SELECT_BY_ID,
                params![invocation_id.to_string()],
                parse_model_invocation_record_row,
            )
            .optional()?;
        let Some(record) = record else {
            tx.commit()?;
            return Ok(StoreCancellationOutcome::Missing);
        };
        if record.admission_status != AdmissionStatus::Admitted
            || !matches!(
                record.status,
                ModelInvocationStatus::Running | ModelInvocationStatus::CancellationRequested
            )
        {
            tx.commit()?;
            return Ok(StoreCancellationOutcome::NoChange(record));
        }
        if record.status == ModelInvocationStatus::CancellationRequested {
            tx.commit()?;
            return Ok(StoreCancellationOutcome::NoChange(record));
        }

        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        tx.execute(
            "UPDATE model_invocations
             SET status = 'cancellation_requested',
                 cancellation_requested_at = COALESCE(cancellation_requested_at, ?1),
                 cancellation_reason = COALESCE(cancellation_reason, ?2),
                 cancellation_mechanism = COALESCE(cancellation_mechanism, ?3)
             WHERE id = ?4
               AND admission_status = 'admitted'
               AND status = 'running'",
            params![now, reason, mechanism, invocation_id.to_string()],
        )?;
        let record = tx
            .query_row(
                MODEL_INVOCATION_SELECT_BY_ID,
                params![invocation_id.to_string()],
                parse_model_invocation_record_row,
            )
            .optional()?;
        tx.commit()?;
        Ok(match record {
            Some(record) => StoreCancellationOutcome::Requested(record),
            None => StoreCancellationOutcome::Missing,
        })
    }

    pub fn build_model_invocation_list(
        &self,
        limit: u32,
        active_only: bool,
        purpose: Option<ModelInvocationPurpose>,
        session_id: Option<Uuid>,
    ) -> Result<ModelInvocationList> {
        let invocations = self
            .list_model_invocation_records(limit, active_only, purpose, session_id)?
            .into_iter()
            .map(|record| self.build_invocation_view(record))
            .collect::<Result<Vec<_>>>()?;
        Ok(ModelInvocationList { invocations })
    }

    pub fn build_model_control_status(
        &self,
        recent_limit: u32,
    ) -> Result<ModelControlStatusReport> {
        let (mode, mode_updated_at) = self.current_model_control_mode_with_updated_at()?;
        let active_invocations = self
            .list_model_invocation_records(recent_limit, true, None, None)?
            .into_iter()
            .map(|record| self.build_invocation_view(record))
            .collect::<Result<Vec<_>>>()?;
        let recent_records = self.list_model_invocation_records(recent_limit, false, None, None)?;
        let mut recent_invocations = Vec::new();
        let mut recent_denials = Vec::new();
        let circuits = self.list_model_circuits()?;
        let policies = self.list_model_budget_policies()?;
        let (circuit_state, circuit_reason) = effective_circuit_summary(mode, &circuits);
        for record in recent_records {
            let view = self.build_invocation_view(record.clone())?;
            if record.admission_status == AdmissionStatus::Denied {
                recent_denials.push(view.clone());
            }
            recent_invocations.push(view);
        }
        Ok(ModelControlStatusReport {
            mode,
            mode_updated_at,
            restart_required_fields: Vec::new(),
            circuit_state,
            circuit_reason,
            circuits,
            policies,
            active_invocations,
            recent_invocations,
            recent_denials,
            recent_budget_alerts: self.list_recent_budget_alert_events(recent_limit)?,
        })
    }

    pub fn budget_alerts_for_invocation(
        &self,
        invocation_id: Uuid,
    ) -> Result<Vec<ModelBudgetAlert>> {
        self.list_budget_alert_events_for_invocation(invocation_id)
    }

    pub fn admit_model_invocation(
        &self,
        invocation_id: Uuid,
        registry: RegistryEntry,
        model_tier: ModelTier,
        request: &ModelAdmissionRequest,
    ) -> Result<StoreAdmissionOutcome> {
        self.admit_model_invocation_with_launch_origin(
            invocation_id,
            registry,
            model_tier,
            request,
            None,
        )
    }

    pub(crate) fn admit_model_invocation_with_launch_origin(
        &self,
        invocation_id: Uuid,
        registry: RegistryEntry,
        model_tier: ModelTier,
        request: &ModelAdmissionRequest,
        origin: Option<&super::manager_resources::ManagerResourceLaunchOrigin>,
    ) -> Result<StoreAdmissionOutcome> {
        match self.admit_model_invocation_for_channel(
            invocation_id,
            registry,
            model_tier,
            request,
            StoreAdmissionChannel::Generic,
            origin,
        )? {
            InternalStoreAdmissionOutcome::Admitted(id) => Ok(StoreAdmissionOutcome::Admitted(id)),
            InternalStoreAdmissionOutcome::Duplicate(id) => {
                Ok(StoreAdmissionOutcome::Duplicate(id))
            }
            InternalStoreAdmissionOutcome::Denied {
                invocation_id,
                inserted,
                reason,
            } => Ok(StoreAdmissionOutcome::Denied {
                invocation_id,
                inserted,
                reason,
            }),
            InternalStoreAdmissionOutcome::CapacityDuplicate(_) => Err(DaemonError::Store(
                "generic_admission_received_capacity_receipt".into(),
            )),
        }
    }

    pub(crate) fn admit_scheduled_capacity_model_invocation(
        &self,
        invocation_id: Uuid,
        registry: RegistryEntry,
        model_tier: ModelTier,
        request: &ModelAdmissionRequest,
    ) -> Result<CapacityStoreAdmissionOutcome> {
        match self.admit_model_invocation_for_channel(
            invocation_id,
            registry,
            model_tier,
            request,
            StoreAdmissionChannel::ScheduledCapacity,
            None,
        )? {
            InternalStoreAdmissionOutcome::Admitted(id) => {
                Ok(CapacityStoreAdmissionOutcome::Admitted(id))
            }
            InternalStoreAdmissionOutcome::CapacityDuplicate(receipt) => {
                Ok(CapacityStoreAdmissionOutcome::Duplicate(receipt))
            }
            InternalStoreAdmissionOutcome::Denied {
                invocation_id,
                inserted,
                reason,
            } => Ok(CapacityStoreAdmissionOutcome::Denied {
                invocation_id,
                inserted,
                reason,
            }),
            InternalStoreAdmissionOutcome::Duplicate(_) => Err(DaemonError::Store(
                "capacity_admission_duplicate_without_receipt".into(),
            )),
        }
    }

    fn admit_model_invocation_for_channel(
        &self,
        invocation_id: Uuid,
        registry: RegistryEntry,
        model_tier: ModelTier,
        request: &ModelAdmissionRequest,
        channel: StoreAdmissionChannel,
        origin: Option<&super::manager_resources::ManagerResourceLaunchOrigin>,
    ) -> Result<InternalStoreAdmissionOutcome> {
        // Reserve the writer before reading scoped usage. A deferred read-to-
        // write upgrade can fail BUSY instead of observing the other admission.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let root_claim = origin.and_then(|o| o.manager_succession_claim());
        if let Some(claim) = root_claim {
            let root = self.manager_succession_effect_gate_on(claim)?;
            if invocation_id != root.model_invocation_id {
                return Err(super::harness_manager_v2::refused(
                    "manager_succession_invocation_changed",
                ));
            }
        } else if request.trigger == "manager_self_succession"
            || request
                .dedup_key
                .as_deref()
                .is_some_and(|k| k.starts_with("manager.self_succession:"))
        {
            return Err(super::harness_manager_v2::refused(
                "manager_succession_origin_required",
            ));
        }
        let capacity_delivery = match channel {
            StoreAdmissionChannel::Generic => {
                if request.trigger == "scheduled_capacity_resume"
                    || request
                        .dedup_key
                        .as_deref()
                        .is_some_and(|key| key.starts_with("scheduled.resume.capacity:"))
                {
                    return Err(DaemonError::PolicyDenied(
                        "generic_model_admission_rejects_capacity_shape".into(),
                    ));
                }
                None
            }
            StoreAdmissionChannel::ScheduledCapacity => Some(
                super::capacity_recovery::capacity_admission_context_tx(&tx, request)?,
            ),
        };
        if let Some(context) = capacity_delivery.as_ref()
            && let Some(receipt) =
                super::capacity_recovery::matching_delivery_receipt_tx(&tx, context)?
        {
            let Some(dedup_key) = request.dedup_key.as_deref() else {
                return Err(DaemonError::Store(
                    "capacity_delivery_receipt_without_dedup_key".into(),
                ));
            };
            let existing = load_existing_dedup_row_tx(&tx, dedup_key)?.ok_or_else(|| {
                DaemonError::Store("capacity_delivery_receipt_without_invocation_key".into())
            })?;
            if existing.id != receipt.invocation_id
                || !dedup_row_matches_request(&existing, request)
            {
                return Err(DaemonError::PolicyDenied(format!(
                    "capacity delivery dedup conflict for key {dedup_key}"
                )));
            }
            tx.commit()?;
            return Ok(InternalStoreAdmissionOutcome::CapacityDuplicate(receipt));
        }
        if let Some(dedup_key) = request.dedup_key.as_deref()
            && let Some(existing) = load_existing_dedup_row_tx(&tx, dedup_key)?
        {
            if channel == StoreAdmissionChannel::ScheduledCapacity {
                return Err(DaemonError::Store(
                    "capacity_admission_duplicate_without_receipt".into(),
                ));
            }
            // A dedup key claims a *live or completed* attempt. Once an attempt
            // reaches terminal failure it holds no spend and produced no
            // result, so continuing to suppress its key would make identical
            // content permanently un-retryable: the retry recomputes the same
            // content-derived key, matches the dead row, and is rejected. That
            // deadlocked memory-transcript sync indefinitely — every pass died
            // on the same failed batch and no transcript was ever indexed.
            //
            // Release the key (keeping the row for audit — `request_fingerprint`
            // still correlates it) and fall through to a normal admission.
            // `dedup_key` is covered by a UNIQUE partial index, so the slot must
            // be vacated before the retry's row can be inserted.
            if root_claim.is_some() {
                // This occurrence may only be settled/reconciled, never mint a
                // new execution capability through generic dedup-key release.
                return Err(super::harness_manager_v2::refused(
                    "manager_succession_admission_already_exists",
                ));
            }
            if dedup_row_is_retryable(&existing) {
                tx.execute(
                    "UPDATE model_invocations SET dedup_key = NULL WHERE id = ?1",
                    params![existing.id.to_string()],
                )?;
            } else {
                if !dedup_row_matches_request(&existing, request) {
                    return Err(DaemonError::PolicyDenied(format!(
                        "model invocation dedup conflict for key {dedup_key}"
                    )));
                }
                if existing.admission_status == "admitted"
                    && existing.status == "running"
                    && let Some(origin) = origin
                    && let Some(session) = request.owner.session_id
                    && self.get_session(session)?.is_none()
                {
                    let provider = request
                        .provider
                        .as_deref()
                        .and_then(|p| super::row_mappers::str_to_session_provider(p).ok())
                        .ok_or_else(|| {
                            super::harness_manager_v2::refused(
                                "manager_v2_launch_provider_required",
                            )
                        })?;
                    self.manager_v2_replay_launch_origin(origin, request, provider, existing.id)?;
                }
                tx.commit()?;
                return Ok(match existing.admission_status.as_str() {
                    "denied" => InternalStoreAdmissionOutcome::Denied {
                        invocation_id: existing.id,
                        inserted: false,
                        reason: "duplicate denied admission".to_string(),
                    },
                    _ => InternalStoreAdmissionOutcome::Duplicate(existing.id),
                });
            }
        }

        let mode = current_model_control_mode_tx(&tx)?;
        let is_local = model_tier == ModelTier::Local;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let reservation = reservation_from_request(request, registry, model_tier)?;
        let lineage = resolve_lineage_scope_ids_tx(
            &tx,
            invocation_id,
            request.parent_invocation_id,
            request.retry_of_invocation_id,
        )?;
        let tree_scope_id = lineage.tree_scope_id.to_string();
        let retry_scope_id = lineage.retry_scope_id.map(|id| id.to_string());
        let scopes = budget_scopes(
            request,
            request.provider.as_deref(),
            registry.foreground,
            &tree_scope_id,
            retry_scope_id.as_deref(),
        );
        let explicit_usage_budget_applies = has_explicit_usage_budget_policy(
            &tx,
            &scopes,
            request.purpose.as_str(),
            to_model_tier(model_tier),
            request.effort.as_deref(),
        )?;
        let policy_snapshot = serde_json::json!({
            "mode": mode,
            "purpose": request.purpose,
            "model_tier": model_tier,
            "default_background_paid_allowed": registry.background_paid_enabled_by_default,
            "tree_scope_id": lineage.tree_scope_id,
            "retry_scope_id": lineage.retry_scope_id,
            "explicit_usage_budget_applies": explicit_usage_budget_applies,
            "reservation": {
                "input_tokens": reservation.input_tokens,
                "output_tokens": reservation.output_tokens,
                "cache_creation_tokens": reservation.cache_creation_tokens,
                "cache_read_tokens": reservation.cache_read_tokens,
                "reasoning_tokens": reservation.reasoning_tokens,
                "embedding_input_count": reservation.embedding_input_count,
                "wall_time_ms": reservation.wall_time_ms,
            },
        });

        // A selected Epic's manager budget applies to ordinary descendant
        // admission too. The invocation ledger transaction serializes this
        // check with the slot reservation below; manager RPC preflight alone
        // would leave child spawns and existing retry owners outside the cap.
        let launch_provider = request
            .provider
            .as_deref()
            .and_then(|provider| super::row_mappers::str_to_session_provider(provider).ok());
        let mut launch_scope = None;
        let resource_gate = match (origin, launch_provider) {
            (Some(origin), Some(provider)) => self
                .manager_v2_admit_launch_origin(origin, request, provider)
                .map(|scope| {
                    launch_scope = scope;
                }),
            (Some(_), None) => Err(super::harness_manager_v2::refused(
                "manager_v2_launch_provider_required",
            )),
            (None, Some(provider)) => self.manager_v2_resource_admission(request, provider),
            (None, None) => Ok(()),
        };
        if let Err(error) = resource_gate {
            insert_model_invocation_denied_tx(
                &tx,
                invocation_id,
                registry.kind,
                registry.foreground,
                registry.paid_risk,
                model_tier,
                request,
                &policy_snapshot,
                "manager_resource_denied",
                &now,
            )?;
            tx.commit()?;
            return Ok(InternalStoreAdmissionOutcome::Denied {
                invocation_id,
                inserted: true,
                reason: error.to_string(),
            });
        }

        if let Err(error) = enforce_mode(
            mode,
            registry.foreground,
            registry.paid_risk.is_paid_capable(),
            registry.background_paid_enabled_by_default,
            is_local,
            request.purpose,
        ) {
            insert_model_invocation_denied_tx(
                &tx,
                invocation_id,
                registry.kind,
                registry.foreground,
                registry.paid_risk,
                model_tier,
                request,
                &policy_snapshot,
                "policy_denied",
                &now,
            )?;
            tx.commit()?;
            return Ok(InternalStoreAdmissionOutcome::Denied {
                invocation_id,
                inserted: true,
                reason: error.to_string(),
            });
        }

        if let Err(_error) =
            enforce_circuit_policy_tx(&tx, request.provider.as_deref(), invocation_id, &now)
        {
            insert_model_invocation_denied_tx(
                &tx,
                invocation_id,
                registry.kind,
                registry.foreground,
                registry.paid_risk,
                model_tier,
                request,
                &policy_snapshot,
                "circuit_open",
                &now,
            )?;
            tx.commit()?;
            return Ok(InternalStoreAdmissionOutcome::Denied {
                invocation_id,
                inserted: true,
                reason: "provider circuit is open".to_string(),
            });
        }

        if let Err(error) = enforce_orchestration_tier_escalation(
            &tx,
            registry,
            model_tier,
            request.effort.as_deref(),
            lineage.tree_scope_id,
        ) {
            insert_model_invocation_denied_tx(
                &tx,
                invocation_id,
                registry.kind,
                registry.foreground,
                registry.paid_risk,
                model_tier,
                request,
                &policy_snapshot,
                "orchestration_escalation_denied",
                &now,
            )?;
            tx.commit()?;
            return Ok(InternalStoreAdmissionOutcome::Denied {
                invocation_id,
                inserted: true,
                reason: error.to_string(),
            });
        }

        let delta = CounterDelta {
            calls: 1,
            active: 1,
            input_tokens: reservation.input_tokens,
            output_tokens: reservation.output_tokens,
            cache_creation_tokens: reservation.cache_creation_tokens,
            cache_read_tokens: reservation.cache_read_tokens,
            reasoning_tokens: reservation.reasoning_tokens,
            embedding_inputs: reservation.embedding_input_count,
            wall_time_ms: reservation.wall_time_ms,
        };
        for scope in &scopes {
            if let Err(_error) = enforce_counter_policies_for_delta(
                &tx,
                &scope.0,
                &scope.1,
                request.purpose.as_str(),
                to_model_tier(model_tier),
                request.effort.as_deref(),
                delta,
            ) {
                insert_model_invocation_denied_tx(
                    &tx,
                    invocation_id,
                    registry.kind,
                    registry.foreground,
                    registry.paid_risk,
                    model_tier,
                    request,
                    &policy_snapshot,
                    "budget_denied",
                    &now,
                )?;
                tx.commit()?;
                return Ok(InternalStoreAdmissionOutcome::Denied {
                    invocation_id,
                    inserted: true,
                    reason: "budget policy denied admission".to_string(),
                });
            }
        }

        if capacity_delivery.is_some() {
            super::capacity_recovery::admission_fault("before_invocation_insert")?;
        }
        tx.execute(
            "INSERT INTO model_invocations (
                id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                provider, model, backend, model_tier, effort, trigger_source,
                session_id, project_id, workflow_id, scheduled_job_id, issue_tracker_id,
                issue_identifier, topology_node_id, recursive_graph_id, recursive_task_id,
                recursive_attempt_id, operator, parent_invocation_id, retry_of_invocation_id,
                dedup_key, request_fingerprint, policy_snapshot_json,
                reserved_input_tokens, reserved_output_tokens,
                reserved_cache_creation_tokens, reserved_cache_read_tokens,
                reserved_reasoning_tokens, reserved_embedding_input_count, reserved_wall_time_ms,
                baseline_input_tokens, baseline_output_tokens,
                baseline_cache_creation_tokens, baseline_cache_read_tokens,
                baseline_reasoning_tokens, baseline_embedding_input_count, baseline_wall_time_ms,
                created_at, started_at
            ) VALUES (
                ?, ?, ?, ?, ?, 'admitted', 'running',
                ?, ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?,
                ?, ?, ?, ?,
                ?, ?, ?, ?,
                ?, ?, ?,
                ?, ?, ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?, ?, ?,
                ?, ?
            )",
            params![
                invocation_id.to_string(),
                request.purpose.as_str(),
                to_kind(registry.kind),
                to_foreground(registry.foreground),
                to_paid_risk(registry.paid_risk),
                request.provider.as_deref(),
                request.model.as_deref(),
                request.backend.as_deref(),
                to_model_tier(model_tier),
                request.effort.as_deref(),
                request.trigger,
                request.owner.session_id.map(|id| id.to_string()),
                request.owner.project_id.map(|id| id.to_string()),
                request.owner.workflow_id.map(|id| id.to_string()),
                request.owner.scheduled_job_id.map(|id| id.to_string()),
                request.owner.issue_tracker_id.as_deref(),
                request.owner.issue_identifier.as_deref(),
                request.owner.topology_node_id.as_deref(),
                request.owner.recursive_graph_id.as_deref(),
                request.owner.recursive_task_id.as_deref(),
                request.owner.recursive_attempt_id.as_deref(),
                request.owner.operator.as_deref(),
                request.parent_invocation_id.map(|id| id.to_string()),
                request.retry_of_invocation_id.map(|id| id.to_string()),
                request.dedup_key.as_deref(),
                request.request_fingerprint.as_deref(),
                policy_snapshot.to_string(),
                reservation.input_tokens,
                reservation.output_tokens,
                reservation.cache_creation_tokens,
                reservation.cache_read_tokens,
                reservation.reasoning_tokens,
                reservation.embedding_input_count,
                reservation.wall_time_ms,
                request.baseline_input_tokens as i64,
                request.baseline_output_tokens as i64,
                request.baseline_cache_creation_tokens as i64,
                request.baseline_cache_read_tokens as i64,
                request.baseline_reasoning_tokens as i64,
                request.baseline_embedding_input_count as i64,
                request.baseline_wall_time_ms as i64,
                now,
                now,
            ],
        )?;
        if let (Some(origin), Some((config, epic)), Some(provider)) =
            (origin, launch_scope, launch_provider)
        {
            self.manager_v2_record_launch_origin(origin, &config, epic, provider, invocation_id)?;
        }
        if let Some(context) = capacity_delivery.as_ref() {
            super::capacity_recovery::admission_fault("after_invocation_insert")?;
            super::capacity_recovery::admission_fault("before_receipt_insert")?;
            super::capacity_recovery::insert_delivery_receipt_tx(
                &tx,
                context,
                invocation_id,
                &now,
            )?;
            super::capacity_recovery::admission_fault("after_receipt_insert")?;
        }

        for scope in &scopes {
            apply_counter_increment(
                &tx,
                &scope.0,
                &scope.1,
                request.purpose.as_str(),
                to_model_tier(model_tier),
                request.effort.as_deref(),
                delta,
            )?;
        }
        tx.commit()?;
        Ok(InternalStoreAdmissionOutcome::Admitted(invocation_id))
    }

    /// Read-only preview of the orchestration tier/effort guardrail for an
    /// invocation that has NOT been admitted yet.
    ///
    /// Calls `enforce_orchestration_tier_escalation` — the identical function
    /// `admit_model_invocation` calls at line 774 — so the two can never drift.
    /// This does not insert, count, reserve, or audit anything: the transaction
    /// exists only because the private lineage helpers are typed on
    /// `&rusqlite::Transaction`, and it is dropped (rolled back) on return.
    ///
    /// Contract:
    /// - `Ok(None)`            → allowed (non-orchestration purpose, no lineage
    ///                           root row, or ranks within the root's limits)
    /// - `Ok(Some(denial))`    → the authority denied; fields are report-only
    /// - `Err(PolicyDenied)`   → lineage itself is unresolvable (cycle / depth /
    ///                           missing ancestor / conflicting roots). Deterministic:
    ///                           `admit_model_invocation` will fail identically.
    /// - `Err(other)`          → I/O or parse failure. Callers must fail OPEN: the
    ///                           authoritative gate still runs later.
    pub(crate) fn preview_orchestration_escalation(
        &self,
        purpose: ModelInvocationPurpose,
        model_tier: ModelTier,
        effort: Option<&str>,
        parent_invocation_id: Option<Uuid>,
    ) -> Result<Option<OrchestrationEscalationDenial>> {
        let Some(registry) = crate::model_control::registry::lookup(purpose) else {
            // Unregistered purpose: let `admit_invocation` raise the real
            // InvalidParam later. A precheck must never invent a denial.
            return Ok(None);
        };
        let tx = self.conn.unchecked_transaction()?;
        // `Uuid::nil()` stands in for the not-yet-minted invocation id. It is only
        // consulted when there is no lineage, in which case `resolve_tree_scope_id_tx`
        // returns it unchanged and the root lookup misses — exactly what happens at
        // real admission time, where the row also does not exist yet (this check is
        // at model_control.rs:774; the row INSERT is at :843).
        let tree_scope_id = resolve_tree_scope_id_tx(&tx, Uuid::nil(), parent_invocation_id, None)?;
        match enforce_orchestration_tier_escalation(
            &tx,
            *registry,
            model_tier,
            effort,
            tree_scope_id,
        ) {
            Ok(()) => Ok(None),
            Err(DaemonError::PolicyDenied(detail)) => {
                let (root_tier, root_effort) = tx
                    .query_row(
                        "SELECT model_tier, effort FROM model_invocations WHERE id = ?1",
                        params![tree_scope_id.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                            ))
                        },
                    )
                    .optional()?
                    .unwrap_or((None, None));
                Ok(Some(OrchestrationEscalationDenial {
                    requested_tier: to_model_tier(model_tier).to_string(),
                    requested_effort: effort.map(str::to_string),
                    root_tier,
                    root_effort,
                    detail,
                }))
            }
            Err(other) => Err(other),
        }
    }

    pub fn insert_model_invocation_denied(
        &self,
        invocation_id: Uuid,
        kind: ModelInvocationKind,
        foreground: InvocationForeground,
        paid_risk: PaidRisk,
        model_tier: ModelTier,
        request: &ModelAdmissionRequest,
        policy_snapshot: &serde_json::Value,
        error_class: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        insert_model_invocation_denied_tx(
            &tx,
            invocation_id,
            kind,
            foreground,
            paid_risk,
            model_tier,
            request,
            policy_snapshot,
            error_class,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn complete_model_invocation(
        &self,
        invocation_id: Uuid,
        completion: &InvocationCompletion,
    ) -> Result<StoreCompletionOutcome> {
        self.complete_model_invocation_reconciling_capacity(invocation_id, completion, None)
    }

    fn complete_model_invocation_reconciling_capacity(
        &self,
        invocation_id: Uuid,
        completion: &InvocationCompletion,
        disable_capacity_wake_id: Option<&str>,
    ) -> Result<StoreCompletionOutcome> {
        let tx = if disable_capacity_wake_id.is_some() {
            Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?
        } else {
            self.conn.unchecked_transaction()?
        };
        let outcome = self.complete_model_invocation_in_tx(
            &tx,
            invocation_id,
            completion,
            disable_capacity_wake_id,
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Complete an invocation as part of a caller-owned transaction. Recovery
    /// uses this to commit orphan completion with its durable intent decision.
    pub(crate) fn complete_model_invocation_in_tx(
        &self,
        tx: &Transaction<'_>,
        invocation_id: Uuid,
        completion: &InvocationCompletion,
        disable_capacity_wake_id: Option<&str>,
    ) -> Result<StoreCompletionOutcome> {
        let row = tx
            .query_row(
                "SELECT purpose, foreground, provider, model_tier, effort, session_id, project_id,
                        workflow_id, scheduled_job_id, issue_tracker_id, recursive_graph_id,
                        recursive_task_id, recursive_attempt_id, operator,
                        parent_invocation_id, retry_of_invocation_id, admission_status, status,
                        reserved_input_tokens, reserved_output_tokens, reserved_cache_creation_tokens,
                        reserved_cache_read_tokens, reserved_reasoning_tokens, reserved_embedding_input_count,
                        reserved_wall_time_ms,
                        input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                        reasoning_tokens, embedding_input_count, wall_time_ms, estimated_cost_usd,
                        usage_confidence, baseline_input_tokens, baseline_output_tokens,
                baseline_cache_creation_tokens, baseline_cache_read_tokens,
                baseline_reasoning_tokens, baseline_embedding_input_count, baseline_wall_time_ms,
                        topology_node_id, policy_snapshot_json
                 FROM model_invocations WHERE id = ?1",
                params![invocation_id.to_string()],
                |row| {
                    Ok(InvocationSettlementRow {
                        purpose: row.get(0)?,
                        foreground: row.get(1)?,
                        provider: row.get(2)?,
                        model_tier: row.get(3)?,
                        effort: row.get(4)?,
                        session_id: row.get(5)?,
                        project_id: row.get(6)?,
                        workflow_id: row.get(7)?,
                        scheduled_job_id: row.get(8)?,
                        issue_tracker_id: row.get(9)?,
                        recursive_graph_id: row.get(10)?,
                        recursive_task_id: row.get(11)?,
                        recursive_attempt_id: row.get(12)?,
                        operator: row.get(13)?,
                        parent_invocation_id: row.get(14)?,
                        retry_of_invocation_id: row.get(15)?,
                        admission_status: row.get(16)?,
                        current_status: row.get(17)?,
                        reserved_input_tokens: row.get(18)?,
                        reserved_output_tokens: row.get(19)?,
                        reserved_cache_creation_tokens: row.get(20)?,
                        reserved_cache_read_tokens: row.get(21)?,
                        reserved_reasoning_tokens: row.get(22)?,
                        reserved_embedding_input_count: row.get(23)?,
                        reserved_wall_time_ms: row.get(24)?,
                        current_input_tokens: row.get(25)?,
                        current_output_tokens: row.get(26)?,
                        current_cache_creation_tokens: row.get(27)?,
                        current_cache_read_tokens: row.get(28)?,
                        current_reasoning_tokens: row.get(29)?,
                        current_embedding_input_count: row.get(30)?,
                        current_wall_time_ms: row.get(31)?,
                        current_estimated_cost_usd: row.get(32)?,
                        current_confidence: row.get(33)?,
                        baseline_input_tokens: row.get(34)?,
                        baseline_output_tokens: row.get(35)?,
                        baseline_cache_creation_tokens: row.get(36)?,
                        baseline_cache_read_tokens: row.get(37)?,
                        baseline_reasoning_tokens: row.get(38)?,
                        baseline_embedding_input_count: row.get(39)?,
                        baseline_wall_time_ms: row.get(40)?,
                        topology_node_id: row.get(41)?,
                        policy_snapshot_json: row.get(42)?,
                    })
                },
            )
            .optional()?;
        let Some(row) = row else {
            return Ok(StoreCompletionOutcome::Missing);
        };
        if row.admission_status != "admitted" {
            return Ok(StoreCompletionOutcome::NoChange);
        }
        if !matches!(
            row.current_status.as_str(),
            "running" | "cancellation_requested"
        ) {
            return Ok(StoreCompletionOutcome::NoChange);
        }

        let next_input_tokens = normalize_completion_value(
            completion.input_tokens,
            row.baseline_input_tokens,
            row.current_input_tokens,
        );
        let next_output_tokens = normalize_completion_value(
            completion.output_tokens,
            row.baseline_output_tokens,
            row.current_output_tokens,
        );
        let next_cache_creation_tokens = normalize_completion_value(
            completion.cache_creation_tokens,
            row.baseline_cache_creation_tokens,
            row.current_cache_creation_tokens,
        );
        let next_cache_read_tokens = normalize_completion_value(
            completion.cache_read_tokens,
            row.baseline_cache_read_tokens,
            row.current_cache_read_tokens,
        );
        let next_reasoning_tokens = normalize_completion_value(
            completion.reasoning_tokens,
            row.baseline_reasoning_tokens,
            row.current_reasoning_tokens,
        );
        let next_embedding_input_count = normalize_completion_value(
            completion.embedding_input_count,
            row.baseline_embedding_input_count,
            row.current_embedding_input_count,
        );
        let next_wall_time_ms = normalize_completion_value(
            completion.wall_time_ms,
            row.baseline_wall_time_ms,
            row.current_wall_time_ms,
        );

        let input_delta = counter_delta(next_input_tokens, row.current_input_tokens);
        let output_delta = counter_delta(next_output_tokens, row.current_output_tokens);
        let cache_creation_delta = counter_delta(
            next_cache_creation_tokens,
            row.current_cache_creation_tokens,
        );
        let cache_read_delta = counter_delta(next_cache_read_tokens, row.current_cache_read_tokens);
        let reasoning_delta = counter_delta(next_reasoning_tokens, row.current_reasoning_tokens);
        let embedding_delta = counter_delta(
            next_embedding_input_count,
            row.current_embedding_input_count,
        );
        let wall_time_delta = counter_delta(next_wall_time_ms, row.current_wall_time_ms);
        let settling_running = matches!(
            row.current_status.as_str(),
            "running" | "cancellation_requested"
        );
        let active_delta = if matches!(
            row.current_status.as_str(),
            "running" | "cancellation_requested"
        ) {
            -1
        } else {
            0
        };
        // Cache-read tokens are deliberately NOT a breaker dimension: every
        // provider turn with a warm prompt cache reports large cache-read
        // counts against a 0-token default reservation (default_expected_usage
        // reserves no cache reads), so including them trips the
        // actual_over_reservation breaker on the first settled turn of any
        // real session. Cache-read *cost* is still governed by the budget
        // policy caps (max_cache_read_tokens / max_total_tokens) at admission
        // time, and the settlement delta below still charges the counters.
        let reservation_exceeded = settling_running
            && [
                next_input_tokens.unwrap_or(0) > row.reserved_input_tokens,
                next_output_tokens.unwrap_or(0) > row.reserved_output_tokens,
                next_cache_creation_tokens.unwrap_or(0) > row.reserved_cache_creation_tokens,
                next_reasoning_tokens.unwrap_or(0) > row.reserved_reasoning_tokens,
                next_embedding_input_count.unwrap_or(0) > row.reserved_embedding_input_count,
                next_wall_time_ms.unwrap_or(0) > row.reserved_wall_time_ms,
            ]
            .into_iter()
            .any(|value| value);
        // Default estimates reserve counter headroom; they are not limits. In
        // the unbounded-by-default mode, an estimate miss is expected for
        // tool-heavy work and must not fail the completed session or globally
        // switch the daemon to deny_paid. Preserve the existing fail-closed
        // behaviour only when this invocation actually matched an explicit
        // usage budget at admission.
        let over_budget_actual = reservation_exceeded
            && policy_snapshot_has_explicit_usage_budget(&row.policy_snapshot_json)
                // Invocations admitted before this change have no snapshot
                // marker. Retain their prior fail-closed settlement semantics
                // rather than silently weakening an in-flight explicit budget.
                .unwrap_or(true);
        let effective_error_class = if over_budget_actual && completion.error_class.is_none() {
            Some("over_budget_actual_exceeded")
        } else {
            completion.error_class.as_deref()
        };

        let next_status = if row.current_status == "cancellation_requested"
            && is_cancellation_terminal_error(completion.error_class.as_deref())
        {
            "cancelled"
        } else if effective_error_class.is_some() {
            "failed"
        } else if matches!(
            row.current_status.as_str(),
            "running" | "cancellation_requested"
        ) {
            "completed"
        } else {
            row.current_status.as_str()
        };
        let next_confidence = completion
            .confidence
            .map(to_confidence)
            .unwrap_or(row.current_confidence.as_str());
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        tx.execute(
            "UPDATE model_invocations
             SET status = ?1,
                 completed_at = CASE WHEN completed_at IS NULL THEN ?2 ELSE completed_at END,
                 input_tokens = ?3,
                 output_tokens = ?4,
                 cache_creation_tokens = ?5,
                 cache_read_tokens = ?6,
                 reasoning_tokens = ?7,
                 embedding_input_count = ?8,
                 wall_time_ms = ?9,
                 estimated_cost_usd = ?10,
                error_class = COALESCE(?11, error_class),
                 usage_confidence = ?12
             WHERE id = ?13",
            params![
                next_status,
                now,
                next_input_tokens,
                next_output_tokens,
                next_cache_creation_tokens,
                next_cache_read_tokens,
                next_reasoning_tokens,
                next_embedding_input_count,
                next_wall_time_ms,
                completion
                    .estimated_cost_usd
                    .or(row.current_estimated_cost_usd),
                effective_error_class,
                next_confidence,
                invocation_id.to_string(),
            ],
        )?;

        if over_budget_actual {
            let mode = current_model_control_mode_tx(&tx)?;
            if mode != ModelControlMode::StopAll {
                for (key, value) in [
                    (KEY_MODEL_CONTROL_MODE, "deny_paid".to_string()),
                    (
                        KEY_MODEL_CONTROL_BREAKER_REASON,
                        "actual_over_reservation".to_string(),
                    ),
                    (
                        KEY_MODEL_CONTROL_BREAKER_INVOCATION_ID,
                        invocation_id.to_string(),
                    ),
                ] {
                    tx.execute(
                        "INSERT INTO daemon_settings (key, value, updated_at)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(key) DO UPDATE
                         SET value = excluded.value,
                             updated_at = excluded.updated_at",
                        params![key, value, now],
                    )?;
                }
            }
        }

        let foreground = parse_foreground(&row.foreground)?;
        let request = synthetic_request(
            &row.purpose,
            row.provider.as_deref(),
            row.session_id,
            row.project_id,
            row.workflow_id,
            row.scheduled_job_id,
            row.issue_tracker_id,
            row.topology_node_id,
            row.recursive_graph_id,
            row.recursive_task_id,
            row.recursive_attempt_id,
            row.operator,
            row.parent_invocation_id,
            row.retry_of_invocation_id,
        )?;
        let lineage = resolve_lineage_scope_ids_tx(
            &tx,
            invocation_id,
            request.parent_invocation_id,
            request.retry_of_invocation_id,
        )?;
        let tree_scope_id = lineage.tree_scope_id.to_string();
        let retry_scope_id = lineage.retry_scope_id.map(|id| id.to_string());
        let reservation_delta = if settling_running {
            CounterDelta {
                calls: 0,
                active: active_delta,
                input_tokens: next_input_tokens.unwrap_or(0) - row.reserved_input_tokens,
                output_tokens: next_output_tokens.unwrap_or(0) - row.reserved_output_tokens,
                cache_creation_tokens: next_cache_creation_tokens.unwrap_or(0)
                    - row.reserved_cache_creation_tokens,
                cache_read_tokens: next_cache_read_tokens.unwrap_or(0)
                    - row.reserved_cache_read_tokens,
                reasoning_tokens: next_reasoning_tokens.unwrap_or(0)
                    - row.reserved_reasoning_tokens,
                embedding_inputs: next_embedding_input_count.unwrap_or(0)
                    - row.reserved_embedding_input_count,
                wall_time_ms: next_wall_time_ms.unwrap_or(0) - row.reserved_wall_time_ms,
            }
        } else {
            CounterDelta {
                calls: 0,
                active: 0,
                input_tokens: input_delta,
                output_tokens: output_delta,
                cache_creation_tokens: cache_creation_delta,
                cache_read_tokens: cache_read_delta,
                reasoning_tokens: reasoning_delta,
                embedding_inputs: embedding_delta,
                wall_time_ms: wall_time_delta,
            }
        };
        for scope in budget_scopes(
            &request,
            row.provider.as_deref(),
            foreground,
            &tree_scope_id,
            retry_scope_id.as_deref(),
        ) {
            apply_counter_increment(
                &tx,
                &scope.0,
                &scope.1,
                &row.purpose,
                row.model_tier.as_deref().unwrap_or("standard"),
                row.effort.as_deref(),
                reservation_delta,
            )?;
        }

        let circuit_transition = reconcile_circuit_after_completion_tx(
            &tx,
            row.provider.as_deref(),
            invocation_id,
            next_status,
            completion.error_class.as_deref(),
            &now,
        )?;

        if let Some(wake_job_id) = disable_capacity_wake_id {
            tx.execute(
                "UPDATE scheduled_jobs
                 SET enabled=0,updated_at=?1
                 WHERE id=?2 AND enabled=1",
                params![now, wake_job_id],
            )?;
        }

        let record = self
            .load_model_invocation_record(invocation_id)?
            .ok_or_else(|| {
                DaemonError::Store(format!(
                    "missing invocation after completion: {invocation_id}"
                ))
            })?;
        Ok(StoreCompletionOutcome::Transitioned {
            record,
            circuit_transition,
        })
    }

    pub fn set_session_model_invocation(
        &self,
        session_id: Uuid,
        invocation_id: Option<Uuid>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions
             SET model_invocation_id = ?1,
                 updated_at = ?2
             WHERE id = ?3",
            params![
                invocation_id.map(|id| id.to_string()),
                Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    pub fn session_model_invocation_id(&self, session_id: Uuid) -> Result<Option<Uuid>> {
        let raw = self
            .conn
            .query_row(
                "SELECT model_invocation_id FROM sessions WHERE id = ?1",
                params![session_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        raw.map(|value| {
            Uuid::parse_str(&value).map_err(|e| {
                DaemonError::Store(format!(
                    "invalid sessions.model_invocation_id for session {session_id}: {e}"
                ))
            })
        })
        .transpose()
    }

    pub(crate) fn mark_manager_question_cleanup_required(&self, invocation: Uuid) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let session: Option<String> = tx.query_row(
            "SELECT session_id FROM model_invocations WHERE id=?1 AND admission_status='admitted' AND status IN ('running','cancellation_requested')",
            [invocation.to_string()], |row| row.get(0),
        ).optional()?.flatten();
        if let Some(session) = session {
            tx.execute(
                "UPDATE model_invocations SET error_class='manager_question_cleanup_required' WHERE id=?1",
                [invocation.to_string()],
            )?;
            // Normal continuation binds after the question clear. This failure
            // branch must publish the established invocation's exact owner now.
            let session =
                Uuid::parse_str(&session).map_err(|e| DaemonError::Store(e.to_string()))?;
            self.set_session_model_invocation(session, Some(invocation))?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn reconcile_running_model_invocations(&self) -> Result<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT mi.id, mi.session_id, s.status, s.total_input_tokens, s.total_output_tokens,
                    s.total_cache_creation_tokens, s.total_cache_read_tokens, s.work_time_ms, s.cost_usd,
                    mi.error_class = 'manager_question_cleanup_required',
                    (mi.purpose='session.continue.resume' AND mi.trigger_source='continue_session'
                     AND substr(mi.dedup_key,1,15)='manager.answer:'), mi.estimated_cost_usd
             FROM model_invocations mi
             LEFT JOIN sessions s ON s.id = mi.session_id
             WHERE mi.admission_status = 'admitted' AND (mi.status = 'running'
                 OR (mi.status = 'cancellation_requested' AND
                     (mi.error_class = 'manager_question_cleanup_required' OR
                      (mi.purpose='session.continue.resume' AND mi.trigger_source='continue_session'
                       AND substr(mi.dedup_key,1,15)='manager.answer:'))))",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<f64>>(8)?,
                    row.get::<_, Option<bool>>(9)?.unwrap_or(false),
                    row.get::<_, Option<bool>>(10)?.unwrap_or(false),
                    row.get::<_, Option<f64>>(11)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut reconciled = 0usize;
        for (
            invocation_id,
            _session_id,
            session_status,
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            work_time_ms,
            cost_usd,
            question_cleanup,
            manager_answer,
            invocation_cost,
        ) in rows
        {
            let invocation_id = Uuid::parse_str(&invocation_id).map_err(|e| {
                DaemonError::Store(format!(
                    "invalid running model invocation id during reconcile: {e}"
                ))
            })?;
            if self.is_dispatchable_unexecuted_capacity_admission(invocation_id)? {
                continue;
            }
            let disable_capacity_wake_id =
                self.unexecuted_capacity_admission_wake_id(invocation_id)?;
            // Production startup proves the daemon's entire process cohort
            // gone before this reconciliation (main.rs). Answer admission's
            // daemon-authored key is durable before provider execution, unlike
            // the cleanup marker. Even a matching Session invocation binding
            // does not prove its aggregate telemetry belongs to this turn:
            // cleanup can bind the new invocation while retaining old totals.
            // Preserve only measurements already stored on this exact ledger
            // row; absent cost remains unavailable across the pre-marker crash.
            #[cfg(not(target_os = "linux"))]
            if question_cleanup || manager_answer {
                continue;
            }
            if question_cleanup || manager_answer {
                if let Some(session) = _session_id
                    .as_deref()
                    .and_then(|id| Uuid::parse_str(id).ok())
                    && !matches!(
                        session_status.as_deref(),
                        Some("Archived") | Some("Deleted")
                    )
                {
                    self.update_session_status(
                        session,
                        rsi_common::types::SessionStatus::Interrupted,
                    )?;
                }
                self.complete_model_invocation(
                    invocation_id,
                    &InvocationCompletion {
                        error_class: Some(
                            if question_cleanup {
                                "manager_question_cleanup_restart"
                            } else {
                                "manager_answer_restart"
                            }
                            .into(),
                        ),
                        confidence: invocation_cost
                            .is_none()
                            .then_some(ModelUsageConfidence::Unavailable),
                        ..Default::default()
                    },
                )?;
                reconciled += 1;
                continue;
            }
            self.complete_model_invocation_reconciling_capacity(
                invocation_id,
                &InvocationCompletion {
                    input_tokens: input_tokens.map(|v| v.max(0) as u64),
                    output_tokens: output_tokens.map(|v| v.max(0) as u64),
                    cache_creation_tokens: cache_creation_tokens.map(|v| v.max(0) as u64),
                    cache_read_tokens: cache_read_tokens.map(|v| v.max(0) as u64),
                    wall_time_ms: work_time_ms.map(|v| v.max(0) as u64),
                    estimated_cost_usd: cost_usd,
                    error_class: Some(
                        if disable_capacity_wake_id.is_some() {
                            "capacity_delivery_abandoned_before_launch"
                        } else {
                            match session_status.as_deref() {
                                Some("Completed") | Some("Archived") => {
                                    "restart_reconciled_completed"
                                }
                                Some("Interrupted") => "restart_reconciled_interrupted",
                                Some("Failed") | Some("Running") | Some("Starting") => {
                                    "restart_reconciled_failed"
                                }
                                Some("WaitingApproval") => "restart_reconciled_waiting_approval",
                                _ => "restart_reconciled_background",
                            }
                        }
                        .to_string(),
                    ),
                    confidence: Some(
                        if input_tokens.is_some()
                            || output_tokens.is_some()
                            || cache_creation_tokens.is_some()
                            || cache_read_tokens.is_some()
                        {
                            ModelUsageConfidence::Stale
                        } else {
                            ModelUsageConfidence::Unavailable
                        },
                    ),
                    ..InvocationCompletion::default()
                },
                disable_capacity_wake_id.as_deref(),
            )?;
            reconciled += 1;
        }
        Ok(reconciled)
    }

    fn build_invocation_view(&self, record: ModelInvocationRecord) -> Result<ModelInvocationView> {
        let (budget, _) = self.collect_budget_views_for_record(&record)?;
        Ok(ModelInvocationView {
            owner_summary: invocation_owner_summary(&record.owner),
            lineage_summary: invocation_lineage_summary(&record),
            scope_summary: invocation_scope_summary(&record),
            denial_reason: invocation_denial_reason(&record),
            stop_mechanism: invocation_stop_mechanism(&record),
            stop_target: record.owner.session_id.map(|id| id.to_string()),
            cancellation_reason: record
                .cancellation_reason
                .clone()
                .or_else(|| record.authorization_reason.clone()),
            budget,
            record,
        })
    }

    fn list_budget_alerts_for_record(
        &self,
        record: &ModelInvocationRecord,
    ) -> Result<Vec<ModelBudgetAlert>> {
        let (_, alerts) = self.collect_budget_views_for_record(record)?;
        Ok(alerts)
    }

    fn collect_budget_views_for_record(
        &self,
        record: &ModelInvocationRecord,
    ) -> Result<(Vec<ModelBudgetHeadroom>, Vec<ModelBudgetAlert>)> {
        let request = synthetic_request_from_record(record)?;
        let foreground = record.foreground;
        let model_tier = classify_model_tier(
            record.provider.as_deref(),
            record.backend.as_deref(),
            record.model.as_deref(),
        );
        let lineage = {
            let tx = self.conn.unchecked_transaction()?;
            let lineage = resolve_lineage_scope_ids_tx(
                &tx,
                record.id,
                record.parent_invocation_id,
                record.retry_of_invocation_id,
            )?;
            tx.commit()?;
            lineage
        };
        let tree_scope_id = lineage.tree_scope_id.to_string();
        let retry_scope_id = lineage.retry_scope_id.map(|id| id.to_string());
        let mut headrooms = Vec::new();
        let mut alerts = Vec::new();
        for (scope_kind, scope_id) in budget_scopes(
            &request,
            record.provider.as_deref(),
            foreground,
            &tree_scope_id,
            retry_scope_id.as_deref(),
        ) {
            let mut stmt = self.conn.prepare(
                "SELECT purpose, model_tier, effort,
                        max_calls, max_total_tokens, max_input_tokens, max_output_tokens,
                        max_embedding_inputs, max_wall_time_ms, max_concurrency,
                        alert_threshold_ratio
                 FROM model_budget_policies
                 WHERE scope_kind = ?1 AND scope_id = ?2
                   AND (purpose IS NULL OR purpose = ?3)
                   AND (model_tier IS NULL OR model_tier = ?4)
                   AND (effort IS NULL OR effort = ?5)",
            )?;
            let mut rows = stmt.query(params![
                scope_kind,
                scope_id,
                record.purpose.as_str(),
                to_model_tier(model_tier),
                record.effort.as_deref(),
            ])?;
            while let Some(row) = rows.next()? {
                let counter_purpose = row
                    .get::<_, Option<String>>(0)?
                    .unwrap_or_else(|| COUNTER_ALL.to_string());
                let counter_model_tier = row
                    .get::<_, Option<String>>(1)?
                    .unwrap_or_else(|| COUNTER_ALL.to_string());
                let counter_effort = row
                    .get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| COUNTER_ALL.to_string());
                let counter_key = counter_key(
                    &scope_kind,
                    &scope_id,
                    &counter_purpose,
                    &counter_model_tier,
                    Some(&counter_effort),
                );
                let current = self
                    .conn
                    .query_row(
                        "SELECT call_count, active_count, input_tokens, output_tokens, embedding_inputs, wall_time_ms
                         FROM model_budget_counters WHERE counter_key = ?1",
                        params![counter_key],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, i64>(4)?,
                                row.get::<_, i64>(5)?,
                            ))
                        },
                    )
                    .optional()?
                    .unwrap_or((0, 0, 0, 0, 0, 0));
                let max_calls: Option<i64> = row.get(3)?;
                let max_total_tokens: Option<i64> = row.get(4)?;
                let max_input_tokens: Option<i64> = row.get(5)?;
                let max_output_tokens: Option<i64> = row.get(6)?;
                let max_embedding_inputs: Option<i64> = row.get(7)?;
                let max_wall_time_ms: Option<i64> = row.get(8)?;
                let max_concurrency: Option<i64> = row.get(9)?;
                let alert_threshold_ratio: Option<f64> = row.get(10)?;
                let parsed_scope_kind = parse_budget_scope_kind(&scope_kind)?;
                let parsed_scope_id = if scope_id == COUNTER_ALL {
                    None
                } else {
                    Some(scope_id.clone())
                };
                let parsed_purpose = if counter_purpose == COUNTER_ALL {
                    None
                } else {
                    Some(parse_purpose(&counter_purpose)?)
                };
                let parsed_model_tier = if counter_model_tier == COUNTER_ALL {
                    None
                } else {
                    Some(parse_model_tier(&counter_model_tier)?)
                };
                let parsed_effort = if counter_effort == COUNTER_ALL {
                    None
                } else {
                    Some(counter_effort.clone())
                };
                let remaining_calls = max_calls.map(|limit| limit - current.0);
                let remaining_active = max_concurrency.map(|limit| limit - current.1);
                let remaining_total_tokens =
                    max_total_tokens.map(|limit| limit - (current.2 + current.3));
                let remaining_input_tokens = max_input_tokens.map(|limit| limit - current.2);
                let remaining_output_tokens = max_output_tokens.map(|limit| limit - current.3);
                let remaining_embedding_inputs =
                    max_embedding_inputs.map(|limit| limit - current.4);
                let remaining_wall_time_ms = max_wall_time_ms.map(|limit| limit - current.5);
                let threshold_ratio = alert_threshold_ratio.unwrap_or(0.1);
                headrooms.push(ModelBudgetHeadroom {
                    scope_kind: parsed_scope_kind,
                    scope_id: if scope_id == COUNTER_ALL {
                        None
                    } else {
                        Some(scope_id.clone())
                    },
                    purpose: parsed_purpose,
                    model_tier: parsed_model_tier,
                    effort: parsed_effort,
                    source: format!("policy:{scope_kind}/{scope_id}"),
                    authorized: record.admission_status != AdmissionStatus::Denied,
                    policy_status: "configured".to_string(),
                    remaining_calls,
                    remaining_active,
                    remaining_total_tokens,
                    remaining_input_tokens,
                    remaining_output_tokens,
                    remaining_embedding_inputs,
                    remaining_wall_time_ms,
                });
                for (metric, remaining, limit) in [
                    ("calls", remaining_calls, max_calls),
                    ("active", remaining_active, max_concurrency),
                    ("input_tokens", remaining_input_tokens, max_input_tokens),
                    ("output_tokens", remaining_output_tokens, max_output_tokens),
                    ("total_tokens", remaining_total_tokens, max_total_tokens),
                    (
                        "embedding_inputs",
                        remaining_embedding_inputs,
                        max_embedding_inputs,
                    ),
                    ("wall_time_ms", remaining_wall_time_ms, max_wall_time_ms),
                ] {
                    let Some(remaining) = remaining else {
                        continue;
                    };
                    let Some(limit) = limit else {
                        continue;
                    };
                    let threshold = ((limit as f64) * threshold_ratio).ceil() as i64;
                    if remaining >= 0 && remaining <= threshold.max(1) {
                        alerts.push(ModelBudgetAlert {
                            invocation_id: record.id,
                            scope_kind: parsed_scope_kind,
                            scope_id: parsed_scope_id.clone(),
                            purpose: parsed_purpose,
                            metric: metric.to_string(),
                            remaining,
                            limit,
                            threshold,
                        });
                    }
                }
            }
        }
        if headrooms.is_empty() {
            headrooms.push(ModelBudgetHeadroom {
                scope_kind: BudgetScopeKind::Global,
                scope_id: None,
                purpose: None,
                model_tier: None,
                effort: None,
                source: "missing_policy".to_string(),
                authorized: false,
                policy_status: "missing".to_string(),
                remaining_calls: None,
                remaining_active: None,
                remaining_total_tokens: None,
                remaining_input_tokens: None,
                remaining_output_tokens: None,
                remaining_embedding_inputs: None,
                remaining_wall_time_ms: None,
            });
        }
        Ok((headrooms, alerts))
    }
}

#[derive(Debug)]
struct InvocationSettlementRow {
    purpose: String,
    foreground: String,
    provider: Option<String>,
    model_tier: Option<String>,
    effort: Option<String>,
    session_id: Option<String>,
    project_id: Option<String>,
    workflow_id: Option<String>,
    scheduled_job_id: Option<String>,
    issue_tracker_id: Option<String>,
    topology_node_id: Option<String>,
    recursive_graph_id: Option<String>,
    recursive_task_id: Option<String>,
    recursive_attempt_id: Option<String>,
    operator: Option<String>,
    parent_invocation_id: Option<String>,
    retry_of_invocation_id: Option<String>,
    admission_status: String,
    current_status: String,
    reserved_input_tokens: i64,
    reserved_output_tokens: i64,
    reserved_cache_creation_tokens: i64,
    reserved_cache_read_tokens: i64,
    reserved_reasoning_tokens: i64,
    reserved_embedding_input_count: i64,
    reserved_wall_time_ms: i64,
    current_input_tokens: Option<i64>,
    current_output_tokens: Option<i64>,
    current_cache_creation_tokens: Option<i64>,
    current_cache_read_tokens: Option<i64>,
    current_reasoning_tokens: Option<i64>,
    current_embedding_input_count: Option<i64>,
    current_wall_time_ms: Option<i64>,
    current_estimated_cost_usd: Option<f64>,
    current_confidence: String,
    baseline_input_tokens: i64,
    baseline_output_tokens: i64,
    baseline_cache_creation_tokens: i64,
    baseline_cache_read_tokens: i64,
    baseline_reasoning_tokens: i64,
    baseline_embedding_input_count: i64,
    baseline_wall_time_ms: i64,
    policy_snapshot_json: String,
}

const MODEL_INVOCATION_SELECT_BY_ID: &str = "SELECT
    id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
    provider, model, backend, model_tier, effort, trigger_source,
    session_id, project_id, workflow_id, scheduled_job_id, issue_tracker_id,
    issue_identifier, topology_node_id, recursive_graph_id, recursive_task_id,
    recursive_attempt_id, operator, parent_invocation_id, retry_of_invocation_id,
    dedup_key, request_fingerprint, policy_snapshot_json, error_class,
    cancellation_requested_at, cancellation_reason, cancellation_mechanism,
    created_at, started_at, completed_at,
    input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
    reasoning_tokens, embedding_input_count, wall_time_ms, estimated_cost_usd,
    usage_confidence, baseline_input_tokens, baseline_output_tokens,
    baseline_cache_creation_tokens, baseline_cache_read_tokens,
    baseline_reasoning_tokens, baseline_embedding_input_count, baseline_wall_time_ms
 FROM model_invocations
 WHERE id = ?1";

fn parse_model_invocation_record_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ModelInvocationRecord> {
    let id: String = row.get(0)?;
    let purpose: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let foreground: String = row.get(3)?;
    let paid_risk: String = row.get(4)?;
    let admission_status: String = row.get(5)?;
    let status: String = row.get(6)?;
    let policy_snapshot_json: String = row.get(28)?;
    let usage_confidence: String = row.get(44)?;
    let parsed_admission_status = parse_admission_status_lossy(&admission_status);
    let parsed_status = parse_invocation_status_lossy(&status);
    let (policy_snapshot, policy_snapshot_status, policy_snapshot_error) =
        parse_policy_snapshot_lossy(&policy_snapshot_json);
    let model_tier = row
        .get::<_, Option<String>>(10)?
        .map(|raw| parse_model_tier(&raw))
        .transpose()
        .map_err(err_to_sql_err)?;
    let owner = rsi_common::model_control::InvocationOwner {
        session_id: parse_optional_uuid(row.get(13)?).map_err(uuid_to_sql_err)?,
        project_id: parse_optional_uuid(row.get(14)?).map_err(uuid_to_sql_err)?,
        workflow_id: parse_optional_uuid(row.get(15)?).map_err(uuid_to_sql_err)?,
        scheduled_job_id: parse_optional_uuid(row.get(16)?).map_err(uuid_to_sql_err)?,
        issue_tracker_id: row.get(17)?,
        issue_identifier: row.get(18)?,
        topology_node_id: row.get(19)?,
        recursive_graph_id: row.get(20)?,
        recursive_task_id: row.get(21)?,
        recursive_attempt_id: row.get(22)?,
        operator: row.get(23)?,
    };
    Ok(ModelInvocationRecord {
        id: Uuid::parse_str(&id).map_err(uuid_to_sql_err)?,
        purpose: parse_purpose(&purpose).map_err(err_to_sql_err)?,
        kind: parse_kind(&kind).map_err(err_to_sql_err)?,
        foreground: parse_foreground(&foreground).map_err(err_to_sql_err)?,
        paid_risk: parse_paid_risk(&paid_risk).map_err(err_to_sql_err)?,
        status: parsed_status,
        provider: row.get(7)?,
        model: row.get(8)?,
        backend: row.get(9)?,
        model_tier,
        effort: row.get(11)?,
        trigger: row.get(12)?,
        owner_scopes: owner_scopes(&owner),
        owner,
        dedup_key: row.get(26)?,
        request_fingerprint: row.get(27)?,
        parent_invocation_id: parse_optional_uuid(row.get(24)?).map_err(uuid_to_sql_err)?,
        retry_of_invocation_id: parse_optional_uuid(row.get(25)?).map_err(uuid_to_sql_err)?,
        raw_admission_status: admission_status.clone(),
        raw_status: status.clone(),
        admission_status: parsed_admission_status,
        usage: ModelInvocationUsage {
            input_tokens: row.get::<_, Option<i64>>(36)?.map(|v| v.max(0) as u64),
            output_tokens: row.get::<_, Option<i64>>(37)?.map(|v| v.max(0) as u64),
            cache_creation_tokens: row.get::<_, Option<i64>>(38)?.map(|v| v.max(0) as u64),
            cache_read_tokens: row.get::<_, Option<i64>>(39)?.map(|v| v.max(0) as u64),
            reasoning_tokens: row.get::<_, Option<i64>>(40)?.map(|v| v.max(0) as u64),
            embedding_input_count: row.get::<_, Option<i64>>(41)?.map(|v| v.max(0) as u64),
            wall_time_ms: row.get::<_, Option<i64>>(42)?.map(|v| v.max(0) as u64),
            estimated_cost_usd: row.get(43)?,
            confidence: parse_confidence(&usage_confidence).map_err(err_to_sql_err)?,
        },
        baseline_usage: ModelInvocationUsage {
            input_tokens: Some(row.get::<_, i64>(45).unwrap_or_default().max(0) as u64),
            output_tokens: Some(row.get::<_, i64>(46).unwrap_or_default().max(0) as u64),
            cache_creation_tokens: Some(row.get::<_, i64>(47).unwrap_or_default().max(0) as u64),
            cache_read_tokens: Some(row.get::<_, i64>(48).unwrap_or_default().max(0) as u64),
            reasoning_tokens: Some(row.get::<_, i64>(49).unwrap_or_default().max(0) as u64),
            embedding_input_count: Some(row.get::<_, i64>(50).unwrap_or_default().max(0) as u64),
            wall_time_ms: Some(row.get::<_, i64>(51).unwrap_or_default().max(0) as u64),
            estimated_cost_usd: None,
            confidence: ModelUsageConfidence::Stale,
        },
        error_class: row.get(29)?,
        cancellation_requested_at: row.get(30)?,
        cancellation_reason: row.get(31)?,
        cancellation_mechanism: row.get(32)?,
        authorization_reason: authorization_reason(
            parsed_admission_status,
            parsed_status,
            row.get(29)?,
            policy_snapshot.as_ref(),
        ),
        policy_authorized: parsed_admission_status != AdmissionStatus::Denied,
        escalation_source: policy_snapshot
            .as_ref()
            .and_then(|value| value.get("escalation_source"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        escalation_reason: policy_snapshot
            .as_ref()
            .and_then(|value| value.get("escalation_reason"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        policy_snapshot,
        policy_snapshot_status,
        policy_snapshot_error,
        created_at: row.get(33)?,
        started_at: row.get(34)?,
        completed_at: row.get(35)?,
    })
}

fn uuid_to_sql_err(error: uuid::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn err_to_sql_err(error: DaemonError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(error.to_string())),
    )
}

fn parse_optional_uuid(raw: Option<String>) -> std::result::Result<Option<Uuid>, uuid::Error> {
    raw.as_deref().map(Uuid::parse_str).transpose()
}

fn current_model_control_mode_tx(tx: &rusqlite::Transaction<'_>) -> Result<ModelControlMode> {
    let raw = tx
        .query_row(
            "SELECT value FROM daemon_settings WHERE key = ?1",
            params![KEY_MODEL_CONTROL_MODE],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    raw.as_deref()
        .map(parse_mode)
        .transpose()
        .map(|mode| mode.unwrap_or(ModelControlMode::Normal))
}

fn set_model_control_mode_tx(
    tx: &rusqlite::Transaction<'_>,
    mode: ModelControlMode,
    updated_at: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO daemon_settings (key, value, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![KEY_MODEL_CONTROL_MODE, to_mode(mode), updated_at],
    )?;
    Ok(())
}

fn update_model_budget_policies_tx(
    tx: &rusqlite::Transaction<'_>,
    policies: &[ModelBudgetPolicy],
    replace: bool,
) -> Result<()> {
    if replace {
        tx.execute("DELETE FROM model_budget_policies", [])?;
    }
    for policy in policies {
        validate_model_budget_policy(policy)?;
        let scope_id = scope_id_for_policy(policy.scope_kind, policy.scope_id.as_deref())?;
        let policy_key = format!(
            "{}:{}:{}:{}:{}",
            to_budget_scope_kind(policy.scope_kind),
            scope_id,
            policy
                .purpose
                .map(|purpose| purpose.as_str())
                .unwrap_or(COUNTER_ALL),
            policy.model_tier.map(to_model_tier).unwrap_or(COUNTER_ALL),
            policy.effort.as_deref().unwrap_or(COUNTER_ALL),
        );
        tx.execute(
            "INSERT INTO model_budget_policies (
                policy_key, scope_kind, scope_id, purpose, model_tier, effort,
                max_calls, max_total_tokens, max_input_tokens, max_output_tokens,
                max_embedding_inputs, max_wall_time_ms, max_concurrency, max_retries,
                alert_threshold_ratio, ceiling_model_tier, ceiling_effort,
                max_cache_creation_tokens, max_cache_read_tokens, max_reasoning_tokens,
                max_calls_per_window, rate_window_seconds, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14,
                ?15, ?16, ?17,
                ?18, ?19, ?20,
                ?21, ?22, ?23
            )
            ON CONFLICT(policy_key) DO UPDATE SET
                max_calls = excluded.max_calls,
                max_total_tokens = excluded.max_total_tokens,
                max_input_tokens = excluded.max_input_tokens,
                max_output_tokens = excluded.max_output_tokens,
                max_embedding_inputs = excluded.max_embedding_inputs,
                max_wall_time_ms = excluded.max_wall_time_ms,
                max_concurrency = excluded.max_concurrency,
                max_retries = excluded.max_retries,
                alert_threshold_ratio = excluded.alert_threshold_ratio,
                ceiling_model_tier = excluded.ceiling_model_tier,
                ceiling_effort = excluded.ceiling_effort,
                max_cache_creation_tokens = excluded.max_cache_creation_tokens,
                max_cache_read_tokens = excluded.max_cache_read_tokens,
                max_reasoning_tokens = excluded.max_reasoning_tokens,
                max_calls_per_window = excluded.max_calls_per_window,
                rate_window_seconds = excluded.rate_window_seconds,
                updated_at = excluded.updated_at",
            params![
                policy_key,
                to_budget_scope_kind(policy.scope_kind),
                scope_id,
                policy.purpose.map(|purpose| purpose.as_str()),
                policy.model_tier.map(to_model_tier),
                policy.effort.as_deref(),
                policy.max_calls.map(|value| value as i64),
                policy.max_total_tokens.map(|value| value as i64),
                policy.max_input_tokens.map(|value| value as i64),
                policy.max_output_tokens.map(|value| value as i64),
                policy.max_embedding_inputs.map(|value| value as i64),
                policy.max_wall_time_ms.map(|value| value as i64),
                policy.max_concurrency.map(|value| value as i64),
                policy.max_retries.map(|value| value as i64),
                policy.alert_threshold_ratio,
                policy.ceiling_model_tier.map(to_model_tier),
                policy.ceiling_effort.as_deref(),
                policy.max_cache_creation_tokens.map(|value| value as i64),
                policy.max_cache_read_tokens.map(|value| value as i64),
                policy.max_reasoning_tokens.map(|value| value as i64),
                policy.max_calls_per_window.map(|value| value as i64),
                policy.rate_window_seconds.map(|value| value as i64),
                Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
            ],
        )?;
    }
    Ok(())
}

fn enforce_mode(
    mode: ModelControlMode,
    foreground: InvocationForeground,
    paid_capable: bool,
    background_paid_enabled_by_default: bool,
    is_local: bool,
    purpose: rsi_common::model_control::ModelInvocationPurpose,
) -> Result<()> {
    match mode {
        ModelControlMode::Normal => {
            if foreground == InvocationForeground::Background
                && paid_capable
                && !is_local
                && !background_paid_enabled_by_default
            {
                return Err(DaemonError::PolicyDenied(format!(
                    "paid background model work is disabled by default for {purpose}"
                )));
            }
        }
        ModelControlMode::PauseBackground => {
            if foreground == InvocationForeground::Background {
                return Err(DaemonError::PolicyDenied(format!(
                    "background model work is paused for {purpose}"
                )));
            }
        }
        ModelControlMode::DenyPaid => {
            if paid_capable && !is_local {
                return Err(DaemonError::PolicyDenied(format!(
                    "paid model work is disabled for {purpose}"
                )));
            }
        }
        ModelControlMode::LocalOnly => {
            if !is_local {
                return Err(DaemonError::PolicyDenied(format!(
                    "only local model work is allowed for {purpose}"
                )));
            }
        }
        ModelControlMode::StopAll => {
            return Err(DaemonError::PolicyDenied(format!(
                "all model work is stopped for {purpose}"
            )));
        }
    }
    Ok(())
}

fn insert_model_invocation_denied_tx(
    tx: &rusqlite::Transaction<'_>,
    invocation_id: Uuid,
    kind: ModelInvocationKind,
    foreground: InvocationForeground,
    paid_risk: PaidRisk,
    model_tier: ModelTier,
    request: &ModelAdmissionRequest,
    policy_snapshot: &serde_json::Value,
    error_class: &str,
    now: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO model_invocations (
            id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
            provider, model, backend, model_tier, effort, trigger_source,
            session_id, project_id, workflow_id, scheduled_job_id, issue_tracker_id,
            issue_identifier, topology_node_id, recursive_graph_id, recursive_task_id,
            recursive_attempt_id, operator, parent_invocation_id, retry_of_invocation_id,
            dedup_key, request_fingerprint, policy_snapshot_json, error_class, created_at, completed_at
        ) VALUES (
            ?, ?, ?, ?, ?, 'denied', 'denied',
            ?, ?, ?, ?, ?, ?,
            ?, ?, ?, ?, ?,
            ?, ?, ?, ?,
            ?, ?, ?, ?,
            ?, ?, ?, ?, ?, ?
        )",
        params![
            invocation_id.to_string(),
            request.purpose.as_str(),
            to_kind(kind),
            to_foreground(foreground),
            to_paid_risk(paid_risk),
            request.provider.as_deref(),
            request.model.as_deref(),
            request.backend.as_deref(),
            to_model_tier(model_tier),
            request.effort.as_deref(),
            request.trigger,
            request.owner.session_id.map(|id| id.to_string()),
            request.owner.project_id.map(|id| id.to_string()),
            request.owner.workflow_id.map(|id| id.to_string()),
            request.owner.scheduled_job_id.map(|id| id.to_string()),
            request.owner.issue_tracker_id.as_deref(),
            request.owner.issue_identifier.as_deref(),
            request.owner.topology_node_id.as_deref(),
            request.owner.recursive_graph_id.as_deref(),
            request.owner.recursive_task_id.as_deref(),
            request.owner.recursive_attempt_id.as_deref(),
            request.owner.operator.as_deref(),
            request.parent_invocation_id.map(|id| id.to_string()),
            request.retry_of_invocation_id.map(|id| id.to_string()),
            request.dedup_key.as_deref(),
            request.request_fingerprint.as_deref(),
            policy_snapshot.to_string(),
            error_class,
            now,
            now,
        ],
    )?;
    Ok(())
}

fn parse_mode(raw: &str) -> Result<ModelControlMode> {
    match raw {
        "normal" => Ok(ModelControlMode::Normal),
        "pause_background" => Ok(ModelControlMode::PauseBackground),
        "deny_paid" => Ok(ModelControlMode::DenyPaid),
        "local_only" => Ok(ModelControlMode::LocalOnly),
        "stop_all" => Ok(ModelControlMode::StopAll),
        other => Err(DaemonError::Store(format!(
            "invalid model control mode persisted in daemon_settings: {other}"
        ))),
    }
}

fn to_mode(value: ModelControlMode) -> &'static str {
    match value {
        ModelControlMode::Normal => "normal",
        ModelControlMode::PauseBackground => "pause_background",
        ModelControlMode::DenyPaid => "deny_paid",
        ModelControlMode::LocalOnly => "local_only",
        ModelControlMode::StopAll => "stop_all",
    }
}

fn to_kind(value: ModelInvocationKind) -> &'static str {
    match value {
        ModelInvocationKind::SessionLifecycle => "session_lifecycle",
        ModelInvocationKind::Orchestration => "orchestration",
        ModelInvocationKind::DirectText => "direct_text",
        ModelInvocationKind::Background => "background",
        ModelInvocationKind::Embedding => "embedding",
        ModelInvocationKind::Discovery => "discovery",
        ModelInvocationKind::Queue => "queue",
    }
}

fn to_foreground(value: InvocationForeground) -> &'static str {
    match value {
        InvocationForeground::Foreground => "foreground",
        InvocationForeground::Background => "background",
    }
}

fn parse_foreground(raw: &str) -> Result<InvocationForeground> {
    match raw {
        "foreground" => Ok(InvocationForeground::Foreground),
        "background" => Ok(InvocationForeground::Background),
        other => Err(DaemonError::Store(format!(
            "invalid foreground value in model_invocations: {other}"
        ))),
    }
}

fn parse_kind(raw: &str) -> Result<ModelInvocationKind> {
    match raw {
        "session_lifecycle" => Ok(ModelInvocationKind::SessionLifecycle),
        "orchestration" => Ok(ModelInvocationKind::Orchestration),
        "direct_text" => Ok(ModelInvocationKind::DirectText),
        "background" => Ok(ModelInvocationKind::Background),
        "embedding" => Ok(ModelInvocationKind::Embedding),
        "discovery" => Ok(ModelInvocationKind::Discovery),
        "queue" => Ok(ModelInvocationKind::Queue),
        other => Err(DaemonError::Store(format!(
            "invalid invocation kind in model_invocations: {other}"
        ))),
    }
}

fn to_paid_risk(value: PaidRisk) -> &'static str {
    match value {
        PaidRisk::PaidCapable => "paid_capable",
        PaidRisk::LocalOnly => "local_only",
        PaidRisk::CatalogOnly => "catalog_only",
        PaidRisk::NonInvocation => "non_invocation",
    }
}

fn parse_paid_risk(raw: &str) -> Result<PaidRisk> {
    match raw {
        "paid_capable" => Ok(PaidRisk::PaidCapable),
        "local_only" => Ok(PaidRisk::LocalOnly),
        "catalog_only" => Ok(PaidRisk::CatalogOnly),
        "non_invocation" => Ok(PaidRisk::NonInvocation),
        other => Err(DaemonError::Store(format!(
            "invalid paid_risk in model_invocations: {other}"
        ))),
    }
}

fn to_model_tier(value: ModelTier) -> &'static str {
    match value {
        ModelTier::Local => "local",
        ModelTier::Standard => "standard",
        ModelTier::Premium => "premium",
    }
}

fn parse_model_tier(raw: &str) -> Result<ModelTier> {
    match raw {
        "local" => Ok(ModelTier::Local),
        "standard" => Ok(ModelTier::Standard),
        "premium" => Ok(ModelTier::Premium),
        other => Err(DaemonError::Store(format!(
            "invalid model_tier in model_invocations: {other}"
        ))),
    }
}

fn to_confidence(value: ModelUsageConfidence) -> &'static str {
    match value {
        ModelUsageConfidence::Measured => "measured",
        ModelUsageConfidence::Estimated => "estimated",
        ModelUsageConfidence::Partial => "partial",
        ModelUsageConfidence::Stale => "stale",
        ModelUsageConfidence::Unavailable => "unavailable",
    }
}

fn parse_confidence(raw: &str) -> Result<ModelUsageConfidence> {
    match raw {
        "measured" => Ok(ModelUsageConfidence::Measured),
        "estimated" => Ok(ModelUsageConfidence::Estimated),
        "partial" => Ok(ModelUsageConfidence::Partial),
        "stale" => Ok(ModelUsageConfidence::Stale),
        "unavailable" => Ok(ModelUsageConfidence::Unavailable),
        other => Err(DaemonError::Store(format!(
            "invalid usage_confidence in model_invocations: {other}"
        ))),
    }
}

fn parse_admission_status(raw: &str) -> Result<AdmissionStatus> {
    match raw {
        "admitted" => Ok(AdmissionStatus::Admitted),
        "denied" => Ok(AdmissionStatus::Denied),
        "duplicate" => Ok(AdmissionStatus::Duplicate),
        other => Err(DaemonError::Store(format!(
            "invalid admission_status in model_invocations: {other}"
        ))),
    }
}

fn parse_admission_status_lossy(raw: &str) -> AdmissionStatus {
    parse_admission_status(raw).unwrap_or(AdmissionStatus::Unknown)
}

fn parse_invocation_status(raw: &str) -> Result<ModelInvocationStatus> {
    match raw {
        "running" => Ok(ModelInvocationStatus::Running),
        "cancellation_requested" => Ok(ModelInvocationStatus::CancellationRequested),
        "completed" => Ok(ModelInvocationStatus::Completed),
        "failed" => Ok(ModelInvocationStatus::Failed),
        "cancelled" => Ok(ModelInvocationStatus::Cancelled),
        "denied" => Ok(ModelInvocationStatus::Denied),
        other => Err(DaemonError::Store(format!(
            "invalid status in model_invocations: {other}"
        ))),
    }
}

fn parse_invocation_status_lossy(raw: &str) -> ModelInvocationStatus {
    parse_invocation_status(raw).unwrap_or(ModelInvocationStatus::Unknown)
}

fn parse_budget_scope_kind(raw: &str) -> Result<BudgetScopeKind> {
    match raw {
        "global" => Ok(BudgetScopeKind::Global),
        "provider" => Ok(BudgetScopeKind::Provider),
        "project" => Ok(BudgetScopeKind::Project),
        "session" => Ok(BudgetScopeKind::Session),
        "tree" => Ok(BudgetScopeKind::Tree),
        "workflow" => Ok(BudgetScopeKind::Workflow),
        "subsystem" => Ok(BudgetScopeKind::Subsystem),
        "retry" => Ok(BudgetScopeKind::Retry),
        "scheduled_job" => Ok(BudgetScopeKind::ScheduledJob),
        "issue_tracker" => Ok(BudgetScopeKind::IssueTracker),
        "recursive_graph" => Ok(BudgetScopeKind::RecursiveGraph),
        "operator" => Ok(BudgetScopeKind::Operator),
        other => Err(DaemonError::Store(format!(
            "invalid budget scope kind: {other}"
        ))),
    }
}

fn to_budget_scope_kind(value: BudgetScopeKind) -> &'static str {
    match value {
        BudgetScopeKind::Global => "global",
        BudgetScopeKind::Provider => "provider",
        BudgetScopeKind::Project => "project",
        BudgetScopeKind::Session => "session",
        BudgetScopeKind::Tree => "tree",
        BudgetScopeKind::Workflow => "workflow",
        BudgetScopeKind::Subsystem => "subsystem",
        BudgetScopeKind::Retry => "retry",
        BudgetScopeKind::ScheduledJob => "scheduled_job",
        BudgetScopeKind::IssueTracker => "issue_tracker",
        BudgetScopeKind::RecursiveGraph => "recursive_graph",
        BudgetScopeKind::Operator => "operator",
    }
}

fn parse_purpose(raw: &str) -> Result<ModelInvocationPurpose> {
    serde_json::from_value(serde_json::Value::String(raw.to_string()))
        .map_err(|e| DaemonError::Store(format!("invalid model invocation purpose in row: {e}")))
}

fn invocation_owner_summary(owner: &rsi_common::model_control::InvocationOwner) -> String {
    let mut parts = Vec::new();
    if let Some(session_id) = owner.session_id {
        parts.push(format!("session {}", short_uuid(session_id)));
    }
    if let Some(workflow_id) = owner.workflow_id {
        parts.push(format!("workflow {}", short_uuid(workflow_id)));
    }
    if let Some(project_id) = owner.project_id {
        parts.push(format!("project {}", short_uuid(project_id)));
    }
    if let Some(job_id) = owner.scheduled_job_id {
        parts.push(format!("job {}", short_uuid(job_id)));
    }
    if let Some(graph_id) = owner.recursive_graph_id.as_deref() {
        parts.push(format!("graph {graph_id}"));
    }
    if let Some(attempt_id) = owner.recursive_attempt_id.as_deref() {
        parts.push(format!("attempt {attempt_id}"));
    }
    if let Some(issue) = owner.issue_identifier.as_deref() {
        parts.push(format!("issue {issue}"));
    }
    if let Some(operator) = owner.operator.as_deref() {
        parts.push(format!("operator {operator}"));
    }
    if parts.is_empty() {
        "unscoped".to_string()
    } else {
        parts.join(" | ")
    }
}

fn invocation_denial_reason(record: &ModelInvocationRecord) -> Option<String> {
    if record.admission_status == AdmissionStatus::Denied {
        record
            .policy_snapshot
            .as_ref()
            .and_then(|value| value.get("reason"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned)
            .or_else(|| record.error_class.clone())
    } else if record.status == ModelInvocationStatus::CancellationRequested {
        record.cancellation_reason.clone()
    } else {
        None
    }
}

fn invocation_stop_mechanism(record: &ModelInvocationRecord) -> String {
    if !matches!(
        record.status,
        ModelInvocationStatus::Running | ModelInvocationStatus::CancellationRequested
    ) {
        "settled".to_string()
    } else if record.owner.session_id.is_some() {
        "interrupt_session".to_string()
    } else if record.owner.operator.is_some() {
        "cancellation_registry".to_string()
    } else {
        "none".to_string()
    }
}

fn invocation_lineage_summary(record: &ModelInvocationRecord) -> String {
    let mut parts = Vec::new();
    if let Some(parent_id) = record.parent_invocation_id {
        parts.push(format!("parent {}", short_uuid(parent_id)));
    }
    if let Some(retry_id) = record.retry_of_invocation_id {
        parts.push(format!("retry {}", short_uuid(retry_id)));
    }
    if parts.is_empty() {
        "root".to_string()
    } else {
        parts.join(" | ")
    }
}

fn invocation_scope_summary(record: &ModelInvocationRecord) -> String {
    let mut parts = record
        .owner_scopes
        .iter()
        .map(|scope| match &scope.scope_id {
            Some(scope_id) => format!("{:?}:{scope_id}", scope.kind).to_ascii_lowercase(),
            None => format!("{:?}", scope.kind).to_ascii_lowercase(),
        })
        .collect::<Vec<_>>();
    parts.dedup();
    if parts.is_empty() {
        "unscoped".to_string()
    } else {
        parts.join(" | ")
    }
}

fn authorization_reason(
    admission_status: AdmissionStatus,
    status: ModelInvocationStatus,
    error_class: Option<String>,
    policy_snapshot: Option<&serde_json::Value>,
) -> Option<String> {
    if let Some(reason) = policy_snapshot
        .and_then(|value| value.get("reason"))
        .and_then(|value| value.as_str())
    {
        return Some(reason.to_string());
    }
    match admission_status {
        AdmissionStatus::Denied => error_class.or_else(|| Some("policy_denied".to_string())),
        AdmissionStatus::Unknown => Some("unknown_admission_status".to_string()),
        _ if status == ModelInvocationStatus::CancellationRequested => {
            Some("cancellation_requested".to_string())
        }
        _ if status == ModelInvocationStatus::Unknown => Some("unknown_runtime_status".to_string()),
        _ => None,
    }
}

fn parse_policy_snapshot_lossy(raw: &str) -> (Option<serde_json::Value>, String, Option<String>) {
    if raw.trim().is_empty() {
        return (
            None,
            "missing".to_string(),
            Some("empty policy_snapshot_json".to_string()),
        );
    }
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => (Some(value), "valid".to_string(), None),
        Err(error) => (
            None,
            "corrupt".to_string(),
            Some(format!("invalid policy_snapshot_json: {error}")),
        ),
    }
}

fn parse_budget_alert_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelBudgetAlert> {
    let invocation_id: String = row.get(0)?;
    let scope_kind: String = row.get(1)?;
    let scope_id: String = row.get(2)?;
    let purpose: String = row.get(3)?;
    Ok(ModelBudgetAlert {
        invocation_id: Uuid::parse_str(&invocation_id).map_err(uuid_to_sql_err)?,
        scope_kind: parse_budget_scope_kind(&scope_kind).map_err(err_to_sql_err)?,
        scope_id: normalize_counter_scope_id(Some(scope_id)),
        purpose: if purpose == COUNTER_ALL {
            None
        } else {
            Some(parse_purpose(&purpose).map_err(err_to_sql_err)?)
        },
        metric: row.get(4)?,
        remaining: row.get(5)?,
        limit: row.get(6)?,
        threshold: row.get(7)?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitFailureDisposition {
    Ignore,
    Immediate,
    Transient,
    Other,
}

pub(super) fn is_cancellation_terminal_error(error_class: Option<&str>) -> bool {
    let Some(error_class) = error_class else {
        return false;
    };
    let normalized = error_class.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "cancelled"
            | "operator_cancelled"
            | "stop_all"
            | "interrupted"
            | "user_cancelled"
            | "cancelled_after_restart"
            | "restart_reconciled_cancel"
    )
}

fn classify_circuit_failure(error_class: Option<&str>) -> CircuitFailureDisposition {
    let Some(error_class) = error_class else {
        return CircuitFailureDisposition::Ignore;
    };
    let normalized = error_class.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || normalized == "policy_denied"
        || normalized == "budget_denied"
        || normalized == "circuit_open"
        || is_cancellation_terminal_error(Some(&normalized))
    {
        return CircuitFailureDisposition::Ignore;
    }
    if normalized.contains("quota")
        || normalized == "auth"
        || normalized.contains("authorization")
        || normalized.contains("provider_config")
        || normalized == "config"
    {
        return CircuitFailureDisposition::Immediate;
    }
    if normalized.contains("timeout")
        || normalized.contains("network")
        || normalized.contains("transport")
        || normalized.contains("overload")
        || normalized.contains("rate")
        || normalized.contains("unavailable")
    {
        return CircuitFailureDisposition::Transient;
    }
    CircuitFailureDisposition::Other
}

fn load_model_circuits_tx(tx: &rusqlite::Transaction<'_>) -> Result<Vec<ModelCircuitStatus>> {
    let raw = tx
        .query_row(
            "SELECT value FROM daemon_settings WHERE key = ?1",
            params![KEY_MODEL_CONTROL_CIRCUITS],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match raw {
        Some(raw) => serde_json::from_str::<Vec<ModelCircuitStatus>>(&raw).map_err(|error| {
            DaemonError::Store(format!("invalid model_control_circuits payload: {error}"))
        }),
        None => Ok(Vec::new()),
    }
}

fn set_model_circuits_tx(
    tx: &rusqlite::Transaction<'_>,
    circuits: &[ModelCircuitStatus],
) -> Result<()> {
    for circuit in circuits {
        validate_model_circuit_status(circuit)?;
    }
    tx.execute(
        "INSERT INTO daemon_settings (key, value, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![
            KEY_MODEL_CONTROL_CIRCUITS,
            serde_json::to_string(circuits).map_err(|error| DaemonError::Store(format!(
                "serialize model circuits: {error}"
            )))?,
            Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        ],
    )?;
    Ok(())
}

fn provider_circuit_index(circuits: &[ModelCircuitStatus], provider: &str) -> Option<usize> {
    circuits.iter().position(|circuit| {
        circuit.scope_kind == BudgetScopeKind::Provider
            && circuit
                .scope_id
                .as_deref()
                .is_some_and(|scope_id| scope_id.eq_ignore_ascii_case(provider))
    })
}

fn ensure_provider_circuit<'a>(
    circuits: &'a mut Vec<ModelCircuitStatus>,
    provider: &str,
    now: &str,
) -> &'a mut ModelCircuitStatus {
    if let Some(index) = provider_circuit_index(circuits, provider) {
        return &mut circuits[index];
    }
    circuits.push(ModelCircuitStatus {
        scope_kind: BudgetScopeKind::Provider,
        scope_id: Some(provider.to_ascii_lowercase()),
        state: "closed".to_string(),
        reason: "healthy".to_string(),
        error_class: None,
        source: "automatic".to_string(),
        opened_at: None,
        updated_at: now.to_string(),
        reset_at: Some(now.to_string()),
        cooldown_secs: Some(DEFAULT_CIRCUIT_COOLDOWN_SECS),
        probe_after: None,
        trip_count: 0,
        transient_failure_count: 0,
        transient_window_started_at: None,
        probe_invocation_id: None,
        probe_lease_started_at: None,
    });
    circuits
        .last_mut()
        .expect("provider circuit inserted before lookup")
}

fn close_circuit_entry(circuit: &mut ModelCircuitStatus, now: &str, reason: &str, source: &str) {
    circuit.state = "closed".to_string();
    circuit.reason = reason.to_string();
    circuit.error_class = None;
    circuit.source = source.to_string();
    circuit.updated_at = now.to_string();
    circuit.reset_at = Some(now.to_string());
    circuit.probe_after = None;
    circuit.probe_invocation_id = None;
    circuit.probe_lease_started_at = None;
    circuit.transient_failure_count = 0;
    circuit.transient_window_started_at = None;
}

fn reopen_circuit_entry(
    circuit: &mut ModelCircuitStatus,
    now: &str,
    reason: &str,
    error_class: Option<&str>,
    source: &str,
) {
    let cooldown_secs = circuit
        .cooldown_secs
        .unwrap_or(DEFAULT_CIRCUIT_COOLDOWN_SECS);
    if circuit.opened_at.is_none() || circuit.state == "closed" {
        circuit.opened_at = Some(now.to_string());
    }
    circuit.trip_count = circuit.trip_count.saturating_add(1);
    circuit.state = "open".to_string();
    circuit.reason = reason.to_string();
    circuit.error_class = error_class.map(ToOwned::to_owned);
    circuit.source = source.to_string();
    circuit.updated_at = now.to_string();
    circuit.cooldown_secs = Some(cooldown_secs);
    circuit.probe_after = Some(
        (Utc::now() + TimeDelta::seconds(cooldown_secs as i64))
            .to_rfc3339_opts(SecondsFormat::Nanos, true),
    );
    circuit.probe_invocation_id = None;
    circuit.probe_lease_started_at = None;
}

fn reconcile_circuit_after_completion_tx(
    tx: &rusqlite::Transaction<'_>,
    provider: Option<&str>,
    invocation_id: Uuid,
    next_status: &str,
    error_class: Option<&str>,
    now: &str,
) -> Result<Option<ModelCircuitStatus>> {
    let Some(provider) = provider.map(|value| value.to_ascii_lowercase()) else {
        return Ok(None);
    };
    let mut circuits = load_model_circuits_tx(tx)?;
    let circuit = ensure_provider_circuit(&mut circuits, &provider, now);
    let is_probe =
        circuit.state == "half_open" && circuit.probe_invocation_id == Some(invocation_id);

    let mut changed = false;
    match next_status {
        "completed" if is_probe => {
            close_circuit_entry(circuit, now, "probe_succeeded", "automatic_probe");
            changed = true;
        }
        "cancelled" if is_probe => {
            reopen_circuit_entry(circuit, now, "probe_cancelled", None, "automatic_probe");
            changed = true;
        }
        "failed" | "cancelled" => match classify_circuit_failure(error_class) {
            CircuitFailureDisposition::Ignore => {
                if is_probe {
                    reopen_circuit_entry(
                        circuit,
                        now,
                        "probe_failed",
                        error_class,
                        "automatic_probe",
                    );
                    changed = true;
                }
            }
            CircuitFailureDisposition::Immediate => {
                reopen_circuit_entry(
                    circuit,
                    now,
                    error_class.unwrap_or("provider_failure"),
                    error_class,
                    "automatic_trip",
                );
                circuit.transient_failure_count = 0;
                circuit.transient_window_started_at = Some(now.to_string());
                changed = true;
            }
            CircuitFailureDisposition::Transient => {
                let within_window = circuit
                    .transient_window_started_at
                    .as_deref()
                    .and_then(|started_at| chrono::DateTime::parse_from_rfc3339(started_at).ok())
                    .is_some_and(|started_at| {
                        (Utc::now() - started_at.with_timezone(&Utc)).num_seconds()
                            <= TRANSIENT_CIRCUIT_WINDOW_SECS
                    });
                if !within_window {
                    circuit.transient_failure_count = 0;
                    circuit.transient_window_started_at = Some(now.to_string());
                }
                circuit.transient_failure_count = circuit.transient_failure_count.saturating_add(1);
                if is_probe || circuit.transient_failure_count >= TRANSIENT_CIRCUIT_THRESHOLD {
                    reopen_circuit_entry(
                        circuit,
                        now,
                        error_class.unwrap_or("transient_failure_storm"),
                        error_class,
                        "automatic_trip",
                    );
                }
                changed = true;
            }
            CircuitFailureDisposition::Other => {
                if is_probe {
                    reopen_circuit_entry(
                        circuit,
                        now,
                        error_class.unwrap_or("probe_failed"),
                        error_class,
                        "automatic_probe",
                    );
                    changed = true;
                }
            }
        },
        "completed" if circuit.state == "closed" && circuit.transient_failure_count != 0 => {
            circuit.transient_failure_count = 0;
            circuit.transient_window_started_at = None;
            circuit.updated_at = now.to_string();
            changed = true;
        }
        _ => {}
    }

    if !changed {
        return Ok(None);
    }
    let transition = circuit.clone();
    set_model_circuits_tx(tx, &circuits)?;
    Ok(Some(transition))
}

fn enforce_circuit_policy_tx(
    tx: &rusqlite::Transaction<'_>,
    provider: Option<&str>,
    invocation_id: Uuid,
    now: &str,
) -> Result<Option<ModelCircuitStatus>> {
    let provider = provider.map(|value| value.to_ascii_lowercase());
    let mut circuits = load_model_circuits_tx(tx)?;
    let mut transition = None;
    let mut changed = false;
    for circuit in &mut circuits {
        let scope_matches = match circuit.scope_kind {
            BudgetScopeKind::Global => true,
            BudgetScopeKind::Provider => circuit
                .scope_id
                .as_ref()
                .zip(provider.as_ref())
                .is_some_and(|(scope_id, provider)| scope_id.eq_ignore_ascii_case(provider)),
            _ => false,
        };
        if !scope_matches {
            continue;
        }
        match circuit.state.as_str() {
            "closed" => continue,
            "half_open" => {
                if circuit.probe_invocation_id == Some(invocation_id) {
                    continue;
                }
                return Err(DaemonError::PolicyDenied(format!(
                    "model circuit {} is half_open: {}",
                    circuit.scope_id.as_deref().unwrap_or("global"),
                    circuit.reason
                )));
            }
            "open" => {
                let operator_open = circuit.source.starts_with("operator");
                let probe_due = circuit
                    .probe_after
                    .as_deref()
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .is_some_and(|probe_after| probe_after.with_timezone(&Utc) <= Utc::now());
                if !operator_open && probe_due {
                    circuit.state = "half_open".to_string();
                    circuit.updated_at = now.to_string();
                    circuit.probe_invocation_id = Some(invocation_id);
                    circuit.probe_lease_started_at = Some(now.to_string());
                    transition = Some(circuit.clone());
                    changed = true;
                    continue;
                }
                return Err(DaemonError::PolicyDenied(format!(
                    "model circuit {} is open: {}",
                    circuit.scope_id.as_deref().unwrap_or("global"),
                    circuit.reason
                )));
            }
            other => {
                return Err(DaemonError::Store(format!(
                    "invalid model circuit state persisted in daemon_settings: {other}"
                )));
            }
        }
    }
    if changed {
        set_model_circuits_tx(tx, &circuits)?;
    }
    Ok(transition)
}

fn normalize_counter_scope_id(raw: Option<String>) -> Option<String> {
    raw.filter(|value| value != COUNTER_ALL)
}

fn owner_scopes(owner: &rsi_common::model_control::InvocationOwner) -> Vec<BudgetScopeRef> {
    let mut scopes = Vec::new();
    if owner.session_id.is_some() {
        scopes.push(BudgetScopeRef {
            kind: BudgetScopeKind::Session,
            scope_id: owner.session_id.map(|id| id.to_string()),
        });
        scopes.push(BudgetScopeRef {
            kind: BudgetScopeKind::Tree,
            scope_id: owner.session_id.map(|id| id.to_string()),
        });
    }
    if owner.project_id.is_some() {
        scopes.push(BudgetScopeRef {
            kind: BudgetScopeKind::Project,
            scope_id: owner.project_id.map(|id| id.to_string()),
        });
    }
    if owner.workflow_id.is_some() {
        scopes.push(BudgetScopeRef {
            kind: BudgetScopeKind::Workflow,
            scope_id: owner.workflow_id.map(|id| id.to_string()),
        });
    }
    if owner.scheduled_job_id.is_some() {
        scopes.push(BudgetScopeRef {
            kind: BudgetScopeKind::ScheduledJob,
            scope_id: owner.scheduled_job_id.map(|id| id.to_string()),
        });
    }
    if let Some(issue_tracker_id) = owner.issue_tracker_id.clone() {
        scopes.push(BudgetScopeRef {
            kind: BudgetScopeKind::IssueTracker,
            scope_id: Some(issue_tracker_id),
        });
    }
    if let Some(graph_id) = owner.recursive_graph_id.clone() {
        scopes.push(BudgetScopeRef {
            kind: BudgetScopeKind::RecursiveGraph,
            scope_id: Some(graph_id),
        });
    }
    if let Some(operator) = owner.operator.clone() {
        scopes.push(BudgetScopeRef {
            kind: BudgetScopeKind::Operator,
            scope_id: Some(operator),
        });
    }
    scopes
}

fn synthetic_request_from_record(record: &ModelInvocationRecord) -> Result<ModelAdmissionRequest> {
    synthetic_request(
        record.purpose.as_str(),
        record.provider.as_deref(),
        record.owner.session_id.map(|id| id.to_string()),
        record.owner.project_id.map(|id| id.to_string()),
        record.owner.workflow_id.map(|id| id.to_string()),
        record.owner.scheduled_job_id.map(|id| id.to_string()),
        record.owner.issue_tracker_id.clone(),
        record.owner.topology_node_id.clone(),
        record.owner.recursive_graph_id.clone(),
        record.owner.recursive_task_id.clone(),
        record.owner.recursive_attempt_id.clone(),
        record.owner.operator.clone(),
        record.parent_invocation_id.map(|id| id.to_string()),
        record.retry_of_invocation_id.map(|id| id.to_string()),
    )
}

fn circuit_state_label(mode: ModelControlMode) -> &'static str {
    match mode {
        ModelControlMode::Normal => "closed",
        ModelControlMode::PauseBackground => "open:pause_background",
        ModelControlMode::DenyPaid => "open:deny_paid",
        ModelControlMode::LocalOnly => "open:local_only",
        ModelControlMode::StopAll => "open:stop_all",
    }
}

fn circuit_reason_label(mode: ModelControlMode) -> &'static str {
    match mode {
        ModelControlMode::Normal => {
            "interactive work remains budget-governed; paid background work is denied by default"
        }
        ModelControlMode::PauseBackground => "background model work paused",
        ModelControlMode::DenyPaid => "paid model work denied",
        ModelControlMode::LocalOnly => "only local model work allowed",
        ModelControlMode::StopAll => "all model work stopped",
    }
}

fn effective_circuit_summary(
    mode: ModelControlMode,
    circuits: &[ModelCircuitStatus],
) -> (String, String) {
    if let Some(circuit) = circuits
        .iter()
        .find(|circuit| circuit.state == "open" || circuit.state == "half_open")
    {
        let scope = circuit.scope_id.as_deref().unwrap_or("global");
        return (
            format!("{}:{}", circuit.state, scope),
            circuit.reason.clone(),
        );
    }
    (
        circuit_state_label(mode).to_string(),
        circuit_reason_label(mode).to_string(),
    )
}

fn short_uuid(id: Uuid) -> String {
    id.as_simple().to_string()[..8].to_string()
}

fn budget_scopes(
    request: &ModelAdmissionRequest,
    provider: Option<&str>,
    foreground: InvocationForeground,
    tree_scope_id: &str,
    retry_scope_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut scopes = vec![
        ("global".to_string(), "global".to_string()),
        (
            "subsystem".to_string(),
            match foreground {
                InvocationForeground::Foreground => "foreground",
                InvocationForeground::Background => "background",
            }
            .to_string(),
        ),
    ];
    if let Some(provider) = provider {
        scopes.push(("provider".to_string(), provider.to_ascii_lowercase()));
    }
    if let Some(session_id) = request.owner.session_id {
        scopes.push(("session".to_string(), session_id.to_string()));
    }
    scopes.push(("tree".to_string(), tree_scope_id.to_string()));
    if let Some(project_id) = request.owner.project_id {
        scopes.push(("project".to_string(), project_id.to_string()));
    }
    if let Some(workflow_id) = request.owner.workflow_id {
        scopes.push(("workflow".to_string(), workflow_id.to_string()));
    }
    if let Some(job_id) = request.owner.scheduled_job_id {
        scopes.push(("scheduled_job".to_string(), job_id.to_string()));
    }
    if let Some(issue_tracker_id) = request.owner.issue_tracker_id.as_deref() {
        scopes.push(("issue_tracker".to_string(), issue_tracker_id.to_string()));
    }
    if let Some(graph_id) = request.owner.recursive_graph_id.as_deref() {
        scopes.push(("recursive_graph".to_string(), graph_id.to_string()));
    }
    if let Some(operator) = request.owner.operator.as_deref() {
        scopes.push(("operator".to_string(), operator.to_string()));
    }
    if let Some(retry_scope_id) = retry_scope_id {
        scopes.push(("retry".to_string(), retry_scope_id.to_string()));
    }
    scopes
}

/// Whether a matching operator policy constrains a usage value that may only
/// be known at settlement. Call-count, concurrency, retry, and tier policies
/// intentionally do not qualify: exceeding a usage estimate cannot violate
/// those dimensions.
fn has_explicit_usage_budget_policy(
    tx: &rusqlite::Transaction<'_>,
    scopes: &[(String, String)],
    purpose: &str,
    model_tier: &str,
    effort: Option<&str>,
) -> Result<bool> {
    let mut stmt = tx.prepare(
        "SELECT EXISTS(
            SELECT 1 FROM model_budget_policies
             WHERE scope_kind = ?1 AND scope_id = ?2
               AND (purpose IS NULL OR purpose = ?3)
               AND (model_tier IS NULL OR model_tier = ?4)
               AND (effort IS NULL OR effort = ?5)
               AND (
                   max_total_tokens IS NOT NULL
                   OR max_input_tokens IS NOT NULL
                   OR max_output_tokens IS NOT NULL
                   OR max_cache_creation_tokens IS NOT NULL
                   OR max_cache_read_tokens IS NOT NULL
                   OR max_reasoning_tokens IS NOT NULL
                   OR max_embedding_inputs IS NOT NULL
                   OR max_wall_time_ms IS NOT NULL
               )
        )",
    )?;
    for (scope_kind, scope_id) in scopes {
        let applies: bool = stmt.query_row(
            params![scope_kind, scope_id, purpose, model_tier, effort],
            |row| row.get(0),
        )?;
        if applies {
            return Ok(true);
        }
    }
    Ok(false)
}

fn policy_snapshot_has_explicit_usage_budget(snapshot: &str) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(snapshot)
        .ok()
        .and_then(|value| {
            value
                .get("explicit_usage_budget_applies")
                .and_then(serde_json::Value::as_bool)
        })
}

fn counter_key(
    scope_kind: &str,
    scope_id: &str,
    purpose: &str,
    model_tier: &str,
    effort: Option<&str>,
) -> String {
    format!(
        "{scope_kind}:{scope_id}:{purpose}:{model_tier}:{}",
        effort.unwrap_or(COUNTER_ALL)
    )
}

fn counter_dimensions<'a>(
    purpose: &'a str,
    model_tier: &'a str,
    effort: Option<&'a str>,
) -> Vec<(&'a str, &'a str, &'a str)> {
    let effort = effort.unwrap_or(COUNTER_ALL);
    let mut dims = Vec::with_capacity(8);
    for counter_purpose in [purpose, COUNTER_ALL] {
        for counter_model_tier in [model_tier, COUNTER_ALL] {
            for counter_effort in [effort, COUNTER_ALL] {
                if !dims.iter().any(|candidate: &(&str, &str, &str)| {
                    candidate.0 == counter_purpose
                        && candidate.1 == counter_model_tier
                        && candidate.2 == counter_effort
                }) {
                    dims.push((counter_purpose, counter_model_tier, counter_effort));
                }
            }
        }
    }
    dims
}

fn apply_counter_increment(
    tx: &rusqlite::Transaction<'_>,
    scope_kind: &str,
    scope_id: &str,
    purpose: &str,
    model_tier: &str,
    effort: Option<&str>,
    delta: CounterDelta,
) -> Result<()> {
    for (counter_purpose, counter_model_tier, counter_effort) in
        counter_dimensions(purpose, model_tier, effort)
    {
        let key = counter_key(
            scope_kind,
            scope_id,
            counter_purpose,
            counter_model_tier,
            Some(counter_effort),
        );
        tx.execute(
            "INSERT INTO model_budget_counters (
                counter_key, scope_kind, scope_id, purpose, model_tier, effort,
                call_count, active_count, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens, reasoning_tokens,
                embedding_inputs, wall_time_ms, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(counter_key) DO UPDATE SET
                call_count = model_budget_counters.call_count + excluded.call_count,
                active_count = CASE
                    WHEN model_budget_counters.active_count + excluded.active_count < 0 THEN 0
                    ELSE model_budget_counters.active_count + excluded.active_count
                END,
                input_tokens = model_budget_counters.input_tokens + excluded.input_tokens,
                output_tokens = model_budget_counters.output_tokens + excluded.output_tokens,
                cache_creation_tokens = model_budget_counters.cache_creation_tokens + excluded.cache_creation_tokens,
                cache_read_tokens = model_budget_counters.cache_read_tokens + excluded.cache_read_tokens,
                reasoning_tokens = model_budget_counters.reasoning_tokens + excluded.reasoning_tokens,
                embedding_inputs = model_budget_counters.embedding_inputs + excluded.embedding_inputs,
                wall_time_ms = model_budget_counters.wall_time_ms + excluded.wall_time_ms,
                updated_at = excluded.updated_at",
            params![
                key,
                scope_kind,
                scope_id,
                counter_purpose,
                counter_model_tier,
                counter_effort,
                delta.calls,
                delta.active,
                delta.input_tokens,
                delta.output_tokens,
                delta.cache_creation_tokens,
                delta.cache_read_tokens,
                delta.reasoning_tokens,
                delta.embedding_inputs,
                delta.wall_time_ms,
                Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
            ],
        )?;
    }
    Ok(())
}

fn enforce_counter_policies_for_delta(
    tx: &rusqlite::Transaction<'_>,
    scope_kind: &str,
    scope_id: &str,
    purpose: &str,
    model_tier: &str,
    effort: Option<&str>,
    delta: CounterDelta,
) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT max_calls, max_concurrency, max_total_tokens, max_input_tokens, max_output_tokens,
                max_cache_creation_tokens, max_cache_read_tokens, max_reasoning_tokens,
                max_embedding_inputs, max_wall_time_ms, max_retries,
                max_calls_per_window, rate_window_seconds,
                ceiling_model_tier, ceiling_effort, purpose, model_tier, effort
         FROM model_budget_policies
         WHERE scope_kind = ?1 AND scope_id = ?2
           AND (purpose IS NULL OR purpose = ?3)
           AND (model_tier IS NULL OR model_tier = ?4)
           AND (effort IS NULL OR effort = ?5)",
    )?;
    let mut rows = stmt.query(params![scope_kind, scope_id, purpose, model_tier, effort])?;
    let mut matched_policies = Vec::new();
    let mut has_explicit_retry_budget = false;
    while let Some(row) = rows.next()? {
        let policy = CounterPolicy {
            max_calls: row.get(0)?,
            max_concurrency: row.get(1)?,
            max_total_tokens: row.get(2)?,
            max_input_tokens: row.get(3)?,
            max_output_tokens: row.get(4)?,
            max_cache_creation_tokens: row.get(5)?,
            max_cache_read_tokens: row.get(6)?,
            max_reasoning_tokens: row.get(7)?,
            max_embedding_inputs: row.get(8)?,
            max_wall_time_ms: row.get(9)?,
            max_retries: row.get(10)?,
            max_calls_per_window: row.get(11)?,
            rate_window_seconds: row.get(12)?,
            ceiling_model_tier: row.get(13)?,
            ceiling_effort: row.get(14)?,
            counter_purpose: row
                .get::<_, Option<String>>(15)?
                .unwrap_or_else(|| COUNTER_ALL.to_string()),
            counter_model_tier: row
                .get::<_, Option<String>>(16)?
                .unwrap_or_else(|| COUNTER_ALL.to_string()),
            counter_effort: row
                .get::<_, Option<String>>(17)?
                .unwrap_or_else(|| COUNTER_ALL.to_string()),
        };
        has_explicit_retry_budget |= scope_kind == "retry" && policy.max_retries.is_some();
        matched_policies.push(policy);
    }
    // Explicit operator policies DISPLACE the hardcoded defaults for their
    // scope instead of stacking on top of them. Stacking made the defaults
    // un-raisable: the lifetime global cap (1M total tokens) kept denying even
    // after the operator set a larger budget, permanently bricking paid
    // admission once real usage crossed it. This generalizes the precedent the
    // retry scope already had (explicit retry budget displaces the default
    // retry cap) — with one carve-out: a retry-scope policy that does not set
    // max_retries keeps the default retry cap, so A9's no-implicit-retry
    // guarantee survives operators who only tune retry token budgets.
    let mut policies = if matched_policies.is_empty() {
        default_counter_policies(scope_kind, effort)
    } else if scope_kind == "retry" && !has_explicit_retry_budget {
        let mut defaults = default_counter_policies(scope_kind, effort);
        defaults.retain(|policy| policy.max_retries.is_some());
        defaults
    } else {
        Vec::new()
    };
    policies.extend(matched_policies);
    for policy in policies {
        enforce_counter_policy(tx, scope_kind, scope_id, model_tier, effort, delta, &policy)?;
    }
    Ok(())
}

fn enforce_counter_policy(
    tx: &rusqlite::Transaction<'_>,
    scope_kind: &str,
    scope_id: &str,
    model_tier: &str,
    effort: Option<&str>,
    delta: CounterDelta,
    policy: &CounterPolicy,
) -> Result<()> {
    if policy
        .ceiling_model_tier
        .as_deref()
        .is_some_and(|limit| model_tier_rank(model_tier) > model_tier_rank(limit))
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} tier {model_tier} exceeds ceiling {}",
            policy.ceiling_model_tier.as_deref().unwrap_or_default()
        )));
    }
    if policy
        .ceiling_effort
        .as_deref()
        .is_some_and(|limit| effort_rank(effort) > effort_rank(Some(limit)))
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} effort {} exceeds ceiling {}",
            effort.unwrap_or(COUNTER_ALL),
            policy.ceiling_effort.as_deref().unwrap_or_default()
        )));
    }

    let key = counter_key(
        scope_kind,
        scope_id,
        &policy.counter_purpose,
        &policy.counter_model_tier,
        Some(&policy.counter_effort),
    );
    let current = tx
        .query_row(
            "SELECT call_count, active_count, input_tokens, output_tokens, cache_creation_tokens,
                    cache_read_tokens, reasoning_tokens, embedding_inputs, wall_time_ms,
                    rate_window_started_at, rate_window_call_count
             FROM model_budget_counters WHERE counter_key = ?1",
            params![key],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?
        .unwrap_or((0, 0, 0, 0, 0, 0, 0, 0, 0, None, 0));
    let projected_calls = current.0.saturating_add(delta.calls);
    let projected_active = (current.1 + delta.active).max(0);
    let projected_input = current.2.saturating_add(delta.input_tokens);
    let projected_output = current.3.saturating_add(delta.output_tokens);
    let projected_cache_creation = current.4.saturating_add(delta.cache_creation_tokens);
    let projected_cache_read = current.5.saturating_add(delta.cache_read_tokens);
    let projected_reasoning = current.6.saturating_add(delta.reasoning_tokens);
    let projected_embedding = current.7.saturating_add(delta.embedding_inputs);
    let projected_wall_time = current.8.saturating_add(delta.wall_time_ms);
    let projected_total_tokens = projected_input
        .saturating_add(projected_output)
        .saturating_add(projected_cache_creation)
        .saturating_add(projected_cache_read)
        .saturating_add(projected_reasoning);

    if policy
        .max_calls
        .is_some_and(|limit| projected_calls > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} call count {projected_calls} > {}",
            policy.max_calls.unwrap_or_default()
        )));
    }
    if policy
        .max_concurrency
        .is_some_and(|limit| projected_active > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} active count {projected_active} > {}",
            policy.max_concurrency.unwrap_or_default()
        )));
    }
    if policy
        .max_total_tokens
        .is_some_and(|limit| projected_total_tokens > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} tokens {projected_total_tokens} > {}",
            policy.max_total_tokens.unwrap_or_default()
        )));
    }
    if policy
        .max_input_tokens
        .is_some_and(|limit| projected_input > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} input tokens {projected_input} > {}",
            policy.max_input_tokens.unwrap_or_default()
        )));
    }
    if policy
        .max_output_tokens
        .is_some_and(|limit| projected_output > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} output tokens {projected_output} > {}",
            policy.max_output_tokens.unwrap_or_default()
        )));
    }
    if policy
        .max_cache_creation_tokens
        .is_some_and(|limit| projected_cache_creation > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} cache creation tokens {projected_cache_creation} > {}",
            policy.max_cache_creation_tokens.unwrap_or_default()
        )));
    }
    if policy
        .max_cache_read_tokens
        .is_some_and(|limit| projected_cache_read > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} cache read tokens {projected_cache_read} > {}",
            policy.max_cache_read_tokens.unwrap_or_default()
        )));
    }
    if policy
        .max_reasoning_tokens
        .is_some_and(|limit| projected_reasoning > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} reasoning tokens {projected_reasoning} > {}",
            policy.max_reasoning_tokens.unwrap_or_default()
        )));
    }
    if policy
        .max_embedding_inputs
        .is_some_and(|limit| projected_embedding > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} embedding inputs {projected_embedding} > {}",
            policy.max_embedding_inputs.unwrap_or_default()
        )));
    }
    if policy
        .max_wall_time_ms
        .is_some_and(|limit| projected_wall_time > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model budget exceeded: scope {scope_kind}/{scope_id} wall time {projected_wall_time} > {}",
            policy.max_wall_time_ms.unwrap_or_default()
        )));
    }
    if scope_kind == "retry"
        && policy
            .max_retries
            .is_some_and(|limit| projected_calls > limit)
    {
        return Err(DaemonError::PolicyDenied(format!(
            "model retry budget exceeded: scope {scope_kind}/{scope_id} retries {projected_calls} > {}",
            policy.max_retries.unwrap_or_default()
        )));
    }
    if let (Some(limit), Some(window_seconds)) =
        (policy.max_calls_per_window, policy.rate_window_seconds)
    {
        let projected_window_calls = count_invocations_in_window_tx(
            tx,
            scope_kind,
            scope_id,
            &policy.counter_purpose,
            &policy.counter_model_tier,
            &policy.counter_effort,
            window_seconds,
        )?
        .saturating_add(delta.calls);
        if projected_window_calls > limit {
            return Err(DaemonError::PolicyDenied(format!(
                "model rate limit exceeded: scope {scope_kind}/{scope_id} calls/window {projected_window_calls} > {limit}"
            )));
        }
    }
    Ok(())
}

fn normalize_completion_value(
    completion_value: Option<u64>,
    baseline: i64,
    current_value: Option<i64>,
) -> Option<i64> {
    let Some(value) = completion_value else {
        return current_value;
    };
    let normalized = value.saturating_sub(baseline.max(0) as u64) as i64;
    Some(current_value.unwrap_or(0).max(normalized))
}

fn counter_delta(next: Option<i64>, current: Option<i64>) -> i64 {
    next.unwrap_or(0).saturating_sub(current.unwrap_or(0))
}

fn reservation_from_request(
    request: &ModelAdmissionRequest,
    registry: RegistryEntry,
    model_tier: ModelTier,
) -> Result<ReservationUsage> {
    let estimate = request
        .expected_usage
        .clone()
        .unwrap_or_else(|| default_expected_usage(registry, model_tier));
    Ok(ReservationUsage {
        input_tokens: i64::try_from(estimate.input_tokens)
            .map_err(|_| DaemonError::Store("input token estimate overflow".to_string()))?,
        output_tokens: i64::try_from(estimate.output_tokens)
            .map_err(|_| DaemonError::Store("output token estimate overflow".to_string()))?,
        cache_creation_tokens: i64::try_from(estimate.cache_creation_tokens).map_err(|_| {
            DaemonError::Store("cache creation token estimate overflow".to_string())
        })?,
        cache_read_tokens: i64::try_from(estimate.cache_read_tokens)
            .map_err(|_| DaemonError::Store("cache read token estimate overflow".to_string()))?,
        reasoning_tokens: i64::try_from(estimate.reasoning_tokens)
            .map_err(|_| DaemonError::Store("reasoning token estimate overflow".to_string()))?,
        embedding_input_count: i64::try_from(estimate.embedding_input_count)
            .map_err(|_| DaemonError::Store("embedding input estimate overflow".to_string()))?,
        wall_time_ms: i64::try_from(estimate.wall_time_ms)
            .map_err(|_| DaemonError::Store("wall time estimate overflow".to_string()))?,
    })
}

fn default_expected_usage(registry: RegistryEntry, model_tier: ModelTier) -> ExpectedUsage {
    let (input_tokens, output_tokens, cache_creation_tokens, reasoning_tokens, wall_time_ms) =
        match (registry.foreground, model_tier) {
            (InvocationForeground::Background, ModelTier::Premium) => {
                (18_000, 4_000, 6_000, 1_500, 480_000)
            }
            (InvocationForeground::Background, ModelTier::Standard) => {
                (12_000, 3_000, 3_000, 1_000, 360_000)
            }
            (InvocationForeground::Background, ModelTier::Local) => {
                (8_000, 2_000, 1_000, 0, 240_000)
            }
            (InvocationForeground::Foreground, ModelTier::Premium) => {
                (24_000, 6_000, 8_000, 2_000, 600_000)
            }
            (InvocationForeground::Foreground, ModelTier::Standard) => {
                (16_000, 4_000, 4_000, 1_000, 420_000)
            }
            (InvocationForeground::Foreground, ModelTier::Local) => {
                (10_000, 3_000, 1_000, 0, 300_000)
            }
        };
    ExpectedUsage {
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens: 0,
        reasoning_tokens,
        embedding_input_count: if registry.kind == ModelInvocationKind::Embedding {
            256
        } else {
            0
        },
        wall_time_ms,
    }
}

fn load_existing_dedup_row_tx(
    tx: &rusqlite::Transaction<'_>,
    dedup_key: &str,
) -> Result<Option<ExistingDedupRow>> {
    tx.query_row(
        "SELECT id, admission_status, status, purpose, request_fingerprint, session_id, project_id,
                workflow_id, scheduled_job_id, issue_tracker_id, issue_identifier, topology_node_id,
                recursive_graph_id, recursive_task_id, recursive_attempt_id, operator
         FROM model_invocations
         WHERE dedup_key = ?1",
        params![dedup_key],
        |row| {
            let id: String = row.get(0)?;
            Ok(ExistingDedupRow {
                id: Uuid::parse_str(&id).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
                admission_status: row.get(1)?,
                status: row.get(2)?,
                purpose: row.get(3)?,
                request_fingerprint: row.get(4)?,
                session_id: row.get(5)?,
                project_id: row.get(6)?,
                workflow_id: row.get(7)?,
                scheduled_job_id: row.get(8)?,
                issue_tracker_id: row.get(9)?,
                issue_identifier: row.get(10)?,
                topology_node_id: row.get(11)?,
                recursive_graph_id: row.get(12)?,
                recursive_task_id: row.get(13)?,
                recursive_attempt_id: row.get(14)?,
                operator: row.get(15)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Whether an existing dedup row represents a dead attempt whose key should be
/// released so identical content can be retried.
///
/// Retryable — the attempt is over and produced nothing:
///   - `failed`    : terminal failure (provider error, process crash)
///   - `cancelled` : abandoned before producing a result
///
/// NOT retryable — suppression is still doing useful work:
///   - `running` / `cancellation_requested` : in flight. This is the case dedup
///     exists for; releasing here would permit concurrent double-spend.
///   - `completed` : the work is done; the caller should reuse the result.
///   - `denied`    : a policy decision that must keep applying.
///   - anything unrecognized : conservative — keep suppressing rather than risk
///     re-spending on a status this code does not understand.
fn dedup_row_is_retryable(existing: &ExistingDedupRow) -> bool {
    matches!(
        parse_invocation_status_lossy(&existing.status),
        ModelInvocationStatus::Failed | ModelInvocationStatus::Cancelled
    )
}

fn dedup_row_matches_request(existing: &ExistingDedupRow, request: &ModelAdmissionRequest) -> bool {
    // A scheduled Fresh replay deliberately reuses its job-and-due-slot key,
    // but each launch attempt receives a new prospective session UUID before
    // admission. The scheduled-job owner plus purpose/fingerprint below are
    // the stable replay identity; requiring the transient session UUID here
    // would turn a safe replay into a dedup conflict before provider dispatch.
    let same_session = existing.session_id == request.owner.session_id.map(|id| id.to_string());
    let same_scheduled_job_replay = request.owner.scheduled_job_id.is_some()
        && existing.scheduled_job_id == request.owner.scheduled_job_id.map(|id| id.to_string());
    existing.purpose == request.purpose.as_str()
        && existing.request_fingerprint == request.request_fingerprint
        && (same_session || same_scheduled_job_replay)
        && existing.project_id == request.owner.project_id.map(|id| id.to_string())
        && existing.workflow_id == request.owner.workflow_id.map(|id| id.to_string())
        && existing.scheduled_job_id == request.owner.scheduled_job_id.map(|id| id.to_string())
        && existing.issue_tracker_id == request.owner.issue_tracker_id
        && existing.issue_identifier == request.owner.issue_identifier
        && existing.topology_node_id == request.owner.topology_node_id
        && existing.recursive_graph_id == request.owner.recursive_graph_id
        && existing.recursive_task_id == request.owner.recursive_task_id
        && existing.recursive_attempt_id == request.owner.recursive_attempt_id
        && existing.operator == request.owner.operator
}

fn default_counter_policies(scope_kind: &str, effort: Option<&str>) -> Vec<CounterPolicy> {
    let mut policies = Vec::new();
    // Budgets are unbounded by default. "global", "provider", "tree", and
    // background "subsystem" scopes used to carry hardcoded ceilings here
    // (1M/500k/300k/250k tokens etc.) that stacked underneath any operator
    // override and, once real usage crossed them, permanently denied
    // admission with no way to raise them shy of hand-editing SQLite (see
    // the `matched_policies` fallback logic below for the fix to the
    // stacking half of that bug). The remaining half is this: don't cap
    // anything until an operator explicitly inserts a row in
    // `model_budget_policies` for that scope. A constraint applies the
    // moment one is placed; absent one, there is none.
    //
    // "retry" is deliberately excluded from this — its default isn't a
    // cost budget, it's the A9 no-implicit-retry invariant (interactive
    // sessions don't auto-retry unless the operator opts in), and it must
    // keep applying even when an operator sets an explicit retry-scope
    // token budget without touching `max_retries` (see the
    // `has_explicit_retry_budget` carve-out above).
    if scope_kind == "retry" {
        policies.push(CounterPolicy {
            max_calls: None,
            max_concurrency: Some(1),
            max_total_tokens: None,
            max_input_tokens: None,
            max_output_tokens: None,
            max_cache_creation_tokens: None,
            max_cache_read_tokens: None,
            max_reasoning_tokens: None,
            max_embedding_inputs: None,
            max_wall_time_ms: None,
            max_retries: Some(0),
            max_calls_per_window: Some(1),
            rate_window_seconds: Some(300),
            ceiling_model_tier: None,
            ceiling_effort: effort.map(str::to_string),
            counter_purpose: COUNTER_ALL.to_string(),
            counter_model_tier: COUNTER_ALL.to_string(),
            counter_effort: COUNTER_ALL.to_string(),
        });
    }
    policies
}

fn model_tier_rank(value: &str) -> u8 {
    match value {
        "local" => 0,
        "standard" => 1,
        "premium" => 2,
        _ => 1,
    }
}

fn effort_rank(value: Option<&str>) -> u8 {
    match value.unwrap_or(COUNTER_ALL) {
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "xhigh" => 4,
        "max" => 5,
        "ultra" => 6,
        _ => 0,
    }
}

fn count_invocations_in_window_tx(
    tx: &rusqlite::Transaction<'_>,
    scope_kind: &str,
    scope_id: &str,
    counter_purpose: &str,
    counter_model_tier: &str,
    counter_effort: &str,
    window_seconds: i64,
) -> Result<i64> {
    let cutoff = (Utc::now() - chrono::Duration::seconds(window_seconds))
        .to_rfc3339_opts(SecondsFormat::Nanos, true);
    match scope_kind {
        "global" => tx
            .query_row(
                "SELECT COUNT(*) FROM model_invocations
                 WHERE admission_status = 'admitted'
                   AND created_at >= ?1
                   AND (?2 = '__all__' OR purpose = ?2)
                   AND (?3 = '__all__' OR COALESCE(model_tier, 'standard') = ?3)
                   AND (?4 = '__all__' OR COALESCE(effort, '__all__') = ?4)",
                params![cutoff, counter_purpose, counter_model_tier, counter_effort],
                |row| row.get::<_, i64>(0),
            )
            .map_err(Into::into),
        "provider" => tx
            .query_row(
                "SELECT COUNT(*) FROM model_invocations
                 WHERE admission_status = 'admitted'
                   AND created_at >= ?1
                   AND lower(COALESCE(provider, '')) = ?5
                   AND (?2 = '__all__' OR purpose = ?2)
                   AND (?3 = '__all__' OR COALESCE(model_tier, 'standard') = ?3)
                   AND (?4 = '__all__' OR COALESCE(effort, '__all__') = ?4)",
                params![
                    cutoff,
                    counter_purpose,
                    counter_model_tier,
                    counter_effort,
                    scope_id
                ],
                |row| row.get::<_, i64>(0),
            )
            .map_err(Into::into),
        "subsystem" => tx
            .query_row(
                "SELECT COUNT(*) FROM model_invocations
                 WHERE admission_status = 'admitted'
                   AND created_at >= ?1
                   AND foreground = ?5
                   AND (?2 = '__all__' OR purpose = ?2)
                   AND (?3 = '__all__' OR COALESCE(model_tier, 'standard') = ?3)
                   AND (?4 = '__all__' OR COALESCE(effort, '__all__') = ?4)",
                params![
                    cutoff,
                    counter_purpose,
                    counter_model_tier,
                    counter_effort,
                    scope_id
                ],
                |row| row.get::<_, i64>(0),
            )
            .map_err(Into::into),
        "retry" => tx
            .query_row(
                "SELECT COUNT(*) FROM model_invocations
                 WHERE admission_status = 'admitted'
                   AND created_at >= ?1
                   AND retry_of_invocation_id = ?5
                   AND (?2 = '__all__' OR purpose = ?2)
                   AND (?3 = '__all__' OR COALESCE(model_tier, 'standard') = ?3)
                   AND (?4 = '__all__' OR COALESCE(effort, '__all__') = ?4)",
                params![
                    cutoff,
                    counter_purpose,
                    counter_model_tier,
                    counter_effort,
                    scope_id
                ],
                |row| row.get::<_, i64>(0),
            )
            .map_err(Into::into),
        _ => Ok(0),
    }
}

#[derive(Debug, Clone, Copy)]
struct LineageScopeIds {
    tree_scope_id: Uuid,
    retry_scope_id: Option<Uuid>,
}

fn resolve_lineage_scope_ids_tx(
    tx: &rusqlite::Transaction<'_>,
    invocation_id: Uuid,
    parent_invocation_id: Option<Uuid>,
    retry_of_invocation_id: Option<Uuid>,
) -> Result<LineageScopeIds> {
    let tree_scope_id = resolve_tree_scope_id_tx(
        tx,
        invocation_id,
        parent_invocation_id,
        retry_of_invocation_id,
    )?;
    let retry_scope_id = retry_of_invocation_id
        .map(|retry_id| resolve_retry_scope_id_tx(tx, retry_id))
        .transpose()?;
    Ok(LineageScopeIds {
        tree_scope_id,
        retry_scope_id,
    })
}

fn scope_id_for_policy(kind: BudgetScopeKind, scope_id: Option<&str>) -> Result<String> {
    match kind {
        BudgetScopeKind::Global => Ok("global".to_string()),
        _ => scope_id
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                DaemonError::InvalidParam(format!(
                    "model budget policy requires scope_id for {:?}",
                    kind
                ))
            }),
    }
}

fn validate_model_budget_policy(policy: &ModelBudgetPolicy) -> Result<()> {
    if policy.max_calls.is_none()
        && policy.max_total_tokens.is_none()
        && policy.max_input_tokens.is_none()
        && policy.max_output_tokens.is_none()
        && policy.max_embedding_inputs.is_none()
        && policy.max_wall_time_ms.is_none()
        && policy.max_concurrency.is_none()
        && policy.max_retries.is_none()
    {
        return Err(DaemonError::InvalidParam(
            "model budget policy must set at least one limit".to_string(),
        ));
    }
    if let Some(ratio) = policy.alert_threshold_ratio
        && (!(0.0..=1.0).contains(&ratio) || ratio == 0.0)
    {
        return Err(DaemonError::InvalidParam(
            "model budget policy alert_threshold_ratio must be in (0, 1]".to_string(),
        ));
    }
    let _ = scope_id_for_policy(policy.scope_kind, policy.scope_id.as_deref())?;
    Ok(())
}

fn validate_model_circuit_status(circuit: &ModelCircuitStatus) -> Result<()> {
    if circuit.state.trim().is_empty() {
        return Err(DaemonError::InvalidParam(
            "model circuit state must not be empty".to_string(),
        ));
    }
    if circuit.reason.trim().is_empty() {
        return Err(DaemonError::InvalidParam(
            "model circuit reason must not be empty".to_string(),
        ));
    }
    let _ = scope_id_for_policy(circuit.scope_kind, circuit.scope_id.as_deref())?;
    Ok(())
}

fn synthetic_request(
    purpose: &str,
    provider: Option<&str>,
    session_id: Option<String>,
    project_id: Option<String>,
    workflow_id: Option<String>,
    scheduled_job_id: Option<String>,
    issue_tracker_id: Option<String>,
    topology_node_id: Option<String>,
    recursive_graph_id: Option<String>,
    recursive_task_id: Option<String>,
    recursive_attempt_id: Option<String>,
    operator: Option<String>,
    parent_invocation_id: Option<String>,
    retry_of_invocation_id: Option<String>,
) -> Result<ModelAdmissionRequest> {
    use rsi_common::model_control::ModelInvocationPurpose as Purpose;
    let purpose = match purpose {
        "session.launch.fresh" => Purpose::SessionLaunchFresh,
        "session.continue.resume" => Purpose::SessionContinueResume,
        "session.retry.auto" => Purpose::SessionRetryAuto,
        "session.rotate.child" => Purpose::SessionRotateChild,
        "session.harness.turn" => Purpose::SessionHarnessTurn,
        "session.harness.compaction" => Purpose::SessionHarnessCompaction,
        "session.codex_app_server.turn" => Purpose::SessionCodexAppServerTurn,
        "session.openai_compatible.turn" => Purpose::SessionOpenAiCompatibleTurn,
        "agent.spawn_child" => Purpose::AgentSpawnChild,
        "agent.reserve_successor" => Purpose::AgentReserveSuccessor,
        "workflow.graph.node" => Purpose::WorkflowGraphNode,
        "workflow.chain.iteration" => Purpose::WorkflowChainIteration,
        "recursive.live.task" => Purpose::RecursiveLiveTask,
        "scheduled.fresh" => Purpose::ScheduledFresh,
        "agent.schedule_wake.fresh" => Purpose::AgentScheduleWakeFresh,
        "scheduled.resume.watch" => Purpose::ScheduledResumeWatch,
        "issue_tracker.dispatch" => Purpose::IssueTrackerDispatch,
        "prompt.compile" => Purpose::PromptCompile,
        "text.generate.rpc" => Purpose::TextGenerateRpc,
        "session.title" => Purpose::SessionTitle,
        "session.summary" => Purpose::SessionSummary,
        "memory.observation.extract" => Purpose::MemoryObservationExtract,
        "memory.embedding.index" => Purpose::MemoryEmbeddingIndex,
        "dream.consolidation" => Purpose::DreamConsolidation,
        "stall.classifier" => Purpose::StallClassifier,
        "dialectic.query" => Purpose::DialecticQuery,
        "model.discovery.claude_probe" => Purpose::ModelDiscoveryClaudeProbe,
        "queue.deferred_model_task" => Purpose::QueueDeferredModelTask,
        other => {
            return Err(DaemonError::Store(format!(
                "unknown model invocation purpose in ledger row: {other}"
            )));
        }
    };
    Ok(ModelAdmissionRequest {
        purpose,
        provider: provider.map(str::to_string),
        model: None,
        backend: None,
        effort: None,
        trigger: "completion".to_string(),
        owner: rsi_common::model_control::InvocationOwner {
            session_id: session_id
                .as_deref()
                .map(Uuid::parse_str)
                .transpose()
                .map_err(|e| {
                    DaemonError::Store(format!("invalid session_id in model_invocations: {e}"))
                })?,
            project_id: project_id
                .as_deref()
                .map(Uuid::parse_str)
                .transpose()
                .map_err(|e| {
                    DaemonError::Store(format!("invalid project_id in model_invocations: {e}"))
                })?,
            workflow_id: workflow_id
                .as_deref()
                .map(Uuid::parse_str)
                .transpose()
                .map_err(|e| {
                    DaemonError::Store(format!("invalid workflow_id in model_invocations: {e}"))
                })?,
            scheduled_job_id: scheduled_job_id
                .as_deref()
                .map(Uuid::parse_str)
                .transpose()
                .map_err(|e| {
                    DaemonError::Store(format!(
                        "invalid scheduled_job_id in model_invocations: {e}"
                    ))
                })?,
            issue_tracker_id,
            topology_node_id,
            recursive_graph_id,
            recursive_task_id,
            recursive_attempt_id,
            operator,
            ..Default::default()
        },
        dedup_key: None,
        request_fingerprint: None,
        parent_invocation_id: parse_optional_uuid_field(
            "parent_invocation_id",
            parent_invocation_id.as_deref(),
        )?,
        retry_of_invocation_id: parse_optional_uuid_field(
            "retry_of_invocation_id",
            retry_of_invocation_id.as_deref(),
        )?,
        expected_usage: Some(crate::model_control::explicit_expected_usage(
            purpose, provider, None, None,
        )),
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    })
}

fn parse_optional_uuid_field(label: &str, value: Option<&str>) -> Result<Option<Uuid>> {
    value
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| DaemonError::Store(format!("invalid {label} in model_invocations: {e}")))
}

fn resolve_tree_scope_id_tx(
    tx: &rusqlite::Transaction<'_>,
    invocation_id: Uuid,
    parent_invocation_id: Option<Uuid>,
    retry_of_invocation_id: Option<Uuid>,
) -> Result<Uuid> {
    let mut candidate_roots = Vec::new();
    if let Some(parent_id) = parent_invocation_id {
        candidate_roots.push(resolve_existing_tree_root_tx(tx, parent_id)?);
    }
    if let Some(retry_id) = retry_of_invocation_id {
        candidate_roots.push(resolve_existing_tree_root_tx(tx, retry_id)?);
    }
    if candidate_roots.is_empty() {
        return Ok(invocation_id);
    }
    let root = candidate_roots[0];
    if candidate_roots.iter().any(|candidate| *candidate != root) {
        return Err(DaemonError::PolicyDenied(
            "model invocation lineage has conflicting tree roots".to_string(),
        ));
    }
    Ok(root)
}

fn resolve_retry_scope_id_tx(
    tx: &rusqlite::Transaction<'_>,
    retry_of_invocation_id: Uuid,
) -> Result<Uuid> {
    let mut seen = HashSet::new();
    let mut current = retry_of_invocation_id;
    loop {
        if !seen.insert(current) {
            return Err(DaemonError::PolicyDenied(
                "model invocation retry lineage contains a cycle".to_string(),
            ));
        }
        let (parent_raw, retry_raw) = lookup_parent_links_tx(tx, current)?.ok_or_else(|| {
            DaemonError::PolicyDenied(format!(
                "model invocation retry lineage missing ancestor {current}"
            ))
        })?;
        let retry_link = parse_optional_uuid_field("retry_of_invocation_id", retry_raw.as_deref())?;
        if let Some(next) = retry_link {
            current = next;
            continue;
        }
        if parent_raw.is_some() {
            return resolve_existing_tree_root_tx(tx, current);
        }
        return Ok(current);
    }
}

fn resolve_existing_tree_root_tx(tx: &rusqlite::Transaction<'_>, start: Uuid) -> Result<Uuid> {
    enum Frame {
        Enter(Uuid),
        Resolve {
            invocation_id: Uuid,
            parent_id: Option<Uuid>,
            retry_id: Option<Uuid>,
        },
    }

    let mut frames = vec![Frame::Enter(start)];
    let mut active = HashSet::new();
    let mut resolved = HashMap::new();

    while let Some(frame) = frames.pop() {
        match frame {
            Frame::Enter(invocation_id) => {
                if resolved.contains_key(&invocation_id) {
                    continue;
                }
                if !active.insert(invocation_id) {
                    return Err(DaemonError::PolicyDenied(
                        "model invocation lineage contains a cycle".to_string(),
                    ));
                }
                let Some((parent_raw, retry_raw)) = lookup_parent_links_tx(tx, invocation_id)?
                else {
                    return Err(DaemonError::PolicyDenied(format!(
                        "model invocation lineage missing ancestor {invocation_id}"
                    )));
                };
                let parent_id =
                    parse_optional_uuid_field("parent_invocation_id", parent_raw.as_deref())?;
                let retry_id =
                    parse_optional_uuid_field("retry_of_invocation_id", retry_raw.as_deref())?;

                frames.push(Frame::Resolve {
                    invocation_id,
                    parent_id,
                    retry_id,
                });
                if let Some(retry_id) = retry_id
                    && !resolved.contains_key(&retry_id)
                {
                    frames.push(Frame::Enter(retry_id));
                }
                if let Some(parent_id) = parent_id
                    && !resolved.contains_key(&parent_id)
                {
                    frames.push(Frame::Enter(parent_id));
                }
            }
            Frame::Resolve {
                invocation_id,
                parent_id,
                retry_id,
            } => {
                active.remove(&invocation_id);
                let parent_root = parent_id.and_then(|id| resolved.get(&id).copied());
                let retry_root = retry_id.and_then(|id| resolved.get(&id).copied());
                let root = match (parent_id, retry_id, parent_root, retry_root) {
                    (None, None, None, None) => invocation_id,
                    (Some(_), None, Some(root), None) | (None, Some(_), None, Some(root)) => root,
                    (Some(_), Some(_), Some(parent_root), Some(retry_root)) => {
                        if parent_root != retry_root {
                            return Err(DaemonError::PolicyDenied(
                                "model invocation lineage has conflicting parent/retry roots"
                                    .to_string(),
                            ));
                        }
                        parent_root
                    }
                    _ => {
                        return Err(DaemonError::Store(
                            "model invocation lineage traversal left an unresolved ancestor"
                                .to_string(),
                        ));
                    }
                };
                resolved.insert(invocation_id, root);
            }
        }
    }

    resolved.get(&start).copied().ok_or_else(|| {
        DaemonError::Store(
            "model invocation lineage traversal did not resolve its root".to_string(),
        )
    })
}

fn lookup_parent_links_tx(
    tx: &rusqlite::Transaction<'_>,
    invocation_id: Uuid,
) -> Result<Option<(Option<String>, Option<String>)>> {
    tx.query_row(
        "SELECT parent_invocation_id, retry_of_invocation_id
         FROM model_invocations WHERE id = ?1",
        params![invocation_id.to_string()],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        },
    )
    .optional()
    .map_err(Into::into)
}

fn enforce_orchestration_tier_escalation(
    tx: &rusqlite::Transaction<'_>,
    registry: RegistryEntry,
    model_tier: ModelTier,
    effort: Option<&str>,
    tree_scope_id: Uuid,
) -> Result<()> {
    if registry.kind != ModelInvocationKind::Orchestration {
        return Ok(());
    }
    let root_route = tx
        .query_row(
            "SELECT model_tier, effort FROM model_invocations WHERE id = ?1",
            params![tree_scope_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()?;
    let Some((root_tier, root_effort)) = root_route else {
        return Ok(());
    };
    let child_tier = to_model_tier(model_tier);
    if root_tier
        .as_deref()
        .is_some_and(|tier| model_tier_rank(child_tier) > model_tier_rank(tier))
    {
        return Err(DaemonError::PolicyDenied(format!(
            "orchestration model tier escalation denied by tree root tier {root_tier:?}; \
             model tier is the cost-dominant guardrail and has no operator override — \
             relaunch the tree root at the higher tier instead"
        )));
    }

    // Effort ceiling (issue #34). The limit is the operator's declared ceiling
    // when `daemon_settings[KEY_ORCHESTRATION_MAX_CHILD_EFFORT]` names a known
    // effort, and the tree root's own effort otherwise — so with the key unset
    // this is bit-for-bit the pre-#34 rule.
    let operator_ceiling = operator_max_child_effort_tx(tx)?;
    let (limit, limited_by) = match &operator_ceiling {
        OperatorEffortCeiling::Set(value) => (Some(value.as_str()), EffortLimitSource::Operator),
        OperatorEffortCeiling::Unset | OperatorEffortCeiling::Invalid(_) => {
            (root_effort.as_deref(), EffortLimitSource::TreeRoot)
        }
    };
    if limit.is_some_and(|limit| effort_rank(effort) > effort_rank(Some(limit))) {
        let source = match limited_by {
            EffortLimitSource::Operator => {
                format!("operator ceiling daemon_settings['{KEY_ORCHESTRATION_MAX_CHILD_EFFORT}']")
            }
            EffortLimitSource::TreeRoot => "tree root effort".to_string(),
        };
        // Name the knob. The pre-#34 message stated the limit but never said
        // how to raise it, which is why the only discovered workaround was to
        // relaunch the whole tree at a higher effort.
        let remedy = match limited_by {
            EffortLimitSource::Operator => String::new(),
            EffortLimitSource::TreeRoot => format!(
                "; raise it for this tree by setting the daemon_settings key \
                 '{KEY_ORCHESTRATION_MAX_CHILD_EFFORT}' to one of \
                 low|medium|high|xhigh|max|ultra"
            ),
        };
        let ignored = match &operator_ceiling {
            OperatorEffortCeiling::Invalid(raw) => format!(
                "; ignoring unrecognized \
                 daemon_settings['{KEY_ORCHESTRATION_MAX_CHILD_EFFORT}']={raw:?}"
            ),
            OperatorEffortCeiling::Set(_) | OperatorEffortCeiling::Unset => String::new(),
        };
        return Err(DaemonError::PolicyDenied(format!(
            "orchestration effort escalation denied by {source} {limit:?}{remedy}{ignored}"
        )));
    }
    Ok(())
}

/// Which of the two candidate limits actually capped an effort request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortLimitSource {
    /// `daemon_settings[KEY_ORCHESTRATION_MAX_CHILD_EFFORT]`.
    Operator,
    /// The tree root's own `model_invocations.effort` (the pre-#34 default).
    TreeRoot,
}

/// The operator's declared child-effort ceiling, read from `daemon_settings`.
///
/// `Invalid` is deliberately distinct from `Unset`: both fall back to the tree
/// root's effort, but only `Invalid` is reported in the denial message, so an
/// operator who typo'd the value learns about it exactly when it bites instead
/// of silently continuing to see the old ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OperatorEffortCeiling {
    Unset,
    Set(String),
    Invalid(String),
}

/// One `daemon_settings.value` cell, classified by storage class.
///
/// Read as raw bytes rather than as `String`/`rusqlite::types::Value` on
/// purpose — see [`operator_max_child_effort_tx`].
enum RawCeilingCell {
    /// A TEXT cell holding valid UTF-8.
    Text(String),
    /// A NULL cell — absence, not a malformed value.
    Null,
    /// Any cell that cannot name an effort, labelled for the denial message.
    NonText(&'static str),
}

fn operator_max_child_effort_tx(tx: &rusqlite::Transaction<'_>) -> Result<OperatorEffortCeiling> {
    // R8 LOW-2. `daemon_settings.value` is declared TEXT, but TEXT affinity
    // coerces only INTEGER and REAL — a BLOB written by the raw-SQL access
    // AGENTS.md sanctions for diagnostics survives as a BLOB. Reading this cell
    // as `String` turned that into `Err(InvalidColumnType)`, which propagated
    // out of the admission transaction into `admit_model_invocation`, where a
    // single `if let Err(..)` treats every error as a policy denial: ONE
    // malformed row denied EVERY orchestration spawn, with a raw database
    // string as the operator-visible reason and no hint that this key was at
    // fault. It also split the precheck from the authority, since
    // `preview_orchestration_escalation` fails that same error open.
    //
    // So the read path degrades a malformed value to "unset" instead of
    // erroring. That is safe in the only direction that matters: "unset" falls
    // back to the tree root's own effort, which is the pre-#34 rule and is
    // strictly TIGHTER than any operator ceiling that raises the limit. A
    // malformed setting can therefore never widen the guardrail issue #2
    // exists to enforce — it can only fail closed, and it can no longer take
    // orchestration admission down with it.
    //
    // `get_ref` is the right primitive here: `row.get::<_, String>` errors on a
    // BLOB, and `row.get::<_, rusqlite::types::Value>` is worse still — its
    // `From<ValueRef>` impl PANICS on a TEXT cell holding invalid UTF-8. Only
    // the raw byte slice lets every storage class be handled totally.
    let raw: Option<RawCeilingCell> = tx
        .query_row(
            "SELECT value FROM daemon_settings WHERE key = ?1",
            params![KEY_ORCHESTRATION_MAX_CHILD_EFFORT],
            |row| {
                Ok(match row.get_ref(0)? {
                    rusqlite::types::ValueRef::Null => RawCeilingCell::Null,
                    rusqlite::types::ValueRef::Integer(_) => RawCeilingCell::NonText("integer"),
                    rusqlite::types::ValueRef::Real(_) => RawCeilingCell::NonText("real"),
                    rusqlite::types::ValueRef::Blob(_) => RawCeilingCell::NonText("blob"),
                    rusqlite::types::ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
                        Ok(text) => RawCeilingCell::Text(text.to_string()),
                        Err(_) => RawCeilingCell::NonText("non-utf8 text"),
                    },
                })
            },
        )
        .optional()?;
    let raw = match raw {
        // A missing row and a NULL cell are both absence, not a typo, so they
        // report as `Unset` rather than nagging in the denial message. The
        // column is NOT NULL, so the NULL arm is defence in depth.
        None | Some(RawCeilingCell::Null) => return Ok(OperatorEffortCeiling::Unset),
        Some(RawCeilingCell::NonText(kind)) => {
            return Ok(OperatorEffortCeiling::Invalid(format!("<{kind} value>")));
        }
        Some(RawCeilingCell::Text(text)) => text,
    };
    let normalized = raw.trim().to_ascii_lowercase();
    // An explicitly cleared ceiling is a deliberate operator choice, not a
    // typo, so it must not be reported as "unrecognized" when it bites.
    if normalized.is_empty()
        || normalized == rsi_common::model_control::ORCHESTRATION_MAX_CHILD_EFFORT_UNSET
    {
        return Ok(OperatorEffortCeiling::Unset);
    }
    // `effort_rank` maps every unknown string to 0. Accepting one here would
    // install a ceiling that denies every child naming any effort at all, so an
    // unrecognized value must degrade to the tree-root default, never to
    // "deny everything".
    if effort_rank(Some(normalized.as_str())) == 0 {
        return Ok(OperatorEffortCeiling::Invalid(raw));
    }
    Ok(OperatorEffortCeiling::Set(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_control::registry;
    use crate::store::capacity_recovery::{CapacityDeliveryPhase, CapacityDeliveryReceipt};
    use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose};
    use rsi_common::types::{SessionProvider, SessionStatus};
    use tempfile::TempDir;

    #[test]
    fn effort_rank_orders_current_codex_efforts() {
        assert_eq!(effort_rank(None), 0);
        assert_eq!(effort_rank(Some("unknown")), 0);
        assert_eq!(effort_rank(Some("low")), 1);
        assert_eq!(effort_rank(Some("medium")), 2);
        assert_eq!(effort_rank(Some("high")), 3);
        assert_eq!(effort_rank(Some("xhigh")), 4);
        assert_eq!(effort_rank(Some("max")), 5);
        assert_eq!(effort_rank(Some("ultra")), 6);
        assert!(effort_rank(Some("xhigh")) > effort_rank(Some("high")));
        assert!(effort_rank(Some("xhigh")) < effort_rank(Some("max")));
        assert!(effort_rank(Some("max")) < effort_rank(Some("ultra")));
    }

    fn request(
        purpose: ModelInvocationPurpose,
        dedup_key: &str,
        session_id: Uuid,
    ) -> ModelAdmissionRequest {
        ModelAdmissionRequest {
            purpose,
            provider: Some("Codex".to_string()),
            model: Some("gpt-5.4".to_string()),
            backend: Some("Codex".to_string()),
            effort: Some("high".to_string()),
            trigger: "test".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            dedup_key: Some(dedup_key.to_string()),
            request_fingerprint: Some("sha256:test".to_string()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::ExpectedUsage {
                input_tokens: 100,
                output_tokens: 40,
                cache_creation_tokens: 10,
                cache_read_tokens: 5,
                reasoning_tokens: 0,
                embedding_input_count: 0,
                wall_time_ms: 100,
            }),
            baseline_input_tokens: 100,
            baseline_output_tokens: 20,
            baseline_cache_creation_tokens: 10,
            baseline_cache_read_tokens: 5,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 50,
        }
    }

    fn capacity_resume_fixture() -> (Store, Uuid, Uuid, chrono::DateTime<Utc>) {
        capacity_resume_fixture_with_store(
            Store::open_in_memory().expect("capacity admission store"),
        )
    }

    fn capacity_resume_fixture_with_store(
        store: Store,
    ) -> (Store, Uuid, Uuid, chrono::DateTime<Utc>) {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-22T12:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let timestamp = now.to_rfc3339_opts(SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO projects(
                    id,name,path,description,color,context_files,created_at,updated_at
                 ) VALUES(?1,'capacity admission test project',NULL,NULL,'#89b4fa',NULL,?2,?2)",
                params![crate::store::d04_test_project_id().to_string(), timestamp],
            )
            .unwrap();
        let mut session = crate::store::tests::make_test_session();
        session.id = Uuid::new_v4();
        session.provider = SessionProvider::Codex;
        session.model = Some("gpt-5.4".into());
        session.status = SessionStatus::Failed;
        session.stop_reason = Some("provider_error:codex_usage_limit".into());
        let controller = session.id;
        store.insert_session(&session).unwrap();
        let guard_job = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
            crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
                message: "guard".into(),
                in_seconds: None,
                at: None,
                name: None,
                every_seconds: None,
                mode: Some("program_guard".into()),
                working_dir: session.working_dir.clone(),
                provider: Some(SessionProvider::Codex),
                model: session.model.clone(),
                project_id: session.project_id,
                origin_session_id: Some(controller),
                watch_session_id: None,
            },
        )
        .unwrap();
        let guard = guard_job.id;
        store.insert_scheduled_job(&guard_job).unwrap();
        let terminal_invocation = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations(
                    id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    trigger_source,session_id,policy_snapshot_json,usage_confidence,
                    created_at,completed_at
                 ) VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                          'admitted','failed','capacity-admission-fixture',?2,'{}','unavailable',?3,?3)",
                params![terminal_invocation.to_string(), controller.to_string(), timestamp],
            )
            .unwrap();
        let recovery = store
            .settle_capacity_failure(controller, controller, guard, terminal_invocation, 7, now)
            .unwrap();
        (store, controller, recovery.wake_job_id, recovery.due_slot)
    }

    fn capacity_resume_request(
        controller: Uuid,
        wake: Uuid,
        due: chrono::DateTime<Utc>,
    ) -> ModelAdmissionRequest {
        let mut request = request(
            ModelInvocationPurpose::SessionContinueResume,
            &format!(
                "scheduled.resume.capacity:{wake}:{}",
                due.to_rfc3339_opts(SecondsFormat::Nanos, true)
            ),
            controller,
        );
        request.owner = InvocationOwner {
            session_id: Some(controller),
            project_id: Some(crate::store::d04_test_project_id()),
            scheduled_job_id: Some(wake),
            ..Default::default()
        };
        request.trigger = "scheduled_capacity_resume".into();
        request
    }

    fn capacity_admission_mutation_snapshot(store: &Store) -> (i64, i64, i64, i64, i64) {
        let count = |table: &str| {
            store
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        let counter_totals = store
            .conn
            .query_row(
                "SELECT COALESCE(sum(call_count),0),COALESCE(sum(active_count),0)
                 FROM model_budget_counters",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap();
        (
            count("model_invocations"),
            count("master_no_idle_capacity_attempts"),
            count("model_budget_alert_events"),
            counter_totals.0,
            counter_totals.1,
        )
    }

    #[test]
    fn capacity_resume_generic_channel_rejects_capacity_shape_without_mutation() {
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .unwrap();
        let (store, controller, wake, due) = capacity_resume_fixture();
        let request = capacity_resume_request(controller, wake, due);
        let before = capacity_admission_mutation_snapshot(&store);
        let error = store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Standard, &request)
            .expect_err("generic Store admission must reject the capacity channel shape");
        assert!(
            error
                .to_string()
                .contains("generic_model_admission_rejects_capacity_shape")
        );
        assert_eq!(capacity_admission_mutation_snapshot(&store), before);
    }

    #[test]
    fn capacity_resume_typed_channel_rejects_inexact_envelopes_without_mutation() {
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .unwrap();
        for case in [
            "purpose",
            "trigger",
            "backend",
            "disabled_wake",
            "wrong_due",
        ] {
            let (store, controller, wake, due) = capacity_resume_fixture();
            let mut request = capacity_resume_request(controller, wake, due);
            match case {
                "purpose" => request.purpose = ModelInvocationPurpose::SessionLaunchFresh,
                "trigger" => request.trigger = "scheduled_resume".into(),
                "backend" => request.backend = Some("Harness".into()),
                "disabled_wake" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                            [wake.to_string()],
                        )
                        .unwrap();
                }
                "wrong_due" => {
                    let wrong_due = due + TimeDelta::seconds(1);
                    request.dedup_key = Some(format!(
                        "scheduled.resume.capacity:{wake}:{}",
                        wrong_due.to_rfc3339_opts(SecondsFormat::Nanos, true)
                    ));
                }
                _ => unreachable!(),
            }
            let before = capacity_admission_mutation_snapshot(&store);
            let error = store
                .admit_scheduled_capacity_model_invocation(
                    Uuid::new_v4(),
                    registry,
                    ModelTier::Standard,
                    &request,
                )
                .expect_err("inexact capacity envelope must fail closed");
            assert!(
                matches!(error, DaemonError::PolicyDenied(_) | DaemonError::Store(_)),
                "{case} returned unexpected error: {error}"
            );
            assert_eq!(
                capacity_admission_mutation_snapshot(&store),
                before,
                "{case} refusal mutated admission state"
            );
        }
    }

    #[test]
    fn capacity_resume_duplicate_without_receipt_is_closed_integrity_error() {
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .unwrap();
        let (store, controller, wake, due) = capacity_resume_fixture();
        let request = capacity_resume_request(controller, wake, due);
        store
            .conn
            .execute(
                "INSERT INTO model_invocations(
                    id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    provider,model,backend,trigger_source,session_id,project_id,scheduled_job_id,
                    dedup_key,request_fingerprint,policy_snapshot_json,usage_confidence,created_at
                 ) VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                          'admitted','running','Codex','gpt-5.4','Codex',
                          'scheduled_capacity_resume',?2,?3,?4,?5,?6,'{}','unavailable',?7)",
                params![
                    Uuid::new_v4().to_string(),
                    controller.to_string(),
                    crate::store::d04_test_project_id().to_string(),
                    wake.to_string(),
                    request.dedup_key.as_deref().unwrap(),
                    request.request_fingerprint.as_deref().unwrap(),
                    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let before = capacity_admission_mutation_snapshot(&store);
        let error = store
            .admit_scheduled_capacity_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Standard,
                &request,
            )
            .expect_err("dedup without exact receipt must be an integrity error");
        assert!(
            error
                .to_string()
                .contains("capacity_admission_duplicate_without_receipt")
        );
        assert_eq!(capacity_admission_mutation_snapshot(&store), before);
    }

    #[tokio::test]
    async fn capacity_resume_policy_denial_is_persisted_and_emitted_exactly_once() {
        let (store, controller, wake, due) = capacity_resume_fixture();
        store
            .set_model_control_mode(ModelControlMode::StopAll)
            .unwrap();
        let request = capacity_resume_request(controller, wake, due);
        let store = std::sync::Arc::new(tokio::sync::Mutex::new(store));
        let bus = std::sync::Arc::new(crate::bus::EventBus::new(8));
        let mut events = bus.subscribe();
        let error = crate::model_control::admit_capacity_invocation(&store, request, &bus)
            .await
            .expect_err("StopAll must deny the scheduled capacity invocation");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));

        let mut denied_events = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(
                *event,
                crate::bus::DaemonEvent::ModelInvocationDenied { .. }
            ) {
                denied_events += 1;
            }
        }
        bus.unsubscribe();
        assert_eq!(denied_events, 1);
        let guard = store.lock().await;
        assert_eq!(
            guard
                .conn
                .query_row(
                    "SELECT count(*) FROM model_invocations
                     WHERE admission_status='denied' AND trigger_source='scheduled_capacity_resume'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            guard
                .conn
                .query_row(
                    "SELECT count(*) FROM master_no_idle_capacity_attempts
                     WHERE delivery_wake_job_id=?1 AND delivery_due_slot=?2",
                    params![
                        wake.to_string(),
                        due.to_rfc3339_opts(SecondsFormat::Nanos, true)
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn capacity_resume_admission_receipt_is_atomic_and_failed_replay_is_duplicate() {
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("capacity resume registry");
        for seam in [
            "before_invocation_insert",
            "after_invocation_insert",
            "before_receipt_insert",
            "after_receipt_insert",
        ] {
            let (store, controller, wake, due) = capacity_resume_fixture();
            let request = capacity_resume_request(controller, wake, due);
            let invocation = Uuid::new_v4();
            crate::store::capacity_recovery::test_fail_next_admission(seam);
            let error = store
                .admit_scheduled_capacity_model_invocation(
                    invocation,
                    registry,
                    ModelTier::Standard,
                    &request,
                )
                .expect_err("capacity admission fault must roll back");
            assert!(error.to_string().contains("capacity_admission_transient"));
            assert_eq!(
                store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM model_invocations WHERE id=?1",
                        [invocation.to_string()],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                0
            );
            assert_eq!(
                store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM master_no_idle_capacity_attempts
                         WHERE delivery_wake_job_id=?1",
                        [wake.to_string()],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                0
            );
        }

        let (store, controller, wake, due) = capacity_resume_fixture();
        let request = capacity_resume_request(controller, wake, due);
        let invocation = Uuid::new_v4();
        assert_eq!(
            store
                .admit_scheduled_capacity_model_invocation(
                    invocation,
                    registry,
                    ModelTier::Standard,
                    &request,
                )
                .unwrap(),
            CapacityStoreAdmissionOutcome::Admitted(invocation)
        );
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status='failed',completed_at=?1 WHERE id=?2",
                params![
                    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                    invocation.to_string()
                ],
            )
            .unwrap();
        assert_eq!(
            store
                .admit_scheduled_capacity_model_invocation(
                    Uuid::new_v4(),
                    registry,
                    ModelTier::Standard,
                    &request,
                )
                .unwrap(),
            CapacityStoreAdmissionOutcome::Duplicate(CapacityDeliveryReceipt {
                invocation_id: invocation,
                phase: CapacityDeliveryPhase::Admitted,
            }),
            "capacity receipt must win before generic failed-key release"
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM master_no_idle_capacity_attempts
                     WHERE delivery_wake_job_id=?1 AND delivery_due_slot=?2",
                    params![
                        wake.to_string(),
                        due.to_rfc3339_opts(SecondsFormat::Nanos, true)
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn restart_reconcile_preserves_only_dispatchable_capacity_admission_receipts() {
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("capacity resume registry");
        for case in [
            "valid",
            "rotated_target",
            "disabled_wake",
            "missing_wake",
            "due_slot_mismatch",
            "malformed_wake_target",
            "malformed_wake_provider",
            "malformed_wake_recurrence",
            "disabled_guard",
            "malformed_guard",
            "closed_incident",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let database = directory
                .path()
                .join(format!("capacity-reconcile-{case}.sqlite"));
            let (store, controller, wake, due) = capacity_resume_fixture_with_store(
                Store::open(&database).expect("open file-backed reconciliation fixture"),
            );
            let target = if case == "rotated_target" {
                let mut successor = store.get_session(controller).unwrap().unwrap();
                successor.id = Uuid::new_v4();
                successor.continued_from = Some(controller);
                successor.created_at += chrono::Duration::seconds(1);
                successor.updated_at = successor.created_at;
                store.insert_session(&successor).unwrap();
                successor.id
            } else {
                controller
            };
            let request = capacity_resume_request(target, wake, due);
            let invocation = Uuid::new_v4();
            assert_eq!(
                store
                    .admit_scheduled_capacity_model_invocation(
                        invocation,
                        registry,
                        ModelTier::Standard,
                        &request,
                    )
                    .unwrap(),
                CapacityStoreAdmissionOutcome::Admitted(invocation)
            );
            let active_before = capacity_admission_mutation_snapshot(&store).4;
            assert!(active_before > 0, "{case} must reserve active counters");

            match case {
                "valid" | "rotated_target" => {}
                "disabled_wake" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                            [wake.to_string()],
                        )
                        .unwrap();
                }
                "missing_wake" => {
                    store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
                    store
                        .conn
                        .execute("DELETE FROM scheduled_jobs WHERE id=?1", [wake.to_string()])
                        .unwrap();
                    store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
                }
                "due_slot_mismatch" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET next_fire_at=?1 WHERE id=?2",
                            params![
                                (due + chrono::Duration::seconds(1))
                                    .to_rfc3339_opts(SecondsFormat::Nanos, true),
                                wake.to_string()
                            ],
                        )
                        .unwrap();
                }
                "malformed_wake_target" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET wake_session_id=?1 WHERE id=?2",
                            params![Uuid::new_v4().to_string(), wake.to_string()],
                        )
                        .unwrap();
                }
                "malformed_wake_provider" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET provider='Claude' WHERE id=?1",
                            [wake.to_string()],
                        )
                        .unwrap();
                }
                "malformed_wake_recurrence" => {
                    let mut job = store.get_scheduled_job(&wake).unwrap().unwrap();
                    job.schedule.recurrence = rsi_common::types::Recurrence::EverySeconds(60);
                    let schedule = serde_json::to_string(&job.schedule).unwrap();
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET schedule_json=?1 WHERE id=?2",
                            params![schedule, wake.to_string()],
                        )
                        .unwrap();
                }
                "disabled_guard" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET enabled=0
                             WHERE id=(SELECT program_guard_job_id
                                       FROM master_no_idle_capacity_incidents
                                       WHERE wake_job_id=?1)",
                            [wake.to_string()],
                        )
                        .unwrap();
                }
                "malformed_guard" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET wake_mode='fresh'
                             WHERE id=(SELECT program_guard_job_id
                                       FROM master_no_idle_capacity_incidents
                                       WHERE wake_job_id=?1)",
                            [wake.to_string()],
                        )
                        .unwrap();
                }
                "closed_incident" => {
                    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
                    store
                        .conn
                        .execute(
                            "UPDATE master_no_idle_capacity_incidents
                             SET state='closed_success',closed_at=?1,
                                 close_reason='non_capacity_success',updated_at=?1
                             WHERE wake_job_id=?2",
                            params![now, wake.to_string()],
                        )
                        .unwrap();
                }
                _ => unreachable!(),
            }

            drop(store);
            let store = Store::open(&database).expect("reopen capacity reconciliation fixture");

            assert_eq!(
                store
                    .is_dispatchable_unexecuted_capacity_admission(invocation)
                    .unwrap(),
                matches!(case, "valid" | "rotated_target"),
                "{case} replay-envelope classification"
            );
            let reconciled = store
                .reconcile_running_model_invocations()
                .expect("restart reconciliation");
            let status: String = store
                .conn
                .query_row(
                    "SELECT status FROM model_invocations WHERE id=?1",
                    [invocation.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let active_after = capacity_admission_mutation_snapshot(&store).4;
            if matches!(case, "valid" | "rotated_target") {
                assert_eq!(reconciled, 0);
                assert_eq!(status, "running");
                assert_eq!(active_after, active_before);
                assert!(store.get_scheduled_job(&wake).unwrap().unwrap().enabled);
            } else {
                assert_eq!(reconciled, 1, "{case} must not strand an admission");
                assert_eq!(status, "failed");
                assert_eq!(active_after, 0, "{case} must release active counters");
                assert_eq!(
                    store
                        .conn
                        .query_row(
                            "SELECT error_class FROM model_invocations WHERE id=?1",
                            [invocation.to_string()],
                            |row| row.get::<_, String>(0),
                        )
                        .unwrap(),
                    "capacity_delivery_abandoned_before_launch",
                    "{case} must retain an honest no-provider-effect disposition"
                );
                let wake_job = store.get_scheduled_job(&wake).unwrap();
                if case == "missing_wake" {
                    assert!(wake_job.is_none());
                } else {
                    assert!(
                        !wake_job.unwrap().enabled,
                        "{case} must retire its invalid recovery wake"
                    );
                }
                assert_eq!(
                    store.reconcile_running_model_invocations().unwrap(),
                    0,
                    "{case} reconciliation must be idempotent"
                );
                assert_eq!(
                    store
                        .conn
                        .query_row(
                            "SELECT state FROM master_no_idle_capacity_attempts
                             WHERE model_invocation_id=?1",
                            [invocation.to_string()],
                            |row| row.get::<_, String>(0),
                        )
                        .unwrap(),
                    "delivery_admitted",
                    "the unexecuted admission remains immutable audit evidence"
                );
            }
        }
    }

    #[test]
    fn restart_reconcile_invalid_capacity_admission_is_atomic_with_wake_disable() {
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("capacity resume registry");
        let (store, controller, wake, due) = capacity_resume_fixture();
        let request = capacity_resume_request(controller, wake, due);
        let invocation = Uuid::new_v4();
        assert_eq!(
            store
                .admit_scheduled_capacity_model_invocation(
                    invocation,
                    registry,
                    ModelTier::Standard,
                    &request,
                )
                .unwrap(),
            CapacityStoreAdmissionOutcome::Admitted(invocation)
        );
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0
                 WHERE id=(SELECT program_guard_job_id
                           FROM master_no_idle_capacity_incidents
                           WHERE wake_job_id=?1)",
                [wake.to_string()],
            )
            .unwrap();
        let active_before = capacity_admission_mutation_snapshot(&store).4;
        store
            .conn
            .execute_batch(&format!(
                "CREATE TRIGGER fail_capacity_wake_disable
                 BEFORE UPDATE OF enabled ON scheduled_jobs
                 WHEN OLD.id='{}'
                 BEGIN SELECT RAISE(ABORT,'injected capacity wake disable failure'); END;",
                wake
            ))
            .unwrap();

        let error = store
            .reconcile_running_model_invocations()
            .expect_err("wake failure must roll back invocation settlement");
        assert!(
            error
                .to_string()
                .contains("injected capacity wake disable failure")
        );
        assert_eq!(
            store
                .load_model_invocation_record(invocation)
                .unwrap()
                .unwrap()
                .status,
            ModelInvocationStatus::Running
        );
        assert_eq!(
            capacity_admission_mutation_snapshot(&store).4,
            active_before
        );
        assert!(store.get_scheduled_job(&wake).unwrap().unwrap().enabled);

        store
            .conn
            .execute_batch("DROP TRIGGER fail_capacity_wake_disable")
            .unwrap();
        assert_eq!(store.reconcile_running_model_invocations().unwrap(), 1);
        assert_eq!(
            store
                .load_model_invocation_record(invocation)
                .unwrap()
                .unwrap()
                .status,
            ModelInvocationStatus::Failed
        );
        assert_eq!(capacity_admission_mutation_snapshot(&store).4, 0);
        assert!(!store.get_scheduled_job(&wake).unwrap().unwrap().enabled);
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT error_class FROM model_invocations WHERE id=?1",
                    [invocation.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "capacity_delivery_abandoned_before_launch"
        );
    }

    #[test]
    fn capacity_resume_reopen_replays_one_receipt_and_disables_only_persisted_due_slot() {
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("capacity resume registry");
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capacity-admission-reopen.sqlite");
        let (store, controller, wake, due) = capacity_resume_fixture_with_store(
            Store::open(&database).expect("file-backed capacity admission store"),
        );
        let request = capacity_resume_request(controller, wake, due);
        let invocation = Uuid::from_u128(0x8200_0000_0000_4000_8000_0000_0000_0001);
        let replay_invocation = Uuid::from_u128(0x8200_0000_0000_4000_8000_0000_0000_0002);
        let persisted_job = store
            .get_scheduled_job(&wake)
            .unwrap()
            .expect("persisted capacity wake");
        assert_eq!(persisted_job.wake_mode, rsi_common::types::WakeMode::Resume);
        assert!(matches!(
            persisted_job.schedule.recurrence,
            rsi_common::types::Recurrence::Once
        ));
        let pre_crash_plan = match store
            .validate_capacity_delivery(&persisted_job, due)
            .unwrap()
        {
            crate::store::capacity_recovery::CapacityDeliveryValidation::Ready(plan) => plan,
            other => panic!("capacity envelope must be Ready, got {other:?}"),
        };
        assert_eq!(pre_crash_plan.controller_session_id, controller);
        assert_eq!(pre_crash_plan.wake_job_id, wake);
        assert_eq!(pre_crash_plan.due_slot, due);
        assert_eq!(
            store
                .admit_scheduled_capacity_model_invocation(
                    invocation,
                    registry,
                    ModelTier::Standard,
                    &request,
                )
                .unwrap(),
            CapacityStoreAdmissionOutcome::Admitted(invocation)
        );
        drop(store);

        let reopened = Store::open(&database).expect("reopen capacity admission store");
        let replay_job = reopened
            .get_scheduled_job(&wake)
            .unwrap()
            .expect("still-enabled due slot after crash window");
        let plan = match reopened
            .validate_capacity_delivery(&replay_job, due)
            .unwrap()
        {
            crate::store::capacity_recovery::CapacityDeliveryValidation::Ready(plan) => plan,
            other => panic!("persisted capacity envelope must remain Ready, got {other:?}"),
        };
        assert_eq!(plan.controller_session_id, controller);
        assert_eq!(plan.wake_job_id, wake);
        assert_eq!(plan.due_slot, due);
        assert_eq!(
            reopened
                .admit_scheduled_capacity_model_invocation(
                    replay_invocation,
                    registry,
                    ModelTier::Standard,
                    &request,
                )
                .unwrap(),
            CapacityStoreAdmissionOutcome::Duplicate(CapacityDeliveryReceipt {
                invocation_id: invocation,
                phase: CapacityDeliveryPhase::Admitted,
            }),
            "typed capacity replay cannot enter failed-key release or start a second provider"
        );
        assert_eq!(
            reopened
                .conn
                .query_row(
                    "SELECT count(*) FROM model_invocations WHERE dedup_key=?1",
                    [request.dedup_key.as_deref().unwrap()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            reopened
                .conn
                .query_row(
                    "SELECT count(*) FROM master_no_idle_capacity_attempts
                     WHERE delivery_wake_job_id=?1 AND delivery_due_slot=?2",
                    params![
                        wake.to_string(),
                        due.to_rfc3339_opts(SecondsFormat::Nanos, true)
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            reopened
                .conn
                .query_row(
                    "SELECT count(*) FROM model_invocations WHERE id=?1",
                    [replay_invocation.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert!(
            !reopened
                .disable_capacity_due_slot(wake, due + TimeDelta::seconds(1), due)
                .unwrap()
        );
        assert!(reopened.get_scheduled_job(&wake).unwrap().unwrap().enabled);
        assert!(
            reopened
                .disable_capacity_due_slot(wake, due, due + TimeDelta::seconds(1))
                .unwrap()
        );
        let disabled = reopened.get_scheduled_job(&wake).unwrap().unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.next_fire_at, due);
        assert!(
            !reopened
                .disable_capacity_due_slot(wake, due, due + TimeDelta::seconds(2))
                .unwrap()
        );
    }

    #[test]
    fn capacity_resume_rotation_lineage_corruption_and_conflicts_fail_closed() {
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .unwrap();

        let (store, controller, wake, due) = capacity_resume_fixture();
        let mut descendant = crate::store::tests::make_test_session();
        descendant.id = Uuid::new_v4();
        descendant.provider = SessionProvider::Codex;
        descendant.model = Some("gpt-5.4".into());
        descendant.continued_from = Some(controller);
        let descendant_id = descendant.id;
        store.insert_session(&descendant).unwrap();
        let mut request = capacity_resume_request(controller, wake, due);
        request.owner.session_id = Some(descendant_id);
        let admitted = Uuid::new_v4();
        assert_eq!(
            store
                .admit_scheduled_capacity_model_invocation(
                    admitted,
                    registry,
                    ModelTier::Standard,
                    &request,
                )
                .unwrap(),
            CapacityStoreAdmissionOutcome::Admitted(admitted),
            "authenticated rotation descendant is the admitted Resume target"
        );
        let mut conflict = request.clone();
        conflict.request_fingerprint = Some("sha256:capacity-conflict".into());
        let error = store
            .admit_scheduled_capacity_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Standard,
                &conflict,
            )
            .expect_err("same due-slot with changed request must conflict");
        assert!(
            error
                .to_string()
                .contains("capacity delivery dedup conflict")
        );

        let (store, controller, wake, due) = capacity_resume_fixture();
        let mut missing = capacity_resume_request(controller, wake, due);
        missing.owner.session_id = Some(Uuid::new_v4());
        let error = store
            .admit_scheduled_capacity_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Standard,
                &missing,
            )
            .expect_err("missing lineage target fails closed");
        assert!(error.to_string().contains("lineage_missing_session"));

        let (store, controller, wake, due) = capacity_resume_fixture();
        let mut cyclic = crate::store::tests::make_test_session();
        cyclic.id = Uuid::new_v4();
        cyclic.provider = SessionProvider::Codex;
        cyclic.model = Some("gpt-5.4".into());
        cyclic.continued_from = Some(cyclic.id);
        let cyclic_id = cyclic.id;
        store.insert_session(&cyclic).unwrap();
        let mut cycle_request = capacity_resume_request(controller, wake, due);
        cycle_request.owner.session_id = Some(cyclic_id);
        let error = store
            .admit_scheduled_capacity_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Standard,
                &cycle_request,
            )
            .expect_err("cyclic lineage fails closed");
        assert!(error.to_string().contains("lineage_cycle"));

        let (store, controller, wake, due) = capacity_resume_fixture();
        let mut changed = capacity_resume_request(controller, wake, due);
        changed.model = Some("changed-model".into());
        let error = store
            .admit_scheduled_capacity_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Standard,
                &changed,
            )
            .expect_err("changed frozen envelope fails closed");
        assert!(error.to_string().contains("frozen_envelope_mismatch"));
    }

    fn session_policy(
        session_id: Uuid,
        purpose: ModelInvocationPurpose,
        max_calls: Option<u64>,
        max_concurrency: Option<u32>,
        alert_threshold_ratio: Option<f64>,
    ) -> ModelBudgetPolicy {
        ModelBudgetPolicy {
            scope_kind: BudgetScopeKind::Session,
            scope_id: Some(session_id.to_string()),
            purpose: Some(purpose),
            model_tier: Some(ModelTier::Premium),
            effort: Some("high".to_string()),
            ceiling_model_tier: None,
            ceiling_effort: None,
            max_calls,
            max_total_tokens: None,
            max_input_tokens: None,
            max_output_tokens: None,
            max_cache_creation_tokens: None,
            max_cache_read_tokens: None,
            max_reasoning_tokens: None,
            max_embedding_inputs: None,
            max_wall_time_ms: None,
            max_concurrency,
            max_retries: None,
            max_calls_per_window: None,
            rate_window_seconds: None,
            alert_threshold_ratio,
        }
    }

    #[test]
    fn normal_admits_only_default_enabled_paid_background_purpose() {
        let store = Store::open_in_memory().expect("store");
        let agent_registry = registry::lookup(ModelInvocationPurpose::AgentScheduleWakeFresh)
            .copied()
            .expect("agent fresh registry");
        let scheduled_registry = registry::lookup(ModelInvocationPurpose::ScheduledFresh)
            .copied()
            .expect("scheduled fresh registry");

        let agent_id = Uuid::new_v4();
        let agent_outcome = store
            .admit_model_invocation(
                agent_id,
                agent_registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::AgentScheduleWakeFresh,
                    "agent-fresh-normal",
                    Uuid::new_v4(),
                ),
            )
            .expect("agent fresh admission");
        assert!(matches!(agent_outcome, StoreAdmissionOutcome::Admitted(_)));
        let agent_record = store
            .load_model_invocation_record(agent_id)
            .expect("load")
            .expect("record");
        assert_eq!(
            agent_record
                .policy_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.get("default_background_paid_allowed"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );

        let scheduled_id = Uuid::new_v4();
        let scheduled_outcome = store
            .admit_model_invocation(
                scheduled_id,
                scheduled_registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::ScheduledFresh,
                    "scheduled-fresh-normal",
                    Uuid::new_v4(),
                ),
            )
            .expect("scheduled fresh admission result");
        assert!(matches!(
            scheduled_outcome,
            StoreAdmissionOutcome::Denied { .. }
        ));
        let scheduled_record = store
            .load_model_invocation_record(scheduled_id)
            .expect("load")
            .expect("record");
        assert_eq!(
            scheduled_record
                .policy_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.get("default_background_paid_allowed"))
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn agent_schedule_wake_fresh_is_denied_by_stronger_modes() {
        let registry = registry::lookup(ModelInvocationPurpose::AgentScheduleWakeFresh)
            .copied()
            .expect("agent fresh registry");
        for mode in [
            ModelControlMode::PauseBackground,
            ModelControlMode::DenyPaid,
            ModelControlMode::LocalOnly,
            ModelControlMode::StopAll,
        ] {
            let store = Store::open_in_memory().expect("store");
            store.set_model_control_mode(mode).expect("set mode");
            let outcome = store
                .admit_model_invocation(
                    Uuid::new_v4(),
                    registry,
                    ModelTier::Premium,
                    &request(
                        ModelInvocationPurpose::AgentScheduleWakeFresh,
                        &format!("agent-fresh-{mode:?}"),
                        Uuid::new_v4(),
                    ),
                )
                .expect("admission result");
            assert!(
                matches!(outcome, StoreAdmissionOutcome::Denied { .. }),
                "{mode:?} must deny remote agent fresh work"
            );
        }
    }

    #[test]
    fn duplicate_admission_reuses_existing_row_without_double_reserving_counters() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");

        let first = store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "dup-key",
                    session_id,
                ),
            )
            .expect("first admission");
        let first_id = match first {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected first admission outcome: {other:?}"),
        };

        let second = store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "dup-key",
                    session_id,
                ),
            )
            .expect("second admission");
        assert_eq!(second, StoreAdmissionOutcome::Duplicate(first_id));

        let row_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |row| {
                row.get(0)
            })
            .expect("row count");
        assert_eq!(row_count, 1);

        let active_count: i64 = store
            .conn
            .query_row(
                "SELECT active_count FROM model_budget_counters
                 WHERE scope_kind = 'global'
                   AND scope_id = 'global'
                   AND purpose = '__all__'
                   AND model_tier = '__all__'
                   AND effort = '__all__'",
                [],
                |row| row.get(0),
            )
            .expect("global aggregate active count");
        assert_eq!(active_count, 1);
    }

    #[test]
    fn completion_is_idempotent_and_uses_baseline_deltas() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("registry");
        let invocation_id = Uuid::new_v4();

        let outcome = store
            .admit_model_invocation(
                invocation_id,
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionContinueResume,
                    "resume-key",
                    session_id,
                ),
            )
            .expect("admission");
        assert_eq!(outcome, StoreAdmissionOutcome::Admitted(invocation_id));

        let completion = InvocationCompletion {
            input_tokens: Some(160),
            output_tokens: Some(55),
            cache_creation_tokens: Some(18),
            cache_read_tokens: Some(7),
            wall_time_ms: Some(125),
            confidence: Some(ModelUsageConfidence::Measured),
            ..InvocationCompletion::default()
        };
        store
            .complete_model_invocation(invocation_id, &completion)
            .expect("first completion");
        store
            .complete_model_invocation(invocation_id, &completion)
            .expect("second completion");

        let settled = store
            .conn
            .query_row(
                "SELECT status, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, wall_time_ms
                 FROM model_invocations WHERE id = ?1",
                params![invocation_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                },
            )
            .expect("settled invocation");
        assert_eq!(settled.0, "completed");
        assert_eq!(settled.1, Some(60));
        assert_eq!(settled.2, Some(35));
        assert_eq!(settled.3, Some(8));
        assert_eq!(settled.4, Some(2));
        assert_eq!(settled.5, Some(75));

        let counters = store
            .conn
            .query_row(
                "SELECT active_count, input_tokens, output_tokens, wall_time_ms
                 FROM model_budget_counters
                 WHERE scope_kind = 'session'
                   AND scope_id = ?1
                   AND purpose = '__all__'
                   AND model_tier = '__all__'
                   AND effort = '__all__'",
                params![session_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .expect("session counters");
        assert_eq!(counters.0, 0);
        assert_eq!(counters.1, 60);
        assert_eq!(counters.2, 35);
        assert_eq!(counters.3, 75);
    }

    #[test]
    fn retry_and_child_invocations_share_tree_scope_root() {
        let store = Store::open_in_memory().expect("store");
        let root_session_id = Uuid::new_v4();
        let retry_session_id = Uuid::new_v4();
        let child_session_id = Uuid::new_v4();

        let root_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("root registry");
        let retry_registry = registry::lookup(ModelInvocationPurpose::SessionRetryAuto)
            .copied()
            .expect("retry registry");
        let child_registry = registry::lookup(ModelInvocationPurpose::AgentSpawnChild)
            .copied()
            .expect("child registry");

        let root_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                root_registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "tree-root",
                    root_session_id,
                ),
            )
            .expect("root admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected root outcome: {other:?}"),
        };

        let mut retry_request = request(
            ModelInvocationPurpose::SessionRetryAuto,
            "tree-retry",
            retry_session_id,
        );
        retry_request.provider = Some("Local".to_string());
        retry_request.model = Some("qwen3".to_string());
        retry_request.backend = Some("ollama".to_string());
        retry_request.retry_of_invocation_id = Some(root_id);
        store
            .conn
            .execute(
                "INSERT INTO model_budget_policies (
                    policy_key, scope_kind, scope_id, max_retries, updated_at
                 ) VALUES (?1, 'retry', ?2, 1, datetime('now'))",
                params![format!("retry-budget:{root_id}"), root_id.to_string()],
            )
            .expect("retry policy");
        let retry_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                retry_registry,
                ModelTier::Local,
                &retry_request,
            )
            .expect("retry admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected retry outcome: {other:?}"),
        };

        let mut child_request = request(
            ModelInvocationPurpose::AgentSpawnChild,
            "tree-child",
            child_session_id,
        );
        child_request.parent_invocation_id = Some(root_id);
        let child_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                child_registry,
                ModelTier::Premium,
                &child_request,
            )
            .expect("child admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected child outcome: {other:?}"),
        };

        let tree_counter = store
            .conn
            .query_row(
                "SELECT call_count, active_count
                 FROM model_budget_counters
                 WHERE scope_kind = 'tree'
                   AND scope_id = ?1
                   AND purpose = '__all__'
                   AND model_tier = '__all__'
                   AND effort = '__all__'",
                params![root_id.to_string()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .expect("tree counter");
        assert_eq!(tree_counter, (3, 3));

        let completion = InvocationCompletion::default();
        store
            .complete_model_invocation(root_id, &completion)
            .expect("root completion");
        store
            .complete_model_invocation(retry_id, &completion)
            .expect("retry completion");
        store
            .complete_model_invocation(child_id, &completion)
            .expect("child completion");

        let active_count: i64 = store
            .conn
            .query_row(
                "SELECT active_count
                 FROM model_budget_counters
                 WHERE scope_kind = 'tree'
                   AND scope_id = ?1
                   AND purpose = '__all__'
                   AND model_tier = '__all__'
                   AND effort = '__all__'",
                params![root_id.to_string()],
                |row| row.get(0),
            )
            .expect("tree active count");
        assert_eq!(active_count, 0);

        let retry_budget: i64 = store
            .conn
            .query_row(
                "SELECT max_retries FROM model_budget_policies
                 WHERE scope_kind = 'retry' AND scope_id = ?1",
                params![root_id.to_string()],
                |row| row.get(0),
            )
            .expect("retry policy row");
        assert_eq!(retry_budget, 1);
    }

    #[test]
    fn long_tree_and_retry_lineages_have_no_lifetime_cap() {
        const LINK_COUNT: usize = 96;

        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let root_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("root registry");
        let continue_registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("continue registry");

        let root_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                root_registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "long-lineage-root",
                    session_id,
                ),
            )
            .expect("root admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected root outcome: {other:?}"),
        };

        let mut lineage = vec![root_id];
        for index in 0..LINK_COUNT {
            let mut continue_request = request(
                ModelInvocationPurpose::SessionContinueResume,
                &format!("long-lineage-{index}"),
                session_id,
            );
            continue_request.parent_invocation_id = lineage.last().copied();
            let invocation_id = match store
                .admit_model_invocation(
                    Uuid::new_v4(),
                    continue_registry,
                    ModelTier::Premium,
                    &continue_request,
                )
                .expect("long lineage admission")
            {
                StoreAdmissionOutcome::Admitted(id) => id,
                other => panic!("unexpected continuation outcome: {other:?}"),
            };
            lineage.push(invocation_id);
        }

        let tree_call_count: i64 = store
            .conn
            .query_row(
                "SELECT call_count
                 FROM model_budget_counters
                 WHERE scope_kind = 'tree'
                   AND scope_id = ?1
                   AND purpose = '__all__'
                   AND model_tier = '__all__'
                   AND effort = '__all__'",
                params![root_id.to_string()],
                |row| row.get(0),
            )
            .expect("tree counter");
        assert_eq!(tree_call_count, (LINK_COUNT + 1) as i64);

        for pair in lineage.windows(2) {
            store
                .conn
                .execute(
                    "UPDATE model_invocations
                     SET parent_invocation_id = NULL, retry_of_invocation_id = ?1
                     WHERE id = ?2",
                    params![pair[0].to_string(), pair[1].to_string()],
                )
                .expect("convert parent link to retry link");
        }
        let tx = store.conn.unchecked_transaction().expect("transaction");
        let retry_root = resolve_retry_scope_id_tx(&tx, *lineage.last().expect("lineage tail"))
            .expect("long retry lineage root");
        assert_eq!(retry_root, root_id);
    }

    #[test]
    fn preview_orchestration_escalation_matches_admit_model_invocation() {
        let store = Store::open_in_memory().expect("store");
        let root_session_id = Uuid::new_v4();
        let child_deny_session_id = Uuid::new_v4();
        let child_allow_session_id = Uuid::new_v4();

        let child_registry = registry::lookup(ModelInvocationPurpose::AgentSpawnChild)
            .copied()
            .expect("child registry");

        // Admit a real root at premium/high through the same admission path
        // AgentSpawnChild always uses. AgentSpawnChild is Foreground +
        // PaidCapable (registry.rs:240-243) and the default mode is Normal, so
        // enforce_mode cannot deny it; a fresh in-memory store has no circuits
        // and no budget policies, and the root's own escalation self-check
        // passes because the check (:774) runs before the row INSERT (:843).
        let root_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                child_registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::AgentSpawnChild,
                    "root",
                    root_session_id,
                ),
            )
            .expect("root admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected root outcome: {other:?}"),
        };

        // 1. Escalating child (effort xhigh > root's high) — preview must deny.
        let denial = store
            .preview_orchestration_escalation(
                ModelInvocationPurpose::AgentSpawnChild,
                ModelTier::Premium,
                Some("xhigh"),
                Some(root_id),
            )
            .expect("preview should succeed")
            .expect("preview should deny an escalating effort request");
        assert_eq!(denial.root_effort.as_deref(), Some("high"));
        assert!(
            denial.detail.contains("effort escalation denied"),
            "detail: {}",
            denial.detail
        );

        // 2. The real admission gate must agree, verdict AND message, using a
        // distinct dedup_key so the dedup short-circuit (model_control.rs:659)
        // cannot make this pass for the wrong reason.
        let mut child_deny_request = request(
            ModelInvocationPurpose::AgentSpawnChild,
            "child-deny",
            child_deny_session_id,
        );
        child_deny_request.effort = Some("xhigh".to_string());
        child_deny_request.parent_invocation_id = Some(root_id);
        match store
            .admit_model_invocation(
                Uuid::new_v4(),
                child_registry,
                ModelTier::Premium,
                &child_deny_request,
            )
            .expect("child-deny admission")
        {
            StoreAdmissionOutcome::Denied { reason, .. } => {
                // admit_model_invocation stores error.to_string(), and
                // DaemonError::PolicyDenied's Display prefixes "Policy denied: "
                // (error.rs:58) — so reason == "Policy denied: " + denial.detail.
                // Do NOT assert equality; assert the shared suffix instead.
                assert!(
                    reason.ends_with(&denial.detail),
                    "reason {reason:?} does not end with preview detail {:?}",
                    denial.detail
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }

        // 3. The allow case agrees too: effort within the root's cap.
        let allow_preview = store
            .preview_orchestration_escalation(
                ModelInvocationPurpose::AgentSpawnChild,
                ModelTier::Premium,
                Some("high"),
                Some(root_id),
            )
            .expect("preview should succeed");
        assert_eq!(allow_preview, None);

        let mut child_allow_request = request(
            ModelInvocationPurpose::AgentSpawnChild,
            "child-allow",
            child_allow_session_id,
        );
        child_allow_request.effort = Some("high".to_string());
        child_allow_request.parent_invocation_id = Some(root_id);
        match store
            .admit_model_invocation(
                Uuid::new_v4(),
                child_registry,
                ModelTier::Premium,
                &child_allow_request,
            )
            .expect("child-allow admission")
        {
            StoreAdmissionOutcome::Admitted(_) => {}
            other => panic!("expected Admitted, got {other:?}"),
        }
    }

    #[test]
    fn durable_denied_and_corrupt_rows_load_visibly() {
        let store = Store::open_in_memory().expect("store");
        let invocation_id = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations (
                    id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                    provider, model, backend, model_tier, effort, trigger_source,
                    policy_snapshot_json, error_class, created_at, completed_at, usage_confidence
                ) VALUES (
                    ?1, 'dream.consolidation', 'background', 'background', 'paid_capable',
                    'denied', 'denied',
                    'Claude', 'claude-sonnet-5', 'Claude', 'premium', 'medium', 'test',
                    '{bad json', 'policy_denied', ?2, ?2, 'unavailable'
                )",
                params![
                    invocation_id.to_string(),
                    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                ],
            )
            .expect("seed denied row");

        let record = store
            .load_model_invocation_record(invocation_id)
            .expect("load")
            .expect("record");
        assert_eq!(record.admission_status, AdmissionStatus::Denied);
        assert_eq!(record.status, ModelInvocationStatus::Denied);
        assert_eq!(record.policy_snapshot_status, "corrupt");
        assert!(record.policy_snapshot.is_none());
        assert!(record.policy_snapshot_error.is_some());
    }

    #[test]
    fn cancel_terminal_invocation_is_explicit_no_op() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        let invocation_id = Uuid::new_v4();

        let outcome = store
            .admit_model_invocation(
                invocation_id,
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "cancel-terminal",
                    session_id,
                ),
            )
            .expect("admission");
        assert_eq!(outcome, StoreAdmissionOutcome::Admitted(invocation_id));
        store
            .complete_model_invocation(invocation_id, &InvocationCompletion::default())
            .expect("complete");

        let outcome = store
            .request_model_invocation_cancellation(
                invocation_id,
                "operator_cancelled",
                "operator_request",
            )
            .expect("cancel");
        let StoreCancellationOutcome::NoChange(record) = outcome else {
            panic!("expected no-op cancellation for terminal record");
        };
        assert_eq!(record.status, ModelInvocationStatus::Completed);
    }

    #[test]
    fn provider_circuit_denies_new_admissions() {
        let store = Store::open_in_memory().expect("store");
        store
            .update_model_circuits(&[ModelCircuitStatus {
                scope_kind: BudgetScopeKind::Provider,
                scope_id: Some("codex".to_string()),
                state: "open".to_string(),
                reason: "quota storm".to_string(),
                error_class: Some("quota".to_string()),
                source: "operator".to_string(),
                opened_at: Some(Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)),
                updated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                reset_at: None,
                cooldown_secs: Some(60),
                probe_after: None,
                trip_count: 0,
                transient_failure_count: 0,
                transient_window_started_at: None,
                probe_invocation_id: None,
                probe_lease_started_at: None,
            }])
            .expect("update circuits");

        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        let outcome = store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "provider-circuit",
                    Uuid::new_v4(),
                ),
            )
            .expect("circuit outcome");
        assert!(matches!(outcome, StoreAdmissionOutcome::Denied { .. }));
    }

    #[test]
    fn cancellation_request_keeps_counters_reserved_until_terminal_winner_releases_once() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        store
            .apply_model_control_policy_transition(
                ModelControlMode::Normal,
                true,
                &[session_policy(
                    session_id,
                    ModelInvocationPurpose::SessionLaunchFresh,
                    None,
                    Some(1),
                    None,
                )],
                &[],
            )
            .expect("policy transition");

        let invocation_id = Uuid::new_v4();
        let admission = store
            .admit_model_invocation(
                invocation_id,
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "cancel-pending",
                    session_id,
                ),
            )
            .expect("first admission");
        assert_eq!(admission, StoreAdmissionOutcome::Admitted(invocation_id));

        let request_outcome = store
            .request_model_invocation_cancellation(
                invocation_id,
                "operator_cancelled",
                "operator_request",
            )
            .expect("request cancellation");
        let StoreCancellationOutcome::Requested(record) = request_outcome else {
            panic!("expected durable cancellation request");
        };
        assert_eq!(record.status, ModelInvocationStatus::CancellationRequested);

        let active_count: i64 = store
            .conn
            .query_row(
                "SELECT active_count
                 FROM model_budget_counters
                 WHERE scope_kind = 'session'
                   AND scope_id = ?1
                   AND purpose = '__all__'
                   AND model_tier = '__all__'
                   AND effort = '__all__'",
                params![session_id.to_string()],
                |row| row.get(0),
            )
            .expect("session active count");
        assert_eq!(active_count, 1);

        let denied_while_pending = store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "cancel-pending-second",
                    session_id,
                ),
            )
            .expect("second admission result");
        assert!(matches!(
            denied_while_pending,
            StoreAdmissionOutcome::Denied { .. }
        ));

        let settled = store
            .complete_model_invocation(invocation_id, &InvocationCompletion::default())
            .expect("settle winner");
        let StoreCompletionOutcome::Transitioned { record, .. } = settled else {
            panic!("expected terminal completion transition");
        };
        assert_eq!(record.status, ModelInvocationStatus::Completed);

        let loser = store
            .complete_model_invocation(
                invocation_id,
                &InvocationCompletion {
                    error_class: Some("operator_cancelled".to_string()),
                    ..InvocationCompletion::default()
                },
            )
            .expect("idempotent loser");
        assert!(matches!(loser, StoreCompletionOutcome::NoChange));

        let record = store
            .load_model_invocation_record(invocation_id)
            .expect("load")
            .expect("record");
        assert_eq!(record.status, ModelInvocationStatus::Completed);

        let active_count: i64 = store
            .conn
            .query_row(
                "SELECT active_count
                 FROM model_budget_counters
                 WHERE scope_kind = 'session'
                   AND scope_id = ?1
                   AND purpose = '__all__'
                   AND model_tier = '__all__'
                   AND effort = '__all__'",
                params![session_id.to_string()],
                |row| row.get(0),
            )
            .expect("session active count after settle");
        assert_eq!(active_count, 0);

        let admitted_after_release = store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "cancel-pending-third",
                    session_id,
                ),
            )
            .expect("admission after release");
        assert!(matches!(
            admitted_after_release,
            StoreAdmissionOutcome::Admitted(_)
        ));
    }

    #[test]
    fn provider_terminal_failures_open_circuit_and_block_followup_admissions() {
        for error_class in ["auth", "authorization", "quota", "invalid_provider_config"] {
            let store = Store::open_in_memory().expect("store");
            let session_id = Uuid::new_v4();
            let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
                .copied()
                .expect("registry");
            let invocation_id = Uuid::new_v4();
            let admission = store
                .admit_model_invocation(
                    invocation_id,
                    registry,
                    ModelTier::Premium,
                    &request(
                        ModelInvocationPurpose::SessionLaunchFresh,
                        &format!("trip-{error_class}"),
                        session_id,
                    ),
                )
                .expect("admission");
            assert_eq!(admission, StoreAdmissionOutcome::Admitted(invocation_id));

            let completion = store
                .complete_model_invocation(
                    invocation_id,
                    &InvocationCompletion {
                        error_class: Some(error_class.to_string()),
                        ..InvocationCompletion::default()
                    },
                )
                .expect("failed completion");
            let StoreCompletionOutcome::Transitioned {
                circuit_transition: Some(circuit),
                ..
            } = completion
            else {
                panic!("expected circuit transition for {error_class}");
            };
            assert_eq!(circuit.scope_kind, BudgetScopeKind::Provider);
            assert_eq!(circuit.scope_id.as_deref(), Some("codex"));
            assert_eq!(circuit.state, "open");
            assert_eq!(circuit.error_class.as_deref(), Some(error_class));

            let denied = store
                .admit_model_invocation(
                    Uuid::new_v4(),
                    registry,
                    ModelTier::Premium,
                    &request(
                        ModelInvocationPurpose::SessionLaunchFresh,
                        &format!("trip-followup-{error_class}"),
                        session_id,
                    ),
                )
                .expect("followup admission");
            assert!(matches!(denied, StoreAdmissionOutcome::Denied { .. }));
        }
    }

    #[test]
    fn half_open_probe_lease_persists_across_restart_and_success_closes_circuit() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("rsi.db");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        let past = (Utc::now() - chrono::Duration::minutes(10))
            .to_rfc3339_opts(SecondsFormat::Nanos, true);
        let invocation_id = {
            let store = Store::open(&db_path).expect("open store");
            store
                .update_model_circuits(&[ModelCircuitStatus {
                    scope_kind: BudgetScopeKind::Provider,
                    scope_id: Some("codex".to_string()),
                    state: "open".to_string(),
                    reason: "quota".to_string(),
                    error_class: Some("quota".to_string()),
                    source: "automatic_failure".to_string(),
                    opened_at: Some(past.clone()),
                    updated_at: past.clone(),
                    reset_at: None,
                    cooldown_secs: Some(1),
                    probe_after: Some(past.clone()),
                    trip_count: 1,
                    transient_failure_count: 0,
                    transient_window_started_at: None,
                    probe_invocation_id: None,
                    probe_lease_started_at: None,
                }])
                .expect("seed open circuit");

            let invocation_id = Uuid::new_v4();
            let outcome = store
                .admit_model_invocation(
                    invocation_id,
                    registry,
                    ModelTier::Premium,
                    &request(
                        ModelInvocationPurpose::SessionLaunchFresh,
                        "probe-lease",
                        session_id,
                    ),
                )
                .expect("probe admission");
            assert_eq!(outcome, StoreAdmissionOutcome::Admitted(invocation_id));

            let circuits = store.list_model_circuits().expect("list circuits");
            assert_eq!(circuits.len(), 1);
            assert_eq!(circuits[0].state, "half_open");
            assert_eq!(circuits[0].probe_invocation_id, Some(invocation_id));
            assert!(circuits[0].probe_lease_started_at.is_some());
            invocation_id
        };

        let reopened = Store::open(&db_path).expect("reopen store");
        let circuits = reopened.list_model_circuits().expect("persisted circuits");
        assert_eq!(circuits.len(), 1);
        assert_eq!(circuits[0].state, "half_open");
        assert_eq!(circuits[0].probe_invocation_id, Some(invocation_id));

        let denied = reopened
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "probe-competitor",
                    session_id,
                ),
            )
            .expect("competitor admission");
        assert!(matches!(denied, StoreAdmissionOutcome::Denied { .. }));

        let completion = reopened
            .complete_model_invocation(invocation_id, &InvocationCompletion::default())
            .expect("probe success");
        let StoreCompletionOutcome::Transitioned {
            circuit_transition: Some(circuit),
            ..
        } = completion
        else {
            panic!("expected probe closure transition");
        };
        assert_eq!(circuit.state, "closed");
        assert_eq!(circuit.reason, "probe_succeeded");
        assert_eq!(circuit.source, "automatic_probe");
        assert!(circuit.reset_at.is_some());

        let circuits = reopened.list_model_circuits().expect("closed circuits");
        assert_eq!(circuits[0].state, "closed");
        assert!(circuits[0].probe_invocation_id.is_none());

        let admitted = reopened
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "probe-after-close",
                    session_id,
                ),
            )
            .expect("admission after circuit close");
        assert!(matches!(admitted, StoreAdmissionOutcome::Admitted(_)));
    }

    #[test]
    fn repeated_budget_alert_crossings_are_deduplicated() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        store
            .apply_model_control_policy_transition(
                ModelControlMode::Normal,
                true,
                &[session_policy(
                    session_id,
                    ModelInvocationPurpose::SessionLaunchFresh,
                    Some(2),
                    None,
                    Some(0.5),
                )],
                &[],
            )
            .expect("policy transition");

        let invocation_id = Uuid::new_v4();
        let admission = store
            .admit_model_invocation(
                invocation_id,
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "alert-dedup",
                    session_id,
                ),
            )
            .expect("admission");
        assert_eq!(admission, StoreAdmissionOutcome::Admitted(invocation_id));

        let first = store
            .record_budget_alert_crossings(invocation_id)
            .expect("first crossing");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].metric, "calls");
        assert_eq!(first[0].remaining, 1);
        assert_eq!(first[0].threshold, 1);

        let second = store
            .record_budget_alert_crossings(invocation_id)
            .expect("second crossing");
        assert!(second.is_empty());

        let persisted = store
            .list_budget_alert_events_for_invocation(invocation_id)
            .expect("persisted alerts");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].metric, "calls");
    }

    #[test]
    fn over_budget_actual_sets_breaker_and_blocks_next_paid_call() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        let mut policy = session_policy(
            session_id,
            ModelInvocationPurpose::SessionLaunchFresh,
            None,
            None,
            None,
        );
        policy.max_input_tokens = Some(1_000_000);
        store
            .update_model_budget_policies(&[policy], false)
            .expect("explicit usage budget");
        let invocation_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "over-budget-root",
                    session_id,
                ),
            )
            .expect("admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected admission: {other:?}"),
        };

        store
            .complete_model_invocation(
                invocation_id,
                &InvocationCompletion {
                    input_tokens: Some(250),
                    output_tokens: Some(55),
                    wall_time_ms: Some(125),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .expect("completion");

        let settled = store
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                params![invocation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("settled row");
        assert_eq!(settled.0, "failed");
        assert_eq!(settled.1.as_deref(), Some("over_budget_actual_exceeded"));

        let breaker = store
            .conn
            .query_row(
                "SELECT value FROM daemon_settings WHERE key = ?1",
                params![KEY_MODEL_CONTROL_MODE],
                |row| row.get::<_, String>(0),
            )
            .expect("breaker mode");
        assert_eq!(breaker, "deny_paid");

        let reason = store
            .conn
            .query_row(
                "SELECT value FROM daemon_settings WHERE key = ?1",
                params![KEY_MODEL_CONTROL_BREAKER_REASON],
                |row| row.get::<_, String>(0),
            )
            .expect("breaker reason");
        assert_eq!(reason, "actual_over_reservation");

        let denied = store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "after-breaker",
                    Uuid::new_v4(),
                ),
            )
            .expect("admission outcome");
        assert!(matches!(denied, StoreAdmissionOutcome::Denied { .. }));
    }

    #[test]
    fn over_reservation_without_explicit_usage_budget_stays_completed_and_admits_follow_up() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        let invocation_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "unbounded-over-reservation",
                    session_id,
                ),
            )
            .expect("admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected admission: {other:?}"),
        };

        store
            .complete_model_invocation(
                invocation_id,
                &InvocationCompletion {
                    input_tokens: Some(250),
                    output_tokens: Some(55),
                    wall_time_ms: Some(125),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .expect("completion");

        let settled = store
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                params![invocation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("settled row");
        assert_eq!(settled.0, "completed");
        assert_eq!(settled.1, None);

        let follow_up = store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "unbounded-follow-up",
                    Uuid::new_v4(),
                ),
            )
            .expect("follow-up admission outcome");
        assert!(matches!(follow_up, StoreAdmissionOutcome::Admitted(_)));
    }

    #[test]
    fn legacy_invocation_without_usage_budget_snapshot_keeps_fail_closed_settlement() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        let invocation_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "legacy-over-reservation",
                    session_id,
                ),
            )
            .expect("admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected admission: {other:?}"),
        };
        store
            .conn
            .execute(
                "UPDATE model_invocations SET policy_snapshot_json = '{}' WHERE id = ?1",
                params![invocation_id.to_string()],
            )
            .expect("simulate pre-change invocation");

        store
            .complete_model_invocation(
                invocation_id,
                &InvocationCompletion {
                    input_tokens: Some(250),
                    output_tokens: Some(55),
                    wall_time_ms: Some(125),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .expect("completion");

        let settled = store
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                params![invocation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("settled row");
        assert_eq!(settled.0, "failed");
        assert_eq!(settled.1.as_deref(), Some("over_budget_actual_exceeded"));
    }

    #[test]
    fn cache_read_overrun_alone_does_not_trip_breaker() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("registry");
        let invocation_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionContinueResume,
                    "cache-read-overrun",
                    session_id,
                ),
            )
            .expect("admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected admission: {other:?}"),
        };

        // Every dimension stays within its reservation except cache reads,
        // which massively exceed the reserved 5 tokens — the shape every
        // warm-prompt-cache provider turn produces. (Kept below the default
        // provider total-token counter cap so the follow-up admission below
        // exercises the breaker, not the cost counters.)
        store
            .complete_model_invocation(
                invocation_id,
                &InvocationCompletion {
                    input_tokens: Some(150),
                    output_tokens: Some(40),
                    cache_read_tokens: Some(50_000),
                    wall_time_ms: Some(120),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .expect("completion");

        let settled = store
            .conn
            .query_row(
                "SELECT status, error_class, cache_read_tokens FROM model_invocations WHERE id = ?1",
                params![invocation_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .expect("settled row");
        assert_eq!(settled.0, "completed");
        assert_eq!(settled.1, None);
        assert_eq!(settled.2, Some(49_995));

        let breaker = store
            .conn
            .query_row(
                "SELECT value FROM daemon_settings WHERE key = ?1",
                params![KEY_MODEL_CONTROL_MODE],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .expect("breaker mode query");
        assert_ne!(breaker.as_deref(), Some("deny_paid"));

        // A follow-up paid call is still admitted.
        match store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionContinueResume,
                    "after-cache-read-overrun",
                    Uuid::new_v4(),
                ),
            )
            .expect("follow-up paid call still admitted")
        {
            StoreAdmissionOutcome::Admitted(_) => {}
            other => panic!("unexpected admission: {other:?}"),
        }
    }

    /// Admit `req`, then settle it as a terminal failure with `error_class`.
    fn admit_then_fail(
        store: &Store,
        registry: RegistryEntry,
        req: &ModelAdmissionRequest,
        error_class: &str,
    ) -> Uuid {
        let id = match store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Local, req)
            .expect("admit")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected admission: {other:?}"),
        };
        store
            .complete_model_invocation(
                id,
                &InvocationCompletion {
                    error_class: Some(error_class.to_string()),
                    wall_time_ms: Some(10),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .expect("settle as failed");
        id
    }

    /// A terminally-failed attempt must not permanently suppress retries of
    /// identical content. This is the deadlock that stalled memory-transcript
    /// sync: the failed row's content-derived dedup key matched every retry, so
    /// the batch was rejected forever and no transcript was ever indexed.
    #[test]
    fn failed_invocation_does_not_block_retry_of_identical_request() {
        let store = Store::open_in_memory().expect("store");
        let registry = registry::lookup(ModelInvocationPurpose::MemoryEmbeddingIndex)
            .copied()
            .expect("registry");
        let session_id = Uuid::new_v4();
        let req = request(
            ModelInvocationPurpose::MemoryEmbeddingIndex,
            "memory-embedding:sessions/abc:sha256:same",
            session_id,
        );

        let failed_id = admit_then_fail(&store, registry, &req, "process");

        let status: String = store
            .conn
            .query_row(
                "SELECT status FROM model_invocations WHERE id = ?1",
                params![failed_id.to_string()],
                |row| row.get(0),
            )
            .expect("status");
        assert_eq!(status, "failed");

        // The identical request must now be admitted, not suppressed.
        let retry_id = match store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Local, &req)
            .expect("retry admitted")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("retry should be admitted, got: {other:?}"),
        };
        assert_ne!(retry_id, failed_id);

        // The failed row is retained for audit, with its key released so the
        // UNIQUE partial index on dedup_key does not reject the retry.
        let (dedup_key, fingerprint): (Option<String>, Option<String>) = store
            .conn
            .query_row(
                "SELECT dedup_key, request_fingerprint FROM model_invocations WHERE id = ?1",
                params![failed_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("failed row retained");
        assert_eq!(dedup_key, None, "dead key must be released");
        assert!(fingerprint.is_some(), "audit trail must survive");
    }

    /// The retry itself claims the key, so a *second* concurrent attempt while
    /// the retry is still running is suppressed as before. Releasing dead keys
    /// must not degrade in-flight double-spend protection.
    #[test]
    fn running_invocation_still_suppresses_duplicate_after_failed_predecessor() {
        let store = Store::open_in_memory().expect("store");
        let registry = registry::lookup(ModelInvocationPurpose::MemoryEmbeddingIndex)
            .copied()
            .expect("registry");
        let session_id = Uuid::new_v4();
        let req = request(
            ModelInvocationPurpose::MemoryEmbeddingIndex,
            "memory-embedding:sessions/def:sha256:same",
            session_id,
        );

        admit_then_fail(&store, registry, &req, "process");

        let retry_id = match store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Local, &req)
            .expect("retry admitted")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("retry should be admitted, got: {other:?}"),
        };

        // Retry is `running` and holds the key — suppress a concurrent third.
        match store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Local, &req)
            .expect("third attempt")
        {
            StoreAdmissionOutcome::Duplicate(id) => assert_eq!(id, retry_id),
            other => panic!("in-flight duplicate must be suppressed, got: {other:?}"),
        }
    }

    /// A completed attempt is real work with a real result; its key stays held.
    #[test]
    fn completed_invocation_still_suppresses_duplicate() {
        let store = Store::open_in_memory().expect("store");
        let registry = registry::lookup(ModelInvocationPurpose::MemoryEmbeddingIndex)
            .copied()
            .expect("registry");
        let session_id = Uuid::new_v4();
        let req = request(
            ModelInvocationPurpose::MemoryEmbeddingIndex,
            "memory-embedding:sessions/ghi:sha256:same",
            session_id,
        );

        let first_id = match store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Local, &req)
            .expect("admit")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected admission: {other:?}"),
        };
        store
            .complete_model_invocation(
                first_id,
                &InvocationCompletion {
                    input_tokens: Some(100),
                    wall_time_ms: Some(10),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .expect("settle completed");

        match store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Local, &req)
            .expect("duplicate attempt")
        {
            StoreAdmissionOutcome::Duplicate(id) => assert_eq!(id, first_id),
            other => panic!("completed duplicate must be suppressed, got: {other:?}"),
        }
    }

    /// 150k-token request used by the displacement tests: big enough that a
    /// handful of settled calls cross the 1M default global cap, small enough
    /// to stay inside the per-tree defaults (300k standard).
    fn heavy_request(dedup_key: &str) -> ModelAdmissionRequest {
        ModelAdmissionRequest {
            expected_usage: Some(crate::model_control::ExpectedUsage {
                input_tokens: 150_000,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                reasoning_tokens: 0,
                embedding_input_count: 0,
                wall_time_ms: 200_000,
            }),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_wall_time_ms: 0,
            ..request(
                ModelInvocationPurpose::SessionLaunchFresh,
                dedup_key,
                Uuid::new_v4(),
            )
        }
    }

    fn settle_heavy(store: &Store, invocation_id: Uuid) {
        store
            .complete_model_invocation(
                invocation_id,
                &InvocationCompletion {
                    input_tokens: Some(150_000),
                    wall_time_ms: Some(100),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .expect("completion");
    }

    #[test]
    fn explicit_policies_displace_default_caps() {
        let store = Store::open_in_memory().expect("store");
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        // Operator raises the global and provider budgets well past the
        // hardcoded defaults (global 1M / provider 500k total tokens).
        let raised = |scope_kind, scope_id: Option<&str>| ModelBudgetPolicy {
            scope_kind,
            scope_id: scope_id.map(str::to_string),
            purpose: None,
            model_tier: None,
            effort: None,
            ceiling_model_tier: None,
            ceiling_effort: None,
            max_calls: None,
            max_total_tokens: Some(5_000_000),
            max_input_tokens: None,
            max_output_tokens: None,
            max_cache_creation_tokens: None,
            max_cache_read_tokens: None,
            max_reasoning_tokens: None,
            max_embedding_inputs: None,
            max_wall_time_ms: None,
            max_concurrency: Some(8),
            max_retries: None,
            max_calls_per_window: None,
            rate_window_seconds: None,
            alert_threshold_ratio: None,
        };
        store
            .update_model_budget_policies(
                &[
                    raised(BudgetScopeKind::Global, None),
                    raised(BudgetScopeKind::Provider, Some("codex")),
                ],
                false,
            )
            .expect("policies persisted");

        // 8 settled 150k calls = 1.2M cumulative tokens — past both defaults.
        // Under the old stack-defaults-on-top behavior the default caps kept
        // denying regardless of the operator's explicit budgets.
        for idx in 0..8 {
            let invocation_id = match store
                .admit_model_invocation(
                    Uuid::new_v4(),
                    registry,
                    ModelTier::Standard,
                    &heavy_request(&format!("displace-{idx}")),
                )
                .expect("admission")
            {
                StoreAdmissionOutcome::Admitted(id) => id,
                other => panic!("call {idx} unexpectedly not admitted: {other:?}"),
            };
            settle_heavy(&store, invocation_id);
        }
    }

    #[test]
    fn default_caps_do_not_apply_without_explicit_policies() {
        let store = Store::open_in_memory().expect("store");
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        // Budgets are unbounded by default now. 3 x 150k settled + a 4th
        // 150k reservation would have crossed the old hardcoded provider cap
        // (500k total tokens); with no explicit policy in place it must be
        // admitted like every other call.
        for idx in 0..4 {
            let invocation_id = match store
                .admit_model_invocation(
                    Uuid::new_v4(),
                    registry,
                    ModelTier::Standard,
                    &heavy_request(&format!("default-cap-{idx}")),
                )
                .expect("admission")
            {
                StoreAdmissionOutcome::Admitted(id) => id,
                other => panic!("call {idx} unexpectedly not admitted: {other:?}"),
            };
            settle_heavy(&store, invocation_id);
        }
    }

    #[test]
    fn explicit_tree_premium_cap_denies_fifth_call() {
        let store = Store::open_in_memory().expect("store");
        let root_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("root registry");
        let child_registry = registry::lookup(ModelInvocationPurpose::AgentSpawnChild)
            .copied()
            .expect("child registry");
        let root_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                root_registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "premium-root",
                    Uuid::new_v4(),
                ),
            )
            .expect("root admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected root outcome: {other:?}"),
        };

        // No default tree cap anymore — an operator must place one explicitly
        // to get the same protection the old hardcoded default used to give
        // for free. Root's own invocation id is the tree's scope id.
        store
            .update_model_budget_policies(
                &[ModelBudgetPolicy {
                    scope_kind: BudgetScopeKind::Tree,
                    scope_id: Some(root_id.to_string()),
                    purpose: None,
                    model_tier: Some(ModelTier::Premium),
                    effort: None,
                    ceiling_model_tier: None,
                    ceiling_effort: None,
                    max_calls: Some(4),
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
                }],
                false,
            )
            .expect("tree cap persisted");

        for idx in 0..3 {
            let mut child = request(
                ModelInvocationPurpose::AgentSpawnChild,
                &format!("premium-child-{idx}"),
                Uuid::new_v4(),
            );
            child.parent_invocation_id = Some(root_id);
            let child_id = match store
                .admit_model_invocation(Uuid::new_v4(), child_registry, ModelTier::Premium, &child)
                .expect("child admission")
            {
                StoreAdmissionOutcome::Admitted(id) => id,
                other => panic!("unexpected child outcome: {other:?}"),
            };
            store
                .complete_model_invocation(child_id, &InvocationCompletion::default())
                .expect("child completion");
        }

        let mut denied = request(
            ModelInvocationPurpose::AgentSpawnChild,
            "premium-child-denied",
            Uuid::new_v4(),
        );
        denied.parent_invocation_id = Some(root_id);
        match store
            .admit_model_invocation(Uuid::new_v4(), child_registry, ModelTier::Premium, &denied)
            .expect("admission call")
        {
            StoreAdmissionOutcome::Denied { reason, .. } => {
                assert!(reason.contains("budget"), "unexpected reason: {reason}");
            }
            other => {
                panic!("expected fifth-call denial once an explicit cap is placed, got: {other:?}")
            }
        }
    }

    // ─── Issue #34: operator-set child-effort ceiling ─────────────────────
    //
    // These exercise BOTH directions. Proving the ceiling can now be raised is
    // only half the job: a fix that quietly disabled the check would pass that
    // half alone, so each relaxation case is paired with a case proving the
    // surviving guardrail still denies what it must.

    /// Writes the operator ceiling into the existing V48 `daemon_settings`
    /// key-value table. Deliberately raw SQL against the same table
    /// `current_model_control_mode_tx` uses — issue #34 adds a KEY, never a
    /// schema version.
    fn set_operator_effort_ceiling(store: &Store, value: &str) {
        store
            .conn
            .execute(
                "INSERT INTO daemon_settings (key, value, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![
                    KEY_ORCHESTRATION_MAX_CHILD_EFFORT,
                    value,
                    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
                ],
            )
            .expect("write operator effort ceiling");
    }

    /// Admits a `premium` root at `root_effort` and returns its invocation id.
    fn premium_root_at_effort(store: &Store, root_effort: &str, dedup: &str) -> Uuid {
        let root_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("root registry");
        let mut root = request(
            ModelInvocationPurpose::SessionLaunchFresh,
            dedup,
            Uuid::new_v4(),
        );
        root.provider = Some("Codex".to_string());
        root.model = Some("gpt-5.4".to_string());
        root.backend = Some("Codex".to_string());
        root.effort = Some(root_effort.to_string());
        match store
            .admit_model_invocation(Uuid::new_v4(), root_registry, ModelTier::Premium, &root)
            .expect("root admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected root outcome: {other:?}"),
        }
    }

    /// Attempts an `AgentSpawnChild` (an `Orchestration`-kind purpose) under
    /// `root_id` at `tier`/`effort`.
    fn spawn_child_at(
        store: &Store,
        root_id: Uuid,
        tier: ModelTier,
        effort: &str,
        dedup: &str,
    ) -> StoreAdmissionOutcome {
        let child_registry = registry::lookup(ModelInvocationPurpose::AgentSpawnChild)
            .copied()
            .expect("child registry");
        let mut child = request(
            ModelInvocationPurpose::AgentSpawnChild,
            dedup,
            Uuid::new_v4(),
        );
        child.parent_invocation_id = Some(root_id);
        child.provider = Some("Codex".to_string());
        child.model = Some("gpt-5.4".to_string());
        child.backend = Some("Codex".to_string());
        child.effort = Some(effort.to_string());
        store
            .admit_model_invocation(Uuid::new_v4(), child_registry, tier, &child)
            .expect("child admission result")
    }

    fn denial_reason(outcome: &StoreAdmissionOutcome) -> &str {
        match outcome {
            StoreAdmissionOutcome::Denied { reason, .. } => reason,
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    /// The exact failure measured on the Issue #21 Phase 2 campaign: identical
    /// `premium` tier on both sides, denied purely because the child asked for
    /// `xhigh` under a `high` orchestrator. With the operator ceiling declared
    /// once for the campaign, the same spawn is admitted.
    #[test]
    fn operator_effort_ceiling_admits_child_above_tree_root_effort() {
        let store = Store::open_in_memory().expect("store");
        let root_id = premium_root_at_effort(&store, "high", "i34-raise-root");

        // Precondition: without the key, this is denied. Asserting it here is
        // what makes the post-condition below evidence of the ceiling rather
        // than evidence that the spawn was always permitted.
        let before = spawn_child_at(&store, root_id, ModelTier::Premium, "xhigh", "i34-raise-a");
        assert!(
            matches!(before, StoreAdmissionOutcome::Denied { .. }),
            "pre-#34 behaviour must still hold with the key unset, got {before:?}"
        );

        set_operator_effort_ceiling(&store, "xhigh");
        let after = spawn_child_at(&store, root_id, ModelTier::Premium, "xhigh", "i34-raise-b");
        assert!(
            matches!(after, StoreAdmissionOutcome::Admitted(_)),
            "operator ceiling xhigh must admit an xhigh child under a high root, got {after:?}"
        );
    }

    /// The surviving guardrail, effort half: raising the ceiling to `xhigh`
    /// must NOT uncap effort — `max` is still above the declared limit.
    #[test]
    fn operator_effort_ceiling_still_denies_above_the_declared_limit() {
        let store = Store::open_in_memory().expect("store");
        let root_id = premium_root_at_effort(&store, "high", "i34-cap-root");
        set_operator_effort_ceiling(&store, "xhigh");

        let outcome = spawn_child_at(&store, root_id, ModelTier::Premium, "max", "i34-cap-child");
        let reason = denial_reason(&outcome);
        assert!(
            reason.contains("effort escalation denied"),
            "max must still be denied above an xhigh ceiling, got {reason}"
        );
        assert!(
            reason.contains(KEY_ORCHESTRATION_MAX_CHILD_EFFORT),
            "denial must attribute the limit to the operator ceiling, got {reason}"
        );
    }

    /// The surviving guardrail, tier half: the effort ceiling must not open a
    /// hole in the cost-dominant tier check. Even with effort uncapped to the
    /// top rank, a `premium` child under a `standard` root stays denied.
    #[test]
    fn operator_effort_ceiling_does_not_relax_the_model_tier_guardrail() {
        let store = Store::open_in_memory().expect("store");
        let root_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("root registry");
        let mut root = request(
            ModelInvocationPurpose::SessionLaunchFresh,
            "i34-tier-root",
            Uuid::new_v4(),
        );
        root.provider = Some("Codex".to_string());
        root.model = Some("gpt-4.1".to_string());
        root.backend = Some("Codex".to_string());
        root.effort = Some("low".to_string());
        let root_id = match store
            .admit_model_invocation(Uuid::new_v4(), root_registry, ModelTier::Standard, &root)
            .expect("root admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected root outcome: {other:?}"),
        };

        set_operator_effort_ceiling(&store, "ultra");
        let outcome = spawn_child_at(&store, root_id, ModelTier::Premium, "low", "i34-tier-child");
        let reason = denial_reason(&outcome);
        assert!(
            reason.contains("model tier escalation denied"),
            "tier guardrail must survive an uncapped effort ceiling, got {reason}"
        );
    }

    /// Default (key unset) is bit-for-bit the pre-#34 rule, and the denial now
    /// names the knob instead of only stating the limit.
    #[test]
    fn unset_operator_ceiling_preserves_tree_root_effort_limit_and_names_the_knob() {
        let store = Store::open_in_memory().expect("store");
        let root_id = premium_root_at_effort(&store, "high", "i34-default-root");

        let outcome = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "xhigh",
            "i34-default-child",
        );
        let reason = denial_reason(&outcome);
        assert!(
            reason.contains("denied by tree root effort"),
            "unset key must fall back to the tree root, got {reason}"
        );
        assert!(
            reason.contains(KEY_ORCHESTRATION_MAX_CHILD_EFFORT),
            "denial must name the knob that raises the ceiling, got {reason}"
        );
    }

    /// The ceiling REPLACES the root's effort, so it can tighten as well as
    /// raise: a `high` child under a `high` root is denied at a `low` ceiling.
    #[test]
    fn operator_effort_ceiling_can_tighten_below_the_tree_root() {
        let store = Store::open_in_memory().expect("store");
        let root_id = premium_root_at_effort(&store, "high", "i34-tighten-root");
        set_operator_effort_ceiling(&store, "low");

        let outcome = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "high",
            "i34-tighten-child",
        );
        let reason = denial_reason(&outcome);
        assert!(
            reason.contains("effort escalation denied"),
            "a low ceiling must bind below the root effort, got {reason}"
        );
    }

    /// A typo must degrade to the tree-root default, never to a rank-0 ceiling
    /// that denies every child naming any effort at all.
    /// Issue #35 / R8 LOW-2. A BLOB in `daemon_settings.value` used to abort
    /// the read with `InvalidColumnType`; `admit_model_invocation` catches
    /// every `Err` as a policy denial, so ONE malformed row denied EVERY
    /// orchestration spawn — including children at or below the tree root's
    /// own effort, which no ceiling should ever refuse. It must now degrade to
    /// unset and fall back to the tree root.
    #[test]
    fn non_text_operator_ceiling_degrades_to_unset_instead_of_denying_every_spawn() {
        let store = Store::open_in_memory().expect("store");
        let root_id = premium_root_at_effort(&store, "high", "i35-blob-root");
        store
            .conn
            .execute(
                "INSERT INTO daemon_settings (key, value, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![
                    KEY_ORCHESTRATION_MAX_CHILD_EFFORT,
                    // TEXT affinity coerces INTEGER and REAL, but not a BLOB.
                    vec![0xdeu8, 0xad, 0xbe, 0xef],
                    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
                ],
            )
            .expect("write BLOB ceiling");

        // The regression: a child AT the tree root's effort must still be
        // admitted. Before the fix this was denied with a raw database string.
        let at_root = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "high",
            "i35-blob-at-root",
        );
        assert!(
            matches!(at_root, StoreAdmissionOutcome::Admitted(_)),
            "a malformed ceiling must not deny a child at the root effort, got {at_root:?}"
        );

        // And the fallback is the tree root, not "allow anything": above-root
        // is still denied, by the root, with a policy message rather than a
        // leaked database error.
        let above = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "xhigh",
            "i35-blob-above",
        );
        let reason = denial_reason(&above);
        assert!(
            reason.contains("denied by tree root effort"),
            "malformed ceiling must fall back to the tree root, got {reason}"
        );
        assert!(
            !reason.contains("Database error") && !reason.contains("Invalid column type"),
            "a malformed setting must not leak a database error to the operator, got {reason}"
        );
    }

    /// Issue #35. The explicit "unset" sentinel the operator surface writes
    /// must read back as absence — the pre-#34 tree-root rule — and must not
    /// be reported as an unrecognized typo.
    #[test]
    fn explicit_unset_sentinel_reads_as_absent_ceiling() {
        let store = Store::open_in_memory().expect("store");
        let root_id = premium_root_at_effort(&store, "high", "i35-unset-root");
        for sentinel in ["unset", "UNSET", "  unset  ", ""] {
            set_operator_effort_ceiling(&store, sentinel);
            let at_root = spawn_child_at(
                &store,
                root_id,
                ModelTier::Premium,
                "high",
                &format!("i35-unset-at-root-{sentinel:?}"),
            );
            assert!(
                matches!(at_root, StoreAdmissionOutcome::Admitted(_)),
                "sentinel {sentinel:?} must behave as unset, got {at_root:?}"
            );
            let above = spawn_child_at(
                &store,
                root_id,
                ModelTier::Premium,
                "xhigh",
                &format!("i35-unset-above-{sentinel:?}"),
            );
            let reason = denial_reason(&above);
            assert!(
                reason.contains("denied by tree root effort"),
                "sentinel {sentinel:?} must fall back to the tree root, got {reason}"
            );
            assert!(
                !reason.contains("ignoring unrecognized"),
                "a deliberate unset must not be reported as a typo, got {reason}"
            );
        }
    }

    /// Issue #35. The operator surface, the shared vocabulary and the ranking
    /// function must not drift apart: every non-sentinel choice the operator
    /// can pick has to be a rank `effort_rank` actually recognizes, or picking
    /// it would silently install a deny-everything ceiling.
    #[test]
    fn orchestration_max_child_effort_choices_match_effort_rank() {
        use rsi_common::model_control::{
            ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES, ORCHESTRATION_MAX_CHILD_EFFORT_UNSET,
        };
        assert_eq!(
            ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES.first(),
            Some(&ORCHESTRATION_MAX_CHILD_EFFORT_UNSET),
            "the sentinel must lead the choice list"
        );
        let mut previous = 0;
        for choice in &ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES[1..] {
            let rank = effort_rank(Some(choice));
            assert_ne!(
                rank, 0,
                "{choice} is offered to the operator but ranks 0, which would deny every child"
            );
            assert!(
                rank > previous,
                "choices must be listed in ascending-ceiling order: {choice} ranks {rank}"
            );
            previous = rank;
        }
        // And the operator surface must cover the whole ranked ladder, so no
        // effort a child can request is unreachable as a ceiling.
        for name in ["low", "medium", "high", "xhigh", "max", "ultra"] {
            assert!(
                ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES.contains(&name),
                "{name} is rankable but not offered on the operator surface"
            );
        }
    }

    /// Issue #35, the point of the whole slice: a ceiling set through the
    /// OPERATOR path (validated `update_field` + the generic `daemon_settings`
    /// write-through, exactly what `UpdateDaemonConfig` does) must actually
    /// change admission. Before this, #34's mechanism was live but inert —
    /// reachable only by hand-writing SQL.
    #[test]
    fn ceiling_set_through_the_operator_write_path_changes_admission() {
        let store = Store::open_in_memory().expect("store");
        let root_id = premium_root_at_effort(&store, "high", "i35-e2e-root");

        // Baseline: with no ceiling, the tree root's `high` caps the child.
        let denied = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "xhigh",
            "i35-e2e-before",
        );
        assert!(
            denial_reason(&denied).contains("denied by tree root effort"),
            "baseline must be capped by the tree root"
        );

        // Operator raises the ceiling through the validated RPC path.
        let runtime_config = crate::config::RuntimeConfig::from_config(&Default::default());
        assert!(
            runtime_config
                .update_field(
                    KEY_ORCHESTRATION_MAX_CHILD_EFFORT,
                    &serde_json::json!("xhigh"),
                )
                .expect("operator write must validate"),
            "field must be recognized"
        );
        assert!(
            crate::store::daemon_settings::persist_runtime_config_field(
                &store,
                &runtime_config,
                KEY_ORCHESTRATION_MAX_CHILD_EFFORT,
            )
            .expect("write-through must succeed"),
            "the ceiling must be on the persisted-field allowlist"
        );

        // Same request, now admitted — the operator surface is not inert.
        let admitted = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "xhigh",
            "i35-e2e-after",
        );
        assert!(
            matches!(admitted, StoreAdmissionOutcome::Admitted(_)),
            "operator-set ceiling must raise the limit, got {admitted:?}"
        );

        // The tier guardrail is untouched by the effort ceiling.
        let tier_escalation = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "max",
            "i35-e2e-above-ceiling",
        );
        assert!(
            denial_reason(&tier_escalation).contains("operator ceiling"),
            "above the operator ceiling must be denied by the operator ceiling"
        );
    }

    /// Issue #35. The persisted field name must equal the key the admission
    /// read path looks up, or the operator surface would write to a row nothing
    /// reads.
    #[test]
    fn orchestration_max_child_effort_field_name_matches_store_key() {
        assert!(
            crate::config::PERSISTED_RUNTIME_CONFIG_FIELDS
                .contains(&KEY_ORCHESTRATION_MAX_CHILD_EFFORT),
            "the ceiling must be persisted under exactly the key the read path uses"
        );
        assert!(crate::config::is_persisted_runtime_config_field(
            KEY_ORCHESTRATION_MAX_CHILD_EFFORT
        ));
    }

    #[test]
    fn unrecognized_operator_ceiling_falls_back_to_tree_root_instead_of_denying_everything() {
        let store = Store::open_in_memory().expect("store");
        let root_id = premium_root_at_effort(&store, "high", "i34-typo-root");
        set_operator_effort_ceiling(&store, "xhigh ");
        // Whitespace/case are normalized, so this one is honoured, not a typo.
        let normalized = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "xhigh",
            "i34-typo-norm",
        );
        assert!(
            matches!(normalized, StoreAdmissionOutcome::Admitted(_)),
            "whitespace must be trimmed, got {normalized:?}"
        );

        set_operator_effort_ceiling(&store, "XHIGH!!");
        // Falls back to the root's `high`: a child AT the root effort is still
        // admitted (proving we did not install a deny-everything ceiling)...
        let at_root = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "high",
            "i34-typo-at-root",
        );
        assert!(
            matches!(at_root, StoreAdmissionOutcome::Admitted(_)),
            "an invalid ceiling must not deny a child at the root effort, got {at_root:?}"
        );
        // ...while a child above the root is denied by the root, and the
        // denial reports the value that was ignored.
        let above = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "xhigh",
            "i34-typo-above",
        );
        let reason = denial_reason(&above);
        assert!(
            reason.contains("denied by tree root effort")
                && reason.contains("ignoring unrecognized"),
            "invalid ceiling must fall back to the root and be reported, got {reason}"
        );
    }

    /// The ordinary paths stay untouched: a child BELOW the root is admitted,
    /// and a non-`Orchestration` purpose is not subject to the guardrail at all
    /// even when it exceeds a deliberately tight ceiling.
    #[test]
    fn child_below_root_and_non_orchestration_purpose_are_unaffected() {
        let store = Store::open_in_memory().expect("store");
        let root_id = premium_root_at_effort(&store, "high", "i34-ordinary-root");
        set_operator_effort_ceiling(&store, "low");

        let below = spawn_child_at(
            &store,
            root_id,
            ModelTier::Premium,
            "low",
            "i34-ordinary-below",
        );
        assert!(
            matches!(below, StoreAdmissionOutcome::Admitted(_)),
            "economising downward must keep working, got {below:?}"
        );

        // SessionLaunchFresh is `SessionLifecycle`, not `Orchestration`, so the
        // guardrail returns Ok before reading any ceiling.
        let lifecycle_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("lifecycle registry");
        let mut lifecycle = request(
            ModelInvocationPurpose::SessionLaunchFresh,
            "i34-ordinary-lifecycle",
            Uuid::new_v4(),
        );
        lifecycle.parent_invocation_id = Some(root_id);
        lifecycle.provider = Some("Codex".to_string());
        lifecycle.model = Some("gpt-5.4".to_string());
        lifecycle.backend = Some("Codex".to_string());
        lifecycle.effort = Some("ultra".to_string());
        let outcome = store
            .admit_model_invocation(
                Uuid::new_v4(),
                lifecycle_registry,
                ModelTier::Premium,
                &lifecycle,
            )
            .expect("lifecycle admission result");
        assert!(
            matches!(outcome, StoreAdmissionOutcome::Admitted(_)),
            "non-Orchestration purposes must not consult the ceiling, got {outcome:?}"
        );
    }

    #[test]
    fn low_tier_root_denies_premium_effort_escalation() {
        let store = Store::open_in_memory().expect("store");
        let root_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("root registry");
        let child_registry = registry::lookup(ModelInvocationPurpose::AgentSpawnChild)
            .copied()
            .expect("child registry");
        let root_session_id = Uuid::new_v4();
        let mut root = request(
            ModelInvocationPurpose::SessionLaunchFresh,
            "standard-root",
            root_session_id,
        );
        root.provider = Some("Codex".to_string());
        root.model = Some("gpt-4.1".to_string());
        root.backend = Some("Codex".to_string());
        root.effort = Some("low".to_string());
        let root_id = match store
            .admit_model_invocation(Uuid::new_v4(), root_registry, ModelTier::Standard, &root)
            .expect("root admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected root outcome: {other:?}"),
        };

        let mut child = request(
            ModelInvocationPurpose::AgentSpawnChild,
            "premium-child",
            Uuid::new_v4(),
        );
        child.parent_invocation_id = Some(root_id);
        child.provider = Some("Codex".to_string());
        child.model = Some("gpt-5.4".to_string());
        child.backend = Some("Codex".to_string());
        child.effort = Some("high".to_string());
        let denied = store
            .admit_model_invocation(Uuid::new_v4(), child_registry, ModelTier::Premium, &child)
            .expect("admission result");
        assert!(matches!(denied, StoreAdmissionOutcome::Denied { .. }));
    }

    #[test]
    fn dedup_conflict_is_denied_when_fingerprint_changes() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");

        let first = request(
            ModelInvocationPurpose::SessionLaunchFresh,
            "dedup-conflict",
            session_id,
        );
        store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Premium, &first)
            .expect("first admission");

        let mut second = request(
            ModelInvocationPurpose::SessionLaunchFresh,
            "dedup-conflict",
            session_id,
        );
        second.request_fingerprint = Some("sha256:other".to_string());
        let error = store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Premium, &second)
            .expect_err("dedup conflict must deny");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
    }

    #[test]
    fn scheduled_replay_dedup_requires_exact_stable_owner_identity() {
        let ordinary_store = Store::open_in_memory().expect("ordinary store");
        let ordinary_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("ordinary registry");
        let ordinary_session_id = Uuid::new_v4();
        let ordinary = request(
            ModelInvocationPurpose::SessionLaunchFresh,
            "ordinary-different-session-conflict",
            ordinary_session_id,
        );
        ordinary_store
            .admit_model_invocation(
                Uuid::new_v4(),
                ordinary_registry,
                ModelTier::Premium,
                &ordinary,
            )
            .expect("admit ordinary request");
        let mut ordinary_different_session = ordinary.clone();
        ordinary_different_session.owner.session_id = Some(Uuid::new_v4());
        let ordinary_error = ordinary_store
            .admit_model_invocation(
                Uuid::new_v4(),
                ordinary_registry,
                ModelTier::Premium,
                &ordinary_different_session,
            )
            .expect_err("ordinary different-session replay must conflict");
        assert!(matches!(ordinary_error, DaemonError::PolicyDenied(_)));

        let store = Store::open_in_memory().expect("scheduled replay store");
        let purpose = ModelInvocationPurpose::AgentScheduleWakeFresh;
        let registry = registry::lookup(purpose)
            .copied()
            .expect("scheduled registry");
        let session_id = Uuid::new_v4();
        let scheduled_job_id = Uuid::new_v4();
        let mut scheduled = request(purpose, "scheduled-replay-identity", session_id);
        scheduled.owner = rsi_common::model_control::InvocationOwner {
            session_id: Some(session_id),
            project_id: Some(Uuid::new_v4()),
            workflow_id: Some(Uuid::new_v4()),
            scheduled_job_id: Some(scheduled_job_id),
            issue_tracker_id: Some("linear".to_string()),
            issue_identifier: Some("RSI-42".to_string()),
            topology_node_id: Some("topology-node-a".to_string()),
            recursive_graph_id: Some("graph-a".to_string()),
            recursive_task_id: Some("task-a".to_string()),
            recursive_attempt_id: Some("attempt-a".to_string()),
            operator: Some("scheduler".to_string()),
        };
        let first_id = Uuid::new_v4();
        assert_eq!(
            store
                .admit_model_invocation(first_id, registry, ModelTier::Premium, &scheduled)
                .expect("admit scheduled request"),
            StoreAdmissionOutcome::Admitted(first_id)
        );

        let mut exact_replay = scheduled.clone();
        exact_replay.owner.session_id = Some(Uuid::new_v4());
        assert_eq!(
            store
                .admit_model_invocation(
                    Uuid::new_v4(),
                    registry,
                    ModelTier::Premium,
                    &exact_replay,
                )
                .expect("exact scheduled replay"),
            StoreAdmissionOutcome::Duplicate(first_id)
        );
        let row_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |row| {
                row.get(0)
            })
            .expect("count scheduled replay rows");
        assert_eq!(row_count, 1, "exact replay retains one invocation row");
        let active_count: i64 = store
            .conn
            .query_row(
                "SELECT active_count FROM model_budget_counters
                 WHERE scope_kind = 'global'
                   AND scope_id = 'global'
                   AND purpose = '__all__'
                   AND model_tier = '__all__'
                   AND effort = '__all__'",
                [],
                |row| row.get(0),
            )
            .expect("load global scheduled replay reservation");
        assert_eq!(active_count, 1, "exact replay does not reserve twice");

        let cases: [(&str, fn(&mut ModelAdmissionRequest)); 12] = [
            ("purpose", |request| {
                request.purpose = ModelInvocationPurpose::ScheduledFresh;
            }),
            ("fingerprint", |request| {
                request.request_fingerprint = Some("sha256:different".to_string());
            }),
            ("project", |request| {
                request.owner.project_id = Some(Uuid::new_v4());
            }),
            ("workflow", |request| {
                request.owner.workflow_id = Some(Uuid::new_v4());
            }),
            ("scheduled job", |request| {
                request.owner.scheduled_job_id = Some(Uuid::new_v4());
            }),
            ("issue tracker", |request| {
                request.owner.issue_tracker_id = Some("github".to_string());
            }),
            ("issue identifier", |request| {
                request.owner.issue_identifier = Some("RSI-99".to_string());
            }),
            ("topology node", |request| {
                request.owner.topology_node_id = Some("topology-node-b".to_string());
            }),
            ("recursive graph", |request| {
                request.owner.recursive_graph_id = Some("graph-b".to_string());
            }),
            ("recursive task", |request| {
                request.owner.recursive_task_id = Some("task-b".to_string());
            }),
            ("recursive attempt", |request| {
                request.owner.recursive_attempt_id = Some("attempt-b".to_string());
            }),
            ("operator", |request| {
                request.owner.operator = Some("manual".to_string());
            }),
        ];
        for (label, mutate) in cases {
            let mut mismatch = exact_replay.clone();
            mutate(&mut mismatch);
            let mismatch_registry = registry::lookup(mismatch.purpose)
                .copied()
                .expect("registry for mismatched request purpose");
            let error = store
                .admit_model_invocation(
                    Uuid::new_v4(),
                    mismatch_registry,
                    ModelTier::Premium,
                    &mismatch,
                )
                .expect_err("{label} mismatch must conflict");
            assert!(
                matches!(error, DaemonError::PolicyDenied(_)),
                "{label} mismatch must fail closed"
            );
        }

        let mut same_session_different_job = scheduled.clone();
        same_session_different_job.owner.scheduled_job_id = Some(Uuid::new_v4());
        let same_session_error = store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &same_session_different_job,
            )
            .expect_err("same session with a different scheduled job must conflict");
        assert!(matches!(same_session_error, DaemonError::PolicyDenied(_)));
    }

    #[test]
    fn missing_lineage_ancestor_is_denied() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::AgentSpawnChild)
            .copied()
            .expect("registry");
        let mut child = request(
            ModelInvocationPurpose::AgentSpawnChild,
            "missing-ancestor",
            session_id,
        );
        child.parent_invocation_id = Some(Uuid::new_v4());

        let error = store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Premium, &child)
            .expect_err("missing ancestor must deny");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
    }

    #[test]
    fn cyclic_lineage_is_denied() {
        let store = Store::open_in_memory().expect("store");
        let root_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        let child_registry = registry::lookup(ModelInvocationPurpose::AgentSpawnChild)
            .copied()
            .expect("registry");
        let root_session_id = Uuid::new_v4();
        let child_session_id = Uuid::new_v4();

        let root_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                root_registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "cycle-root",
                    root_session_id,
                ),
            )
            .expect("root admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected root admission: {other:?}"),
        };

        let mut child_request = request(
            ModelInvocationPurpose::AgentSpawnChild,
            "cycle-child",
            child_session_id,
        );
        child_request.parent_invocation_id = Some(root_id);
        let child_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                child_registry,
                ModelTier::Premium,
                &child_request,
            )
            .expect("child admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected child admission: {other:?}"),
        };

        store
            .conn
            .execute(
                "UPDATE model_invocations SET parent_invocation_id = ?1 WHERE id = ?2",
                params![child_id.to_string(), root_id.to_string()],
            )
            .expect("create cycle");

        let mut grandchild_request = request(
            ModelInvocationPurpose::AgentSpawnChild,
            "cycle-grandchild",
            Uuid::new_v4(),
        );
        grandchild_request.parent_invocation_id = Some(root_id);
        let error = store
            .admit_model_invocation(
                Uuid::new_v4(),
                child_registry,
                ModelTier::Premium,
                &grandchild_request,
            )
            .expect_err("cycle must deny");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
    }

    #[test]
    fn parent_and_retry_links_must_converge_on_one_root() {
        let store = Store::open_in_memory().expect("store");
        let root_registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("root registry");
        let child_registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("child registry");

        let admit_root = |dedup_key: &str| match store
            .admit_model_invocation(
                Uuid::new_v4(),
                root_registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    dedup_key,
                    Uuid::new_v4(),
                ),
            )
            .expect("root admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected root outcome: {other:?}"),
        };
        let first_root_id = admit_root("converging-root");
        let second_root_id = admit_root("conflicting-root");

        let mut child_request = request(
            ModelInvocationPurpose::SessionContinueResume,
            "dual-link-child",
            Uuid::new_v4(),
        );
        child_request.parent_invocation_id = Some(first_root_id);
        let child_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                child_registry,
                ModelTier::Premium,
                &child_request,
            )
            .expect("child admission")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected child outcome: {other:?}"),
        };

        store
            .conn
            .execute(
                "UPDATE model_invocations SET retry_of_invocation_id = ?1 WHERE id = ?2",
                params![first_root_id.to_string(), child_id.to_string()],
            )
            .expect("add converging retry link");
        {
            let tx = store.conn.unchecked_transaction().expect("transaction");
            assert_eq!(
                resolve_existing_tree_root_tx(&tx, child_id).expect("converging root"),
                first_root_id
            );
        }

        store
            .conn
            .execute(
                "UPDATE model_invocations SET retry_of_invocation_id = ?1 WHERE id = ?2",
                params![second_root_id.to_string(), child_id.to_string()],
            )
            .expect("replace with conflicting retry link");
        let tx = store.conn.unchecked_transaction().expect("transaction");
        let error =
            resolve_existing_tree_root_tx(&tx, child_id).expect_err("conflicting roots must deny");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
    }

    #[test]
    fn reconcile_running_model_invocations_releases_active_counters() {
        let store = Store::open_in_memory().expect("store");
        let session_id = Uuid::new_v4();
        let registry = registry::lookup(ModelInvocationPurpose::SessionLaunchFresh)
            .copied()
            .expect("registry");
        let invocation_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                registry,
                ModelTier::Premium,
                &request(
                    ModelInvocationPurpose::SessionLaunchFresh,
                    "reconcile-running",
                    session_id,
                ),
            )
            .expect("admit")
        {
            StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected admission: {other:?}"),
        };

        store
            .reconcile_running_model_invocations()
            .expect("reconcile invocations");

        let status: String = store
            .conn
            .query_row(
                "SELECT status FROM model_invocations WHERE id = ?1",
                params![invocation_id.to_string()],
                |row| row.get(0),
            )
            .expect("status");
        assert_eq!(status, "failed");
        let active_count: i64 = store
            .conn
            .query_row(
                "SELECT active_count FROM model_budget_counters
                 WHERE scope_kind = 'global'
                   AND scope_id = 'global'
                   AND purpose = '__all__'
                   AND model_tier = '__all__'
                   AND effort = '__all__'",
                [],
                |row| row.get(0),
            )
            .expect("global active count");
        assert_eq!(active_count, 0);
    }
}
