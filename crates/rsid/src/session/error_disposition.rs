//! Bounded, daemon-owned disposition of errored leaf sessions (#3427).
use super::SessionManager;
use crate::bus::DaemonEvent;
use crate::error::Result;
use crate::sandbox::git_worktree as git;
use crate::store::error_disposition::{Disposition, ErrorFacts, PAGE_SIZE, WorkEvidence};
use chrono::Utc;
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Log one summary even when a store or preservation operation fails.
struct PassSummary {
    started: Instant,
    scanned: usize,
    counts: [usize; 6],
    store_lock_ms: u128,
    completed: bool,
}

impl Drop for PassSummary {
    fn drop(&mut self) {
        tracing::info!(
            scanned = self.scanned,
            held = self.counts[0],
            deleted = self.counts[1],
            resumed = self.counts[2],
            archived = self.counts[3],
            superseded = self.counts[4],
            notices = self.counts[5],
            store_lock_ms = self.store_lock_ms,
            elapsed_ms = self.started.elapsed().as_millis(),
            completed = self.completed,
            "error disposition pass"
        );
    }
}

/// No Git work or filesystem I/O runs under the Store mutex. Physical sandboxes
/// are retained even after archival; their ordinary custody cleanup owns them.
fn work_evidence(facts: &ErrorFacts, budget: u32) -> Result<WorkEvidence> {
    let s = &facts.session;
    git::with_repository_mutation(&s.working_dir, || {
        let mut work = WorkEvidence::default();
        let branch = s.sandbox_branch.as_ref().map(|b| format!("refs/heads/{b}"));
        let root = s.sandbox_root.as_deref().unwrap_or(&s.working_dir);
        let head = if root.exists() {
            let (clean, head) = git::observe_clean_head_bounded(root)?;
            work.dirty = !clean;
            if let Some(branch) = &branch {
                if git::resolve_ref_locked(&s.working_dir, branch)?.as_deref() != Some(&head) {
                    work.unknown = true;
                }
            }
            Some(head)
        } else if let Some(branch) = &branch {
            git::resolve_ref_locked(&s.working_dir, branch)?
        } else {
            None
        };
        if let Some(head) = head {
            let rolling = git::resolve_ref_locked(&s.working_dir, "refs/remotes/origin/rolling")?;
            let on_rolling = match rolling {
                Some(rolling) => git::is_ancestor_locked(&s.working_dir, &head, &rolling)?,
                None => false,
            };
            work.commits = facts
                .base
                .as_ref()
                .map_or(!on_rolling, |base| *base != head);
            work.landed = work.commits && on_rolling;
            // Pin before any archive write. Pins are cheap and also protect work
            // already held by a queue/topology from later retirement of that owner.
            if work.commits
                && !on_rolling
                && matches!(
                    facts.classify(&work, budget, Utc::now()),
                    Disposition::Archive | Disposition::Superseded
                )
            {
                work.preserved_ref = Some(git::preserve_errored_session_locked(
                    &s.working_dir,
                    s.id,
                    &head,
                )?);
            }
        }
        Ok(work)
    })
}

