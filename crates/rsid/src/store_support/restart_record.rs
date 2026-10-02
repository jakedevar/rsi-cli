//! Watchdog crash record (moved down from `watchdog`, which re-exports it and
//! keeps its constructor impl) so `daemon_restart_persistence` can sit in the
//! store tree.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A small crash record written without using the possibly wedged Store.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestartRecord {
    pub version: u8,
    pub id: Uuid,
    pub observed_at: DateTime<Utc>,
    pub last_healthy_at: DateTime<Utc>,
    pub failed_probes: Vec<String>,
}
