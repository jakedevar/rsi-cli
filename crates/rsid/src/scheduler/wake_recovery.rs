//! A permanently refused wake keeps its result behind one durable recovery
//! notice. Never repair custody, reuse a rejected root, or guess a source SHA.

use super::*;

pub(super) fn permanent_custody_refusal(error: &DaemonError) -> bool {
    matches!(
        sandbox_custody_refusal_code(error),
        Some(
            "sandbox_custody:source_revision_unavailable"
                | "sandbox_custody:ownership_missing"
                // Revalidation quarantines the original root. A restart after
                // the notice write can see this compatible historical tuple.
                | "sandbox_custody:cleanup_failed"
        )
    )
}

/// Shared by permanent custody failures and exhausted continuation fences.
/// The escalation's deterministic wake id survives a crash between notice and
/// retirement. A failed notice write leaves the original wake armed, with no
/// delivery retries spent; an edited/disarmed wake owns its newer epoch.
pub(super) async fn settle_recoverable_wake(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    job: &rsi_common::types::ScheduledJob,
    target: Uuid,
    refusal: &str,
    permanent: bool,
) {
    let root = {
        let guard = store.lock().await;
        if !retry_fence_holds(&guard, job) {
            return;
        }
        match guard.get_session(target) {
            Ok(session) => {
                session.map(|session| session.sandbox_root.unwrap_or(session.working_dir))
            }
            Err(error) => {
                tracing::error!(job_id = %job.id, %error, "could not observe recovery source; wake retained");
                return;
            }
        }
    };
    // Bounded, hardened Git observation only; this confers no custody permit.
    let preserved = tokio::task::spawn_blocking(move || {
        let root = root?;
        crate::sandbox::git_worktree::observe_clean_head_bounded(&root).ok()
    })
    .await
    .ok()
    .flatten();
    let source = match preserved {
        Some((clean, head)) => format!(
            "preserved exact head {head} (clean={clean}); relaunch a new authorized worker with \
             sandbox_source {{\"commit\":\"{head}\"}}. Preserve the old worktree{}; \
             merge origin/rolling, never rebase",
            if clean { "" } else { " and its uncommitted files" },
        ),
        None => "exact head unavailable: preserve the old worktree and inspect its committed HEAD \
                 before relaunching a new authorized worker with sandbox_source {\"commit\":\"<verified exact head>\"}"
            .to_string(),
    };
    let message = format!(
        "wake_recoverable: target {target}; {refusal}; {source}. {}",
        if permanent {
            "Permanent custody refusal; no delivery retry budget spent."
        } else {
            "Continuation delivery exhausted; the original result is retained in this notice."
        },
    );
    // Check again after the blocking observation. Keep the lock through notice
    // creation and settlement so an in-process edit cannot cross this fence.
    let guard = store.lock().await;
    if !retry_fence_holds(&guard, job) {
        return;
    }
    match guard.escalate_undelivered_wake(job, &message, Utc::now()) {
        Ok(_) => {}
        Err(error) => {
            tracing::error!(job_id = %job.id, %error, "recovery notice not durable; wake retained");
            let retry_at = Utc::now() + GONE_WAKE_ESCALATION_CAPACITY_BACKOFF;
            if let Err(error) = guard.defer_wake_when(job.id, retry_at) {
                tracing::error!(job_id = %job.id, %error, "could not defer recovery notice write");
            }
            return;
        }
    }
    #[cfg(test)]
    if take_fault_for_test(job.id, "crash_after_recovery_notice") {
        return;
    }
    // A permanent failure retires even a recurring wake: ticking the same
    // unauthorized root again cannot recover it. Exhaustion keeps ordinary
    // recurrence semantics for compatibility.
    if permanent {
        match guard.settle_watch_fire_with_retry(
            &WatchFireCapture::default(),
            job.id,
            Some(&Utc::now()),
            None,
            false,
            true,
        ) {
            Ok(true) => bus.publish(DaemonEvent::SystemMessage {
                level: "error".into(),
                message,
            }),
            Ok(false) => {}
            Err(error) => {
                tracing::error!(job_id = %job.id, %error, "could not settle recoverable wake")
            }
        }
    } else {
        drop(guard);
        advance_job_after_attempt_clearing_retry(store, job).await;
    }
}
