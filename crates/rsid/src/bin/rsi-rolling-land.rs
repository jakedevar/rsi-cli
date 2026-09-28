use rsid::integration::{
    Candidate, CandidateKind, CommitIdentity, GuardCommand, GuardSpec, IntegrationConfig,
    IntegrationError, Prepared, Refusal, discard_candidate, prepare_candidate, run_guard,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;

#[path = "rsi-rolling-land/base_cache.rs"]
mod base_cache;
#[path = "rsi-rolling-land/landing_policy.rs"]
mod landing_policy;
#[path = "rsi-rolling-land/remote_gate.rs"]
mod remote_gate;

const PROVISIONAL_SCRIPT: &str = "tools/rolling-migration-renumber.py";

#[derive(Debug, Clone)]
struct ProvisionalLanding {
    base: String,
    source: String,
    unit_path: PathBuf,
    unit_candidate: String,
    proof_path: PathBuf,
    assigned_version: u32,
    proof: Value,
}

#[cfg(not(test))]
const REMOTE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const REMOTE_PUSH_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(not(test))]
const REMOTE_CHILD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
/// Default per-command gate guard.
const DEFAULT_GUARD_TIMEOUT_SECS: u64 = 20 * 60;
/// Upper bound for an operator-raised per-command gate guard.
const MAX_GUARD_TIMEOUT_SECS: u64 = 4 * 60 * 60;

/// Per-command gate guard. A loaded host can raise it with
/// `RSI_LANDER_GUARD_TIMEOUT_SECS` (clamped to 60..=14400 s); an absent or
/// unparsable value keeps the 20-minute default. Waiting on the shared base
/// cache lock counts against this guard, so serialized landers need headroom.
fn guard_timeout() -> Duration {
    guard_timeout_from(
        std::env::var("RSI_LANDER_GUARD_TIMEOUT_SECS")
            .ok()
            .as_deref(),
    )
}

fn guard_timeout_from(value: Option<&str>) -> Duration {
    let secs = value
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map_or(DEFAULT_GUARD_TIMEOUT_SECS, |secs| {
            secs.clamp(60, MAX_GUARD_TIMEOUT_SECS)
        });
    Duration::from_secs(secs)
}

const TARGET: &str = "refs/heads/rolling";
const RSID_SHARDS: &[&str] = &[
    "memory-01",
    "memory-02",
    "other-01",
    "other-02",
    "other-03",
    "other-04",
    "other-05",
    "session-01",
    "session-02",
    "session-03",
    "session-04",
    "session-05",
    "store-01",
    "store-02",
    "store-03",
    "store-04",
];

#[derive(Debug, Clone)]
struct AcceptedPair {
    // Empty only while parsing SOURCE without an explicit historical base.
    base: String,
    source: String,
}

#[derive(Debug, Clone)]
struct Options {
    repo: PathBuf,
    remote: String,
    accepted: Vec<AcceptedPair>,
    test_filters: Vec<String>,
    cargo_build_jobs: u8,
    remote_gate: Option<remote_gate::Config>,
    tmpfs_min_free_gb: u64,
    disk_scratch_shards: BTreeSet<String>,
}

#[derive(Debug)]
struct LandReport {
    candidate: String,
    kind: CandidateKind,
    fetched_tip: String,
    published_tip: String,
    provisional: Vec<ProvisionalLanding>,
    source_bindings: Vec<landing_policy::SourceBinding>,
    base_reds: BTreeSet<String>,
    flakes: BTreeSet<String>,
    base_reused: BTreeSet<String>,
    local_base_confirmed: BTreeSet<String>,
    canary_reused_tree: Option<String>,
    gate_scratch: String,
    gate_disk_scratch: BTreeSet<String>,
    stale_retries: Vec<StaleRetry>,
}

// A disjoint advance reuses the earlier gate, so losing the publish race to
// one costs a fetch, a merge and a push. Several landers publishing within
// seconds of each other is normal, so those retries get a generous budget.
// An overlapping advance re-runs the whole gate and keeps the tight budget.
const MAX_STALE_RETRIES: usize = 8;
const MAX_REGATED_STALE_RETRIES: usize = 2;

/// One lost publish race: the tip this attempt was built on, the newer tip it
/// was remade on, and whether the remade candidate reused the earlier gate.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StaleRetry {
    fetched: String,
    observed: String,
    reused_gate: bool,
}

impl StaleRetry {
    fn label(&self) -> String {
        let gate = if self.reused_gate {
            "gate_reused"
        } else {
            "regated"
        };
        format!("{}..{}:{gate}", self.fetched, self.observed)
    }
}

fn stale_retry_lines(retries: &[StaleRetry]) -> Vec<String> {
    let mut lines = vec![format!("stale_attempts={}", retries.len())];
    for (index, retry) in retries.iter().enumerate() {
        lines.push(format!("stale_retry_{}={}", index + 1, retry.label()));
    }
    lines
}

#[derive(Default)]
struct TestGate {
    // An entry is reusable only for this exact rolling commit and command.
    base_cache: BTreeMap<(String, String), BTreeSet<String>>,
    // Preserve QA origin when a base result is reused again in this process.
    qa_cache_provenance: BTreeMap<(String, String), String>,
    base_reds: BTreeSet<String>,
    flakes: BTreeSet<String>,
    base_reused: BTreeSet<String>,
    local_base_confirmed: BTreeSet<String>,
    base_worktrees: BTreeMap<String, PathBuf>,
    cache_root: Option<PathBuf>,
    skip_tests: bool,
    remote: Option<Arc<Mutex<remote_gate::Executor>>>,
    scratch: Option<GateScratch>,
    disk_scratch_used: BTreeSet<String>,
}

enum GateScratch {
    Tmpfs(tempfile::TempDir),
    Disk(PathBuf),
}

impl GateScratch {
    fn path(&self) -> &Path {
        match self {
            Self::Tmpfs(dir) => dir.path(),
            Self::Disk(path) => path,
        }
    }

    fn report(&self) -> String {
        let kind = match self {
            Self::Tmpfs(_) => "tmpfs",
            Self::Disk(_) => "disk",
        };
        format!("{kind}:{}", self.path().display())
    }
}

impl TestGate {
    fn for_landing() -> Result<Self, String> {
        // Hermetic binary tests opt in to a temporary cache explicitly.
        let cache_root = if cfg!(test) {
            None
        } else {
            Some(base_cache::default_root()?)
        };
        Ok(Self {
            cache_root,
            ..Self::default()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationState {
    NotPublished,
    Published,
    Unknown,
}

impl PublicationState {
    const fn label(self) -> &'static str {
        match self {
            Self::NotPublished => "not_published",
            Self::Published => "published",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    General,
    Policy,
    Cleanup,
    CanaryRed,
    DescendantGreen,
}

#[derive(Debug)]
struct LandFailure {
    state: PublicationState,
    kind: FailureKind,
    policy_fence: Option<landing_policy::PolicyFence>,
    candidate: Option<String>,
    fetched_tip: Option<String>,
    published_tip: Option<String>,
    observed_tip: Option<String>,
    forward_revert_id: Option<String>,
    forward_revert_status: Option<PublicationState>,
    recovery_path: Option<PathBuf>,
    stale_retries: Vec<StaleRetry>,
    message: String,
}

impl From<String> for LandFailure {
    fn from(message: String) -> Self {
        Self {
            state: PublicationState::NotPublished,
            kind: FailureKind::General,
            policy_fence: None,
            candidate: None,
            fetched_tip: None,
            published_tip: None,
            observed_tip: None,
            forward_revert_id: None,
            forward_revert_status: None,
            recovery_path: None,
            stale_retries: Vec::new(),
            message,
        }
    }
}

impl LandFailure {
    fn interrupted() -> Self {
        let mut failure = Self::from(
            "landing interrupted; fetch rolling and check source ancestry before retrying"
                .to_string(),
        );
        // A signal can arrive after a remote push but before its receipt.
        failure.state = PublicationState::Unknown;
        failure
    }

    fn candidate(
        state: PublicationState,
        message: impl Into<String>,
        candidate: &Candidate,
        fetched_tip: &str,
    ) -> Self {
        Self {
            state,
            kind: FailureKind::General,
            policy_fence: None,
            candidate: Some(candidate.oid.clone()),
            fetched_tip: Some(fetched_tip.to_string()),
            published_tip: (state == PublicationState::Published).then(|| candidate.oid.clone()),
            observed_tip: None,
            forward_revert_id: None,
            forward_revert_status: None,
            recovery_path: None,
            stale_retries: Vec::new(),
            message: message.into(),
        }
    }

    fn policy(
        refusal: landing_policy::PolicyRefusal,
        candidate: &Candidate,
        fetched_tip: &str,
    ) -> Self {
        let mut failure = Self::candidate(
            PublicationState::NotPublished,
            refusal.message,
            candidate,
            fetched_tip,
        );
        failure.kind = FailureKind::Policy;
        failure.policy_fence = Some(refusal.fence);
        failure
    }

    fn evidence_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("publication_status={}", self.state.label()),
            format!("landing_outcome={}", self.outcome_label()),
            format!("exit_code={}", self.exit_code()),
        ];
        if let Some(fence) = self.policy_fence {
            lines.push(format!("policy_fence={}", fence.label()));
        }
        if let Some(candidate) = &self.candidate {
            lines.push(format!("candidate_id={candidate}"));
        }
        if let Some(fetched_tip) = &self.fetched_tip {
            lines.push(format!("fetched_target_id={fetched_tip}"));
        }
        if let Some(published_tip) = &self.published_tip {
            lines.push(format!("published_target_id={published_tip}"));
        }
        if let Some(observed_tip) = &self.observed_tip {
            lines.push(format!("observed_target_id={observed_tip}"));
        }
        if let Some(forward_revert_id) = &self.forward_revert_id {
            lines.push(format!("forward_revert_id={forward_revert_id}"));
        }
        if let Some(forward_revert_status) = self.forward_revert_status {
            lines.push(format!(
                "forward_revert_status={}",
                forward_revert_status.label()
            ));
        }
        if let Some(recovery_path) = &self.recovery_path {
            lines.push(format!("recovery_path={}", recovery_path.display()));
        }
        lines.extend(stale_retry_lines(&self.stale_retries));
        lines
    }

    const fn outcome_label(&self) -> &'static str {
        match (self.kind, self.forward_revert_status, self.state) {
            (FailureKind::Policy, _, _) => "policy_refused",
            (FailureKind::Cleanup, _, _) => "published_cleanup_failed",
            (FailureKind::CanaryRed, Some(PublicationState::Published), _) => "canary_red_reverted",
            (FailureKind::CanaryRed, Some(PublicationState::Unknown), _) => {
                "canary_red_revert_unknown"
            }
            (FailureKind::CanaryRed, _, _) => "canary_red_unreverted",
            (FailureKind::DescendantGreen, _, _) => "published_descendant_unverified",
            (_, _, PublicationState::NotPublished) => "not_published",
            (_, _, PublicationState::Published) => "published_unverified",
            (_, _, PublicationState::Unknown) => "publication_unknown",
        }
    }

    const fn exit_code(&self) -> i32 {
        match (self.kind, self.forward_revert_status, self.state) {
            (FailureKind::Policy, _, _) => 8,
            (FailureKind::Cleanup, _, _) => 2,
            (FailureKind::CanaryRed, Some(PublicationState::Published), _) => 4,
            (FailureKind::CanaryRed, Some(PublicationState::Unknown), _) => 3,
            (FailureKind::CanaryRed, _, _) => 5,
            (FailureKind::DescendantGreen, _, _) => 7,
            (_, _, PublicationState::NotPublished) => 1,
            (_, _, PublicationState::Published) => 6,
            (_, _, PublicationState::Unknown) => 3,
        }
    }
}

fn main() {
    let result = parse_args(std::env::args().skip(1))
        .map_err(|error| Box::new(LandFailure::from(error)))
        .and_then(|options| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    Box::new(LandFailure::from(format!(
                        "cannot start async runtime: {error}"
                    )))
                })?;
            runtime
                .block_on(land_with_signals(options))
                .map_err(Box::new)
        });
    match result {
        Ok(report) => {
            println!("publication_status=published");
            println!("candidate_id={}", report.candidate);
            println!("candidate_kind={:?}", report.kind);
            println!("fetched_target_id={}", report.fetched_tip);
            println!("published_target_id={}", report.published_tip);
            println!("gate_scratch={}", report.gate_scratch);
            for entry in &report.gate_disk_scratch {
                println!("gate_disk_scratch={entry}");
            }
            if let Some(tree) = &report.canary_reused_tree {
                println!("canary: reused gate evidence for tree {tree}");
            }
            for name in &report.base_reds {
                println!("base_red={name}");
            }
            for name in &report.flakes {
                println!("candidate_flake={name}");
            }
            for entry in &report.base_reused {
                println!("base_reused={entry}");
            }
            for entry in &report.local_base_confirmed {
                println!("local_base_confirmed={entry}");
            }
            for binding in &report.source_bindings {
                println!("source_binding={}:{}", binding.source, binding.state);
            }
            for line in stale_retry_lines(&report.stale_retries) {
                println!("{line}");
            }
            for (index, unit) in report.provisional.iter().enumerate() {
                println!("migration_{index}_source_id={}", unit.source);
                println!("migration_{index}_final_version={}", unit.assigned_version);
                println!("migration_{index}_published_id={}", report.published_tip);
                println!("migration_{index}_proof={}", unit.proof);
            }
        }
        Err(error) => {
            for line in error.evidence_lines() {
                println!("{line}");
            }
            eprintln!("rsi-rolling-land: {}", error.message);
            std::process::exit(error.exit_code());
        }
    }
}

async fn land_with_signals(options: Options) -> Result<LandReport, LandFailure> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| {
        LandFailure::from(format!("cannot watch termination signal: {error}"))
    })?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|error| {
        LandFailure::from(format!("cannot watch interrupt signal: {error}"))
    })?;
    land_until_cancelled(options, async move {
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
        }
    })
    .await
}

async fn land_until_cancelled(
    options: Options,
    cancellation: impl std::future::Future<Output = ()>,
) -> Result<LandReport, LandFailure> {
    tokio::select! {
        result = land(options) => result,
        () = cancellation => Err(LandFailure::interrupted()),
    }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut args = args.into_iter();
    let mut repo = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut remote = "origin".to_string();
    let mut accepted = Vec::new();
    let mut test_filters = Vec::new();
    let mut remote_host = None;
    let mut remote_workdir = None;
    let mut remote_identity = None;
    let mut remote_run_as = "rsi".to_string();
    let mut tmpfs_min_free_gb = 12;
    let mut disk_scratch_shards = BTreeSet::new();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => repo = PathBuf::from(next_value(&mut args, "--repo")?),
            "--remote" => remote = next_value(&mut args, "--remote")?,
            "--accepted" => {
                let value = next_value(&mut args, "--accepted")?;
                let (base, source) = value.split_once(':').unwrap_or(("", &value));
                if source.is_empty() || value.starts_with(':') || value.matches(':').count() > 1 {
                    return Err("--accepted expects SOURCE or BASE:SOURCE IDs".to_string());
                }
                accepted.push(AcceptedPair {
                    base: base.to_string(),
                    source: source.to_string(),
                });
            }
            "--test-filter" => test_filters.push(next_value(&mut args, "--test-filter")?),
            "--remote-gate-host" => {
                remote_host = Some(next_value(&mut args, "--remote-gate-host")?)
            }
            "--remote-gate-dir" => {
                remote_workdir = Some(next_value(&mut args, "--remote-gate-dir")?)
            }
            "--remote-gate-identity" => {
                remote_identity = Some(PathBuf::from(next_value(
                    &mut args,
                    "--remote-gate-identity",
                )?))
            }
            "--remote-gate-run-as" => {
                remote_run_as = next_value(&mut args, "--remote-gate-run-as")?
            }
            "--tmpfs-min-free-gb" => {
                let value = next_value(&mut args, "--tmpfs-min-free-gb")?;
                tmpfs_min_free_gb = value
                    .parse::<u64>()
                    .ok()
                    .filter(|value| (1..=1024).contains(value))
                    .ok_or("--tmpfs-min-free-gb expects an integer from 1 to 1024")?;
            }
            "--disk-scratch-shard" => {
                let shard = next_value(&mut args, "--disk-scratch-shard")?;
                if !RSID_SHARDS.contains(&shard.as_str()) {
                    return Err(format!("unknown disk scratch shard: {shard}"));
                }
                disk_scratch_shards.insert(shard);
            }
            "--help" | "-h" => return Err(usage().to_string()),
            _ => return Err(format!("unknown argument `{argument}`\n{}", usage())),
        }
    }
    if accepted.is_empty() {
        return Err(format!(
            "at least one --accepted SOURCE or BASE:SOURCE is required\n{}",
            usage()
        ));
    }
    let remote_gate = match (remote_host, remote_workdir, remote_identity) {
        (None, None, None) if remote_run_as == "rsi" => None,
        (Some(target), Some(workdir), Some(identity)) => {
            let config = remote_gate::Config { target, workdir, identity, run_as: remote_run_as };
            config.validate()?;
            Some(config)
        }
        _ => return Err("remote shard gate requires --remote-gate-host USER@HOST, --remote-gate-dir PATH, and --remote-gate-identity PATH together".into()),
    };
    Ok(Options {
        repo,
        remote,
        accepted,
        test_filters,
        cargo_build_jobs: parse_cargo_build_jobs(std::env::var_os("CARGO_BUILD_JOBS").as_deref())?,
        remote_gate,
        tmpfs_min_free_gb,
        disk_scratch_shards,
    })
}

// Preserve the existing four-job ceiling while allowing central QA to lower it.
fn parse_cargo_build_jobs(value: Option<&std::ffi::OsStr>) -> Result<u8, String> {
    let Some(value) = value else {
        return Ok(4);
    };
    value
        .to_str()
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|jobs| (1..=6).contains(jobs))
        .ok_or_else(|| {
            "CARGO_BUILD_JOBS must be a positive integer from 1 to 6 (default: 4)".into()
        })
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

const fn usage() -> &'static str {
    "usage: rsi-rolling-land [--repo PATH] [--remote NAME] --accepted SOURCE|BASE:SOURCE [--accepted SOURCE|BASE:SOURCE ...] [--test-filter PACKAGE=FILTER ...] [--tmpfs-min-free-gb N] [--disk-scratch-shard SHARD ...] [--remote-gate-host USER@HOST --remote-gate-dir PATH --remote-gate-identity PATH [--remote-gate-run-as USER]]\nFor a landing candidate, omitted BASE is merge-base(SOURCE, current landing target tip); an explicit BASE must equal it. Already-integrated sources require the explicit historical BASE.\nAn rsid shard filter is rsid=shard:SHARD[:FILTERSET], for example rsid=shard:session-02:test(manager_recovery_). The full shard inventory is checked before any focused run.\n--tmpfs-min-free-gb requires N GiB free on /dev/shm before private gate TMPDIR is used (default 12); otherwise gates use sandbox target scratch. --disk-scratch-shard gives a named shard disk scratch on both sides.\nRemote shard execution is opt-in and refuses publication on missing evidence or mismatched commit/fingerprint. The SSH host must already be in known_hosts. Only full rsid shards run remotely; local regression comparison and isolated retries are preserved.\nCARGO_BUILD_JOBS: positive integer from 1 to 6; defaults to 4 when unset."
}

#[allow(clippy::too_many_lines)] // Keeps fetch, preparation, and owned cleanup visibly ordered.
async fn land(options: Options) -> Result<LandReport, LandFailure> {
    land_with_gate(options, TestGate::for_landing()?).await
}

