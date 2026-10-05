//! Watchdog crash record, its failed-probe codes and the crash-sidecar file I/O
//! (moved down from `watchdog`, which re-exports them and keeps the trip
//! constructor) so `daemon_restart_persistence` can sit in the store tree.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const SIDECAR_PREFIX: &str = "daemon-watchdog-restart-";

/// A small crash record written without using the possibly wedged Store.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestartRecord {
    pub version: u8,
    pub id: Uuid,
    pub observed_at: DateTime<Utc>,
    pub last_healthy_at: DateTime<Utc>,
    pub failed_probes: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailedProbe {
    Rpc,
    Store,
    Scheduler,
    Reconciliation,
}

impl FailedProbe {
    pub fn code(self) -> &'static str {
        match self {
            Self::Rpc => "rpc_timeout",
            Self::Store => "store_probe_timeout",
            Self::Scheduler => "scheduler_tick_stale",
            Self::Reconciliation => "reconciliation_tick_stale",
        }
    }
}

pub fn sidecar_path(data_dir: &Path, id: Uuid) -> PathBuf {
    data_dir.join(format!("{SIDECAR_PREFIX}{id}.json"))
}

pub fn pending_restart_record_paths(data_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix(SIDECAR_PREFIX)
            .and_then(|suffix| suffix.strip_suffix(".json"))
            .is_some_and(|id| Uuid::parse_str(id).is_ok())
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

/// Rename and sync the containing directory so a Store deadlock cannot erase
/// the reason for the next boot. The caller must hold the daemon instance lease.
pub fn write_restart_record(path: &Path, record: &RestartRecord) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("sidecar has no parent"))?;
    if path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "watchdog restart record already exists",
        ));
    }
    let temporary = parent.join(format!(".{SIDECAR_PREFIX}{}.tmp", record.id));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn read_restart_record(path: &Path) -> std::io::Result<Option<RestartRecord>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let record: RestartRecord = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    if record.version != 1
        || path.file_name() != sidecar_path(Path::new(""), record.id).file_name()
        || record.failed_probes.is_empty()
        || record.last_healthy_at > record.observed_at
        || record.failed_probes.iter().any(|code| {
            ![
                FailedProbe::Rpc.code(),
                FailedProbe::Store.code(),
                FailedProbe::Scheduler.code(),
                FailedProbe::Reconciliation.code(),
            ]
            .contains(&code.as_str())
        })
    {
        return Err(std::io::Error::other("invalid watchdog restart record"));
    }
    Ok(Some(record))
}

/// Replay crash sidecars after the Store has opened. `persist` must return
/// only after its insert transaction commits; its insert must be idempotent by
/// restart ID because an unlink or directory sync can fail after that commit.
/// Invalid records remain on disk so the operator can inspect the raw bytes.
pub fn import_pending_restart_records(
    data_dir: &Path,
    mut persist: impl FnMut(&RestartRecord) -> std::io::Result<()>,
) -> std::io::Result<Vec<RestartRecord>> {
    let mut imported = Vec::new();
    for path in pending_restart_record_paths(data_dir)? {
        let record = match read_restart_record(&path) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "invalid watchdog restart sidecar retained");
                continue;
            }
        };
        persist(&record)?;
        std::fs::remove_file(&path)?;
        imported.push(record);
    }
    if !imported.is_empty() {
        std::fs::File::open(data_dir)?.sync_all()?;
    }
    Ok(imported)
}
