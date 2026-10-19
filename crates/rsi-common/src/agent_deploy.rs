//! `AgentRequestDeploy` (#1045 slice 2): wire types for a daemon-owned deploy.
//!
//! The current appointed manager, holding the operator-granted `Deploy`
//! capability, names a commit SHA and a directory of already-built binaries.
//! The daemon stages verified copies, waits for a quiet point, swaps them in,
//! restarts under `rsid-supervisor.sh` (exit 75, environment intact) and wakes
//! the caller once with the outcome. Nothing here carries an environment value,
//! credential or command line.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Artifacts the daemon may replace, by file name. `rsid` is mandatory; every
/// other member is deployed (and sha-verified) when present in the binaries
/// directory and reported in the receipt's `skipped` list when absent (#1110).
/// A missing `cargo-slot` or `target-pool` is never silent either: it is named
/// in `skipped` like any other absent member (#2722).
pub const DEPLOY_BINARIES: [&str; 14] = [
    "rsid",
    "rsi",
    "rsi-rpc",
    "rsi-agent-mcp",
    "rsi-build-rustc",
    "rsi-contract-validate",
    "rsi-rolling-land",
    "rsi-remote",
    "rsi-turn-shim",
    "rsi-socket-hold",
    "rsid-supervisor.sh",
    "cargo-slot",
    "target-pool",
    "rsi-spill",
];

/// The scripts the daemon executes from `~/.rsi/bin` (not from the install
/// directory the binaries go to): `cargo-slot` wraps every job's cargo unit and
/// `target-pool` seeds each worker sandbox's target (#1741). `rsi-spill` is the
/// stable copy `~/.local/bin/rsi-spill` links (hub #2958). A deploy publishes
/// them from the binaries directory next to the supervisor (#2722).
pub const DEPLOY_TOOL_SCRIPTS: [&str; 3] = ["cargo-slot", "target-pool", "rsi-spill"];

/// Whether `name` is one of [`DEPLOY_TOOL_SCRIPTS`].
#[must_use]
pub fn is_deploy_tool_script(name: &str) -> bool {
    DEPLOY_TOOL_SCRIPTS.contains(&name)
}

/// Managed binaries a deploy did not carry: those named in `DEPLOY_BINARIES`
/// with no entry in `manifest`, in managed-set order.
#[must_use]
pub fn skipped_binaries(manifest: &[DeployBinaryV1]) -> Vec<String> {
    DEPLOY_BINARIES
        .iter()
        .filter(|name| !manifest.iter().any(|entry| entry.name == **name))
        .map(|name| (*name).to_string())
        .collect()
}
pub const DEPLOY_DEFAULT_MAX_WAIT_SECS: u32 = 900;
pub const DEPLOY_MAX_WAIT_SECS: u32 = 3600;
pub const DEPLOY_MAX_KEY_BYTES: usize = 128;
/// Default of the operator setting `deploy_restarts_per_hour` (#1777): the
/// deploy restarts allowed in a rolling hour. The supervisor adds a separate
/// rollback/watchdog restart reserve to this value.
pub const DEPLOY_RESTARTS_PER_HOUR_DEFAULT: u64 = 2;
/// Lower bound of `deploy_restarts_per_hour`.
pub const DEPLOY_RESTARTS_PER_HOUR_MIN: u64 = 1;
/// Upper bound of `deploy_restarts_per_hour`. The supervisor adds one
/// rollback/watchdog reserve above this value, with a total ceiling of 13.
pub const DEPLOY_RESTARTS_PER_HOUR_MAX: u64 = 12;
/// Additional supervisor restart reserved for rollback/watchdog recovery.
pub const SUPERVISOR_RESTART_RESERVE: u64 = 1;
/// Absolute supervisor restart budget ceiling, including the reserve.
pub const SUPERVISOR_RESTART_BUDGET_MAX: u64 = 13;
/// Default of the operator setting `supervisor_shutdown_grace_secs` (#2561):
/// how long the supervisor lets a daemon shutdown run before it sends SIGTERM
/// again, then SIGKILL to the daemon's process group, and restarts it.
pub const SUPERVISOR_SHUTDOWN_GRACE_DEFAULT_SECS: u64 = 120;
/// Lower bound of `supervisor_shutdown_grace_secs`: the daemon's own bounded
/// shutdown (operator drain plus stages) must be able to finish first.
pub const SUPERVISOR_SHUTDOWN_GRACE_MIN_SECS: u64 = 60;
/// Upper bound of `supervisor_shutdown_grace_secs`.
pub const SUPERVISOR_SHUTDOWN_GRACE_MAX_SECS: u64 = 3600;
/// Marker the `deploy_restart_budget` refusal carries before the RFC3339
/// time the budget next has room.
pub const DEPLOY_RETRY_AFTER_MARKER: &str = " retry_after=";

