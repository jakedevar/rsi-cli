//! Bounded, operator-only cross-project fleet snapshot.
use crate::types::Session;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetAgent {
    pub session: Session,
    pub project: String,
    pub role: String,
    /// Start of the current running invocation, when recorded.
    pub turn_started_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FleetUsage {
    pub invocations: u64,
    pub errors: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    /// Invocations lacking at least one usage field; totals are partial.
    pub unknown_usage: u64,
}
impl FleetUsage {
    pub fn tokens_per_minute(&self, seconds: i64) -> f64 {
        (self.input + self.output + self.cache_read + self.cache_write) as f64 * 60.0
            / seconds as f64
    }
    pub fn cost_per_hour(&self, seconds: i64) -> f64 {
        self.cost * 3600.0 / seconds as f64
    }
    pub fn error_pct(&self) -> f64 {
        if self.invocations == 0 {
            0.0
        } else {
            self.errors as f64 * 100.0 / self.invocations as f64
        }
    }
}
pub const FLEET_WINDOWS: [i64; 3] = [300, 3600, 86400];
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FleetGroup {
    /// project, provider, or model. Project keys are UUIDs, never names.
    pub dimension: String,
    pub key: String,
    pub label: String,
    pub active: u64,
    pub windows: [FleetUsage; 3],
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetOverview {
    pub as_of: DateTime<Utc>,
    pub agents: Vec<FleetAgent>,
    pub groups: Vec<FleetGroup>,
    pub totals: [FleetUsage; 3],
    pub agents_truncated: bool,
    pub usage_truncated: bool,
}
