//! F3 Slice 3a — restart recovery of open rotation intents (RPC-1 C7).
//!
//! A provider that died while its session was in `pending_interrupt` or
//! `writing_handoff` leaves an open rotation intent: an `entered` rotation
//! event with no terminal decision. `restore_sessions` reconciles that row
//! `Failed`/`ProcessDied` and then hands it here exactly once:
//!
//! - a successor row already exists for the intent (crash after the durable
//!   reservation): publish it once if it settled live, otherwise record
//!   `refused:<code>` and keep it `Failed`. Never a second `continued_from`.
//! - no successor row: run the Slice 1 decider once (under its spawn guard)
//!   with a bound own handoff (`/resume_handoff <path>`) or the task query.
//!
//! Recovery claims the intent durably (`recovery_claimed`) under the
//! predecessor's spawn guard before acting. Every later boot re-scans the
//! claimed intents that are still open, so a daemon that dies again before
//! the decider reserves a successor (or settles) leaves the intent
//! recoverable rather than orphaned; a closed intent is a no-op.

use super::{RotationPredecessorSource, SessionManager};
use crate::bus::DaemonEvent;
use crate::error::{DaemonError, Result};
use crate::store::RotationRequestSuccessor;
use rsi_common::types::{Session, SessionStatus};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

/// How one open intent was resolved. Returned for tests and logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) enum RecoveredRotation {
    /// The existing successor settled live and is now published.
    Published(Uuid),
    /// The existing successor is not live; the rotation is refused.
    Refused(&'static str),
    /// A successor is in flight in this process; its own decider settles it.
    InFlight(Uuid),
    /// No successor existed; the decider was launched.
    DeciderLaunched,
    /// The existing successor never settled live and holds the predecessor's
    /// transferred sandbox custody: the intent stays open and the operator is
    /// told once (#1156); custody is not moved back automatically.
    Blocked(Uuid),
}