pub const DEPLOY_NOT_AUTHORIZED: &str = "deploy_not_authorized";
pub const DEPLOY_CAPABILITY_REQUIRED: &str = "deploy_capability_required";
pub const DEPLOY_EXECUTE_REQUIRED: &str = "deploy_execute_required";
pub const DEPLOY_INVALID_REQUEST: &str = "deploy_invalid_request";
pub const DEPLOY_SHA_INVALID: &str = "deploy_sha_invalid";
pub const DEPLOY_KEY_INVALID: &str = "deploy_idempotency_key_invalid";
pub const DEPLOY_KEY_CONFLICT: &str = "deploy_idempotency_key_conflict";
pub const DEPLOY_SOURCE_REQUIRED: &str = "deploy_source_required";
pub const DEPLOY_SOURCE_AMBIGUOUS: &str = "deploy_source_ambiguous";
pub const DEPLOY_BUILD_NOT_SUPPORTED: &str = "deploy_build_not_supported";
pub const DEPLOY_DIR_NOT_ALLOWED: &str = "deploy_directory_not_allowed";
pub const DEPLOY_BINARY_MISSING: &str = "deploy_binary_missing";
pub const DEPLOY_STAGE_FAILED: &str = "deploy_stage_failed";
pub const DEPLOY_SHA_MISMATCH: &str = "deploy_sha_mismatch";
pub const DEPLOY_SCHEMA_DOWNGRADE: &str = "deploy_schema_downgrade";
pub const DEPLOY_NEEDS_SUPERVISOR: &str = "deploy_needs_supervisor";
/// The supervisor runs an rsid outside the directory a deploy installs into, so
/// a restart would relaunch the old binary (#1164). The message names both paths.
pub const DEPLOY_TARGET_MISMATCH: &str = "deploy_target_mismatch";
pub const DEPLOY_RESTART_BUDGET: &str = "deploy_restart_budget";
/// A staged copy no longer matches its verified hash at swap time.
pub const DEPLOY_STAGED_CHANGED: &str = "deploy_staged_copy_changed";
pub const DEPLOY_IN_PROGRESS: &str = "deploy_already_in_progress";
/// #1320/#1311: the settled reason of a deploy its owner cancelled while it
/// waited (`cancel: true`); the deploy settles `failed` with this reason.
pub const DEPLOY_CANCELLED: &str = "deploy_cancelled";
/// A cancel arrived after the deploy started its restart: too late to stop.
pub const DEPLOY_CANCEL_TOO_LATE: &str = "deploy_cancel_too_late";
/// A cancel named no deploy of the caller under that idempotency key.
pub const DEPLOY_NOT_FOUND: &str = "deploy_not_found";
/// Default (seconds) of the operator setting `deploy_drain_hold_secs`: how
/// long an agent deploy may hold new worker starts while it waits.
pub const DEPLOY_DRAIN_HOLD_DEFAULT_SECS: u64 = 600;
/// #2048: how long a staged `interrupt_workers` deploy waits for running
/// `test`/`scoped_test` jobs (counted from the request) before it stops them
/// with `interrupted_by_deploy`. A fixed policy, not an operator setting: it
/// only bounds how long re-runnable verification may hold the line, and the
/// receipt reports it. Build and landing jobs are never stopped.
pub const DEPLOY_JOB_GRACE_SECS: u64 = 300;
/// #2278: after a staged `interrupt_workers` deploy times out, new and
/// requeued `test`/`build` jobs stay held this long (seconds), so an immediate
/// retry of the deploy meets the same held set instead of a fresh crop of
/// running jobs. A retry inside the window keeps the hold through its own life;
/// with none, the hold ends when the window does.
pub const DEPLOY_JOB_RETRY_HOLD_SECS: u64 = 300;
/// Upper bound of `deploy_drain_hold_secs` (a deploy waits at most this long).
pub const DEPLOY_DRAIN_HOLD_MAX_SECS: u64 = DEPLOY_MAX_WAIT_SECS as u64;
/// Satellite deploy: the satellite has no local session to own the deploy
/// (its operator has declared no inbound scope root that exists).
pub const SATELLITE_DEPLOY_OWNER_REQUIRED: &str = "satellite_deploy_owner_required";
/// Satellite deploy: no enabled link reached the paired satellite.
pub const SATELLITE_DEPLOY_UNREACHABLE: &str = "satellite_deploy_unreachable";
/// Satellite deploy (#2587): `interrupt_workers` was asked of a satellite whose
/// build does not advertise `deploy_interrupt`. Nothing was sent: dropping the
/// option would leave the deploy waiting on a mid-turn worker until it timed out.
pub const SATELLITE_DEPLOY_INTERRUPT_UNSUPPORTED: &str = "satellite_deploy_interrupt_unsupported";

