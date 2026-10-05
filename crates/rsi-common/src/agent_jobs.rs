//! Wire types for daemon-owned durable jobs (#1002 slice 1).
//!
//! `AgentSubmitJob` hands a long operation (test, build, landing, cloud gate,
//! cloud sweep)
//! to the daemon, which runs it in its own `systemd-run --user` unit outside
//! every session scope and wakes the owner once with the typed result.
//! `AgentGetJob` reads one job back. Parameters are typed: an agent never
//! supplies a command line, only the fields below.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const JOB_MAX_NAME_BYTES: usize = 80;
pub const JOB_MAX_IDEMPOTENCY_BYTES: usize = 128;
pub const JOB_MAX_TEST_FILTERS: usize = 32;
pub const JOB_MAX_TOKEN_BYTES: usize = 200;

/// Stable refusal codes; each is the whole `InvalidParam`/`PolicyDenied` text.
pub const JOB_INVALID_REQUEST: &str = "job_invalid_request";
pub const JOB_INVALID_PARAMS: &str = "job_invalid_params";
pub const JOB_NAME_INVALID: &str = "job_name_invalid";
pub const JOB_KEY_INVALID: &str = "job_idempotency_key_invalid";
pub const JOB_KIND_UNSUPPORTED: &str = "job_kind_unsupported";
pub const JOB_DIR_NOT_ALLOWED: &str = "job_directory_not_allowed";
pub const JOB_NOT_FOUND: &str = "job_not_found";
/// `landing`, `cloud_gate` and `cloud_sweep` publish to `rolling` / spend the
/// operator's cloud grant: only the current appointed manager or current Epic lead may submit.
pub const JOB_KIND_NOT_AUTHORIZED: &str = "job_kind_not_authorized";
pub const JOB_LAUNCH_FAILED: &str = "job_launch_failed";
pub const JOB_KEY_CONFLICT: &str = "job_idempotency_key_conflict";
/// `AgentCancelJob` (#1106): the refusal recorded on a job its owner stopped.
/// The job settles `failed` with this refusal, the same typed route the
/// scratch-quota stop uses, so no new state or migration is needed.
pub const JOB_CANCELLED: &str = "job_cancelled";

/// Typed job kind. Strings match the SQLite CHECK exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Test,
    Build,
    Landing,
    CloudGate,
    CloudSweep,
}

impl JobKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Build => "build",
            Self::Landing => "landing",
            Self::CloudGate => "cloud_gate",
            Self::CloudSweep => "cloud_sweep",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "test" => Self::Test,
            "build" => Self::Build,
            "landing" => Self::Landing,
            "cloud_gate" => Self::CloudGate,
            "cloud_sweep" => Self::CloudSweep,
            _ => return None,
        })
    }
}

/// Job lifecycle. `Lost` means the unit ended without recording an exit
/// status (stopped, timed out or killed by the host).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
    Lost,
}

impl JobState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Lost => "lost",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "lost" => Self::Lost,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Who is woken when a job settles (#1006). `owner` (the default) delivers one
/// resume wake to the owner per job; `none` settles the job silently: its
/// result stays readable through `AgentGetJob`/`AgentListJobs`, and a caller
/// that wants one wake for a batch arms `AgentScheduleWake` mode `when` with a
/// `jobs_terminal` predicate instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum JobWake {
    #[default]
    Owner,
    None,
}

impl JobWake {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::None => "none",
        }
    }
}

/// `AgentSubmitJob` request. `params` is decoded per `kind` by
/// [`AgentSubmitJobRequestV1::typed_params`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSubmitJobRequestV1 {
    pub kind: JobKind,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub name: Option<String>,
    /// Optional replay key: an identical retry returns the original job.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Appointed managers only: run in this other worktree of the caller's own
    /// repository (an integration worktree) instead of the caller's sandbox.
    #[serde(default)]
    pub worktree: Option<String>,
    /// `owner` (default) or `none`: see [`JobWake`].
    #[serde(default)]
    pub wake: Option<JobWake>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGetJobRequestV1 {
    pub job_id: Uuid,
}