#[allow(clippy::too_many_lines)] // Keeps fetch, preparation, and owned cleanup visibly ordered.
async fn land_with_gate(
    options: Options,
    mut test_gate: TestGate,
) -> Result<LandReport, LandFailure> {
    let mut options = options;
    let cargo_target_dir = validate_cargo_target_dir()?;
    let selected_scratch = select_gate_scratch(&cargo_target_dir, options.tmpfs_min_free_gb)?;
    println!("gate_scratch={}", selected_scratch.report());
    test_gate.scratch = Some(selected_scratch);
    let repo = options
        .repo
        .canonicalize()
        .map_err(|error| format!("cannot resolve repository: {error}"))?;
    let remote_url = git_text(&repo, &["remote", "get-url", "--push", &options.remote])?;
    let workspace_parent = landing_workspace_parent(&repo, &cargo_target_dir)?;
    let temp = tempfile::Builder::new()
        .prefix("rsi-rolling-land-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(workspace_parent)
        .map_err(|error| format!("cannot create private landing workspace: {error}"))?;
    let private_repo = temp.path().join("repo");
    git_ok(
        &repo,
        &[
            "clone",
            "--shared",
            "--no-checkout",
            "--quiet",
            &repo.to_string_lossy(),
            &private_repo.to_string_lossy(),
        ],
    )?;
    // Preserve both committed guards without checking out the full source tree.
    for script in [
        "scripts/rolling-landing-guard.py",
        "tools/check-released-migrations.py",
    ] {
        copy_committed_script(&private_repo, script)?;
    }
    if git_output(
        &private_repo,
        &["show", &format!("HEAD:{PROVISIONAL_SCRIPT}")],
    )?
    .status
    .success()
    {
        copy_committed_script(&private_repo, PROVISIONAL_SCRIPT)?;
    }
    for pair in &mut options.accepted {
        if !pair.base.is_empty() {
            pair.base = git_text(
                &private_repo,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("{}^{{commit}}", pair.base),
                ],
            )?;
        }
        pair.source = git_text(
            &private_repo,
            &[
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", pair.source),
            ],
        )?;
    }
    for (key, value) in [
        ("maintenance.auto", "false"),
        ("maintenance.autoDetach", "false"),
        ("gc.auto", "0"),
        ("gc.autoDetach", "false"),
        ("fetch.writeCommitGraph", "false"),
    ] {
        git_ok(&private_repo, &["config", "--local", key, value])?;
    }
    git_ok(&private_repo, &["remote", "add", "publish", &remote_url])?;
    // Keep the clone's primary worktree empty. Only engine-owned candidate
    // worktrees materialize files; retained recovery custody stays small.
    git_ok(
        &private_repo,
        &["symbolic-ref", "HEAD", "refs/heads/rsi-private-unborn"],
    )?;
    git_ok(&private_repo, &["update-ref", "-d", TARGET])?;

    // This fetch is intentionally adjacent to candidate preparation. It writes
    // only the private clone's rolling ref, never the caller's local branch.
    remote_fetch(&private_repo).await?;
    let fetched_tip = git_text(
        &private_repo,
        &["rev-parse", "--verify", "refs/heads/rolling^{commit}"],
    )?;
    let config = IntegrationConfig {
        allowed_targets: vec![TARGET.to_string()],
        identity: CommitIdentity {
            name: "rsi rolling landing".to_string(),
            email: "rsi-rolling-land@rsi.invalid".to_string(),
        },
        git_timeout: Duration::from_secs(120),
    };
    let scratch = temp.path().join("candidate-scratch");
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("cannot create candidate scratch directory: {error}"))?;
    let mut expected_tip = fetched_tip.clone();
    let mut candidate: Option<Candidate> = None;
    let mut provisional = Vec::<ProvisionalLanding>::new();
    let guard_options = options.clone();
    if let Some(config) = options.remote_gate.clone() {
        // A fingerprint records the toolchain and runner, but does not prove
        // that a desktop base result and cloud candidate ran under equivalent
        // host conditions. Compare both sides on this executor.
        test_gate.cache_root = None;
        test_gate.remote = Some(Arc::new(Mutex::new(remote_gate::Executor::new(
            config,
            private_repo.clone(),
        )?)));
    }
    for pair in &mut options.accepted {
        resolve_accepted_base(&private_repo, pair, &expected_tip)?;
        let pair = pair.clone();
        let unit_path = if !git_is_ancestor(&private_repo, &pair.source, &expected_tip)?
            && has_provisional_declaration(&private_repo, &pair)?
        {
            let mut unit = provisional_command(
                &private_repo,
                &[
                    "--transform",
                    "--base",
                    &pair.base,
                    "--source",
                    &pair.source,
                    "--target",
                    &expected_tip,
                ],
            )?;
            unit["prior_units"] = Value::Array(
                provisional
                    .iter()
                    .map(|prior| {
                        serde_json::json!({
                            "base": prior.base,
                            "source": prior.source,
                            "target": prior.proof["target"],
                            "unit_candidate": prior.unit_candidate,
                        })
                    })
                    .collect(),
            );
            let path = temp
                .path()
                .join(format!("provisional-unit-{}.json", provisional.len()));
            std::fs::write(&path, unit.to_string())
                .map_err(|error| format!("cannot record provisional unit: {error}"))?;
            Some((path, unit))
        } else {
            None
        };
        let prepared: Result<Prepared, String> = async {
            if let Some((path, _unit)) = &unit_path {
                let unit_arg = path.to_string_lossy().into_owned();
                let scratch_arg = scratch.to_string_lossy().into_owned();
                let built = provisional_command(
                    &private_repo,
                    &[
                        "--build",
                        "--unit-file",
                        &unit_arg,
                        "--scratch",
                        &scratch_arg,
                    ],
                )?;
                let exact_candidate = provisional_oid(&built, "candidate")?;
                let prepared = prepare_candidate(
                    &config,
                    &private_repo,
                    TARGET,
                    &expected_tip,
                    exact_candidate,
                    &scratch,
                )
                .await
                .map_err(|error| stale_error(error, &fetched_tip))?;
                Ok(match prepared {
                    Prepared::Candidate(mut next) => {
                        // The commit's checked parents, rather than the engine's
                        // fast-forward classification, describe this candidate.
                        next.kind = CandidateKind::Merge;
                        Prepared::Candidate(next)
                    }
                    other @ Prepared::AlreadyIntegrated => other,
                })
            } else {
                prepare_candidate(
                    &config,
                    &private_repo,
                    TARGET,
                    &expected_tip,
                    &pair.source,
                    &scratch,
                )
                .await
                .map_err(|error| stale_error(error, &fetched_tip))
            }
        }
        .await;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(previous) = candidate.take() {
                    let cleanup = discard_candidate(&config, &private_repo, &previous.handle).await;
                    if let Err(cleanup) = cleanup {
                        return Err(format!("{error}; candidate cleanup failed: {cleanup}").into());
                    }
                }
                return Err(error.into());
            }
        };
        let next = match prepared {
            Prepared::AlreadyIntegrated => continue,
            Prepared::Candidate(next) => next,
        };
        if let Some((unit_path, unit)) = unit_path {
            let unit_arg = unit_path.to_string_lossy().into_owned();
            let checked = (|| -> Result<(Value, PathBuf), String> {
                let proof = provisional_command(
                    &private_repo,
                    &[
                        "--prove",
                        "--unit-file",
                        &unit_arg,
                        "--unit-candidate",
                        &next.oid,
                        "--candidate",
                        &next.oid,
                    ],
                )?;
                let proof_path = temp
                    .path()
                    .join(format!("provisional-proof-{}.json", provisional.len()));
                std::fs::write(&proof_path, proof.to_string())
                    .map_err(|error| format!("cannot record provisional proof: {error}"))?;
                Ok((proof, proof_path))
            })();
            let checked = match checked {
                Ok((proof, proof_path)) => {
                    let guard = run_guard_pair_with_proof(
                        &private_repo,
                        &guard_options,
                        &pair,
                        &expected_tip,
                        &next.oid,
                        None,
                        false,
                        Some(&proof_path),
                        &mut test_gate,
                    )
                    .await;
                    guard.map(|()| (proof, proof_path))
                }
                Err(error) => Err(error),
            };
            let (proof, proof_path) = match checked {
                Ok(checked) => checked,
                Err(error) => {
                    let mut message = error;
                    if let Err(cleanup) =
                        discard_candidate(&config, &private_repo, &next.handle).await
                    {
                        let _ = write!(message, "; candidate cleanup failed: {cleanup}");
                    }
                    if let Some(previous) = candidate.take()
                        && let Err(cleanup) =
                            discard_candidate(&config, &private_repo, &previous.handle).await
                    {
                        let _ = write!(message, "; prior cleanup failed: {cleanup}");
                    }
                    return Err(message.into());
                }
            };
            println!("provisional_unit_proof={proof}");
            provisional.push(ProvisionalLanding {
                base: pair.base.clone(),
                source: pair.source.clone(),
                unit_path,
                unit_candidate: next.oid.clone(),
                proof_path,
                assigned_version: provisional_version(&unit, "new_version")?,
                proof,
            });
        }
        if let Err(error) = git_ok(
            &private_repo,
            &["update-ref", TARGET, &next.oid, &expected_tip],
        ) {
            let next_cleanup = discard_candidate(&config, &private_repo, &next.handle).await;
            if let Some(previous) = candidate.take() {
                let previous_cleanup =
                    discard_candidate(&config, &private_repo, &previous.handle).await;
                if let Err(cleanup) = previous_cleanup {
                    return Err(
                        format!("{error}; prior candidate cleanup failed: {cleanup}").into(),
                    );
                }
            }
            if let Err(cleanup) = next_cleanup {
                return Err(format!("{error}; candidate cleanup failed: {cleanup}").into());
            }
            return Err(error.into());
        }
        if let Some(previous) = candidate.take()
            && let Err(error) = discard_candidate(&config, &private_repo, &previous.handle).await
        {
            let next_cleanup = discard_candidate(&config, &private_repo, &next.handle).await;
            return Err(match next_cleanup {
                Ok(()) => format!("cannot clean superseded candidate: {error}").into(),
                Err(cleanup) => format!(
                    "cannot clean superseded candidate: {error}; current candidate cleanup failed: {cleanup}"
                )
                .into(),
            });
        }
        expected_tip = next.oid.clone();
        candidate = Some(next);
    }
    let Some(candidate) = candidate else {
        for pair in &options.accepted {
            run_guard_pair(
                &private_repo,
                &options,
                pair,
                &fetched_tip,
                &fetched_tip,
                None,
                false,
            )
            .await?;
        }
        return Err(format!(
            "all accepted sources are ancestors of rolling at {fetched_tip}; lost-hunk guard verified them, and no landing candidate was created"
        )
        .into());
    };
    if let Err(error) = finalize_provisional_proofs(&private_repo, &candidate.oid, &mut provisional)
    {
        let cleanup = discard_candidate(&config, &private_repo, &candidate.handle).await;
        return Err(match cleanup {
            Ok(()) => error.into(),
            Err(cleanup) => format!("{error}; candidate cleanup failed: {cleanup}").into(),
        });
    }
    let mut stale = Vec::new();
    let result = land_candidate(
        &private_repo,
        &repo,
        &remote_url,
        &options,
        &candidate,
        &fetched_tip,
        &provisional,
        &mut test_gate,
        &mut stale,
    )
    .await;
    let cleanup = discard_candidate(&config, &private_repo, &candidate.handle).await;
    let mut outcome = match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => {
            let mut failure = LandFailure::candidate(
                PublicationState::Published,
                format!("landing completed but candidate cleanup failed: {error}"),
                &candidate,
                &fetched_tip,
            );
            failure.kind = FailureKind::Cleanup;
            Err(failure)
        }
        (Err(error), Err(cleanup)) => Err(LandFailure {
            message: format!(
                "{}; candidate cleanup also failed: {cleanup}",
                error.message
            ),
            ..error
        }),
    };
    if let Err(error) = &mut outcome {
        // Every receipt shows how many publish races were lost, so an
        // operator can tell exhausted retries from a first-attempt failure.
        error.stale_retries.clone_from(&stale);
    }
    if let Err(error) = &mut outcome
        && (error.state == PublicationState::Unknown
            || error
                .forward_revert_status
                .is_some_and(|state| state != PublicationState::Published))
    {
        // An uncertain remote effect or failed forward revert needs its exact
        // candidate objects for reconciliation. Keep only this private clone;
        // the caller's source and checked-out rolling worktrees are untouched.
        error.recovery_path = Some(temp.keep());
    }
    outcome
}

fn finalize_provisional_proofs(
    repo: &Path,
    final_candidate: &str,
    provisional: &mut [ProvisionalLanding],
) -> Result<(), String> {
    for unit in provisional {
        let unit_arg = unit.unit_path.to_string_lossy().into_owned();
        let proof = provisional_command(
            repo,
            &[
                "--prove",
                "--unit-file",
                &unit_arg,
                "--unit-candidate",
                &unit.unit_candidate,
                "--candidate",
                final_candidate,
            ],
        )?;
        std::fs::write(&unit.proof_path, proof.to_string())
            .map_err(|error| format!("cannot record final provisional proof: {error}"))?;
        unit.proof = proof;
    }
    Ok(())
}

fn resolve_accepted_base(repo: &Path, pair: &mut AcceptedPair, target: &str) -> Result<(), String> {
    if git_is_ancestor(repo, &pair.source, target)? {
        if pair.base.is_empty() {
            return Err(format!(
                "accepted source {} is already integrated; supply --accepted BASE:{} with its historical accepted base for the plan-only lost-hunk check",
                pair.source, pair.source
            ));
        }
        return Ok(());
    }
    let merge_base = git_text(repo, &["merge-base", &pair.source, target])?;
    if pair.base.is_empty() {
        pair.base = merge_base;
    } else if pair.base != merge_base {
        return Err(format!(
            "accepted base {} does not match merge-base(source {}, target {}) = {}; use --accepted {}:{} or omit BASE",
            pair.base, pair.source, target, merge_base, merge_base, pair.source
        ));
    }
    Ok(())
}

fn has_provisional_declaration(repo: &Path, pair: &AcceptedPair) -> Result<bool, String> {
    let output = git_output(
        repo,
        &[
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            &pair.base,
            &pair.source,
        ],
    )?;
    if !output.status.success() {
        return Err("cannot inspect accepted migration declaration paths".into());
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .any(|path| path.starts_with(b"tools/provisional-migrations/") && path.ends_with(b".json")))
}

fn provisional_command(repo: &Path, args: &[&str]) -> Result<Value, String> {
    let script = repo.join(PROVISIONAL_SCRIPT);
    if !script.is_file() {
        return Err(format!(
            "committed provisional migration tool is missing: {}",
            script.display()
        ));
    }
    let mut command = Command::new("python3");
    command
        .arg(&script)
        .arg("--repo")
        .arg(repo)
        .args(args)
        .current_dir(repo)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONHOME");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") || key.to_string_lossy().starts_with("RSI_") {
            command.env_remove(key);
        }
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot start provisional migration tool: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "provisional migration refused: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("provisional migration tool returned invalid JSON: {error}"))
}

fn provisional_version(value: &Value, name: &str) -> Result<u32, String> {
    value[name]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| format!("provisional migration result lacks {name}"))
}

fn provisional_oid<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    let oid = value[name]
        .as_str()
        .ok_or_else(|| format!("provisional migration result lacks {name}"))?;
    if !matches!(oid.len(), 40 | 64)
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("provisional migration result has invalid {name}"));
    }
    Ok(oid)
}

fn validate_cargo_target_dir() -> Result<PathBuf, String> {
    let configured = std::env::var_os("CARGO_TARGET_DIR").ok_or_else(|| {
        "CARGO_TARGET_DIR must point to the current session's existing sandbox target directory"
            .to_string()
    })?;
    let path = PathBuf::from(configured)
        .canonicalize()
        .map_err(|error| format!("CARGO_TARGET_DIR is unavailable: {error}"))?;
    if !path.is_dir() {
        return Err("CARGO_TARGET_DIR must be an existing directory".into());
    }
    let temp = std::env::temp_dir()
        .canonicalize()
        .map_err(|error| format!("cannot resolve the system temporary directory: {error}"))?;
    if path.starts_with(&temp) {
        return Err("CARGO_TARGET_DIR must not be inside the system temporary directory".into());
    }
    Ok(path)
}

fn landing_workspace_parent(repo: &Path, cargo_target_dir: &Path) -> Result<PathBuf, String> {
    // A sandbox-local target is inside the source worktree. Keep recovery
    // custody outside Cargo's cleanable target tree and out of git status.
    if cargo_target_dir.starts_with(repo) {
        return PathBuf::from(git_text(repo, &["rev-parse", "--absolute-git-dir"])?)
            .canonicalize()
            .map_err(|error| format!("cannot resolve repository git directory: {error}"));
    }
    Ok(cargo_target_dir.to_path_buf())
}