/// Lifecycle state. Strings match the SQLite CHECK exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployState {
    Staged,
    Restarting,
    Succeeded,
    Failed,
    TimedOut,
}

impl DeployState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Restarting => "restarting",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "staged" => Self::Staged,
            "restarting" => Self::Restarting,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "timed_out" => Self::TimedOut,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::TimedOut)
    }
}

/// Strict request. Exactly one of `binaries_dir` or `build: true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRequestDeployRequestV1 {
    /// Full lowercase 40-hex commit SHA the binaries were built from.
    pub sha: String,
    /// Absolute directory holding freshly built binaries (`rsid` at least).
    #[serde(default)]
    pub binaries_dir: Option<String>,
    /// Reserved: build at `sha` through the job path. Not supported yet.
    #[serde(default)]
    pub build: Option<bool>,
    pub idempotency_key: String,
    /// Bound on the wait for a quiet point; default 900, at most 3600.
    #[serde(default)]
    pub max_wait_secs: Option<u32>,
    /// #1017 slice 2: deploy on this paired satellite (over the hub link)
    /// instead of on this daemon. `binaries_dir` is then a path on the
    /// satellite. The hub keeps no state: the satellite's own deploy row
    /// (idempotency key included) is the record.
    #[serde(default)]
    pub peer_id: Option<crate::satellite::SatelliteUuidV1>,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// #1320/#1311: cancel the caller's own waiting deploy recorded under
    /// `idempotency_key` (and `sha`) instead of requesting one. Stops its hold
    /// on new launches at once; refused `deploy_cancel_too_late` once the
    /// restart began. With `peer_id`, the paired satellite applies the same
    /// owner and idempotency checks to its own stored deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    /// #1461: once the operator's drain hold (`deploy_drain_hold_secs`) is over,
    /// a worker still mid-turn no longer blocks the quiet point: the restart
    /// interrupts it and the existing post-restart path resumes the turn with
    /// a continue prompt. A landing and a local test/build/landing job still
    /// block. Default false (workers block, as always).
    /// The outcome lists the interrupted worker session ids and each one is an
    /// andon friction event. With `peer_id` (#2587) the satellite applies the
    /// same semantics to its own workers, provided it advertises the
    /// `deploy_interrupt` capability; an older satellite is refused
    /// `satellite_deploy_interrupt_unsupported` rather than silently waiting.
    /// Not available with `cancel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_workers: Option<bool>,
}

impl AgentRequestDeployRequestV1 {
    /// True for a cancel request.
    #[must_use]
    pub fn is_cancel(&self) -> bool {
        self.cancel == Some(true)
    }

    /// True when workers mid-turn may be interrupted after the drain hold.
    #[must_use]
    pub fn interrupts_workers(&self) -> bool {
        self.interrupt_workers == Some(true)
    }

