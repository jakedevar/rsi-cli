//! Startup ownership transfer for Claude turns; never launches a provider.
use super::*;
use crate::claude::{
    ClaudeProcess,
    turn_spool::{DetachedTurn, lock_held},
};
use crate::store::provider_turn_custody::ProviderTurnCustody;

impl SessionManager {
    /// Fence a prior daemon's turn before admission or a provider side effect.
    pub(in crate::session) async fn fence_prior_detached_turn(
        &self,
        session_id: Uuid,
    ) -> Result<()> {
        let store = self.store.lock().await;
        store.abandon_unlocked_provider_turns_from_prior_boot(
            session_id,
            self.program_run_boot_id,
        )?;
        // Claims precede recovery dispatch and active-map publication. A
        // current-boot adopted row still owns this turn during that window,
        // including a completed spool awaiting monitor finalization.
        if store
            .list_active_provider_turn_custody()?
            .iter()
            .any(|row| {
                row.session_id == session_id
                    && row.boot_id == self.program_run_boot_id
                    && row.state
                        == crate::store::provider_turn_custody::ProviderTurnCustodyState::Adopted
            })
        {
            return Err(DaemonError::Process(
                "live_detached_turn: startup claim still owns this turn".into(),
            ));
        }
        Ok(())
    }

    pub(in crate::session) async fn claim_detached_turns_on_startup(
        &self,
    ) -> Result<HashMap<Uuid, ProviderTurnCustody>> {
        let store = self.store.clone();
        let boot = self.program_run_boot_id;
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let mut claimed = HashMap::new();
            for mut row in store.list_active_provider_turn_custody()? {
                // A bad spool or a lost claim belongs to this turn alone. Keep
                // its prior-boot custody as a fence while ordinary recovery
                // handles the session; never disable startup for other turns.
                let claim: Result<bool> = (|| {
                    let Some(session) = store.get_session(row.session_id)? else {
                        return Ok(false);
                    };
                    if !matches!(
                        session.provider,
                        SessionProvider::Claude
                            | SessionProvider::Codex
                            | SessionProvider::Antigravity
                    ) || !matches!(
                        session.status,
                        SessionStatus::Running
                            | SessionStatus::Starting
                            | SessionStatus::WaitingApproval
                    ) || store.session_model_invocation_id(session.id)?
                        != Some(row.invocation_id)
                    {
                        return Ok(false);
                    }
                    // A turn may finish while rsid is down. Its durable exit and
                    // remaining stdout still belong to this invocation.
                    if !row.is_adoptable()? {
                        if lock_held(&row.spool_dir)? {
                            return Err(DaemonError::Process(
                                "live_detached_turn: startup identity cannot be proven".into(),
                            ));
                        }
                        if !row.spool_dir.join("exit.json").is_file() {
                            store.abandon_unlocked_provider_turns_from_prior_boot(
                                session.id, boot,
                            )?;
                            return Ok(false);
                        }
                    }
                    if row.boot_id != boot
                        && !store.claim_provider_turn_custody(
                            row.invocation_id,
                            row.boot_id,
                            boot,
                        )?
                    {
                        return Err(DaemonError::Process(
                            "turn_custody_changed: startup claim lost".into(),
                        ));
                    }
                    Ok(true)
                })();
                match claim {
                    Ok(true) => {
                        row.boot_id = boot;
                        claimed.insert(row.session_id, row);
                    }
                    Ok(false) => {}
                    Err(error) => tracing::warn!(
                        session_id = %row.session_id,
                        invocation_id = %row.invocation_id,
                        reason = "turn_adoption_claim_failed",
                        %error,
                        "Detached turn claim failed; continuing startup recovery"
                    ),
                }
            }
            Ok(claimed)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    pub(in crate::session) async fn restore_detached_turn(
        &self,
        mut session: Session,
        row: ProviderTurnCustody,
    ) -> Result<()> {
        let _spawn_guard = super::super::spawn_single_flight::acquire_spawn_guard(session.id).await;
        if self.active.read().await.contains_key(&session.id) {
            tracing::warn!(
                session_id = %session.id,
                invocation_id = %row.invocation_id,
                reason = "turn_adoption_active_session",
                "Detached turn monitor not installed: session already active; custody needs inspection"
            );
            return Ok(());
        }
        let session_id = session.id;
        let (events, metrics) = {
            let store = self.store.lock().await;
            (
                store.load_events(session_id)?,
                store.load_turn_metrics(session_id)?,
            )
        };
        let sequence = events.iter().map(|e| e.sequence).max().unwrap_or(0);
        let generation = self.next_spawn_generation();
        if let Some(resolved) = session.resolved_context_budget.take() {
            session.resolved_context_budget = Some(
                crate::provider_capabilities::rehydrate_resolved_context_budget(
                    session.provider,
                    session.model.as_deref().unwrap_or("unknown"),
                    resolved,
                ),
            );
        }
        let (stop_tx, stop_rx) = mpsc::channel(1);
        let mut tracked = TrackedSession::restored(session, stop_tx);
        tracked.spawn_generation = generation;
        tracked.events = events;
        tracked.turn_metrics = metrics;
        tracked.rotation = super::super::rotation_coordinator::RotationCoordinator::new(
            session_id,
            tracked.session.rotation_depth,
            self.context_rotation_enabled && tracked.session.rotation_disabled_at.is_none(),
        );
        // Resolve fallible monitor dependencies before publishing the process.
        let settlements = self.model_call_settlements.handle()?;
        let turn = DetachedTurn::adopt_with_output(row, tracked.session.provider)?;
        let (tx, rx) = mpsc::channel(100);
        turn.spawn_reader(tx);
        tracked.process = Some(match tracked.session.provider {
            SessionProvider::Claude => ProviderProcess::Claude(ClaudeProcess::adopt(turn)),
            SessionProvider::Codex => {
                ProviderProcess::Codex(crate::codex::CodexProcess::adopt(turn))
            }
            SessionProvider::Antigravity => {
                ProviderProcess::Antigravity(crate::agy::AgyProcess::adopt(turn))
            }
            _ => return Err(DaemonError::Process("unsupported detached provider".into())),
        });
        self.active.write().await.insert(session_id, tracked);

        let active = self.active.clone();
        let completed = self.completed.clone();
        let event_bus = self.event_bus.clone();
        let store = self.store.clone();
        let persistence = self.persistence.clone();
        let rotation_enabled = self.context_rotation_enabled;
        let socket = self.socket_path.clone();
        let counter = self.token_counter.clone();
        let memory = self.memory_handle.clone();
        let retry = self.retry_tx.clone();
        let tools = self.tool_registry.clone();
        let runtime = self.runtime_config.clone();
        let coordinator = self.spawn_coordinator.clone();
        let tokens = self.agent_tokens.clone();
        let epoch = self.spawn_epoch.clone();
        let arbiter = self.agent_message_arbiter.clone();
        let codegraph = self.codegraph_handle.clone();
        let custody = self.custody_execution_runtime();
        tokio::spawn(Self::monitor_session(
            session_id,
            generation,
            Box::new(crate::provider::CliProviderSession::new(rx)),
            active,
            completed,
            event_bus,
            stop_rx,
            store,
            settlements,
            persistence,
            sequence,
            rotation_enabled,
            socket,
            counter,
            memory,
            retry,
            tools,
            crate::turn_controller::TurnController::new(
                crate::turn_controller::ContinuationPolicy::Single,
            ),
            runtime,
            coordinator,
            tokens,
            epoch,
            arbiter,
            codegraph,
            custody,
        ));
        Ok(())
    }
}
