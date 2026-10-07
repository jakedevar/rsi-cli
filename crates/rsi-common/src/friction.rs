//! Friction telemetry, the "andon" (#1333): the daemon records structural and
//! process friction (refusals, timeouts, failed handoffs, lander refusals) as
//! append-only data, rolls it up for managers and the operator, and files one
//! deduplicated kaizen Issue when a signature repeats.
//!
//! Privacy: an event carries a signature built from stable codes, a session,
//! a project and one evidence reference (`deploy:<uuid>`, `session:<uuid>`,
//! ...). Never prompt text, message bodies, error prose or secrets: every
//! signature segment and evidence reference must pass the token checks below,
//! and a segment that does not is recorded as [`UNCLASSIFIED`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The operator-only friction RPCs (never in an agent catalog). Agents read
/// the rollup through `AgentManagerInspect {section:"friction"}`.
pub const OPERATOR_METHODS: [&str; 1] = ["ListFrictionRollup"];

/// The segment recorded for a code that is not a clean token.
pub const UNCLASSIFIED: &str = "unclassified";

/// Occurrences of one signature within the window that make it due.
pub const ANDON_MIN_OCCURRENCES: u64 = 3;
/// Distinct sessions the occurrences must span (one looping session is not a
/// structural signal).
pub const ANDON_MIN_SESSIONS: u64 = 2;
/// The rolling window, in hours, for both the threshold and the rollup default.
pub const ANDON_WINDOW_HOURS: u32 = 24;
/// Most Issues the andon files in any rolling 24 hours, across the daemon.
pub const ANDON_DAILY_FILING_CAP: u64 = 5;
/// Labels on every auto-filed Issue.
pub const ANDON_ISSUE_LABELS: [&str; 2] = ["kaizen", "andon"];
/// Longest rollup window the operator RPC accepts (30 days).
pub const FRICTION_MAX_WINDOW_HOURS: u32 = 720;
/// Default and largest rollup page.
pub const FRICTION_ROLLUP_DEFAULT_LIMIT: u32 = 50;
pub const FRICTION_ROLLUP_MAX_LIMIT: u32 = 200;
/// Longest signature and evidence reference (bytes).
pub const FRICTION_SIGNATURE_MAX_BYTES: usize = 160;
pub const FRICTION_EVIDENCE_MAX_BYTES: usize = 128;
const CODE_MAX_BYTES: usize = 64;

/// #1337 CPU-time andon, operator setting `cpu_andon_cpu_minutes`: one agent
/// process tree past this many CPU-minutes is a runaway. 0 turns it off.
pub const CPU_ANDON_CPU_MINUTES_DEFAULT: u32 = 240;
pub const CPU_ANDON_CPU_MINUTES_MIN: u32 = 30;
pub const CPU_ANDON_CPU_MINUTES_MAX: u32 = 10_000;
/// #1337, operator setting `cpu_andon_host_load`: at or above this 1-minute
/// host load, one tree using a dominant share of it is a runaway. 0 turns it
/// off. The 2026-10-07 load rule queues build work above 40.
pub const CPU_ANDON_HOST_LOAD_DEFAULT: u32 = 40;
pub const CPU_ANDON_HOST_LOAD_MIN: u32 = 4;
pub const CPU_ANDON_HOST_LOAD_MAX: u32 = 1024;

/// The recording points. The kind is the first signature segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrictionKind {
    /// One session repeated an identical tool error at least three times.
    ToolError,
    /// A terminal assistant message reported friction without an Issue.
    Handoff,
    /// A tokened `Agent*` verb returned an error: `agent_refusal:<verb>:<code>`.
    AgentRefusal,
    /// A deploy reached its deadline without a quiet point:
    /// `deploy_timeout:<blocker>`.
    DeployTimeout,
    /// #1461: a deploy restart interrupted a worker still mid-turn
    /// (`AgentRequestDeploy {interrupt_workers: true}`):
    /// `deploy_interrupt:<blocker>`, one event per interrupted worker session.
    DeployInterrupt,
    /// A session ended Failed for a structural terminal reason:
    /// `terminal:<stop_reason>`.
    Terminal,
    /// The merge queue refused or failed a landing: `lander:<state>:<code>`.
    Lander,
    /// A background job was lost, or a landing job failed:
    /// `agent_job:<kind>:<state>`.
    AgentJob,
    /// #1337: an agent-owned process tree passed the CPU-time andon, or a
    /// test job ran past its timeout: `runaway_process:<tree>:<reason>`.
    RunawayProcess,
    /// One sandbox Cargo cache was staged for reclamation.
    SandboxTargetReclaim,
}

impl FrictionKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolError => "tool_error",
            Self::Handoff => "handoff",
            Self::AgentRefusal => "agent_refusal",
            Self::DeployTimeout => "deploy_timeout",
            Self::DeployInterrupt => "deploy_interrupt",
            Self::Terminal => "terminal",
            Self::Lander => "lander",
            Self::AgentJob => "agent_job",
            Self::RunawayProcess => "runaway_process",
            Self::SandboxTargetReclaim => "sandbox_target_reclaim",
        }
    }
}

