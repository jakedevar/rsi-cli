//! The daemon-owned cold-archive service (#2384).
//!
//! A background loop starts a pass at most once per [`COLD_ARCHIVE_INTERVAL`]
//! while `cold_archive_enabled` is on (the last start is read from the durable
//! run rows, so a daemon restart does not reset it, #2436), and the operator's
//! `RunColdArchiveNow` starts one on demand. Since slice S4 the scheduled pass is
//! a `move`: it copies the bulky rows of old `Archived` and `Deleted` sessions
//! into append-only archive files, reads them back to verify, and then deletes
//! the verified rows from the live database (only while a fresh backup exists)
//! and reclaims their pages in small steps. A verify pass re-reads every chunk
//! once a week ([`COLD_VERIFY_INTERVAL`]). The store crate owns the passes
//! (`store::cold_archive`); this module owns when they run: outside startup and
//! readiness, one at a time, on their own low-priority thread, held back while
//! the host is loaded. Failures raise operator notices (`notices`), and the loop
//! settles the compaction a supervised restart ran (`cold_compact`).

use crate::cold_archive_lifecycle::{ArchiveLifecycle, PassTicket};
use crate::error::{DaemonError, Result};
use crate::store::cold_archive::reclaim::{
    COMPACT_DEFERRED_OUTCOME, INCREMENTAL_BUDGET, reclaim_incremental_until,
};
use crate::store::cold_archive::{
    ColdError, ColdMode, ColdRestoreCommit, ColdSessionStorage, ColdSettings, PassLimits,
    PassParams, RestoreReceipt, RunSummary, decode_cold_session, open_read_connection,
    resolve_archive_dir, run_move_pass, run_stage_pass, run_verify_pass, unavailable_error,
};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use uuid::Uuid;

/// How often the loop looks at the setting.
pub(crate) const COLD_ARCHIVE_TICK: Duration = Duration::from_secs(10 * 60);
/// Minimum time between two scheduled passes.
pub(crate) const COLD_ARCHIVE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// The loop does nothing for this long after the daemon is ready, so a restart
/// storm never starts a pass.
pub(crate) const COLD_ARCHIVE_INITIAL_DELAY: Duration = Duration::from_secs(15 * 60);
/// Minimum time between two scheduled verify passes (the weekly re-read of
/// every chunk, plan §9 risk 1).
pub(crate) const COLD_VERIFY_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A compaction due for longer than this raises a notice suggesting a restart.
pub(crate) const COMPACTION_DUE_NOTICE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Since when the compaction has been due (in memory: a restart is exactly
/// what runs it).
static COMPACTION_DUE_SINCE: std::sync::Mutex<Option<DateTime<Utc>>> = std::sync::Mutex::new(None);

/// Whether a scheduled stage pass is due: none has ever started, or the last
/// one started at least [`COLD_ARCHIVE_INTERVAL`] ago. A start time in the
/// future (the clock moved back) holds the schedule for at most one interval,
/// never longer.
pub(crate) fn cold_archive_pass_due(
    last_stage_started: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    interval_due(last_stage_started, now, COLD_ARCHIVE_INTERVAL)
}

/// Whether a scheduled verify pass is due: some archived chunk was last read
/// back more than [`COLD_VERIFY_INTERVAL`] ago, and no verify pass started
/// within the last [`COLD_ARCHIVE_INTERVAL`] (a verify that cannot finish in
/// its budget resumes at the next interval, it never spins).
pub(crate) fn cold_verify_pass_due(
    oldest_verified: Option<DateTime<Utc>>,
    last_verify_started: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    let Some(oldest) = oldest_verified else {
        return false;
    };
    let week = chrono::Duration::from_std(COLD_VERIFY_INTERVAL).unwrap_or(chrono::Duration::MAX);
    now - oldest >= week && interval_due(last_verify_started, now, COLD_ARCHIVE_INTERVAL)
}

fn interval_due(last: Option<DateTime<Utc>>, now: DateTime<Utc>, every: Duration) -> bool {
    let Some(last) = last else {
        return true;
    };
    let interval = chrono::Duration::from_std(every).unwrap_or(chrono::Duration::MAX);
    let elapsed = now - last;
    elapsed >= interval || elapsed < -interval
}

/// What one look of the scheduled loop did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ColdArchiveTick {
    /// `cold_archive_enabled` is off.
    Disabled,
    /// The last stage pass started less than [`COLD_ARCHIVE_INTERVAL`] ago.
    NotDue,
    /// A pass started with this run id.
    Started(Uuid),
    /// The weekly verify pass started with this run id.
    VerifyStarted(Uuid),
    /// A pass was due but could not start (running, no database, ...).
    Deferred(String),
}

