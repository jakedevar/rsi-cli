//! Operator view of the cold-storage archive (#2384): the replies of the
//! operator-only `GetColdSessionStorage`, `RestoreColdSession`,
//! `GetColdArchiveStatus` and `RunColdArchiveNow` RPCs as the TUI reads them.
//!
//! The daemon builds these replies from its own store types and serialises them
//! to JSON; these are the receiving shapes. Every field the TUI does not need is
//! omitted and unknown fields are ignored, so the daemon can add a field without
//! breaking an older TUI. `rsid` pins each shape against the daemon's real reply
//! (`rpc::tests::cold_s3::the_tui_view_types_read_the_daemons_cold_replies`), so a
//! rename on either side fails a test instead of showing a blank panel.

use serde::{Deserialize, Serialize};

/// Where a session's event rows are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColdTier {
    /// In the live database (never moved, or only staged for the archive).
    #[default]
    Live,
    /// Moved to an archive file; reads merge them back transparently.
    Cold,
    /// Moved once and restored to the live database.
    Restored,
}

/// `GetColdSessionStorage {session_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColdSessionStorageV1 {
    pub tier: ColdTier,
    #[serde(default)]
    pub generation: Option<i64>,
    #[serde(default)]
    pub archived_at: Option<String>,
    #[serde(default)]
    pub restored_at: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub archived_events: i64,
    #[serde(default)]
    pub raw_bytes: i64,
    #[serde(default)]
    pub compressed_bytes: i64,
}

/// `RestoreColdSession {session_id}`: both outcomes are success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColdRestoreReceiptV1 {
    /// `restored` or `already_hot`.
    pub outcome: String,
    #[serde(default)]
    pub generation: i64,
    /// Event rows put back in the live table.
    #[serde(default)]
    pub events: i64,
    #[serde(default)]
    pub provenance: i64,
}

/// `RunColdArchiveNow {mode}`. `scheduled` is true for `compact`: the
/// compaction runs in an isolated helper at the next supervised restart, never
/// in the live daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColdRunStartedV1 {
    pub run_id: String,
    pub mode: String,
    #[serde(default)]
    pub scheduled: bool,
}

/// The resolved settings inside `GetColdArchiveStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColdSettingsViewV1 {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub after_days: u32,
    #[serde(default)]
    pub dir: String,
}

/// The live database file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColdLiveDbV1 {
    #[serde(default)]
    pub file_bytes: Option<u64>,
    #[serde(default)]
    pub wal_bytes: Option<u64>,
    #[serde(default)]
    pub page_count: i64,
    #[serde(default)]
    pub freelist_count: i64,
    #[serde(default)]
    pub page_size: i64,
}

/// One archive file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColdArchiveFileV1 {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub month: String,
    #[serde(default)]
    pub bytes: i64,
    #[serde(default)]
    pub chunk_count: i64,
    #[serde(default)]
    pub last_verified_at: Option<String>,
    #[serde(default)]
    pub last_verify_error: Option<String>,
}

/// Counts over the archive index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColdTotalsV1 {
    #[serde(default)]
    pub staged_sessions: i64,
    #[serde(default)]
    pub verified_sessions: i64,
    #[serde(default)]
    pub cold_sessions: i64,
    #[serde(default)]
    pub restored_sessions: i64,
    #[serde(default)]
    pub sessions_with_error: i64,
    #[serde(default)]
    pub archived_events: i64,
    #[serde(default)]
    pub raw_bytes: i64,
    #[serde(default)]
    pub compressed_bytes: i64,
}

/// The newest archive run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColdRunV1 {
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub trigger: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub sessions_staged: i64,
    #[serde(default)]
    pub sessions_moved: i64,
    #[serde(default)]
    pub raw_bytes: i64,
    #[serde(default)]
    pub compressed_bytes: i64,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_detail: Option<String>,
}

/// Space reclaim after a move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColdCompactionV1 {
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub due: bool,
    /// An operator asked for a compaction; it runs at the next supervised restart.
    #[serde(default)]
    pub pending: bool,
    #[serde(default)]
    pub last_at: Option<String>,
    #[serde(default)]
    pub last_outcome: Option<String>,
    #[serde(default)]
    pub last_ms: Option<i64>,
    #[serde(default)]
    pub bytes_before: Option<i64>,
    #[serde(default)]
    pub bytes_after: Option<i64>,
}

/// `GetColdArchiveStatus {}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColdArchiveStatusV1 {
    #[serde(default)]
    pub settings: ColdSettingsViewV1,
    #[serde(default)]
    pub live_db: ColdLiveDbV1,
    #[serde(default)]
    pub archives: Vec<ColdArchiveFileV1>,
    #[serde(default)]
    pub totals: ColdTotalsV1,
    #[serde(default)]
    pub last_run: Option<ColdRunV1>,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub compaction: ColdCompactionV1,
    /// Open problems, newest information first.
    #[serde(default)]
    pub errors: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_read_the_daemons_lowercase_names() {
        for (text, tier) in [
            ("live", ColdTier::Live),
            ("cold", ColdTier::Cold),
            ("restored", ColdTier::Restored),
        ] {
            let parsed: ColdSessionStorageV1 =
                serde_json::from_value(serde_json::json!({ "tier": text })).unwrap();
            assert_eq!(parsed.tier, tier);
        }
    }

    #[test]
    fn a_reply_with_an_unknown_field_still_reads() {
        let status: ColdArchiveStatusV1 = serde_json::from_value(serde_json::json!({
            "settings": {"enabled": true, "after_days": 14, "dir": "/x", "future": 1},
            "running": true,
            "errors": ["boom"],
            "brand_new": {}
        }))
        .unwrap();
        assert!(status.settings.enabled && status.running);
        assert_eq!(status.errors, vec!["boom".to_string()]);
    }
}