impl SessionManager {
    /// Recover every durable open rotation intent: those among `reconciled`
    /// (rows restore just moved from a live status to `Failed`/`ProcessDied`)
    /// plus every intent an earlier boot claimed and never closed. Returns
    /// the predecessors it owns, including any whose recovery errored, so
    /// restart retry never relaunches a predecessor with an open intent.
    pub(in crate::session) async fn recover_open_rotation_intents(
        &self,
        reconciled: &[Uuid],
    ) -> HashSet<Uuid> {
        let claimed = {
            let store = self.store.lock().await;
            store.recovery_claimed_open_rotation_sessions()
        };
        let claimed = claimed.unwrap_or_else(|error| {
            tracing::warn!(%error, "Could not list claimed open rotation intents");
            Vec::new()
        });
        // #1149: an idle seat's rotation (manual or cap) is an open intent
        // from its trigger; its `Completed` predecessor is never reconciled
        // from a live status, so restart recovery takes it on first sight.
        let triggered = {
            let store = self.store.lock().await;
            store.open_completed_trigger_rotation_sessions()
        }
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "Could not list open completed-trigger rotation intents");
            Vec::new()
        });
        let mut candidates: Vec<Uuid> = reconciled.to_vec();
        for predecessor in claimed.into_iter().chain(triggered) {
            if !candidates.contains(&predecessor) {
                candidates.push(predecessor);
            }
        }
        let mut owned = HashSet::new();
        for predecessor in candidates {
            match self.recover_open_rotation_intent(predecessor).await {
                Ok(Some(outcome)) => {
                    tracing::info!(%predecessor, ?outcome, "Recovered open rotation intent after restart");
                    owned.insert(predecessor);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%predecessor, %error, "Open rotation intent recovery failed");
                    owned.insert(predecessor);
                }
            }
        }
        owned
    }

    pub(in crate::session) async fn recover_open_rotation_intent(
        &self,
        predecessor: Uuid,
    ) -> Result<Option<RecoveredRotation>> {
        let spawn_guard = super::super::spawn_single_flight::acquire_spawn_guard(predecessor).await;
        let (intent, successor) = {
            let store = self.store.lock().await;
            let Some(intent) = store.latest_open_rotation_intent(predecessor)? else {
                return Ok(None);
            };
            // Single owner: this boot holds guard(P); the durable claim keeps
            // the intent in every later boot's scan until it closes.
            store.claim_open_rotation_intent_for_recovery(predecessor, &intent)?;
            // #1153: the intent's own reservation names its successor; a row
            // another rotation reserved is never this intent's effect.
            let entered = chrono::DateTime::parse_from_rfc3339(&intent.entered_at)
                .map_err(|error| DaemonError::Store(format!("rotation intent timestamp: {error}")))?
                .with_timezone(&chrono::Utc);
            let successor = store.rotation_request_successor(
                predecessor,
                &intent.rotation_id,
                Some(entered),
            )?;
            drop(store);
            (intent, successor)
        };
        if let RotationRequestSuccessor::Ambiguous { candidates } = successor {
            tracing::warn!(%predecessor, candidates, "Open rotation intent has several possible successors; refusing it");
            drop(spawn_guard);
            self.refuse_recovered_rotation(
                predecessor,
                None,
                &intent.rotation_id,
                "successor_ambiguous",
            )
            .await?;
            return Ok(Some(RecoveredRotation::Refused("successor_ambiguous")));
        }
        if let RotationRequestSuccessor::Found {
            id: successor,
            status,
        } = successor
        {
            // Publication takes guard(P) and guard(S) in the global order.
            drop(spawn_guard);
            return self
                .settle_existing_rotation_successor(
                    predecessor,
                    successor,
                    status,
                    &intent.rotation_id,
                )
                .await
                .map(Some);
        }
        let snapshot = {
            let completed = self.completed.read().await;
            completed
                .get(&predecessor)
                .map(|completed| completed.session.clone())
        };
        let Some(snapshot) = snapshot else {
            return Ok(None);
        };
        // #1149: an automatic cap rotation is recovered only while its cap
        // request is still the seat's current one. A deferred, failed or
        // replanned request is superseded, never reinterpreted as a manual
        // rotation that skips the cap's pause, disable, cap and incarnation
        // checks.
        if intent.phase == crate::store::COMPLETED_TRIGGER_PHASE {
            let store = self.store.lock().await;
            let automatic = store
                .rotation_intent_trigger(predecessor, &intent.rotation_id)?
                .as_deref()
                == Some(super::super::context_cap::CAP_TRIGGER);
            if automatic
                && !super::super::context_cap::cap_request_is_current(
                    &store,
                    predecessor,
                    &intent.rotation_id,
                )?
            {
                store.close_open_rotation_intent(
                    predecessor,
                    &intent.rotation_id,
                    "cap_superseded",
                )?;
                drop(store);
                drop(spawn_guard);
                return Ok(Some(RecoveredRotation::Refused("cap_superseded")));
            }
        }
        // A completed-trigger intent (#1149) never wrote a handoff turn: its
        // decider resumes the task (or the cap's daemon-written handoff), and
        // its predecessor is still `Completed`, not crash-reconciled.
        let from_idle_seat = intent.phase == crate::store::COMPLETED_TRIGGER_PHASE;
        let handoff = if from_idle_seat {
            None
        } else {
            bind_recovered_own_handoff(
                &snapshot,
                intent.start_head.as_deref(),
                &intent.entered_at,
                &self.store,
            )
            .await
        };
        if let Some(ref path) = handoff {
            self.store.lock().await.conn.execute(
                "UPDATE sessions SET handoff_filepath=?1 WHERE id=?2 AND status='Failed'",
                rusqlite::params![path, predecessor.to_string()],
            )?;
            if let Some(completed) = self.completed.write().await.get_mut(&predecessor) {
                completed.session.handoff_filepath = Some(path.clone());
            }
        }
        // The decider takes the same per-predecessor spawn guard itself.
        drop(spawn_guard);
        let model_call_settlements = self.model_call_settlements.handle()?;
        let decider = Self::decide_rotation_successor(
            predecessor,
            handoff,
            Some(intent.rotation_id),
            Arc::clone(&self.active),
            Arc::clone(&self.completed),
            Arc::clone(&self.event_bus),
            Arc::clone(&self.store),
            model_call_settlements,
            self.persistence.clone(),
            self.context_rotation_enabled,
            self.socket_path.clone(),
            Arc::clone(&self.token_counter),
            self.memory_handle.clone(),
            self.retry_tx.clone(),
            Arc::clone(&self.runtime_config),
            Arc::clone(&self.spawn_coordinator),
            Arc::clone(&self.agent_tokens),
            Arc::clone(&self.spawn_epoch),
            Arc::clone(&self.agent_message_arbiter),
            self.codegraph_handle.clone(),
            self.custody_execution_runtime(),
            if from_idle_seat {
                RotationPredecessorSource::Completed
            } else {
                RotationPredecessorSource::RecoveredOpenIntent
            },
        );
        #[cfg(test)]
        if crash_before_recovery_reservation_for_test(predecessor) {
            // The daemon "dies" after claiming, before any reservation.
            drop(decider);
            return Ok(Some(RecoveredRotation::DeciderLaunched));
        }
        // The decider awaits the child's monitor; it must not block restore.
        tokio::spawn(Box::pin(decider));
        Ok(Some(RecoveredRotation::DeciderLaunched))
    }

    /// #1142 R3: recover the exact successor of an automatic cap rotation
    /// request that an earlier daemon process left unsettled. The cap record is
    /// the request's durable owner (its `Completed` predecessor has no
    /// `entered` intent for ordinary recovery to find), so the cap pass hands
    /// the successor row it found, whatever its status, to the same settlement
    /// ordinary recovery uses: a successor that settled live is published
    /// once, one that never did is refused (`refused:successor_not_live`),
    /// and no other successor is ever allocated under the request's identity.
    /// `None` when the rotation already has its terminal event.
    pub(in crate::session) async fn recover_cap_successor(
        &self,
        predecessor: Uuid,
        successor: Uuid,
        status: SessionStatus,
        rotation_id: &str,
    ) -> Result<Option<RecoveredRotation>> {
        let spawn_guard = super::super::spawn_single_flight::acquire_spawn_guard(predecessor).await;
        let closed: bool = {
            let store = self.store.lock().await;
            store.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1 AND rotation_id=?2
                   AND (event_type IN ('completed','suppressed_final_handoff')
                        OR event_type LIKE 'refused:%'))",
                rusqlite::params![predecessor.to_string(), rotation_id],
                |row| row.get(0),
            )?
        };
        // Publication takes guard(P) and guard(S) in the global order.
        drop(spawn_guard);
        if closed {
            return Ok(None);
        }
        self.settle_existing_rotation_successor(predecessor, successor, status, rotation_id)
            .await
            .map(Some)
    }

    /// Publish the exact successor `rotation_id` reserved, once: the Epic lead
    /// pointers and the global manager grant move to it and the terminal
    /// `completed` event is written in one commit, under both spawn guards.
    /// The in-memory lead and the bus follow the commit.
    ///
    /// # Errors
    /// The store's refusal (`PolicyDenied` for a lead lock, a settled rotation
    /// or a row this rotation did not reserve); nothing changes then.
    async fn publish_exact_rotation_successor(
        &self,
        predecessor: Uuid,
        successor: Uuid,
        rotation_id: &str,
    ) -> Result<()> {
        let query = {
            let store = self.store.lock().await;
            store.get_session(successor)?.map(|row| row.query)
        };
        let handoff_path = query
            .as_deref()
            .and_then(|query| query.strip_prefix("/resume_handoff "));
        let metadata = serde_json::json!({
            "handoff_filepath": handoff_path,
            "successor_id": successor,
            "query_kind": if handoff_path.is_some() { "resume_handoff" } else { "task" },
            "recovered": true,
        })
        .to_string();
        let guards =
            crate::session::RotationPublicationGuards::acquire(predecessor, successor).await;
        let published = {
            let store = self.store.lock().await;
            store.publish_rotation_successor(&guards, rotation_id, &metadata)
        };
        drop(guards);
        for epic_id in published? {
            self.set_lead_in_memory(epic_id, Some(successor)).await;
            self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
                session_id: epic_id,
                model: None,
                pinned_at: None,
                project_id: None,
                parent_id: None,
                lead_session_id: Some(Some(successor)),
                testing_needed_at: None,
                rotation_disabled_at: None,
                resolved_context_budget: None,
            });
        }
        Ok(())
    }

    /// #1158: the operator continued the exact successor a blocked rotation
    /// reserved (it holds the seat's transferred sandbox custody, so no other
    /// session can run in it) and its provider is now installed: publish it in
    /// this boot, moving the Epic lead pointers and the global grant to it.
    /// A refusal leaves the rotation open for the next recovery pass.
    ///
    /// # Errors
    /// The publication's refusal.
    pub(in crate::session) async fn publish_continued_blocked_successor(
        &self,
        blocked: &crate::store::BlockedRotation,
    ) -> Result<()> {
        if let Err(error) = self
            .publish_exact_rotation_successor(
                blocked.predecessor,
                blocked.successor,
                &blocked.rotation_id,
            )
            .await
        {
            // #1180: overlapping operator Continues of the same successor both
            // capture the blocked reservation before either takes the spawn
            // guard, and both launches succeed. The loser finds the rotation
            // already completed with this exact successor: that is its own
            // outcome, not a failure. A refusal, a supersession, or another
            // published successor stays an error.
            let settled_here = matches!(
                &error,
                DaemonError::PolicyDenied(reason) if reason.starts_with("rotation_already_settled")
            ) && self.store.lock().await.rotation_published_successor_is(
                blocked.predecessor,
                &blocked.rotation_id,
                blocked.successor,
            )?;
            if !settled_here {
                return Err(error);
            }
            tracing::info!(
                predecessor = %blocked.predecessor,
                successor = %blocked.successor,
                rotation_id = %blocked.rotation_id,
                "Blocked rotation already published by an overlapping Continue"
            );
            return Ok(());
        }
        tracing::info!(
            predecessor = %blocked.predecessor,
            successor = %blocked.successor,
            rotation_id = %blocked.rotation_id,
            "Blocked rotation published after the operator continued its successor"
        );
        Ok(())
    }

    async fn settle_existing_rotation_successor(
        &self,
        predecessor: Uuid,
        successor: Uuid,
        status: SessionStatus,
        rotation_id: &str,
    ) -> Result<RecoveredRotation> {
        match status {
            // Only a decider running in this process can hold a live row
            // after restore's crash reconciliation; it publishes or refuses.
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval => {
                Ok(RecoveredRotation::InFlight(successor))
            }
            SessionStatus::Completed | SessionStatus::Interrupted => {
                match self
                    .publish_exact_rotation_successor(predecessor, successor, rotation_id)
                    .await
                {
                    Ok(()) => Ok(RecoveredRotation::Published(successor)),
                    Err(error) => {
                        tracing::warn!(%predecessor, %successor, %error, "Recovered rotation publication refused");
                        // #1153: a row this rotation did not reserve is not
                        // failed by its refusal; it belongs to another owner.
                        let not_ours = matches!(
                            &error,
                            DaemonError::PolicyDenied(reason)
                                if reason.starts_with("rotation_successor_not_reserved")
                        );
                        let (settle, code) = if not_ours {
                            (None, "successor_not_reserved")
                        } else {
                            (Some(successor), "lead_transfer")
                        };
                        self.refuse_recovered_rotation(predecessor, settle, rotation_id, code)
                            .await?;
                        Ok(RecoveredRotation::Refused(code))
                    }
                }
            }
            // Failed, Archived, Deleted: the successor never settled live.
            _ => {
                // #1156: a successor that was bound to the predecessor's
                // sandbox custody before it died owns that custody (transfers
                // are forward-only) while the seat still sits on the
                // predecessor. Closing the intent would strand the seat with
                // nobody owning the recovery, so it stays open: recovery never
                // allocates another successor, never moves custody back, and
                // tells the operator once.
                let blocked = {
                    let store = self.store.lock().await;
                    if store.rotation_successor_holds_transferred_custody(predecessor, successor)? {
                        Some(store.record_rotation_recovery_blocked(
                            predecessor,
                            rotation_id,
                            successor,
                        )?)
                    } else {
                        None
                    }
                };
                if let Some(first) = blocked {
                    if first {
                        let message = format!(
                            "Rotation {rotation_id} of {predecessor} stopped after its sandbox moved to successor {successor}, which never started. The daemon keeps the rotation open and does not move the sandbox back (custody only moves forward). Continue {successor} to start it: it holds the sandbox, and the daemon publishes it as {predecessor}'s successor once it runs."
                        );
                        tracing::error!("{message}");
                        self.event_bus.publish(DaemonEvent::SystemMessage {
                            level: "error".into(),
                            message,
                        });
                    }
                    return Ok(RecoveredRotation::Blocked(successor));
                }
                self.refuse_recovered_rotation(
                    predecessor,
                    None,
                    rotation_id,
                    "successor_not_live",
                )
                .await?;
                Ok(RecoveredRotation::Refused("successor_not_live"))
            }
        }
    }

    /// RPC-1 C4/C7 refusal: the successor stays (or becomes) `Failed`, the
    /// predecessor keeps its lead pointers, and exactly one terminal
    /// `refused:<code>` event closes the intent.
    async fn refuse_recovered_rotation(
        &self,
        predecessor: Uuid,
        settle_successor: Option<Uuid>,
        rotation_id: &str,
        code: &'static str,
    ) -> Result<()> {
        if let Some(successor) = settle_successor {
            self.store.lock().await.set_session_terminal_status(
                successor,
                SessionStatus::Failed,
                &format!("rotation_recovery:{code}"),
            )?;
            if let Some(completed) = self.completed.write().await.get_mut(&successor) {
                completed.session.status = SessionStatus::Failed;
            }
        }
        Self::record_rotation_refusal(
            predecessor,
            Some(rotation_id),
            code,
            &self.completed,
            &self.event_bus,
            &self.persistence,
            &self.store,
        )
        .await;
        // The refusal is queued behind the persistence worker. Recovery is
        // only done once it is durable: the cap pass re-checks the rotation's
        // terminal event right after, and a slow queue must never make it
        // recover (and refuse) the same rotation twice or wait on a refusal
        // that is still in flight.
        self.persistence.barrier().await?;
        Ok(())
    }
}