/// Holds the "a pass is running" flag for the life of one run.
pub(crate) struct ColdArchiveRunGuard {
    manager: Arc<super::SessionManager>,
}

impl ColdArchiveRunGuard {
    fn acquire(manager: &Arc<super::SessionManager>) -> Option<Self> {
        manager
            .cold_archive_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self {
                manager: Arc::clone(manager),
            })
    }
}

impl Drop for ColdArchiveRunGuard {
    fn drop(&mut self) {
        self.manager
            .cold_archive_running
            .store(false, Ordering::Release);
    }
}

/// Runs one pass on the calling (dedicated, low-priority) thread and returns the
/// run's summary. The pass stops at its next safe boundary when `lifecycle` asks
/// it to (a supervised restart is draining); it opens every SQLite handle it
/// uses itself and has dropped them all when this returns.
fn run_pass_blocking(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    database_path: &Path,
    run_id: Uuid,
    mode: ColdMode,
    settings: &ColdSettings,
    host_load: &Arc<crate::host_load::HostLoadAdmission>,
    lifecycle: &ArchiveLifecycle,
) -> std::result::Result<RunSummary, ColdError> {
    let gate = || host_load.hold_now().is_none();
    let cancel = || lifecycle.cancel_requested();
    let params = PassParams {
        run_id,
        settings,
        limits: PassLimits::default(),
        batch_gate: Some(&gate),
        cancel: Some(&cancel),
    };
    match mode {
        ColdMode::Stage => {
            let read = open_read_connection(database_path)?;
            let report = run_stage_pass(store, &read, &params)?;
            tracing::info!(
                run_id = %run_id,
                sessions_examined = report.sessions_examined,
                sessions_staged = report.sessions_staged,
                sessions_unchanged = report.sessions_unchanged,
                sessions_failed = report.sessions_failed,
                raw_bytes = report.raw_bytes,
                compressed_bytes = report.compressed_bytes,
                budget_exhausted = report.budget_exhausted,
                cancelled = report.cancelled,
                "cold archive stage pass finished"
            );
            Ok(report.run_summary(None))
        }
        ColdMode::Move => {
            let read = open_read_connection(database_path)?;
            let report = run_move_pass(store, &read, &params, database_path)?;
            drop(read);
            // Reclaim what the move freed, one short store hold per step.
            let reclaim = reclaim_incremental_until(store, INCREMENTAL_BUDGET, &cancel)?;
            tracing::info!(
                run_id = %run_id,
                sessions_examined = report.sessions_examined,
                sessions_staged = report.sessions_staged,
                sessions_moved = report.moved.sessions_moved,
                sessions_reverified = report.moved.sessions_reverified,
                events_moved = report.moved.events_moved,
                provenance_moved = report.moved.provenance_moved,
                sessions_failed = report.sessions_failed,
                sessions_deferred = report.moved.sessions_deferred,
                cancelled = report.cancelled,
                cold_archive_batch_lock_ms = report.moved.max_batch_lock_ms,
                backup = report.moved.backup.as_deref().unwrap_or(""),
                backup_produced = report.moved.backup_produced,
                pages_reclaimed = reclaim.pages_reclaimed,
                incremental = reclaim.incremental,
                budget_exhausted = report.budget_exhausted,
                "cold archive move pass finished"
            );
            let mut summary = report.run_summary(None);
            summary.pages_reclaimed = reclaim.pages_reclaimed;
            Ok(summary)
        }
        ColdMode::Verify => {
            let report = run_verify_pass(store, &params)?;
            tracing::info!(
                run_id = %run_id,
                sessions_checked = report.sessions_checked,
                sessions_failed = report.sessions_failed,
                orphan_chunks = report.orphan_chunks,
                cancelled = report.cancelled,
                "cold archive verify pass finished"
            );
            Ok(report.run_summary(None))
        }
        ColdMode::Compact => Err(ColdError::new(
            "cold_archive_mode_unavailable",
            "a compaction never runs in the live daemon; it is scheduled for a supervised restart",
        )),
    }
}