    /// # Errors
    /// A stable `deploy_*` code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.sha.len() != 40
            || !self
                .sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(DEPLOY_SHA_INVALID);
        }
        if self.idempotency_key.is_empty() || self.idempotency_key.len() > DEPLOY_MAX_KEY_BYTES {
            return Err(DEPLOY_KEY_INVALID);
        }
        if self
            .max_wait_secs
            .is_some_and(|wait| wait == 0 || wait > DEPLOY_MAX_WAIT_SECS)
        {
            return Err(DEPLOY_INVALID_REQUEST);
        }
        if self.is_cancel() {
            // A cancel names an existing deploy; the source fields are ignored.
            return if self.interrupts_workers() {
                Err(DEPLOY_INVALID_REQUEST)
            } else {
                Ok(())
            };
        }
        match (self.binaries_dir.as_deref(), self.build) {
            (Some(_), Some(true)) => Err(DEPLOY_SOURCE_AMBIGUOUS),
            (None, Some(true)) => Err(DEPLOY_BUILD_NOT_SUPPORTED),
            (Some(dir), _) if dir.is_empty() || !dir.starts_with('/') => {
                Err(DEPLOY_DIR_NOT_ALLOWED)
            }
            (Some(_), _) => Ok(()),
            (None, _) => Err(DEPLOY_SOURCE_REQUIRED),
        }
    }

    #[must_use]
    pub fn wait_secs(&self) -> u32 {
        self.max_wait_secs.unwrap_or(DEPLOY_DEFAULT_MAX_WAIT_SECS)
    }
}

/// One staged binary: its install path and the sha256 of the verified copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployBinaryV1 {
    pub name: String,
    pub dest: String,
    pub sha256: String,
    /// Whether the install path held a binary when the deploy was staged. A
    /// failed deploy restores exactly that set: a binary that was absent is
    /// removed again, not left behind (#1114). Rows recorded before this field
    /// existed read as present, which keeps the old restore-`.prev` behaviour.
    #[serde(default = "default_prior_present")]
    pub prior_present: bool,
    /// The identity of the verified staged file, which the swap renames (so it
    /// is also the identity of the installed file). A rollback removes a
    /// first-time install only when the object at the path still has this
    /// identity: a replacement installed by anyone else is preserved (#1127).
    /// Rows recorded before this field existed carry none, and then nothing is
    /// removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_identity: Option<ObjectIdentityV1>,
}

/// A file object's identity: device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectIdentityV1 {
    pub dev: u64,
    pub ino: u64,
}

fn default_prior_present() -> bool {
    true
}

/// #2056: one thing a staged deploy waits for, with the ids that name it so
/// the owner needs no source or SQL to find it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployBlockerV1 {
    /// `landing_in_progress`, `job_running` or `worker_mid_turn`.
    pub kind: String,
    /// What holds the quiet point, oldest first; capped at
    /// [`DEPLOY_BLOCKER_SUBJECTS_MAX`] (`subjects_total` is the full count).
    #[serde(default)]
    pub subjects: Vec<DeployBlockerSubjectV1>,
    #[serde(default)]
    pub subjects_total: u32,
}

/// Most subjects listed per blocker kind.
pub const DEPLOY_BLOCKER_SUBJECTS_MAX: usize = 8;

/// One subject of a [`DeployBlockerV1`]; only the fields of its kind are set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployBlockerSubjectV1 {
    /// `landing_in_progress`: the rolling queue entry's sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_sequence: Option<i64>,
    /// `landing_in_progress`: `admitted` or `gating`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_state: Option<String>,
    /// `landing_in_progress`: the gating batch, once the entry has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<Uuid>,
    /// `job_running`: the job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_kind: Option<String>,
    /// `job_running`: the session that owns the job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<Uuid>,
    /// `worker_mid_turn`: the worker session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    /// RFC3339 nanos: when the subject entered that state (queue enqueue,
    /// job start); absent for a worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