#[cfg(test)]
fn recovery_crash_points() -> &'static std::sync::Mutex<HashSet<Uuid>> {
    static POINTS: std::sync::OnceLock<std::sync::Mutex<HashSet<Uuid>>> =
        std::sync::OnceLock::new();
    POINTS.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

/// Test crash point: the next recovery of `predecessor` claims the intent
/// and then stops before its decider reserves a successor.
#[cfg(test)]
pub(in crate::session) fn install_recovery_crash_before_reservation_for_test(predecessor: Uuid) {
    recovery_crash_points()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(predecessor);
}

#[cfg(test)]
fn crash_before_recovery_reservation_for_test(predecessor: Uuid) -> bool {
    recovery_crash_points()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&predecessor)
}

/// The predecessor's own handoff, bound to its worktree: exactly one
/// `thoughts/shared/handoffs/**.md` that is uncommitted there or committed
/// after the handoff turn started (`start_head..HEAD`, else commits since the
/// intent was entered). A shared, unsandboxed checkout additionally requires
/// the predecessor's own transcript to name the path. Anything ambiguous
/// binds nothing, and the decider falls back to the task query (INV-2).
async fn bind_recovered_own_handoff(
    predecessor: &Session,
    start_head: Option<&str>,
    entered_at: &str,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
) -> Option<String> {
    let root = predecessor
        .sandbox_root
        .clone()
        .unwrap_or_else(|| predecessor.working_dir.clone());
    let dirty = git_lines(
        &root,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
            "thoughts/shared/handoffs",
        ],
    )
    .await?;
    let since = format!("--since={entered_at}");
    let range = start_head.map(|head| format!("{head}..HEAD"));
    let mut log_args = vec!["log", "--format=", "--name-only", "--diff-filter=AM"];
    match range.as_deref() {
        Some(range) => log_args.push(range),
        None => log_args.extend(["HEAD", since.as_str()]),
    }
    log_args.extend(["--", "thoughts/shared/handoffs"]);
    let committed = git_lines(&root, &log_args).await?;
    let mut candidates: Vec<String> = dirty
        .iter()
        .filter_map(|line| line.get(3..))
        .map(|path| path.rsplit(" -> ").next().unwrap_or(path).trim_matches('"'))
        .chain(committed.iter().map(String::as_str))
        .map(str::trim)
        .filter(|path| super::super::rotation_coordinator::is_handoff_file(path))
        .filter(|path| root.join(path).is_file())
        .map(str::to_string)
        .collect();
    candidates.sort();
    candidates.dedup();
    if predecessor.sandbox_root.is_none() {
        let store = store.lock().await;
        candidates.retain(|path| {
            store
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1
                       AND (instr(content,?2)>0 OR instr(COALESCE(tool_input,''),?2)>0))",
                    rusqlite::params![predecessor.id.to_string(), path],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(false)
        });
    }
    match candidates.as_slice() {
        [path] => Some(path.clone()),
        [] => None,
        many => {
            tracing::warn!(
                predecessor = %predecessor.id,
                candidates = many.len(),
                "Recovered rotation found several own handoffs; binding none"
            );
            None
        }
    }
}