/// `AgentCancelJob` (#1106): stop one of your running jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCancelJobRequestV1 {
    pub job_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListJobsRequestV1 {
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `cargo test` (or one nextest shard) through the cargo build-slot wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestJobParams {
    /// Run `scripts/run-rsid-test-shards.sh shard <name>`.
    #[serde(default)]
    pub shard: Option<String>,
    /// Shard-only `test(NAME)` filterset.
    #[serde(default)]
    pub filterset: Option<String>,
    /// Run `cargo test -p <package>`.
    #[serde(default)]
    pub package: Option<String>,
    /// Test-name filters after `--` (package runs only). Empty runs every test
    /// of the package (#1106).
    #[serde(default)]
    pub filters: Vec<String>,
    /// Package runs: pass `--lib`.
    #[serde(default)]
    pub lib_only: bool,
    /// #1099 candidate receipt (manager/Epic lead only): the branch or full
    /// commit to verify against `origin/rolling` in a temporary detached
    /// worktree. The typed receipt is `result.receipt`.
    #[serde(default)]
    pub candidate_receipt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildCommand {
    Check,
    Build,
}

impl BuildCommand {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Build => "build",
        }
    }
}

/// `cargo check|build` through the cargo build-slot wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildJobParams {
    pub command: BuildCommand,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub workspace: bool,
    #[serde(default)]
    pub all_targets: bool,
    #[serde(default)]
    pub release: bool,
}

/// `rsi-rolling-land` (kind `landing`) or `scripts/cloud-gate.sh` (kind
/// `cloud_gate`) for one accepted source commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandingJobParams {
    pub accepted: String,
    #[serde(default)]
    pub test_filters: Vec<String>,
}

/// `scripts/cloud-sweep.sh cloud <sha>` (kind `cloud_sweep`): the full QA sweep
/// of one rolling-tip commit on an ephemeral cloud host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudSweepJobParams {
    pub sha: String,
}

/// Validated, typed parameters for one job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobParams {
    Test(TestJobParams),
    Build(BuildJobParams),
    Landing(LandingJobParams),
    CloudGate(LandingJobParams),
    CloudSweep(CloudSweepJobParams),
}

impl JobParams {
    #[must_use]
    pub const fn kind(&self) -> JobKind {
        match self {
            Self::Test(_) => JobKind::Test,
            Self::Build(_) => JobKind::Build,
            Self::Landing(_) => JobKind::Landing,
            Self::CloudGate(_) => JobKind::CloudGate,
            Self::CloudSweep(_) => JobKind::CloudSweep,
        }
    }
}

fn bare_token(value: &str, extra: &[char]) -> bool {
    !value.is_empty()
        && value.len() <= JOB_MAX_TOKEN_BYTES
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || extra.contains(&c))
}

fn full_lower_hex_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `PACKAGE=FILTER`, the lander's `--test-filter` shape.
fn lander_filter(value: &str) -> bool {
    value.split_once('=').is_some_and(|(package, filter)| {
        bare_token(package, &['.'])
            && !filter.is_empty()
            && filter.len() <= JOB_MAX_TOKEN_BYTES
            && !filter.starts_with('-')
            && filter
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'))
    })
}

/// The shard script's own `test(NAME)` pattern.
fn test_filterset(value: &str) -> bool {
    value
        .strip_prefix("test(")
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|name| {
            !name.is_empty()
                && name.len() <= JOB_MAX_TOKEN_BYTES
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'))
        })
}

/// A branch name, `origin/<branch>` or full commit id for `candidate_receipt`:
/// never an option, never a revision expression.
fn candidate_ref(value: &str) -> bool {
    bare_token(value, &['/', '.'])
        && !value.contains("..")
        && !value.contains("//")
        && !value.ends_with(['/', '.'])
}