fn select_gate_scratch(cargo_target_dir: &Path, min_free_gb: u64) -> Result<GateScratch, String> {
    let tmpfs = Path::new("/dev/shm");
    let enough_space = nix::sys::statvfs::statvfs(tmpfs)
        .map(|stats| {
            let free = u128::from(stats.blocks_available())
                .saturating_mul(u128::from(stats.fragment_size()));
            free >= u128::from(min_free_gb) * 1024 * 1024 * 1024
        })
        .unwrap_or(false);
    if enough_space
        && let Ok(dir) = tempfile::Builder::new()
            .prefix("rsi-landing-gate-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(tmpfs)
    {
        return Ok(GateScratch::Tmpfs(dir));
    }
    let disk = cargo_target_dir.join(".rsi-tmp");
    std::fs::create_dir_all(&disk)
        .map_err(|error| format!("cannot create disk-backed gate scratch: {error}"))?;
    Ok(GateScratch::Disk(disk))
}

fn stale_error(error: IntegrationError, fetched_tip: &str) -> String {
    match error {
        IntegrationError::Refused(Refusal::StaleTarget { observed }) => {
            format!("stale target: fetched {fetched_tip}, observed {observed:?}")
        }
        other => other.to_string(),
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // Keeps gate, canary, and remote settlement in order.
async fn land_candidate(
    repo: &Path,
    source_repo: &Path,
    remote_url: &str,
    options: &Options,
    candidate: &Candidate,
    fetched_tip: &str,
    provisional: &[ProvisionalLanding],
    test_gate: &mut TestGate,
    stale: &mut Vec<StaleRetry>,
) -> Result<LandReport, LandFailure> {
    for pair in &options.accepted {
        let proof = provisional
            .iter()
            .find(|unit| unit.source == pair.source && unit.base == pair.base)
            .map(|unit| unit.proof_path.as_path());
        run_guard_pair_with_proof(
            repo,
            options,
            pair,
            fetched_tip,
            &candidate.oid,
            Some(&candidate.handle.worktree),
            false,
            proof,
            test_gate,
        )
        .await
        .map_err(|error| {
            LandFailure::candidate(
                PublicationState::NotPublished,
                error,
                candidate,
                fetched_tip,
            )
        })?;
    }
    // A disjoint stale advance may reuse the earlier gate without testing
    // its new tree. Only a gate executed for this candidate can be reused.
    let candidate_was_gated = !test_gate.skip_tests;
    test_gate.skip_tests = false;

    verify_remote_binding(source_repo, &options.remote, remote_url).map_err(|error| {
        LandFailure::candidate(
            PublicationState::NotPublished,
            error,
            candidate,
            fetched_tip,
        )
    })?;
    let proved_versions = provisional
        .iter()
        .map(|unit| unit.assigned_version)
        .collect::<Vec<_>>();
    let source_bindings = landing_policy::check_with_proof(
        repo,
        repo,
        fetched_tip,
        &candidate.oid,
        &options.accepted,
        &proved_versions,
    )
    .map_err(|error| LandFailure::policy(error, candidate, fetched_tip))?;
    let published_tip = match publish_candidate(repo, candidate, fetched_tip).await {
        Ok(tip) => tip,
        Err(mut error) if error.state == PublicationState::NotPublished => {
            let Some(observed) = error.observed_tip.clone() else {
                return Err(error);
            };
            if stale.len() >= MAX_STALE_RETRIES {
                let _ = write!(
                    error.message,
                    "; stale retries exhausted after {} attempts",
                    stale.len()
                );
                return Err(error);
            }
            return retry_stale_candidate(
                repo,
                source_repo,
                remote_url,
                options,
                candidate,
                fetched_tip,
                &observed,
                provisional,
                test_gate,
                stale,
            )
            .await;
        }
        Err(error) => return Err(error),
    };
    let candidate_tree = git_text(repo, &["rev-parse", &format!("{}^{{tree}}", candidate.oid)])
        .map_err(|error| {
            LandFailure::candidate(PublicationState::Published, error, candidate, fetched_tip)
        })?;
    let published_tree = git_text(repo, &["rev-parse", &format!("{published_tip}^{{tree}}")])
        .map_err(|error| {
            LandFailure::candidate(PublicationState::Published, error, candidate, fetched_tip)
        })?;
    let canary_reused_tree =
        (candidate_was_gated && candidate_tree == published_tree).then_some(published_tree);
    for pair in &options.accepted {
        if canary_reused_tree.is_some() {
            break;
        }
        let proof = provisional
            .iter()
            .find(|unit| unit.source == pair.source && unit.base == pair.base)
            .map(|unit| unit.proof_path.as_path());
        if let Err(error) = run_guard_pair_with_proof(
            repo,
            options,
            pair,
            fetched_tip,
            &published_tip,
            Some(&candidate.handle.worktree),
            false,
            proof,
            test_gate,
        )
        .await
        {
            return Err(forward_revert_after_canary(
                repo,
                source_repo,
                options,
                remote_url,
                candidate,
                fetched_tip,
                error,
            )
            .await);
        }
    }
    match remote_tip(repo, "publish").await {
        Ok(Some(tip)) if tip == published_tip => {}
        Ok(Some(observed)) => {
            // Another ordinary push may have landed on our candidate while
            // the canary ran. Fetch into private custody to prove ancestry;
            // this reports a distinct outcome because the new tip was not
            // covered by our candidate canary.
            if remote_fetch(repo).await.is_ok()
                && git_is_ancestor(repo, &published_tip, &observed).unwrap_or(false)
            {
                let mut failure = LandFailure::candidate(
                    PublicationState::Published,
                    "remote advanced from the green candidate; combined tip needs verification",
                    candidate,
                    fetched_tip,
                );
                failure.kind = FailureKind::DescendantGreen;
                failure.observed_tip = Some(observed);
                return Err(failure);
            }
            let mut failure = LandFailure::candidate(
                PublicationState::Published,
                "post-canary remote tip changed; exact published-tip verification is required",
                candidate,
                fetched_tip,
            );
            failure.observed_tip = Some(observed);
            return Err(failure);
        }
        Ok(None) => {
            let failure = LandFailure::candidate(
                PublicationState::Published,
                "post-canary remote tip is missing; exact published-tip verification is required",
                candidate,
                fetched_tip,
            );
            return Err(failure);
        }
        Err(error) => {
            return Err(LandFailure::candidate(
                PublicationState::Published,
                format!("post-canary remote verification failed: {error}"),
                candidate,
                fetched_tip,
            ));
        }
    }
    verify_remote_binding(source_repo, &options.remote, remote_url).map_err(|error| {
        LandFailure::candidate(PublicationState::Published, error, candidate, fetched_tip)
    })?;
    Ok(LandReport {
        candidate: candidate.oid.clone(),
        kind: candidate.kind,
        fetched_tip: fetched_tip.to_string(),
        published_tip,
        provisional: provisional.to_vec(),
        source_bindings,
        base_reds: test_gate.base_reds.clone(),
        flakes: test_gate.flakes.clone(),
        base_reused: test_gate.base_reused.clone(),
        local_base_confirmed: test_gate.local_base_confirmed.clone(),
        canary_reused_tree,
        gate_scratch: test_gate
            .scratch
            .as_ref()
            .ok_or_else(|| {
                LandFailure::candidate(
                    PublicationState::Published,
                    "gate scratch was not selected",
                    candidate,
                    fetched_tip,
                )
            })?
            .report(),
        gate_disk_scratch: test_gate.disk_scratch_used.clone(),
        stale_retries: stale.clone(),
    })
}

fn changed_paths(repo: &Path, from: &str, to: &str) -> Result<BTreeSet<Vec<u8>>, String> {
    let output = git_output(
        repo,
        &["diff", "--name-only", "-z", "--no-renames", from, to, "--"],
    )?;
    if !output.status.success() {
        return Err(format!(
            "cannot compare stale target paths: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

fn touches_provisional_gate(
    repo: &Path,
    options: &Options,
    fetched_tip: &str,
    current: &str,
    incoming_paths: &BTreeSet<Vec<u8>>,
) -> Result<bool, String> {
    // Keep the fixed entries aligned with check-released-migrations.py's
    // tracked_source_paths. Manifest sections supply its remaining paths.
    let mut surface = BTreeSet::from([
        b"crates/rsid/src/store/mod.rs".to_vec(),
        b"crates/rsid/src/store/cohort_settlement.rs".to_vec(),
        b"crates/rsid/src/store/tests.rs".to_vec(),
        b"tools/released-migrations.json".to_vec(),
        b"tools/rolling-migration-renumber.py".to_vec(),
        b"tools/check-released-migrations.py".to_vec(),
        b"scripts/rolling-landing-guard.py".to_vec(),
        b"scripts/run-rsid-test-shards.sh".to_vec(),
        b"scripts/rolling-shard-fingerprint.py".to_vec(),
    ]);
    let revisions = std::iter::once(fetched_tip)
        .chain(std::iter::once(current))
        .chain(options.accepted.iter().map(|pair| pair.base.as_str()))
        .collect::<BTreeSet<_>>();
    for revision in revisions {
        let output = git_output(
            repo,
            &[
                "show",
                &format!("{revision}:tools/released-migrations.json"),
            ],
        )?;
        if !output.status.success() {
            return Err(format!(
                "cannot read provisional gate inventory at {revision}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let manifest: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            format!("invalid provisional gate inventory at {revision}: {error}")
        })?;
        let migration_file = manifest["migration_file"].as_str().ok_or_else(|| {
            format!("provisional gate inventory lacks migration_file at {revision}")
        })?;
        surface.insert(migration_file.as_bytes().to_vec());
        let sections = manifest["protected_sections"].as_object().ok_or_else(|| {
            format!("provisional gate inventory lacks protected_sections at {revision}")
        })?;
        for section in sections.values() {
            let path = section["path"].as_str().ok_or_else(|| {
                format!("provisional gate inventory has invalid protected path at {revision}")
            })?;
            surface.insert(path.as_bytes().to_vec());
        }
    }
    Ok(incoming_paths
        .iter()
        .any(|path| surface.contains(path) || path.starts_with(b"tools/provisional-migrations/")))
}

async fn remake_stale_provisional(
    repo: &Path,
    options: &Options,
    current: &str,
    previous: &[ProvisionalLanding],
    config: &IntegrationConfig,
    scratch: &Path,
) -> Result<(Candidate, Vec<ProvisionalLanding>), String> {
    let mut tip = current.to_owned();
    let mut candidate: Option<Candidate> = None;
    let mut provisional = Vec::<ProvisionalLanding>::new();
    for pair in &options.accepted {
        if git_is_ancestor(repo, &pair.source, &tip)? {
            continue;
        }
        let step: Result<(Candidate, Option<ProvisionalLanding>), String> = async {
            let original = previous
                .iter()
                .find(|unit| unit.base == pair.base && unit.source == pair.source);
            let built = if let Some(original) = original {
                let mut unit = provisional_command(
                    repo,
                    &[
                        "--transform",
                        "--base",
                        &pair.base,
                        "--source",
                        &pair.source,
                        "--target",
                        &tip,
                    ],
                )?;
                let version = provisional_version(&unit, "new_version")?;
                if version != original.assigned_version {
                    return Err(format!(
                        "stale provisional migration version changed from {} to {version}",
                        original.assigned_version
                    ));
                }
                unit["prior_units"] = Value::Array(
                    provisional
                        .iter()
                        .map(|prior| {
                            serde_json::json!({
                                "base": prior.base,
                                "source": prior.source,
                                "target": prior.proof["target"],
                                "unit_candidate": prior.unit_candidate,
                            })
                        })
                        .collect(),
                );
                let unit_path =
                    scratch.join(format!("provisional-unit-{}.json", provisional.len()));
                std::fs::write(&unit_path, unit.to_string())
                    .map_err(|error| format!("cannot record stale provisional unit: {error}"))?;
                let built = provisional_command(
                    repo,
                    &[
                        "--build",
                        "--unit-file",
                        &unit_path.to_string_lossy(),
                        "--scratch",
                        &scratch.to_string_lossy(),
                    ],
                )?;
                Some((unit_path, provisional_oid(&built, "candidate")?.to_owned()))
            } else {
                None
            };
            let source = built
                .as_ref()
                .map_or(pair.source.as_str(), |(_, oid)| oid.as_str());
            let next = match prepare_candidate(config, repo, TARGET, &tip, source, scratch).await {
                Ok(Prepared::Candidate(mut next)) => {
                    if built.is_some() {
                        next.kind = CandidateKind::Merge;
                    }
                    next
                }
                Ok(Prepared::AlreadyIntegrated) => {
                    return Err("accepted source became integrated during stale retry".into());
                }
                Err(error) => return Err(format!("stale target could not merge cleanly: {error}")),
            };
            let checked = (|| -> Result<Option<ProvisionalLanding>, String> {
                let landing = if let Some((unit_path, _)) = built {
                    let proof_path =
                        scratch.join(format!("provisional-proof-{}.json", provisional.len()));
                    let proof = provisional_command(
                        repo,
                        &[
                            "--prove",
                            "--unit-file",
                            &unit_path.to_string_lossy(),
                            "--unit-candidate",
                            &next.oid,
                            "--candidate",
                            &next.oid,
                        ],
                    )?;
                    std::fs::write(&proof_path, proof.to_string()).map_err(|error| {
                        format!("cannot record stale provisional proof: {error}")
                    })?;
                    Some(ProvisionalLanding {
                        base: pair.base.clone(),
                        source: pair.source.clone(),
                        unit_path,
                        unit_candidate: next.oid.clone(),
                        proof_path,
                        assigned_version: original
                            .expect("built provisional has prior proof")
                            .assigned_version,
                        proof,
                    })
                } else {
                    None
                };
                git_ok(repo, &["update-ref", TARGET, &next.oid, &tip])?;
                Ok(landing)
            })();
            match checked {
                Ok(landing) => Ok((next, landing)),
                Err(error) => {
                    let cleanup = discard_candidate(config, repo, &next.handle).await;
                    Err(match cleanup {
                        Ok(()) => error,
                        Err(cleanup) => {
                            format!("{error}; stale candidate cleanup failed: {cleanup}")
                        }
                    })
                }
            }
        }
        .await;
        let (next, landing) = match step {
            Ok(step) => step,
            Err(error) => {
                if let Some(prior) = candidate.take() {
                    let cleanup = discard_candidate(config, repo, &prior.handle).await;
                    return Err(match cleanup {
                        Ok(()) => error,
                        Err(cleanup) => {
                            format!("{error}; prior stale candidate cleanup failed: {cleanup}")
                        }
                    });
                }
                return Err(error);
            }
        };
        if let Some(prior) = candidate.take()
            && let Err(error) = discard_candidate(config, repo, &prior.handle).await
        {
            let cleanup = discard_candidate(config, repo, &next.handle).await;
            return Err(match cleanup {
                Ok(()) => format!("cannot clean superseded stale candidate: {error}"),
                Err(cleanup) => format!(
                    "cannot clean superseded stale candidate: {error}; current cleanup failed: {cleanup}"
                ),
            });
        }
        if let Some(landing) = landing {
            provisional.push(landing);
        }
        tip = next.oid.clone();
        candidate = Some(next);
    }
    let candidate = candidate.ok_or("all accepted sources became integrated during stale retry")?;
    if let Err(error) = finalize_provisional_proofs(repo, &candidate.oid, &mut provisional) {
        let cleanup = discard_candidate(config, repo, &candidate.handle).await;
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; stale candidate cleanup failed: {cleanup}"),
        });
    }
    Ok((candidate, provisional))
}

#[allow(clippy::too_many_arguments)]
async fn retry_stale_candidate(
    repo: &Path,
    source_repo: &Path,
    remote_url: &str,
    options: &Options,
    candidate: &Candidate,
    fetched_tip: &str,
    observed: &str,
    provisional: &[ProvisionalLanding],
    test_gate: &mut TestGate,
    stale: &mut Vec<StaleRetry>,
) -> Result<LandReport, LandFailure> {
    let fail = |message: String| {
        LandFailure::candidate(
            PublicationState::NotPublished,
            message,
            candidate,
            fetched_tip,
        )
    };
    remote_fetch(repo).await.map_err(fail)?;
    let current = git_text(
        repo,
        &["rev-parse", "--verify", "refs/heads/rolling^{commit}"],
    )
    .map_err(fail)?;
    // Under concurrent landing the remote can advance again between the
    // observation and this fetch. Any fast-forward of the observed tip is a
    // valid retry base: disjointness below is computed against `current`.
    if !git_is_ancestor(repo, observed, &current).map_err(fail)?
        || !git_is_ancestor(repo, fetched_tip, &current).map_err(fail)?
    {
        return Err(fail(format!(
            "stale target changed during retry: expected descendant {observed}, fetched {current}"
        )));
    }
    let candidate_paths = changed_paths(repo, fetched_tip, &candidate.oid).map_err(fail)?;
    let incoming_paths = changed_paths(repo, fetched_tip, &current).map_err(fail)?;
    let disjoint = candidate_paths.is_disjoint(&incoming_paths)
        && (provisional.is_empty()
            || !touches_provisional_gate(repo, options, fetched_tip, &current, &incoming_paths)
                .map_err(fail)?);
    let regated = stale.iter().filter(|retry| !retry.reused_gate).count();
    if !disjoint && regated >= MAX_REGATED_STALE_RETRIES {
        let mut failure = fail(format!(
            "stale retries exhausted: {current} overlaps the candidate after {regated} re-gated retries ({} total)",
            stale.len()
        ));
        failure.observed_tip = Some(current);
        return Err(failure);
    }
    stale.push(StaleRetry {
        fetched: fetched_tip.to_string(),
        observed: current.clone(),
        reused_gate: disjoint,
    });
    let config = IntegrationConfig {
        allowed_targets: vec![TARGET.to_string()],
        identity: CommitIdentity {
            name: "rsi rolling landing".into(),
            email: "rsi-rolling-land@rsi.invalid".into(),
        },
        git_timeout: Duration::from_secs(120),
    };
    let scratch = repo
        .parent()
        .ok_or_else(|| fail("private repository has no parent".into()))?
        .join(format!("stale-scratch-{}", stale.len()));
    std::fs::create_dir_all(&scratch)
        .map_err(|error| fail(format!("cannot create stale scratch: {error}")))?;
    let (remade, remade_provisional) =
        if provisional.is_empty() {
            let remade =
                match prepare_candidate(&config, repo, TARGET, &current, &candidate.oid, &scratch)
                    .await
                {
                    Ok(Prepared::Candidate(next)) => next,
                    Ok(Prepared::AlreadyIntegrated) => {
                        return Err(fail(
                            "candidate already integrated during stale retry".into(),
                        ));
                    }
                    Err(error) => {
                        return Err(fail(format!(
                            "stale target could not merge cleanly: {error}"
                        )));
                    }
                };
            (remade, Vec::new())
        } else {
            remake_stale_provisional(repo, options, &current, provisional, &config, &scratch)
                .await
                .map_err(fail)?
        };
    let previous_skip = test_gate.skip_tests;
    test_gate.skip_tests = disjoint;
    let result = Box::pin(land_candidate(
        repo,
        source_repo,
        remote_url,
        options,
        &remade,
        &current,
        &remade_provisional,
        test_gate,
        stale,
    ))
    .await;
    test_gate.skip_tests = previous_skip;
    let cleanup = discard_candidate(&config, repo, &remade.handle).await;
    match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(report), Err(error)) => {
            let mut failure = LandFailure::candidate(
                PublicationState::Published,
                format!("stale candidate published but cleanup failed: {error}"),
                &remade,
                &current,
            );
            failure.kind = FailureKind::Cleanup;
            failure.published_tip = Some(report.published_tip);
            Err(failure)
        }
        (Err(mut error), Err(cleanup)) => {
            let _ = write!(error.message, "; stale candidate cleanup failed: {cleanup}");
            Err(error)
        }
    }
}

fn verify_remote_binding(repo: &Path, remote: &str, expected_url: &str) -> Result<(), String> {
    let observed = git_text(repo, &["remote", "get-url", "--push", remote])?;
    if observed != expected_url {
        return Err("configured publishing remote changed during landing".into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct GuardPlan {
    accepted_source: String,
    candidate: String,
    lost_hunks: Vec<String>,
    affected_crates: Vec<String>,
}

#[derive(Deserialize)]
struct WorkspaceMetadata {
    workspace_members: Vec<String>,
    packages: Vec<WorkspacePackage>,
}

#[derive(Deserialize)]
struct WorkspacePackage {
    id: String,
    name: String,
    dependencies: Vec<WorkspaceDependency>,
}

#[derive(Deserialize)]
struct WorkspaceDependency {
    name: String,
    path: Option<PathBuf>,
}

fn workspace_metadata(worktree: &Path) -> Result<WorkspaceMetadata, String> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(worktree)
        .output()
        .map_err(|error| format!("cannot inspect candidate workspace: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot inspect candidate workspace: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid candidate workspace metadata: {error}"))
}

fn reverse_workspace_dependents(
    metadata: &WorkspaceMetadata,
    affected: &[String],
) -> Result<Vec<String>, String> {
    let members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let packages: BTreeMap<&str, &WorkspacePackage> = metadata
        .packages
        .iter()
        .filter(|package| members.contains(package.id.as_str()))
        .map(|package| (package.name.as_str(), package))
        .collect();
    for name in affected {
        if !packages.contains_key(name.as_str()) {
            return Err(format!("affected crate {name} is not a workspace package"));
        }
    }
    let mut reached: BTreeSet<&str> = affected.iter().map(String::as_str).collect();
    loop {
        let newly_reached: Vec<&str> = packages
            .iter()
            .filter(|(name, package)| {
                !reached.contains(*name)
                    && package.dependencies.iter().any(|dependency| {
                        dependency.path.is_some() && reached.contains(dependency.name.as_str())
                    })
            })
            .map(|(name, _)| *name)
            .collect();
        if newly_reached.is_empty() {
            break;
        }
        reached.extend(newly_reached);
    }
    Ok(reached
        .into_iter()
        .filter(|name| !affected.iter().any(|affected| affected == name))
        .map(str::to_owned)
        .collect())
}

async fn run_guard_pair(
    repo: &Path,
    options: &Options,
    pair: &AcceptedPair,
    fetched_tip: &str,
    candidate: &str,
    worktree: Option<&Path>,
    allow_lost_hunks: bool,
) -> Result<(), String> {
    let mut test_gate = TestGate::for_landing()?;
    run_guard_pair_with_proof(
        repo,
        options,
        pair,
        fetched_tip,
        candidate,
        worktree,
        allow_lost_hunks,
        None,
        &mut test_gate,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_guard_pair_with_proof(
    repo: &Path,
    options: &Options,
    pair: &AcceptedPair,
    fetched_tip: &str,
    candidate: &str,
    worktree: Option<&Path>,
    allow_lost_hunks: bool,
    proof: Option<&Path>,
    test_gate: &mut TestGate,
) -> Result<(), String> {
    let script = repo.join("scripts/rolling-landing-guard.py");
    if !script.is_file() {
        return Err(format!(
            "landing guard script is missing: {}",
            script.display()
        ));
    }
    let args = vec![
        script.to_string_lossy().into_owned(),
        "--repo".into(),
        repo.to_string_lossy().into_owned(),
        "--base".into(),
        pair.base.clone(),
        "--source".into(),
        pair.source.clone(),
        "--target".into(),
        fetched_tip.into(),
        "--candidate".into(),
        candidate.into(),
        "--plan-only".into(),
    ];
    let mut args = args;
    if let Some(proof) = proof {
        args.extend(["--proof".into(), proof.to_string_lossy().into_owned()]);
    }
    for filter in &options.test_filters {
        args.extend(["--test-filter".into(), filter.clone()]);
    }
    let plan_report = run_guard(
        repo,
        &GuardSpec {
            commands: vec![GuardCommand {
                program: "python3".into(),
                args,
                timeout: Duration::from_secs(120),
            }],
            env: BTreeMap::new(),
            output_tail_bytes: 256 * 1024,
        },
    )
    .await;
    let Some(plan_command) = plan_report.commands.first() else {
        return Err("rolling landing guard did not run".into());
    };
    let plan: GuardPlan = serde_json::from_str(&plan_command.stdout_tail).map_err(|error| {
        format!(
            "landing guard rejected accepted pair {}:{} ({:?}): {error}; {}",
            pair.base, pair.source, plan_command.status, plan_command.stderr_tail
        )
    })?;
    if plan.accepted_source != pair.source || plan.candidate != candidate {
        return Err("landing guard plan does not match the accepted source and candidate".into());
    }
    if let Some(worktree) = worktree {
        if git_text(worktree, &["rev-parse", "HEAD"])? != candidate
            || !git_text(worktree, &["status", "--porcelain=v1"])?.is_empty()
        {
            return Err("landing guard worktree must be clean at the candidate".into());
        }
        let metadata = workspace_metadata(worktree)?;
        let spec = affected_crate_guard_spec(
            fetched_tip,
            candidate,
            &plan.affected_crates,
            &options.test_filters,
            proof.is_some(),
            &metadata,
            options.cargo_build_jobs,
        )?;
        run_affected_gate(
            repo,
            worktree,
            fetched_tip,
            &spec,
            test_gate,
            &options.disk_scratch_shards,
        )
        .await?;
    }
    if !plan.lost_hunks.is_empty() && !allow_lost_hunks {
        return Err(format!(
            "rolling landing guard rejected accepted pair {}:{} after affected-crate tests: {}",
            pair.base,
            pair.source,
            plan.lost_hunks.join("; ")
        ));
    }
    if !plan_report.passed && (!allow_lost_hunks || plan.lost_hunks.is_empty()) {
        return Err(format!(
            "rolling landing guard rejected accepted pair {}:{} ({:?}): {}",
            pair.base, pair.source, plan_command.status, plan_command.stderr_tail
        ));
    }
    if proof.is_some() {
        landing_policy::check_released_migrations(repo, repo, fetched_tip, candidate)?;
    }
    Ok(())
}

fn test_failure_names(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("test ") {
                return rest.strip_suffix(" ... FAILED").map(str::to_owned);
            }
            // Nextest prints a duration, an ordinal, and the crate before
            // the test identity. The final token is the exact retry target.
            line.strip_prefix("FAIL [")
                .and_then(|_| line.split_whitespace().last())
                .map(str::to_owned)
        })
        .collect()
}

fn observed_test_failures(
    report: &rsid::integration::GuardCommandReport,
) -> Result<BTreeSet<String>, String> {
    use rsid::integration::GuardStatus;
    if report.output_truncated {
        return Err(format!(
            "test output was truncated for {} {:?}",
            report.program, report.args
        ));
    }
    let names = test_failure_names(&format!("{}\n{}", report.stdout_tail, report.stderr_tail));
    match &report.status {
        GuardStatus::Passed if names.is_empty() => Ok(names),
        GuardStatus::Failed { .. } if !names.is_empty() => Ok(names),
        _ => Err(format!(
            "affected-crate guard failed ({:?}) running {} {:?}: {} {}",
            report.status, report.program, report.args, report.stdout_tail, report.stderr_tail
        )),
    }
}

fn isolated_retry_command(command: &GuardCommand, name: &str) -> Result<GuardCommand, String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
    {
        return Err(format!("cannot isolate test failure: {name}"));
    }
    let mut retry = command.clone();
    if retry.program == "scripts/run-rsid-test-shards.sh" {
        let shard = retry.args.get(1).ok_or("rsid shard command has no shard")?;
        retry.program = "cargo".into();
        retry.args = vec![
            "test".into(),
            "-p".into(),
            "rsid".into(),
            "--lib".into(),
            "--no-default-features".into(),
            "--features".into(),
            format!("test-shard-{shard}"),
            name.into(),
            "--".into(),
            "--exact".into(),
            "--test-threads=4".into(),
        ];
    } else if retry.program == "cargo" {
        let separator = retry
            .args
            .iter()
            .position(|arg| arg == "--")
            .ok_or("cargo test command has no argument separator")?;
        if separator < 4 || retry.args.first().is_none_or(|arg| arg != "test") {
            return Err("unsupported cargo test command for isolated retry".into());
        }
        retry.args.truncate(4);
        retry.args.extend([
            name.into(),
            "--".into(),
            "--exact".into(),
            "--test-threads=4".into(),
        ]);
    } else {
        return Err("unsupported test command for isolated retry".into());
    }
    Ok(retry)
}

async fn run_one_guard(
    worktree: &Path,
    spec: &GuardSpec,
    command: &GuardCommand,
) -> Result<rsid::integration::GuardCommandReport, String> {
    let mut env = spec.env.clone();
    if command.program == "cargo" || command.program == "scripts/run-rsid-test-shards.sh" {
        // Cargo fingerprints use file mtimes. A candidate worktree can be
        // older than artifacts produced by the base run, so sharing one target
        // may compile candidate rsid against base rsi-common metadata.
        let parent = worktree.parent().ok_or("guard worktree has no parent")?;
        let name = worktree
            .file_name()
            .ok_or("guard worktree has no file name")?
            .to_string_lossy();
        env.insert(
            "CARGO_TARGET_DIR".into(),
            parent
                .join(format!("{name}-cargo-target"))
                .to_string_lossy()
                .into_owned(),
        );
    }
    let report = run_guard(
        worktree,
        &GuardSpec {
            commands: vec![command.clone()],
            env,
            output_tail_bytes: spec.output_tail_bytes,
        },
    )
    .await;
    report
        .commands
        .into_iter()
        .next()
        .ok_or_else(|| "landing guard executed no command".into())
}

fn full_shard(command: &GuardCommand) -> Option<(&str, u32)> {
    if command.program != "scripts/run-rsid-test-shards.sh" || command.args.len() != 4 {
        return None;
    }
    let args = &command.args;
    if args[0] != "shard" || args[2] != "--jobs" || !RSID_SHARDS.contains(&args[1].as_str()) {
        return None;
    }
    Some((&args[1], args[3].parse().ok()?))
}

fn is_test_guard(command: &GuardCommand) -> bool {
    (command.program == "cargo" && command.args.first().is_some_and(|arg| arg == "test"))
        || command.program == "scripts/run-rsid-test-shards.sh"
}

fn shard_fingerprint(
    worktree: &Path,
    base: &str,
    shard: &str,
    jobs: u32,
) -> Result<String, String> {
    let output = Command::new("python3")
        .arg(worktree.join("scripts/rolling-shard-fingerprint.py"))
        .args(["--sha", base, "--shard", shard, "--jobs", &jobs.to_string()])
        .current_dir(worktree)
        .output()
        .map_err(|error| format!("cannot run shard fingerprint: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot fingerprint shard runner: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let fingerprint = String::from_utf8(output.stdout)
        .map_err(|error| format!("non-UTF8 shard fingerprint: {error}"))?;
    Ok(fingerprint.trim().to_owned())
}

async fn run_shard_or_local(
    worktree: &Path,
    fingerprint_source: &Path,
    base_sha: &str,
    spec: &GuardSpec,
    command: &GuardCommand,
    gate: &TestGate,
) -> Result<rsid::integration::GuardCommandReport, String> {
    let Some((shard, jobs)) = full_shard(command) else {
        return run_one_guard(worktree, spec, command).await;
    };
    let Some(remote) = gate.remote.clone() else {
        return run_one_guard(worktree, spec, command).await;
    };
    let sha = git_text(worktree, &["rev-parse", "HEAD"])?;
    let fingerprint_source_sha = git_text(fingerprint_source, &["rev-parse", "HEAD"])?;
    let shard = shard.to_owned();
    let command = command.clone();
    let base_sha = base_sha.to_owned();
    tokio::task::spawn_blocking(move || {
        remote
            .lock()
            .map_err(|_| "remote_missing_evidence: executor lock poisoned".to_string())?
            .run_full_shard(
                &sha,
                &fingerprint_source_sha,
                &base_sha,
                &shard,
                jobs,
                &command,
            )
    })
    .await
    .map_err(|error| format!("remote_missing_evidence: executor task failed: {error}"))?
}

fn ensure_base_worktree(repo: &Path, base: &str, gate: &mut TestGate) -> Result<PathBuf, String> {
    if let Some(path) = gate.base_worktrees.get(base) {
        return Ok(path.clone());
    }
    let path = repo
        .parent()
        .ok_or("private repository has no parent")?
        .join(format!("base-{base}"));
    git_ok(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            path.to_str().ok_or("non-UTF8 base worktree")?,
            base,
        ],
    )?;
    gate.base_worktrees.insert(base.to_owned(), path.clone());
    Ok(path)
}

async fn run_base_guard(
    repo: &Path,
    base: &str,
    spec: &GuardSpec,
    command: &GuardCommand,
    gate: &mut TestGate,
    fingerprint_source: &Path,
) -> Result<BTreeSet<String>, String> {
    let base_worktree = ensure_base_worktree(repo, base, gate)?;
    observed_test_failures(
        &run_shard_or_local(
            &base_worktree,
            fingerprint_source,
            base,
            spec,
            command,
            gate,
        )
        .await?,
    )
}

async fn confirm_local_base_failure(
    repo: &Path,
    base: &str,
    spec: &GuardSpec,
    isolated: &GuardCommand,
    name: &str,
    gate: &mut TestGate,
) -> Result<bool, String> {
    let base_worktree = ensure_base_worktree(repo, base, gate)?;
    let report = run_one_guard(&base_worktree, spec, isolated).await?;
    let output = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
    if output.contains("running 0 tests") {
        return Err(format!("local base isolated test did not run {name}"));
    }
    let failures = observed_test_failures(&report)?;
    if failures.contains(name) {
        if failures.len() != 1 {
            return Err(format!(
                "local base isolated test returned extra failures: {failures:?}"
            ));
        }
        return Ok(true);
    }
    if !failures.is_empty() {
        return Err(format!(
            "local base isolated test returned another failure: {failures:?}"
        ));
    }
    if !output.contains("running 1 test") && !output.contains(&format!("test {name} ... ok")) {
        return Err(format!("local base did not prove isolated test {name} ran"));
    }
    Ok(false)
}

fn command_scratch_path(
    repo: &Path,
    scratch: &GateScratch,
    command: &GuardCommand,
    disk_scratch_shards: &BTreeSet<String>,
    disk_scratch_used: &mut BTreeSet<String>,
) -> Result<PathBuf, String> {
    if command.program != "scripts/run-rsid-test-shards.sh"
        || !command
            .args
            .get(1)
            .is_some_and(|shard| disk_scratch_shards.contains(shard))
    {
        return Ok(scratch.path().to_path_buf());
    }
    let shard = command.args.get(1).ok_or("marked shard has no name")?;
    let path = repo
        .parent()
        .ok_or("private repository has no parent")?
        .join("disk-test-scratch");
    std::fs::create_dir_all(&path)
        .map_err(|error| format!("cannot create marked disk scratch: {error}"))?;
    disk_scratch_used.insert(format!("{shard}:{}", path.display()));
    Ok(path)
}

async fn run_affected_gate(
    repo: &Path,
    candidate_worktree: &Path,
    base: &str,
    spec: &GuardSpec,
    gate: &mut TestGate,
    disk_scratch_shards: &BTreeSet<String>,
) -> Result<(), String> {
    for command in &spec.commands {
        let mut scratch_spec = spec.clone();
        if let Some(scratch) = &gate.scratch {
            let scratch_path = command_scratch_path(
                repo,
                scratch,
                command,
                disk_scratch_shards,
                &mut gate.disk_scratch_used,
            )?;
            scratch_spec
                .env
                .insert("TMPDIR".into(), scratch_path.to_string_lossy().into_owned());
        }
        let is_test = is_test_guard(command);
        if is_test && gate.skip_tests {
            continue;
        }
        if !is_test {
            let report = run_one_guard(candidate_worktree, &scratch_spec, command).await?;
            if report.status != rsid::integration::GuardStatus::Passed {
                return Err(format!(
                    "affected-crate guard failed ({:?}) running {} {:?}: {} {}",
                    report.status,
                    report.program,
                    report.args,
                    report.stdout_tail,
                    report.stderr_tail
                ));
            }
            continue;
        }
        let key = (base.to_owned(), format!("{command:?}"));
        let base_failures = if let Some(cached) = gate.base_cache.get(&key) {
            if let Some((shard, _)) = full_shard(command) {
                gate.base_reused
                    .insert(format!("{base}:{shard}:in-process"));
            }
            cached.clone()
        } else {
            let cache_root = if gate.remote.is_some() {
                None
            } else {
                gate.cache_root.clone()
            };
            let failures = if let (Some(root), Some((shard, jobs))) =
                (cache_root.as_deref(), full_shard(command))
            {
                let fingerprint = shard_fingerprint(candidate_worktree, base, shard, jobs)?;
                let slot =
                    base_cache::BaseShardSlot::acquire(root, base, shard, &fingerprint).await?;
                if let Some(entry) = slot.read()? {
                    gate.base_reused
                        .insert(format!("{base}:{shard}:{}", entry.provenance));
                    if entry.provenance.starts_with("qa:") {
                        gate.qa_cache_provenance
                            .insert(key.clone(), entry.provenance.clone());
                    }
                    entry.failures
                } else {
                    let failures = run_base_guard(
                        repo,
                        base,
                        &scratch_spec,
                        command,
                        gate,
                        candidate_worktree,
                    )
                    .await?;
                    slot.write(&base_cache::BaseShardEntry::new(
                        base,
                        shard,
                        &fingerprint,
                        failures.clone(),
                        "lander",
                    ))?;
                    failures
                }
            } else {
                run_base_guard(repo, base, &scratch_spec, command, gate, candidate_worktree).await?
            };
            gate.base_cache.insert(key.clone(), failures.clone());
            failures
        };
        gate.base_reds.extend(base_failures.iter().cloned());
        let report = run_shard_or_local(
            candidate_worktree,
            candidate_worktree,
            base,
            &scratch_spec,
            command,
            gate,
        )
        .await?;
        let candidate_failures = observed_test_failures(&report)?;
        let new_failures: BTreeSet<_> = candidate_failures
            .difference(&base_failures)
            .cloned()
            .collect();
        for name in &new_failures {
            let isolated = isolated_retry_command(command, name)?;
            let retry = run_one_guard(candidate_worktree, &scratch_spec, &isolated).await?;
            if retry.stdout_tail.contains("running 0 tests")
                || retry.stderr_tail.contains("running 0 tests")
            {
                return Err(format!("isolated retry did not run {name}"));
            }
            let retry_failures = observed_test_failures(&retry)?;
            let persistent: BTreeSet<_> =
                retry_failures.difference(&base_failures).cloned().collect();
            if !persistent.is_empty() {
                if let Some(provenance) = gate.qa_cache_provenance.get(&key).cloned() {
                    if persistent.len() != 1 || !persistent.contains(name) {
                        return Err(format!(
                            "QA cached base {provenance} has unexpected isolated failures: {persistent:?}"
                        ));
                    }
                    match confirm_local_base_failure(
                        repo,
                        base,
                        &scratch_spec,
                        &isolated,
                        name,
                        gate,
                    )
                    .await
                    {
                        Ok(true) => {
                            gate.base_reds.insert(name.clone());
                            gate.local_base_confirmed
                                .insert(format!("{base}:{name}:red:{provenance}"));
                            continue;
                        }
                        Ok(false) => {
                            return Err(format!(
                                "new test failures relative to rolling {base}: {name}; QA cached base {provenance}; isolated local base passed {name}"
                            ));
                        }
                        Err(error) => {
                            return Err(format!(
                                "QA cached base {provenance}; local base confirmation failed closed for {name}: {error}"
                            ));
                        }
                    }
                }
                return Err(format!(
                    "new test failures relative to rolling {base}: {}; base reds: {}",
                    persistent.into_iter().collect::<Vec<_>>().join(", "),
                    base_failures.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            gate.flakes.insert(name.clone());
        }
    }
    Ok(())
}

fn affected_crate_guard_spec(
    target: &str,
    candidate: &str,
    packages: &[String],
    filters: &[String],
    provisional: bool,
    metadata: &WorkspaceMetadata,
    cargo_build_jobs: u8,
) -> Result<GuardSpec, String> {
    let mut selected: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for filter in filters {
        let (package, value) = filter
            .split_once('=')
            .ok_or_else(|| format!("invalid test filter: {filter}"))?;
        if !packages.iter().any(|affected| affected == package)
            || value.is_empty()
            || value.starts_with('-')
        {
            return Err(format!("invalid test filter: {filter}"));
        }
        selected.entry(package).or_default().push(value);
    }
    let mut commands = vec![GuardCommand {
        program: "git".into(),
        args: vec![
            "diff".into(),
            "--check".into(),
            target.into(),
            candidate.into(),
        ],
        timeout: Duration::from_secs(30),
    }];
    if provisional {
        for filter in [
            "store::tests::rewind_tears_down_the_non_idempotent_migration_tail",
            "store::tests::every_recovered_migration_step_actually_executes",
        ] {
            commands.push(rsid_shard_guard_command(&format!(
                "store-01:test({filter})"
            ))?);
        }
    }
    for package in packages {
        if package == "rsid" {
            // A focused shard can be unrelated to the changed tests. Without
            // a proven file-to-shard map, gate all shards for an rsid change.
            for shard in RSID_SHARDS {
                commands.push(rsid_shard_guard_command(shard)?);
            }
        } else {
            commands.push(cargo_guard_command(package, None));
        }
        if let Some(package_filters) = selected.get(package.as_str()) {
            for filter in package_filters {
                let focused = if package == "rsid" {
                    match filter.strip_prefix("shard:") {
                        Some(shard) => rsid_shard_guard_command(shard)?,
                        None => cargo_guard_command(package, Some(filter)),
                    }
                } else {
                    cargo_guard_command(package, Some(filter))
                };
                if !commands.contains(&focused) {
                    commands.push(focused);
                }
            }
        }
    }
    for dependent in reverse_workspace_dependents(metadata, packages)? {
        commands.push(cargo_check_command(&dependent));
    }
    Ok(GuardSpec {
        commands,
        env: BTreeMap::from([
            ("CARGO_BUILD_JOBS".into(), cargo_build_jobs.to_string()),
            ("CARGO_PROFILE_DEV_DEBUG".into(), "line-tables-only".into()),
        ]),
        output_tail_bytes: 8 * 1024 * 1024,
    })
}

fn cargo_check_command(package: &str) -> GuardCommand {
    GuardCommand {
        program: "cargo".into(),
        args: vec![
            "check".into(),
            "-p".into(),
            package.into(),
            "--all-targets".into(),
        ],
        timeout: guard_timeout(),
    }
}

fn cargo_guard_command(package: &str, filter: Option<&str>) -> GuardCommand {
    let mut args = vec!["test".into(), "-p".into(), package.into(), "--lib".into()];
    if let Some(filter) = filter {
        args.push(filter.into());
    }
    args.extend(["--".into(), "--test-threads=4".into()]);
    GuardCommand {
        program: "cargo".into(),
        args,
        timeout: guard_timeout(),
    }
}

fn rsid_shard_guard_command(shard: &str) -> Result<GuardCommand, String> {
    let (shard, filterset) = match shard.split_once(':') {
        Some((shard, filterset)) if !filterset.is_empty() => (shard, Some(filterset)),
        Some(_) => return Err(format!("invalid rsid shard test filter: {shard}")),
        None => (shard, None),
    };
    if !RSID_SHARDS.contains(&shard) {
        return Err(format!("invalid rsid shard test filter: {shard}"));
    }
    let mut args = vec!["shard".into(), shard.into(), "--jobs".into(), "4".into()];
    if let Some(filterset) = filterset {
        let test_name = filterset
            .strip_prefix("test(")
            .and_then(|value| value.strip_suffix(')'))
            .ok_or_else(|| format!("invalid rsid shard test filter: {filterset}"))?;
        if test_name.is_empty()
            || !test_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
        {
            return Err(format!("invalid rsid shard test filter: {filterset}"));
        }
        args.extend(["--filterset".into(), filterset.into()]);
    }
    Ok(GuardCommand {
        program: "scripts/run-rsid-test-shards.sh".into(),
        args,
        timeout: guard_timeout(),
    })
}

async fn forward_revert_after_canary(
    repo: &Path,
    source_repo: &Path,
    options: &Options,
    remote_url: &str,
    candidate: &Candidate,
    fetched_tip: &str,
    canary_error: String,
) -> LandFailure {
    let mut failure = LandFailure::candidate(
        PublicationState::Published,
        format!("published-tip canary failed: {canary_error}"),
        candidate,
        fetched_tip,
    );
    failure.kind = FailureKind::CanaryRed;
    if let Err(error) = verify_remote_binding(source_repo, &options.remote, remote_url) {
        failure.message.push_str("; forward revert blocked: ");
        failure.message.push_str(&error);
        failure.forward_revert_status = Some(PublicationState::NotPublished);
        return failure;
    }
    let tree = match git_text(repo, &["rev-parse", &format!("{fetched_tip}^{{tree}}")]) {
        Ok(tree) => tree,
        Err(error) => {
            failure
                .message
                .push_str("; cannot prepare forward revert: ");
            failure.message.push_str(&error);
            failure.forward_revert_status = Some(PublicationState::NotPublished);
            return failure;
        }
    };
    // A new child of the published candidate restores the prior rolling tree.
    // Accepted source remains in the ancestry even after a red canary.
    let revert = match git_text(
        repo,
        &[
            "-c",
            "user.name=rsi rolling landing",
            "-c",
            "user.email=rsi-rolling-land@rsi.invalid",
            "commit-tree",
            &tree,
            "-p",
            &candidate.oid,
            "-m",
            "Forward revert failed rolling canary",
        ],
    ) {
        Ok(revert) => revert,
        Err(error) => {
            failure
                .message
                .push_str("; cannot prepare forward revert: ");
            failure.message.push_str(&error);
            failure.forward_revert_status = Some(PublicationState::NotPublished);
            return failure;
        }
    };
    failure.forward_revert_id = Some(revert.clone());
    let mut revert_candidate = candidate.clone();
    revert_candidate.oid = revert.clone();
    let publication = publish_candidate(repo, &revert_candidate, &candidate.oid).await;
    let publication = match publication {
        Err(error) if error.state == PublicationState::NotPublished => {
            retry_forward_revert(
                repo,
                source_repo,
                options,
                remote_url,
                candidate,
                fetched_tip,
                error,
            )
            .await
        }
        other => other,
    };
    match publication {
        Ok(observed) => {
            failure.forward_revert_status = Some(PublicationState::Published);
            failure.forward_revert_id = Some(observed.clone());
            failure.observed_tip = Some(observed);
            failure
                .message
                .push_str("; forward revert published and verified");
        }
        Err(error) => {
            failure.forward_revert_status = Some(error.state);
            failure.forward_revert_id = error.candidate;
            failure.observed_tip = error.observed_tip;
            failure.message.push_str("; forward revert not verified: ");
            failure.message.push_str(&error.message);
        }
    }
    failure
}

/// Rebuild the inverse landing delta on a freshly observed descendant. The
/// original pre-landing tree is never substituted for another lead's work.
async fn retry_forward_revert(
    repo: &Path,
    source_repo: &Path,
    options: &Options,
    remote_url: &str,
    original: &Candidate,
    fetched_tip: &str,
    mut stale: LandFailure,
) -> Result<String, LandFailure> {
    for _attempt in 0..2 {
        let Some(observed) = stale.observed_tip.clone() else {
            break;
        };
        if remote_fetch(repo).await.is_err()
            || !git_is_ancestor(repo, &original.oid, &observed).unwrap_or(false)
        {
            break;
        }
        let scratch = match tempfile::Builder::new()
            .prefix("rsi-forward-revert-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(repo.parent().unwrap_or(repo))
        {
            Ok(scratch) => scratch,
            Err(error) => {
                stale.message = format!("cannot create forward-revert scratch: {error}");
                break;
            }
        };
        let tree_path = scratch.path().join("tree");
        let tree_arg = tree_path.to_string_lossy().to_string();
        if let Err(error) = git_ok(
            repo,
            &[
                "worktree", "add", "--detach", "--quiet", &tree_arg, &observed,
            ],
        ) {
            stale.message = format!("cannot prepare forward-revert worktree: {error}");
            break;
        }
        let attempt = prepare_descendant_revert(
            repo,
            &tree_path,
            scratch.path(),
            original,
            fetched_tip,
            &observed,
        );
        let attempt = match attempt {
            Ok(revert) => {
                let mut guard_result = Ok(());
                for pair in &options.accepted {
                    if let Err(error) = run_guard_pair(
                        repo,
                        options,
                        pair,
                        &observed,
                        &revert,
                        Some(&tree_path),
                        true,
                    )
                    .await
                    {
                        guard_result = Err(error);
                        break;
                    }
                }
                guard_result.map(|()| revert)
            }
            Err(error) => Err(error),
        };
        let cleanup = git_ok(repo, &["worktree", "remove", "--force", &tree_arg]);
        let revert = match (attempt, cleanup) {
            (Ok(revert), Ok(())) => revert,
            (Err(error), Ok(())) | (Ok(_), Err(error)) => {
                stale.message = format!("forward revert retry blocked: {error}");
                break;
            }
            (Err(error), Err(cleanup)) => {
                stale.message = format!(
                    "forward revert retry blocked: {error}; worktree cleanup failed: {cleanup}"
                );
                break;
            }
        };
        let mut candidate = original.clone();
        candidate.oid = revert;
        if let Err(error) = verify_remote_binding(source_repo, &options.remote, remote_url) {
            stale.message = format!("forward revert retry blocked by remote rebinding: {error}");
            stale.candidate = Some(candidate.oid);
            break;
        }
        match publish_candidate(repo, &candidate, &observed).await {
            Ok(tip) => return Ok(tip),
            Err(error) if error.state == PublicationState::NotPublished => stale = error,
            Err(error) => return Err(error),
        }
    }
    Err(stale)
}

fn prepare_descendant_revert(
    repo: &Path,
    worktree: &Path,
    scratch: &Path,
    original: &Candidate,
    fetched_tip: &str,
    observed: &str,
) -> Result<String, String> {
    let patch = git_output(
        repo,
        &["diff", "--binary", &original.oid, fetched_tip, "--"],
    )?;
    if !patch.status.success() {
        return Err("cannot compute inverse landing delta".into());
    }
    let patch_path = scratch.join("inverse.patch");
    std::fs::write(&patch_path, patch.stdout)
        .map_err(|error| format!("cannot record inverse landing delta: {error}"))?;
    git_ok(
        worktree,
        &["apply", "--3way", "--index", &patch_path.to_string_lossy()],
    )?;
    let tree = git_text(worktree, &["write-tree"])?;
    let revert = git_text(
        repo,
        &[
            "-c",
            "user.name=rsi rolling landing",
            "-c",
            "user.email=rsi-rolling-land@rsi.invalid",
            "commit-tree",
            &tree,
            "-p",
            observed,
            "-m",
            "Forward revert failed rolling canary on advanced target",
        ],
    )?;
    git_ok(worktree, &["reset", "--hard", &revert])?;
    Ok(revert)
}

async fn publish_candidate(
    repo: &Path,
    candidate: &Candidate,
    fetched_tip: &str,
) -> Result<String, LandFailure> {
    let observed = remote_tip(repo, "publish").await.map_err(|error| {
        LandFailure::candidate(
            PublicationState::NotPublished,
            error,
            candidate,
            fetched_tip,
        )
    })?;
    if observed.as_deref() != Some(fetched_tip) {
        let mut failure = LandFailure::candidate(
            PublicationState::NotPublished,
            format!(
                "stale target before push: fetched {fetched_tip}, remote now resolves to {observed:?}"
            ),
            candidate,
            fetched_tip,
        );
        failure.observed_tip = observed;
        return Err(failure);
    }

    // A normal push has Git's default fast-forward-only behavior. The full
    // candidate OID is the source, so no branch name or force refspec is used.
    let refspec = format!("{}:refs/heads/rolling", candidate.oid);
    let mut push = remote_git_command(repo, &["push", "--porcelain", "publish", &refspec]);
    push.env("RSI_ROLLING_LANDER", "1");
    push.stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut push = push.spawn().map_err(|error| {
        LandFailure::candidate(
            PublicationState::NotPublished,
            format!("cannot push candidate to remote: {error}"),
            candidate,
            fetched_tip,
        )
    })?;
    let push_pid = child_pid(&push, "push candidate to remote").map_err(|error| {
        LandFailure::candidate(PublicationState::Unknown, error, candidate, fetched_tip)
    })?;
    let push_result = tokio::time::timeout(remote_push_timeout(), push.wait()).await;
    let (context, reported_success) = match push_result {
        Ok(Ok(status)) if status.success() => ("push reported success".to_string(), true),
        Ok(Ok(status)) => (format!("push exited {}", status_code(status)), false),
        Ok(Err(error)) => {
            terminate_and_reap(&mut push, push_pid).await;
            (format!("cannot wait for candidate push: {error}"), false)
        }
        Err(_) => {
            terminate_and_reap(&mut push, push_pid).await;
            ("fast-forward-only push timed out".to_string(), false)
        }
    };
    confirm_post_push(repo, candidate, fetched_tip, &context, reported_success).await
}

async fn confirm_post_push(
    repo: &Path,
    candidate: &Candidate,
    fetched_tip: &str,
    context: &str,
    reported_success: bool,
) -> Result<String, LandFailure> {
    match remote_tip(repo, "publish").await {
        Ok(Some(tip)) if tip == candidate.oid => Ok(tip),
        Ok(Some(tip)) if tip == fetched_tip && !reported_success => Err(LandFailure::candidate(
            PublicationState::NotPublished,
            format!("{context}; remote rolling remained at {fetched_tip}"),
            candidate,
            fetched_tip,
        )),
        Ok(Some(tip)) if !reported_success && tip != fetched_tip => {
            if remote_fetch(repo).await.is_ok()
                && git_text(repo, &["rev-parse", "refs/heads/rolling"])
                    .ok()
                    .as_deref()
                    == Some(tip.as_str())
                && git_is_ancestor(repo, fetched_tip, &tip).unwrap_or(false)
                && !git_is_ancestor(repo, &candidate.oid, &tip).unwrap_or(true)
            {
                let mut failure = LandFailure::candidate(
                    PublicationState::NotPublished,
                    format!("{context}; stale target advanced to {tip} before publication"),
                    candidate,
                    fetched_tip,
                );
                failure.observed_tip = Some(tip);
                return Err(failure);
            }
            let mut failure = LandFailure::candidate(
                PublicationState::Unknown,
                format!("{context}; remote rolling advanced to {tip}, publication is unconfirmed"),
                candidate,
                fetched_tip,
            );
            failure.observed_tip = Some(tip);
            Err(failure)
        }
        Ok(observed) => {
            let mut failure = LandFailure::candidate(
                PublicationState::Unknown,
                format!(
                    "{context}; remote rolling resolves to {observed:?}, publication is unconfirmed"
                ),
                candidate,
                fetched_tip,
            );
            failure.observed_tip = observed;
            Err(failure)
        }
        Err(error) => Err(LandFailure::candidate(
            PublicationState::Unknown,
            format!("{context}; remote verification failed: {error}"),
            candidate,
            fetched_tip,
        )),
    }
}

async fn remote_fetch(repo: &Path) -> Result<(), String> {
    let mut command = remote_git_command(
        repo,
        &[
            "fetch",
            "--no-tags",
            "--no-auto-maintenance",
            "publish",
            "refs/heads/rolling:refs/remotes/publish/rolling",
        ],
    );
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot fetch remote rolling: {error}"))?;
    let pid = child_pid(&child, "remote rolling fetch")?;
    match tokio::time::timeout(remote_lookup_timeout(), child.wait()).await {
        Ok(Ok(status)) if status.success() => {
            let tip = git_text(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "refs/remotes/publish/rolling^{commit}",
                ],
            )?;
            git_ok(repo, &["update-ref", TARGET, &tip])
        }
        Ok(Ok(status)) => Err(format!(
            "remote rolling fetch failed (exit {})",
            status_code(status)
        )),
        Ok(Err(error)) => {
            terminate_and_reap(&mut child, pid).await;
            Err(format!("cannot wait for remote rolling fetch: {error}"))
        }
        Err(_) => {
            terminate_and_reap(&mut child, pid).await;
            Err(format!(
                "remote rolling fetch timed out after {:?}",
                remote_lookup_timeout()
            ))
        }
    }
}

async fn remote_tip(repo: &Path, remote: &str) -> Result<Option<String>, String> {
    const MAX_REMOTE_RESPONSE: usize = 1024;
    let mut command = remote_git_command(repo, &["ls-remote", remote, "refs/heads/rolling"]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot read remote rolling tip: {error}"))?;
    let pid = child_pid(&child, "remote rolling lookup")?;
    let Some(stdout) = child.stdout.take() else {
        terminate_and_reap(&mut child, pid).await;
        return Err("cannot read remote rolling tip: stdout unavailable".to_string());
    };
    let mut bytes = Vec::new();
    let deadline = tokio::time::Instant::now() + remote_lookup_timeout();
    let read_result = tokio::time::timeout_at(
        deadline,
        stdout
            .take((MAX_REMOTE_RESPONSE + 1) as u64)
            .read_to_end(&mut bytes),
    )
    .await;
    match read_result {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            terminate_and_reap(&mut child, pid).await;
            return Err(format!("cannot read remote rolling tip: {error}"));
        }
        Err(_) => {
            terminate_and_reap(&mut child, pid).await;
            return Err(format!(
                "remote rolling lookup timed out after {:?}",
                remote_lookup_timeout()
            ));
        }
    }
    if bytes.len() > MAX_REMOTE_RESPONSE {
        terminate_and_reap(&mut child, pid).await;
        return Err("remote rolling lookup exceeded the 1024-byte response limit".into());
    }
    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            terminate_and_reap(&mut child, pid).await;
            return Err(format!("cannot wait for remote rolling lookup: {error}"));
        }
        Err(_) => {
            terminate_and_reap(&mut child, pid).await;
            return Err(format!(
                "remote rolling lookup timed out after {:?}",
                remote_lookup_timeout()
            ));
        }
    };
    if !status.success() {
        return Err(format!(
            "cannot read remote rolling tip: git ls-remote exited {}",
            status_code(status)
        ));
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| "remote rolling lookup returned invalid UTF-8".to_string())?;
    let Some(line) = value.lines().next() else {
        return Ok(None);
    };
    if value.lines().nth(1).is_some() {
        return Err("remote rolling lookup returned more than one ref line".into());
    }
    let mut fields = line.split_whitespace();
    let oid = fields
        .next()
        .ok_or_else(|| "remote rolling lookup returned a malformed ref line".to_string())?;
    let reference = fields
        .next()
        .ok_or_else(|| "remote rolling lookup returned a malformed ref line".to_string())?;
    if fields.next().is_some()
        || !matches!(oid.len(), 40 | 64)
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || reference != "refs/heads/rolling"
    {
        return Err("remote rolling lookup returned a malformed ref line".into());
    }
    Ok(Some(oid.to_string()))
}

fn remote_git_command(repo: &Path, args: &[&str]) -> TokioCommand {
    let mut command = TokioCommand::new("git");
    command
        .current_dir(repo)
        .args([
            "--no-optional-locks",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-c",
            "fetch.writeCommitGraph=false",
        ])
        .args(args);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") || key == "RSI_ROLLING_LANDER" {
            command.env_remove(key);
        }
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.as_std_mut().process_group(0);
    command.kill_on_drop(true);
    command
}

fn child_pid(child: &tokio::process::Child, operation: &str) -> Result<nix::unistd::Pid, String> {
    child
        .id()
        .map(|pid| nix::unistd::Pid::from_raw(pid.cast_signed()))
        .ok_or_else(|| format!("cannot identify {operation} process"))
}

async fn terminate_and_reap(child: &mut tokio::process::Child, process_group: nix::unistd::Pid) {
    use nix::sys::signal::{Signal, kill, killpg};

    let _ = killpg(process_group, Signal::SIGKILL);
    let reaped = tokio::time::timeout(remote_cleanup_timeout(), child.wait()).await;
    if !matches!(reaped, Ok(Ok(_))) {
        let _ = kill(process_group, Signal::SIGKILL);
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

const fn remote_lookup_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_secs(2)
    }
    #[cfg(not(test))]
    {
        REMOTE_LOOKUP_TIMEOUT
    }
}

const fn remote_push_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_secs(2)
    }
    #[cfg(not(test))]
    {
        REMOTE_PUSH_TIMEOUT
    }
}

const fn remote_cleanup_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_secs(1)
    }
    #[cfg(not(test))]
    {
        REMOTE_CHILD_CLEANUP_TIMEOUT
    }
}

fn status_code(status: ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string())
}

fn git_text(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output(repo, args)?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or("?"),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_ok(repo: &Path, args: &[&str]) -> Result<(), String> {
    git_text(repo, args).map(|_| ())
}

fn git_is_ancestor(repo: &Path, older: &str, newer: &str) -> Result<bool, String> {
    let output = git_output(repo, &["merge-base", "--is-ancestor", older, newer])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("cannot verify observed remote ancestry".into()),
    }
}

fn git_output(repo: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .args([
            "--no-optional-locks",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-c",
            "fetch.writeCommitGraph=false",
            "-C",
        ])
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|error| format!("cannot start git: {error}"))
}

