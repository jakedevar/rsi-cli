//! `AgentListWakes` result (#2389): the caller's own scheduled wakes with
//! everything a successor seat needs to confirm what carried over (#2021)
//! without reading `scheduled_jobs` by SQL: mode, watched session, the
//! `when` predicate and its deadline, the next fire time, and whether a
//! delivered message (a manager report, notice or ruling) is waiting inside
//! the wake.
//!
//! The owner is the transport-bound caller unless the request names a
//! `session_id` (#2418): then only a live worker the caller's
//! `AgentWorkerRoster` scope covers is read, and any other session is refused
//! with [`LIST_WAKES_TARGET_NOT_AUTHORIZED`].

use crate::wake_predicate::WakePredicate;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Refusal code for a `session_id` outside the caller's live roster scope.
pub const LIST_WAKES_TARGET_NOT_AUTHORIZED: &str = "list_wakes_target_not_authorized";

/// Longest prefix, in characters, of a pending message the listing carries.
/// The whole message is delivered when the wake fires.
pub const WAKE_PENDING_PREVIEW_CHARS: usize = 400;

/// A message delivered into a wake that has not fired yet (for example a
/// manager report queued as a `resume` wake on the receiving seat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWakePendingMessageV1 {
    pub message_id: Uuid,
    /// `report`, `escalation`, `ruling` or `message`.
    pub kind: String,
    /// Who sent it, as the routing layer names the source.
    pub from: String,
    /// `queued` (not yet claimed) or `claimed` (taken, delivery unsettled).
    pub state: String,
    /// The first [`WAKE_PENDING_PREVIEW_CHARS`] characters of the delivery.
    pub preview: String,
}

/// One wake owned by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWakeV1 {
    pub job_id: Uuid,
    pub name: String,
    /// `fresh`, `resume`, `on_terminal` or `when`.
    pub mode: String,
    /// The watched session of an `on_terminal` wake.
    pub watch_session_id: Option<Uuid>,
    /// The predicate of a `when` wake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<WakePredicate>,
    pub next_fire_at: DateTime<Utc>,
    /// When a `when` wake fires with `timed_out: true` if still pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    /// Daemon-owned (program guard, manager watch, background process): not
    /// cancellable by the owner.
    pub protected: bool,
    /// Present when a delivered message is waiting in this wake.
    pub pending_message: Option<AgentWakePendingMessageV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentListWakesResultV1 {
    pub wakes: Vec<AgentWakeV1>,
    pub count: usize,
    pub truncated: bool,
}
