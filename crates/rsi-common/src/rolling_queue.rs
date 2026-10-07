//! Wire types for the daemon-owned rolling merge queue (#1007).
//!
//! `AgentEnqueueLandingSource` is the only agent-facing verb; it admits one
//! accepted source commit. The queue settings and the read-only queue view are
//! operator-only (`daemon_config` fields and `GetRollingQueue`).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ROLLING_QUEUE_MAX_TEST_FILTERS: usize = 32;
pub const ROLLING_QUEUE_MAX_FILTER_BYTES: usize = 256;
pub const ROLLING_QUEUE_MAX_IDEMPOTENCY_BYTES: usize = 128;
pub const ROLLING_QUEUE_MIN_BATCH_SIZE: u64 = 1;
pub const ROLLING_QUEUE_MAX_BATCH_SIZE: u64 = 8;
pub const ROLLING_QUEUE_DEFAULT_BATCH_SIZE: u32 = 4;
pub const ROLLING_QUEUE_MAX_SPECULATION_DEPTH: u64 = 2;
pub const ROLLING_QUEUE_DEFAULT_SPECULATION_DEPTH: u32 = 1;
pub const ROLLING_QUEUE_LIST_MAX: usize = 200;
/// Wall-clock budget, in minutes, of one batch's gating (every lander run of
/// the batch, its bisect included). Operator setting
/// `rolling_queue_gate_timeout_mins` (#1208).
pub const ROLLING_QUEUE_MIN_GATE_TIMEOUT_MINS: u64 = 30;
pub const ROLLING_QUEUE_MAX_GATE_TIMEOUT_MINS: u64 = 1440;
pub const ROLLING_QUEUE_DEFAULT_GATE_TIMEOUT_MINS: u32 = 360;

/// Stable refusal codes; each is the whole `InvalidParam`/`PolicyDenied` text.
pub const QUEUE_DISABLED: &str = "queue_disabled";
pub const QUEUE_SOURCE_INVALID: &str = "queue_source_invalid";
pub const QUEUE_FILTER_INVALID: &str = "queue_filter_invalid";
pub const QUEUE_KEY_INVALID: &str = "queue_idempotency_key_invalid";
pub const QUEUE_DUPLICATE_SOURCE: &str = "queue_duplicate_source";
/// A test filter selects no test (refused at enqueue, or reported by the gate
/// when "no tests to run" slips past the static check). Names the filter.
pub const QUEUE_FILTER_MATCHES_NO_TESTS: &str = "filter_matches_no_tests";
pub const QUEUE_NOT_AUTHORIZED: &str = "queue_not_authorized";
pub const QUEUE_REGATE_EXHAUSTED: &str = "queue_out_of_band_regate_exhausted";
/// A source's merge onto the tip (or onto its batch predecessors) conflicted.
pub const QUEUE_BATCH_MERGE_CONFLICT: &str = "queue_batch_merge_conflict";
/// The lander's policy refused the source or its batch before any gate ran.
pub const QUEUE_BATCH_POLICY_REFUSED: &str = "queue_batch_policy_refused";
/// A migration-carrying source cannot take the next migration number in order.
pub const QUEUE_MIGRATION_OUT_OF_ORDER: &str = "queue_migration_out_of_order";
/// The published tip does not contain the source the batch reported landing.
pub const QUEUE_BATCH_ANCESTRY_UNVERIFIED: &str = "queue_batch_ancestry_unverified";
/// A red-batch bisect step failed to shrink the suspect window (a guard: the
/// remaining sources settle with this code rather than looping).
pub const QUEUE_BISECT_NO_PROGRESS: &str = "queue_bisect_no_progress";
/// The batch spent its whole gate wall-time budget
/// (`rolling_queue_gate_timeout_mins`); its unsettled members are refused
/// with this code instead of holding the queue (#1208).
pub const QUEUE_GATE_TIMEOUT: &str = "queue_gate_timeout";
/// The lander reported a source already integrated but the queue could not
/// confirm it on `origin/rolling` (#1208). A confirmed one settles published.
pub const QUEUE_SOURCE_ALREADY_INTEGRATED: &str = "queue_source_already_integrated";

/// Lifecycle of one queue entry. Strings match the SQLite CHECK exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollingQueueEntryState {
    Queued,
    Admitted,
    Gating,
    Published,
    Refused,
    Failed,
    Superseded,
}

impl RollingQueueEntryState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Admitted => "admitted",
            Self::Gating => "gating",
            Self::Published => "published",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "admitted" => Self::Admitted,
            "gating" => Self::Gating,
            "published" => Self::Published,
            "refused" => Self::Refused,
            "failed" => Self::Failed,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Published | Self::Refused | Self::Failed | Self::Superseded
        )
    }
}

/// Whether the source is bound to a ledger Work item. Reported, never required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollingQueueBinding {
    Bound,
    Unbound,
}

impl RollingQueueBinding {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Unbound => "unbound",
        }
    }
}