async fn git_lines(root: &std::path::Path, args: &[&str]) -> Option<Vec<String>> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("git")
            .kill_on_drop(true)
            .arg("-C")
            .arg(root)
            .args(args)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect(),
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::significant_drop_tightening)]
mod tests {
    use super::super::tests::{
        SeatedParent, persist_live_rotation_parent, reserve_and_bind_live_successor_for_test,
        rotation_manager, rotation_manager_on, rotation_manager_with_context_rotation,
        seat_holders, seated_live_parent, terminal_rotation_events, test_session,
    };
    use super::super::{
        CompletedSession, ROTATION_HANDOFF_PROMPT, install_rotation_child_id_for_test,
    };
    use super::*;
    use std::path::Path;

    const OWN_HANDOFF: &str = "thoughts/shared/handoffs/F3/own.md";

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A nested repository with one base commit; returns (repo, base HEAD).
    fn predecessor_repo(dir: &Path) -> (std::path::PathBuf, String) {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "f3@example.invalid"]);
        git(&repo, &["config", "user.name", "f3"]);
        std::fs::write(repo.join("README.md"), "base\n").expect("base file");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        (repo, head)
    }

    /// A predecessor that was Running with an open intent when the daemon
    /// died: exactly the durable state restart recovery must resolve.
    async fn crashed_rotating_predecessor(
        manager: &SessionManager,
        repo: &Path,
        intent: &[(&str, Option<String>)],
    ) -> anyhow::Result<Session> {
        let mut predecessor = test_session(Uuid::new_v4(), SessionStatus::Running);
        predecessor.working_dir = repo.to_path_buf();
        predecessor.query = "Objective X".into();
        let mut store = manager.store.lock().await;
        store.insert_session(&predecessor)?;
        store.publish_startup_ordinary(predecessor.id)?;
        for (phase, metadata) in intent {
            store.insert_rotation_event(
                predecessor.id,
                "rot-crash",
                phase,
                "entered",
                metadata.as_deref(),
            )?;
        }
        Ok(predecessor)
    }

    async fn continued_from_rows(manager: &SessionManager, predecessor: Uuid) -> Vec<Session> {
        let store = manager.store.lock().await;
        let mut statement = store
            .conn
            .prepare("SELECT id FROM sessions WHERE continued_from=?1")
            .expect("prepare");
        let ids: Vec<String> = statement
            .query_map([predecessor.to_string()], |row| row.get(0))
            .expect("query")
            .collect::<std::result::Result<_, _>>()
            .expect("rows");
        ids.iter()
            .map(|id| {
                store
                    .get_session(Uuid::parse_str(id).expect("uuid"))
                    .expect("read")
                    .expect("row")
            })
            .collect()
    }

    /// Run restore, release the scripted recovered child, and wait until
    /// the recovered rotation has its terminal decision.
    async fn restore_and_settle(manager: &SessionManager, predecessor: Uuid, child: Uuid) {
        install_rotation_child_id_for_test(predecessor, child);
        let _scripted =
            super::super::super::launch::install_controller_candidate_test_process(child);
        manager.restore_sessions().await.expect("restore sessions");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !manager.active.read().await.contains_key(&child) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("recovered successor launches");
        super::super::super::launch::drop_controller_candidate_test_stream(child);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let settled = !terminal_rotation_events(&*manager.store.lock().await, predecessor)
                    .expect("terminal events")
                    .is_empty();
                if settled {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("recovered rotation reaches a terminal decision");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_rotation_intent_survives_restart_with_exactly_one_successor() -> anyhow::Result<()>
    {
        let (manager, dir) = rotation_manager();
        let (repo, start_head) = predecessor_repo(dir.path());
        let predecessor = crashed_rotating_predecessor(
            &manager,
            &repo,
            &[
                ("pending_interrupt", None),
                (
                    "writing_handoff",
                    Some(serde_json::json!({ "start_head": start_head }).to_string()),
                ),
            ],
        )
        .await?;
        // The handoff turn committed its own handoff before the crash.
        std::fs::create_dir_all(repo.join("thoughts/shared/handoffs/F3"))?;
        std::fs::write(repo.join(OWN_HANDOFF), "# own handoff\n")?;
        git(&repo, &["add", OWN_HANDOFF]);
        git(&repo, &["commit", "-q", "-m", "handoff"]);
        // The handoff turn's own persisted Write names the path.
        manager
            .store
            .lock()
            .await
            .insert_event(&rsi_common::types::ConversationEvent {
                id: 0,
                session_id: predecessor.id,
                sequence: 0,
                event_type: rsi_common::types::EventType::ToolUse,
                role: None,
                created_at: chrono::Utc::now(),
                content: String::new(),
                tool_name: Some("Write".into()),
                tool_input: Some(Box::new(serde_json::json!({ "file_path": OWN_HANDOFF }))),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })?;

        let child = Uuid::new_v4();
        restore_and_settle(&manager, predecessor.id, child).await;
        // A second restore pass is a no-op.
        manager.restore_sessions().await?;

        let successors = continued_from_rows(&manager, predecessor.id).await;
        assert_eq!(
            successors.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![child]
        );
        assert_eq!(
            successors[0].query,
            format!("/resume_handoff {OWN_HANDOFF}")
        );
        let store = manager.store.lock().await;
        let row = store
            .get_session(predecessor.id)?
            .ok_or_else(|| anyhow::anyhow!("predecessor missing"))?;
        assert_eq!(row.handoff_filepath.as_deref(), Some(OWN_HANDOFF));
        let events = terminal_rotation_events(&store, predecessor.id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "completed");
        assert_eq!(
            store.find_published_rotation_successor(predecessor.id)?,
            Some(child)
        );
        Ok(())
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_rotation_intent_without_handoff_restores_task_query() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (repo, _) = predecessor_repo(dir.path());
        let predecessor =
            crashed_rotating_predecessor(&manager, &repo, &[("pending_interrupt", None)]).await?;

        let child = Uuid::new_v4();
        restore_and_settle(&manager, predecessor.id, child).await;
        manager.restore_sessions().await?;

        let successors = continued_from_rows(&manager, predecessor.id).await;
        assert_eq!(
            successors.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![child]
        );
        assert!(successors[0].query.starts_with("Objective X"));
        assert!(
            successors[0]
                .query
                .contains(&format!("Rotation continuation of {}", predecessor.id))
        );
        assert_ne!(successors[0].query.trim(), ROTATION_HANDOFF_PROMPT);
        let store = manager.store.lock().await;
        let events = terminal_rotation_events(&store, predecessor.id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "completed");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(events[0].1.as_deref().unwrap_or("{}"))?["query_kind"],
            "task"
        );
        Ok(())
    }

    /// Crash after the durable reservation but before publication: the
    /// reserved successor died with the daemon, so recovery refuses and
    /// never creates a second `continued_from` row.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_rotation_intent_with_unpublished_successor_refuses_without_second_row()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (repo, _) = predecessor_repo(dir.path());
        let predecessor =
            crashed_rotating_predecessor(&manager, &repo, &[("writing_handoff", None)]).await?;
        let mut reserved = test_session(Uuid::new_v4(), SessionStatus::Running);
        reserved.working_dir = repo.clone();
        reserved.continued_from = Some(predecessor.id);
        reserved.rotation_depth = 1;
        manager.store.lock().await.insert_session(&reserved)?;

        manager.restore_sessions().await?;
        manager.restore_sessions().await?;

        let successors = continued_from_rows(&manager, predecessor.id).await;
        assert_eq!(
            successors
                .iter()
                .map(|row| (row.id, row.status))
                .collect::<Vec<_>>(),
            vec![(reserved.id, SessionStatus::Failed)]
        );
        let store = manager.store.lock().await;
        let events = terminal_rotation_events(&store, predecessor.id)?;
        assert_eq!(
            events
                .iter()
                .map(|(kind, _)| kind.as_str())
                .collect::<Vec<_>>(),
            vec!["refused:successor_not_live"]
        );
        assert_eq!(
            store.find_published_rotation_successor(predecessor.id)?,
            None
        );
        Ok(())
    }

    /// An idle (`Completed`) seat whose rotation was triggered: the state a
    /// crash right after the trigger leaves. The intent is the one the
    /// trigger funnel writes (#1149).
    async fn triggered_idle_predecessor(
        manager: &SessionManager,
        repo: &Path,
        rotation_id: &str,
    ) -> anyhow::Result<Session> {
        let mut predecessor = test_session(Uuid::new_v4(), SessionStatus::Completed);
        predecessor.working_dir = repo.to_path_buf();
        predecessor.query = "Objective X".into();
        let mut store = manager.store.lock().await;
        store.insert_session(&predecessor)?;
        store.publish_startup_ordinary(predecessor.id)?;
        assert!(store.record_completed_trigger_intent(
            predecessor.id,
            rotation_id,
            "manual_triggered"
        )?);
        store.insert_rotation_event(
            predecessor.id,
            rotation_id,
            "completed",
            "manual_triggered",
            Some(&serde_json::json!({ "manual": true }).to_string()),
        )?;
        Ok(predecessor)
    }

    /// #1149: the daemon died after an idle seat's rotation trigger was
    /// logged and before any successor was reserved. The trigger left an open
    /// intent, so restart recovery owns the rotation: exactly one successor,
    /// published once, resumed from the task, and a second restore is a no-op.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_seat_rotation_crash_after_trigger_before_reservation_is_recovered()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (repo, _) = predecessor_repo(dir.path());
        let predecessor = triggered_idle_predecessor(&manager, &repo, "rot-idle").await?;

        let child = Uuid::new_v4();
        restore_and_settle(&manager, predecessor.id, child).await;
        manager.restore_sessions().await?;

        let successors = continued_from_rows(&manager, predecessor.id).await;
        assert_eq!(
            successors.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![child]
        );
        assert!(successors[0].query.starts_with("Objective X"));
        let store = manager.store.lock().await;
        let events = terminal_rotation_events(&store, predecessor.id)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "completed");
        assert_eq!(
            store.find_published_rotation_successor(predecessor.id)?,
            Some(child)
        );
        assert!(store.open_completed_trigger_rotation_sessions()?.is_empty());
        Ok(())
    }

    /// #1149: the daemon died after the idle seat's successor was reserved and
    /// finished its first turn, before publication. Recovery publishes that
    /// exact successor once and allocates no second row.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_seat_rotation_crash_after_reservation_publishes_the_reserved_successor_once()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (repo, _) = predecessor_repo(dir.path());
        let predecessor = triggered_idle_predecessor(&manager, &repo, "rot-idle").await?;
        let mut reserved = test_session(Uuid::new_v4(), SessionStatus::Completed);
        reserved.working_dir = repo.clone();
        reserved.continued_from = Some(predecessor.id);
        reserved.rotation_depth = 1;
        {
            let store = manager.store.lock().await;
            store.insert_session(&reserved)?;
            store.insert_rotation_event(
                predecessor.id,
                "rot-idle",
                "reserved",
                "successor_reserved",
                Some(&serde_json::json!({ "successor_id": reserved.id }).to_string()),
            )?;
        }

        manager.restore_sessions().await?;
        manager.restore_sessions().await?;

        let successors = continued_from_rows(&manager, predecessor.id).await;
        assert_eq!(
            successors.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![reserved.id]
        );
        let store = manager.store.lock().await;
        let events = terminal_rotation_events(&store, predecessor.id)?;
        assert_eq!(
            events
                .iter()
                .map(|(kind, _)| kind.as_str())
                .collect::<Vec<_>>(),
            vec!["completed"]
        );
        assert_eq!(
            store.find_published_rotation_successor(predecessor.id)?,
            Some(reserved.id)
        );
        Ok(())
    }

    /// #1149: the live manual trigger of an idle seat records the intent
    /// before its decider runs, once, and the rotation still completes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn manual_trigger_of_an_idle_seat_records_its_intent() -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager_with_context_rotation(true);
        let (repo, _) = predecessor_repo(dir.path());
        let mut predecessor = test_session(Uuid::new_v4(), SessionStatus::Completed);
        predecessor.working_dir = repo.clone();
        predecessor.query = "Objective X".into();
        {
            let mut store = manager.store.lock().await;
            store.insert_session(&predecessor)?;
            store.publish_startup_ordinary(predecessor.id)?;
        }
        manager.completed.write().await.insert(
            predecessor.id,
            CompletedSession::for_test(predecessor.clone()),
        );
        let child = Uuid::new_v4();
        install_rotation_child_id_for_test(predecessor.id, child);
        let _scripted =
            super::super::super::launch::install_controller_candidate_test_process(child);

        manager.trigger_rotation(predecessor.id).await?;
        {
            let store = manager.store.lock().await;
            let intents: Vec<(String, String)> = store
                .conn
                .prepare(
                    "SELECT phase, json_extract(metadata,'$.trigger') FROM rotation_events
                     WHERE session_id=?1 AND event_type='entered'",
                )?
                .query_map([predecessor.id.to_string()], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            assert_eq!(
                intents,
                vec![(
                    "completed_trigger".to_string(),
                    "manual_triggered".to_string()
                )],
                "the trigger recorded exactly one durable intent"
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !manager.active.read().await.contains_key(&child) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("successor launches");
        super::super::super::launch::drop_controller_candidate_test_stream(child);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if !terminal_rotation_events(&*manager.store.lock().await, predecessor.id)?
                    .is_empty()
                {
                    break anyhow::Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("rotation reaches a terminal decision")?;
        let store = manager.store.lock().await;
        assert_eq!(
            store.find_published_rotation_successor(predecessor.id)?,
            Some(child)
        );
        assert!(store.open_completed_trigger_rotation_sessions()?.is_empty());
        Ok(())
    }

    fn custody_owner_and_projection(
        store: &crate::store::Store,
        custody_id: Uuid,
        predecessor: Uuid,
    ) -> (String, String) {
        let owner: String = store
            .conn
            .query_row(
                "SELECT owner_session_id FROM sandbox_custody_roots WHERE custody_id=?1",
                [custody_id.to_string()],
                |row| row.get(0),
            )
            .expect("custody owner");
        let projection: String = store
            .conn
            .query_row(
                "SELECT execution_state FROM session_execution_projections WHERE session_id=?1",
                [predecessor.to_string()],
                |row| row.get(0),
            )
            .expect("predecessor projection");
        (owner, projection)
    }

    fn system_errors(rx: &mut tokio::sync::broadcast::Receiver<Arc<DaemonEvent>>) -> usize {
        let mut errors = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(&*event, DaemonEvent::SystemMessage { level, .. } if level == "error") {
                errors += 1;
            }
        }
        errors
    }

    /// #1156: the daemon died after a sandboxed seat's rotation successor was
    /// reserved and bound to the seat's transferred sandbox custody, before
    /// publication. Restart fails the child; custody moves only forward, so
    /// recovery neither closes the intent (that would strand the seat with the
    /// sandbox on a dead child) nor moves custody back nor allocates another
    /// successor: it keeps the intent open and tells the operator once, across
    /// restarts.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn crash_after_custody_bind_keeps_the_intent_open_and_escalates_once()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager_with_context_rotation(true);
        let fixture = persist_live_rotation_parent(&manager, dir.path(), None).await;
        let parent = fixture.parent.id;
        {
            let store = manager.store.lock().await;
            assert!(store.record_completed_trigger_intent(
                parent,
                "rot-bound",
                "manual_triggered"
            )?);
        }
        let child =
            reserve_and_bind_live_successor_for_test(&manager, &fixture, "rot-bound").await?;
        {
            let store = manager.store.lock().await;
            assert_eq!(
                custody_owner_and_projection(&store, fixture.custody_id, parent),
                (child.id.to_string(), "historical_transferred".to_string()),
                "binding moved the sandbox to the successor"
            );
        }
        drop(manager); // the daemon dies

        for boot in 1..=2 {
            let restarted = rotation_manager_on(dir.path(), true);
            let mut events = restarted.event_bus.subscribe();
            restarted.restore_sessions().await?;
            let store = restarted.store.lock().await;
            assert_eq!(
                store.get_session(child.id)?.expect("child").status,
                SessionStatus::Failed,
                "boot {boot}: startup fails the child that never started"
            );
            assert!(
                terminal_rotation_events(&store, parent)?.is_empty(),
                "boot {boot}: the intent is not closed"
            );
            assert_eq!(
                store.open_completed_trigger_rotation_sessions()?,
                vec![parent]
            );
            assert_eq!(
                custody_owner_and_projection(&store, fixture.custody_id, parent),
                (child.id.to_string(), "historical_transferred".to_string()),
                "boot {boot}: custody is not moved back"
            );
            let blocked: i64 = store.conn.query_row(
                "SELECT COUNT(*) FROM rotation_events WHERE session_id=?1 AND event_type='recovery_blocked'",
                [parent.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(blocked, 1, "boot {boot}: blocked is recorded once");
            let successors: i64 = store.conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE continued_from=?1",
                [parent.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(successors, 1, "boot {boot}: no second successor");
            drop(store);
            assert_eq!(
                system_errors(&mut events),
                usize::from(boot == 1),
                "boot {boot}: the operator is told once, on the first boot"
            );
        }
        Ok(())
    }

    /// #1153: the open intent reserved A; another rotation of the same
    /// predecessor reserved a newer, `Completed` B. Recovery acts on A (the
    /// intent's own reservation) and never publishes or fails B.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_rotation_intent_recovers_its_own_reservation_not_a_newer_foreign_row()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (repo, _) = predecessor_repo(dir.path());
        let predecessor =
            crashed_rotating_predecessor(&manager, &repo, &[("writing_handoff", None)]).await?;
        let mut reserved = test_session(Uuid::new_v4(), SessionStatus::Running);
        reserved.working_dir = repo.clone();
        reserved.continued_from = Some(predecessor.id);
        reserved.rotation_depth = 1;
        let mut foreign = test_session(Uuid::new_v4(), SessionStatus::Completed);
        foreign.working_dir = repo.clone();
        foreign.continued_from = Some(predecessor.id);
        foreign.rotation_depth = 1;
        foreign.created_at = chrono::Utc::now() + chrono::Duration::seconds(30);
        {
            let store = manager.store.lock().await;
            store.insert_session(&reserved)?;
            store.insert_session(&foreign)?;
            // The intent's own reservation is its newest event, so it is the
            // latest rotation the recovery scan considers.
            for (rotation_id, id) in [("manual-rotation", foreign.id), ("rot-crash", reserved.id)] {
                store.insert_rotation_event(
                    predecessor.id,
                    rotation_id,
                    "reserved",
                    "successor_reserved",
                    Some(&serde_json::json!({ "successor_id": id }).to_string()),
                )?;
            }
        }

        manager.restore_sessions().await?;

        let store = manager.store.lock().await;
        assert_eq!(
            terminal_rotation_events(&store, predecessor.id)?
                .iter()
                .map(|(kind, _)| kind.as_str())
                .collect::<Vec<_>>(),
            vec!["refused:successor_not_live"],
            "the intent is refused on its own reservation"
        );
        assert_eq!(
            store.get_session(foreign.id)?.expect("foreign").status,
            SessionStatus::Completed,
            "the other rotation's successor is not failed"
        );
        assert_eq!(
            store.find_published_rotation_successor(predecessor.id)?,
            None,
            "nothing was published"
        );
        Ok(())
    }

    /// Review round 2 `rotation_recovery_second_crash`: boot 1 reconciles P
    /// `Failed` and claims its open intent, then the daemon dies before the
    /// recovery decider reserves a successor. Boot 2 no longer sees P as
    /// crash-reconciled, yet recovers the claimed intent: exactly one
    /// successor is published and P is never relaunched by restart retry.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn claimed_open_rotation_intent_survives_a_second_crash_before_reservation()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager();
        let (repo, _) = predecessor_repo(dir.path());
        let predecessor =
            crashed_rotating_predecessor(&manager, &repo, &[("pending_interrupt", None)]).await?;
        {
            let store = manager.store.lock().await;
            store.conn.execute(
                "UPDATE sessions SET max_retries=3, retry_attempt=0 WHERE id=?1",
                [predecessor.id.to_string()],
            )?;
        }

        // Boot 1: P is reconciled Failed, its intent claimed, then the daemon
        // dies before any reservation.
        install_recovery_crash_before_reservation_for_test(predecessor.id);
        manager.restore_sessions().await?;
        {
            let store = manager.store.lock().await;
            let row = store
                .get_session(predecessor.id)?
                .ok_or_else(|| anyhow::anyhow!("predecessor missing"))?;
            assert_eq!(row.status, SessionStatus::Failed);
            assert_eq!(
                store
                    .latest_open_rotation_intent(predecessor.id)?
                    .map(|intent| intent.rotation_id),
                Some("rot-crash".to_string())
            );
            assert_eq!(
                store.recovery_claimed_open_rotation_sessions()?,
                vec![predecessor.id]
            );
        }
        assert!(
            continued_from_rows(&manager, predecessor.id)
                .await
                .is_empty()
        );
        drop(manager);

        // Boot 2: a fresh daemon on the same database recovers the claim.
        let restarted = rotation_manager_on(dir.path(), false);
        let child = Uuid::new_v4();
        restore_and_settle(&restarted, predecessor.id, child).await;
        // Boot 3 is a no-op.
        restarted.restore_sessions().await?;

        let successors = continued_from_rows(&restarted, predecessor.id).await;
        assert_eq!(
            successors.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![child]
        );
        let store = restarted.store.lock().await;
        let events = terminal_rotation_events(&store, predecessor.id)?;
        assert_eq!(
            events
                .iter()
                .map(|(kind, _)| kind.as_str())
                .collect::<Vec<_>>(),
            vec!["completed"]
        );
        assert_eq!(
            store.find_published_rotation_successor(predecessor.id)?,
            Some(child)
        );
        assert_eq!(
            store
                .get_session(predecessor.id)?
                .and_then(|row| row.retry_attempt)
                .unwrap_or(0),
            0,
            "restart retry never relaunched the predecessor"
        );
        Ok(())
    }
    /// Crash state of a sandboxed coordinator seat: the manually triggered
    /// rotation `rot-bound` reserved a successor and bound the seat's custody
    /// to it; the daemon died before publication. `pre_launch` models the
    /// crash before the provider ever reported a thread. Also reserves a newer
    /// successor under another rotation, which recovery must not touch.
    async fn crashed_bound_seat(
        manager: &SessionManager,
        root: &Path,
        pre_launch: bool,
    ) -> anyhow::Result<(SeatedParent, Session, Session)> {
        let seat = seated_live_parent(manager, root).await?;
        let parent = seat.fixture.parent.id;
        let mut foreign = test_session(Uuid::new_v4(), SessionStatus::Completed);
        foreign.working_dir = seat.fixture.repo.clone();
        foreign.continued_from = Some(parent);
        foreign.rotation_depth = 1;
        foreign.created_at = chrono::Utc::now() + chrono::Duration::seconds(30);
        {
            let store = manager.store.lock().await;
            store.insert_session(&foreign)?;
            store.insert_rotation_event(
                parent,
                "other-rotation",
                "reserved",
                "successor_reserved",
                Some(&serde_json::json!({ "successor_id": foreign.id }).to_string()),
            )?;
            store.record_completed_trigger_intent(parent, "rot-bound", "manual_triggered")?;
        }
        let failed =
            reserve_and_bind_live_successor_for_test(manager, &seat.fixture, "rot-bound").await?;
        if pre_launch {
            // The reservation never carries a provider thread; it is set by
            // the first provider event, which this crash precedes.
            manager.store.lock().await.conn.execute(
                "UPDATE sessions SET claude_session_id=NULL WHERE id=?1",
                [failed.id.to_string()],
            )?;
        }
        Ok((seat, failed, foreign))
    }

    /// #1158: a blocked rotation survives restarts with the seat and its
    /// grant untouched, names its own reservation (never a newer
    /// `continued_from` row), and refuses to publish any other row.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn blocked_rotation_keeps_the_seat_across_restart_and_names_the_exact_reservation()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager_with_context_rotation(true);
        let (seat, failed, foreign) = crashed_bound_seat(&manager, dir.path(), true).await?;
        let parent = seat.fixture.parent.id;
        drop(manager); // the daemon dies

        for boot in 1..=2 {
            let restarted = rotation_manager_on(dir.path(), true);
            restarted.restore_sessions().await?;
            let store = restarted.store.lock().await;
            assert_eq!(
                seat_holders(&store, seat.epic),
                (Some(parent), Some(parent)),
                "boot {boot}: the Epic lead and the grant stay on the seat"
            );
            assert_eq!(
                custody_owner_and_projection(&store, seat.fixture.custody_id, parent),
                (failed.id.to_string(), "historical_transferred".to_string()),
                "boot {boot}: custody is the failed successor's (forward only)"
            );
            assert_eq!(
                store.blocked_rotation_of(parent)?,
                Some(crate::store::BlockedRotation {
                    predecessor: parent,
                    rotation_id: "rot-bound".into(),
                    successor: failed.id,
                }),
                "boot {boot}: the block names the intent's own reservation"
            );
            assert_eq!(store.blocked_rotation_of_successor(foreign.id)?, None);
            drop(store);
            // A row another rotation reserved is never published under this one.
            let refused = restarted
                .publish_continued_blocked_successor(&crate::store::BlockedRotation {
                    predecessor: parent,
                    rotation_id: "rot-bound".into(),
                    successor: foreign.id,
                })
                .await;
            assert!(
                matches!(refused, Err(DaemonError::PolicyDenied(_))),
                "boot {boot}: {refused:?}"
            );
            let store = restarted.store.lock().await;
            assert_eq!(
                seat_holders(&store, seat.epic),
                (Some(parent), Some(parent)),
                "boot {boot}: a refused publication moves nothing"
            );
            assert!(terminal_rotation_events(&store, parent)?.is_empty());
        }
        Ok(())
    }

    /// Run the operator Continue of a blocked successor after a restart and
    /// assert the seat: the successor started (a scripted provider), is
    /// published in this boot, and the Epic lead pointer and global grant
    /// moved to it in that one commit, with custody still its own.
    async fn operator_continue_publishes_the_blocked_successor(
        pre_launch: bool,
    ) -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager_with_context_rotation(true);
        let (seat, failed, foreign) = crashed_bound_seat(&manager, dir.path(), pre_launch).await?;
        let parent = seat.fixture.parent.id;
        drop(manager); // the daemon dies

        let restarted = rotation_manager_on(dir.path(), true);
        restarted.restore_sessions().await?;
        {
            let store = restarted.store.lock().await;
            assert_eq!(
                store
                    .get_session(failed.id)?
                    .expect("failed")
                    .claude_session_id
                    .is_none(),
                pre_launch
            );
            assert!(store.blocked_rotation_of_successor(failed.id)?.is_some());
            assert_eq!(
                seat_holders(&store, seat.epic),
                (Some(parent), Some(parent))
            );
        }

        let _scripted =
            super::super::super::launch::install_controller_candidate_test_process(failed.id);
        restarted
            .continue_session_operator(failed.id, "start the blocked successor".into())
            .await?;
        super::super::super::launch::drop_controller_candidate_test_stream(failed.id);

        let store = restarted.store.lock().await;
        assert_eq!(
            seat_holders(&store, seat.epic),
            (Some(failed.id), Some(failed.id)),
            "the Epic lead and the grant moved to the exact reserved successor"
        );
        let events = terminal_rotation_events(&store, parent)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "completed");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(events[0].1.as_deref().unwrap_or("{}"))?["successor_id"],
            serde_json::json!(failed.id)
        );
        assert_eq!(
            store.find_published_rotation_successor(parent)?,
            Some(failed.id),
            "the exact reservation, not the newer continued_from row"
        );
        assert_eq!(
            custody_owner_and_projection(&store, seat.fixture.custody_id, parent),
            (failed.id.to_string(), "historical_transferred".to_string()),
            "one writer: the successor owned the sandbox throughout"
        );
        assert_eq!(
            store.get_session(foreign.id)?.expect("foreign").status,
            SessionStatus::Completed,
            "the other rotation's successor is untouched"
        );
        assert_eq!(store.blocked_rotation_of(parent)?, None);
        let successors: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE continued_from=?1",
            [parent.to_string()],
            |row| row.get(0),
        )?;
        assert_eq!(successors, 2, "no third successor was allocated");
        Ok(())
    }

    /// #1158: the operator's Continue of a successor that failed before it
    /// ever started (no provider thread, no retry budget) starts it in a new
    /// thread and publishes it in this boot.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn operator_continue_of_a_pre_launch_blocked_successor_starts_and_publishes_in_this_boot()
    -> anyhow::Result<()> {
        operator_continue_publishes_the_blocked_successor(true).await
    }

    /// #1158: a continuation that succeeds publishes the exact reservation in
    /// the same boot, not at the next restart's recovery scan.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn operator_continue_of_a_blocked_successor_with_a_provider_thread_publishes_in_this_boot()
    -> anyhow::Result<()> {
        operator_continue_publishes_the_blocked_successor(false).await
    }

    /// #1180: the operator's Continue published a successor that had never
    /// started, and the daemon died before the provider reported a thread. The
    /// restored successor is `Failed` with no thread and its rotation closed;
    /// the operator's next Continue of that exact successor must still start
    /// it (custody never moves back), keeping one usable seat: the Epic lead
    /// and the grant stay on it, with a single completion.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn published_never_started_successor_restarts_after_a_crash_before_its_first_provider_event()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager_with_context_rotation(true);
        let (seat, failed, foreign) = crashed_bound_seat(&manager, dir.path(), true).await?;
        let parent = seat.fixture.parent.id;
        // A real rotation successor is a direct child of the Epic it leads
        // (restore clears a lead pointer to a row that is not); the shared
        // fixture does not set it.
        manager.store.lock().await.conn.execute(
            "UPDATE sessions SET parent_id=?1 WHERE id=?2",
            [seat.epic.to_string(), failed.id.to_string()],
        )?;
        drop(manager); // the daemon dies before publication

        // First repair: the Continue starts the successor and publishes it.
        let repaired = rotation_manager_on(dir.path(), true);
        repaired.restore_sessions().await?;
        let _scripted =
            super::super::super::launch::install_controller_candidate_test_process(failed.id);
        repaired
            .continue_session_operator(failed.id, "start the blocked successor".into())
            .await?;
        super::super::super::launch::drop_controller_candidate_test_stream(failed.id);
        {
            let store = repaired.store.lock().await;
            assert_eq!(
                seat_holders(&store, seat.epic),
                (Some(failed.id), Some(failed.id))
            );
            assert_eq!(store.blocked_rotation_of(parent)?, None);
        }
        // The scripted provider never reports a thread. Let the dropped
        // stream's monitor settle, then leave the row as a daemon that died
        // while the successor was live leaves it.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let status = repaired
                    .store
                    .lock()
                    .await
                    .get_session(failed.id)?
                    .expect("successor")
                    .status;
                if !matches!(status, SessionStatus::Starting | SessionStatus::Running) {
                    break anyhow::Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the dropped stream settles")?;
        repaired.store.lock().await.conn.execute(
            "UPDATE sessions SET status='Running', claude_session_id=NULL WHERE id=?1",
            [failed.id.to_string()],
        )?;
        drop(repaired); // the daemon dies after publication, before a provider event

        for boot in 1..=2 {
            let restarted = rotation_manager_on(dir.path(), true);
            restarted.restore_sessions().await?;
            let store = restarted.store.lock().await;
            let row = store.get_session(failed.id)?.expect("successor");
            assert_eq!(row.status, SessionStatus::Failed, "boot {boot}");
            assert!(row.claude_session_id.is_none(), "boot {boot}");
            assert_eq!(
                seat_holders(&store, seat.epic),
                (Some(failed.id), Some(failed.id)),
                "boot {boot}: the published seat is the successor's"
            );
            assert_eq!(
                terminal_rotation_events(&store, parent)?.len(),
                1,
                "boot {boot}: one completion"
            );
            assert_eq!(store.blocked_rotation_of_successor(failed.id)?, None);
            assert_eq!(
                store.published_blocked_rotation_of_successor(failed.id)?,
                Some(crate::store::BlockedRotation {
                    predecessor: parent,
                    rotation_id: "rot-bound".into(),
                    successor: failed.id,
                }),
                "boot {boot}: the operator's bootstrap path survives publication"
            );
            assert_eq!(
                store.published_blocked_rotation_of_successor(foreign.id)?,
                None,
                "boot {boot}: only the exact published successor"
            );
            drop(store);
            if boot == 1 {
                continue;
            }
            let _scripted =
                super::super::super::launch::install_controller_candidate_test_process(failed.id);
            restarted
                .continue_session_operator(failed.id, "start the published successor again".into())
                .await?;
            super::super::super::launch::drop_controller_candidate_test_stream(failed.id);
            let store = restarted.store.lock().await;
            assert_eq!(
                seat_holders(&store, seat.epic),
                (Some(failed.id), Some(failed.id)),
                "the seat holders are the successor after the second Continue"
            );
            assert_eq!(terminal_rotation_events(&store, parent)?.len(), 1);
            assert_eq!(
                custody_owner_and_projection(&store, seat.fixture.custody_id, parent),
                (failed.id.to_string(), "historical_transferred".to_string()),
                "custody stayed forward-only on the successor"
            );
            assert_eq!(
                store.find_published_rotation_successor(parent)?,
                Some(failed.id)
            );
            let successors: i64 = store.conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE continued_from=?1",
                [parent.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(successors, 2, "no third successor was allocated");
        }
        Ok(())
    }

    /// #1180: two operator Continues of the same blocked successor capture the
    /// same blocked reservation before either publishes. The loser finds the
    /// rotation already completed with its exact successor, which is its own
    /// outcome (success, one completion); every other settlement still
    /// refuses.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlapping_continues_of_a_blocked_successor_both_succeed_with_one_completion()
    -> anyhow::Result<()> {
        let (manager, dir) = rotation_manager_with_context_rotation(true);
        let (seat, failed, foreign) = crashed_bound_seat(&manager, dir.path(), true).await?;
        let parent = seat.fixture.parent.id;
        drop(manager); // the daemon dies

        let restarted = rotation_manager_on(dir.path(), true);
        restarted.restore_sessions().await?;
        let captured = restarted
            .store
            .lock()
            .await
            .blocked_rotation_of_successor(failed.id)?
            .expect("both Continues capture the blocked reservation");

        // The first Continue starts the successor and publishes.
        let _scripted =
            super::super::super::launch::install_controller_candidate_test_process(failed.id);
        restarted
            .continue_session_operator(failed.id, "first continue".into())
            .await?;
        super::super::super::launch::drop_controller_candidate_test_stream(failed.id);

        // The second Continue's wrapper publishes its stale capture.
        restarted
            .publish_continued_blocked_successor(&captured)
            .await?;
        restarted
            .publish_continued_blocked_successor(&captured)
            .await?;

        // A different successor, or a different rotation, is still refused.
        for mismatch in [
            crate::store::BlockedRotation {
                successor: foreign.id,
                ..captured.clone()
            },
            crate::store::BlockedRotation {
                rotation_id: "other-rotation".into(),
                ..captured.clone()
            },
        ] {
            let refused = restarted
                .publish_continued_blocked_successor(&mismatch)
                .await;
            assert!(
                matches!(refused, Err(DaemonError::PolicyDenied(_))),
                "{mismatch:?}: {refused:?}"
            );
        }

        let store = restarted.store.lock().await;
        assert_eq!(
            seat_holders(&store, seat.epic),
            (Some(failed.id), Some(failed.id))
        );
        let events = terminal_rotation_events(&store, parent)?;
        assert_eq!(events.len(), 1, "exactly one completion");
        assert_eq!(events[0].0, "completed");
        assert_eq!(
            store.find_published_rotation_successor(parent)?,
            Some(failed.id)
        );
        Ok(())
    }
}
