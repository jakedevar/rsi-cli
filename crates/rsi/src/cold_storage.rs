//! Cold-storage surfaces of the TUI (#2384, slice S3): the "cold storage" badge
//! of an archived session, the restore action (`:cold-restore`), the
//! `:cold-archive` command and the Settings > Cold Storage status rows.
//!
//! A session's bulky rows (tool output, system and thinking events) move to a
//! compressed archive file once it has been Archived or Deleted for long enough.
//! Reading it is transparent (`GetConversation` merges the archive back, so the
//! transcript is whole); this module only tells the operator where the rows are
//! and lets them put the rows back. Everything goes through the operator-only
//! `GetColdSessionStorage`, `RestoreColdSession`, `GetColdArchiveStatus` and
//! `RunColdArchiveNow` RPCs. Only an `Archived` or `Deleted` session can be cold,
//! so the TUI asks about no other.

use crate::app::App;
use crate::settings::DaemonFeatureEntry;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use rsi_common::cold_storage::{
    ColdArchiveStatusV1, ColdRestoreReceiptV1, ColdRunV1, ColdSessionStorageV1, ColdTier,
};
use rsi_common::types::SessionStatus;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use uuid::Uuid;

/// The badge text of a session whose rows are in cold storage.
pub(crate) const COLD_STORAGE_BADGE: &str = "cold storage";

/// Settings rows backed by the archive status (`app.daemon_features` fields).
pub(crate) const STATUS_FIELD: &str = "cold_archive_status";
pub(crate) const LAST_RUN_FIELD: &str = "cold_archive_last_run";
pub(crate) const RUN_NOW_FIELD: &str = "cold_archive_run_now";

const TIER_FETCH_TIMEOUT: Duration = Duration::from_secs(3);
const PROBLEM_MAX_CHARS: usize = 90;

/// Only an `Archived` or `Deleted` session can be cold: the archive's candidate
/// query selects no other status and the daemon refuses to move any other
/// status out of a cold session.
#[must_use]
pub(crate) const fn can_be_cold(status: SessionStatus) -> bool {
    matches!(status, SessionStatus::Archived | SessionStatus::Deleted)
}

/// A finished tier read for one session.
#[derive(Debug)]
pub(crate) struct ColdTierFetched {
    pub(crate) session_id: Uuid,
    pub(crate) revision: u64,
    pub(crate) storage: Result<ColdSessionStorageV1, String>,
}

/// What the TUI knows about cold storage: the tier of each session it asked
/// about, and the channel its lazy reads answer on.
pub(crate) struct ColdStorageState {
    tiers: HashMap<Uuid, ColdSessionStorageV1>,
    inflight: HashSet<Uuid>,
    revisions: HashMap<Uuid, u64>,
    tx: UnboundedSender<ColdTierFetched>,
    pub(crate) rx: UnboundedReceiver<ColdTierFetched>,
}

impl Default for ColdStorageState {
    fn default() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            tiers: HashMap::new(),
            inflight: HashSet::new(),
            revisions: HashMap::new(),
            tx,
            rx,
        }
    }
}

impl ColdStorageState {
    /// The last tier read for `session_id`.
    #[must_use]
    pub(crate) fn tier(&self, session_id: Uuid) -> Option<&ColdSessionStorageV1> {
        self.tiers.get(&session_id)
    }

    /// Whether the last read said the session's rows are in the archive.
    #[must_use]
    pub(crate) fn is_cold(&self, session_id: Uuid) -> bool {
        self.tier(session_id)
            .is_some_and(|storage| storage.tier == ColdTier::Cold)
    }

    pub(crate) fn record(&mut self, session_id: Uuid, storage: ColdSessionStorageV1) -> bool {
        self.tiers.insert(session_id, storage.clone()) != Some(storage)
    }

    /// Supersede reads that observed the session before a newer operation.
    fn invalidate_reads(&mut self, session_id: Uuid) -> u64 {
        let revision = self.revisions.entry(session_id).or_default();
        *revision += 1;
        self.inflight.remove(&session_id);
        *revision
    }
}

