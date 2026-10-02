//! Wire types and settings limits for the daemon's child-aware continuation
//! policy (#794 S3): holding program-mode wakes while children run and the
//! slow keep-alive valve.
//!
//! Everything here is operator-only. No agent verb reads or sets any of it;
//! the read side (`ListScheduledJobHolds`) exists so the Scheduled Jobs zone
//! can show why a wake has not fired.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Setting keys (persisted runtime-config fields).
pub const PROGRAM_HOLD_SETTING: &str = "program_hold_while_children_run";
pub const KEEPALIVE_ENABLED_SETTING: &str = "child_keepalive_enabled";
pub const KEEPALIVE_WINDOW_SETTING: &str = "child_keepalive_window_secs";

pub const PROGRAM_HOLD_DEFAULT: bool = true;
pub const KEEPALIVE_ENABLED_DEFAULT: bool = false;
pub const KEEPALIVE_WINDOW_DEFAULT_SECS: u64 = 1500;
pub const KEEPALIVE_WINDOW_MIN_SECS: u64 = 300;
pub const KEEPALIVE_WINDOW_MAX_SECS: u64 = 21_600;

/// Name prefix of a daemon-owned valve row; the full name is
/// `keepalive-{parent_session_id}`.
pub const KEEPALIVE_NAME_PREFIX: &str = "keepalive-";
/// Name prefix of the automatic per-child terminal watches the valve and the
/// hold read as the "children the parent waits on" ledger.
pub const CHILD_WATCH_NAME_PREFIX: &str = "agent-child-";

/// One held wake, for the Scheduled Jobs read side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledJobHoldV1 {
    pub job_id: Uuid,
    pub parent_session_id: Uuid,
    /// Logical children still running (rotation tips), oldest watch first.
    pub running_children: Vec<Uuid>,
    /// When the hold began (the wake's due time or the parent's last output,
    /// whichever is later).
    pub held_since: DateTime<Utc>,
    /// When the held wake is released once, even if a child still runs.
    pub release_at: DateTime<Utc>,
}
