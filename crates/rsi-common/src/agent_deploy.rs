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
pub const DEPLOY_BINARIES: [&str; 18] = [
    "rsid",
    "rsi",
    "rsi-rpc",
    "rsi-agent-mcp",
    "rsi-build-rustc",
    "rsi-contract-validate",
    "rsi-known-failure",
    "rsi-rolling-land",
    "rsi-repo-guard",
    "rsi-remote",
    "rsi-turn-shim",
    "rsi-socket-hold",
    "rsi-cloud",
    "rsi-eval",
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
pub const DEPLOY_BINARIES_DIR_NOT_ABSOLUTE: &str = "deploy_binaries_dir_not_absolute";
pub const DEPLOY_DIR_NOT_ALLOWED: &str = "deploy_directory_not_allowed";
pub const DEPLOY_BINARY_MISSING: &str = "deploy_binary_missing";
pub const DEPLOY_STAGE_FAILED: &str = "deploy_stage_failed";
/// The build a deploy would put live runs from a directory with no trusted
/// `rsi-repo-guard` beside its `rsid` or `rsi-rolling-land`, so the daemon's
/// provisional probe and the lander would refuse every landing (hub #3886). The
/// message names the consumer and the directory.
pub const DEPLOY_GUARD_UNRESOLVABLE: &str = "deploy_guard_unresolvable";
/// A plain member the build installed is not the file beside its `rsid`, where the
/// daemon and the lander resolve it (hub #3886); the message names the member.
pub const DEPLOY_MEMBER_UNRESOLVABLE: &str = "deploy_member_unresolvable";
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
    /// a continue prompt. Ordinary test jobs are never stopped or rerun (#3554): they
    /// keep running across the restart and the new daemon re-adopts them; landing,
    /// build and protected jobs still block. Omission defaults
    /// to true with `peer_id`, false for a local deploy. Explicit false waits
    /// for a lull on either target.
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
        !self.is_cancel() && self.interrupt_workers.unwrap_or(self.peer_id.is_some())
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
            return if self.interrupt_workers == Some(true) {
                Err(DEPLOY_INVALID_REQUEST)
            } else {
                Ok(())
            };
        }
        match (self.binaries_dir.as_deref(), self.build) {
            (Some(_), Some(true)) => Err(DEPLOY_SOURCE_AMBIGUOUS),
            (None, Some(true)) => Err(DEPLOY_BUILD_NOT_SUPPORTED),
            (Some(dir), _) if !std::path::Path::new(dir).is_absolute() => {
                Err(if self.peer_id.is_some() {
                    DEPLOY_BINARIES_DIR_NOT_ABSOLUTE
                } else {
                    DEPLOY_DIR_NOT_ALLOWED
                })
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
    /// Live prediction while staged; workers still mid-turn may finish first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_interrupted_workers: Vec<Uuid>,
    /// Legacy (#2278): ordinary test jobs are no longer stopped and requeued by
    /// a deploy (#3554), so a current daemon leaves this empty; an older daemon
    /// listed the running ordinary test jobs expected to requeue after the grace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_requeued_jobs: Vec<Uuid>,
    /// Legacy: jobs a deploy of an older daemon requeued under their own ids.
    /// Empty from a current daemon (#3554).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requeued_jobs: Vec<Uuid>,
    /// #3554: live prediction while staged. The running ordinary test jobs
    /// (`scoped_test` included) that keep running across the restart: the new
    /// daemon re-adopts each unit and settles it once, with one owner wake. They
    /// do not block the quiet point. Empty without `interrupt_workers`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_surviving_jobs: Vec<Uuid>,
    /// #3554: the jobs that were still running when this deploy restarted the
    /// daemon, and so kept running. A job that finished before the restart is
    /// not listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub surviving_jobs: Vec<Uuid>,
    /// Legacy: the grace an older daemon gave ordinary test jobs before it
    /// requeued them. Absent from a current daemon, which stops none (#3554).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_grace_secs: Option<u64>,
    /// #2048: the jobs this deploy stopped. A hub QA run settles
    /// `interrupted_by_deploy` and wakes its owner at once. An ordinary test job
    /// is not stopped (#3554); an older daemon requeued it here (#2278).
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
        relative.peer_id = Some(crate::satellite::SatelliteUuidV1(Uuid::new_v4()));
        for dir in ["bin", "~/.rsi/staging/bin", ""] {
            relative.binaries_dir = Some(dir.into());
            assert_eq!(relative.validate(), Err(DEPLOY_BINARIES_DIR_NOT_ABSOLUTE));
        }
        relative.cancel = Some(true);
        assert_eq!(relative.validate(), Ok(()));
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

    /// Omitted peer requests interrupt; local requests and explicit false wait.
    /// Cancellation must not acquire the peer default.
    #[test]
    fn interrupt_workers_defaults_to_peer_only_and_explicit_false_waits() {
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
        with_peer.interrupt_workers = None;
        assert_eq!(with_peer.validate(), Ok(()));
        assert!(with_peer.interrupts_workers());
        with_peer.interrupt_workers = Some(false);
        assert!(!with_peer.interrupts_workers());
        with_peer.interrupt_workers = None;
        with_peer.cancel = Some(true);
        assert_eq!(with_peer.validate(), Ok(()));
        assert!(!with_peer.interrupts_workers());
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
            expected_interrupted_workers: Vec::new(),
            expected_requeued_jobs: Vec::new(),
            requeued_jobs: Vec::new(),
            expected_surviving_jobs: Vec::new(),
            surviving_jobs: Vec::new(),
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
        receipt.cancelled_jobs = vec![job];
        receipt.surviving_jobs = vec![job];
        let value = serde_json::to_value(&receipt).unwrap();
        assert!(value.get("job_grace_secs").is_none());
        assert_eq!(value["surviving_jobs"], serde_json::json!([job]));
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

    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(crate::manifest_dir!()).join("../..")
    }

    fn repo_file(relative: &str) -> String {
        let path = repo_root().join(relative);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// #451: `rsi-repo-guard` ships beside `rsid` and `rsi-rolling-land`. The
    /// lander and the queue probe find it only as the sibling of the running
    /// binary and fail closed without it (`repo_guard_call::trusted_tool`).
    #[test]
    fn repo_guard_is_a_deployed_binary_next_to_the_lander() {
        assert!(DEPLOY_BINARIES.contains(&"rsi-repo-guard"));
        assert!(!is_deploy_tool_script("rsi-repo-guard"));
        let position = |name: &str| DEPLOY_BINARIES.iter().position(|n| *n == name).unwrap();
        assert!(position("rsi-repo-guard") > position("rsi-rolling-land"));
        let absent = skipped_binaries(&[]);
        assert!(absent.contains(&"rsi-repo-guard".to_string()));
    }

    /// The shell installer's list (`scripts/rsid-quiet-restart.sh`) is a second
    /// copy of the managed set; it must name exactly the binaries, in order,
    /// that `DEPLOY_BINARIES` names, minus the tool scripts the installer copies
    /// by hand (`cargo-slot`, `target-pool`, `rsi-spill`).
    #[test]
    fn the_shell_installer_list_matches_the_managed_set() {
        let script = repo_file("scripts/rsid-quiet-restart.sh");
        let line = script
            .lines()
            .find(|line| line.starts_with("RSID_DEPLOY_BINARIES=("))
            .expect("RSID_DEPLOY_BINARIES array");
        let listed: Vec<&str> = line
            .trim_start_matches("RSID_DEPLOY_BINARIES=(")
            .trim_end_matches(')')
            .split_whitespace()
            .collect();
        let expected: Vec<&str> = DEPLOY_BINARIES
            .iter()
            .copied()
            .filter(|name| !is_deploy_tool_script(name))
            .collect();
        assert_eq!(listed, expected);
    }

    /// #451 constraint: `crates/rsi-repo-guards` enables `serde_json/preserve_order`
    /// and Cargo unifies features across one build, so co-building it with `rsid`
    /// reorders every `serde_json::Map` in the daemon (starter digests moved).
    /// The installer builds it in its own cargo invocation.
    #[test]
    fn the_installer_builds_the_repo_guard_in_its_own_cargo_invocation() {
        let script = repo_file("scripts/install-release.sh");
        let start = script
            .find("build_release_binaries() {")
            .expect("build_release_binaries");
        let body = &script[start..];
        let body = &body[..body.find("\n}\n").expect("end of build_release_binaries")];
        let builds: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("cargo build"))
            .collect();
        assert_eq!(
            builds.len(),
            2,
            "one daemon build, one guard build: {builds:?}"
        );
        let (daemon, guard): (Vec<&str>, Vec<&str>) = builds
            .iter()
            .copied()
            .partition(|line| line.contains("--bin rsid "));
        assert_eq!((daemon.len(), guard.len()), (1, 1), "{builds:?}");
        assert!(daemon[0].contains("--bin rsi-rolling-land"));
        assert!(!daemon[0].contains("repo-guard"), "{}", daemon[0]);
        assert!(guard[0].contains("-p rsi-repo-guards --bin rsi-repo-guard"));
        assert!(!guard[0].contains("--bin rsid"), "{}", guard[0]);
        assert!(!guard[0].contains("--workspace"), "{}", guard[0]);
    }

    // Cargo.lock does not distinguish production and dev edges. Validate the
    // manifests first, including aliases and target-specific dependency tables,
    // then allow only those verified fixture consumers in the resolved graph.
    fn repo_guard_fixture_aliases(manifest: &toml::Value) -> Result<Vec<String>, String> {
        fn visit(value: &toml::Value, aliases: &mut Vec<String>) -> Result<(), String> {
            let Some(table) = value.as_table() else {
                return Ok(());
            };
            for (section, value) in table {
                if matches!(
                    section.as_str(),
                    "dependencies" | "dev-dependencies" | "build-dependencies"
                ) {
                    for (alias, dependency) in value.as_table().ok_or("dependency table")? {
                        let package = dependency
                            .get("package")
                            .and_then(toml::Value::as_str)
                            .unwrap_or(alias);
                        if package != "rsi-repo-guards" {
                            continue;
                        }
                        if section != "dev-dependencies"
                            || dependency
                                .get("default-features")
                                .and_then(toml::Value::as_bool)
                                != Some(false)
                            || dependency.get("workspace").and_then(toml::Value::as_bool)
                                == Some(true)
                            || dependency
                                .get("features")
                                .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
                        {
                            return Err(format!(
                                "{section}.{alias}: rsi-repo-guards must be fixture-only, \
                                 with default-features = false and no requested features"
                            ));
                        }
                        aliases.push(alias.clone());
                    }
                } else {
                    visit(value, aliases)?;
                }
            }
            Ok(())
        }
        let mut aliases = Vec::new();
        visit(manifest, &mut aliases)?;
        if let Some(features) = manifest.get("features").and_then(toml::Value::as_table) {
            for feature in features.values() {
                for enabled in feature.as_array().ok_or("feature array")? {
                    if let Some((dependency, _)) =
                        enabled.as_str().ok_or("feature string")?.split_once('/')
                        && aliases
                            .iter()
                            .any(|alias| alias == dependency.trim_end_matches('?'))
                    {
                        return Err(format!("fixture dependency feature forwarded: {enabled}"));
                    }
                }
            }
        }
        Ok(aliases)
    }

    fn repo_guard_lock_is_isolated(
        lock: &toml::Value,
        fixtures: &std::collections::BTreeSet<String>,
    ) -> Result<(), String> {
        for package in lock["package"].as_array().ok_or("lock packages")? {
            let name = package["name"].as_str().ok_or("lock package name")?;
            for dependency in package
                .get("dependencies")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
            {
                if dependency
                    .as_str()
                    .ok_or("lock dependency")?
                    .split_whitespace()
                    .next()
                    == Some("rsi-repo-guards")
                    && !fixtures.contains(name)
                {
                    return Err(format!(
                        "{name} has an unverified rsi-repo-guards lock edge"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Preserve the hub's inline, table-form and target-specific regression
    /// cases (#3937), using the same feature-isolation check as the live scan.
    #[test]
    fn guard_dependency_is_accepted_only_in_dev_dependency_tables() {
        for dev in [
            "[package]\nname = 'a'\n[dev-dependencies]\nrsi-repo-guards = { path = '../rsi-repo-guards', default-features = false }\n",
            "[target.'cfg(unix)'.dev-dependencies]\nrsi-repo-guards = { version = '0.1', default-features = false }\n",
            "[dev-dependencies.rsi-repo-guards]\npath = '../rsi-repo-guards'\ndefault-features = false\n",
            "[target.'cfg(unix)'.dev-dependencies.rsi-repo-guards]\npath = '../rsi-repo-guards'\ndefault-features = false\n",
        ] {
            assert_eq!(
                repo_guard_fixture_aliases(&toml::from_str(dev).unwrap()),
                Ok(vec!["rsi-repo-guards".into()]),
                "{dev}"
            );
        }
        for shipped in [
            "[dependencies]\nrsi-repo-guards = { path = '../rsi-repo-guards', default-features = false }\n",
            "[build-dependencies]\nrsi-repo-guards = { version = '0.1', default-features = false }\n",
            "[target.'cfg(unix)'.dependencies]\nrsi-repo-guards = { version = '0.1', default-features = false }\n",
            "[dependencies.rsi-repo-guards]\npath = '../rsi-repo-guards'\ndefault-features = false\n",
            "[dev-dependencies.rsi-repo-guards]\npath = '../rsi-repo-guards'\ndefault-features = false\n[features]\nguards = ['rsi-repo-guards/preserve-json-order']\n",
        ] {
            assert!(
                repo_guard_fixture_aliases(&toml::from_str(shipped).unwrap()).is_err(),
                "{shipped}"
            );
        }
        let commented = "[dependencies]\n# rsi-repo-guards stays out of the daemon (#451)\n";
        assert_eq!(
            repo_guard_fixture_aliases(&toml::from_str(commented).unwrap()),
            Ok(Vec::new())
        );
    }

    /// Production edges to the guard remain forbidden (#451). Fixture consumers
    /// may use its feature-isolated library (#413); its default preserve-order
    /// feature must never be unified into the daemon through those fixtures.
    #[test]
    fn no_crate_but_the_guard_crate_depends_on_rsi_repo_guards() {
        let crates = repo_root().join("crates");
        let mut checked = Vec::new();
        let mut fixtures = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&crates).unwrap() {
            let dir = entry.unwrap().path();
            let manifest = dir.join("Cargo.toml");
            if !manifest.is_file() || dir.file_name().unwrap() == "rsi-repo-guards" {
                continue;
            }
            let data: toml::Value =
                toml::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
            let aliases = repo_guard_fixture_aliases(&data)
                .unwrap_or_else(|error| panic!("{}: {error}", manifest.display()));
            if !aliases.is_empty() {
                fixtures.insert(data["package"]["name"].as_str().unwrap().to_owned());
            }
            checked.push(dir.file_name().unwrap().to_string_lossy().into_owned());
        }
        for required in ["rsid", "rsi-common", "rsid-store"] {
            assert!(
                checked.iter().any(|name| name == required),
                "{required} not scanned"
            );
        }
        let root: toml::Value = toml::from_str(&repo_file("Cargo.toml")).unwrap();
        repo_guard_fixture_aliases(&root).unwrap();
        let lock: toml::Value = toml::from_str(&repo_file("Cargo.lock")).unwrap();
        repo_guard_lock_is_isolated(&lock, &fixtures).unwrap();
    }

    #[test]
    fn repo_guard_accepts_only_feature_isolated_fixture_declarations() {
        for section in ["dev-dependencies", "target.'cfg(unix)'.dev-dependencies"] {
            for (alias, package) in [
                ("rsi-repo-guards", ""),
                ("fixtures", "package = 'rsi-repo-guards',"),
            ] {
                let text = format!(
                    "[{section}]\n{alias} = {{ {package} path = '../rsi-repo-guards', \
                     default-features = false, features = [] }}"
                );
                assert_eq!(
                    repo_guard_fixture_aliases(&toml::from_str(&text).unwrap()),
                    Ok(vec![alias.into()])
                );
            }
        }
        let data = toml::from_str(
            "# rsi-repo-guards mentioned in a comment is harmless\n\
             [dev-dependencies.fixtures]\npackage = 'rsi-repo-guards'\n\
             path = '../rsi-repo-guards'\ndefault-features = false\n\
             [features]\ndefault = []",
        )
        .unwrap();
        assert_eq!(
            repo_guard_fixture_aliases(&data),
            Ok(vec!["fixtures".into()])
        );
    }

    #[test]
    fn repo_guard_rejects_production_and_feature_enabling_dependencies() {
        for section in [
            "dependencies",
            "build-dependencies",
            "target.'cfg(windows)'.dependencies",
            "target.'cfg(unix)'.build-dependencies",
            "workspace.dependencies",
        ] {
            let text = format!(
                "[{section}]\nfixtures = {{ package = 'rsi-repo-guards', default-features = false }}"
            );
            assert!(
                repo_guard_fixture_aliases(&toml::from_str(&text).unwrap()).is_err(),
                "{text}"
            );
        }
        for settings in [
            "",
            "default-features = true,",
            "default-features = false, features = ['preserve-json-order'],",
            "default-features = false, features = ['default'],",
            "default-features = false, workspace = true,",
        ] {
            let text = format!(
                "[dev-dependencies]\nrsi-repo-guards = {{ {settings} path = '../rsi-repo-guards' }}"
            );
            assert!(
                repo_guard_fixture_aliases(&toml::from_str(&text).unwrap()).is_err(),
                "{text}"
            );
        }
        for enabled in [
            "fixtures/preserve-json-order",
            "fixtures?/preserve-json-order",
            "fixtures/default",
        ] {
            let text = format!(
                "[dev-dependencies.fixtures]\npackage = 'rsi-repo-guards'\ndefault-features = false\n[features]\nindirect = ['{enabled}']"
            );
            assert!(
                repo_guard_fixture_aliases(&toml::from_str(&text).unwrap()).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn repo_guard_lock_edges_require_a_validated_fixture_consumer() {
        let lock = toml::from_str("[[package]]\nname = 'rsid'\ndependencies = ['rsi-repo-guards 0.1.0']\n[[package]]\nname = 'rsi-common'\ndependencies = ['serde_json']").unwrap();
        let fixtures = std::collections::BTreeSet::from(["rsid".into()]);
        assert_eq!(repo_guard_lock_is_isolated(&lock, &fixtures), Ok(()));
        assert!(repo_guard_lock_is_isolated(&lock, &std::collections::BTreeSet::new()).is_err());
        let transitive =
            toml::from_str("[[package]]\nname = 'transitive'\ndependencies = ['rsi-repo-guards']")
                .unwrap();
        assert!(repo_guard_lock_is_isolated(&transitive, &fixtures).is_err());
    }
}