/// The badge of a session in cold storage, for the conversation header; `None`
/// for every other session.
#[must_use]
pub(crate) fn badge_span(app: &App, session_id: Uuid) -> Option<Span<'static>> {
    let status = app.sessions.get(&session_id)?.session.status;
    (can_be_cold(status) && app.cold_storage.is_cold(session_id)).then(|| {
        Span::styled(
            format!("[{COLD_STORAGE_BADGE}]"),
            Style::default()
                .fg(crate::ui::theme::sapphire())
                .add_modifier(Modifier::BOLD),
        )
    })
}

impl App {
    /// Ask the daemon, off the render path, where an archived session's rows are.
    /// Called when a session detail view becomes active; a session that cannot
    /// be cold costs nothing.
    pub(crate) fn trigger_cold_storage_fetch_if_needed(&mut self, session_id: Uuid) {
        if self.cold_storage.inflight.contains(&session_id) {
            return;
        }
        let Some(state) = self.sessions.get(&session_id) else {
            return;
        };
        if !can_be_cold(state.session.status) {
            return;
        }
        // Production always runs under the event loop's runtime; synchronous
        // unit tests have none.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let revision = self.cold_storage.invalidate_reads(session_id);
        self.cold_storage.inflight.insert(session_id);
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.cold_storage.tx.clone();
        runtime.spawn(async move {
            let storage = match tokio::time::timeout(TIER_FETCH_TIMEOUT, async {
                let mut client = crate::client::DaemonClient::new(socket_path);
                client.connect().await.map_err(|error| error.to_string())?;
                client
                    .get_cold_session_storage(session_id)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await
            {
                Ok(result) => result,
                Err(_) => Err("cold storage read timed out".to_string()),
            };
            let _ = tx.send(ColdTierFetched {
                session_id,
                revision,
                storage,
            });
        });
    }

    /// Consume a finished tier read. Returns `true` when the screen changed.
    pub(crate) fn apply_cold_tier_fetch(&mut self, result: ColdTierFetched) -> bool {
        if self.cold_storage.revisions.get(&result.session_id) != Some(&result.revision) {
            return false;
        }
        self.cold_storage.inflight.remove(&result.session_id);
        match result.storage {
            Ok(storage) => self.cold_storage.record(result.session_id, storage),
            Err(error) => {
                tracing::debug!(
                    session = %result.session_id,
                    %error,
                    "cold storage tier read failed (non-fatal; shown on the next open)"
                );
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Restore (`:cold-restore`)
// ---------------------------------------------------------------------------

/// The toast of a finished restore.
#[must_use]
pub(crate) fn restore_message(receipt: &ColdRestoreReceiptV1) -> String {
    match receipt.outcome.as_str() {
        "restored" => format!(
            "Restored from cold storage: {} event{} back in the live database",
            receipt.events,
            if receipt.events == 1 { "" } else { "s" }
        ),
        "already_hot" => "Already in the live database: nothing to restore".to_string(),
        other => format!("Cold-storage restore: {other}"),
    }
}

/// `:cold-restore` for the focused session: put its archived rows back into the
/// live database. Idempotent: a session that is not cold answers `already_hot`.
pub(crate) async fn restore_focused_session(app: &mut App) {
    let Some(session_id) = app.selected_session_id() else {
        app.notify("No session selected");
        return;
    };
    match app.client.restore_cold_session(session_id).await {
        Ok(receipt) => {
            app.cold_storage.invalidate_reads(session_id);
            // The restore receipt is authoritative even if the metadata read
            // below fails. An old cold reply must not undo the successful action.
            let tier = match receipt.outcome.as_str() {
                "restored" => Some(ColdTier::Restored),
                "already_hot" => Some(
                    app.cold_storage
                        .tier(session_id)
                        .filter(|storage| storage.tier == ColdTier::Restored)
                        .map_or(ColdTier::Live, |storage| storage.tier),
                ),
                _ => None,
            };
            if let Some(tier) = tier {
                app.cold_storage.record(
                    session_id,
                    ColdSessionStorageV1 {
                        tier,
                        generation: (receipt.generation > 0).then_some(receipt.generation),
                        ..ColdSessionStorageV1::default()
                    },
                );
            }
            app.notify_success(restore_message(&receipt));
            match app.client.get_cold_session_storage(session_id).await {
                Ok(storage) => {
                    app.cold_storage.record(session_id, storage);
                }
                Err(error) => tracing::debug!(
                    session = %session_id,
                    %error,
                    "cold storage tier re-read after a restore failed"
                ),
            }
        }
        Err(error) => app.notify_error(format!("Restore from cold storage failed: {error}")),
    }
    app.mark_dirty();
}

// ---------------------------------------------------------------------------
// The archive status (Settings > Cold Storage, `:cold-archive`)
// ---------------------------------------------------------------------------

/// `1.5 GiB`, `612.4 MiB`, `3.0 KiB` or `12 B` for a byte count.
#[must_use]
pub(crate) fn bytes_text(bytes: i64) -> String {
    const KIB: f64 = 1024.0;
    let bytes = bytes.max(0);
    // Archive sizes are far below 2^53, so the float is exact enough to print.
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64;
    if value >= KIB * KIB * KIB {
        format!("{:.1} GiB", value / (KIB * KIB * KIB))
    } else if value >= KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn count_text(count: i64, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// The one-line state of the archive: the live database, the archive files, what
/// is staged, verified and cold, and whether a pass is running.
#[must_use]
pub(crate) fn status_line(status: &ColdArchiveStatusV1) -> String {
    let live_bytes = status.live_db.file_bytes.map_or_else(
        || "unknown".to_string(),
        |bytes| bytes_text(i64::try_from(bytes).unwrap_or(i64::MAX)),
    );
    let archive_bytes: i64 = status.archives.iter().map(|file| file.bytes).sum();
    let mut parts = vec![
        format!(
            "archive {}",
            if status.settings.enabled { "on" } else { "off" }
        ),
        format!("live DB {live_bytes}"),
        format!(
            "{} {}",
            count_text(
                i64::try_from(status.archives.len()).unwrap_or(i64::MAX),
                "file"
            ),
            bytes_text(archive_bytes)
        ),
        format!(
            "{} staged ({} verified)",
            status.totals.staged_sessions, status.totals.verified_sessions
        ),
        format!("{} cold", status.totals.cold_sessions),
    ];
    if status.totals.restored_sessions > 0 {
        parts.push(format!("{} restored", status.totals.restored_sessions));
    }
    if status.compaction.pending {
        parts.push("compaction at next restart".to_string());
    } else if status.compaction.due {
        parts.push("compaction due".to_string());
    }
    parts.push(if status.running { "running" } else { "idle" }.to_string());
    if let Some(problems) = open_problems_line(status) {
        parts.insert(0, problems);
    }
    parts.join(" · ")
}

/// Problem entries can describe whole files or a group of sessions, so keep
/// the session count separate rather than counting it twice as open problems.
fn open_problems_line(status: &ColdArchiveStatusV1) -> Option<String> {
    let mut parts = Vec::new();
    if !status.errors.is_empty() {
        parts.push(count_text(
            i64::try_from(status.errors.len()).unwrap_or(i64::MAX),
            "open problem",
        ));
    }
    if status.totals.sessions_with_error > 0 {
        parts.push(format!(
            "{} with archive errors",
            count_text(status.totals.sessions_with_error, "session")
        ));
    }
    if let Some(error) = status.errors.first() {
        parts.push(format!("problem: {}", clipped(error, PROBLEM_MAX_CHARS)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn clipped(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let mut cut: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The newest pass, one line: mode, trigger, outcome, when, what it did and its
/// problem, if any; the first open problem when there is no pass.
#[must_use]
pub(crate) fn last_run_line(status: &ColdArchiveStatusV1) -> String {
    let Some(run) = &status.last_run else {
        return status.errors.first().map_or_else(
            || "no pass has run yet".to_string(),
            |error| {
                format!(
                    "no pass yet · problem: {}",
                    clipped(error, PROBLEM_MAX_CHARS)
                )
            },
        );
    };
    let mut line = run_line(run);
    if let Some(problems) = open_problems_line(status) {
        line.push_str(&format!(" · {problems}"));
    }
    line
}

fn run_line(run: &ColdRunV1) -> String {
    let mut parts = vec![
        format!("{} ({})", run.mode, run.trigger),
        run.outcome.clone(),
        run.started_at.clone(),
    ];
    if run.mode == "stage" || run.mode == "move" {
        parts.push(format!("{} staged", run.sessions_staged));
        if run.mode == "move" {
            parts.push(format!("{} moved", run.sessions_moved));
        }
        if run.raw_bytes > 0 {
            parts.push(format!(
                "{} → {}",
                bytes_text(run.raw_bytes),
                bytes_text(run.compressed_bytes)
            ));
        }
    }
    if let Some(code) = &run.error_code {
        let detail = run
            .error_detail
            .as_deref()
            .map(|detail| format!(": {}", clipped(detail, PROBLEM_MAX_CHARS)))
            .unwrap_or_default();
        parts.push(format!("problem: {code}{detail}"));
    }
    parts.join(" · ")
}

/// Write one status read into the Settings rows.
pub(crate) fn sync_rows(app: &mut App, status: &ColdArchiveStatusV1) {
    DaemonFeatureEntry::set_display_text(
        &mut app.daemon_features,
        STATUS_FIELD,
        status_line(status),
    );
    DaemonFeatureEntry::set_display_text(
        &mut app.daemon_features,
        LAST_RUN_FIELD,
        last_run_line(status),
    );
}

/// Read `GetColdArchiveStatus` into the Settings rows. Returns the status.
pub(crate) async fn refresh_status(app: &mut App) -> Option<ColdArchiveStatusV1> {
    match app.client.get_cold_archive_status().await {
        Ok(status) => {
            sync_rows(app, &status);
            app.mark_dirty();
            Some(status)
        }
        Err(error) => {
            let text = format!("Refresh failed: {error}");
            DaemonFeatureEntry::set_display_text(&mut app.daemon_features, STATUS_FIELD, text);
            app.mark_dirty();
            None
        }
    }
}

/// `:cold-archive` / `:cold-archive status`: refresh the rows and announce the
/// state and the first open problem.
async fn show_status(app: &mut App) {
    match refresh_status(app).await {
        Some(status) => {
            let message = format!("Cold storage: {}", status_line(&status));
            if open_problems_line(&status).is_some() {
                app.notify_error(message);
            } else {
                app.notify(message);
            }
        }
        None => app.notify_error("Cold storage: the status read failed (see the Cold Storage tab)"),
    }
}

/// Start an archive pass in the background (`stage`, `move`, `verify`), or
/// schedule the compaction (`compact`: it runs at the next supervised restart).
pub(crate) async fn run_pass(app: &mut App, mode: &str) {
    match app.client.run_cold_archive_now(mode).await {
        Ok(started) => {
            let short: String = started.run_id.chars().take(8).collect();
            if started.scheduled {
                app.notify_success(format!(
                    "Cold-storage {} scheduled for the next supervised restart (run {short})",
                    started.mode
                ));
            } else {
                app.notify_success(format!(
                    "Cold-storage {} pass started (run {short})",
                    started.mode
                ));
            }
            refresh_status(app).await;
        }
        Err(error) => app.notify_error(format!("Cold-storage {mode} pass: {error}")),
    }
    app.mark_dirty();
}

/// `:cold-archive [status|run|move|verify|compact]`.
pub(crate) async fn cold_archive_command(app: &mut App, args: Option<&str>) {
    match args.map(str::trim).unwrap_or_default() {
        "" | "status" => show_status(app).await,
        "run" | "stage" => run_pass(app, "stage").await,
        "move" => run_pass(app, "move").await,
        "verify" => run_pass(app, "verify").await,
        "compact" => run_pass(app, "compact").await,
        other => app.notify_error(format!(
            "Unknown cold-archive action `{other}`; use :cold-archive [status|run|move|verify|compact]"
        )),
    }
}

#[cfg(test)]
mod tests;