impl super::SessionManager {
    /// The settings a pass starts with: the live toggles, and the archive
    /// directory (`<database directory>/archive` unless configured).
    pub(crate) async fn cold_archive_settings(&self) -> (ColdSettings, Option<PathBuf>) {
        let database_path = {
            let store = self.store.lock().await;
            store.database_path().map(PathBuf::from)
        };
        let configured = self.runtime_config.cold_archive_dir.read().clone();
        let settings = ColdSettings {
            enabled: self
                .runtime_config
                .cold_archive_enabled
                .load(Ordering::Relaxed),
            after_days: self
                .runtime_config
                .cold_archive_after_days
                .load(Ordering::Relaxed),
            dir: resolve_archive_dir(&configured, database_path.as_deref()),
            // Where this daemon allocates sandboxes (#2435): a pass refuses an
            // archive directory that resolves into it.
            forbidden_roots: vec![self.sandbox_allocator.base_dir().to_path_buf()],
        };
        (settings, database_path)
    }

    /// Puts a cold session's rows back into the live tables (slice S2, plan
    /// §4.3), under the caller's hold on the session's spawn guard. A hot session
    /// is `already_hot` with nothing read. The chunks are decoded outside the
    /// store mutex; the insert is one fenced transaction, so a twin restore, a
    /// regeneration or a move that raced the decode is detected and retried
    /// (`cold_restore_contended` after three attempts).
    ///
    /// The guard is the caller's proof that nothing resumes, unarchives or moves
    /// this session meanwhile; this function never acquires it (the guard is not
    /// re-entrant).
    pub(crate) async fn ensure_session_hot(
        &self,
        guard: &super::spawn_single_flight::SpawnGuard,
        session_id: Uuid,
    ) -> Result<RestoreReceipt> {
        for _ in 0..crate::store::cold_archive::RESTORE_ATTEMPTS {
            let plan = {
                let store = self.store.lock().await;
                match store.cold_restore_plan(guard, session_id)? {
                    Some(cold) => Ok(cold),
                    None => Err(store.cold_already_hot_receipt(session_id)?),
                }
            };
            let cold = match plan {
                Ok(cold) => cold,
                Err(receipt) => return Ok(receipt),
            };
            let for_decode = cold.clone();
            let decoded = tokio::task::spawn_blocking(move || decode_cold_session(&for_decode))
                .await
                .map_err(|error| DaemonError::Store(error.to_string()))?
                .map_err(|error| unavailable_error(&cold.path.to_string_lossy(), &error))?;
            let commit = {
                let store = self.store.lock().await;
                match store.cold_restore_commit(guard, &cold, &decoded) {
                    Ok(commit) => commit,
                    Err(error) => {
                        if error.code == "cold_restore_conflict" {
                            store.cold_archive_raise_notice(error.code, &error.detail);
                        }
                        return Err(error.into());
                    }
                }
            };
            match commit {
                ColdRestoreCommit::Restored(receipt) => {
                    tracing::info!(
                        %session_id,
                        generation = receipt.generation,
                        events = receipt.events,
                        provenance = receipt.provenance,
                        "cold session restored"
                    );
                    return Ok(receipt);
                }
                ColdRestoreCommit::AlreadyHot { generation } => {
                    return Ok(RestoreReceipt::already_hot(generation));
                }
                ColdRestoreCommit::Stale => {}
            }
        }
        Err(ColdError::new(
            "cold_restore_contended",
            format!(
                "{session_id} changed under {} restore attempts",
                crate::store::cold_archive::RESTORE_ATTEMPTS
            ),
        )
        .into())
    }