impl TestJobParams {
    /// A `candidate_receipt` run: only the appointed manager or an Epic lead
    /// may submit it (it fetches and builds an arbitrary branch).
    #[must_use]
    pub const fn is_candidate_receipt(&self) -> bool {
        self.candidate_receipt.is_some()
    }

    fn validate(&self) -> Result<(), &'static str> {
        if let Some(candidate) = &self.candidate_receipt {
            let ok = self.shard.is_none()
                && self.package.is_none()
                && self.filterset.is_none()
                && self.filters.is_empty()
                && !self.lib_only
                && candidate_ref(candidate);
            return ok.then_some(()).ok_or(JOB_INVALID_PARAMS);
        }
        let ok = match (&self.shard, &self.package) {
            (Some(shard), None) => {
                bare_token(shard, &[])
                    && self.filters.is_empty()
                    && !self.lib_only
                    && self.filterset.as_deref().is_none_or(test_filterset)
            }
            (None, Some(package)) => {
                bare_token(package, &['.'])
                    && self.filterset.is_none()
                    && self.filters.len() <= JOB_MAX_TEST_FILTERS
                    && self.filters.iter().all(|f| bare_token(f, &[':', '.']))
            }
            _ => false,
        };
        ok.then_some(()).ok_or(JOB_INVALID_PARAMS)
    }
}

impl BuildJobParams {
    fn validate(&self) -> Result<(), &'static str> {
        let scope = match (&self.package, self.workspace) {
            (Some(package), false) => bare_token(package, &['.']),
            (None, true) => true,
            _ => false,
        };
        scope.then_some(()).ok_or(JOB_INVALID_PARAMS)
    }
}

impl LandingJobParams {
    fn validate(&self) -> Result<(), &'static str> {
        let ok = full_lower_hex_oid(&self.accepted)
            && self.test_filters.len() <= JOB_MAX_TEST_FILTERS
            && self.test_filters.iter().all(|f| lander_filter(f));
        ok.then_some(()).ok_or(JOB_INVALID_PARAMS)
    }
}

impl CloudSweepJobParams {
    fn validate(&self) -> Result<(), &'static str> {
        full_lower_hex_oid(&self.sha)
            .then_some(())
            .ok_or(JOB_INVALID_PARAMS)
    }
}

impl AgentSubmitJobRequestV1 {
    /// Validate the name, replay key and per-kind parameters.
    ///
    /// # Errors
    /// The stable `job_*` code for the first invalid field.
    pub fn typed_params(&self) -> Result<JobParams, &'static str> {
        if let Some(name) = &self.name
            && (name.is_empty()
                || name.len() > JOB_MAX_NAME_BYTES
                || name.chars().any(char::is_control))
        {
            return Err(JOB_NAME_INVALID);
        }
        if let Some(key) = &self.idempotency_key
            && (key.is_empty() || key.len() > JOB_MAX_IDEMPOTENCY_BYTES || key.contains('\0'))
        {
            return Err(JOB_KEY_INVALID);
        }
        let params = if self.params.is_null() {
            serde_json::json!({})
        } else {
            self.params.clone()
        };
        if !params.is_object() {
            return Err(JOB_INVALID_PARAMS);
        }
        let decode_err = |_| JOB_INVALID_PARAMS;
        Ok(match self.kind {
            JobKind::Test => {
                let p: TestJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::Test(p)
            }
            JobKind::Build => {
                let p: BuildJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::Build(p)
            }
            JobKind::Landing => {
                let p: LandingJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::Landing(p)
            }
            JobKind::CloudGate => {
                let p: LandingJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::CloudGate(p)
            }
            JobKind::CloudSweep => {
                let p: CloudSweepJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::CloudSweep(p)
            }
        })
    }
}

/// Most failing test names a sweep result lists per class.
pub const SWEEP_MAX_FAILURES: usize = 32;

/// The sweep's overall outcome. A missing or malformed verdict is
/// `Incomplete`, never `Green`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SweepVerdict {
    Green,
    Red,
    Incomplete,
}

impl SweepVerdict {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "GREEN",
            Self::Red => "RED",
            Self::Incomplete => "INCOMPLETE",
        }
    }
}

