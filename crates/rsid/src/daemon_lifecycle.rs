//! Daemon start/stop journal (#3480).
//!
//! A daemon that stops again and again (a merge-gate batch whose test signals
//! the hub, a crash loop, an operator restarting it by hand) used to leave only
//! `daemon.log` pairs for every seat to trace by hand. This module keeps a
//! small append-only journal beside the daemon socket: one `start` record per
//! boot and one `stop` record when a shutdown begins, carrying the reason and
//! the lander and job units that were active. A start with no stop before the
//! next start is a stop nobody announced (a crash or a SIGKILL).
//! `health_anomaly` reads it to raise `daemon_restart_loop`.
//!
//! The journal is telemetry: every write is best effort and never fails a
//! start or a shutdown. It holds no secret and no operator data (pids, times,
//! a friction-code reason, unit names). The path is set once by `main`; with no
//! path (every unit test) nothing is written and nothing is read.

use crate::store::Store;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::rolling_queue::RollingQueueEntryState;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// File name of the journal, beside the daemon socket.
pub const JOURNAL_FILE_NAME: &str = "daemon-lifecycle.jsonl";
/// The reason of an operator-requested deploy drain: the one announced stop.
pub const ANNOUNCED_REASON: &str = "drain_restart";
/// The reason shown for a stop that left no `stop` record.
pub const UNRECORDED_REASON: &str = "unrecorded";
/// Records kept; older ones are dropped at each start.
const MAX_RECORDS: usize = 64;
/// Active units one `stop` record names.
const MAX_ACTIVE_UNITS: usize = 6;
/// How long a shutdown waits for the store to name the active units.
const ACTIVE_UNITS_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

static JOURNAL: OnceLock<PathBuf> = OnceLock::new();

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum JournalRecord {
    /// A daemon process began.
    Start { pid: u32, at: String },
    /// A shutdown began.
    Stop {
        pid: u32,
        at: String,
        reason: String,
        #[serde(default)]
        active: Vec<String>,
    },
}

/// One daemon stop, rebuilt from the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStop {
    /// When the stopped process started, when the journal knows.
    pub started_at: Option<DateTime<Utc>>,
    /// When the stop began; for a stop with no record, when the next daemon
    /// started (an upper bound).
    pub stopped_at: DateTime<Utc>,
    /// The recorded reason, or [`UNRECORDED_REASON`].
    pub reason: String,
    /// Lander and job units active at the stop (empty when unknown).
    pub active: Vec<String>,
}

impl DaemonStop {
    /// Only an operator deploy drain announces itself.
    #[must_use]
    pub fn is_announced_stop(&self) -> bool {
        self.reason == ANNOUNCED_REASON
    }

    /// Seconds the process ran (an upper bound for an unrecorded stop).
    #[must_use]
    pub fn uptime_secs(&self) -> Option<i64> {
        self.started_at
            .map(|started| (self.stopped_at - started).num_seconds().max(0))
    }
}

