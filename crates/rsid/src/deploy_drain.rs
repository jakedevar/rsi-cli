//! Deploy drain (#1073): while an agent-requested deploy waits for its quiet
//! point, hold the work that would keep it from arriving.
//!
//! The deploy runner ([`crate::deploy::poll_once`]) re-derives the hold from
//! the durable live deploy row every tick, so a restarted daemon picks it up
//! again. The hold is bounded by the deploy's own deadline: past it the hold
//! is released whether or not the runner has settled the row yet, and the
//! deploy then settles as refused with its blockers, as before.
//!
//! Only work that starts a *worker* is held: child launches, child
//! continuations, scheduled child wakes and agent jobs. Running turns and
//! running jobs are never interrupted. A parentless operator session and the
//! deploy's own caller are never held.
//!
//! Held work is deferred, never dropped: durable work (spawn requests, topology
//! nodes) waits in place and runs when the hold releases; scheduled wakes stay
//! due and are re-evaluated next tick; work whose caller keeps the request
//! (a manager create, a continuation, a job submit) is answered with the typed
//! refusal [`DEPLOY_DRAINING`] and retried by that caller.

use crate::error::{DaemonError, Result};
use crate::store::agent_deploys::DeployRow;
use chrono::{DateTime, SecondsFormat, Utc};
pub use rsi_common::agent_daemon_info::DEPLOY_DRAINING;
use rsi_common::agent_daemon_info::{DeployDrainV1, HeldWorkV1};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;
use uuid::Uuid;

/// JSON-RPC code of the structured `deploy_draining` refusal.
pub const DEPLOY_DRAINING_RPC_CODE: i32 = -32031;

/// Retry hint (ms) carried by the `deploy_draining` refusal.
const DEPLOY_DRAINING_RETRY_AFTER_MS: u64 = 5_000;

/// The typed, retryable `deploy_draining` refusal: a stable code and an
/// explicit retry action, identical over every transport.
#[must_use]
pub fn draining_error() -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: DEPLOY_DRAINING_RPC_CODE,
        message: DEPLOY_DRAINING.into(),
        data: serde_json::json!({
            "kind": DEPLOY_DRAINING,
            "code": DEPLOY_DRAINING,
            "retryable": true,
            "retry_after_ms": DEPLOY_DRAINING_RETRY_AFTER_MS,
            "next_action": "retry the same request after the deploy settles; AgentGetDaemonInfo deploy_drain.release_by bounds the hold",
        }),
    }
}

/// True for the `deploy_draining` refusal in either of its shapes.
#[must_use]
pub fn is_draining_error(error: &DaemonError) -> bool {
    match error {
        DaemonError::StructuredRpc { data, .. } => {
            data.get("kind").and_then(serde_json::Value::as_str) == Some(DEPLOY_DRAINING)
        }
        DaemonError::PolicyDenied(message) => message == DEPLOY_DRAINING,
        _ => false,
    }
}

/// What kind of work is parked behind the drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldKind {
    ChildSpawn,
    TopologyNode,
}

impl HeldKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ChildSpawn => "child_spawn",
            Self::TopologyNode => "topology_node",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Active {
    deploy_id: Uuid,
    owner: Uuid,
    deadline: DateTime<Utc>,
}

struct Held {
    ticket: u64,
    kind: HeldKind,
    session_id: Option<Uuid>,
    since: DateTime<Utc>,
}

/// Process-wide (per `SessionManager`) deploy drain state.
pub struct DeployDrain {
    state: watch::Sender<Option<Active>>,
    waiting: Mutex<Vec<Held>>,
    next_ticket: AtomicU64,
    refused: AtomicU64,
    wakes_held: AtomicU64,
}

impl Default for DeployDrain {
    fn default() -> Self {
        Self::new()
    }
}

/// Removes the waiting entry when the waiter finishes or is cancelled.
struct WaitGuard<'a> {
    drain: &'a DeployDrain,
    ticket: u64,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut waiting) = self.drain.waiting.lock() {
            waiting.retain(|held| held.ticket != self.ticket);
        }
    }
}

