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
pub const DEPLOY_BINARIES: [&str; 11] = [
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
];

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
/// Restarts a deploy may use in the supervisor's one-hour window; the
/// supervisor allows three, and one stays free for a rollback or watchdog.
pub const DEPLOY_MAX_RESTARTS_PER_HOUR: i64 = 2;

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
/// Upper bound of `deploy_drain_hold_secs` (a deploy waits at most this long).
pub const DEPLOY_DRAIN_HOLD_MAX_SECS: u64 = DEPLOY_MAX_WAIT_SECS as u64;
/// Satellite deploy: the satellite has no local session to own the deploy
/// (its operator has declared no inbound scope root that exists).
pub const SATELLITE_DEPLOY_OWNER_REQUIRED: &str = "satellite_deploy_owner_required";
/// Satellite deploy: no enabled link reached the paired satellite.
pub const SATELLITE_DEPLOY_UNREACHABLE: &str = "satellite_deploy_unreachable";

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
    /// restart began. Not available with `peer_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    /// #1461: once the operator's drain hold (`deploy_drain_hold_secs`) is over,
    /// a worker still mid-turn no longer blocks the quiet point: the restart
    /// interrupts it and the existing post-restart path resumes the turn with
    /// a continue prompt. A landing and a local test/build/landing job still
    /// block. Default false (workers block, as always).
    /// The outcome lists the interrupted worker session ids and each one is an
    /// andon friction event. Not available with `peer_id` or `cancel`.
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
            return if self.peer_id.is_some() || self.interrupts_workers() {
                Err(DEPLOY_INVALID_REQUEST)
            } else {
                Ok(())
            };
        }
        if self.interrupts_workers() && self.peer_id.is_some() {
            // The satellite runs its own deploy flow, which has no such option.
            return Err(DEPLOY_INVALID_REQUEST);
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
        assert_eq!(cancel.validate(), Err(DEPLOY_INVALID_REQUEST));
        assert!(
            serde_json::from_value::<AgentRequestDeployRequestV1>(serde_json::json!({
                "sha": "a".repeat(40), "binaries_dir": "/x", "idempotency_key": "k", "env": {}
            }))
            .is_err()
        );
    }

    /// #1461: `interrupt_workers` is optional and off by default, travels on the
    /// wire only when set, and is not available with a satellite deploy.
    #[test]
    fn interrupt_workers_is_optional_off_by_default_and_not_for_a_peer() {
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
        assert_eq!(with_peer.validate(), Err(DEPLOY_INVALID_REQUEST));
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
        };
        let value = serde_json::to_value(&receipt).unwrap();
        assert!(value.get("interrupt_workers").is_none());
        assert!(value.get("interrupted_workers").is_none());
        receipt.interrupt_workers = true;
        receipt.interrupted_workers = vec![worker];
        let value = serde_json::to_value(&receipt).unwrap();
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