fn copy_committed_script(repo: &Path, relative_path: &str) -> Result<(), String> {
    let script = git_output(repo, &["show", &format!("HEAD:{relative_path}")])?;
    if !script.status.success() {
        return Err(format!("committed {relative_path} is missing"));
    }
    let path = repo.join(relative_path);
    let parent = path.parent().ok_or("committed script path has no parent")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create private script directory: {error}"))?;
    std::fs::write(path, script.stdout)
        .map_err(|error| format!("cannot copy committed {relative_path}: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::await_holding_lock,
        clippy::expect_used,
        clippy::similar_names,
        clippy::significant_drop_tightening,
        clippy::unwrap_used,
        clippy::used_underscore_binding
    )]

    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn guard_timeout_defaults_to_twenty_minutes_and_honours_bounded_override() {
        assert_eq!(guard_timeout_from(None), Duration::from_secs(20 * 60));
        assert_eq!(
            guard_timeout_from(Some("not-a-number")),
            Duration::from_secs(20 * 60)
        );
        assert_eq!(guard_timeout_from(Some("3600")), Duration::from_secs(3600));
        assert_eq!(
            guard_timeout_from(Some(" 5400 ")),
            Duration::from_secs(5400)
        );
        assert_eq!(guard_timeout_from(Some("5")), Duration::from_secs(60));
        assert_eq!(
            guard_timeout_from(Some("999999")),
            Duration::from_secs(4 * 60 * 60)
        );
        let shard = rsid_shard_guard_command("session-01").expect("known shard");
        assert!(shard.timeout >= Duration::from_secs(60));
    }

    fn guard_workspace_metadata() -> WorkspaceMetadata {
        fn package(name: &str, dependencies: &[&str]) -> WorkspacePackage {
            WorkspacePackage {
                id: name.into(),
                name: name.into(),
                dependencies: dependencies
                    .iter()
                    .map(|dependency| WorkspaceDependency {
                        name: (*dependency).into(),
                        path: Some(PathBuf::from(format!("crates/{dependency}"))),
                    })
                    .collect(),
            }
        }
        WorkspaceMetadata {
            workspace_members: ["rsi-common", "rsi-graph", "rsi", "rsid", "rsi-baseline"]
                .map(str::to_owned)
                .to_vec(),
            packages: vec![
                package("rsi-common", &[]),
                package("rsi-graph", &["rsi-common"]),
                package("rsi", &["rsi-graph"]),
                package("rsid", &["rsi-common", "rsi-graph"]),
                package("rsi-baseline", &[]),
            ],
        }
    }

    struct Fixture {
        _environment_lock: std::sync::MutexGuard<'static, ()>,
        root: TempDir,
        repo: PathBuf,
        bare: PathBuf,
        base: String,
        bin: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            Self::from_root(tempfile::tempdir().expect("temp root"))
        }

        fn new_in(parent: &Path) -> Self {
            Self::from_root(tempfile::tempdir_in(parent).expect("temp root in Cargo target"))
        }

        fn from_root(root: TempDir) -> Self {
            let environment_lock = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let repo = root.path().join("repo");
            let bare = root.path().join("remote.git");
            let bin = root.path().join("bin");
            fs::create_dir_all(&repo).expect("repository directory");
            fs::create_dir_all(&bin).expect("bin directory");
            git_run(&repo, &["init", "-q", "-b", "rolling"]);
            write(
                &repo,
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/demo\"]\nresolver = \"2\"\n",
            );
            write(
                &repo,
                "crates/demo/Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            );
            write(
                &repo,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"base\" }\n",
            );
            let source_guard = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/rolling-landing-guard.py");
            fs::create_dir_all(repo.join("scripts")).expect("scripts dir");
            fs::copy(source_guard, repo.join("scripts/rolling-landing-guard.py"))
                .expect("copy current guard");
            write(
                &repo,
                "scripts/rolling-shard-fingerprint.py",
                "print('sha256:' + 'a' * 64)\n",
            );
            write(
                &repo,
                "scripts/run-rsid-test-shards.sh",
                "#!/bin/sh\noid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> \"$RSI_QA899_RUN_LOG\"\nif [ \"$oid\" = \"$RSI_QA899_FAIL_ON_OID\" ]; then echo 'test demo::red ... FAILED'; exit 1; fi\nexit 0\n",
            );
            let mut shard_permissions = fs::metadata(repo.join("scripts/run-rsid-test-shards.sh"))
                .expect("shard script metadata")
                .permissions();
            shard_permissions.set_mode(0o755);
            fs::set_permissions(
                repo.join("scripts/run-rsid-test-shards.sh"),
                shard_permissions,
            )
            .expect("executable shard script");
            fs::create_dir_all(repo.join("tools")).expect("tools dir");
            fs::copy(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tools/check-released-migrations.py"),
                repo.join("tools/check-released-migrations.py"),
            )
            .expect("copy released guard");
            write(
                &repo,
                "tools/released-migrations.json",
                "{\"migration_file\":\"crates/rsid/src/store/mod.rs\",\"protected_sections\":{}}\n",
            );
            git_run(&repo, &["add", "."]);
            git_run(&repo, &["commit", "-q", "-m", "base"]);
            let base = git_value(&repo, &["rev-parse", "HEAD"]);
            git_run(
                root.path(),
                &["init", "--bare", "-q", bare.to_str().unwrap()],
            );
            git_run(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
            git_run(&repo, &["push", "-q", "origin", "rolling"]);
            Self {
                _environment_lock: environment_lock,
                root,
                repo,
                bare,
                base,
                bin,
            }
        }

        fn commit(&self, parent: &str, file: &str, content: &str, message: &str) -> String {
            let path = self
                .root
                .path()
                .join(format!("builder-{}", uuid::Uuid::new_v4()));
            git_run(
                &self.repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    path.to_str().unwrap(),
                    parent,
                ],
            );
            write(&path, file, content);
            git_run(&path, &["add", file]);
            git_run(&path, &["commit", "-q", "-m", message]);
            let oid = git_value(&path, &["rev-parse", "HEAD"]);
            git_run(&self.repo, &["worktree", "remove", path.to_str().unwrap()]);
            oid
        }

        fn add_fake_cargo(&self, extra: &str) {
            let metadata = serde_json::json!({
                "workspace_members": ["demo"],
                "packages": [{ "id": "demo", "name": "demo", "dependencies": [] }],
            });
            let script = format!(
                "#!/bin/sh\nif [ \"$1\" = metadata ]; then\n  printf '%s\\n' '{metadata}'\n  exit 0\nfi\n{extra}\nexit 0\n"
            );
            let cargo = self.bin.join("cargo");
            fs::write(&cargo, script).expect("fake cargo");
            let mut permissions = fs::metadata(&cargo).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(cargo, permissions).expect("chmod fake cargo");
        }

        fn add_fake_canary_cargo(&self, prepublish_tip: &str, action: &str) {
            self.add_fake_cargo(&format!(
                "remote=$(/usr/bin/git --git-dir='{}' rev-parse refs/heads/rolling)\nif [ \"$remote\" != '{}' ]; then\n{}\nfi",
                self.bare.display(), prepublish_tip, action,
            ));
        }

        fn add_fake_git(&self, ls_remote: &str) {
            self.add_fake_git_behaviors(ls_remote, "");
        }

        fn add_fake_git_behaviors(&self, ls_remote: &str, push: &str) {
            self.add_fake_git_behaviors_with_fetch(ls_remote, push, "");
        }

        fn add_fake_git_behaviors_with_fetch(&self, ls_remote: &str, push: &str, fetch: &str) {
            let ls_remote_case = if ls_remote.is_empty() {
                String::new()
            } else {
                format!("  if [ \"$arg\" = ls-remote ]; then\n{ls_remote}\n    exit 0\n  fi\n")
            };
            let push_case = if push.is_empty() {
                String::new()
            } else {
                format!("  if [ \"$arg\" = push ]; then\n{push}\n    exit 0\n  fi\n")
            };
            let fetch_case = if fetch.is_empty() {
                String::new()
            } else {
                format!("  if [ \"$arg\" = fetch ]; then\n{fetch}\n    exit 0\n  fi\n")
            };
            let script = format!(
                "#!/bin/sh\nfor arg in \"$@\"; do\n{ls_remote_case}{push_case}{fetch_case}done\nexec /usr/bin/git \"$@\"\n"
            );
            let git = self.bin.join("git");
            fs::write(&git, script).expect("fake git");
            let mut permissions = fs::metadata(&git).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(git, permissions).expect("chmod fake git");
        }

        fn add_fake_cargo_log(&self) -> PathBuf {
            let args = self.root.path().join("cargo-args.log");
            let env = self.root.path().join("cargo-env.log");
            // Match the metadata projection consumed by WorkspaceMetadata for
            // Fixture::new's single demo package, which has no dependencies.
            let metadata = serde_json::json!({
                "workspace_members": ["demo"],
                "packages": [{ "id": "demo", "name": "demo", "dependencies": [] }],
            });
            let script = format!(
                r#"#!/bin/sh
printf '%s\n' "$@" >> '{args}'
if [ "$1" = metadata ]; then
    printf '%s\n' '{metadata}'
    exit 0
fi
printf '%s\n%s\n%s\n%s\n%s\n' "$CARGO_TARGET_DIR" "$CARGO_BUILD_JOBS" "$CARGO_PROFILE_DEV_DEBUG" "$(pwd -P)" "$TMPDIR" >> '{env}'
exit 0
"#,
                args = args.display(),
                env = env.display(),
            );
            let cargo = self.bin.join("cargo");
            fs::write(&cargo, script).expect("fake cargo with invocation log");
            let mut permissions = fs::metadata(&cargo).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(cargo, permissions).expect("chmod fake cargo");
            args
        }

        async fn land(&self, accepted: Vec<AcceptedPair>) -> Result<LandReport, LandFailure> {
            self.land_with_filters(accepted, Vec::new()).await
        }

        // Models the canary branch of a disjoint stale retry: its remade
        // candidate was published without running its own pre-push tests.
        async fn land_with_ungated_candidate(
            &self,
            accepted: Vec<AcceptedPair>,
        ) -> Result<LandReport, LandFailure> {
            land_with_gate(
                Options {
                    repo: self.repo.clone(),
                    remote: "origin".into(),
                    accepted,
                    test_filters: Vec::new(),
                    cargo_build_jobs: 1,
                    remote_gate: None,
                    tmpfs_min_free_gb: 12,
                    disk_scratch_shards: BTreeSet::new(),
                },
                TestGate {
                    skip_tests: true,
                    ..TestGate::default()
                },
            )
            .await
        }

        async fn land_with_filters(
            &self,
            accepted: Vec<AcceptedPair>,
            test_filters: Vec<String>,
        ) -> Result<LandReport, LandFailure> {
            land(Options {
                repo: self.repo.clone(),
                remote: "origin".into(),
                accepted,
                test_filters,
                cargo_build_jobs: 1,
                remote_gate: None,
                tmpfs_min_free_gb: 12,
                disk_scratch_shards: BTreeSet::new(),
            })
            .await
        }
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).expect("parent directory");
        fs::write(path, contents).expect("fixture file");
    }

    fn git_run(cwd: &Path, args: &[&str]) {
        let output = Command::new("/usr/bin/git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("HOME", "/nonexistent")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .output()
            .expect("git launch");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_value(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("/usr/bin/git")
            .current_dir(cwd)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git launch");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn assert_source_clean(repo: &Path) {
        assert_eq!(git_value(repo, &["status", "--porcelain=v1"]), "");
    }

    struct ScopedCargoTarget(Option<std::ffi::OsString>);

    impl ScopedCargoTarget {
        fn set(path: &Path) -> Self {
            let old = std::env::var_os("CARGO_TARGET_DIR");
            // Fixture tests serialize environment changes through ENV_LOCK.
            unsafe { std::env::set_var("CARGO_TARGET_DIR", path) };
            Self(old)
        }
    }

    impl Drop for ScopedCargoTarget {
        fn drop(&mut self) {
            // Restore before releasing Fixture's ENV_LOCK.
            unsafe {
                if let Some(old) = &self.0 {
                    std::env::set_var("CARGO_TARGET_DIR", old);
                } else {
                    std::env::remove_var("CARGO_TARGET_DIR");
                }
            }
        }
    }

    fn use_fake_path(fixture: &Fixture) -> Option<std::ffi::OsString> {
        let old = std::env::var_os("PATH");
        let path = format!(
            "{}:{}",
            fixture.bin.display(),
            old.as_deref().unwrap_or_default().to_string_lossy()
        );
        // Tests serialize all PATH changes through ENV_LOCK.
        unsafe { std::env::set_var("PATH", path) };
        old
    }

    fn restore_path(old: Option<std::ffi::OsString>) {
        // Tests serialize all PATH changes through ENV_LOCK.
        unsafe {
            if let Some(old) = old {
                std::env::set_var("PATH", old);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    fn policy_work(source: &str, domain: &str, version: u32) -> serde_json::Value {
        serde_json::json!({
            "key": "landing-work",
            "epic_id": "epic-j",
            "source_commit": source,
            "source_accepted": true,
            "ownership": [{
                "work_key": "landing-work",
                "active": true,
                "mode": "exclusive",
                "domain": domain,
                "files": [domain]
            }],
            "migration_reservations": [{
                "work_key": "landing-work",
                "version": version,
                "row_version": 1
            }]
        })
    }

    #[tokio::test]
    async fn paired_test_gate_handles_identical_new_fixed_and_flaky_reds() {
        for scenario in ["identical", "new", "fixed", "flaky"] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"candidate\" }\n",
                "candidate",
            );
            let candidate_tree = fixture.root.path().join("candidate-gate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    candidate_tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            let log = fixture.root.path().join("gate-runs");
            let target_log = fixture.root.path().join("gate-targets");
            let marker = fixture.root.path().join("flaky-first-run");
            let base_fails = matches!(scenario, "identical" | "fixed");
            let candidate_fails = matches!(scenario, "identical" | "new" | "flaky");
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\nprintf '%s\\t%s\\n' \"$oid\" \"$CARGO_TARGET_DIR\" >> '{target_log}'\nif [ \"$oid\" = '{base}' ] && [ '{base_fails}' = true ]; then echo 'test demo::red ... FAILED'; exit 1; fi\nif [ \"$oid\" = '{candidate}' ] && [ '{candidate_fails}' = true ]; then\n  if [ '{scenario}' = flaky ]; then\n    if [ ! -e '{marker}' ]; then : > '{marker}'; echo 'test demo::red ... FAILED'; exit 1; fi\n  else echo 'test demo::red ... FAILED'; exit 1; fi\nfi",
                log = log.display(), target_log = target_log.display(), base = fixture.base, candidate = candidate,
                marker = marker.display(),
            ));
            let spec = GuardSpec {
                commands: vec![cargo_guard_command("demo", None)],
                env: BTreeMap::new(),
                output_tail_bytes: 16 * 1024,
            };
            let mut gate = TestGate::default();
            let old_path = use_fake_path(&fixture);
            let result = run_affected_gate(
                &fixture.repo,
                &candidate_tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            restore_path(old_path);
            let lines = fs::read_to_string(log).unwrap();
            let candidate_runs = lines.lines().filter(|oid| *oid == candidate).count();
            match scenario {
                "identical" => {
                    result.expect("identical baseline red is allowed");
                    assert!(gate.base_reds.contains("demo::red"));
                    assert_eq!(candidate_runs, 1);
                }
                "new" => {
                    assert!(
                        result
                            .unwrap_err()
                            .contains("new test failures relative to rolling")
                    );
                    assert_eq!(candidate_runs, 2);
                }
                "fixed" => {
                    result.expect("fixed baseline red is allowed");
                    assert!(gate.base_reds.contains("demo::red"));
                    assert_eq!(candidate_runs, 1);
                }
                "flaky" => {
                    result.expect("isolated retry classifies a flake");
                    assert!(gate.flakes.contains("demo::red"));
                    assert_eq!(candidate_runs, 2);
                }
                _ => unreachable!(),
            }
            assert_eq!(lines.lines().filter(|oid| *oid == fixture.base).count(), 1);
            let targets = fs::read_to_string(target_log).unwrap();
            let base_target = targets
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{}\t", fixture.base)))
                .unwrap();
            let candidate_target = targets
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{candidate}\t")))
                .unwrap();
            assert_ne!(base_target, candidate_target);
            assert!(base_target.starts_with(fixture.root.path().to_str().unwrap()));
            assert!(candidate_target.starts_with(fixture.root.path().to_str().unwrap()));
        }
    }

    #[tokio::test]
    async fn persistent_base_shard_cache_reuses_lander_and_qa_results() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let candidate_tree = fixture.root.path().join("cache-candidate");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                candidate_tree.to_str().unwrap(),
                &candidate,
            ],
        );
        let log = fixture.root.path().join("shard-runs.log");
        let cache_root = fixture.root.path().join("shared-cache");
        let spec = GuardSpec {
            commands: vec![rsid_shard_guard_command("store-01").unwrap()],
            env: BTreeMap::from([
                ("RSI_QA899_RUN_LOG".into(), log.display().to_string()),
                ("RSI_QA899_FAIL_ON_OID".into(), "none".into()),
            ]),
            output_tail_bytes: 16 * 1024,
        };
        let mut first = TestGate {
            cache_root: Some(cache_root.clone()),
            ..TestGate::default()
        };
        run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &spec,
            &mut first,
            &BTreeSet::new(),
        )
        .await
        .expect("initial base and candidate run");
        let first_log = fs::read_to_string(&log).unwrap();
        assert_eq!(
            first_log.lines().filter(|oid| *oid == fixture.base).count(),
            1
        );
        let mut reused = TestGate {
            cache_root: Some(cache_root.clone()),
            ..TestGate::default()
        };
        run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &spec,
            &mut reused,
            &BTreeSet::new(),
        )
        .await
        .expect("reuse cached base");
        let second_log = fs::read_to_string(&log).unwrap();
        assert_eq!(
            second_log
                .lines()
                .filter(|oid| *oid == fixture.base)
                .count(),
            1
        );
        assert!(
            reused
                .base_reused
                .contains(&format!("{}:store-01:lander", fixture.base))
        );

        let fingerprint = "sha256:".to_owned() + &"a".repeat(64);
        let qa_root = fixture.root.path().join("qa-cache");
        let slot =
            base_cache::BaseShardSlot::acquire(&qa_root, &fixture.base, "store-01", &fingerprint)
                .await
                .unwrap();
        slot.write(&base_cache::BaseShardEntry::new(
            &fixture.base,
            "store-01",
            &fingerprint,
            BTreeSet::new(),
            "qa:QA.md",
        ))
        .unwrap();
        drop(slot);
        let mut qa_reused = TestGate {
            cache_root: Some(qa_root),
            ..TestGate::default()
        };
        run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &spec,
            &mut qa_reused,
            &BTreeSet::new(),
        )
        .await
        .expect("reuse QA base result");
        assert!(
            qa_reused
                .base_reused
                .contains(&format!("{}:store-01:qa:QA.md", fixture.base))
        );

        let mut candidate_red_spec = spec.clone();
        candidate_red_spec
            .env
            .insert("RSI_QA899_FAIL_ON_OID".into(), candidate.clone());
        fixture.add_fake_cargo(
            "if [ \"$1\" = test ]; then echo 'test demo::red ... FAILED'; exit 1; fi",
        );
        let mut candidate_red = TestGate {
            cache_root: Some(fixture.root.path().join("shared-cache")),
            ..TestGate::default()
        };
        let old_path = use_fake_path(&fixture);
        let result = run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &candidate_red_spec,
            &mut candidate_red,
            &BTreeSet::new(),
        )
        .await;
        restore_path(old_path);
        let error = result.expect_err("cached base must not waive a candidate regression");
        assert!(
            error.contains("new test failures relative to rolling"),
            "{error}"
        );
        assert!(
            candidate_red
                .base_reused
                .contains(&format!("{}:store-01:lander", fixture.base))
        );

        let base_tree = first.base_worktrees.get(&fixture.base).unwrap();
        git_run(
            &fixture.repo,
            &["worktree", "remove", base_tree.to_str().unwrap()],
        );
        let mismatch = fixture.commit(
            &candidate,
            "scripts/rolling-shard-fingerprint.py",
            "print('sha256:' + 'b' * 64)\n",
            "different toolchain fingerprint",
        );
        let mismatch_tree = fixture.root.path().join("cache-mismatch");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                mismatch_tree.to_str().unwrap(),
                &mismatch,
            ],
        );
        let mut different = TestGate {
            cache_root: Some(cache_root),
            ..TestGate::default()
        };
        run_affected_gate(
            &fixture.repo,
            &mismatch_tree,
            &fixture.base,
            &spec,
            &mut different,
            &BTreeSet::new(),
        )
        .await
        .expect("different fingerprint recomputes base");
        let final_log = fs::read_to_string(log).unwrap();
        assert_eq!(
            final_log.lines().filter(|oid| *oid == fixture.base).count(),
            2
        );
    }

    #[tokio::test]
    async fn qa_cached_base_confirms_candidate_red_on_local_base() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let candidate_tree = fixture.root.path().join("qa-candidate");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                candidate_tree.to_str().unwrap(),
                &candidate,
            ],
        );
        let cache_root = fixture.root.path().join("qa-cache");
        let fingerprint = "sha256:".to_owned() + &"a".repeat(64);
        let slot = base_cache::BaseShardSlot::acquire(
            &cache_root,
            &fixture.base,
            "store-01",
            &fingerprint,
        )
        .await
        .unwrap();
        slot.write(&base_cache::BaseShardEntry::new(
            &fixture.base,
            "store-01",
            &fingerprint,
            BTreeSet::new(),
            "qa:same-host:QA.json",
        ))
        .unwrap();
        drop(slot);
        fixture.add_fake_cargo(
            "if [ \"$1\" = test ]; then
  oid=$(/usr/bin/git rev-parse HEAD)
  printf '%s\\n' \"$oid\" >> \"$RSI_QA_LOCAL_RUN_LOG\"
  if [ \"$oid\" = \"$RSI_QA_BASE_OID\" ]; then
    case \"$RSI_QA_CONFIRM_MODE\" in
      red) echo 'test demo::red ... FAILED'; exit 1;;
      green) echo 'running 1 test'; echo 'test demo::red ... ok'; exit 0;;
      broken) echo 'local executor failed' >&2; exit 1;;
    esac
  fi
  echo 'test demo::red ... FAILED'; exit 1