impl DeployDrain {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: watch::channel(None).0,
            waiting: Mutex::new(Vec::new()),
            next_ticket: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            wakes_held: AtomicU64::new(0),
        }
    }

    /// Re-derive the hold from the live deploy row: engaged while a deploy is
    /// live (`staged`, or `restarting` until startup verification settles it),
    /// the operator setting is on and the deploy's deadline has not passed.
    /// Waiters are woken the moment the hold ends.
    pub(crate) fn sync(&self, live: Option<&DeployRow>, enabled: bool, now: DateTime<Utc>) {
        let next = live
            .filter(|row| enabled && now < row.deadline_at)
            .map(|row| Active {
                deploy_id: row.id,
                owner: row.owner_session_id,
                deadline: row.deadline_at,
            });
        self.state.send_if_modified(|current| {
            if *current == next {
                false
            } else {
                *current = next;
                true
            }
        });
    }

    fn active_at(&self, now: DateTime<Utc>) -> Option<Active> {
        (*self.state.borrow()).filter(|active| now < active.deadline)
    }

    /// True while a hold is engaged (deadline not yet passed).
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.active_at(Utc::now()).is_some()
    }

    /// True when `session` must be held: a hold is engaged and the session is
    /// neither the deploy's caller nor parentless (`has_parent == false`).
    #[must_use]
    pub fn holds(&self, session: Option<Uuid>, has_parent: bool) -> bool {
        self.holds_at(session, has_parent, Utc::now())
    }

    fn holds_at(&self, session: Option<Uuid>, has_parent: bool, now: DateTime<Utc>) -> bool {
        self.active_at(now)
            .is_some_and(|active| has_parent && session != Some(active.owner))
    }

    /// Refuse with the typed `deploy_draining` code when `session` is held.
    /// For work whose caller keeps the request and retries.
    ///
    /// # Errors
    /// `deploy_draining`.
    pub fn refuse_if_draining(&self, session: Option<Uuid>, has_parent: bool) -> Result<()> {
        if self.holds(session, has_parent) {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return Err(draining_error());
        }
        Ok(())
    }

    /// Note a queued manager action that was returned to the queue.
    pub fn note_manager_action_held(&self) {
        self.wakes_held.fetch_add(1, Ordering::Relaxed);
    }

    /// Note a scheduled resume wake that was left due instead of delivered.
    pub fn note_wake_held(&self) {
        self.wakes_held.fetch_add(1, Ordering::Relaxed);
    }

    /// Wait, in place, until the hold releases. Returns at once when the work
    /// is not held. Bounded by the deploy's deadline even if the runner stalls.
    /// Returns `true` when it actually waited.
    pub async fn wait_released(
        &self,
        kind: HeldKind,
        session_id: Option<Uuid>,
        has_parent: bool,
    ) -> bool {
        let now = Utc::now();
        if !self.holds_at(session_id, has_parent, now) {
            return false;
        }
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.push(Held {
                ticket,
                kind,
                session_id,
                since: now,
            });
        }
        let _guard = WaitGuard {
            drain: self,
            ticket,
        };
        let mut rx = self.state.subscribe();
        loop {
            let Some(active) = self.active_at(Utc::now()) else {
                return true;
            };
            let remaining = (active.deadline - Utc::now())
                .to_std()
                .unwrap_or(std::time::Duration::ZERO);
            match tokio::time::timeout(remaining, rx.changed()).await {
                Ok(Ok(())) => {}
                // Deadline reached or the sender is gone: release.
                Ok(Err(_)) | Err(_) => return true,
            }
        }
    }

    /// Operator/agent-visible status (`AgentGetDaemonInfo`).
    #[must_use]
    pub fn status(&self) -> DeployDrainV1 {
        let active = self.active_at(Utc::now());
        let held = self.waiting.lock().map_or_else(
            |_| Vec::new(),
            |waiting| {
                waiting
                    .iter()
                    .map(|entry| HeldWorkV1 {
                        kind: entry.kind.as_str().to_string(),
                        session_id: entry.session_id,
                        since: entry.since.to_rfc3339_opts(SecondsFormat::Nanos, true),
                        reason: DEPLOY_DRAINING.to_string(),
                    })
                    .collect()
            },
        );
        DeployDrainV1 {
            draining: active.is_some(),
            deploy_id: active.map(|value| value.deploy_id),
            release_by: active
                .map(|value| value.deadline.to_rfc3339_opts(SecondsFormat::Nanos, true)),
            held,
            refused_total: self.refused.load(Ordering::Relaxed),
            wakes_held_total: self.wakes_held.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::agent_deploy::DeployState;
    use std::sync::Arc;

    fn row(owner: Uuid, deadline: DateTime<Utc>) -> DeployRow {
        DeployRow {
            id: Uuid::new_v4(),
            owner_session_id: owner,
            sha: "0".repeat(40),
            manifest: Vec::new(),
            state: DeployState::Staged,
            reason: None,
            deadline_at: deadline,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_refusal_is_structured_with_a_stable_code_and_a_retry_action() {
        let drain = DeployDrain::new();
        let owner = Uuid::new_v4();
        let worker = Uuid::new_v4();
        drain.sync(
            Some(&row(owner, Utc::now() + chrono::Duration::seconds(60))),
            true,
            Utc::now(),
        );
        let error = drain.refuse_if_draining(Some(worker), true).unwrap_err();
        assert!(is_draining_error(&error));
        let DaemonError::StructuredRpc {
            rpc_code,
            message,
            data,
        } = &error
        else {
            panic!("the refusal is a structured RPC error: {error}");
        };
        assert_eq!(*rpc_code, DEPLOY_DRAINING_RPC_CODE);
        assert_eq!(message, DEPLOY_DRAINING);
        assert_eq!(data["code"], DEPLOY_DRAINING);
        assert_eq!(data["retryable"], true);
        assert!(data["retry_after_ms"].as_u64().is_some_and(|ms| ms > 0));
        assert!(
            data["next_action"]
                .as_str()
                .is_some_and(|action| action.contains("retry"))
        );
        assert!(!is_draining_error(&DaemonError::PolicyDenied(
            "other".into()
        )));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn parentless_sessions_and_the_deploy_caller_are_never_held() {
        let drain = DeployDrain::new();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        drain.sync(
            Some(&row(owner, now + chrono::Duration::seconds(60))),
            true,
            now,
        );
        let worker = Uuid::new_v4();
        assert!(drain.holds(Some(worker), true));
        assert!(drain.holds(None, true), "a new launch has no id yet");
        assert!(
            !drain.holds(Some(worker), false),
            "parentless operator session"
        );
        assert!(!drain.holds(Some(owner), true), "the deploy's caller");
        assert!(drain.refuse_if_draining(Some(owner), true).is_ok());
        let error = drain
            .refuse_if_draining(Some(worker), true)
            .expect_err("held");
        assert!(error.to_string().contains(DEPLOY_DRAINING));
        assert_eq!(drain.status().refused_total, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_setting_off_or_a_passed_deadline_holds_nothing() {
        let drain = DeployDrain::new();
        let now = Utc::now();
        let live = row(Uuid::new_v4(), now + chrono::Duration::seconds(60));
        drain.sync(Some(&live), false, now);
        assert!(!drain.is_draining());
        drain.sync(Some(&live), true, now);
        assert!(drain.is_draining());
        drain.sync(Some(&live), true, now + chrono::Duration::seconds(61));
        assert!(!drain.is_draining());
        drain.sync(Some(&live), true, now);
        drain.sync(None, true, now);
        assert!(!drain.status().draining);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_waiter_runs_when_the_hold_releases_and_is_listed_meanwhile() {
        let drain = Arc::new(DeployDrain::new());
        let now = Utc::now();
        drain.sync(
            Some(&row(Uuid::new_v4(), now + chrono::Duration::seconds(60))),
            true,
            now,
        );
        let child = Uuid::new_v4();
        let waiter = {
            let drain = Arc::clone(&drain);
            tokio::spawn(async move {
                drain
                    .wait_released(HeldKind::ChildSpawn, Some(child), true)
                    .await
            })
        };
        for _ in 0..200 {
            if !drain.status().held.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let status = drain.status();
        assert_eq!(status.held.len(), 1);
        assert_eq!(status.held[0].session_id, Some(child));
        assert_eq!(status.held[0].reason, DEPLOY_DRAINING);
        assert!(!waiter.is_finished());
        drain.sync(None, true, Utc::now());
        assert!(waiter.await.expect("join"), "the waiter waited, then ran");
        assert!(drain.status().held.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn the_hold_is_bounded_by_the_deploy_deadline_even_if_the_runner_stalls() {
        let drain = DeployDrain::new();
        let now = Utc::now();
        drain.sync(
            Some(&row(
                Uuid::new_v4(),
                now + chrono::Duration::milliseconds(150),
            )),
            true,
            now,
        );
        let started = std::time::Instant::now();
        let waited = drain
            .wait_released(HeldKind::TopologyNode, None, true)
            .await;
        assert!(waited);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!drain.is_draining());
        assert!(
            !drain
                .wait_released(HeldKind::ChildSpawn, Some(Uuid::new_v4()), true)
                .await,
            "after release nothing waits"
        );
    }
}
