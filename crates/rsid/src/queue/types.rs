//! Types for the background task queue.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Task types that can be enqueued.
/// Each variant corresponds to a downstream processor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskType {
    /// Extract observations from session conversation events.
    ExtractObservations,
    /// Generate session summary (short and long).
    Summarize,
    /// Run dream consolidation (deduction/induction).
    Dream,
    /// Update entity card from new observations.
    UpdateCard,
    /// Reconcile embedding consistency.
    Reconcile,
}

impl TaskType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExtractObservations => "extract_observations",
            Self::Summarize => "summarize",
            Self::Dream => "dream",
            Self::UpdateCard => "update_card",
            Self::Reconcile => "reconcile",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "extract_observations" => Some(Self::ExtractObservations),
            "summarize" => Some(Self::Summarize),
            "dream" => Some(Self::Dream),
            "update_card" => Some(Self::UpdateCard),
            "reconcile" => Some(Self::Reconcile),
            _ => None,
        }
    }

    /// Default token threshold for this task type.
    /// Tasks with 0 threshold process immediately (no batching).
    pub fn default_token_threshold(&self) -> i64 {
        match self {
            Self::ExtractObservations => 1024,
            Self::Summarize => 2048,
            Self::Dream => 0,      // Scheduled, not token-gated
            Self::UpdateCard => 0, // Triggered by observation count
            Self::Reconcile => 0,  // Timer-driven
        }
    }
}

impl fmt::Display for TaskType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Configuration for the background queue.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Polling interval in seconds. Default: 30.
    pub poll_interval_secs: u64,
    /// Default token threshold for batching. Default: 1024.
    pub default_token_threshold: i64,
    /// Stale claim timeout in seconds. Default: 300 (5 minutes).
    pub stale_claim_timeout_secs: i64,
    /// Max retry attempts per task. Default: 5.
    pub max_attempts: i32,
    /// Retention period for completed items in seconds. Default: 86400 (24 hours).
    pub completed_retention_secs: i64,
    /// Whether the queue is enabled. Default: true.
    pub enabled: bool,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 30,
            default_token_threshold: 1024,
            stale_claim_timeout_secs: 300,
            max_attempts: 5,
            completed_retention_secs: 86400,
            enabled: true,
        }
    }
}

/// Composite work unit key for task grouping.
pub fn make_work_unit_key(
    task_type: TaskType,
    project_id: Option<&str>,
    session_id: &str,
) -> String {
    let project = project_id.unwrap_or("_");
    format!("{}:{}:{}", task_type.as_str(), project, session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_type_as_str_roundtrip() {
        let variants = [
            TaskType::ExtractObservations,
            TaskType::Summarize,
            TaskType::Dream,
            TaskType::UpdateCard,
            TaskType::Reconcile,
        ];
        for variant in &variants {
            let s = variant.as_str();
            let parsed = TaskType::from_str(s);
            assert_eq!(parsed, Some(*variant), "roundtrip failed for {s}");
        }
    }

    #[test]
    fn test_task_type_from_str_unknown() {
        assert_eq!(TaskType::from_str("nonexistent"), None);
        assert_eq!(TaskType::from_str(""), None);
    }

    #[test]
    fn test_make_work_unit_key_format() {
        let key = make_work_unit_key(TaskType::ExtractObservations, Some("proj-123"), "sess-456");
        assert_eq!(key, "extract_observations:proj-123:sess-456");
    }

    #[test]
    fn test_make_work_unit_key_no_project() {
        let key = make_work_unit_key(TaskType::Summarize, None, "sess-789");
        assert_eq!(key, "summarize:_:sess-789");
    }

    #[test]
    fn test_queue_config_default() {
        let config = QueueConfig::default();
        assert_eq!(config.poll_interval_secs, 30);
        assert_eq!(config.default_token_threshold, 1024);
        assert_eq!(config.stale_claim_timeout_secs, 300);
        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.completed_retention_secs, 86400);
        assert!(config.enabled);
    }

    #[test]
    fn test_task_type_serde_roundtrip() {
        let original = TaskType::ExtractObservations;
        let json = serde_json::to_string(&original).unwrap();
        let deser: TaskType = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deser);
    }

    #[test]
    fn test_task_type_display() {
        assert_eq!(format!("{}", TaskType::Dream), "dream");
        assert_eq!(format!("{}", TaskType::UpdateCard), "update_card");
    }
}