fi",
        );
        let shard_log = fixture.root.path().join("qa-shard-runs.log");
        let local_log = fixture.root.path().join("qa-local-runs.log");
        let mut spec = GuardSpec {
            commands: vec![rsid_shard_guard_command("store-01").unwrap()],
            env: BTreeMap::from([
                ("RSI_QA899_RUN_LOG".into(), shard_log.display().to_string()),
                ("RSI_QA899_FAIL_ON_OID".into(), candidate.clone()),
                ("RSI_QA_BASE_OID".into(), fixture.base.clone()),
                (
                    "RSI_QA_LOCAL_RUN_LOG".into(),
                    local_log.display().to_string(),
                ),
            ]),
            output_tail_bytes: 16 * 1024,
        };
        let mut gate = TestGate {
            cache_root: Some(cache_root.clone()),
            ..TestGate::default()
        };
        let old_path = use_fake_path(&fixture);
        for mode in ["red", "green", "broken"] {
            spec.env.insert("RSI_QA_CONFIRM_MODE".into(), mode.into());
            let result = run_affected_gate(
                &fixture.repo,
                &candidate_tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            match mode {
                "red" => {
                    result.expect("local base red waives a QA cache false refusal");
                    assert!(gate.base_reds.contains("demo::red"));
                    assert!(gate.local_base_confirmed.contains(&format!(
                        "{}:demo::red:red:qa:same-host:QA.json",
                        fixture.base
                    )));
                }
                "green" => {
                    let error = result.expect_err("local base green keeps candidate refusal");
                    assert!(
                        error.contains("isolated local base passed demo::red"),
                        "{error}"
                    );
                    assert!(error.contains("qa:same-host:QA.json"), "{error}");
                }
                "broken" => {
                    let error = result.expect_err("local base execution failure fails closed");
                    assert!(
                        error.contains("local base confirmation failed closed"),
                        "{error}"
                    );
                    assert!(error.contains("local executor failed"), "{error}");
                }
                _ => unreachable!(),
            }
        }
        restore_path(old_path);
        assert_eq!(
            fs::read_to_string(&shard_log)
                .unwrap()
                .lines()
                .filter(|oid| *oid == candidate)
                .count(),
            3
        );
        assert_eq!(fs::read_to_string(local_log).unwrap().lines().count(), 6);
        let slot = base_cache::BaseShardSlot::acquire(
            &cache_root,
            &fixture.base,
            "store-01",
            &fingerprint,
        )
        .await
        .unwrap();
        assert!(slot.read().unwrap().unwrap().failures.is_empty());
    }

    #[tokio::test]
    async fn qa_cache_from_another_host_class_is_not_reused() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "scripts/rolling-shard-fingerprint.py",
            "import os\nclass_name = os.environ.get('RSI_QA_HOST_CLASS')\nprint('sha256:' + ('a' if class_name == 'desktop' else 'b') * 64)\n",
            "fingerprint by host class",
        );
        let candidate_tree = fixture.root.path().join("class-candidate");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                candidate_tree.to_str().unwrap(),
                &candidate,
            ],
        );
        let cache_root = fixture.root.path().join("qa-cache");
        let desktop_fingerprint = "sha256:".to_owned() + &"a".repeat(64);
        let slot = base_cache::BaseShardSlot::acquire(
            &cache_root,
            &fixture.base,
            "store-01",
            &desktop_fingerprint,
        )
        .await
        .unwrap();
        slot.write(&base_cache::BaseShardEntry::new(
            &fixture.base,
            "store-01",
            &desktop_fingerprint,
            BTreeSet::new(),
            "qa:desktop:QA.json",
        ))
        .unwrap();
        drop(slot);
        let log = fixture.root.path().join("class-shard-runs.log");
        let spec = GuardSpec {
            commands: vec![rsid_shard_guard_command("store-01").unwrap()],
            env: BTreeMap::from([
                ("RSI_QA899_RUN_LOG".into(), log.display().to_string()),
                ("RSI_QA899_FAIL_ON_OID".into(), "none".into()),
            ]),
            output_tail_bytes: 16 * 1024,
        };
        let previous = std::env::var_os("RSI_QA_HOST_CLASS");
        unsafe { std::env::set_var("RSI_QA_HOST_CLASS", "desktop") };
        let mut desktop = TestGate {
            cache_root: Some(cache_root.clone()),
            ..TestGate::default()
        };
        let first = run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &spec,
            &mut desktop,
            &BTreeSet::new(),
        )
        .await;
        unsafe { std::env::set_var("RSI_QA_HOST_CLASS", "cloud") };
        let mut cloud = TestGate {
            cache_root: Some(cache_root),
            ..TestGate::default()
        };
        let second = run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &spec,
            &mut cloud,
            &BTreeSet::new(),
        )
        .await;
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("RSI_QA_HOST_CLASS", previous);
            } else {
                std::env::remove_var("RSI_QA_HOST_CLASS");
            }
        }
        first.expect("same host class may reuse exact QA base");
        second.expect("different host class recomputes base");
        assert!(
            desktop
                .base_reused
                .contains(&format!("{}:store-01:qa:desktop:QA.json", fixture.base))
        );
        assert!(cloud.base_reused.is_empty());
        assert_eq!(
            fs::read_to_string(log)
                .unwrap()
                .lines()
                .filter(|oid| *oid == fixture.base)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn unrelated_green_filter_cannot_bypass_affected_tests() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"regression\" }\n",
            "new regression",
        );
        fixture.add_fake_cargo(&format!(
            "if [ \"$1\" = test ] && [ \"$(/usr/bin/git rev-parse HEAD)\" = '{source}' ]; then\n  case \"$5\" in --|demo::regression) echo 'test demo::regression ... FAILED'; exit 1;; esac\nfi"
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_filters(
                vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source,
                }],
                vec!["demo=unrelated_green_test".into()],
            )
            .await;
        restore_path(old_path);
        let failure = result.expect_err("unrelated filter must not hide a new red");
        assert_eq!(failure.state, PublicationState::NotPublished);
        assert!(failure.message.contains("demo::regression"), "{failure:?}");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
    }

    #[test]
    fn nextest_failure_names_are_compared_by_test_identity() {
        let output = "    FAIL [  0.123s] ( 76/338) rsid store::tests::baseline_red\n    FAIL [  0.456s] (139/338) rsid store::tests::new_red\n";
        assert_eq!(
            test_failure_names(output),
            BTreeSet::from([
                "store::tests::baseline_red".to_string(),
                "store::tests::new_red".to_string(),
            ])
        );
    }

    #[test]
    fn shard_retry_targets_only_the_candidate_failure() {
        let command = rsid_shard_guard_command("store-01:test(existing_filter)").unwrap();
        let retry = isolated_retry_command(&command, "store::tests::new_red").unwrap();
        assert_eq!(retry.program, "cargo");
        assert_eq!(
            retry.args,
            [
                "test",
                "-p",
                "rsid",
                "--lib",
                "--no-default-features",
                "--features",
                "test-shard-store-01",
                "store::tests::new_red",
                "--",
                "--exact",
                "--test-threads=4",
            ]
        );
    }

    #[tokio::test]
    async fn stale_target_reuses_gate_for_disjoint_paths_and_regates_overlap() {
        for overlapping in [false, true] {
            let fixture = Fixture::new();
            let (base, source, incoming) = if overlapping {
                let original = (1..=20).fold(String::new(), |mut lines, line| {
                    let _ = writeln!(lines, "line {line}");
                    lines
                });
                let setup = fixture.commit(&fixture.base, "race.txt", &original, "race setup");
                git_run(
                    &fixture.repo,
                    &[
                        "push",
                        "-q",
                        "origin",
                        &format!("{setup}:refs/heads/rolling"),
                    ],
                );
                let first = fixture.commit(
                    &setup,
                    "crates/demo/src/lib.rs",
                    "pub fn value() -> &'static str { \"source\" }\n",
                    "source crate",
                );
                let source = fixture.commit(
                    &first,
                    "race.txt",
                    &original.replace("line 2\n", "source line 2\n"),
                    "source race file",
                );
                let incoming = fixture.commit(
                    &setup,
                    "race.txt",
                    &original.replace("line 18\n", "incoming line 18\n"),
                    "incoming race file",
                );
                (setup, source, incoming)
            } else {
                let source = fixture.commit(
                    &fixture.base,
                    "crates/demo/src/lib.rs",
                    "pub fn value() -> &'static str { \"source\" }\n",
                    "source crate",
                );
                let incoming = fixture.commit(
                    &fixture.base,
                    "incoming.txt",
                    "incoming\n",
                    "incoming disjoint file",
                );
                (fixture.base.clone(), source, incoming)
            };
            let marker = fixture.root.path().join("advanced-once");
            let log = fixture.root.path().join("tested-commits");
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\nif [ \"$oid\" = '{source}' ] && [ ! -e '{marker}' ]; then\n  : > '{marker}'\n  /usr/bin/git -C '{repo}' push -q origin '{incoming}:refs/heads/rolling' || exit 43\nfi",
                log = log.display(), marker = marker.display(), repo = fixture.repo.display(),
            ));
            let old_path = use_fake_path(&fixture);
            let result = fixture
                .land(vec![AcceptedPair {
                    base: base.clone(),
                    source: source.clone(),
                }])
                .await;
            restore_path(old_path);
            let report = result.expect("stale candidate remade and published");
            assert_eq!(report.fetched_tip, incoming);
            assert_eq!(
                git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
                report.published_tip
            );
            let tested = fs::read_to_string(log).unwrap();
            assert_eq!(tested.lines().filter(|oid| *oid == source).count(), 1);
            let remade_runs = tested
                .lines()
                .filter(|oid| *oid == report.candidate)
                .count();
            assert_eq!(remade_runs, 1);
            assert_eq!(
                report.canary_reused_tree.is_some(),
                overlapping,
                "a disjoint stale retry must execute the canary, while a gated retry reuses it"
            );
        }
    }

    // Stages chained rolling advances on the remote without moving rolling.
    fn stage_advances(
        fixture: &Fixture,
        parent: &str,
        contents: &[(String, String)],
    ) -> Vec<String> {
        let mut parent = parent.to_string();
        let mut advances = Vec::new();
        for (index, (file, content)) in contents.iter().enumerate() {
            let next = fixture.commit(&parent, file, content, "incoming advance");
            git_run(
                &fixture.repo,
                &[
                    "push",
                    "-q",
                    "origin",
                    &format!("{next}:refs/heads/race-{index}"),
                ],
            );
            advances.push(next.clone());
            parent = next;
        }
        advances
    }

    fn expected_stale_history(
        start: &str,
        advances: &[String],
        reused_gate: bool,
    ) -> Vec<StaleRetry> {
        let mut fetched = start.to_string();
        advances
            .iter()
            .map(|observed| StaleRetry {
                fetched: std::mem::replace(&mut fetched, observed.clone()),
                observed: observed.clone(),
                reused_gate,
            })
            .collect()
    }

    #[tokio::test]
    async fn consecutive_disjoint_stale_advances_publish_without_regating() {
        // Issue #952: landers publishing seconds apart. One advance lands
        // during the gate; each later one wins the race against our push,
        // which the remote rejects (`cannot lock ref`). Two push races
        // exhausted the former two-retry budget and lost a 93-minute gate.
        for push_races in [1_usize, 2] {
            let fixture = Fixture::new();
            let source = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"source\" }\n",
                "source crate",
            );
            let contents = (0..=push_races)
                .map(|index| (format!("incoming-{index}.txt"), "incoming\n".to_string()))
                .collect::<Vec<_>>();
            let advances = stage_advances(&fixture, &fixture.base, &contents);
            let marker = fixture.root.path().join("advanced-once");
            let log = fixture.root.path().join("tested-commits");
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\nif [ \"$oid\" = '{source}' ] && [ ! -e '{marker}' ]; then\n  : > '{marker}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling '{first}' || exit 43\nfi",
                log = log.display(),
                marker = marker.display(),
                bare = fixture.bare.display(),
                first = advances[0],
            ));
            let races = fixture.root.path().join("push-races");
            fs::write(&races, format!("{}\n", advances[1..].join("\n"))).unwrap();
            fixture.add_fake_git_behaviors(
                "",
                &format!(
                    "next=$(head -n 1 '{races}')\nif [ -n \"$next\" ]; then\n  sed -i 1d '{races}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling \"$next\" || exit 44\n  echo 'remote rejected: cannot lock ref refs/heads/rolling' >&2\n  exit 1\nfi\nexec /usr/bin/git \"$@\"",
                    races = races.display(),
                    bare = fixture.bare.display(),
                ),
            );
            let old_path = use_fake_path(&fixture);
            let result = fixture
                .land(vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source: source.clone(),
                }])
                .await;
            restore_path(old_path);
            let report = result.expect("every disjoint stale advance is retried to publication");
            let last = advances.last().unwrap();
            assert_eq!(&report.fetched_tip, last);
            assert_eq!(
                git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
                report.published_tip
            );
            assert_eq!(
                report.stale_retries,
                expected_stale_history(&fixture.base, &advances, true)
            );
            let receipt = stale_retry_lines(&report.stale_retries);
            assert_eq!(receipt[0], format!("stale_attempts={}", push_races + 1));
            assert_eq!(
                receipt[1],
                format!(
                    "stale_retry_1={}..{}:gate_reused",
                    fixture.base, advances[0]
                )
            );
            let tested = fs::read_to_string(&log).unwrap();
            assert_eq!(tested.lines().filter(|oid| *oid == source).count(), 1);
            assert_eq!(
                tested
                    .lines()
                    .filter(|oid| *oid == report.published_tip)
                    .count(),
                1,
                "the published tip runs its canary once"
            );
            // Base-side runs test rolling tips; only the intermediate remade
            // candidates, which were never published, must stay ungated.
            assert!(
                tested.lines().all(|oid| oid == source
                    || oid == report.published_tip
                    || oid == fixture.base
                    || advances.iter().any(|advance| advance == oid)),
                "no remade candidate is re-gated: {tested}"
            );
        }
    }

    #[tokio::test]
    async fn overlapping_stale_advances_stop_at_the_regated_budget_with_history() {
        let fixture = Fixture::new();
        let original = (1..=20).fold(String::new(), |mut lines, line| {
            let _ = writeln!(lines, "line {line}");
            lines
        });
        let setup = fixture.commit(&fixture.base, "race.txt", &original, "race setup");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{setup}:refs/heads/rolling"),
            ],
        );
        let first = fixture.commit(
            &setup,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"source\" }\n",
            "source crate",
        );
        let source = fixture.commit(
            &first,
            "race.txt",
            &original.replace("line 2\n", "source line 2\n"),
            "source race file",
        );
        let mut edited = original.clone();
        let contents = [8, 13, 18]
            .iter()
            .map(|line| {
                edited = edited.replace(
                    &format!("line {line}\n"),
                    &format!("incoming line {line}\n"),
                );
                ("race.txt".to_string(), edited.clone())
            })
            .collect::<Vec<_>>();
        let advances = stage_advances(&fixture, &setup, &contents);
        let pending = fixture.root.path().join("pending-advances");
        fs::write(&pending, format!("{}\n", advances.join("\n"))).unwrap();
        // Rolling tips run only as base-side comparisons; mark them seen.
        let seen = fixture.root.path().join("gated-commits");
        fs::write(&seen, format!("{setup}\n{}\n", advances.join("\n"))).unwrap();
        // Every newly gated candidate loses the race to the next advance.
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\nnext=$(head -n 1 '{pending}')\nif [ -n \"$next\" ] && ! grep -qx \"$oid\" '{seen}'; then\n  printf '%s\\n' \"$oid\" >> '{seen}'\n  sed -i 1d '{pending}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling \"$next\" || exit 43\nfi",
            pending = pending.display(),
            seen = seen.display(),
            bare = fixture.bare.display(),
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: setup.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("a third overlapping advance exceeds the re-gated budget");
        assert_eq!(failure.state, PublicationState::NotPublished, "{failure:?}");
        assert!(
            failure.message.contains("stale retries exhausted"),
            "{failure:?}"
        );
        assert_eq!(
            failure.observed_tip.as_deref(),
            advances.last().map(String::as_str)
        );
        assert_eq!(
            failure.stale_retries,
            expected_stale_history(&setup, &advances[..2], false)
        );
        let evidence = failure.evidence_lines();
        assert!(
            evidence.contains(&"stale_attempts=2".to_string()),
            "{evidence:?}"
        );
        assert!(
            evidence.contains(&format!(
                "stale_retry_2={}..{}:regated",
                advances[0], advances[1]
            )),
            "{evidence:?}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            *advances.last().unwrap()
        );
    }

    fn provisional_stale_fixture(
        fixture: &Fixture,
        change: &str,
    ) -> (PathBuf, PathBuf, String, String, String) {
        let repo = fixture.root.path().join(format!("migration-{change}"));
        let bare = fixture.root.path().join(format!("migration-{change}.git"));
        let setup = r#"
import importlib.util, json, pathlib, shutil, subprocess, sys
root, destination, bare = map(pathlib.Path, sys.argv[1:4])
change = sys.argv[4]
spec = importlib.util.spec_from_file_location('migration_test', root / 'scripts/tests/test_rolling_migration_renumber.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
case = module.ProvisionalMigrationTest('test_two_v130_sources_land_as_v130_v131_with_exact_parents_and_proofs')
case.setUp()
repo = case.repo
for name in ('scripts/rolling-landing-guard.py', 'tools/check-released-migrations.py', 'tools/rolling-migration-renumber.py'):
    case.write(repo, name, (root / name).read_text())
case.write(repo, 'Cargo.toml', '[workspace]\nmembers = ["crates/rsid"]\nresolver = "2"\n')
protected_path = 'crates/rsid/src/store/protected_fixture.rs'
protected_text = '// RSI-RELEASED-MIGRATION-BEGIN: fixture-section\n// pinned\n// RSI-RELEASED-MIGRATION-END: fixture-section\n'
case.write(repo, protected_path, protected_text)
inventory = module.RENUMBER.guard.inventory({
    module.RENUMBER.STORE: case.base_store(),
    'crates/rsid/src/store/cohort_settlement.rs': '',
    'crates/rsid/src/store/tests.rs': case.base_tests(),
    protected_path: protected_text,
})
case.write(repo, module.RENUMBER.MANIFEST, json.dumps(inventory, indent=2) + '\n')
case.write(repo, 'scripts/run-rsid-test-shards.sh', '''#!/bin/sh
oid=$(/usr/bin/git rev-parse HEAD)
printf '%s\\n' "$oid" >> "$LANDER924_LOG"
if [ "$oid" != "$LANDER924_BASE" ] && [ ! -e "$LANDER924_MARKER" ]; then
  : > "$LANDER924_MARKER"
  if [ "$LANDER924_CHANGE" = proof ]; then
    private=$(/usr/bin/git rev-parse --git-common-dir)
    printf '%s\\n' 'raise SystemExit(43)' > "${private%/.git}/tools/rolling-migration-renumber.py"
  fi
  /usr/bin/git -C "$LANDER924_REPO" push -q origin "$LANDER924_INCOMING:refs/heads/rolling" || exit 43
fi
exit 0
''')
(repo / 'scripts/run-rsid-test-shards.sh').chmod(0o755)
case.git(repo, 'add', '.')
case.git(repo, 'commit', '-q', '-m', 'landing support')
case.base = case.git(repo, 'rev-parse', 'HEAD')
source = case.source('alpha')
case.git(repo, 'update-ref', 'refs/heads/source-for-landing', source)
if change == 'version':
    second = case.source('beta')
    _, incoming = case.candidate(second, case.base)
else:
    worktree = pathlib.Path(case.temp.name) / 'incoming'
    case.git(repo, 'worktree', 'add', '-q', '--detach', str(worktree), case.base)
    if change == 'store':
        filename = 'crates/rsid/src/store/mod.rs'
        (worktree / filename).chmod(0o755)
    elif change == 'catalog':
        filename = 'tools/released-migrations.json'
        (worktree / filename).chmod(0o755)
    elif change in ('renumber_tool', 'released_tool', 'guard_tool', 'cohort', 'protected'):
        filename = {
            'renumber_tool': 'tools/rolling-migration-renumber.py',
            'released_tool': 'tools/check-released-migrations.py',
            'guard_tool': 'scripts/rolling-landing-guard.py',
            'cohort': 'crates/rsid/src/store/cohort_settlement.rs',
            'protected': protected_path,
        }[change]
        comment = '# incoming\n' if change.endswith('tool') else '// incoming\n'
        case.write(worktree, filename, (worktree / filename).read_text() + comment)
    else:
        filename = 'incoming.txt'
        case.write(worktree, filename, 'incoming\n')
    case.git(worktree, 'add', filename)
    case.git(worktree, 'commit', '-q', '-m', 'incoming')
    incoming = case.git(worktree, 'rev-parse', 'HEAD')
case.git(repo, 'update-ref', 'refs/heads/incoming-for-landing', incoming)
subprocess.run(['git', 'init', '--bare', '-q', str(bare)], check=True)
case.git(repo, 'push', '-q', str(bare), f'{case.base}:refs/heads/rolling')
subprocess.run(['git', 'clone', '-q', str(repo), str(destination)], check=True)
case.git(destination, 'remote', 'set-url', 'origin', str(bare))
print(json.dumps({'base': case.base, 'source': source, 'incoming': incoming}))
"#;
        let output = Command::new("python3")
            .arg("-c")
            .arg(setup)
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .arg(&repo)
            .arg(&bare)
            .arg(change)
            .output()
            .expect("migration fixture script");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let details: Value = serde_json::from_slice(&output.stdout).expect("fixture details");
        (
            repo,
            bare,
            details["base"].as_str().unwrap().into(),
            details["source"].as_str().unwrap().into(),
            details["incoming"].as_str().unwrap().into(),
        )
    }

    #[tokio::test]
    async fn provisional_stale_retry_regenerates_or_refuses_proof() {
        for change in [
            "disjoint",
            "store",
            "catalog",
            "renumber_tool",
            "released_tool",
            "guard_tool",
            "cohort",
            "protected",
            "version",
            "proof",
        ] {
            let fixture = Fixture::new();
            let (repo, bare, base, source, incoming) = provisional_stale_fixture(&fixture, change);
            let work_rows = fixture.root.path().join("work-rows.json");
            fs::write(
                &work_rows,
                serde_json::json!([{
                    "key": "test-migration", "epic_id": "test-epic",
                    "source_commit": source, "source_accepted": true,
                    "migration_reservations": [{
                        "version": 130, "active": true,
                        "work_key": "test-migration", "row_version": 1
                    }]
                }])
                .to_string(),
            )
            .unwrap();
            unsafe { std::env::set_var("RSI_LANDER_TEST_WORK_ROWS", &work_rows) };
            // The source holds the one live V130 seal with no lower predecessor,
            // so the gate never reaches the caller's live migration ledger.
            let allocation_rows = fixture.root.path().join("allocation-rows.json");
            fs::write(
                &allocation_rows,
                serde_json::json!([{
                    "source_commit": source, "version": 130,
                    "state": "active", "predecessor_sources": []
                }])
                .to_string(),
            )
            .unwrap();
            unsafe { std::env::set_var("RSI_LANDER_TEST_ALLOCATION_ROWS", &allocation_rows) };
            let marker = fixture.root.path().join("advanced-once");
            let log = fixture.root.path().join("tested-commits");
            let metadata = serde_json::json!({
                "workspace_members": ["rsid"],
                "packages": [{"id":"rsid","name":"rsid","dependencies":[]}],
            });
            let cargo = fixture.bin.join("cargo");
            fs::write(&cargo, format!(
                "#!/bin/sh\nif [ \"$1\" = metadata ]; then printf '%s\\n' '{metadata}'; exit 0; fi\nexit 0\n",
            )).unwrap();
            let mut permissions = fs::metadata(&cargo).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&cargo, permissions).unwrap();
            for (key, value) in [
                ("LANDER924_LOG", log.to_string_lossy().into_owned()),
                ("LANDER924_MARKER", marker.to_string_lossy().into_owned()),
                ("LANDER924_BASE", base.clone()),
                ("LANDER924_REPO", repo.to_string_lossy().into_owned()),
                ("LANDER924_INCOMING", incoming.clone()),
                ("LANDER924_CHANGE", change.to_owned()),
            ] {
                unsafe { std::env::set_var(key, value) };
            }
            let old_path = use_fake_path(&fixture);
            let result = land(Options {
                repo: repo.clone(),
                remote: "origin".into(),
                accepted: vec![AcceptedPair {
                    base: base.clone(),
                    source,
                }],
                test_filters: Vec::new(),
                cargo_build_jobs: 1,
                remote_gate: None,
                tmpfs_min_free_gb: 12,
                disk_scratch_shards: BTreeSet::new(),
            })
            .await;
            for key in [
                "LANDER924_LOG",
                "LANDER924_MARKER",
                "LANDER924_BASE",
                "LANDER924_REPO",
                "LANDER924_INCOMING",
                "LANDER924_CHANGE",
            ] {
                unsafe { std::env::remove_var(key) };
            }
            unsafe { std::env::remove_var("RSI_LANDER_TEST_WORK_ROWS") };
            unsafe { std::env::remove_var("RSI_LANDER_TEST_ALLOCATION_ROWS") };
            restore_path(old_path);
            if change == "version" || change == "proof" {
                let failure = result.expect_err("stale proof must refuse");
                assert_eq!(failure.state, PublicationState::NotPublished, "{failure:?}");
                assert!(
                    failure.message.contains(if change == "version" {
                        "version changed"
                    } else {
                        "provisional migration refused"
                    }),
                    "{failure:?}"
                );
                assert_eq!(
                    git_value(&bare, &["rev-parse", "refs/heads/rolling"]),
                    incoming
                );
            } else {
                let report = result.expect("stale provisional migration publishes");
                assert_eq!(report.fetched_tip, incoming);
                assert_eq!(report.provisional.len(), 1);
                assert_eq!(report.provisional[0].proof["target"], incoming);
                assert_eq!(report.canary_reused_tree.is_some(), change != "disjoint");
                assert_eq!(
                    git_value(&bare, &["rev-parse", "refs/heads/rolling"]),
                    report.published_tip
                );
            }
        }
    }

    #[test]
    fn committed_released_guard_replaces_dirty_worktree_copy() {
        let fixture = Fixture::new();
        let script = "tools/check-released-migrations.py";
        let committed = git_output(&fixture.repo, &["show", &format!("HEAD:{script}")])
            .expect("committed script");
        write(&fixture.repo, script, "raise SystemExit(0)\n");
        copy_committed_script(&fixture.repo, script).expect("restore committed script");
        assert_eq!(
            fs::read(fixture.repo.join(script)).unwrap(),
            committed.stdout
        );
    }

    #[test]
    fn hermetic_hot_file_allows_unbound_source_unless_another_work_claims_it() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "AGENTS.md", "policy\n", "hot file");
        let accepted = [AcceptedPair {
            base: fixture.base.clone(),
            source: source.clone(),
        }];
        let check = |rows| {
            landing_policy::check_with_work(
                &fixture.repo,
                &fixture.repo,
                &fixture.base,
                &source,
                &accepted,
                || Ok(rows),
            )
        };
        check(vec![policy_work(&source, "AGENTS.md", 125)]).expect("live claim passes");
        check(vec![]).expect("an unbound Tier-0/1 source can publish without a claim");
        let wrong_source = fixture.base.clone();
        assert!(
            check(vec![policy_work(&wrong_source, "AGENTS.md", 125)])
                .unwrap_err()
                .message
                .contains("claimed by another live unintegrated Work")
        );
        check(vec![policy_work(&source, "other.md", 125)])
            .expect("a claim on another path does not block this landing");
    }

    #[test]
    fn hermetic_migration_order_requires_each_new_version_reservation() {
        let fixture = Fixture::new();
        let store = "crates/rsid/src/store/mod.rs";
        let v124 = fixture.commit(
            &fixture.base,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 124;\n",
            "schema 124",
        );
        let v126 = fixture.commit(
            &v124,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 126;\n",
            "schema 126",
        );
        // The released-migration script has its own fixture suite. This fixture
        // isolates the landing policy's comparison and reservation sequence.
        write(
            fixture.root.path(),
            "tools/check-released-migrations.py",
            "import sys\nsys.exit(0)\n",
        );
        let accepted = [AcceptedPair {
            base: v124.clone(),
            source: v126.clone(),
        }];
        let check = |rows| {
            landing_policy::check_with_work(
                &fixture.repo,
                fixture.root.path(),
                &v124,
                &v126,
                &accepted,
                || Ok(rows),
            )
        };
        let mut row = policy_work(&v126, store, 126);
        assert!(
            check(vec![row.clone()])
                .unwrap_err()
                .message
                .contains("V125")
        );
        row["migration_reservations"] = serde_json::json!([
            {"work_key":"landing-work","version":125,"row_version":1},
            {"work_key":"landing-work","version":126,"row_version":1}
        ]);
        check(vec![row]).expect("contiguous new versions are reserved");
        landing_policy::check_with_work_and_proof(
            &fixture.repo,
            fixture.root.path(),
            &v124,
            &v126,
            &accepted,
            &[125, 126],
            || Ok(vec![policy_work(&v126, store, 999)]),
        )
        .expect("proved migration versions need no pre-landing reservation");
        let error = landing_policy::check_with_work_and_proof(
            &fixture.repo,
            fixture.root.path(),
            &v124,
            &v126,
            &accepted,
            &[127],
            || Ok(vec![policy_work(&v126, store, 999)]),
        )
        .expect_err("proof outside the appended range must refuse");
        assert_eq!(error.fence, landing_policy::PolicyFence::SchemaVersion);
    }

    #[test]
    fn cargo_build_jobs_defaults_to_four_and_accepts_positive_bounded_values() {
        assert_eq!(parse_cargo_build_jobs(None).unwrap(), 4);
        for jobs in 1..=6 {
            let value = jobs.to_string();
            assert_eq!(
                parse_cargo_build_jobs(Some(std::ffi::OsStr::new(&value))).unwrap(),
                jobs
            );
        }
    }

    #[test]
    fn cargo_build_jobs_rejects_invalid_values_clearly() {
        use std::os::unix::ffi::OsStrExt;

        for value in [
            b"".as_slice(),
            b"0",
            b"-1",
            b"+1",
            b"7",
            b"256",
            b"999999999999999999999999999999",
            b"1.0",
            b"auto",
            b" 1",
            b"1 ",
            b"\xff",
        ] {
            assert_eq!(
                parse_cargo_build_jobs(Some(std::ffi::OsStr::from_bytes(value))).unwrap_err(),
                "CARGO_BUILD_JOBS must be a positive integer from 1 to 6 (default: 4)"
            );
        }
    }

    #[test]
    fn cargo_build_jobs_one_preserves_all_guard_commands() {
        let metadata = guard_workspace_metadata();
        for (package, provisional) in [("rsid", false), ("rsi-common", false), ("rsid", true)] {
            let packages = [package.to_string()];
            let filters = [format!("{package}=focused_test")];
            let spec = |jobs| {
                affected_crate_guard_spec(
                    "1111111111111111111111111111111111111111",
                    "2222222222222222222222222222222222222222",
                    &packages,
                    &filters,
                    provisional,
                    &metadata,
                    jobs,
                )
                .unwrap()
            };
            let default = spec(parse_cargo_build_jobs(None).unwrap());
            let one = spec(parse_cargo_build_jobs(Some(std::ffi::OsStr::new("1"))).unwrap());
            let mut expected_env = default.env.clone();
            expected_env.insert("CARGO_BUILD_JOBS".into(), "1".into());
            assert_eq!(one.env, expected_env);
            assert_eq!(one.output_tail_bytes, default.output_tail_bytes);
            assert_eq!(one.commands.len(), default.commands.len());
            for (actual, expected) in one.commands.iter().zip(&default.commands) {
                assert_eq!(actual.program, expected.program);
                assert_eq!(actual.args, expected.args);
                assert_eq!(actual.timeout, expected.timeout);
            }
        }
    }

    #[test]
    fn rsid_shard_filter_uses_bounded_harness() {
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsid".into()],
            &["rsid=shard:session-01".into()],
            false,
            &guard_workspace_metadata(),
            4,
        )
        .unwrap();
        assert_eq!(spec.commands.len(), 1 + RSID_SHARDS.len());
        assert!(spec.commands.iter().any(|command| {
            command.program == "scripts/run-rsid-test-shards.sh"
                && command.args == ["shard", "session-01", "--jobs", "4"]
        }));
        assert!(
            spec.commands
                .iter()
                .any(|command| command.args == ["shard", "store-04", "--jobs", "4"])
        );
        assert!(rsid_shard_guard_command("unknown").is_err());
        assert!(rsid_shard_guard_command("session-02:").is_err());
        assert!(rsid_shard_guard_command("session-02:test()").is_err());
        assert!(rsid_shard_guard_command("session-02:test(").is_err());
        assert!(rsid_shard_guard_command("session-02:not(test(gap))").is_err());
        let focused = rsid_shard_guard_command("session-02:test(manager_recovery_)").unwrap();
        assert_eq!(
            focused.args,
            [
                "shard",
                "session-02",
                "--jobs",
                "4",
                "--filterset",
                "test(manager_recovery_)"
            ]
        );
    }

    #[test]
    fn rsid_shard_filter_runner_accepts_lander_arguments() {
        let guard = rsid_shard_guard_command("session-03:test(event_wake)").unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let output = Command::new(&guard.program)
            .args(&guard.args)
            .arg("--dry-run")
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan = String::from_utf8(output.stdout).unwrap();
        assert!(plan.contains("--runtime-shard session-03"));
        assert!(plan.contains("--filterset test\\(event_wake\\)"));
    }

    #[test]
    fn provisional_guard_keeps_rewind_and_replay_tests_with_user_filter() {
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsid".into()],
            &["rsid=topology_v129".into()],
            true,
            &guard_workspace_metadata(),
            4,
        )
        .expect("valid affected crate filter");
        assert_eq!(
            spec.env.get("CARGO_BUILD_JOBS").map(String::as_str),
            Some("4")
        );
        for command in spec
            .commands
            .iter()
            .filter(|command| command.program == "cargo")
        {
            assert_eq!(
                command.args[command.args.len() - 2..],
                ["--", "--test-threads=4"]
            );
        }
        let commands = spec
            .commands
            .iter()
            .map(|command| command.args.join(" "))
            .collect::<Vec<_>>();
        assert!(commands.iter().any(|command| {
            command.contains("rewind_tears_down_the_non_idempotent_migration_tail")
        }));
        assert!(commands.iter().any(|command| command.contains("every_recovered_migration_step_actually_executes")));
        assert!(
            commands
                .iter()
                .any(|command| command.contains("topology_v129"))
        );
    }

    #[test]
    fn changed_library_checks_transitive_workspace_dependents() {
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsi-common".into()],
            &["rsi-common=manager_operator_delegation".into()],
            false,
            &guard_workspace_metadata(),
            4,
        )
        .expect("valid workspace graph");
        let cargo_commands = spec
            .commands
            .iter()
            .filter(|command| command.program == "cargo")
            .map(|command| command.args.join(" "))
            .collect::<Vec<_>>();
        assert_eq!(
            cargo_commands,
            [
                "test -p rsi-common --lib -- --test-threads=4",
                "test -p rsi-common --lib manager_operator_delegation -- --test-threads=4",
                "check -p rsi --all-targets",
                "check -p rsi-graph --all-targets",
                "check -p rsid --all-targets",
            ]
        );

        let direct = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsid".into()],
            &[],
            false,
            &guard_workspace_metadata(),
            4,
        )
        .expect("valid single-crate change");
        let shard_commands = direct
            .commands
            .iter()
            .filter(|command| command.program == "scripts/run-rsid-test-shards.sh")
            .collect::<Vec<_>>();
        assert_eq!(shard_commands.len(), RSID_SHARDS.len());
        for (command, shard) in shard_commands.iter().zip(RSID_SHARDS) {
            assert_eq!(command.args, ["shard", *shard, "--jobs", "4"]);
        }
    }

    #[test]
    fn released_guard_runs_for_pinned_file_without_store_mod_change() {
        let fixture = Fixture::new();
        let protected = "crates/rsid/src/store/manager_ledger/facts.rs";
        let manifest = format!(
            "{{\"migration_file\":\"crates/rsid/src/store/mod.rs\",\"protected_sections\":{{\"facts\":{{\"path\":\"{protected}\"}}}}}}\n"
        );
        let base = fixture.commit(
            &fixture.base,
            "tools/released-migrations.json",
            &manifest,
            "pin facts",
        );
        let source = fixture.commit(&base, protected, "edited\n", "change pinned facts");
        write(
            fixture.root.path(),
            "tools/check-released-migrations.py",
            "import sys\nsys.exit(2)\n",
        );
        let error = landing_policy::check_with_work(
            &fixture.repo,
            fixture.root.path(),
            &base,
            &source,
            &[AcceptedPair {
                base: base.clone(),
                source: source.clone(),
            }],
            || Ok(vec![]),
        )
        .unwrap_err();
        assert_eq!(error.fence, landing_policy::PolicyFence::ReleasedMigration);
        assert!(error.message.contains("released-migration guard refused"));
    }

    #[test]
    fn lowered_schema_version_has_stable_policy_fence() {
        let fixture = Fixture::new();
        let store = "crates/rsid/src/store/mod.rs";
        let base = fixture.commit(
            &fixture.base,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 126;\n",
            "schema 126",
        );
        let source = fixture.commit(
            &base,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 125;\n",
            "lower schema",
        );
        write(
            fixture.root.path(),
            "tools/check-released-migrations.py",
            "import sys\nsys.exit(0)\n",
        );
        let error = landing_policy::check_with_work(
            &fixture.repo,
            fixture.root.path(),
            &base,
            &source,
            &[AcceptedPair {
                base: base.clone(),
                source: source.clone(),
            }],
            || Ok(vec![]),
        )
        .unwrap_err();
        assert_eq!(error.fence, landing_policy::PolicyFence::SchemaVersion);
    }

    #[tokio::test]
    async fn hot_file_without_live_ledger_refuses_before_push() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "AGENTS.md", "policy\n", "hot file");
        fixture.add_fake_cargo("");
        let old_token = std::env::var_os("RSI_SESSION_TOKEN");
        unsafe { std::env::remove_var("RSI_SESSION_TOKEN") };
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        if let Some(token) = old_token {
            unsafe { std::env::set_var("RSI_SESSION_TOKEN", token) };
        }
        let refusal = result.expect_err("missing ledger access must refuse");
        assert_eq!(refusal.state, PublicationState::NotPublished);
        assert_eq!(refusal.exit_code(), 8);
        assert!(
            refusal
                .evidence_lines()
                .contains(&"landing_outcome=policy_refused".into())
        );
        assert!(
            refusal
                .evidence_lines()
                .contains(&"policy_fence=ledger_unavailable".into())
        );
        assert!(
            refusal
                .message
                .contains("requires an rsi-managed Epic lead")
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        assert_eq!(
            git_value(&fixture.repo, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
    }

    #[tokio::test]
    async fn fast_forward_candidate_pushes_exact_source_oid() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        fixture.add_fake_cargo("");
        let old_path = use_fake_path(&fixture);
        let old_git_dir = std::env::var_os("GIT_DIR");
        unsafe {
            std::env::set_var("GIT_DIR", fixture.root.path().join("not-a-git-dir"));
        }
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base[..12].to_string(),
                source: source[..12].to_string(),
            }])
            .await;
        restore_path(old_path);
        unsafe {
            if let Some(old_git_dir) = old_git_dir {
                std::env::set_var("GIT_DIR", old_git_dir);
            } else {
                std::env::remove_var("GIT_DIR");
            }
        }
        let report = result.expect("landing succeeds");
        assert_eq!(report.candidate, source);
        assert_eq!(report.kind, CandidateKind::FastForward);
        assert_eq!(
            git_value(&fixture.repo, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
        );
    }

    #[tokio::test]
    async fn derived_base_allows_landing_after_rolling_regenerates_merged_file() {
        let fixture = Fixture::new();
        let feature = fixture.commit(&fixture.base, "feature.txt", "feature\n", "branch feature");
        let generated = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"generated\" }\n",
            "regenerate on rolling",
        );
        let source_tree = fixture.root.path().join("source-merge");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                source_tree.to_str().unwrap(),
                &feature,
            ],
        );
        git_run(
            &source_tree,
            &["merge", "-q", "--no-ff", &generated, "-m", "merge rolling"],
        );
        let source = git_value(&source_tree, &["rev-parse", "HEAD"]);
        git_run(
            &fixture.repo,
            &["worktree", "remove", source_tree.to_str().unwrap()],
        );
        let target = fixture.commit(
            &generated,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"base\" }\n",
            "regenerate again on rolling",
        );
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{target}:refs/heads/rolling"),
            ],
        );
        assert_eq!(
            git_value(&fixture.repo, &["merge-base", &source, &target]),
            generated
        );

        let old_base = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await
            .expect_err("historical base predates the derived merge-base");
        assert!(old_base.message.contains(&generated), "{old_base:?}");
        assert!(old_base.message.contains("omit BASE"), "{old_base:?}");
        assert_eq!(old_base.state, PublicationState::NotPublished);

        let report = fixture
            .land(vec![AcceptedPair {
                base: String::new(),
                source: source.clone(),
            }])
            .await
            .expect("derived-base landing succeeds");
        assert_eq!(report.kind, CandidateKind::Merge);
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
        assert_eq!(
            git_value(
                &fixture.bare,
                &["show", &format!("{}:feature.txt", report.candidate)]
            ),
            "feature"
        );
        assert_eq!(
            git_value(
                &fixture.bare,
                &[
                    "show",
                    &format!("{}:crates/demo/src/lib.rs", report.candidate)
                ]
            ),
            "pub fn value() -> &'static str { \"base\" }"
        );
    }

    #[tokio::test]
    async fn explicit_nonancestor_base_must_equal_derived_merge_base() {
        let fixture = Fixture::new();
        let first_hunk = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "first branch hunk",
        );
        let source = fixture.commit(
            &first_hunk,
            "feature.txt",
            "feature\n",
            "second branch hunk",
        );
        let unrelated = fixture.commit(&fixture.base, "unrelated.txt", "other\n", "other branch");
        let target = fixture.commit(&fixture.base, "target.txt", "target\n", "rolling change");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{target}:refs/heads/rolling"),
            ],
        );

        for wrong_base in [first_hunk, unrelated] {
            let error = fixture
                .land(vec![AcceptedPair {
                    base: wrong_base.clone(),
                    source: source.clone(),
                }])
                .await
                .expect_err("wrong explicit base must be refused before candidate preparation");
            assert_eq!(error.state, PublicationState::NotPublished);
            assert!(
                error.message.contains("does not match merge-base"),
                "{error:?}"
            );
            assert!(error.message.contains(&wrong_base), "{error:?}");
            assert!(error.message.contains(&fixture.base), "{error:?}");
            assert!(error.message.contains("omit BASE"), "{error:?}");
            assert_eq!(
                git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
                target
            );
        }
    }

    #[tokio::test]
    async fn derived_base_still_detects_reverted_branch_only_hunk() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "branch-only hunk",
        );
        let target = fixture.commit(&fixture.base, "target.txt", "target\n", "rolling change");
        let reverted = fixture.commit(
            &source,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"base\" }\n",
            "revert branch hunk",
        );
        let candidate_tree = fixture.root.path().join("candidate-merge");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                candidate_tree.to_str().unwrap(),
                &reverted,
            ],
        );
        git_run(
            &candidate_tree,
            &["merge", "-q", "--no-ff", &target, "-m", "merge target"],
        );
        let candidate = git_value(&candidate_tree, &["rev-parse", "HEAD"]);
        git_run(
            &fixture.repo,
            &["worktree", "remove", candidate_tree.to_str().unwrap()],
        );
        let mut pair = AcceptedPair {
            base: String::new(),
            source,
        };
        resolve_accepted_base(&fixture.repo, &mut pair, &target).expect("derive base");
        assert_eq!(pair.base, fixture.base);
        let options = Options {
            repo: fixture.repo.clone(),
            remote: "origin".into(),
            accepted: vec![pair.clone()],
            test_filters: Vec::new(),
            cargo_build_jobs: 4,
            remote_gate: None,
            tmpfs_min_free_gb: 12,
            disk_scratch_shards: BTreeSet::new(),
        };
        let error = run_guard_pair(
            &fixture.repo,
            &options,
            &pair,
            &target,
            &candidate,
            None,
            false,
        )
        .await
        .expect_err("reverted source hunk must be detected");
        assert!(error.contains("restored base line"), "{error}");
    }

    #[tokio::test]
    async fn red_published_tip_canary_creates_ancestry_preserving_forward_revert() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        fixture.add_fake_canary_cargo(&fixture.base, "exit 42");
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("red post-publish canary must fail the landing");
        assert_eq!(failure.state, PublicationState::Published);
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::Published)
        );
        let revert = failure.forward_revert_id.expect("forward revert commit");
        assert_eq!(failure.observed_tip.as_deref(), Some(revert.as_str()));
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            revert
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^1")]),
            source
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^{{tree}}")]),
            git_value(
                &fixture.repo,
                &["rev-parse", &format!("{}^{{tree}}", fixture.base)]
            )
        );
        assert_eq!(
            git_value(&fixture.repo, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
    }

    #[tokio::test]
    async fn red_merge_canary_reverts_to_prior_target_tree_without_losing_source_ancestry() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let target = fixture.commit(&fixture.base, "target.txt", "target\n", "target side");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{target}:refs/heads/rolling"),
            ],
        );
        fixture.add_fake_canary_cargo(&target, "exit 42");
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("red merge canary must forward revert");
        let candidate = failure.candidate.expect("merge candidate");
        let revert = failure.forward_revert_id.expect("forward revert commit");
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::Published)
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^1")]),
            candidate
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^{{tree}}")]),
            git_value(&fixture.bare, &["rev-parse", &format!("{target}^{{tree}}")])
        );
        git_run(
            &fixture.bare,
            &["merge-base", "--is-ancestor", &source, &revert],
        );
    }

    #[tokio::test]
    async fn concurrent_remote_advance_blocks_forward_revert_without_overwrite() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other = fixture.commit(&source, "other.txt", "later work\n", "concurrent work");
        fixture.add_fake_canary_cargo(
            &fixture.base,
            &format!(
                "/usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43\nexit 42",
                fixture.repo.display(),
                other,
            ),
        );
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("red canary with concurrent advance must fail");
        assert_eq!(failure.state, PublicationState::Published);
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::NotPublished)
        );
        assert!(failure.forward_revert_id.is_some());
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            other
        );
        assert_eq!(
            git_value(&fixture.repo, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        let recovery_path = failure.recovery_path.expect("private recovery clone");
        assert!(recovery_path.join("repo/.git").exists());
        fs::remove_dir_all(recovery_path).expect("cleanup private test recovery clone");
    }

    #[tokio::test]
    async fn concurrent_descendant_revert_preserves_other_landing() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other = fixture.commit(&source, "other.txt", "later work\n", "concurrent work");
        let second = fixture.root.path().join("red-canary-observed");
        fixture.add_fake_canary_cargo(&fixture.base, &format!(
            "if [ ! -e '{}' ]; then /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43; : > '{}'; exit 42; fi",
            second.display(), fixture.repo.display(), other, second.display(),
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("red canary requires a forward revert");
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::Published)
        );
        assert_eq!(failure.exit_code(), 4);
        let revert = failure.forward_revert_id.expect("verified revert commit");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^1")]),
            other
        );
        assert_eq!(
            git_value(&fixture.bare, &["show", &format!("{revert}:other.txt")]),
            "later work"
        );
        assert_eq!(
            git_value(
                &fixture.bare,
                &["show", &format!("{revert}:crates/demo/src/lib.rs")]
            ),
            "pub fn value() -> &'static str { \"base\" }"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            revert
        );
    }

    #[tokio::test]
    async fn two_stale_revert_attempts_keep_recovery_and_remote_work() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other1 = fixture.commit(&source, "other1.txt", "one\n", "first advance");
        let other2 = fixture.commit(&other1, "other2.txt", "two\n", "second advance");
        let other3 = fixture.commit(&other2, "other3.txt", "three\n", "third advance");
        let marker = fixture.root.path().join("guard-run-count");
        fixture.add_fake_cargo(&format!(
            "count=$(cat '{}' 2>/dev/null || echo 0)\ncount=$((count + 1))\necho \"$count\" > '{}'\ncase $count in\n  2) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43; exit 42;;\n  3) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43;;\n  5) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43;;\nesac",
            marker.display(), marker.display(), fixture.repo.display(), other1,
            fixture.repo.display(), other2, fixture.repo.display(), other3,
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("bounded retries must stop after two stale tips");
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::NotPublished)
        );
        assert_eq!(failure.exit_code(), 5);
        assert_eq!(failure.observed_tip.as_deref(), Some(other3.as_str()));
        assert_eq!(fs::read_to_string(marker).unwrap().trim(), "6");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            other3
        );
        let recovery_path = failure.recovery_path.expect("red remote retains custody");
        assert!(recovery_path.join("repo/.git").exists());
        fs::remove_dir_all(recovery_path).expect("cleanup private test recovery clone");
    }

    #[tokio::test]
    async fn green_canary_reports_descendant_tip_separately() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other = fixture.commit(&source, "other.txt", "later work\n", "concurrent work");
        fixture.add_fake_canary_cargo(
            &fixture.base,
            &format!(
                "/usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43",
                fixture.repo.display(),
                other,
            ),
        );
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("combined descendant was not canary-tested");
        assert_eq!(failure.kind, FailureKind::DescendantGreen);
        assert_eq!(failure.exit_code(), 7);
        assert_eq!(failure.observed_tip.as_deref(), Some(other.as_str()));
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            other
        );
    }

    #[tokio::test]
    async fn changed_configured_push_remote_blocks_publication() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other_bare = fixture.root.path().join("other-remote.git");
        git_run(
            fixture.root.path(),
            &["init", "--bare", "-q", other_bare.to_str().unwrap()],
        );
        fixture.add_fake_cargo(&format!(
            "/usr/bin/git -C '{}' remote set-url --push origin '{}'",
            fixture.repo.display(),
            other_bare.display()
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("changed remote must block publication");
        assert_eq!(failure.state, PublicationState::NotPublished);
        assert!(
            failure
                .message
                .contains("configured publishing remote changed")
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
    }

    #[tokio::test]
    async fn canary_remote_rebinding_blocks_forward_revert_to_old_remote() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other_bare = fixture.root.path().join("other-remote.git");
        git_run(
            fixture.root.path(),
            &["init", "--bare", "-q", other_bare.to_str().unwrap()],
        );
        fixture.add_fake_canary_cargo(
            &fixture.base,
            &format!(
                "/usr/bin/git -C '{}' remote set-url --push origin '{}' || exit 43\nexit 42",
                fixture.repo.display(),
                other_bare.display(),
            ),
        );
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("remote rebinding must block forward revert");
        assert_eq!(failure.state, PublicationState::Published);
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::NotPublished)
        );
        assert!(
            failure
                .message
                .contains("configured publishing remote changed")
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
        );
        let recovery_path = failure.recovery_path.expect("private recovery clone");
        fs::remove_dir_all(recovery_path).expect("cleanup private test recovery clone");
    }

    #[tokio::test]
    async fn identical_published_tree_reuses_candidate_gate_without_extra_cargo() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let args_log = fixture.add_fake_cargo_log();
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_filters(
                vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source,
                }],
                vec!["demo=accepted::focus".into(), "demo=other::focus".into()],
            )
            .await;
        restore_path(old_path);
        let report = result.expect("focused landing succeeds");
        assert_eq!(
            report.canary_reused_tree,
            Some(git_value(
                &fixture.repo,
                &["rev-parse", &format!("{}^{{tree}}", report.candidate)]
            ))
        );
        let expected_pass = concat!(
            "metadata\n--no-deps\n--format-version\n1\n--offline\n",
            "test\n-p\ndemo\n--lib\n--\n--test-threads=4\n",
            "test\n-p\ndemo\n--lib\n--\n--test-threads=4\n",
            "test\n-p\ndemo\n--lib\naccepted::focus\n--\n--test-threads=4\n",
            "test\n-p\ndemo\n--lib\naccepted::focus\n--\n--test-threads=4\n",
            "test\n-p\ndemo\n--lib\nother::focus\n--\n--test-threads=4\n",
            "test\n-p\ndemo\n--lib\nother::focus\n--\n--test-threads=4\n",
        );
        assert_eq!(
            fs::read_to_string(args_log).expect("cargo args"),
            expected_pass,
            "base and candidate run each focused test; canary reuses exact-base results"
        );
        let env_log = fs::read_to_string(fixture.root.path().join("cargo-env.log"))
            .expect("cargo environment");
        let mut lines = env_log.lines();
        let configured_target =
            std::env::var("CARGO_TARGET_DIR").expect("session target configured");
        let target = validate_cargo_target_dir().expect("session target directory");
        let workspace_parent = landing_workspace_parent(&fixture.repo, &target)
            .expect("selected landing workspace parent");
        let gate_scratch = report
            .gate_scratch
            .split_once(':')
            .expect("scratch kind and path")
            .1;
        for _ in 0..6 {
            let guard_target = lines.next().expect("isolated guard target");
            assert_ne!(guard_target, configured_target);
            assert_eq!(lines.next(), Some("1"));
            assert_eq!(lines.next(), Some("line-tables-only"));
            let guard_worktree = lines.next().expect("guard worktree directory");
            let worktree = Path::new(guard_worktree);
            let expected_target = worktree.with_file_name(format!(
                "{}-cargo-target",
                worktree.file_name().unwrap().to_string_lossy()
            ));
            assert_eq!(Path::new(guard_target), expected_target);
            let relative = Path::new(guard_worktree)
                .strip_prefix(&workspace_parent)
                .expect("guard worktree is inside the selected workspace parent");
            let workspace = relative.components().next().expect("private workspace");
            assert!(
                workspace
                    .as_os_str()
                    .to_string_lossy()
                    .starts_with("rsi-rolling-land-"),
                "guard worktree has no private landing workspace: {guard_worktree}"
            );
            assert_eq!(lines.next(), Some(gate_scratch));
        }
        assert_eq!(lines.next(), None);
        if report.gate_scratch.starts_with("tmpfs:") {
            assert!(!Path::new(gate_scratch).exists(), "private scratch cleaned");
        }
        assert!(!fixture.repo.join("target").exists());
    }

    #[test]
    fn private_landing_workspace_uses_git_dir_for_sandbox_target() {
        let fixture = Fixture::new();
        let sandbox_target = fixture.repo.join("target");
        fs::create_dir(&sandbox_target).expect("sandbox target");
        assert!(fixture.repo.join(".git").exists());
        let git_dir = PathBuf::from(git_value(
            &fixture.repo,
            &["rev-parse", "--absolute-git-dir"],
        ));
        assert_eq!(
            landing_workspace_parent(&fixture.repo, &sandbox_target)
                .expect("sandbox landing parent"),
            git_dir
        );

        let custom_target = fixture.root.path().join("custom-cargo-target");
        fs::create_dir(&custom_target).expect("custom target");
        assert_eq!(
            landing_workspace_parent(&fixture.repo, &custom_target).expect("custom landing parent"),
            custom_target
        );
    }

    #[test]
    fn insufficient_tmpfs_uses_sandbox_target_scratch() {
        let root = tempfile::tempdir().expect("scratch fixture");
        let target = root.path().join("target");
        fs::create_dir(&target).expect("target directory");
        let scratch = select_gate_scratch(&target, 1024).expect("fallback scratch");
        assert!(matches!(scratch, GateScratch::Disk(_)));
        assert_eq!(scratch.path(), target.join(".rsi-tmp"));
        assert!(scratch.path().is_dir());
    }

    #[tokio::test]
    async fn marked_shard_uses_same_private_disk_scratch_on_both_sides() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "marked.txt", "marked\n", "marked shard");
        let base_worktree = fixture.root.path().join("marked-base");
        let candidate_worktree = fixture.root.path().join("marked-candidate");
        for (path, commit) in [
            (&base_worktree, &fixture.base),
            (&candidate_worktree, &source),
        ] {
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    path.to_str().unwrap(),
                    commit,
                ],
            );
        }
        let log = fixture.root.path().join("marked-scratch.log");
        for worktree in [&base_worktree, &candidate_worktree] {
            let script = worktree.join("scripts/run-rsid-test-shards.sh");
            write(
                worktree,
                "scripts/run-rsid-test-shards.sh",
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$TMPDIR\" >> '{}'\n",
                    log.display()
                ),
            );
            let mut permissions = fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script, permissions).unwrap();
        }
        let target = fixture.root.path().join("target");
        fs::create_dir(&target).unwrap();
        let mut gate = TestGate {
            scratch: Some(select_gate_scratch(&target, 1).unwrap()),
            ..TestGate::default()
        };
        gate.base_worktrees
            .insert(fixture.base.clone(), base_worktree.clone());
        let spec = GuardSpec {
            commands: vec![rsid_shard_guard_command("store-01").unwrap()],
            env: BTreeMap::new(),
            output_tail_bytes: 4096,
        };
        run_affected_gate(
            &fixture.repo,
            &candidate_worktree,
            &fixture.base,
            &spec,
            &mut gate,
            &BTreeSet::from(["store-01".to_string()]),
        )
        .await
        .expect("marked shard gate");
        let disk = fixture.root.path().join("disk-test-scratch");
        assert_eq!(
            fs::read_to_string(log).unwrap(),
            format!("{0}\n{0}\n", disk.display())
        );
        assert_eq!(gate.disk_scratch_used.len(), 1);
        for worktree in [&base_worktree, &candidate_worktree] {
            git_run(
                worktree,
                &["restore", "--", "scripts/run-rsid-test-shards.sh"],
            );
            git_run(
                &fixture.repo,
                &["worktree", "remove", worktree.to_str().unwrap()],
            );
        }
    }

    #[tokio::test]
    async fn merge_candidate_has_exactly_two_parents_and_is_published() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"source\" }\n",
            "source",
        );
        let target = fixture.commit(&fixture.base, "target.txt", "target\n", "target side");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{target}:refs/heads/rolling"),
            ],
        );
        fixture.add_fake_cargo("");
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let report = result.expect("merge landing succeeds");
        assert_eq!(report.kind, CandidateKind::Merge);
        let parents = git_value(
            &fixture.bare,
            &["rev-list", "--parents", "-n", "1", &report.candidate],
        );
        assert_eq!(parents.split_whitespace().count(), 3);
        assert!(parents.contains(&target));
        assert!(parents.contains(&source));
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.candidate
        );
        assert_source_clean(&fixture.repo);
    }

    #[tokio::test]
    async fn lost_hunk_guard_rejects_without_changing_remote() {
        let fixture = Fixture::new();
        let args_log = fixture.add_fake_cargo_log();
        let old_path = use_fake_path(&fixture);
        let accepted = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted",
        );
        let reverted = fixture.commit(
            &accepted,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"base\" }\n",
            "revert accepted hunk",
        );
        let followup = fixture.commit(
            &reverted,
            "crates/demo/src/other.rs",
            "pub fn other() {}\n",
            "unrelated affected-crate followup",
        );
        let result = fixture
            .land(vec![
                AcceptedPair {
                    base: fixture.base.clone(),
                    source: accepted.clone(),
                },
                AcceptedPair {
                    base: accepted,
                    source: followup,
                },
            ])
            .await;
        restore_path(old_path);
        let error = result.unwrap_err();
        assert!(error.message.contains("guard rejected"), "{error:?}");
        assert!(
            fs::read_to_string(args_log)
                .expect("affected-crate tests ran despite lost hunk")
                .contains("demo"),
            "lost-hunk diagnostics must not skip the affected-crate test phase"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        assert_source_clean(&fixture.repo);
    }

    #[tokio::test]
    async fn interrupted_landing_cleans_private_workspace_and_source() {
        let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
        fs::create_dir_all(&parent).expect("workspace target directory");
        let parent = parent.canonicalize().expect("canonical workspace target");
        let fixture = Fixture::new_in(&parent);
        let target = fixture.repo.join("target");
        fs::create_dir(&target).expect("sandbox-local Cargo target");
        let _target_env = ScopedCargoTarget::set(&target);
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"interrupted\" }\n",
            "interrupted source",
        );
        let marker = fixture.root.path().join("cargo-started");
        fixture.add_fake_cargo(&format!("touch '{}'\nsleep 30", marker.display()));
        let old_path = use_fake_path(&fixture);
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            land_until_cancelled(
                Options {
                    repo: fixture.repo.clone(),
                    remote: "origin".into(),
                    accepted: vec![AcceptedPair {
                        base: fixture.base.clone(),
                        source,
                    }],
                    test_filters: Vec::new(),
                    cargo_build_jobs: 1,
                    remote_gate: None,
                    tmpfs_min_free_gb: 12,
                    disk_scratch_shards: BTreeSet::new(),
                },
                async {
                    while !marker.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                },
            ),
        )
        .await;
        restore_path(old_path);
        let result = result.expect("landing reached its interruptible guard");
        assert_eq!(result.unwrap_err().state, PublicationState::Unknown);
        assert_source_clean(&fixture.repo);
        let git_dir = landing_workspace_parent(&fixture.repo, &target)
            .expect("sandbox landing workspace parent");
        assert_eq!(
            fs::read_dir(git_dir)
                .expect("git metadata directory")
                .filter(|entry| {
                    entry.as_ref().is_ok_and(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("rsi-rolling-land-")
                    })
                })
                .count(),
            0,
            "interrupt must remove the candidate and scratch workspace"
        );
    }

    #[tokio::test]
    async fn ancestor_remote_advance_during_guard_regates_and_publishes() {
        let fixture = Fixture::new();
        let intermediate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"intermediate\" }\n",
            "intermediate accepted source",
        );
        let candidate = fixture.commit(
            &intermediate,
            "crates/demo/src/extra.rs",
            "pub const EXTRA: bool = true;\n",
            "final accepted source",
        );
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{intermediate}:refs/heads/intermediate"),
            ],
        );
        let marker = fixture.root.path().join("advanced-once");
        fixture.add_fake_cargo(&format!(
            "if [ ! -e '{}' ]; then /usr/bin/git --git-dir='{}' update-ref refs/heads/rolling {intermediate}; : > '{}'; fi",
            marker.display(), fixture.bare.display(), marker.display(),
        ));
        let old_path = use_fake_path(&fixture);
        let old_git_dir = std::env::var_os("GIT_DIR");
        // The remote read/push subprocesses must ignore this inherited repo override.
        unsafe {
            std::env::set_var("GIT_DIR", fixture.root.path().join("not-a-git-dir"));
        }
        let result = fixture
            .land(vec![
                AcceptedPair {
                    base: fixture.base.clone(),
                    source: intermediate.clone(),
                },
                AcceptedPair {
                    base: intermediate.clone(),
                    source: candidate.clone(),
                },
            ])
            .await;
        restore_path(old_path);
        unsafe {
            if let Some(old_git_dir) = old_git_dir {
                std::env::set_var("GIT_DIR", old_git_dir);
            } else {
                std::env::remove_var("GIT_DIR");
            }
        }
        let report = result.expect("overlapping ancestor advance is regated");
        assert_eq!(report.fetched_tip, intermediate);
        assert!(
            Command::new("/usr/bin/git")
                .current_dir(&fixture.repo)
                .args(["merge-base", "--is-ancestor", &intermediate, &candidate])
                .status()
                .expect("check candidate ancestry")
                .success()
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
    }

    #[tokio::test]
    async fn remote_lookup_rejects_oversized_output_without_waiting_for_eof() {
        let fixture = Fixture::new();
        fixture.add_fake_git("    yes x\n    exit 0");
        let old_path = use_fake_path(&fixture);
        let result = remote_tip(&fixture.repo, "origin").await;
        restore_path(old_path);
        assert_eq!(
            result.unwrap_err(),
            "remote rolling lookup exceeded the 1024-byte response limit"
        );
    }

    #[tokio::test]
    async fn stalled_remote_lookup_times_out_and_reaps_git_child() {
        let fixture = Fixture::new();
        let pid_file = fixture.root.path().join("stalled-git.pid");
        fixture.add_fake_git(&format!(
            "    printf '%s\\n' \"$$\" > '{}'\n    exec sleep 30",
            pid_file.display()
        ));
        let old_path = use_fake_path(&fixture);
        let started = std::time::Instant::now();
        let result = remote_tip(&fixture.repo, "origin").await;
        restore_path(old_path);
        let elapsed = started.elapsed();
        let error = result.unwrap_err();
        assert!(
            error.contains("remote rolling lookup timed out"),
            "{error:?}"
        );
        assert!(elapsed < Duration::from_secs(4), "lookup took {elapsed:?}");
        let pid = fs::read_to_string(pid_file)
            .expect("fake Git wrote its pid")
            .trim()
            .parse::<i32>()
            .expect("valid pid");
        assert!(matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        ));
    }

    #[tokio::test]
    async fn stalled_initial_fetch_is_bounded_and_private_maintenance_is_disabled() {
        let fixture = Fixture::new();
        let pid_file = fixture.root.path().join("stalled-fetch.pid");
        let config_file = fixture.root.path().join("private-config.txt");
        fixture.add_fake_git_behaviors_with_fetch(
            "",
            "",
            &format!(
                "    /usr/bin/git config --local --get maintenance.auto > '{}'\n    /usr/bin/git config --local --get maintenance.autoDetach >> '{}'\n    /usr/bin/git config --local --get gc.autoDetach >> '{}'\n    printf '%s\\n' \"$$\" > '{}'\n    exec sleep 30",
                config_file.display(),
                config_file.display(),
                config_file.display(),
                pid_file.display()
            ),
        );
        let old_path = use_fake_path(&fixture);
        let started = std::time::Instant::now();
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: fixture.base.clone(),
            }])
            .await;
        restore_path(old_path);
        let error = result.unwrap_err();
        assert_eq!(error.state, PublicationState::NotPublished);
        assert!(
            error.message.contains("remote rolling fetch timed out"),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(
            fs::read_to_string(config_file).unwrap(),
            "false\nfalse\nfalse\n"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        let pid = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert!(matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        ));
    }

    #[tokio::test]
    async fn already_integrated_source_with_lost_hunk_is_rejected() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let reverted = fixture.commit(
            &source,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"base\" }\n",
            "revert accepted hunk",
        );
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{reverted}:refs/heads/rolling"),
            ],
        );
        let error = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await
            .unwrap_err();
        assert_eq!(error.state, PublicationState::NotPublished);
        assert!(error.message.contains("guard rejected"), "{error:?}");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            reverted
        );
    }

    #[tokio::test]
    async fn already_integrated_source_requires_historical_base() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "source.txt", "accepted\n", "accepted source");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{source}:refs/heads/rolling"),
            ],
        );
        let error = fixture
            .land(vec![AcceptedPair {
                base: String::new(),
                source: source.clone(),
            }])
            .await
            .expect_err("already-integrated source needs historical base");
        assert!(
            error.message.contains("historical accepted base"),
            "{error:?}"
        );
        assert_eq!(error.state, PublicationState::NotPublished);
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
        );
    }

    #[tokio::test]
    async fn rejected_push_with_proven_divergence_remains_unpublished() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other = fixture.commit(&fixture.base, "other.txt", "other\n", "other source");
        git_run(
            &fixture.repo,
            &["push", "-q", "origin", &format!("{other}:refs/heads/other")],
        );
        fixture.add_fake_cargo("");
        fixture.add_fake_git_behaviors(
            "",
            &format!(
                "      /usr/bin/git --git-dir='{}' update-ref refs/heads/rolling {other}\n      exit 1",
                fixture.bare.display()
            ),
        );
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let error = result.unwrap_err();
        assert_eq!(error.state, PublicationState::NotPublished);
        assert_eq!(error.exit_code(), 1);
        assert!(
            error.message.contains("remote rolling remained"),
            "{error:?}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            other
        );
    }

    #[test]
    fn published_failure_reports_distinct_exit_and_target_evidence() {
        let error = LandFailure {
            state: PublicationState::Published,
            kind: FailureKind::Cleanup,
            policy_fence: None,
            candidate: Some("candidate".into()),
            fetched_tip: Some("old".into()),
            published_tip: Some("candidate".into()),
            observed_tip: None,
            forward_revert_id: None,
            forward_revert_status: None,
            recovery_path: None,
            stale_retries: Vec::new(),
            message: "cleanup failed".into(),
        };
        assert_eq!(error.exit_code(), 2);
        assert_eq!(
            error.evidence_lines(),
            [
                "publication_status=published",
                "landing_outcome=published_cleanup_failed",
                "exit_code=2",
                "candidate_id=candidate",
                "fetched_target_id=old",
                "published_target_id=candidate",
                "stale_attempts=0"
            ]
        );
    }

    #[test]
    fn canary_red_exit_codes_distinguish_revert_outcomes() {
        for (status, expected_code, expected_outcome) in [
            (PublicationState::Published, 4, "canary_red_reverted"),
            (PublicationState::NotPublished, 5, "canary_red_unreverted"),
            (PublicationState::Unknown, 3, "canary_red_revert_unknown"),
        ] {
            let failure = LandFailure {
                state: PublicationState::Published,
                kind: FailureKind::CanaryRed,
                policy_fence: None,
                candidate: Some("candidate".into()),
                fetched_tip: Some("prior".into()),
                published_tip: Some("candidate".into()),
                observed_tip: None,
                forward_revert_id: Some("revert".into()),
                forward_revert_status: Some(status),
                recovery_path: None,
                stale_retries: Vec::new(),
                message: "published-tip canary failed".into(),
            };
            assert_eq!(failure.exit_code(), expected_code);
            assert!(
                failure
                    .evidence_lines()
                    .contains(&format!("landing_outcome={expected_outcome}"))
            );
        }
    }

    #[tokio::test]
    async fn stalled_push_times_out_and_leaves_remote_tip_unchanged() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        fixture.add_fake_cargo("");
        let pid_file = fixture.root.path().join("stalled-push.pid");
        fixture.add_fake_git_behaviors(
            "",
            &format!(
                "      printf '%s\\n' \"$$\" > '{}'\n      echo 'fake push started'\n      exec sleep 30",
                pid_file.display()
            ),
        );
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let error = result.unwrap_err();
        assert!(
            error.message.contains("fast-forward-only push timed out"),
            "{error:?}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        let pid = fs::read_to_string(pid_file)
            .expect("fake Git wrote its pid")
            .trim()
            .parse::<i32>()
            .expect("valid pid");
        assert!(matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        ));
    }

    #[test]
    fn requires_an_accepted_pair() {
        assert!(
            parse_args(Vec::<String>::new())
                .unwrap_err()
                .contains("at least one --accepted")
        );
    }

    #[test]
    fn parses_source_only_and_explicit_accepted_pairs() {
        let options = parse_args([
            "--accepted".to_string(),
            "source".to_string(),
            "--accepted".to_string(),
            "base:other".to_string(),
        ])
        .expect("accepted pairs");
        assert_eq!(options.accepted[0].base, "");
        assert_eq!(options.accepted[0].source, "source");
        assert_eq!(options.accepted[1].base, "base");
        assert_eq!(options.accepted[1].source, "other");
        assert_eq!(options.tmpfs_min_free_gb, 12);
        let configured = parse_args([
            "--accepted".to_string(),
            "source".to_string(),
            "--tmpfs-min-free-gb".to_string(),
            "24".to_string(),
            "--disk-scratch-shard".to_string(),
            "store-01".to_string(),
        ])
        .expect("configured minimum");
        assert_eq!(configured.tmpfs_min_free_gb, 24);
        assert!(configured.disk_scratch_shards.contains("store-01"));
        assert!(
            parse_args([
                "--accepted".to_string(),
                "source".to_string(),
                "--tmpfs-min-free-gb".to_string(),
                "0".to_string(),
            ])
            .is_err()
        );
        assert!(
            parse_args([
                "--accepted".to_string(),
                "source".to_string(),
                "--disk-scratch-shard".to_string(),
                "not-a-shard".to_string(),
            ])
            .is_err()
        );
        for malformed in ["", ":source", "base:", "a:b:c"] {
            assert!(
                parse_args(["--accepted".to_string(), malformed.to_string()]).is_err(),
                "{malformed:?} must be rejected"
            );
        }
    }

    #[test]
    fn remote_gate_cli_requires_complete_valid_configuration() {
        let identity = std::env::current_exe().expect("test binary exists");
        let args = vec![
            "--accepted".into(),
            "source".into(),
            "--remote-gate-host".into(),
            "ec2-user@example.com".into(),
            "--remote-gate-dir".into(),
            "/srv/rsi/gates".into(),
            "--remote-gate-identity".into(),
            identity.to_string_lossy().into_owned(),
        ];
        let options = parse_args(args.clone()).expect("complete remote configuration");
        assert_eq!(options.remote_gate.unwrap().target, "ec2-user@example.com");
        assert!(parse_args(args[..6].to_vec()).is_err());
    }
}
