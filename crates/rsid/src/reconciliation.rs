//! Periodic reconciliation loop for session state validation.
//!
//! Validates that in-memory session state matches subprocess reality and
//! SQLite persistence. Detects dead processes, state drift, and optionally
//! auto-remediates stalled sessions.

use serde::{Deserialize, Serialize};

/// Reason a session was reconciled (transitioned by the reconciliation loop).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconciliationReason {
    /// Process exited but session was still marked as active.
    ProcessDied,
    /// Session was active in SQLite but not in the in-memory map (or vice versa).
    StoreDesync,
    /// Session was stalled beyond threshold and auto-remediation was enabled.
    StallRemediation,
}

/// Action to take when a session is detected as stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallAction {
    /// Current behavior: publish bus event only.
    Notify,
    /// Send interrupt signal to terminate the process.
    Interrupt,
    /// Interrupt the process and queue a retry (if retries remain).
    InterruptAndRetry,
}

impl StallAction {
    /// Parse from a string env var value. Unknown values default to `Notify`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "interrupt" => Self::Interrupt,
            "interrupt_and_retry" => Self::InterruptAndRetry,
            _ => Self::Notify,
        }
    }
}

impl std::str::FromStr for StallAction {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(StallAction::from_str(s))
    }
}

/// Configuration for the reconciliation loop.
pub struct ReconciliationConfig {
    /// Interval between liveness checks (seconds). Default: 120.
    pub liveness_interval_secs: u64,
    /// Interval between SQLite consistency checks (seconds). Default: 600.
    pub consistency_interval_secs: u64,
    /// Stall action for Standard (interactive) sessions.
    pub standard_stall_action: StallAction,
    /// Stall action for TaskRabbit/Bug (unattended) sessions.
    pub unattended_stall_action: StallAction,
}

impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            liveness_interval_secs: 120,
            consistency_interval_secs: 600,
            standard_stall_action: StallAction::Notify,
            unattended_stall_action: StallAction::InterruptAndRetry,
        }
    }
}

// ---------------------------------------------------------------------------
// Liveness check loop (Phase 2)
// ---------------------------------------------------------------------------

use crate::bus::{DaemonEvent, EventBus};
use crate::error::{DaemonError, Result as DaemonResult};
use crate::session::SessionManager;
use crate::session::types::TrackedSession;
use crate::store::{IdeaControllerReconciliationCursor, Store};
use chrono::{DateTime, Utc};
use rsi_common::types::SessionStatus;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Process-local handoff fence. UUIDs are daemon-wide identities; only the
/// current process can be settling a turn. A restart begins with an empty set.
fn terminal_settlements() -> &'static Mutex<HashSet<Uuid>> {
    static SETTLING: OnceLock<Mutex<HashSet<Uuid>>> = OnceLock::new();
    SETTLING.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) struct TerminalSettlementGuard {
    session_id: Uuid,
}

impl TerminalSettlementGuard {
    pub(crate) fn new(session_id: Uuid) -> Self {
        terminal_settlements()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id);
        Self { session_id }
    }
}

impl Drop for TerminalSettlementGuard {
    fn drop(&mut self) {
        terminal_settlements()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session_id);
    }
}