/// True for a clean code token: ASCII letter first, then letters, digits and
/// `_`, at most 64 bytes. Verb names (`AgentGetIssue`) and refusal codes
/// (`agent_issue_authority_denied`) pass; prose, paths and JSON do not.
#[must_use]
pub fn is_friction_code(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= CODE_MAX_BYTES
        && bytes[0].is_ascii_alphabetic()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// `raw` when it is a clean code, else [`UNCLASSIFIED`].
#[must_use]
pub fn friction_code(raw: &str) -> &str {
    if is_friction_code(raw) {
        raw
    } else {
        UNCLASSIFIED
    }
}

/// `<kind>:<segment>:...`, each segment passed through [`friction_code`].
#[must_use]
pub fn friction_signature(kind: FrictionKind, segments: &[&str]) -> String {
    let mut signature = kind.as_str().to_string();
    for segment in segments {
        signature.push(':');
        signature.push_str(friction_code(segment));
    }
    signature
}

/// The kind segment of a stored signature (`agent_refusal`, ...).
#[must_use]
pub fn signature_kind(signature: &str) -> &str {
    signature.split(':').next().unwrap_or(signature)
}

/// True for a signature this module could have built: code segments joined
/// by `:`, within [`FRICTION_SIGNATURE_MAX_BYTES`]. The store refuses others.
#[must_use]
pub fn is_friction_signature(signature: &str) -> bool {
    signature.len() <= FRICTION_SIGNATURE_MAX_BYTES
        && signature.contains(':')
        && signature.split(':').all(is_friction_code)
}

/// True for an evidence reference: `<code>:<id>` where the id is letters,
/// digits, `-`, `_` or `.` (a UUID, a short SHA, a job id).
#[must_use]
pub fn is_friction_evidence(reference: &str) -> bool {
    let Some((label, id)) = reference.split_once(':') else {
        return false;
    };
    reference.len() <= FRICTION_EVIDENCE_MAX_BYTES
        && is_friction_code(label)
        && !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// One event to record. The store fills `project_id` from the session when
/// it is absent and drops an evidence reference that is not clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFrictionEventV1 {
    pub signature: String,
    pub session_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub evidence_ref: Option<String>,
}

impl NewFrictionEventV1 {
    #[must_use]
    pub fn new(kind: FrictionKind, segments: &[&str]) -> Self {
        Self {
            signature: friction_signature(kind, segments),
            session_id: None,
            project_id: None,
            evidence_ref: None,
        }
    }

    #[must_use]
    pub fn session(mut self, session_id: Option<Uuid>) -> Self {
        self.session_id = session_id;
        self
    }

    #[must_use]
    pub fn evidence(mut self, label: &str, id: impl std::fmt::Display) -> Self {
        let reference = format!("{label}:{id}");
        self.evidence_ref = is_friction_evidence(&reference).then_some(reference);
        self
    }
}

/// Operator-only `ListFrictionRollup`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListFrictionRollupRequestV1 {
    /// One project only; omit for every project and the project-less rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// Window in hours (1..=720); default 24.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_hours: Option<u32>,
    /// Most rows (1..=200); default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One signature's rollup within the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrictionRollupRowV1 {
    pub project_id: Option<Uuid>,
    pub signature: String,
    pub kind: String,
    pub occurrences: u64,
    pub sessions: u64,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
    /// Newest distinct evidence references, at most five.
    pub evidence_refs: Vec<String>,
    /// The Issue the andon filed for this signature, if any (ever).
    pub filed_issue_id: Option<Uuid>,
    pub filed_display_number: Option<u64>,
    /// Over the threshold and not yet filed, so the next sweep files it
    /// unless the daily cap is spent. Evaluated only for the andon's 24 h
    /// window and for rows with a project; false otherwise.
    pub due: bool,
}

/// `ListFrictionRollup` result: rows by occurrences, most first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFrictionRollupResultV1 {
    pub window_hours: u32,
    pub observed_at: DateTime<Utc>,
    pub rows: Vec<FrictionRollupRowV1>,
    /// More signatures exist past `limit`.
    pub truncated: bool,
    /// Issues the andon filed in the last 24 hours, and the cap.
    pub filings_last_24h: u64,
    pub daily_filing_cap: u64,
}

/// One Issue the andon filed in a sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndonFilingV1 {
    pub project_id: Uuid,
    pub signature: String,
    pub issue_id: Uuid,
    pub display_number: u64,
    pub occurrences: u64,
    pub sessions: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_keep_codes_and_drop_prose() {
        assert_eq!(
            friction_signature(
                FrictionKind::AgentRefusal,
                &["AgentGetIssue", "agent_issue_authority_denied"]
            ),
            "agent_refusal:AgentGetIssue:agent_issue_authority_denied"
        );
        assert_eq!(
            friction_signature(FrictionKind::Lander, &["refused", "token sk-abc: oops"]),
            "lander:refused:unclassified"
        );
        assert!(is_friction_signature("deploy_timeout:worker_mid_turn"));
        assert_eq!(
            friction_signature(FrictionKind::DeployInterrupt, &["worker_mid_turn"]),
            "deploy_interrupt:worker_mid_turn"
        );
        assert_eq!(
            friction_signature(FrictionKind::SandboxTargetReclaim, &["removed"]),
            "sandbox_target_reclaim:removed"
        );
        assert!(!is_friction_signature("deploy_timeout"));
        assert!(!is_friction_signature("terminal:has space"));
        assert_eq!(signature_kind("lander:refused:x"), "lander");
    }

    #[test]
    fn evidence_is_a_labelled_id_only() {
        let event = NewFrictionEventV1::new(FrictionKind::DeployTimeout, &["worker_mid_turn"])
            .evidence("deploy", Uuid::nil());
        assert_eq!(
            event.evidence_ref.as_deref(),
            Some("deploy:00000000-0000-0000-0000-000000000000")
        );
        let prose =
            NewFrictionEventV1::new(FrictionKind::Terminal, &["x"]).evidence("session", "a b/c");
        assert_eq!(prose.evidence_ref, None);
    }

    #[test]
    fn rollup_request_rejects_unknown_fields() {
        assert!(
            serde_json::from_value::<ListFrictionRollupRequestV1>(
                serde_json::json!({"window_hours": 24, "prompt": "x"})
            )
            .is_err()
        );
    }
}
