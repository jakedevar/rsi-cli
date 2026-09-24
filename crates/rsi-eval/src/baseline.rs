//! Baseline JSON I/O.
//!
//! Atomic write via tempfile-rename so a partial write never leaves a
//! corrupt baseline on disk. Pretty-printed with sorted keys (BTreeMap on
//! TicketMetrics + alphabetical struct field order) so the resulting JSON
//! is line-diffable across runs.

use crate::errors::{EvalError, Result};
use crate::metrics::BaselineSnapshot;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn read_baseline(path: &Path) -> Result<BaselineSnapshot> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        EvalError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {}", path.display(), e),
        ))
    })?;
    serde_json::from_str(&raw).map_err(EvalError::from)
}

pub fn write_baseline(snapshot: &BaselineSnapshot, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Tempfile in the same directory; rename is atomic on the same filesystem.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    let serialized = serde_json::to_string_pretty(snapshot).map_err(EvalError::from)?;
    tmp.write_all(serialized.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| {
        EvalError::Io(std::io::Error::other(format!(
            "rename to {}: {}",
            path.display(),
            e
        )))
    })?;
    Ok(())
}

/// Default capture path: `eval/baselines/<harness>.json`.
#[must_use]
pub fn default_baseline_path(harness: &str) -> PathBuf {
    PathBuf::from("eval/baselines").join(format!("{harness}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{AggregateMetrics, BaselineSnapshot, TicketMetrics};
    use std::collections::BTreeMap;

    fn fix_snapshot() -> BaselineSnapshot {
        let mut tickets = BTreeMap::new();
        tickets.insert(
            "impl-001".to_string(),
            TicketMetrics {
                approval_wait_ms: 0,
                asked_clarification: false,
                clippy_passed: Some(true),
                completion_status: "Completed".to_string(),
                phase_failure_count: 0,
                retry_count: 0,
                test_passed: Some(true),
                token_cost_total: 12345,
                turn_count: 5,
                wall_time_ms: 60_000,
            },
        );
        BaselineSnapshot {
            aggregate: AggregateMetrics {
                asked_clarification_rate: 0.0,
                clippy_pass_rate: 1.0,
                completion_rate: 1.0,
                phase_failure_count: 0,
                test_pass_rate: 1.0,
                token_cost_total: 12345,
            },
            captured_at: "2026-05-08T00:00:00Z".to_string(),
            corpus: "default".to_string(),
            git_commit: "deadbeef".to_string(),
            harness_version_hash: "h1".to_string(),
            schema_version: 1,
            tickets,
            wall_time_seconds: 60.0,
        }
    }

    #[test]
    fn round_trip_preserves_struct_equality() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.json");
        let snapshot = fix_snapshot();

        write_baseline(&snapshot, &path).unwrap();
        assert!(path.exists());

        let read_back = read_baseline(&path).unwrap();
        assert_eq!(snapshot, read_back);
    }

    #[test]
    fn missing_file_returns_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let result = read_baseline(&path);
        assert!(matches!(result, Err(EvalError::Io(_))));
    }
}
