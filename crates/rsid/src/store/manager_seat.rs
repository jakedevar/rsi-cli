//! Issue #669: daemon-owned, bounded, in-place recovery of a `Failed`
//! appointed manager seat, plus its durable `manager_seat` record.
//!
//! The seat is the lineage tip resolved by `current_manager_session_on`. A
//! `Failed` tip with no live process is down. Recovery reuses the operator's
//! persisted V2 policy (Execute, not paused, `max_recovery_attempts > 0`,
//! runtime retries enabled, no spend hold, no human gate, resource admission)
//! and never creates authority: the executor resumes the SAME session row and
//! sandbox. It never launches Fresh/AgentFresh, never succeeds the seat and
//! never creates a session. Each attempt is one `seat_recovery` operation row
//! whose idempotency key `seat:<tip>:<n>` and queued-to-running CAS admit it
//! exactly once. A claim left running by a previous owner becomes
//! `uncertain`; it is never blindly relaunched.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::harness_manager::{
    HarnessManagerConfigV1, ManagerSeatConditionV1, ManagerSeatStateV1,
};
use rsi_common::harness_manager_v2::{HarnessManagerPolicyConfigV2, ManagerOperatingModeV2};
use rsi_common::types::{Session, SessionStatus};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::json;
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::harness_manager_v2::{fingerprint, refused};

pub(crate) const SEAT_RECORD_KIND: &str = "manager_seat";
pub(crate) const SEAT_RECORD_KEY: &str = "current";
pub(crate) const SEAT_OPERATION_KIND: &str = "seat_recovery";
/// Stable operator-facing prefix of every seat `SystemMessage`.
pub(crate) const SEAT_MESSAGE_PREFIX: &str = "[manager-seat]";
pub(crate) const SEAT_EXHAUSTED_REASON: &str = "manager_seat_recovery_budget_exhausted";
pub(crate) const SEAT_DISABLED_REASON: &str = "manager_seat_recovery_disabled";
pub(crate) const SEAT_UNCONFIRMED_REASON: &str = "manager_seat_recovery_unconfirmed";
pub(crate) const SEAT_BUSY_OUTCOME: &str = "manager_seat_tip_busy";
/// The tip's provider cannot be continued in place (e.g. `CodexAppServer`,
/// or no captured provider session): K13's shared resumability predicate.
pub(crate) const SEAT_UNAVAILABLE_REASON: &str = "manager_seat_recovery_unavailable";
/// A V1 appointment without the separate V2 policy opt-in.
pub(crate) const SEAT_NO_POLICY_REASON: &str = "manager_seat_policy_absent";
pub(crate) const SEAT_TIP_CHANGED: &str = "manager_seat_tip_changed";
const MAX_BACKOFF_SECONDS: i64 = 86_400;

