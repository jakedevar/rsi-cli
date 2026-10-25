//! Runtime adapter for the durable manager intent and answer journals.

use std::sync::atomic::Ordering;

use rsi_common::harness_manager_v2::{
    AnswerHarnessManagerDecisionRequestV2, ManagerMutationReceiptV2,
};
use serde_json::Value;
use uuid::Uuid;

use super::SessionManager;

/// A periodic pass that keeps the shared Store this long is logged (#3021).
const SLOW_STORE_HOLD_MS: u64 = 100;
/// #3486: a project whose last intent pass completed this recently skips a pass
/// whose off-lock observation was rejected (an input changed while it ran)
/// instead of re-reading everything under the shared Store lock; the next tick
/// observes again. Past the window the pass reads under the lock, so a project
/// on a store that never goes quiet is still reconciled.
const INTENT_DEFER_WINDOW_SECS: i64 = 45;
use crate::bus::DaemonEvent;
use crate::error::Result;
use crate::store::harness_manager_v2::refused;
use crate::store::manager_coordinator::ManagerDecisionDeliveryV2;

/// Policy-less V1 manager seats observed per coordinator pass (#669).
pub(crate) const MANAGER_SEAT_V1_PAGE: usize = 32;

impl SessionManager {
    /// Only the operator RPC calls this adapter. Admission records the exact
    /// answer and its delivery obligation; it never clears a question early.
    pub async fn answer_harness_manager_decision(
        &self,
        request: AnswerHarnessManagerDecisionRequestV2,
    ) -> Result<ManagerMutationReceiptV2> {
        let receipt = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::answer_harness_manager_decision",
            )
            .await;
            store.manager_v2_prepare_decision_answer(&request, |config, key, version, digest| {
                store.manager_v2_apply_operator_acceptance(config, key, version, digest)
            })?
        };
        self.event_bus.publish(DaemonEvent::SystemMessage {
            level: "info".into(),
            message: format!(
                "Harness manager decision {}: operator answer recorded; inspect delivery state.",
                request.decision_key
            ),
        });
        Ok(receipt)
    }

    pub async fn reconcile_harness_managers_startup(&self) -> Result<usize> {
        let recovered = crate::store_lock::lock(
            &self.store,
            "manager_coordinator::reconcile_harness_managers_startup",
        )
        .await
        .manager_v2_recover_decision_deliveries(self.program_run_boot_id)?;
        let actions = self.reconcile_manager_actions_startup().await?;
        // A seat attempt claimed by an earlier boot has no owner now. It is
        // made uncertain, never relaunched (#669).
        let seats = crate::store_lock::lock(
            &self.store,
            "manager_coordinator::reconcile_harness_managers_startup",
        )
        .await
        .recover_manager_seat_claims(Some(self.program_run_boot_id))?;
        Ok(recovered + actions + seats + self.reconcile_harness_managers_once().await?)
    }

    /// #2753: refresh the stale CLI login probes in the background (one probe
    /// in flight per provider) and record one deduped notice per project for
    /// each provider that went from `logged_in` to `logged_out`.
    async fn reconcile_provider_login_notices(&self, projects: &[Uuid]) -> usize {
        self.provider_login.spawn_refresh_stale();
        let mut changed = 0;
        for (provider, transitioned_at) in self.provider_login.logout_transitions() {
            for project in projects {
                match crate::store_lock::lock(
                    &self.store,
                    "manager_coordinator::reconcile_provider_login_notices",
                )
                .await
                .record_provider_login_required_notice(
                    *project,
                    provider,
                    transitioned_at,
                ) {
                    Ok(Some(_)) => changed += 1,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(project_id=%project,error=%error,"provider login notice deferred");
                    }
                }
            }
        }
        changed
    }

    /// #2755: re-run the provider CLI contract canary for any CLI whose version
    /// changed since its last result (a stat of each binary; nothing runs
    /// otherwise), and record one deduped notice per project for each provider
    /// that is degraded, per episode.
    async fn reconcile_provider_contract_notices(&self, projects: &[Uuid]) -> usize {
        self.spawn_provider_contract_check();
        let mut changed = 0;
        for (provider, reason, since, detail) in self.provider_contract.degraded_episodes() {
            for project in projects {
                match crate::store_lock::lock(
                    &self.store,
                    "manager_coordinator::reconcile_provider_contract_notices",
                )
                .await
                .record_provider_contract_degraded_notice(
                    *project,
                    provider,
                    &reason,
                    detail.as_deref(),
                    since,
                ) {
                    Ok(Some(_)) => changed += 1,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(project_id=%project,error=%error,"provider contract notice deferred");
                    }
                }
            }
        }
        changed
    }

    /// #3154: one project-wide notice per OpenRouter spend line crossed (the
    /// warn line, the stop line), deduped per arming across restarts by the
    /// notice subject and the arming time. Reads the last probe the daemon's
    /// spend loop keeps fresh; a cap that is off, or a state below the warn
    /// line, notifies nothing.
    async fn reconcile_openrouter_spend_notices(&self, projects: &[Uuid]) -> usize {
        let enforcer = crate::openrouter_spend::SpendEnforcer::production(
            std::sync::Arc::clone(&self.runtime_config),
            std::sync::Arc::clone(&self.store),
        );
        let cap = enforcer.state(false).await;
        let kinds: &[&str] = match cap.state {
            rsi_common::openrouter_spend::SpendCapState::Warn => &["warn"],
            // The stop line is its own, stronger notice; a jump straight past
            // the warn line sends it alone.
            rsi_common::openrouter_spend::SpendCapState::Reached => &["reached"],
            _ => return 0,
        };
        let Some(armed_at) = cap.armed_at else {
            return 0;
        };
        let detail = rsi_common::openrouter_spend::summary_line(&cap);
        let mut changed = 0;
        for kind in kinds {
            for project in projects {
                match crate::store_lock::lock(
                    &self.store,
                    "manager_coordinator::reconcile_openrouter_spend_notices",
                )
                .await
                .record_openrouter_spend_notice(*project, kind, armed_at, &detail)
                {
                    Ok(Some(_)) => changed += 1,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(project_id=%project,error=%error,"OpenRouter spend notice deferred");
                    }
                }
            }
        }
        changed
    }

    /// One cursor/guard serializes startup, event hints and periodic backstop.
    /// The cursor is only a bounded traversal hint; all ownership is durable.
    pub async fn reconcile_harness_managers_once(&self) -> Result<usize> {
        let Ok(mut cursor) = self.manager_coordinator_cursor.try_lock() else {
            return Ok(0);
        };
        // Sample cgroup files before any SQLite guard; the Store receives only
        // the completed, optional Health projection.
        let worker_pressure = crate::process_scope::worker_slice_memory_pressure().await;
        let pressure_observed_at = chrono::Utc::now();
        let projects = crate::store_lock::lock(
            &self.store,
            "manager_coordinator::reconcile_harness_managers_once",
        )
        .await
        .manager_v2_projects_after(*cursor, 8)?;
        let closure_changes = self.retry_appserver_approval_closures().await?;
        let mut changed = closure_changes + {
            let live = super::pending_approvals::live_incarnations();
            let store = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::reconcile_harness_managers_once",
            )
            .await;
            // Synchronous under the guard already held: no extra await state.
            sweep_superseded_resume_wakes(&store);
            store.expire_appserver_approvals(&live)?
                + store.manager_v2_recover_decision_deliveries(self.program_run_boot_id)?
                + store.recover_manager_actions_startup(self.program_run_boot_id)?
        };
        match crate::store_lock::lock(
            &self.store,
            "manager_coordinator::reconcile_harness_managers_once",
        )
        .await
        .recover_remote_answers(self.program_run_boot_id)
        {
            Ok(recovered) => changed += recovered,
            Err(error) => tracing::warn!(%error, "remote answer recovery deferred"),
        }
        match self.reconcile_remote_answers_once().await {
            Ok(delivered) => changed += delivered,
            Err(error) => tracing::warn!(%error, "remote answer delivery deferred"),
        }
        let auth_episodes: Vec<(
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        )> = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::reconcile_harness_managers_once",
            )
            .await;
            match store.provider_launch_stats(pressure_observed_at) {
                Ok(stats) => stats
                    .into_iter()
                    .filter_map(|(provider, stats)| {
                        Some((
                            provider,
                            stats.auth_episode_started_at?,
                            stats.last_auth_failure_at?,
                        ))
                    })
                    .collect(),
                Err(error) => {
                    tracing::warn!(error=%error,"provider auth episode scan deferred");
                    Vec::new()
                }
            }
        };
        // #2735: models the provider withdrew; one deduped notice per episode.
        let unavailable_models = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::reconcile_harness_managers_once",
            )
            .await;
            match store.models_unavailable(pressure_observed_at) {
                Ok(models) => models,
                Err(error) => {
                    tracing::warn!(error=%error,"unavailable model scan deferred");
                    Vec::new()
                }
            }
        };
        // #2753: keep the CLI login probes fresh and notify the managers of a
        // login loss (boxed: the coordinator future must not grow).
        changed += Box::pin(self.reconcile_provider_login_notices(&projects)).await;
        changed += Box::pin(self.reconcile_provider_contract_notices(&projects)).await;
        changed += Box::pin(self.reconcile_openrouter_spend_notices(&projects)).await;
        for project in &projects {
            for episode in &unavailable_models {
                match crate::store_lock::lock(
                    &self.store,
                    "manager_coordinator::reconcile_harness_managers_once",
                )
                .await
                .record_model_unavailable_notice(*project, episode)
                {
                    Ok(Some(_)) => changed += 1,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(project_id=%project,error=%error,"model unavailable notice deferred");
                    }
                }
            }
            for (provider, started, last) in &auth_episodes {
                match crate::store_lock::lock(
                    &self.store,
                    "manager_coordinator::reconcile_harness_managers_once",
                )
                .await
                .record_provider_auth_invalid_notice(*project, provider, *started, *last)
                {
                    Ok(Some(_)) => changed += 1,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(project_id=%project,error=%error,"provider auth notice deferred");
                    }
                }
            }
            if let Some(pressure) = worker_pressure.as_ref() {
                match crate::store_lock::lock(
                    &self.store,
                    "manager_coordinator::reconcile_harness_managers_once",
                )
                .await
                .record_worker_slice_pressure_notice(
                    *project,
                    pressure,
                    pressure_observed_at,
                ) {
                    Ok(Some(_)) => changed += 1,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(project_id=%project,error=%error,"worker slice pressure notice deferred");
                    }
                }
            }
            let reconciled = Box::pin(self.reconcile_manager_intent_off_lock(*project)).await;
            match reconciled {
                Ok(result) => {
                    changed += result.changed;
                    if result.changed > 0 {
                        self.event_bus.publish(DaemonEvent::SystemMessage {level:if result.blocked>0 {"warn"} else {"info"}.into(),message:format!("Harness manager {project}: {} unfinished deliverables, {} blocked Epics, {} recovery actions admitted. Read the manager board for evidence and next actions.",result.unfinished,result.blocked,result.queued)});
                    }
                    crate::store_lock::lock(
                        &self.store,
                        "manager_coordinator::reconcile_harness_managers_once",
                    )
                    .await
                    .manager_v2_coordinator_error(*project, None)?;
                }
                Err(error) => {
                    // One malformed/oversized project must not starve others.
                    tracing::warn!(project_id=%project,error=%error,"manager intent reconciliation deferred");
                    let new_evidence = crate::store_lock::lock(
                        &self.store,
                        "manager_coordinator::reconcile_harness_managers_once",
                    )
                    .await
                    .manager_v2_coordinator_error(*project, Some(&error.to_string()))?;
                    if new_evidence {
                        self.event_bus.publish(DaemonEvent::SystemMessage {
                            level: "warn".into(),
                            message: format!("Harness manager {project} needs attention: {error}"),
                        });
                    }
                }
            }
            // Boxed: the seat executor must not grow the coordinator future.
            changed += Box::pin(self.reconcile_manager_seat(*project)).await;
            let notice_config = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::reconcile_harness_managers_once",
            )
            .await
            .get_harness_manager_notice_config(*project)?;
            if let Some(config) = notice_config {
                if let Err(error) = crate::store_lock::lock(
                    &self.store,
                    "manager_coordinator::manager_v2_reconcile_notices",
                )
                .await
                .manager_v2_reconcile_notices(&config)
                {
                    tracing::warn!(project_id=%project,error=%error,"manager notice deferred; durable board retains obligation");
                }
            }
            let config = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::reconcile_harness_managers_once",
            )
            .await
            .get_harness_manager(*project)?;
            if let Some(config) = config {
                for _ in 0..4 {
                    let delivery = crate::store_lock::lock(
                        &self.store,
                        "manager_coordinator::reconcile_harness_managers_once",
                    )
                    .await
                    .manager_v2_claim_decision_delivery(&config, self.program_run_boot_id)?;
                    let Some(delivery) = delivery else { break };
                    let outcome = self.deliver_manager_decision(delivery.clone()).await;
                    let store = crate::store_lock::lock(
                        &self.store,
                        "manager_coordinator::reconcile_harness_managers_once",
                    )
                    .await;
                    let Some(record) =
                        store.manager_v2_record(&config, "decision_delivery", &delivery.key)?
                    else {
                        continue;
                    };
                    let current: ManagerDecisionDeliveryV2 =
                        serde_json::from_value(record.payload)?;
                    if current.state != "running" {
                        changed += 1;
                        continue;
                    }
                    match outcome {
                        Ok(()) => {
                            store.manager_v2_set_decision_delivery(&current,"delivered",true,Some("provider continuation established for the exact operator answer".into()))?;
                        }
                        Err(error) => {
                            let state = if current.effect_started {
                                "uncertain"
                            } else {
                                "blocked"
                            };
                            store.manager_v2_set_decision_delivery(
                                &current,
                                state,
                                current.effect_started,
                                Some(error.to_string()),
                            )?;
                        }
                    }
                    changed += 1;
                }
            }
        }
        // #669: V1 appointments without the V2 opt-in are outside the V2
        // traversal above; still observe and signal their seat (recovery
        // stays disabled without a policy). One page per pass behind a
        // wrapping cursor: every such seat is observed within
        // ceil(n / MANAGER_SEAT_V1_PAGE) passes.
        {
            let mut v1_cursor = self.manager_seat_v1_cursor.lock().await;
            let v1_projects = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::reconcile_harness_managers_once",
            )
            .await
            .manager_seat_v1_projects_after(*v1_cursor, MANAGER_SEAT_V1_PAGE)?;
            *v1_cursor = if v1_projects.len() < MANAGER_SEAT_V1_PAGE {
                None
            } else {
                v1_projects.last().copied()
            };
            drop(v1_cursor);
            for project in v1_projects {
                changed += Box::pin(self.reconcile_manager_seat(project)).await;
            }
        }
        *cursor = if projects.len() < 8 {
            None
        } else {
            projects.last().copied()
        };
        // Keep prompt action progress on successful coordinator passes. The
        // daemon's independent action backstop uses this same single-flight
        // reconciler when a seat or notice pass is parked (#720).
        changed += self.reconcile_manager_actions_once().await?;
        Ok(changed)
    }

    /// #2939: one project's intent pass without holding the shared Store lock
    /// for its reads. Under the lock: reuse a memo whose inputs are unchanged,
    /// else pin the input version. Off the lock: the read-only observation on
    /// the coordinator's own connection. Under the lock again: the pass, which
    /// uses the observation only if no input changed since the pin and reads
    /// again otherwise. The hub's rsi project held the lock ~0.5 s every 10 s.
    async fn reconcile_manager_intent_off_lock(
        &self,
        project: Uuid,
    ) -> Result<crate::store::manager_intent::ManagerIntentReconciliationV2> {
        let retry = self.runtime_config.retry_enabled.load(Ordering::Relaxed);
        let (pin, path) = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::reconcile_manager_intent_off_lock:pin",
            )
            .await;
            if store.manager_intent_memoized(project)? {
                return store.reconcile_manager_intent(project, retry);
            }
            (
                store.manager_intent_input_pin()?,
                store.database_path().map(std::path::PathBuf::from),
            )
        };
        let observed = match (pin, path) {
            (Some(pin), Some(path)) => {
                let observer = std::sync::Arc::clone(&self.manager_intent_observer);
                let observed = tokio::task::spawn_blocking(move || {
                    let mut slot = observer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let observer = match slot.as_ref() {
                        Some(observer) => observer,
                        None => slot.insert(crate::store::Store::open_read_only_observer(&path)?),
                    };
                    let observed = observer.observe_manager_intent(project, pin);
                    if observed.is_err() {
                        // Reopen on the next pass rather than reuse a broken handle.
                        *slot = None;
                    }
                    observed
                })
                .await;
                match observed {
                    Ok(Ok(observed)) => observed,
                    Ok(Err(error)) => {
                        tracing::debug!(project_id=%project,error=%error,"manager intent observation read under the Store lock instead");
                        None
                    }
                    Err(error) => {
                        tracing::warn!(project_id=%project,error=%error,"manager intent observation task failed");
                        None
                    }
                }
            }
            _ => None,
        };
        let observation_offered = observed.is_some();
        let guard = crate::store_lock::lock(
            &self.store,
            "manager_coordinator::reconcile_manager_intent_off_lock:apply",
        )
        .await;
        let held_since = std::time::Instant::now();
        let result = guard.reconcile_manager_intent_observed_with(
            project,
            retry,
            observed,
            crate::store::manager_intent::ObservationFallback::DeferWithin(
                chrono::Duration::seconds(INTENT_DEFER_WINDOW_SECS),
            ),
        );
        let held_ms = held_since.elapsed().as_millis() as u64;
        drop(guard);
        // #3021: a periodic pass that holds the shared Store for a long time
        // says whether it had to re-read what the off-lock observation covers.
        if held_ms >= SLOW_STORE_HOLD_MS {
            tracing::warn!(
                project_id = %project,
                held_ms,
                observation_offered,
                read_under_lock = result.as_ref().is_ok_and(|r| r.read_under_lock),
                deferred = result.as_ref().is_ok_and(|r| r.deferred),
                observation_rejected = result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.observation_rejected.as_deref())
                    .unwrap_or(""),
                "manager intent pass held the Store lock"
            );
        } else if let Some(reason) = result
            .as_ref()
            .ok()
            .and_then(|r| r.observation_rejected.as_deref())
        {
            tracing::debug!(project_id = %project, observation_rejected = reason, "manager intent observation rejected");
        }
        result
    }

    /// Issue #669: classify the appointed manager seat, publish a
    /// `[manager-seat]` `SystemMessage` on each state change, and execute at
    /// most one claimed in-place resume of the SAME tip row and sandbox.
    /// Caller holds the coordinator single-flight cursor. Errors are logged:
    /// one project's seat must not starve the others.
    pub(super) async fn reconcile_manager_seat(&self, project: Uuid) -> usize {
        match Box::pin(self.reconcile_manager_seat_inner(project)).await {
            Ok(changed) => changed,
            Err(error) => {
                tracing::warn!(project_id=%project,error=%error,"manager seat reconciliation deferred");
                0
            }
        }
    }

    async fn reconcile_manager_seat_inner(&self, project: Uuid) -> Result<usize> {
        let active: std::collections::HashSet<Uuid> =
            self.active.read().await.keys().copied().collect();
        let retry_enabled = self.runtime_config.retry_enabled.load(Ordering::Relaxed);
        let pass = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::reconcile_manager_seat_inner",
            )
            .await;
            // The cursor excludes every other pass, so no live executor owns
            // a running claim here; an abandoned one becomes uncertain.
            let mut changed = store.recover_manager_seat_claims(None)?;
            let pass = store.reconcile_manager_seat(
                project,
                |tip| active.contains(&tip),
                retry_enabled,
                self.program_run_boot_id,
                chrono::Utc::now(),
            )?;
            drop(store);
            changed += pass.changed;
            (pass, changed)
        };
        let (pass, mut changed) = pass;
        if let Some((level, message)) = pass.notice {
            self.event_bus
                .publish(DaemonEvent::SystemMessage { level, message });
        }
        if let Some(seat) = pass.seat_report.as_ref() {
            // hub #3859: tell the hub once that this satellite's seat is down.
            // Informational and at most once, so a refusal is only logged.
            let queued = crate::store_lock::lock(
                &self.store,
                "manager_coordinator::queue_manager_seat_report",
            )
            .await
            .queue_manager_seat_report(seat, chrono::Utc::now());
            if let Err(error) = queued {
                tracing::warn!(project_id=%project,error=%error,"manager seat report to the hub not queued");
            }
        }
        let Some(claim) = pass.claim else {
            return Ok(changed);
        };
        changed += Box::pin(self.execute_manager_seat_claim(project, claim)).await?;
        Ok(changed)
    }

    /// #1102: tell the appointed manager its saved policy was revoked by an
    /// operator scope action (re-appoint, scope edit or clear). Best effort:
    /// delivery waits out a busy turn (the continuation fence refuses a busy
    /// tip) and gives up after about ten minutes.
    pub(crate) fn notify_manager_policy_revoked(
        self: &std::sync::Arc<Self>,
        project: Uuid,
        reason: &'static str,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            let message = rsi_common::daemon_message::wrap(
                "manager-policy",
                &format!(
                    "Your manager policy was revoked by an operator action ({reason}). Until the operator re-saves it you cannot launch workers, touch Issues or deploy. Ask the operator to re-save the policy (`:manager policy`); `AgentManagerInspect {{}}` shows policy.revoked."
                ),
            );
            for _ in 0..20 {
                let tip = match crate::store_lock::lock(
                    &manager.store,
                    "manager_coordinator::notify_manager_policy_revoked",
                )
                .await
                .get_harness_manager_notice_config(project)
                {
                    Ok(Some(config)) => config.current_session_id,
                    _ => None,
                };
                let Some(tip) = tip else {
                    return;
                };
                let sent = async {
                    let fence = manager
                        .capture_exact_continuation_fence(
                            tip,
                            crate::store::manager_actions::fence::ContinuationAuthorityV1::Automated,
                        )
                        .await?;
                    manager.continue_fenced(tip, message.clone(), fence).await
                }
                .await;
                if sent.is_ok() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    /// Execute one exact seat claim through the gated `ManagerSeat`
    /// continuation and settle it. Refusals at the boundary are typed
    /// non-success outcomes; only an established continuation succeeds.
    pub(crate) async fn execute_manager_seat_claim(
        &self,
        project: Uuid,
        claim: crate::store::manager_intent::manager_seat::ManagerSeatClaimV1,
    ) -> Result<usize> {
        let prompt = rsi_common::daemon_message::wrap(
            "manager-seat",
            &format!(
                "Your previous turn ended Failed, so rsid resumed this appointed manager seat in place (automatic recovery attempt {}/{} under the operator's persisted policy). Retrieve AgentManagerInbox and AgentManagerProgress, then continue your persisted duties. Pending lead mail and notices were retained while the seat was down.",
                claim.attempt, claim.max_attempts
            ),
        );
        // Same row, same sandbox_root, exact tip: the gate refuses a busy,
        // rotated, paused or otherwise ineligible tip before any launch.
        let outcome = Box::pin(self.continue_manager_seat(claim.clone(), prompt))
            .await
            .map_err(|error| match error {
                crate::error::DaemonError::InvalidParam(code) => code,
                other => other.to_string(),
            });
        if let Err(error) = &outcome {
            tracing::warn!(project_id=%project,session_id=%claim.tip_session_id,error=%error,"manager seat recovery attempt did not establish a continuation");
        }
        Ok(usize::from(
            crate::store_lock::lock(
                &self.store,
                "manager_coordinator::execute_manager_seat_claim",
            )
            .await
            .finish_manager_seat_claim(&claim, outcome.as_ref().copied().map_err(String::as_str))?,
        ))
    }

    pub(super) async fn check_manager_decision_runtime(
        &self,
        delivery: &ManagerDecisionDeliveryV2,
    ) -> Result<()> {
        self.check_question_runtime(decision_session(delivery)?, &delivery.target)
            .await?;
        crate::store_lock::lock(
            &self.store,
            "manager_coordinator::check_manager_decision_runtime",
        )
        .await
        .manager_v2_check_decision_target(delivery)?;
        Ok(())
    }

    pub(super) async fn check_question_runtime(
        &self,
        target: Uuid,
        expected: &Value,
    ) -> Result<()> {
        // Match the actual latest question event, including an unpersisted
        // event whose id is still zero. Hold runtime read guards through the
        // durable check so detection+append cannot interleave between them.
        let matches_runtime = |pending: &Option<rsi_common::types::PendingQuestion>,
                               events: &[rsi_common::types::ConversationEvent]|
         -> Result<()> {
            let event = events.iter().rev().find(|event| {
                event.event_type == rsi_common::types::EventType::ToolUse
                    && event.tool_name.as_deref() == Some("AskUserQuestion")
            });
            let exact = event.is_some_and(|event| {
                event.id > 0
                    && expected["event_id"].as_i64() == Some(event.id)
                    && expected["event_sequence"].as_i64() == Some(i64::from(event.sequence))
                    && expected["tool_use_id"].as_str() == event.tool_use_id.as_deref()
            });
            if !exact || serde_json::to_value(pending)? != expected["question"] {
                return Err(refused("manager_v2_decision_runtime_changed"));
            }
            Ok(())
        };
        let active = self.active.read().await;
        let completed = self.completed.read().await;
        if let Some(tracked) = active.get(&target) {
            matches_runtime(&tracked.pending_question, &tracked.events)?;
        } else if let Some(cached) = completed.get(&target) {
            // Cold startup placeholders contain no runtime evidence; the
            // producer-bound durable publication is authoritative there.
            if cached.events_hydrated {
                matches_runtime(&cached.session.pending_question, &cached.events)?;
            }
        }
        Ok(())
    }

    /// The caller holds the spawn guard and has established the provider, but
    /// has not installed its event monitor. No newer question can be consumed
    /// while this exact old question is cleared.
    pub(super) async fn clear_delivered_manager_question(
        &self,
        delivery: &ManagerDecisionDeliveryV2,
    ) -> Result<()> {
        let target = decision_session(delivery)?;
        self.check_manager_decision_runtime(delivery).await?;
        crate::store_lock::lock(
            &self.store,
            "manager_coordinator::clear_delivered_manager_question",
        )
        .await
        .clear_pending_question_exact(target, &delivery.target)
    }
}

pub(super) fn decision_session(delivery: &ManagerDecisionDeliveryV2) -> Result<Uuid> {
    delivery
        .target
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| refused("manager_v2_decision_target_required"))
}

/// #953: superseded lineages (rotation leftovers, gone targets) never keep a
/// live resume wake. Hygiene only: a failure defers, never aborts the pass.
fn sweep_superseded_resume_wakes(store: &crate::store::Store) {
    match store.sweep_superseded_resume_wakes() {
        Ok(retired) if !retired.is_empty() => tracing::info!(
            count = retired.len(),
            job_ids = ?retired,
            "retired superseded-lineage resume wakes"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "superseded resume wake sweep deferred"),
    }
}