impl SessionManager {
    /// Reconcile committed `ProgramRun` action truth after Session repair and
    /// live D03/A6 grant restoration, before the dispatcher can publish.
    ///
    /// # Errors
    ///
    /// Returns a storage error when a bounded page cannot be classified or committed.
    pub async fn reconcile_program_runs_at_startup(&self) -> DaemonResult<()> {
        let mut cursor = None;
        loop {
            let active = self.active.read().await;
            let store = self.store.lock().await;
            let tokens = self.agent_tokens.read().await;
            let mut live_controllers = HashMap::new();
            for session_id in active.keys().copied() {
                if tokens.token_for_session(session_id).is_some()
                    && let Some(grant) = store.controller_grant_v1(session_id)
                    && let Ok(epoch) = u64::try_from(grant.controller_epoch())
                {
                    live_controllers.insert(session_id, epoch);
                }
            }
            let page = store
                .reconcile_program_runs_page_v1(
                    None,
                    cursor.as_ref(),
                    256,
                    2_000,
                    false,
                    Utc::now(),
                    &live_controllers,
                    self.program_run_boot_id,
                )
                .map_err(|error| DaemonError::Store(error.to_string()))?;
            drop(tokens);
            drop(store);
            drop(active);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(())
    }

    /// Reconcile durable Idea/controller state before any restored session can
    /// reconstruct a semantic grant. Pages are deliberately bounded at the
    /// Store's fixed 64-Idea limit and yield between pages.
    pub(crate) async fn reconcile_idea_controllers_at_startup(
        &self,
        boot_at: DateTime<Utc>,
    ) -> DaemonResult<()> {
        // A6 bindings and semantic grants are process-local. Restart begins
        // with neither, even when this helper is re-entered by a test.
        self.store.lock().await.clear_controller_grants_v1();

        let mut cursor: Option<IdeaControllerReconciliationCursor> = None;
        loop {
            let page = {
                let store = self.store.lock().await;
                store.reconcile_idea_controllers_page_v1(cursor, boot_at)
            }
            .map_err(|error| DaemonError::Store(error.to_string()))?;

            for idea_id in page.corrupt_idea_ids {
                tracing::error!(
                    %idea_id,
                    "Idea controller reconciliation failed closed for corrupt durable state"
                );
            }
            for event_id in page.committed_event_ids {
                // Consumers deduplicate with the committed Idea event ID.
                self.event_bus.publish(DaemonEvent::SystemMessage {
                    level: "info".to_string(),
                    message: format!("idea_controller_reconciled:{event_id}"),
                });
            }

            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}

/// Snapshot of a session's liveness state, collected under a single lock.
struct LivenessSnapshot {
    session_id: Uuid,
    status: SessionStatus,
    has_process: bool,
    is_alive: bool,
    is_rotating: bool,
}

/// Collect liveness snapshots for all active sessions.
/// Takes a write lock (needed for `try_wait()` mutation) then releases it.
async fn collect_liveness_snapshots(
    active: &RwLock<HashMap<Uuid, TrackedSession>>,
) -> Vec<LivenessSnapshot> {
    use crate::session::RotationState;

    let mut snapshots = Vec::new();
    let mut active_guard = active.write().await;

    for (session_id, tracked) in active_guard.iter_mut() {
        let is_rotating = matches!(
            tracked.rotation.state(),
            RotationState::PendingInterrupt { .. } | RotationState::WritingHandoff { .. }
        );

        let (has_process, is_alive) = if let Some(ref mut process) = tracked.process {
            (true, process.is_alive())
        } else {
            (false, false)
        };

        snapshots.push(LivenessSnapshot {
            session_id: *session_id,
            status: tracked.session.status,
            has_process,
            is_alive,
            is_rotating,
        });
    }

    snapshots
}

/// Signal a session's monitor task to finalize (process is dead or zombie).
/// Publishes a `SessionReconciled` bus event.
async fn signal_finalization(
    active: &RwLock<HashMap<Uuid, TrackedSession>>,
    session_id: Uuid,
    event_bus: &EventBus,
) {
    let mut active_guard = active.write().await;
    if let Some(tracked) = active_guard.get_mut(&session_id) {
        let old_status = tracked.session.status;
        // Do NOT set interrupt_requested = true: this is a process failure, not user intent.
        // Signal the monitor loop to exit, which triggers finalize_session().
        let _ = tracked.stop_tx.try_send(());

        event_bus.publish(DaemonEvent::SessionReconciled {
            session_id,
            old_status,
            new_status: SessionStatus::Failed,
            reason: ReconciliationReason::ProcessDied,
        });
    }
}

/// Compare active map against SQLite to detect state drift.
pub(crate) async fn reconcile_store_consistency(
    active: &RwLock<HashMap<Uuid, TrackedSession>>,
    store: &tokio::sync::Mutex<Store>,
    event_bus: &EventBus,
    control: Option<&crate::session::agent_verbs::AgentControlHandle>,
) {
    // Collect in-memory active IDs
    let active_ids: HashSet<Uuid> = active.read().await.keys().copied().collect();

    // Query SQLite for sessions with active-like statuses
    let store_active_ids: HashSet<Uuid> = {
        let store_guard = store.lock().await;
        match store_guard.load_active_session_ids() {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                tracing::error!(error = %e, "Reconciliation: failed to query store for active sessions");
                return;
            }
        }
    };

    // Sessions active in SQLite but not in memory: mark as Failed
    for session_id in store_active_ids.difference(&active_ids) {
        let sid = *session_id;
        // A5-P2 TOCTOU guard (review 2026-07-04): `active_ids` was snapshotted at
        // the top of this fn BEFORE `store_active_ids`, and this loop runs
        // PERIODICALLY (not only at boot). A session that finished launching in
        // that read gap is live-and-tracked NOW despite being absent from the
        // stale snapshot (launch inserts into `active` before its store row
        // becomes queryable). Reaping it would SIGKILL a healthy provider. So
        // re-read the CURRENT map and skip such a session entirely — never reap
        // or Fail a process we still manage. A genuine crashed-daemon orphan is
        // absent from the live map too, so it still falls through to the reap.
        let still_owned = {
            let active_guard = active.read().await;
            let settling_guard = terminal_settlements()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active_guard.contains_key(&sid) || settling_guard.contains(&sid)
        };
        if still_owned {
            continue;
        }
        tracing::warn!(
            session_id = %sid,
            "Reconciliation: session active in store but not in memory -- marking Failed"
        );
        // Reap the surviving orphaned OS subprocess (byte-exact `RSI_SESSION_ID`
        // env match, subprocess-only, self-skip, TOCTOU-guarded via
        // `reap_orphans_for_session`) BEFORE the Failed-flip, closing the bounded
        // process leak. Blocking `/proc` walk -> `spawn_blocking`. Any inventory
        // or proof failure preserves the active-like row for a later retry;
        // marking it Failed would discard the only durable ownership evidence.
        let reaped = match tokio::task::spawn_blocking(move || {
            crate::session::reap_orphans_for_session(sid)
        })
        .await
        {
            Ok(Ok(reaped)) => reaped,
            Ok(Err(error)) => {
                tracing::error!(
                    error = %error,
                    session_id = %sid,
                    "Reconciliation: orphan exclusion failed; retaining active-like row for retry"
                );
                continue;
            }
            Err(error) => {
                tracing::error!(
                    error = %error,
                    session_id = %sid,
                    "Reconciliation: orphan exclusion task failed; retaining active-like row for retry"
                );
                continue;
            }
        };
        if reaped > 0 {
            tracing::warn!(
                session_id = %sid,
                reaped,
                "Reconciliation: reaped orphaned provider subprocess(es) before marking Failed"
            );
        }
        let staged = {
            // The finalizer installs its marker under the active write lock
            // before removing the session. Hold the active read lock through
            // the store transition, then re-check both ownership and durable
            // status after the potentially slow orphan proof. This closes the
            // active-map removal -> terminal-status persistence gap.
            let active_guard = active.read().await;
            let store_guard = store.lock().await;
            let settling_guard = terminal_settlements()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let row_is_active = match store_guard.get_session(sid) {
                Ok(Some(row)) => matches!(
                    row.status,
                    SessionStatus::Starting
                        | SessionStatus::Running
                        | SessionStatus::WaitingApproval
                ),
                Ok(None) => false,
                Err(error) => {
                    tracing::error!(%error, session_id = %sid, "Reconciliation: final status re-read failed");
                    false
                }
            };
            if active_guard.contains_key(&sid) || settling_guard.contains(&sid) || !row_is_active {
                None
            } else {
                Some(store_guard.update_failed_and_stage_c5_autofile(
                    sid,
                    crate::store::daemon_settings::AutofileCause::StoreDesync,
                ))
            }
        };
        let Some(staged) = staged else { continue };
        if let Err(e) = staged {
            tracing::error!(
                error = %e,
                session_id = %session_id,
                "Reconciliation: failed to update store status"
            );
        } else {
            event_bus.publish(DaemonEvent::SessionReconciled {
                session_id: *session_id,
                // We don't know the exact old status without another query; approximate.
                old_status: SessionStatus::Running,
                new_status: SessionStatus::Failed,
                reason: ReconciliationReason::StoreDesync,
            });
            if let Some(control) = control {
                control
                    .maybe_autofile_terminal_failure(
                        *session_id,
                        crate::store::daemon_settings::RecoveryDisposition::NoRecoverySource,
                    )
                    .await;
            }
        }
    }

    // Sessions in memory but not active in SQLite: warn (unusual -- store write likely pending)
    for session_id in active_ids.difference(&store_active_ids) {
        tracing::warn!(
            session_id = %session_id,
            "Reconciliation: session in memory but not active in store (pending write?)"
        );
        event_bus.publish(DaemonEvent::SystemMessage {
            level: "warn".to_string(),
            message: format!("Session {session_id} is active in memory but not in store"),
        });
    }
}

/// Spawn the reconciliation background task.
///
/// Periodically:
/// 1. Checks process liveness for all active sessions (every `liveness_interval_secs`).
/// 2. Compares active map against SQLite for state drift (every `consistency_interval_secs`).
pub fn spawn_reconciliation_loop(
    active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    store: Arc<tokio::sync::Mutex<Store>>,
    event_bus: Arc<EventBus>,
    config: ReconciliationConfig,
) -> tokio::task::JoinHandle<()> {
    spawn_reconciliation_loop_with_control(active, store, event_bus, config, None)
}

/// C5 production entrypoint. Tests that only exercise reconciliation mechanics
/// may use the legacy wrapper above; the daemon always provides the shared
/// policy choke so store-desync failures settle immediately after persistence.
pub fn spawn_reconciliation_loop_with_control(
    active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    store: Arc<tokio::sync::Mutex<Store>>,
    event_bus: Arc<EventBus>,
    config: ReconciliationConfig,
    control: Option<crate::session::agent_verbs::AgentControlHandle>,
) -> tokio::task::JoinHandle<()> {
    spawn_reconciliation_loop_with_heartbeat(active, store, event_bus, config, control, None)
}

/// Production entrypoint with a completed-pass marker for the independent watchdog.
pub fn spawn_reconciliation_loop_with_heartbeat(
    active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    store: Arc<tokio::sync::Mutex<Store>>,
    event_bus: Arc<EventBus>,
    config: ReconciliationConfig,
    control: Option<crate::session::agent_verbs::AgentControlHandle>,
    heartbeat: Option<crate::watchdog::LoopHeartbeat>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut liveness_interval =
            tokio::time::interval(Duration::from_secs(config.liveness_interval_secs));
        liveness_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let consistency_every_n = if config.liveness_interval_secs > 0 {
            config.consistency_interval_secs / config.liveness_interval_secs
        } else {
            5 // fallback: every 5 ticks
        };
        let mut consistency_counter: u64 = 0;

        loop {
            liveness_interval.tick().await;
            consistency_counter += 1;

            // --- Sub-step A: Process liveness check ---
            let snapshots = collect_liveness_snapshots(&active).await;

            for snapshot in &snapshots {
                // Only check sessions in active statuses
                if !matches!(
                    snapshot.status,
                    SessionStatus::Running
                        | SessionStatus::Starting
                        | SessionStatus::WaitingApproval
                ) {
                    continue;
                }

                // Skip sessions in rotation handoff — don't interrupt mid-handoff
                if snapshot.is_rotating {
                    tracing::debug!(
                        session_id = %snapshot.session_id,
                        "Reconciliation: skipping session in rotation"
                    );
                    continue;
                }

                // Detect dead process (has handle but it's no longer alive)
                if snapshot.has_process && !snapshot.is_alive {
                    tracing::warn!(
                        session_id = %snapshot.session_id,
                        status = ?snapshot.status,
                        "Reconciliation: process died but session still active"
                    );
                    signal_finalization(&active, snapshot.session_id, &event_bus).await;
                    continue;
                }

                // Detect zombie (active status but no process handle)
                if !snapshot.has_process {
                    tracing::warn!(
                        session_id = %snapshot.session_id,
                        status = ?snapshot.status,
                        "Reconciliation: session active but no process handle (zombie)"
                    );
                    signal_finalization(&active, snapshot.session_id, &event_bus).await;
                }
            }

            // --- Sub-step B: SQLite consistency check (less frequent) ---
            if consistency_counter >= consistency_every_n {
                consistency_counter = 0;
                reconcile_store_consistency(&active, &store, &event_bus, control.as_ref()).await;
            }
            if let Some(heartbeat) = &heartbeat {
                heartbeat.mark_completed();
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconciliation_reason_serde_roundtrip() {
        for reason in [
            ReconciliationReason::ProcessDied,
            ReconciliationReason::StoreDesync,
            ReconciliationReason::StallRemediation,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let deser: ReconciliationReason = serde_json::from_str(&json).unwrap();
            assert_eq!(reason, deser);
        }
    }

    #[test]
    fn test_reconciliation_config_default() {
        let config = ReconciliationConfig::default();
        assert_eq!(config.liveness_interval_secs, 120);
        assert_eq!(config.consistency_interval_secs, 600);
        assert_eq!(config.standard_stall_action, StallAction::Notify);
        assert_eq!(
            config.unattended_stall_action,
            StallAction::InterruptAndRetry
        );
    }

    #[test]
    fn test_stall_action_from_str() {
        assert_eq!(StallAction::from_str("notify"), StallAction::Notify);
        assert_eq!(StallAction::from_str("interrupt"), StallAction::Interrupt);
        assert_eq!(
            StallAction::from_str("interrupt_and_retry"),
            StallAction::InterruptAndRetry
        );
        // Unknown values default to Notify
        assert_eq!(StallAction::from_str("unknown"), StallAction::Notify);
        assert_eq!(StallAction::from_str("INTERRUPT"), StallAction::Interrupt);
    }

    #[test]
    fn d05_reconciliation_executes_bounded_dry_run_without_mutation() {
        let store = Store::open_in_memory().unwrap();
        let before = store.conn.total_changes();
        let page = store
            .reconcile_program_runs_page_v1(
                None,
                None,
                256,
                1,
                true,
                Utc::now(),
                &HashMap::new(),
                store.program_run_boot_id(),
            )
            .unwrap();
        assert!(page.items.is_empty());
        assert!(page.dry_run);
        assert_eq!(store.conn.total_changes(), before);
        assert!(
            store
                .reconcile_program_runs_page_v1(
                    None,
                    None,
                    257,
                    1,
                    true,
                    Utc::now(),
                    &HashMap::new(),
                    store.program_run_boot_id(),
                )
                .is_err()
        );
    }

    /// A5-P2 (F-013/F-014): boot-reconciliation orphan reap. Real OS
    /// subprocesses + `/proc`; Linux-only (the reaper is Linux-only). Proves the
    /// crashed-daemon Failed-flip path SIGKILLs the surviving stamped orphan and
    /// that the surgical env-match never touches an unrelated process.
    #[cfg(target_os = "linux")]
    mod boot_reconciliation_reap {
        use super::super::{TerminalSettlementGuard, reconcile_store_consistency};
        use crate::bus::EventBus;
        use crate::store::Store;
        use nix::errno::Errno;
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;
        use rsi_common::identity::{
            ENV_PROCESS_OWNERSHIP_NAMESPACE, ENV_SESSION_ID, process_ownership_namespace,
        };
        use rsi_common::types::{Session, SessionStatus};
        use std::collections::HashMap;
        use std::time::Duration;
        use tokio::sync::RwLock;
        use uuid::Uuid;

        /// RAII safety net: SIGKILL the pid on drop so a panicking test can never
        /// leak the stand-in. ESRCH (already reaped) is ignored.
        struct KillOnDrop(i32);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = signal::kill(Pid::from_raw(self.0), Signal::SIGKILL);
            }
        }

        /// signal-0 existence probe: `Ok` while the pid entry exists (including a
        /// not-yet-reaped zombie), `Err(ESRCH)` once it is gone.
        fn alive(pid: i32) -> bool {
            signal::kill(Pid::from_raw(pid), None::<Signal>).is_ok()
        }

        /// A single long-lived process stamped `RSI_SESSION_ID=<sid>` — a
        /// race-free stand-in for a crashed daemon's orphaned provider (SIGKILL
        /// is uncatchable, so a lone `sleep` dies identically to a real provider).
        fn spawn_stamped(sid: &str) -> tokio::process::Child {
            tokio::process::Command::new("sleep")
                .arg("2147483647")
                .env(ENV_SESSION_ID, sid)
                .env(
                    ENV_PROCESS_OWNERSHIP_NAMESPACE,
                    process_ownership_namespace(),
                )
                .spawn()
                .expect("spawn sleep stand-in")
        }

        /// Minimal store-active session row: only the serde-required `Session`
        /// fields are set; every other field takes its `#[serde(default)]`.
        fn active_session_row(id: Uuid) -> Session {
            let now = chrono::Utc::now().to_rfc3339();
            serde_json::from_value(serde_json::json!({
                "id": id.to_string(),
                "status": "Running",
                "created_at": now,
                "updated_at": now,
                "query": "boot-reconciliation-reap test",
                "working_dir": "/tmp/rsi-boot-reap-test",
                "claude_session_id": null,
            }))
            .expect("build test session")
        }

        async fn poll_gone(pid: i32) -> bool {
            for _ in 0..200 {
                if signal::kill(Pid::from_raw(pid), None::<Signal>) == Err(Errno::ESRCH) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        }

        /// Positive: on a boot pass, a store-active session absent from the fresh
        /// in-memory map has its surviving stamped subprocess SIGKILLed **and**
        /// its row flipped to `Failed` — the reap runs inside the same Failed-flip
        /// loop, structurally before the flip.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn reaps_orphan_before_marking_failed() {
            let sid = Uuid::new_v4();
            let store = tokio::sync::Mutex::new(Store::open_in_memory().expect("in-memory store"));
            store
                .lock()
                .await
                .insert_session(&active_session_row(sid))
                .expect("insert active session");

            // The orphan a crashed daemon leaves reparented to init: a live OS
            // process stamped with THIS session id.
            let mut child = spawn_stamped(&sid.to_string());
            let pid = child.id().expect("child pid") as i32;
            let _guard = KillOnDrop(pid);
            assert!(alive(pid), "orphan must be alive before reconcile");

            // Fresh boot: nothing tracked in memory.
            let active = RwLock::new(HashMap::new());
            let event_bus = EventBus::new(16);

            reconcile_store_consistency(&active, &store, &event_bus, None).await;

            // Reap the now-SIGKILLed zombie so the existence probe can reach ESRCH.
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            assert!(
                poll_gone(pid).await,
                "orphaned subprocess must be reaped by boot reconciliation"
            );

            // ...and the row the reap precedes is now Failed.
            let status = store
                .lock()
                .await
                .get_session(sid)
                .expect("get_session")
                .expect("row present")
                .status;
            assert_eq!(
                status,
                SessionStatus::Failed,
                "row must be flipped to Failed"
            );
        }

        /// Daemon-safety negative: reconcile reaps only the exact id it is
        /// failing. A live process stamped with a DIFFERENT session id (a foreign
        /// session / the daemon analogue) is never touched — the crashed-daemon
        /// reap cannot misfire onto an unrelated process.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn does_not_reap_unrelated_stamped_process() {
            let failing = Uuid::new_v4();
            let bystander = Uuid::new_v4();
            let store = tokio::sync::Mutex::new(Store::open_in_memory().expect("in-memory store"));
            store
                .lock()
                .await
                .insert_session(&active_session_row(failing))
                .expect("insert active session");

            // A live process stamped with a DIFFERENT id that reconcile must
            // never SIGKILL.
            let mut child = spawn_stamped(&bystander.to_string());
            let pid = child.id().expect("child pid") as i32;
            let _guard = KillOnDrop(pid);
            assert!(alive(pid));

            let active = RwLock::new(HashMap::new());
            let event_bus = EventBus::new(16);

            reconcile_store_consistency(&active, &store, &event_bus, None).await;

            // Bystander survives; only the failing row is flipped.
            assert!(
                alive(pid),
                "a process with a non-matching id must remain alive"
            );
            let status = store
                .lock()
                .await
                .get_session(failing)
                .expect("get_session")
                .expect("row present")
                .status;
            assert_eq!(status, SessionStatus::Failed);

            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }

        /// A tracked (in-`active`-map) session is NEVER reaped or Failed by
        /// reconcile — even with an active-like store row and a live process
        /// carrying its stamp. Guards the invariant the A5-P2 TOCTOU re-check
        /// protects: a session that finished launching in the active-read ->
        /// store-read gap is a live managed session, not an orphan. (Here the
        /// session is in the map when reconcile snapshots it, so the difference
        /// set excludes it; the fresh `contains_key` re-check is the second line
        /// of defense for the stale-snapshot race.)
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn does_not_reap_tracked_live_session() {
            use crate::session::types::TrackedSession;
            let sid = Uuid::new_v4();
            let store = tokio::sync::Mutex::new(Store::open_in_memory().expect("in-memory store"));
            let row = active_session_row(sid);
            store
                .lock()
                .await
                .insert_session(&row)
                .expect("insert active session");

            // A live process carrying this session's stamp...
            let mut child = spawn_stamped(&sid.to_string());
            let pid = child.id().expect("child pid") as i32;
            let _guard = KillOnDrop(pid);
            assert!(alive(pid));

            // ...but the session IS tracked in the live map (a managed session).
            let active = RwLock::new(HashMap::new());
            active
                .write()
                .await
                .insert(sid, TrackedSession::new_for_test(row));
            let event_bus = EventBus::new(16);

            reconcile_store_consistency(&active, &store, &event_bus, None).await;

            // Neither reaped nor Failed.
            assert!(
                alive(pid),
                "a tracked/managed session's process must never be reaped"
            );
            let status = store
                .lock()
                .await
                .get_session(sid)
                .expect("get_session")
                .expect("row present")
                .status;
            assert_eq!(
                status,
                SessionStatus::Running,
                "a tracked session must not be flipped to Failed"
            );

            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn orphan_proof_failure_retains_active_like_row_for_retry() {
            let sid = Uuid::new_v4();
            let store = tokio::sync::Mutex::new(Store::open_in_memory().expect("in-memory store"));
            store
                .lock()
                .await
                .insert_session(&active_session_row(sid))
                .expect("insert active session");

            crate::session::fail_runtime_orphan_reap_for_test(sid);
            let active = RwLock::new(HashMap::new());
            let event_bus = EventBus::new(16);

            reconcile_store_consistency(&active, &store, &event_bus, None).await;

            let status = store
                .lock()
                .await
                .get_session(sid)
                .expect("get_session")
                .expect("row present")
                .status;
            assert_eq!(
                status,
                SessionStatus::Running,
                "failed orphan proof must preserve durable ownership evidence"
            );
        }

        /// A finalizer removes the active entry before SQLite records its
        /// terminal status. That interval remains owned by the finalizer;
        /// reconciliation must leave it alone and honor the eventual status.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn settling_turn_keeps_terminal_status_during_reconciliation() {
            let sid = Uuid::new_v4();
            let store = tokio::sync::Mutex::new(Store::open_in_memory().unwrap());
            store
                .lock()
                .await
                .insert_session(&active_session_row(sid))
                .unwrap();
            let active = RwLock::new(HashMap::new());
            let event_bus = EventBus::new(16);
            let settling = TerminalSettlementGuard::new(sid);

            reconcile_store_consistency(&active, &store, &event_bus, None).await;
            assert_eq!(
                store.lock().await.get_session(sid).unwrap().unwrap().status,
                SessionStatus::Running,
                "a turn settling outside the active map retains its status"
            );

            store
                .lock()
                .await
                .update_session_status(sid, SessionStatus::Completed)
                .unwrap();
            drop(settling);
            reconcile_store_consistency(&active, &store, &event_bus, None).await;
            assert_eq!(
                store.lock().await.get_session(sid).unwrap().unwrap().status,
                SessionStatus::Completed,
                "a stale active-ID snapshot cannot overwrite terminal truth"
            );
        }
    }

    #[tokio::test]
    async fn d03_controller_reconciliation_clears_process_local_grants() -> anyhow::Result<()> {
        use crate::config::{Config, RuntimeConfig};
        use crate::idea_control::BoundControllerWriteAuthority;

        let temp = tempfile::tempdir()?;
        let store = Store::open(&temp.path().join("reconciliation.db"))?;
        let session_id = Uuid::new_v4();
        store.install_controller_grant_v1(BoundControllerWriteAuthority::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            session_id,
            3,
        )?);
        let manager = SessionManager::new(
            Arc::new(EventBus::new(16)),
            store,
            false,
            temp.path().join("daemon.sock"),
            None,
            Vec::new(),
            RuntimeConfig::from_config(&Config::from_env()),
            temp.path().join("sandboxes"),
        )?;
        assert!(
            manager
                .store
                .lock()
                .await
                .controller_grant_v1(session_id)
                .is_some()
        );
        manager
            .reconcile_idea_controllers_at_startup(chrono::Utc::now())
            .await?;
        assert_eq!(
            manager.store.lock().await.controller_grant_v1(session_id),
            None
        );
        assert_eq!(
            rsi_common::types::IDEA_CONTROLLER_RECONCILIATION_BATCH_SIZE,
            64
        );
        Ok(())
    }
}