/// Everything the pure classifier needs; gathered under one transaction.
#[derive(Debug, Clone)]
pub(crate) struct ManagerSeatObservationV1 {
    /// Tip status is `Failed` and no live process owns it.
    pub tip_failed: bool,
    /// A claimed attempt is still `running` for this tip.
    pub in_flight: bool,
    /// An `uncertain` attempt has no provider output after it.
    pub unconfirmed: bool,
    /// Typed refusal from the persisted policy/runtime gates, if any.
    pub bounds: std::result::Result<(), String>,
    pub attempts: u16,
    pub max_attempts: u16,
    pub retry_delay_seconds: u32,
    /// Start of the current down episode.
    pub down_since: DateTime<Utc>,
    /// Latest instant output must follow to prove the seat live again.
    pub evidence_after: Option<DateTime<Utc>>,
    pub last_output: Option<DateTime<Utc>>,
    /// Stored condition for this same tip, if any.
    pub previous: Option<ManagerSeatConditionV1>,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManagerSeatVerdictV1 {
    Live,
    /// Not failed, but no positive evidence yet: keep the stored record.
    Retain,
    Down {
        reason: String,
    },
    Recovering {
        attempt: u16,
        not_before: DateTime<Utc>,
        due: bool,
    },
    Exhausted,
}

/// `retry_delay_seconds * 2^(n-1)` for attempt `n`, capped at one day.
pub(crate) fn seat_backoff(retry_delay_seconds: u32, attempt: u16) -> Duration {
    let exponent = u32::from(attempt.saturating_sub(1)).min(20);
    Duration::seconds(
        i64::from(retry_delay_seconds)
            .saturating_mul(1_i64 << exponent)
            .min(MAX_BACKOFF_SECONDS),
    )
}

/// Pure seat classification. Live only on positive evidence; exhaustion is
/// terminal for the tip; a pending attempt is due after its backoff.
pub(crate) fn classify_manager_seat(o: &ManagerSeatObservationV1) -> ManagerSeatVerdictV1 {
    if o.in_flight {
        return ManagerSeatVerdictV1::Retain;
    }
    if !o.tip_failed {
        return match o.previous {
            None | Some(ManagerSeatConditionV1::Live) => ManagerSeatVerdictV1::Live,
            Some(_)
                if o.last_output
                    .is_some_and(|at| o.evidence_after.is_none_or(|after| at > after)) =>
            {
                ManagerSeatVerdictV1::Live
            }
            Some(_) => ManagerSeatVerdictV1::Retain,
        };
    }
    if o.unconfirmed {
        return ManagerSeatVerdictV1::Down {
            reason: SEAT_UNCONFIRMED_REASON.into(),
        };
    }
    if o.max_attempts > 0 && o.attempts >= o.max_attempts {
        return ManagerSeatVerdictV1::Exhausted;
    }
    if let Err(reason) = &o.bounds {
        return ManagerSeatVerdictV1::Down {
            reason: reason.clone(),
        };
    }
    let attempt = o.attempts.saturating_add(1);
    let not_before = o.down_since + seat_backoff(o.retry_delay_seconds, attempt);
    ManagerSeatVerdictV1::Recovering {
        attempt,
        not_before,
        due: o.now >= not_before,
    }
}

fn next_action(reason: &str) -> String {
    match reason {
        SEAT_EXHAUSTED_REASON => "Automatic seat recovery is exhausted for this manager session. Inspect its last error, then resume it as operator, appoint a new manager, or succeed the seat.".into(),
        SEAT_DISABLED_REASON => "Resume the manager as operator, or grant Execute with max_recovery_attempts > 0 for bounded automatic seat recovery.".into(),
        SEAT_UNCONFIRMED_REASON => "A recovery attempt lost its owner (daemon restart) and is unconfirmed. Inspect the manager, then resume it explicitly.".into(),
        SEAT_UNAVAILABLE_REASON => "This manager session cannot be resumed in place. The operator (or an authorized manager) must retry or replace it: appoint or succeed a new manager session.".into(),
        SEAT_NO_POLICY_REASON => "Resume the manager as operator. Automatic seat recovery requires the separate V2 manager policy (Execute, max_recovery_attempts > 0).".into(),
        _ => format!("Automatic seat recovery is withheld ({reason}). Resolve it or resume the manager as operator."),
    }
}

/// One exact claimed attempt the session half must execute and settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagerSeatClaimV1 {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub tip_session_id: Uuid,
    pub attempt: u16,
    pub max_attempts: u16,
    pub boot_id: Uuid,
}

/// Result of one store pass. `notice` is `(level, message)` for the
/// operator `SystemMessage` on a seat state change.
#[derive(Debug, Default)]
pub(crate) struct ManagerSeatPassV1 {
    pub changed: usize,
    pub notice: Option<(String, String)>,
    pub claim: Option<ManagerSeatClaimV1>,
}

struct SeatAttempts {
    counted: u16,
    in_flight: bool,
    last_attempt_at: Option<DateTime<Utc>>,
    last_uncertain_at: Option<DateTime<Utc>>,
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Typed continuation-boundary refusals (no provider effect happened).
fn is_seat_refusal(error: &str) -> bool {
    error.starts_with("manager_seat_") || error.starts_with("manager_v2_")
}

fn gate_code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => format!("manager_seat_gate_unavailable: {other}"),
    }
}