/// Receipt: the deploy row as accepted (or replayed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequestDeployReceiptV1 {
    pub deploy_id: Uuid,
    pub state: DeployState,
    pub sha: String,
    /// RFC3339 nanos: the quiet-point wait ends here.
    pub deadline_at: String,
    pub binaries: Vec<DeployBinaryV1>,
    /// Managed binaries absent from `binaries_dir`, so left as installed.
    #[serde(default)]
    pub skipped: Vec<String>,
    /// True when an earlier call with the same key already created this row.
    pub replayed: bool,
    /// The settled reason (for example `deploy_cancelled`); absent while live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// #1461: the request accepted interrupting workers still mid-turn once
    /// the drain hold is over.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupt_workers: bool,
    /// #1461: worker session ids that were still mid-turn when the restart
    /// began (each gets a continue prompt after it); empty until the restart.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interrupted_workers: Vec<Uuid>,
    /// #2048: with `interrupt_workers`, the grace (seconds from the request)
    /// after which running `test`/`scoped_test` jobs are stopped as
    /// `interrupted_by_deploy`; absent without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_grace_secs: Option<u64>,
    /// #2048: the jobs this deploy stopped; empty until the grace passes. A test
    /// job it stopped is requeued under its own id (#2278): the owner's wake
    /// fires once, when the rerun ends. A hub QA run settles
    /// `interrupted_by_deploy` instead and wakes its owner at once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cancelled_jobs: Vec<Uuid>,
    /// #2056: while the deploy is `staged`, what holds its quiet point right
    /// now, read live on each call (a replay is the way to ask again). An
    /// `interrupt_workers` deploy stops waiting for `worker_mid_turn` once the
    /// operator's drain hold is over.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<DeployBlockerV1>,
    /// #2056: RFC3339 nanos the deploy was requested, so the owner sees how
    /// long it has waited; set with `blockers` while staged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_since: Option<String>,
    /// #2743: warnings about the deployed SHA. `not_qa_green` means the SHA is
    /// not covered by the latest full-library-green SHA of `AgentListRollingQueue`
    /// (`qa_green`); it names the open `qa` Issues. The deploy is not refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub qa_warnings: Vec<crate::rolling_queue::DeployQaWarningV1>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AgentRequestDeployRequestV1 {
        AgentRequestDeployRequestV1 {
            project_id: None,
            sha: "a".repeat(40),
            binaries_dir: Some("/tmp/bin".into()),
            build: None,
            idempotency_key: "k".into(),
            max_wait_secs: None,
            peer_id: None,
            cancel: None,
            interrupt_workers: None,
        }
    }

    #[test]
    fn request_validation_is_typed() {
        assert_eq!(request().validate(), Ok(()));
        let mut bad = request();
        bad.sha = "A".repeat(40);
        assert_eq!(bad.validate(), Err(DEPLOY_SHA_INVALID));
        let mut none = request();
        none.binaries_dir = None;
        assert_eq!(none.validate(), Err(DEPLOY_SOURCE_REQUIRED));
        none.build = Some(true);
        assert_eq!(none.validate(), Err(DEPLOY_BUILD_NOT_SUPPORTED));
        let mut both = request();
        both.build = Some(true);
        assert_eq!(both.validate(), Err(DEPLOY_SOURCE_AMBIGUOUS));
        let mut relative = request();
        relative.binaries_dir = Some("bin".into());
        assert_eq!(relative.validate(), Err(DEPLOY_DIR_NOT_ALLOWED));
        let mut wait = request();
        wait.max_wait_secs = Some(DEPLOY_MAX_WAIT_SECS + 1);
        assert_eq!(wait.validate(), Err(DEPLOY_INVALID_REQUEST));
        let mut cancel = request();
        cancel.binaries_dir = None;
        cancel.cancel = Some(true);
        assert_eq!(cancel.validate(), Ok(()), "a cancel needs no source");
        cancel.peer_id = Some(crate::satellite::SatelliteUuidV1(Uuid::new_v4()));
        assert_eq!(cancel.validate(), Ok(()), "peer cancellation is supported");
        assert!(
            serde_json::from_value::<AgentRequestDeployRequestV1>(serde_json::json!({
                "sha": "a".repeat(40), "binaries_dir": "/x", "idempotency_key": "k", "env": {}
            }))
            .is_err()
        );
    }

    /// #1461: `interrupt_workers` is optional and off by default, travels on the
    /// wire only when set, and (#2587) is available with a satellite deploy.
    #[test]
    fn interrupt_workers_is_optional_off_by_default_and_allowed_for_a_peer() {
        let plain = request();
        assert!(!plain.interrupts_workers());
        assert!(
            !serde_json::to_value(&plain)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("interrupt_workers")
        );
        let parsed: AgentRequestDeployRequestV1 = serde_json::from_value(serde_json::json!({
            "sha": "a".repeat(40), "binaries_dir": "/x", "idempotency_key": "k",
            "interrupt_workers": true
        }))
        .unwrap();
        assert!(parsed.interrupts_workers());
        assert_eq!(parsed.validate(), Ok(()));
        let explicit_false: AgentRequestDeployRequestV1 =
            serde_json::from_value(serde_json::json!({
                "sha": "a".repeat(40), "binaries_dir": "/x", "idempotency_key": "k",
                "interrupt_workers": false
            }))
            .unwrap();
        assert!(!explicit_false.interrupts_workers());
        let mut with_peer = parsed;
        with_peer.peer_id = Some(crate::satellite::SatelliteUuidV1(Uuid::new_v4()));
        // #2587: a satellite deploy carries the same option.
        assert_eq!(with_peer.validate(), Ok(()));
        assert!(with_peer.interrupts_workers());
    }

    #[test]
    fn interrupt_workers_is_refused_with_cancel() {
        let mut cancel = request();
        cancel.cancel = Some(true);
        assert_eq!(cancel.validate(), Ok(()));
        cancel.interrupt_workers = Some(false);
        assert_eq!(cancel.validate(), Ok(()));
        cancel.interrupt_workers = Some(true);
        assert_eq!(cancel.validate(), Err(DEPLOY_INVALID_REQUEST));
    }

    /// #1461: the receipt names the request and, once known, the interrupted
    /// workers; a receipt that has neither keeps its old shape.
    #[test]
    fn the_receipt_names_interrupted_workers_only_when_there_are_some() {
        let worker = Uuid::new_v4();
        let mut receipt = AgentRequestDeployReceiptV1 {
            deploy_id: Uuid::new_v4(),
            state: DeployState::Staged,
            sha: "a".repeat(40),
            deadline_at: "2026-10-07T00:00:00.000000000Z".into(),
            binaries: Vec::new(),
            skipped: Vec::new(),
            replayed: false,
            reason: None,
            interrupt_workers: false,
            interrupted_workers: Vec::new(),
            job_grace_secs: None,
            cancelled_jobs: Vec::new(),
            blockers: Vec::new(),
            waiting_since: None,
            qa_warnings: Vec::new(),
        };
        let value = serde_json::to_value(&receipt).unwrap();
        assert!(value.get("interrupt_workers").is_none());
        assert!(value.get("interrupted_workers").is_none());
        receipt.interrupt_workers = true;
        receipt.interrupted_workers = vec![worker];
        let job = Uuid::new_v4();
        receipt.job_grace_secs = Some(DEPLOY_JOB_GRACE_SECS);
        receipt.cancelled_jobs = vec![job];
        let value = serde_json::to_value(&receipt).unwrap();
        assert_eq!(value["job_grace_secs"], DEPLOY_JOB_GRACE_SECS);
        assert_eq!(value["cancelled_jobs"], serde_json::json!([job]));
        assert_eq!(value["interrupt_workers"], true);
        assert_eq!(value["interrupted_workers"], serde_json::json!([worker]));
        assert_eq!(
            serde_json::from_value::<AgentRequestDeployReceiptV1>(value).unwrap(),
            receipt
        );
    }

    #[test]
    fn state_strings_round_trip() {
        for state in [
            DeployState::Staged,
            DeployState::Restarting,
            DeployState::Succeeded,
            DeployState::Failed,
            DeployState::TimedOut,
        ] {
            assert_eq!(DeployState::parse(state.as_str()), Some(state));
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
    }
}