    /// `RestoreColdSession`: restores one session, taking its spawn guard.
    pub(crate) async fn restore_cold_session(&self, session_id: Uuid) -> Result<RestoreReceipt> {
        if self.store.lock().await.get_session(session_id)?.is_none() {
            return Err(DaemonError::SessionNotFound(session_id));
        }
        let guard = super::spawn_single_flight::acquire_spawn_guard(session_id).await;
        self.ensure_session_hot(&guard, session_id).await
    }

    /// `GetColdSessionStorage`: where a session's rows are.
    pub(crate) async fn cold_session_storage(
        &self,
        session_id: Uuid,
    ) -> Result<ColdSessionStorage> {
        let store = self.store.lock().await;
        if store.get_session(session_id)?.is_none() {
            return Err(DaemonError::SessionNotFound(session_id));
        }
        Ok(store.cold_session_storage(&session_id.to_string())?)
    }

    /// The fence between the pass threads and a supervised restart (#205).
    #[must_use]
    pub fn cold_archive_lifecycle(&self) -> &Arc<ArchiveLifecycle> {
        &self.cold_archive_lifecycle
    }

    /// Whether a pass is running now.
    pub(crate) fn cold_archive_is_running(&self) -> bool {
        self.cold_archive_running.load(Ordering::Acquire)
    }

    /// `RunColdArchiveNow {mode: "compact"}`: records the request; the
    /// compaction runs in an isolated helper at the next supervised restart
    /// (never in the live daemon). Returns the run id of the request.
    pub(crate) async fn request_cold_compaction(&self) -> Result<Uuid> {
        let (settings, database_path) = self.cold_archive_settings().await;
        if !settings.enabled {
            return Err(DaemonError::InvalidParam(
                "cold_archive_disabled: turn on cold_archive_enabled first".into(),
            ));
        }
        if database_path.is_none() {
            return Err(DaemonError::InvalidParam(
                "cold_archive_no_database: the store has no database file to compact".into(),
            ));
        }
        Ok(self.store.lock().await.cold_request_compaction()?)
    }