impl Store {
    /// The durable seat record, or `None` when the seat was never observed
    /// down. A malformed record never breaks mail or inspection.
    pub(crate) fn manager_seat_state(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<Option<ManagerSeatStateV1>> {
        Ok(self
            .manager_v2_record(config, SEAT_RECORD_KIND, SEAT_RECORD_KEY)?
            .and_then(|record| serde_json::from_value(record.payload).ok()))
    }

    fn manager_seat_attempts(
        &self,
        config: &HarnessManagerConfigV1,
        tip: Uuid,
    ) -> Result<SeatAttempts> {
        let mut stmt = self.conn.prepare(
            "SELECT state,created_at,updated_at FROM harness_manager_v2_operations
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND kind=?4 AND target_session_id=?5 ORDER BY created_at,id LIMIT 257",
        )?;
        let rows = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    SEAT_OPERATION_KIND,
                    tip.to_string()
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = SeatAttempts {
            counted: 0,
            in_flight: false,
            last_attempt_at: None,
            last_uncertain_at: None,
        };
        for (state, created, updated) in rows {
            let created = crate::store::parse_timestamp(&created)
                .map_err(|_| DaemonError::Store("manager seat attempt timestamp".into()))?;
            let updated = crate::store::parse_timestamp(&updated)
                .map_err(|_| DaemonError::Store("manager seat attempt timestamp".into()))?;
            out.in_flight |= matches!(state.as_str(), "queued" | "running");
            // Every claimed attempt is charged, including one refused at the
            // continuation boundary (busy tip, pause, rotation). The next claim
            // therefore takes a fresh `seat:<tip>:<n+1>` key and the budget
            // exhausts deterministically.
            out.counted = out.counted.saturating_add(1);
            out.last_attempt_at = out.last_attempt_at.max(Some(created));
            if state == "uncertain" {
                out.last_uncertain_at = out.last_uncertain_at.max(Some(updated));
            }
        }
        Ok(out)
    }

    fn manager_seat_bounds(
        &self,
        config: &HarnessManagerConfigV1,
        grant: Option<&HarnessManagerPolicyConfigV2>,
        tip: &Session,
        retry_enabled: bool,
    ) -> Result<std::result::Result<(), String>> {
        let Some(grant) = grant else {
            return Ok(Err(SEAT_NO_POLICY_REASON.into()));
        };
        let policy = &grant.policy;
        let refusal = if grant.revoked {
            Some("manager_v2_policy_revoked".to_string())
        } else if policy.max_recovery_attempts == 0 {
            Some(SEAT_DISABLED_REASON.into())
        } else if policy.mode != ManagerOperatingModeV2::Execute {
            Some("manager_seat_recovery_requires_execute".into())
        } else if policy.paused {
            Some("manager_v2_policy_paused".into())
        } else if !retry_enabled {
            Some("manager_v2_retry_disabled".into())
        } else if !crate::session::lifecycle::manager_lead_provider_resumable(tip) {
            // Never claim a tip whose continuation would allocate a new row.
            Some(SEAT_UNAVAILABLE_REASON.into())
        } else if super::manager_v2_spend_blocked(
            policy,
            &self.manager_v2_resource_snapshot(config)?,
        ) {
            Some("manager_v2_spend_hold".into())
        } else if let Err(error) = self.manager_action_human_gate(tip.id) {
            Some(gate_code(error))
        } else if let Err(error) =
            self.manager_v2_resource_gate(config, None, tip.provider, Some(tip.id))
        {
            Some(gate_code(error))
        } else {
            None
        };
        Ok(refusal.map_or(Ok(()), Err))
    }

    /// Mark attempts whose execution owner is gone as `uncertain`. With a
    /// boot id, only claims of other boots; without, every running claim
    /// (the caller owns the coordinator single-flight guard).
    pub(crate) fn recover_manager_seat_claims(&self, boot_id: Option<Uuid>) -> Result<usize> {
        let at = stamp(Utc::now());
        Ok(self.conn.execute(
            "UPDATE harness_manager_v2_operations
             SET state='uncertain',row_version=row_version+1,updated_at=?3,
                 outcome_json=json_object('outcome','execution_owner_lost_unconfirmed')
             WHERE kind=?1 AND state IN ('queued','running')
               AND (?2 IS NULL OR claim_boot_id IS NOT ?2)",
            params![SEAT_OPERATION_KIND, boot_id.map(|id| id.to_string()), at],
        )?)
    }

    /// One bounded seat pass for `project`. `tip_active` is the runtime
    /// active-map membership of the current tip, read by the caller.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn reconcile_manager_seat(
        &self,
        project: Uuid,
        tip_active: impl FnOnce(Uuid) -> bool,
        retry_enabled: bool,
        boot_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<ManagerSeatPassV1> {
        let mut pass = ManagerSeatPassV1::default();
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(pass);
        };
        let Some(tip) = config.current_session_id else {
            return Ok(pass);
        };
        // A V1 appointment without the V2 opt-in is still observed and
        // signalled; its automatic recovery stays disabled.
        let grant = self.get_harness_manager_policy(project)?;
        let (max_attempts, retry_delay) = grant.as_ref().map_or((0, 60), |g| {
            (g.policy.max_recovery_attempts, g.policy.retry_delay_seconds)
        });
        let active = tip_active(tip);
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(session) = self.get_session(tip)? else {
            return Ok(pass);
        };
        let stored = self.manager_seat_state(&config)?;
        let previous = stored.as_ref().filter(|s| s.tip_session_id == tip);
        let attempts = self.manager_seat_attempts(&config, tip)?;
        let last_output = self.last_provider_output_at(tip)?;
        let tip_failed = session.status == SessionStatus::Failed && !active;
        let bounds = if tip_failed {
            self.manager_seat_bounds(&config, grant.as_ref(), &session, retry_enabled)?
        } else {
            Ok(())
        };
        // A new down episode starts when the tip fails after the last attempt.
        let down_since = match previous {
            Some(p)
                if p.is_down()
                    && attempts
                        .last_attempt_at
                        .is_none_or(|attempt| p.since >= attempt) =>
            {
                p.since
            }
            _ => now,
        };
        let observation = ManagerSeatObservationV1 {
            tip_failed,
            in_flight: attempts.in_flight,
            unconfirmed: attempts
                .last_uncertain_at
                .is_some_and(|at| last_output.is_none_or(|output| output <= at)),
            bounds,
            attempts: attempts.counted,
            max_attempts,
            retry_delay_seconds: retry_delay,
            down_since,
            evidence_after: previous.map(|p| p.since).max(attempts.last_attempt_at),
            last_output,
            previous: previous.map(|p| p.state),
            now,
        };
        let (last_invocation_id, last_error_class, last_terminal_reason) =
            self.manager_seat_last_invocation(tip)?;
        let base = |state, reason: &str| ManagerSeatStateV1 {
            state,
            tip_session_id: tip,
            since: down_since,
            attempts: attempts.counted,
            max_attempts,
            not_before: None,
            reason: reason.to_string(),
            next_action: (state != ManagerSeatConditionV1::Live).then(|| next_action(reason)),
            last_invocation_id,
            last_error_class: last_error_class.clone(),
            last_terminal_reason: last_terminal_reason.clone(),
        };
        let next = match classify_manager_seat(&observation) {
            ManagerSeatVerdictV1::Retain => None,
            ManagerSeatVerdictV1::Live => stored
                .as_ref()
                .filter(|s| s.tip_session_id != tip || s.state != ManagerSeatConditionV1::Live)
                .map(|_| ManagerSeatStateV1 {
                    since: now,
                    ..base(ManagerSeatConditionV1::Live, "provider_output_observed")
                }),
            ManagerSeatVerdictV1::Down { reason } => {
                Some(base(ManagerSeatConditionV1::Down, &reason))
            }
            ManagerSeatVerdictV1::Exhausted => Some(base(
                ManagerSeatConditionV1::Exhausted,
                SEAT_EXHAUSTED_REASON,
            )),
            ManagerSeatVerdictV1::Recovering {
                attempt,
                not_before,
                due,
            } => {
                let mut state = ManagerSeatStateV1 {
                    not_before: Some(not_before),
                    next_action: Some(format!(
                        "rsid resumes the manager in place (attempt {attempt}/{max_attempts}) at or after {}.",
                        stamp(not_before)
                    )),
                    ..base(
                        ManagerSeatConditionV1::Recovering,
                        "manager_seat_recovery_scheduled",
                    )
                };
                if due
                    && let Some(grant) = grant.as_ref()
                    && let Some(claim) = self.manager_seat_claim(
                        &config, grant, tip, attempt, not_before, boot_id, now,
                    )?
                {
                    state.attempts = attempt;
                    state.reason = "manager_seat_recovery_in_flight".into();
                    state.next_action = Some(format!(
                        "rsid is resuming the manager in place (attempt {attempt}/{max_attempts}); the seat clears on provider output."
                    ));
                    pass.claim = Some(claim);
                }
                Some(state)
            }
        };
        if let Some(next) = next {
            let value = serde_json::to_value(&next)?;
            if self.manager_v2_record_changed(
                &config,
                SEAT_RECORD_KIND,
                SEAT_RECORD_KEY,
                None,
                &value,
            )? {
                pass.changed += 1;
            }
            let transition = stored.as_ref().is_none_or(|old| {
                (old.state, old.attempts, &old.reason, old.tip_session_id)
                    != (next.state, next.attempts, &next.reason, next.tip_session_id)
            });
            if transition {
                pass.notice = Some(seat_notice(project, &next));
            }
        }
        tx.commit()?;
        Ok(pass)
    }