fn journal_stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn journal_parse_at(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Set the journal path for this process (first call wins). Called once by
/// `main`; without it nothing is written or read.
pub fn set_journal_path(path: PathBuf) {
    let _ = JOURNAL.set(path);
}

/// The journal beside `socket_path`.
#[must_use]
pub fn journal_path_beside(socket_path: &Path) -> PathBuf {
    socket_path.with_file_name(JOURNAL_FILE_NAME)
}

/// Parse journal text; malformed lines are skipped.
#[must_use]
pub fn parse_journal_records(text: &str) -> Vec<JournalRecord> {
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn read_journal_records(path: &Path) -> Vec<JournalRecord> {
    std::fs::read_to_string(path)
        .map(|text| parse_journal_records(&text))
        .unwrap_or_default()
}

/// The stops `records` hold, oldest first. The newest `start` with no later
/// record is the live daemon, not a stop.
#[must_use]
pub fn stops_from_journal(records: &[JournalRecord]) -> Vec<DaemonStop> {
    let mut stops = Vec::new();
    // The process that has started and not (yet) been seen to stop.
    let mut running: Option<(u32, DateTime<Utc>)> = None;
    for record in records {
        match record {
            JournalRecord::Start { pid, at } => {
                let Some(at) = journal_parse_at(at) else {
                    continue;
                };
                if let Some((_, started)) = running.take() {
                    stops.push(DaemonStop {
                        started_at: Some(started),
                        stopped_at: at,
                        reason: UNRECORDED_REASON.to_owned(),
                        active: Vec::new(),
                    });
                }
                running = Some((*pid, at));
            }
            JournalRecord::Stop {
                pid,
                at,
                reason,
                active,
            } => {
                let Some(at) = journal_parse_at(at) else {
                    continue;
                };
                let started_at = match running {
                    Some((running_pid, started)) if running_pid == *pid => {
                        running = None;
                        Some(started)
                    }
                    _ => None,
                };
                stops.push(DaemonStop {
                    started_at,
                    stopped_at: at,
                    reason: reason.clone(),
                    active: active.clone(),
                });
            }
        }
    }
    stops
}

/// Append one record. Best effort.
fn journal_append(path: &Path, record: &JournalRecord) {
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(error) = result {
        tracing::warn!(%error, path = %path.display(), "daemon lifecycle journal not written");
    }
}

/// JournalRecord this boot at `path`, trimming the journal to its newest records
/// (written aside and renamed, so a reader never sees half a file).
pub fn journal_record_start_at(path: &Path, pid: u32, now: DateTime<Utc>) {
    let mut records = read_journal_records(path);
    records.push(JournalRecord::Start {
        pid,
        at: journal_stamp(now),
    });
    if records.len() > MAX_RECORDS {
        records.drain(..records.len() - MAX_RECORDS);
    }
    let text: String = records
        .iter()
        .filter_map(|record| serde_json::to_string(record).ok())
        .map(|line| line + "\n")
        .collect();
    let staged = path.with_extension("tmp");
    let result = std::fs::write(&staged, text).and_then(|()| std::fs::rename(&staged, path));
    if let Err(error) = result {
        tracing::warn!(%error, path = %path.display(), "daemon lifecycle journal not written");
    }
}

/// Record that this process began (no-op without [`set_journal_path`]).
pub fn journal_record_start() {
    if let Some(path) = JOURNAL.get() {
        journal_record_start_at(path, std::process::id(), Utc::now());
    }
}

/// Record that this process began to shut down for `reason` (no-op without
/// [`set_journal_path`]). The reason is the same friction code `mark_shutdown_begun` takes.
pub fn journal_record_stop_at(
    path: &Path,
    pid: u32,
    reason: &str,
    active: Vec<String>,
    now: DateTime<Utc>,
) {
    let mut active = active;
    active.truncate(MAX_ACTIVE_UNITS);
    journal_append(
        path,
        &JournalRecord::Stop {
            pid,
            at: journal_stamp(now),
            reason: reason.to_owned(),
            active,
        },
    );
}

/// The lander entries gating and the job units running now, for a `stop`
/// record. Waits at most [`ACTIVE_UNITS_LOCK_WAIT`] for the store: a shutdown
/// never blocks on telemetry, and an unknown set is recorded as empty.
pub async fn journal_active_units(store: &Arc<rsid_core::store_lock::Mutex<Store>>) -> Vec<String> {
    let Ok(guard) = tokio::time::timeout(ACTIVE_UNITS_LOCK_WAIT, store.lock()).await else {
        return Vec::new();
    };
    let mut units = Vec::new();
    if let Ok(entries) = guard.list_rolling_queue_entries(Some(RollingQueueEntryState::Gating), 16)
    {
        units.extend(entries.into_iter().map(|(entry, _)| {
            let short: String = entry.source_commit.chars().take(12).collect();
            format!("lander:entry={}:source={short}", entry.id)
        }));
    }
    if let Ok(jobs) = guard.list_running_agent_jobs() {
        units.extend(
            jobs.into_iter()
                .map(|row| format!("job:{}:{}", row.job.kind.as_str(), row.job.unit_name)),
        );
    }
    units
}

/// JournalRecord the start of a shutdown for `reason` with the units active now
/// (no-op without [`set_journal_path`]). Call right after `mark_shutdown_begun`.
pub async fn journal_record_stop(store: &Arc<rsid_core::store_lock::Mutex<Store>>, reason: &str) {
    let Some(path) = JOURNAL.get() else { return };
    let active = journal_active_units(store).await;
    journal_record_stop_at(path, std::process::id(), reason, active, Utc::now());
}

/// Every stop the journal holds, oldest first. Empty without [`set_journal_path`].
#[must_use]
pub fn journal_stops() -> Vec<DaemonStop> {
    JOURNAL
        .get()
        .map(|path| stops_from_journal(&read_journal_records(path)))
        .unwrap_or_default()
}

/// The unannounced stops in `stops` that fall inside the window.
#[must_use]
pub fn unannounced_within(
    stops: &[DaemonStop],
    now: DateTime<Utc>,
    window: Duration,
) -> Vec<DaemonStop> {
    stops
        .iter()
        .filter(|stop| !stop.is_announced_stop() && now - stop.stopped_at <= window)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-21T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(minutes)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rsid-lifecycle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(JOURNAL_FILE_NAME)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn a_stop_is_paired_with_its_start_and_names_the_active_units() {
        let path = scratch("paired");
        journal_record_start_at(&path, 10, at(0));
        journal_record_stop_at(&path, 10, "ctrl_c", vec!["lander:entry=a".into()], at(4));
        journal_record_start_at(&path, 11, at(5));
        let stops = stops_from_journal(&read_journal_records(&path));
        assert_eq!(stops.len(), 1, "the live daemon is not a stop");
        assert_eq!(stops[0].reason, "ctrl_c");
        assert_eq!(stops[0].uptime_secs(), Some(240));
        assert_eq!(stops[0].active, vec!["lander:entry=a".to_owned()]);
        assert!(!stops[0].is_announced_stop());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn a_start_with_no_stop_before_the_next_start_is_an_unrecorded_stop() {
        let path = scratch("lost");
        journal_record_start_at(&path, 10, at(0));
        journal_record_start_at(&path, 11, at(30));
        let stops = stops_from_journal(&read_journal_records(&path));
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].reason, UNRECORDED_REASON);
        assert_eq!(stops[0].uptime_secs(), Some(30 * 60));
        assert!(!stops[0].is_announced_stop());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn an_operator_deploy_drain_is_announced_and_never_counted() {
        let path = scratch("drain");
        journal_record_start_at(&path, 10, at(0));
        journal_record_stop_at(&path, 10, ANNOUNCED_REASON, Vec::new(), at(10));
        journal_record_start_at(&path, 11, at(11));
        journal_record_stop_at(&path, 11, "sigterm", Vec::new(), at(20));
        let stops = stops_from_journal(&read_journal_records(&path));
        let counted = unannounced_within(&stops, at(30), Duration::hours(2));
        assert_eq!(counted.len(), 1);
        assert_eq!(counted[0].reason, "sigterm");
        // A stop older than the window is forgotten.
        assert!(unannounced_within(&stops, at(20 + 121), Duration::hours(2)).is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn the_journal_is_trimmed_and_ignores_malformed_lines() {
        let path = scratch("trim");
        std::fs::write(&path, "not json\n{\"event\":\"mystery\"}\n").unwrap();
        for pid in 0..(MAX_RECORDS as u32 + 10) {
            journal_record_start_at(&path, pid, at(i64::from(pid)));
        }
        let records = read_journal_records(&path);
        assert_eq!(records.len(), MAX_RECORDS);
        assert!(matches!(
            records.last(),
            Some(JournalRecord::Start { pid, .. }) if *pid == MAX_RECORDS as u32 + 9
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn nothing_is_read_without_an_initialised_journal() {
        // The journal path is process-global and `main` alone sets it.
        assert!(journal_stops().is_empty());
    }
}