impl SessionManager {
    pub async fn run_error_disposition_loop(self: Arc<Self>) {
        let mut cursor = None;
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if !self
                .runtime_config
                .error_disposition_enabled
                .load(Ordering::Relaxed)
            {
                continue;
            }
            if let Err(error) = self.error_disposition_pass(&mut cursor).await {
                tracing::warn!(%error,"error disposition pass failed");
            }
        }
    }

    async fn error_disposition_pass(&self, cursor: &mut Option<Uuid>) -> Result<()> {
        let mut summary = PassSummary {
            started: Instant::now(),
            scanned: 0,
            counts: [0; 6],
            store_lock_ms: 0,
            completed: false,
        };
        let now = Utc::now();
        let grace = self
            .runtime_config
            .error_disposition_grace_minutes
            .load(Ordering::Relaxed);
        let budget = self
            .runtime_config
            .error_disposition_continuation_budget
            .load(Ordering::Relaxed);
        let ids = {
            let store = self.store.lock().await;
            let held = Instant::now();
            let ids = store.error_disposition_page(
                *cursor,
                now - chrono::Duration::minutes(i64::from(grace)),
            )?;
            summary.store_lock_ms += held.elapsed().as_millis();
            ids
        };
        let count = ids.len();
        for id in ids {
            summary.scanned += 1;
            *cursor = Some(id);
            let _spawn_guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
            if self.active.read().await.contains_key(&id) {
                summary.counts[0] += 1;
                continue;
            }
            let facts = {
                let store = self.store.lock().await;
                let held = Instant::now();
                let facts = store.error_disposition_facts(id, now)?;
                summary.store_lock_ms += held.elapsed().as_millis();
                facts
            };
            let Some(facts) = facts else {
                continue;
            };
            if facts.held || facts.terminal_at > now - chrono::Duration::minutes(i64::from(grace)) {
                summary.counts[0] += 1;
                continue;
            }
            let facts = Arc::new(facts);
            let git_facts = Arc::clone(&facts);
            let work = match tokio::task::spawn_blocking(move || work_evidence(&git_facts, budget))
                .await
            {
                Ok(Ok(work)) => work,
                error => {
                    tracing::debug!(%id,?error,"error disposition retained uncertain Git evidence");
                    WorkEvidence {
                        unknown: true,
                        ..Default::default()
                    }
                }
            };
            let draining =
                self.restart_draining.load(Ordering::Acquire) || self.deploy_drain.is_draining();
            let disposition = {
                let store = self.store.lock().await;
                let held = Instant::now();
                let result =
                    store.apply_error_disposition(&facts, &work, budget, grace, draining, now);
                summary.store_lock_ms += held.elapsed().as_millis();
                result?
            };
            let index = match disposition {
                Disposition::Hold => 0,
                Disposition::Delete => 1,
                Disposition::Resume => 2,
                Disposition::Archive => 3,
                Disposition::Superseded => 4,
                Disposition::Notice => 5,
            };
            summary.counts[index] += 1;
            match disposition {
                Disposition::Delete => {
                    self.completed.write().await.remove(&id);
                    self.event_bus
                        .publish(DaemonEvent::SessionDeleted { session_id: id });
                }
                Disposition::Archive | Disposition::Superseded => {
                    self.finish_agent_archive(&[id]).await;
                }
                _ => {}
            }
        }
        if count < PAGE_SIZE {
            *cursor = None;
        }
        summary.completed = true;
        Ok(())
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-session-05")
))]
mod tests {
    use super::*;
    use crate::store::Store;
    use rsi_common::types::SessionStatus;
    use std::path::Path;

    fn command(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn unlanded_commit_is_pinned_before_the_session_is_archived() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        command(root, &["init"]);
        command(
            root,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
        );
        let base = command(root, &["rev-parse", "HEAD"]);
        command(root, &["update-ref", "refs/remotes/origin/rolling", &base]);
        std::fs::write(root.join("work.txt"), "preserve this work").unwrap();
        command(root, &["add", "work.txt"]);
        command(
            root,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "work",
            ],
        );
        let head = command(root, &["rev-parse", "HEAD"]);
        let store = Store::open_in_memory().unwrap();
        let mut s = rsid_store::test_support::test_session(Uuid::new_v4(), root.to_path_buf());
        s.status = SessionStatus::Failed;
        s.stop_reason = Some("model_unsupported".into());
        s.turn_count = Some(12);
        s.max_retries = None;
        s.retry_attempt = None;
        s.updated_at = Utc::now() - chrono::Duration::hours(2);
        store.insert_session(&s).unwrap();
        let mut facts = store
            .error_disposition_facts(s.id, Utc::now())
            .unwrap()
            .unwrap();
        facts.base = Some(base);
        let work = work_evidence(&facts, 3).unwrap();
        assert!(work.commits);
        let reference = work.preserved_ref.as_ref().unwrap();
        assert_eq!(command(root, &["rev-parse", reference]), head);
        assert_eq!(
            store.get_session(s.id).unwrap().unwrap().status,
            SessionStatus::Failed
        );
        assert_eq!(
            store
                .apply_error_disposition(&facts, &work, 3, 15, false, Utc::now())
                .unwrap(),
            Disposition::Archive
        );
        assert_eq!(
            store.get_session(s.id).unwrap().unwrap().status,
            SessionStatus::Archived
        );
        assert_eq!(
            std::fs::read_to_string(root.join("work.txt")).unwrap(),
            "preserve this work"
        );
        assert_eq!(
            work_evidence(&facts, 3).unwrap().preserved_ref,
            Some(reference.clone())
        );
    }
}