/// Typed `cloud_sweep` outcome, parsed from the sweep's final
/// `VERDICT <state> <sha>` line and its `QA.md` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudSweepResultV1 {
    pub verdict: SweepVerdict,
    pub sha: String,
    /// Where the collected report and failure logs are (`~/.rsi/cloud/results/<sha>`).
    pub results_dir: String,
    /// Failing tests the classifier found NEW (at most [`SWEEP_MAX_FAILURES`]).
    #[serde(default)]
    pub new_failures: Vec<String>,
    /// Failing tests matching a known-failure Issue (`#N name`, bounded).
    #[serde(default)]
    pub known_failures: Vec<String>,
}

/// Typed result delivered once to the owner. Absent fields did not apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentJobResultV1 {
    pub exit_code: Option<i32>,
    /// Published tip (`landing` and `cloud_gate` jobs that landed).
    pub landed_sha: Option<String>,
    /// Stable refusal code for a `landing`/`cloud_gate` job that did not land.
    pub refusal: Option<String>,
    #[serde(default)]
    pub failing_tests: Vec<String>,
    /// Bounded tail of the job log.
    pub detail: Option<String>,
    /// `cloud_sweep` jobs: the typed sweep verdict.
    #[serde(default)]
    pub sweep: Option<CloudSweepResultV1>,
    /// `test` jobs with `candidate_receipt` (#1099): the typed candidate
    /// receipt `scripts/check-touched-shards` produced (base, head, merge
    /// clean, shards compiled, audit verdicts, migrations, suggested filters).
    #[serde(default)]
    pub receipt: Option<serde_json::Value>,
}