    /// Opens a run row and starts the pass in the background; returns the run id.
    /// Refused while the setting is off, while another pass is running, and for a
    /// store without a database file.
    pub(crate) async fn start_cold_archive_run(
        self: &Arc<Self>,
        trigger: &'static str,
        mode: ColdMode,
    ) -> Result<Uuid> {
        if mode == ColdMode::Compact {
            return Err(DaemonError::InvalidParam(
                "cold_archive_mode_unavailable: a compaction is scheduled, not run as a pass"
                    .into(),
            ));
        }
        let (settings, database_path) = self.cold_archive_settings().await;
        if !settings.enabled {
            return Err(DaemonError::InvalidParam(
                "cold_archive_disabled: turn on cold_archive_enabled first".into(),
            ));
        }
        let Some(database_path) = database_path else {
            return Err(DaemonError::InvalidParam(
                "cold_archive_no_database: the store has no database file to archive".into(),
            ));
        };
        let Some(guard) = ColdArchiveRunGuard::acquire(self) else {
            return Err(DaemonError::InvalidParam(
                "cold_archive_running: a cold-archive pass is already running".into(),
            ));
        };
        // A supervised restart that is draining admits no new pass (#2384 review,
        // #205): the compaction helper opens the database once every pass thread
        // has returned.
        let Some(ticket) = self.cold_archive_lifecycle.admit() else {
            return Err(DaemonError::InvalidParam(
                "cold_archive_shutting_down: a supervised restart is draining; start the pass after it"
                    .into(),
            ));
        };
        let run_id = {
            let store = self.store.lock().await;
            store.cold_begin_run(trigger, mode)?
        };
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager
                .execute_cold_archive_run(guard, ticket, run_id, mode, settings, database_path)
                .await;
        });
        Ok(run_id)
    }

    /// Runs the pass on its own thread (never a pooled worker: it lowers its own
    /// priority) and settles the run row. The guard is released last.
    async fn execute_cold_archive_run(
        self: Arc<Self>,
        guard: ColdArchiveRunGuard,
        ticket: PassTicket,
        run_id: Uuid,
        mode: ColdMode,
        settings: ColdSettings,
        database_path: PathBuf,
    ) {
        let store = Arc::clone(&self.store);
        let host_load = self.host_load();
        let lifecycle = Arc::clone(&self.cold_archive_lifecycle);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("rsid-cold-archive".into())
            .spawn(move || {
                // Held to the end of the thread: dropped after the pass returned and
                // closed every SQLite handle it opened, which is what a restart
                // waits for before it starts the compaction helper.
                let _ticket = ticket;
                crate::store::cold_archive::lower_current_thread_priority();
                let result = run_pass_blocking(
                    &store,
                    &database_path,
                    run_id,
                    mode,
                    &settings,
                    &host_load,
                    &lifecycle,
                );
                let _ = sender.send(result);
            });
        let result = match spawned {
            Ok(_) => receiver.await.unwrap_or_else(|_| {
                Err(ColdError::new(
                    "cold_archive_thread_lost",
                    "the cold-archive thread ended without a result",
                ))
            }),
            Err(error) => Err(ColdError::new(
                "cold_archive_thread_spawn",
                error.to_string(),
            )),
        };
        let mut summary = match result {
            Ok(summary) => summary,
            Err(error) => {
                tracing::warn!(run_id = %run_id, error = %error, "cold archive pass failed");
                RunSummary {
                    outcome: "failed",
                    holds_json: "{}".into(),
                    error_code: Some(error.code.to_string()),
                    error_detail: Some(error.detail),
                    ..RunSummary::default()
                }
            }
        };
        {
            let store = self.store.lock().await;
            summary.live_bytes_after = store.cold_live_db_bytes().ok();
            if let Err(error) = store.cold_finish_run(run_id, &summary) {
                tracing::warn!(run_id = %run_id, error = %error, "cold archive run row not settled");
            }
            // Plan §6.4: never silent. A failed or partial run raises its first
            // error as a notice; a completed one closes the pass notices.
            store.cold_archive_settle_run_notices(&summary);
        }
        drop(guard);
    }

    /// One look of the scheduled loop: nothing while the setting is off or while
    /// the last stage pass started less than [`COLD_ARCHIVE_INTERVAL`] before
    /// `now`, else it starts a pass. The last start is read from the durable run
    /// rows, not from memory, so a restart does not make a pass due (#2436).
    pub(crate) async fn cold_archive_tick(self: &Arc<Self>, now: DateTime<Utc>) -> ColdArchiveTick {
        // Notices queued by readers (a missing archive) are raised here.
        self.store.lock().await.cold_archive_flush_notices();
        if !self
            .runtime_config
            .cold_archive_enabled
            .load(Ordering::Relaxed)
        {
            return ColdArchiveTick::Disabled;
        }
        let (last, last_verify, oldest_verified) = {
            let store = self.store.lock().await;
            note_compaction_due(&store, now);
            match (
                store.cold_last_stage_started_at(),
                store.cold_last_verify_started_at(),
                store.cold_oldest_verified_at(),
            ) {
                (Ok(last), Ok(last_verify), Ok(oldest)) => (last, last_verify, oldest),
                (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                    return ColdArchiveTick::Deferred(format!(
                        "cannot read the last cold-archive run: {error}"
                    ));
                }
            }
        };
        if cold_archive_pass_due(last, now) {
            // Slice S4: the scheduled pass moves (stage, verify, delete, reclaim).
            return match self
                .start_cold_archive_run("scheduled", ColdMode::Move)
                .await
            {
                Ok(run_id) => ColdArchiveTick::Started(run_id),
                Err(error) => ColdArchiveTick::Deferred(error.to_string()),
            };
        }
        if cold_verify_pass_due(oldest_verified, last_verify, now) {
            return match self
                .start_cold_archive_run("scheduled", ColdMode::Verify)
                .await
            {
                Ok(run_id) => ColdArchiveTick::VerifyStarted(run_id),
                Err(error) => ColdArchiveTick::Deferred(error.to_string()),
            };
        }
        ColdArchiveTick::NotDue
    }

    /// Settles the compactions an earlier daemon ran at its restart (from the
    /// helper's result files) and raises or closes their notices. Runs at the
    /// start of the cold-archive loop, after `request_ready`.
    pub(crate) async fn reconcile_cold_compactions(&self) {
        let store = self.store.lock().await;
        let Some(database_path) = store.database_path().map(PathBuf::from) else {
            return;
        };
        match store
            .cold_reconcile_compactions(&database_path, crate::cold_compact::scheduled_run_id())
        {
            Ok(settled) => {
                for run in settled {
                    tracing::info!(run_id = %run.run_id, outcome = %run.outcome, "cold archive compaction settled");
                    // A compaction the exiting daemon deferred (a pass thread had not
                    // settled) is neither a failure nor a completion: the request is
                    // back in the queue for the next restart.
                    if run.outcome == COMPACT_DEFERRED_OUTCOME {
                        continue;
                    }
                    match run.error_code {
                        Some(code) => store.cold_archive_raise_notice(&code, &run.run_id),
                        None => {
                            let _ = store.cold_archive_resolve_notices(true);
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "cold archive compaction reconcile deferred");
            }
        }
    }

    /// The scheduled loop. Spawned after `request_ready`; waits
    /// [`COLD_ARCHIVE_INITIAL_DELAY`], then looks at the setting every
    /// [`COLD_ARCHIVE_TICK`] (see [`Self::cold_archive_tick`]).
    pub async fn run_cold_archive_loop(self: Arc<Self>) {
        // A compaction the previous daemon ran at its restart is settled first
        // (this loop is spawned after `request_ready`).
        self.reconcile_cold_compactions().await;
        tokio::time::sleep(COLD_ARCHIVE_INITIAL_DELAY).await;
        let mut interval = tokio::time::interval(COLD_ARCHIVE_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match self.cold_archive_tick(Utc::now()).await {
                ColdArchiveTick::Started(run_id) => {
                    tracing::info!(run_id = %run_id, "cold archive pass started");
                }
                ColdArchiveTick::VerifyStarted(run_id) => {
                    tracing::info!(run_id = %run_id, "cold archive weekly verify pass started");
                }
                ColdArchiveTick::Deferred(reason) => {
                    tracing::warn!(reason = %reason, "cold archive pass deferred");
                }
                ColdArchiveTick::Disabled | ColdArchiveTick::NotDue => {}
            }
        }
    }
}

/// Tracks how long the compaction has been due and raises a notice once it has
/// been due for [`COMPACTION_DUE_NOTICE_AFTER`] (it runs only at a supervised
/// restart, plan §5).
fn note_compaction_due(store: &crate::store::Store, now: DateTime<Utc>) {
    let due = store.cold_compaction_due().unwrap_or(false);
    let mut since = COMPACTION_DUE_SINCE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !due {
        *since = None;
        return;
    }
    let first = *since.get_or_insert(now);
    let limit =
        chrono::Duration::from_std(COMPACTION_DUE_NOTICE_AFTER).unwrap_or(chrono::Duration::MAX);
    if now - first >= limit {
        store.cold_archive_raise_notice(
            "compact_due_too_long",
            "the live database has been due for compaction for more than 24 hours",
        );
    }
}
