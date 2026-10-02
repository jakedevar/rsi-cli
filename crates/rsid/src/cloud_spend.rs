//! Remote-gate spend view and caps mirror (Issue #1036).
//!
//! The caps live in daemon settings (`cloud_spend_stop_line_usd`,
//! `cloud_spend_daily_cap_usd`). This module mirrors them into
//! `<cloud dir>/spend-caps.json`, which `scripts/cloud-spend.py` reads on
//! every remote run, and builds the read-only report behind the operator-only
//! `GetCloudSpend` RPC from the ledger `<cloud dir>/spend.md`.

use rsi_common::cloud_spend::{
    CAPS_FILE_NAME, CloudSpendCaps, CloudSpendReport, LEDGER_FILE_NAME, build_report,
    caps_file_json,
};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `$RSI_CLOUD_DIR`, else `~/.rsi/cloud` (the directory the gate scripts use).
pub fn default_cloud_dir() -> PathBuf {
    match std::env::var_os("RSI_CLOUD_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => rsi_common::identity::data_path("cloud", "cloud"),
    }
}

/// Atomically write the caps file the gate scripts read.
pub fn write_caps_file(cloud_dir: &Path, caps: &CloudSpendCaps) -> std::io::Result<()> {
    std::fs::create_dir_all(cloud_dir)?;
    let mut temporary = tempfile::NamedTempFile::new_in(cloud_dir)?;
    temporary.write_all(caps_file_json(caps).as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(cloud_dir.join(CAPS_FILE_NAME))
        .map_err(|error| error.error)?;
    Ok(())
}

/// The spend report for the operator's current caps and the given UTC day.
pub fn report(
    cloud_dir: &Path,
    caps: &CloudSpendCaps,
    today: chrono::NaiveDate,
) -> CloudSpendReport {
    let ledger = std::fs::read_to_string(cloud_dir.join(LEDGER_FILE_NAME)).ok();
    build_report(ledger.as_deref(), caps, today)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn caps_file_mirrors_the_settings_and_report_reads_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let caps = CloudSpendCaps {
            stop_line_usd: 120,
            daily_cap_usd: 8,
        };
        write_caps_file(dir.path(), &caps).unwrap();
        let written: CloudSpendCaps = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(CAPS_FILE_NAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(written, caps);
        std::fs::write(
            dir.path().join(LEDGER_FILE_NAME),
            "Operator grant: $100\nStop and report by $90 cumulative\n\
             Gate window i-a: stop 2026-09-29T19:23:23Z, est compute $2.00 at $1.78/h\n",
        )
        .unwrap();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let report = report(dir.path(), &written, today);
        assert!(report.ledger_found);
        assert_eq!(report.today_usd, 2.0);
        assert_eq!((report.stop_line_usd, report.daily_cap_usd), (120, 8));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn missing_ledger_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let caps = CloudSpendCaps {
            stop_line_usd: 90,
            daily_cap_usd: 15,
        };
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        assert!(!report(dir.path(), &caps, today).ledger_found);
    }
}