/// One job as reported to its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentJobV1 {
    pub id: Uuid,
    pub kind: JobKind,
    pub name: Option<String>,
    pub state: JobState,
    pub owner_session_id: Uuid,
    pub unit_name: String,
    pub cwd: String,
    pub log_path: String,
    pub params: JobParams,
    pub exit_code: Option<i32>,
    pub result: Option<AgentJobResultV1>,
    pub created_at: String,
    pub finished_at: Option<String>,
    /// Per-job completion wake policy chosen at submit.
    #[serde(default)]
    pub wake: JobWake,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSubmitJobReceiptV1 {
    pub job: AgentJobV1,
    /// True when an identical idempotent retry returned the original job.
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCancelJobResultV1 {
    /// The job after the call: `failed` with refusal `job_cancelled` when this
    /// call stopped it, or its existing terminal state otherwise.
    pub job: AgentJobV1,
    /// True when this call stopped a running job; false when it had already
    /// settled (a repeat cancel changes nothing).
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentListJobsResultV1 {
    pub jobs: Vec<AgentJobV1>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(kind: JobKind, params: serde_json::Value) -> AgentSubmitJobRequestV1 {
        AgentSubmitJobRequestV1 {
            kind,
            params,
            name: None,
            idempotency_key: None,
            worktree: None,
            wake: None,
        }
    }

    #[test]
    fn typed_params_accept_the_five_kinds() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert!(matches!(
            request(
                JobKind::Test,
                json!({"package":"rsid","filters":["rolling_queue::tests"],"lib_only":true})
            )
            .typed_params(),
            Ok(JobParams::Test(_))
        ));
        // #1106: a package run needs no filters; it runs every test.
        for all in [
            json!({"package":"rsid"}),
            json!({"package":"rsid","filters":[]}),
            json!({"package":"rsid","lib_only":true}),
        ] {
            assert!(
                matches!(
                    request(JobKind::Test, all.clone()).typed_params(),
                    Ok(JobParams::Test(_))
                ),
                "{all}"
            );
        }
        assert!(matches!(
            request(
                JobKind::Test,
                json!({"shard":"other-01","filterset":"test(foo_bar)"})
            )
            .typed_params(),
            Ok(JobParams::Test(_))
        ));
        assert!(matches!(
            request(
                JobKind::Build,
                json!({"command":"check","workspace":true,"all_targets":true})
            )
            .typed_params(),
            Ok(JobParams::Build(_))
        ));
        assert!(matches!(
            request(
                JobKind::Landing,
                json!({"accepted":oid,"test_filters":["rsid=rolling_queue"]})
            )
            .typed_params(),
            Ok(JobParams::Landing(_))
        ));
        assert!(matches!(
            request(JobKind::CloudGate, json!({"accepted":oid})).typed_params(),
            Ok(JobParams::CloudGate(_))
        ));
        assert!(matches!(
            request(JobKind::CloudSweep, json!({"sha":oid})).typed_params(),
            Ok(JobParams::CloudSweep(_))
        ));
    }

    #[test]
    fn typed_params_refuse_anything_that_could_smuggle_a_command() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let bad = [
            request(
                JobKind::Test,
                json!({"package":"rsid; rm -rf /","filters":["a"]}),
            ),
            request(
                JobKind::Test,
                json!({"package":"rsid","filters":["--nocapture"]}),
            ),
            request(JobKind::Test, json!({"package":"rsid","filters":["a b"]})),
            request(
                JobKind::Test,
                json!({"shard":"x","package":"rsid","filters":["a"]}),
            ),
            request(
                JobKind::Test,
                json!({"shard":"x","filterset":"test(a) | test(b)"}),
            ),
            request(JobKind::Test, json!({"command":"rm -rf /"})),
            request(JobKind::Build, json!({"command":"check"})),
            request(
                JobKind::Build,
                json!({"command":"check","package":"a","workspace":true}),
            ),
            request(
                JobKind::Build,
                json!({"command":"install","workspace":true}),
            ),
            request(JobKind::Landing, json!({"accepted":"HEAD"})),
            request(
                JobKind::Landing,
                json!({"accepted":oid,"test_filters":["--repo=x"]}),
            ),
            request(JobKind::CloudGate, json!({"accepted":oid,"argv":["sh"]})),
            request(JobKind::Landing, json!("sh -c id")),
            request(JobKind::CloudSweep, json!({"sha":"HEAD"})),
            request(JobKind::CloudSweep, json!({"sha":oid.to_uppercase()})),
            request(JobKind::CloudSweep, json!({"sha":oid,"argv":["sh"]})),
            request(JobKind::CloudSweep, json!({"accepted":oid})),
            request(JobKind::CloudSweep, json!({})),
        ];
        for request in bad {
            assert_eq!(request.typed_params().unwrap_err(), JOB_INVALID_PARAMS);
        }
    }

    #[test]
    fn name_and_key_are_bounded() {
        let mut ok = request(JobKind::Build, json!({"command":"check","workspace":true}));
        ok.name = Some("x".repeat(JOB_MAX_NAME_BYTES + 1));
        assert_eq!(ok.typed_params().unwrap_err(), JOB_NAME_INVALID);
        ok.name = Some("check".into());
        ok.idempotency_key = Some(String::new());
        assert_eq!(ok.typed_params().unwrap_err(), JOB_KEY_INVALID);
    }

    #[test]
    fn a_candidate_receipt_takes_only_a_bare_ref() {
        let ok = |value: serde_json::Value| request(JobKind::Test, value).typed_params().is_ok();
        for good in [
            "rsi/8e8b947d-4dcd",
            "origin/rolling",
            "a".repeat(40).as_str(),
        ] {
            assert!(ok(json!({"candidate_receipt": good})), "{good}");
        }
        for bad in [
            "",
            "--upload-pack=x",
            "a..b",
            "a//b",
            "ref/",
            "sh -c id",
            "a;b",
            "HEAD~1",
        ] {
            assert!(!ok(json!({"candidate_receipt": bad})), "{bad}");
        }
        // It takes no other test selector.
        assert!(!ok(json!({"candidate_receipt":"x","package":"rsid"})));
        assert!(!ok(json!({"candidate_receipt":"x","shard":"other-01"})));
        assert!(!ok(json!({"candidate_receipt":"x","lib_only":true})));
    }
}
