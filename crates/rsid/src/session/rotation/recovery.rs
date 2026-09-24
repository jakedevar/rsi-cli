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
        let mut candidates: Vec<Uuid> = reconciled.to_vec();
        for predecessor in claimed {
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
            let successor = intent_successor(&store, predecessor, &intent.entered_at)?;
            drop(store);
            (intent, successor)
        };
        if let Some((successor, status)) = successor {
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
        let handoff = bind_recovered_own_handoff(
            &snapshot,
            intent.start_head.as_deref(),
            &intent.entered_at,
            &self.store,
        )
        .await;
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
            RotationPredecessorSource::RecoveredOpenIntent,
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
                    crate::session::RotationPublicationGuards::acquire(predecessor, successor)
                        .await;
                let published = {
                    let store = self.store.lock().await;
                    store.publish_rotation_successor(&guards, rotation_id, &metadata)
                };
                drop(guards);
                match published {
                    Ok(epics) => {
                        for epic_id in epics {
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
                        Ok(RecoveredRotation::Published(successor))
                    }
                    Err(error) => {
                        tracing::warn!(%predecessor, %successor, %error, "Recovered rotation publication refused");
                        self.refuse_recovered_rotation(
                            predecessor,
                            Some(successor),
                            rotation_id,
                            "lead_transfer",
                        )
                        .await?;
                        Ok(RecoveredRotation::Refused("lead_transfer"))
                    }
                }
            }
            // Failed, Archived, Deleted: the successor never settled live.
            _ => {
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
            self.store
                .lock()
                .await
                .update_session_status(successor, SessionStatus::Failed)?;
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

/// The newest `continued_from = predecessor` row created at or after the
/// intent was entered. Older rows belong to earlier, already-settled
/// rotations and are never reused.
fn intent_successor(
    store: &crate::store::Store,
    predecessor: Uuid,
    entered_at: &str,
) -> Result<Option<(Uuid, SessionStatus)>> {
    let entered = chrono::DateTime::parse_from_rfc3339(entered_at)
        .map_err(|error| DaemonError::Store(format!("rotation intent timestamp: {error}")))?;
    let mut statement = store.conn.prepare(
        "SELECT id, status, created_at FROM sessions WHERE continued_from=?1
         ORDER BY created_at DESC",
    )?;
    let rows = statement
        .query_map([predecessor.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, status, created_at) in rows {
        let created = crate::store::parse_timestamp(&created_at).map_err(DaemonError::Store)?;
        if created < entered {
            continue;
        }
        let id = Uuid::parse_str(&id).map_err(|error| DaemonError::Store(error.to_string()))?;
        let status: SessionStatus = serde_json::from_value(serde_json::Value::String(status))?;
        return Ok(Some((id, status)));
    }
    Ok(None)
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
        rotation_manager, rotation_manager_on, terminal_rotation_events, test_session,
    };
    use super::super::{ROTATION_HANDOFF_PROMPT, install_rotation_child_id_for_test};
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

    /// Review round 2 `rotation_recovery_second_crash`: boot 1 reconciles P
    /// `Failed` and claims its open intent, then the daemon dies before the
    /// recovery decider reserves a successor. Boot 2 no longer sees P as
    /// crash-reconciled, yet recovers the claimed intent: exactly one
    /// successor is published and P is never relaunched by restart retry.
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
}