    /// Last invocation, its error class and the tip's terminal reason. A
    /// Claude resume that aborts before streaming (`aborted_streaming`, no
    /// provider output) is the transient case bounded recovery retries.
    fn manager_seat_last_invocation(
        &self,
        tip: Uuid,
    ) -> Result<(Option<Uuid>, Option<String>, Option<String>)> {
        let row: (Option<String>, Option<String>, Option<String>) = self.conn.query_row(
            "SELECT s.model_invocation_id,m.error_class,s.terminal_reason FROM sessions s
             LEFT JOIN model_invocations m ON m.id=s.model_invocation_id WHERE s.id=?1",
            [tip.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok((row.0.and_then(|id| Uuid::parse_str(&id).ok()), row.1, row.2))
    }

    #[allow(clippy::too_many_arguments)]
    fn manager_seat_claim(
        &self,
        config: &HarnessManagerConfigV1,
        grant: &HarnessManagerPolicyConfigV2,
        tip: Uuid,
        attempt: u16,
        not_before: DateTime<Utc>,
        boot_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<ManagerSeatClaimV1>> {
        let id = Uuid::new_v4();
        let payload = json!({"origin":"daemon_seat_recovery","tip_session_id":tip,
            "attempt":attempt,"max_attempts":grant.policy.max_recovery_attempts});
        let at = stamp(now);
        let inserted = self.conn.execute(
            "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,scope_version,
                policy_version,actor_session_id,idempotency_key,fingerprint,kind,payload_json,state,
                row_version,target_session_id,not_before,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,NULL,?6,?7,?8,?9,'queued',1,?10,?11,?12,?12)
             ON CONFLICT(project_id,manager_session_id,scope_version,idempotency_key) DO NOTHING",
            params![
                id.to_string(),
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                grant.row_version,
                format!("seat:{tip}:{attempt}"),
                fingerprint(&payload)?,
                SEAT_OPERATION_KIND,
                serde_json::to_string(&payload)?,
                tip.to_string(),
                stamp(not_before),
                at
            ],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        let claimed = self.conn.execute(
            "UPDATE harness_manager_v2_operations SET state='running',row_version=2,attempts=1,
                claim_boot_id=?2,updated_at=?3 WHERE id=?1 AND state='queued'",
            params![id.to_string(), boot_id.to_string(), at],
        )?;
        Ok((claimed == 1).then_some(ManagerSeatClaimV1 {
            operation_id: id,
            project_id: config.project_id,
            tip_session_id: tip,
            attempt,
            max_attempts: grant.policy.max_recovery_attempts,
            boot_id,
        }))
    }

    /// Execution-time recheck at the continuation boundary, called under the
    /// session's spawn guard before any provider effect. The claim must still
    /// be this boot's running attempt, the claimed tip must still be the exact
    /// current lineage tip of the same scope, the tip must still be `Failed`,
    /// and every recovery bound must still hold (policy present, not revoked
    /// or paused, recovery allowed, no human gate, resumable provider,
    /// resources). Any change yields a typed refusal and no launch.
    pub(crate) fn manager_seat_effect_gate(
        &self,
        claim: &ManagerSeatClaimV1,
        retry_enabled: bool,
    ) -> Result<()> {
        let owned: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT manager_session_id,scope_version FROM harness_manager_v2_operations
                 WHERE id=?1 AND kind=?2 AND state='running' AND claim_boot_id=?3",
                params![
                    claim.operation_id.to_string(),
                    SEAT_OPERATION_KIND,
                    claim.boot_id.to_string()
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((anchor, scope)) = owned else {
            return Err(refused("manager_seat_claim_lost"));
        };
        let config = self
            .get_harness_manager(claim.project_id)?
            .filter(|c| c.manager_session_id.to_string() == anchor && c.row_version == scope)
            .ok_or_else(|| refused("manager_seat_scope_changed"))?;
        if config.current_session_id != Some(claim.tip_session_id) {
            return Err(refused(SEAT_TIP_CHANGED));
        }
        let session = self
            .get_session(claim.tip_session_id)?
            .ok_or_else(|| refused(SEAT_TIP_CHANGED))?;
        // Bounds first, so a pause, revocation or human question is reported
        // by its own typed reason even when it also changed the tip status.
        let grant = self.get_harness_manager_policy(claim.project_id)?;
        self.manager_seat_bounds(&config, grant.as_ref(), &session, retry_enabled)?
            .map_err(|code| refused(&code))?;
        if session.status != SessionStatus::Failed {
            return Err(refused("manager_seat_tip_not_failed"));
        }
        Ok(())
    }

    /// One keyset page of V1-appointed projects without a V2 policy row,
    /// after `after`. The V2 coordinator traversal never visits them; the
    /// caller wraps the cursor, so every such seat is observed within
    /// `ceil(n / limit)` passes.
    pub(crate) fn manager_seat_v1_projects_after(
        &self,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.project_id FROM harness_manager_scopes s
             WHERE s.project_id > ?1
               AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_policies p
                              WHERE p.project_id=s.project_id)
             ORDER BY s.project_id LIMIT ?2",
        )?;
        let ids = stmt
            .query_map(
                params![after.map(|id| id.to_string()).unwrap_or_default(), limit],
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids
            .into_iter()
            .filter_map(|id| Uuid::parse_str(&id).ok())
            .collect())
    }

    /// Settle an exact claim after its executor returned. Only the owning
    /// boot's running claim moves; a recovered (uncertain) row is retained.
    pub(crate) fn finish_manager_seat_claim(
        &self,
        claim: &ManagerSeatClaimV1,
        outcome: std::result::Result<(), &str>,
    ) -> Result<bool> {
        let (state, detail) = match &outcome {
            Ok(()) => ("succeeded", "continuation_established".to_string()),
            Err(error) if error.contains("is still active") => {
                ("blocked", SEAT_BUSY_OUTCOME.to_string())
            }
            // A typed refusal at the continuation boundary: no provider effect.
            Err(error) if is_seat_refusal(error) => ("blocked", (*error).to_string()),
            Err(error) => ("failed", error.chars().take(512).collect()),
        };
        Ok(self.conn.execute(
            "UPDATE harness_manager_v2_operations
             SET state=?3,row_version=row_version+1,updated_at=?4,
                 outcome_json=json_object('outcome',?5)
             WHERE id=?1 AND state='running' AND claim_boot_id=?2",
            params![
                claim.operation_id.to_string(),
                claim.boot_id.to_string(),
                state,
                stamp(Utc::now()),
                detail
            ],
        )? == 1)
    }
}

/// Operator notice: `error` while the seat is not live, `info` on recovery.
pub(crate) fn seat_notice(project: Uuid, seat: &ManagerSeatStateV1) -> (String, String) {
    let tip = seat.tip_session_id;
    let level = if seat.state == ManagerSeatConditionV1::Live {
        "info"
    } else {
        "error"
    };
    let body = match seat.state {
        ManagerSeatConditionV1::Live => {
            format!("seat recovered: manager {tip} produced provider output.")
        }
        ManagerSeatConditionV1::Recovering if seat.reason == "manager_seat_recovery_in_flight" => {
            format!(
                "seat down: resuming Failed manager {tip} in place (attempt {}/{}).",
                seat.attempts, seat.max_attempts
            )
        }
        ManagerSeatConditionV1::Recovering => format!(
            "seat down: manager {tip} Failed; in-place recovery attempt {}/{} scheduled at {}.",
            seat.attempts.saturating_add(1),
            seat.max_attempts,
            seat.not_before.map_or_else(String::new, stamp)
        ),
        ManagerSeatConditionV1::Down => format!(
            "seat down: manager {tip} Failed ({}). {}",
            seat.reason,
            seat.next_action.as_deref().unwrap_or_default()
        ),
        ManagerSeatConditionV1::Exhausted => format!(
            "seat recovery exhausted after {} attempts for manager {tip} ({}). {}",
            seat.attempts,
            seat.reason,
            seat.next_action.as_deref().unwrap_or_default()
        ),
    };
    (
        level.into(),
        format!("{SEAT_MESSAGE_PREFIX} Harness manager {project}: {body}"),
    )
}

#[cfg(test)]
#[path = "manager_seat_tests.rs"]
mod tests;