/// Typed per-source result delivered once to the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RollingQueueOutcome {
    pub landed_sha: Option<String>,
    /// Stable refusal code (`queue_*`, or a lander `policy_fence`).
    pub refusal: Option<String>,
    /// Failing test names the gate reported, if any.
    #[serde(default)]
    pub failing_tests: Vec<String>,
    /// Bounded tail of the lander's own message.
    pub detail: Option<String>,
}

/// One queue entry as reported to operators and enqueue receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollingQueueEntryV1 {
    pub id: Uuid,
    pub sequence: i64,
    pub source_commit: String,
    pub source_session_id: Uuid,
    pub owner_epic_id: Option<Uuid>,
    pub binding: RollingQueueBinding,
    pub work_key: Option<String>,
    pub migration_version: Option<u32>,
    pub hot_files: Vec<String>,
    pub test_filters: Vec<String>,
    pub state: RollingQueueEntryState,
    pub outcome: Option<RollingQueueOutcome>,
    pub enqueued_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEnqueueLandingSourceRequestV1 {
    pub source_commit: String,
    #[serde(default)]
    pub test_filters: Vec<String>,
    pub idempotency_key: String,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// #1235: land from this in-reach session's sandbox instead of the
    /// caller's own. The caller's manager scope must reach the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<Uuid>,
}

impl AgentEnqueueLandingSourceRequestV1 {
    /// Validate the source OID, `PACKAGE=FILTER` pairs and replay key.
    ///
    /// # Errors
    /// Returns the stable `queue_*` code for the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !is_full_lower_hex_oid(&self.source_commit) {
            return Err(QUEUE_SOURCE_INVALID);
        }
        if self.test_filters.len() > ROLLING_QUEUE_MAX_TEST_FILTERS
            || self.test_filters.iter().any(|f| !valid_filter(f))
        {
            return Err(QUEUE_FILTER_INVALID);
        }
        let key = &self.idempotency_key;
        if key.is_empty() || key.len() > ROLLING_QUEUE_MAX_IDEMPOTENCY_BYTES || key.contains('\0') {
            return Err(QUEUE_KEY_INVALID);
        }
        Ok(())
    }
}

fn is_full_lower_hex_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `PACKAGE=FILTER`, both non-empty, neither starting with `-` (the lander
/// refuses the same shapes; refusing here gives the owner an immediate answer).
fn valid_filter(value: &str) -> bool {
    let Some((package, filter)) = value.split_once('=') else {
        return false;
    };
    value.len() <= ROLLING_QUEUE_MAX_FILTER_BYTES
        && !package.is_empty()
        && !filter.is_empty()
        && !package.starts_with('-')
        && !filter.starts_with('-')
        && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEnqueueLandingSourceReceiptV1 {
    pub entry: RollingQueueEntryV1,
    /// True when an identical earlier request was replayed.
    pub replayed: bool,
}

/// Operator-only read of the queue and its live settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetRollingQueueParams {
    #[serde(default)]
    pub state: Option<RollingQueueEntryState>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollingQueueResponse {
    pub enabled: bool,
    pub batch_size: u32,
    pub speculation_depth: u32,
    /// The batch gate wall-time budget in minutes (#1208).
    #[serde(default)]
    pub gate_timeout_mins: u32,
    pub entries: Vec<RollingQueueEntryV1>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AgentEnqueueLandingSourceRequestV1 {
        AgentEnqueueLandingSourceRequestV1 {
            project_id: None,
            source_session_id: None,
            source_commit: "a".repeat(40),
            test_filters: vec!["rsid=rolling_queue".into()],
            idempotency_key: "k1".into(),
        }
    }

    #[test]
    fn valid_request_passes_and_unknown_fields_are_refused() {
        assert_eq!(request().validate(), Ok(()));
        let forged = serde_json::json!({
            "source_commit": "a".repeat(40), "idempotency_key": "k", "owner": "x"
        });
        assert!(serde_json::from_value::<AgentEnqueueLandingSourceRequestV1>(forged).is_err());
    }

    #[test]
    fn source_filter_and_key_are_validated() {
        let mut r = request();
        r.source_commit = "A".repeat(40);
        assert_eq!(r.validate(), Err(QUEUE_SOURCE_INVALID));
        r.source_commit = "abc".into();
        assert_eq!(r.validate(), Err(QUEUE_SOURCE_INVALID));
        for bad in ["nofilter", "=x", "p=", "-p=x", "p=-x"] {
            let mut r = request();
            r.test_filters = vec![bad.into()];
            assert_eq!(r.validate(), Err(QUEUE_FILTER_INVALID), "{bad}");
        }
        let mut r = request();
        r.idempotency_key = String::new();
        assert_eq!(r.validate(), Err(QUEUE_KEY_INVALID));
    }

    #[test]
    fn state_strings_round_trip() {
        for state in [
            RollingQueueEntryState::Queued,
            RollingQueueEntryState::Admitted,
            RollingQueueEntryState::Gating,
            RollingQueueEntryState::Published,
            RollingQueueEntryState::Refused,
            RollingQueueEntryState::Failed,
            RollingQueueEntryState::Superseded,
        ] {
            assert_eq!(RollingQueueEntryState::parse(state.as_str()), Some(state));
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
    }
}
