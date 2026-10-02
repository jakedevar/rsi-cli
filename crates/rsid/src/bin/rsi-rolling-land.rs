// One shard inventory for the lander and the impact selector.
use rsi_codegraph::impact::RSID_SHARDS;
use rsi_common::failure_signature::{Classification, Snapshot, classify, failure_text_for};
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

#[path = "rsi-rolling-land/canary.rs"]
mod canary;
#[path = "rsi-rolling-land/canary_runner.rs"]
mod canary_runner;

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
/// Default guard for one isolated retry. A single-test retry that hangs must
/// fail the gate in minutes, not hold it for the whole shard timeout (#988).
const DEFAULT_RETRY_TIMEOUT_SECS: u64 = 600;

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

/// Guard for the isolated retry of one candidate failure. An absent or
/// unparsable `RSI_LANDER_RETRY_TIMEOUT_SECS` keeps the 10-minute default.
fn retry_timeout(guard_timeout: Duration) -> Duration {
    retry_timeout_from(
        std::env::var("RSI_LANDER_RETRY_TIMEOUT_SECS")
            .ok()
            .as_deref(),
        guard_timeout,
    )
}

/// Clamp a retry guard to 60..=the guard it retries, so a retry never outlives
/// the command it isolates.
fn retry_timeout_from(value: Option<&str>, guard_timeout: Duration) -> Duration {
    let secs = value
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETRY_TIMEOUT_SECS);
    let ceiling = guard_timeout.as_secs().max(1);
    let floor = 60.min(ceiling);
    Duration::from_secs(secs.clamp(floor, ceiling))
}

const TARGET: &str = "refs/heads/rolling";

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
    /// The rolling tip the merge queue probed just before this run (#1007):
    /// a different initial fetch is an out-of-band advance that spends one
    /// re-gated retry. Manual landings leave it unset.
    expected_tip: Option<String>,
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
    /// `known_red=` annotations for `base_reds`, empty without a snapshot.
    known_red_lines: Vec<String>,
    flakes: BTreeSet<String>,
    base_reused: BTreeSet<String>,
    local_base_confirmed: BTreeSet<String>,
    base_absent_packages: BTreeSet<String>,
    base_static_inventory_reds: BTreeSet<String>,
    canary_reused_tree: Option<String>,
    /// Registry root when the post-push canary was handed to the runner.
    canary_queued: Option<String>,
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

/// The merge queue (#1007) lowers the re-gated budget to 1 through
/// `RSI_LANDER_MAX_REGATED_STALE_RETRIES`; the value can only tighten the
/// default, never raise it.
fn max_regated_stale_retries() -> usize {
    std::env::var("RSI_LANDER_MAX_REGATED_STALE_RETRIES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map_or(MAX_REGATED_STALE_RETRIES, |value| {
            value.min(MAX_REGATED_STALE_RETRIES)
        })
}

/// `RSI_LANDER_EXACT_GATE=1` (set by the merge queue, #1007) makes every stale
/// retry re-gate the remade candidate: a disjoint advance no longer reuses the
/// earlier gate, so the published tip is always the tree that was tested. The
/// re-gated budget still bounds it. Manual landings keep the reuse path.
fn exact_gate_required() -> bool {
    std::env::var("RSI_LANDER_EXACT_GATE").is_ok_and(|value| value.trim() == "1")
}

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

type RemoteShardResults =
    BTreeMap<(String, String), Result<rsid::integration::GuardCommandReport, String>>;

#[derive(Default)]
struct TestGate {
    // An entry is reusable only for this exact rolling commit and command.
    base_cache: BTreeMap<(String, String), BTreeSet<String>>,
    // Preserve QA origin when a base result is reused again in this process.
    qa_cache_provenance: BTreeMap<(String, String), String>,
    base_reds: BTreeSet<String>,
    /// Failure text per base red, when the base shard report was in-process.
    base_red_texts: BTreeMap<String, String>,
    // Known-failure snapshot loaded once per landing (annotation only).
    known_failures: Option<Snapshot>,
    flakes: BTreeSet<String>,
    base_reused: BTreeSet<String>,
    local_base_confirmed: BTreeSet<String>,
    // Workspace package names per base commit; `None` when unproven.
    base_packages: BTreeMap<String, Option<BTreeSet<String>>>,
    base_absent_packages: BTreeSet<String>,
    // `<base>:<reason>` for each base whose shard-script static inventory
    // check refused; its results come from the direct bounded harness.
    base_static_inventory_reds: BTreeSet<String>,
    base_worktrees: BTreeMap<String, PathBuf>,
    cache_root: Option<PathBuf>,
    skip_tests: bool,
    remote: Option<Arc<Mutex<remote_gate::Executor>>>,
    // Remote shard results run ahead of the sequential gate loop (#970),
    // keyed by (commit, shard) and consumed once.
    remote_results: Mutex<RemoteShardResults>,
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
            known_failures: load_known_failures(),
            ..Self::default()
        })
    }
}

/// Snapshot path: `RSI_KNOWN_FAILURES_SNAPSHOT`, else `$HOME/.rsi/qa/known-failures.v1.json`.
fn known_failures_path() -> PathBuf {
    std::env::var_os("RSI_KNOWN_FAILURES_SNAPSHOT").map_or_else(
        || {
            let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
            home.join(".rsi/qa/known-failures.v1.json")
        },
        PathBuf::from,
    )
}

/// Load the known-failure snapshot for one landing. A missing or unreadable
/// file is reported once and disables annotation; it never fails a landing.
/// Read at gate construction (not process-wide) so each landing, and each
/// test, sees the snapshot its own environment names.
fn load_known_failures() -> Option<Snapshot> {
    let path = known_failures_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Snapshot>(&text) {
            Ok(snapshot) => {
                if let Some(warning) = snapshot.stale_warning(chrono::Utc::now()) {
                    println!("known_failures=stale:{warning}");
                }
                Some(snapshot)
            }
            Err(error) => {
                println!("known_failures=unavailable:{}: {error}", path.display());
                None
            }
        },
        Err(error) => {
            println!("known_failures=unavailable:{}: {error}", path.display());
            None
        }
    }
}

/// `known_red=<name>:#<issue>:<class>`, `:<name>:known?:#<issues>` or `:new`.
fn known_red_line(name: &str, text: &str, snapshot: &Snapshot) -> String {
    match classify(snapshot, name, text, None) {
        Classification::Known { issue, class, .. } => {
            format!("known_red={name}:#{issue}:{}", class.as_str())
        }
        Classification::NameOnly { issues } => {
            let joined = issues
                .iter()
                .map(|issue| format!("#{issue}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("known_red={name}:known?:{joined}")
        }
        Classification::New => format!("known_red={name}:new"),
    }
}

/// One annotation line per base red; empty without a snapshot.
fn known_red_lines(
    base_reds: &BTreeSet<String>,
    texts: &BTreeMap<String, String>,
    snapshot: Option<&Snapshot>,
) -> Vec<String> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    base_reds
        .iter()
        .map(|name| known_red_line(name, texts.get(name).map_or("", String::as_str), snapshot))
        .collect()
}

/// ` [known #N class]`, ` [known? #N]` or ` [new]` appended to a refusal.
fn known_red_suffix(name: &str, text: &str, snapshot: Option<&Snapshot>) -> String {
    match snapshot.map(|snapshot| classify(snapshot, name, text, None)) {
        Some(Classification::Known { issue, class, .. }) => {
            format!(" [known #{issue} {}]", class.as_str())
        }
        Some(Classification::NameOnly { issues }) => {
            let joined = issues
                .iter()
                .map(|issue| format!("#{issue}"))
                .collect::<Vec<_>>()
                .join(",");
            format!(" [known? {joined}]")
        }
        Some(Classification::New) => " [new]".to_string(),
        None => String::new(),
    }
}

const NEW_FAILURES_PREFIX: &str = "new test failures relative to rolling ";

/// The refusal for tests that fail on the candidate but not on the base.
/// `failing` entries are test names, each optionally followed by a
/// ` [known ...]` suffix; `rest` is the trailing explanation.
fn new_failures_message(base: &str, failing: &[String], rest: &str) -> String {
    format!(
        "{NEW_FAILURES_PREFIX}{base}: {}; {rest}",
        failing.join(", ")
    )
}

/// The failing test names in a refusal built by `new_failures_message`, for
/// the receipt's machine-readable `failing_test=` lines.
fn failing_test_names(message: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in message.lines() {
        let Some(after) = line.split_once(NEW_FAILURES_PREFIX).map(|(_, rest)| rest) else {
            continue;
        };
        let Some((_, list)) = after.split_once(": ") else {
            continue;
        };
        let list = list.split_once("; ").map_or(list, |(names, _)| names);
        for entry in list.split(", ") {
            let name = entry.split(" [").next().unwrap_or(entry).trim();
            if !name.is_empty() && !names.iter().any(|seen| seen == name) {
                names.push(name.to_string());
            }
        }
    }
    names
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
        Self::oid(state, message, &candidate.oid, fetched_tip)
    }

    /// A failure about one commit, for callers that hold no prepared
    /// `Candidate` (the detached canary runner's forward revert).
    fn oid(
        state: PublicationState,
        message: impl Into<String>,
        oid: &str,
        fetched_tip: &str,
    ) -> Self {
        Self {
            state,
            kind: FailureKind::General,
            policy_fence: None,
            candidate: Some(oid.to_string()),
            fetched_tip: Some(fetched_tip.to_string()),
            published_tip: (state == PublicationState::Published).then(|| oid.to_string()),
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
        lines.extend(
            failing_test_names(&self.message)
                .into_iter()
                .map(|name| rsid::rolling_queue::failing_test_line(&name)),
        );
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
    // The detached single canary runner (#951) shares this binary.
    match std::env::args().nth(1).as_deref() {
        Some("--canary-runner") => std::process::exit(canary_runner::run_main()),
        Some("--canary-status") => {
            std::process::exit(canary_runner::status_main(std::env::args().nth(2)))
        }
        _ => {}
    }
    let result = parse_args_from_env(std::env::args().skip(1))
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
            if let Some(registry) = &report.canary_queued {
                println!(
                    "canary: queued tip={} registry={registry}",
                    report.published_tip
                );
            }
            for name in &report.base_reds {
                println!("base_red={name}");
            }
            for line in &report.known_red_lines {
                println!("{line}");
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
            for name in &report.base_absent_packages {
                println!("base_absent_package={name}");
            }
            for entry in &report.base_static_inventory_reds {
                println!("base_static_inventory_red={entry}");
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

fn parse_args_from_env(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    parse_args(args, std::env::var_os("CARGO_BUILD_JOBS").as_deref())
}

fn parse_args(
    args: impl IntoIterator<Item = String>,
    cargo_build_jobs_env: Option<&std::ffi::OsStr>,
) -> Result<Options, String> {
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
    let mut expected_tip = None;
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
            "--expected-tip" => {
                let value = next_value(&mut args, "--expected-tip")?;
                if value.is_empty() || !value.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err("--expected-tip expects a commit SHA".to_string());
                }
                expected_tip = Some(value.to_ascii_lowercase());
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
        cargo_build_jobs: parse_cargo_build_jobs(cargo_build_jobs_env)?,
        remote_gate,
        tmpfs_min_free_gb,
        disk_scratch_shards,
        expected_tip,
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
    "usage: rsi-rolling-land [--repo PATH] [--remote NAME] --accepted SOURCE|BASE:SOURCE [--accepted SOURCE|BASE:SOURCE ...] [--test-filter PACKAGE=FILTER ...] [--tmpfs-min-free-gb N] [--disk-scratch-shard SHARD ...] [--expected-tip SHA] [--remote-gate-host USER@HOST --remote-gate-dir PATH --remote-gate-identity PATH [--remote-gate-run-as USER]]\nFor a landing candidate, omitted BASE is merge-base(SOURCE, current landing target tip); an explicit BASE must equal it. Already-integrated sources require the explicit historical BASE.\nAn rsid shard filter is rsid=shard:SHARD[:FILTERSET], for example rsid=shard:session-02:test(manager_recovery_). The full shard inventory is checked before any focused run.\n--tmpfs-min-free-gb requires N GiB free on /dev/shm before private gate TMPDIR is used (default 12); otherwise gates use sandbox target scratch. --disk-scratch-shard gives a named shard disk scratch on both sides.\nRemote shard execution is opt-in and refuses publication on missing evidence or mismatched commit/fingerprint. The SSH host must already be in known_hosts. Only full rsid shards run remotely; local regression comparison and isolated retries are preserved.\n--expected-tip is used by the merge queue: an initial fetch that differs from SHA is an out-of-band advance charged against the re-gated retry budget.\nCARGO_BUILD_JOBS: positive integer from 1 to 6; defaults to 4 when unset."
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
    // The queue probed the tip just before this run: a push in between moved
    // this run's base without a lost publish race, so it spends a re-gated
    // retry like one (the gate below runs on the fetched tip regardless).
    let mut stale = Vec::new();
    if let Some(probed) = options
        .expected_tip
        .as_ref()
        .filter(|probed| !fetched_tip.starts_with(probed.as_str()))
    {
        if max_regated_stale_retries() == 0 {
            return Err(format!(
                "stale retries exhausted: rolling advanced from the probed {probed} to \
                 {fetched_tip} before the initial fetch and no re-gated retry remains"
            )
            .into());
        }
        stale.push(StaleRetry {
            fetched: probed.clone(),
            observed: fetched_tip.clone(),
            reused_gate: false,
        });
    }
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

/// Prefix of the one gate error that means "the candidate introduced a test
/// failure relative to its base". Every other gate error is a setup or
/// environment refusal that produced no test verdict (#1025).
const NEW_TEST_FAILURES: &str = "new test failures relative to rolling";

/// Whether a post-publish canary error is a real test red. Only a test red may
/// forward-revert a landing (#1025): a lander refusal before any shard, a
/// missing target directory, a guard timeout or a load gate did not judge the
/// landing.
pub(crate) fn is_test_red(error: &str) -> bool {
    error.starts_with(NEW_TEST_FAILURES)
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
    // Operator P0 (2026-09-29): every policy check runs before the test gate,
    // so a refusal costs seconds, not a full gate (#955 lost three 68-minute
    // gates to a refusal at the final step).
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
    run_guard_pairs_once(
        repo,
        options,
        provisional,
        fetched_tip,
        &candidate.oid,
        &candidate.handle.worktree,
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
    // A disjoint stale advance may reuse the earlier gate without testing
    // its new tree. Only a gate executed for this candidate can be reused.
    let candidate_was_gated = !test_gate.skip_tests;
    test_gate.skip_tests = false;

    // The remote binding is re-read after the gate: it is the one policy
    // fact that can change while tests run.
    verify_remote_binding(source_repo, &options.remote, remote_url).map_err(|error| {
        LandFailure::candidate(
            PublicationState::NotPublished,
            error,
            candidate,
            fetched_tip,
        )
    })?;
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
    // #951: hand the post-push canary to the single runner instead of holding
    // this lander's slot for a second full gate. Provisional migrations keep
    // the inline canary; a failed handoff falls back to it.
    let canary_queued = if canary_reused_tree.is_none()
        && provisional.is_empty()
        && canary_runner::handoff_enabled()
    {
        match canary_runner::hand_off(
            source_repo,
            remote_url,
            options,
            fetched_tip,
            &published_tip,
        ) {
            Ok(root) => Some(root.display().to_string()),
            Err(error) => {
                eprintln!("rsi-rolling-land: canary handoff failed, running it inline: {error}");
                None
            }
        }
    } else {
        None
    };
    if canary_reused_tree.is_none()
        && canary_queued.is_none()
        && let Err(error) = run_guard_pairs_once(
            repo,
            options,
            provisional,
            fetched_tip,
            &published_tip,
            &candidate.handle.worktree,
            test_gate,
        )
        .await
    {
        if !is_test_red(&error) {
            // The canary could not judge the landing (environment or setup
            // error, #1025): never revert it.
            let mut failure = LandFailure::candidate(
                PublicationState::Published,
                format!("published-tip canary could not run, no revert: {error}"),
                candidate,
                fetched_tip,
            );
            failure.published_tip = Some(published_tip.clone());
            return Err(failure);
        }
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
        known_red_lines: known_red_lines(
            &test_gate.base_reds,
            &test_gate.base_red_texts,
            test_gate.known_failures.as_ref(),
        ),
        flakes: test_gate.flakes.clone(),
        base_reused: test_gate.base_reused.clone(),
        local_base_confirmed: test_gate.local_base_confirmed.clone(),
        base_absent_packages: test_gate.base_absent_packages.clone(),
        base_static_inventory_reds: test_gate.base_static_inventory_reds.clone(),
        canary_reused_tree,
        canary_queued,
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
    let disjoint = !exact_gate_required()
        && candidate_paths.is_disjoint(&incoming_paths)
        && (provisional.is_empty()
            || !touches_provisional_gate(repo, options, fetched_tip, &current, &incoming_paths)
                .map_err(fail)?);
    let regated = stale.iter().filter(|retry| !retry.reused_gate).count();
    if !disjoint && regated >= max_regated_stale_retries() {
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
    let plan = run_guard_pair_static(
        repo,
        options,
        pair,
        fetched_tip,
        candidate,
        allow_lost_hunks,
        proof,
    )
    .await?;
    if let Some(worktree) = worktree {
        run_candidate_test_gate(
            repo,
            options,
            fetched_tip,
            candidate,
            worktree,
            &plan.affected_crates,
            proof.is_some(),
            test_gate,
        )
        .await?;
    }
    Ok(())
}

/// Every source of a multi-source landing keeps its own static checks (the
/// guard plan, lost hunks, released migrations), but the candidate is one
/// tree, so its test gate runs once over the union of the requested filters
/// and the union of the plans' affected crates (#1007). A single source takes
/// exactly the same steps as `run_guard_pair_with_proof`.
async fn run_guard_pairs_once(
    repo: &Path,
    options: &Options,
    provisional: &[ProvisionalLanding],
    fetched_tip: &str,
    candidate: &str,
    worktree: &Path,
    test_gate: &mut TestGate,
) -> Result<(), String> {
    if options.accepted.is_empty() {
        return Ok(());
    }
    let mut affected_crates = BTreeSet::<String>::new();
    let mut any_proof = false;
    for pair in &options.accepted {
        let proof = provisional
            .iter()
            .find(|unit| unit.source == pair.source && unit.base == pair.base)
            .map(|unit| unit.proof_path.as_path());
        any_proof |= proof.is_some();
        let plan = run_guard_pair_static(repo, options, pair, fetched_tip, candidate, false, proof)
            .await?;
        affected_crates.extend(plan.affected_crates);
    }
    let affected_crates = affected_crates.into_iter().collect::<Vec<_>>();
    run_candidate_test_gate(
        repo,
        options,
        fetched_tip,
        candidate,
        worktree,
        &affected_crates,
        any_proof,
        test_gate,
    )
    .await
}

/// The per-source checks that need no build: plan parse, lost hunks, guard
/// status and the released-migration check. Returns the guard plan.
async fn run_guard_pair_static(
    repo: &Path,
    options: &Options,
    pair: &AcceptedPair,
    fetched_tip: &str,
    candidate: &str,
    allow_lost_hunks: bool,
    proof: Option<&Path>,
) -> Result<GuardPlan, String> {
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
    // Every cheap static guard refuses before any build or test runs (#1052).
    if !plan.lost_hunks.is_empty() && !allow_lost_hunks {
        return Err(format!(
            "rolling landing guard rejected accepted pair {}:{} before any test ran: {}",
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
    Ok(plan)
}

/// The candidate's test gate: one run of the affected crates plus the
/// requested filters against the candidate worktree.
#[allow(clippy::too_many_arguments)]
async fn run_candidate_test_gate(
    repo: &Path,
    options: &Options,
    fetched_tip: &str,
    candidate: &str,
    worktree: &Path,
    affected_crates: &[String],
    provisional: bool,
    test_gate: &mut TestGate,
) -> Result<(), String> {
    if git_text(worktree, &["rev-parse", "HEAD"])? != candidate
        || !git_text(worktree, &["status", "--porcelain=v1"])?.is_empty()
    {
        return Err("landing guard worktree must be clean at the candidate".into());
    }
    let metadata = workspace_metadata(worktree)?;
    let spec = affected_crate_guard_spec(
        fetched_tip,
        candidate,
        affected_crates,
        &options.test_filters,
        provisional,
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
    .await
}

fn test_failure_names(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("test ") {
                return rest.strip_suffix(" ... FAILED").map(str::to_owned);
            }
            // Nextest prints a status, a duration, an ordinal, and the crate
            // before the test identity. The final token is the exact retry
            // target.
            let (status, rest) = line.split_once(' ')?;
            if !nextest_unsuccessful_status(status) || !rest.trim_start().starts_with('[') {
                return None;
            }
            line.split_whitespace().last().map(str::to_owned)
        })
        .collect()
}

/// The panic or error lines of a failed isolated retry, bounded, so a refusal
/// receipt names the failing assertion and not only the test. Flakes such as
/// #926 were refused repeatedly with no assertion on record.
fn retry_failure_evidence(report: &rsid::integration::GuardCommandReport) -> String {
    const LIMIT: usize = 2048;
    let mut evidence = String::new();
    for stream in [&report.stderr_tail, &report.stdout_tail] {
        let lines: Vec<&str> = stream.lines().map(str::trim).collect();
        for (index, line) in lines.iter().enumerate() {
            let details = if line.contains("panicked at") {
                &lines[index..(index + 4).min(lines.len())]
            } else if line.starts_with("Error: ") {
                &lines[index..=index]
            } else {
                continue;
            };
            for detail in details {
                if detail.is_empty() || detail.starts_with("note: run with") {
                    continue;
                }
                if evidence.len() + detail.len() + 3 > LIMIT {
                    return evidence;
                }
                if !evidence.is_empty() {
                    evidence.push_str(" | ");
                }
                evidence.push_str(detail);
            }
        }
    }
    evidence
}

/// Every nextest status that ends a test unsuccessfully. A test killed at the
/// per-test timeout under host load (`TIMEOUT`) or by a signal must reach
/// the isolated retry and base comparison exactly like a `FAIL` (#963).
fn nextest_unsuccessful_status(status: &str) -> bool {
    matches!(status, "FAIL" | "TIMEOUT" | "LEAK-FAIL" | "ABORT")
        || (status.len() > 3
            && status.starts_with("SIG")
            && status.bytes().all(|byte| byte.is_ascii_uppercase()))
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

/// The isolated retry of one failure with its own bounded guard, so a hung
/// retry fails the gate quickly instead of holding it for the shard timeout.
fn bounded_isolated_retry_command(
    command: &GuardCommand,
    name: &str,
) -> Result<GuardCommand, String> {
    let mut isolated = isolated_retry_command(command, name)?;
    isolated.timeout = retry_timeout(command.timeout);
    Ok(isolated)
}

/// A hung retry is its own refusal: an ordinary test failure carries a failing
/// assertion, a timeout carries the retry guard.
fn retry_hung_error(name: &str, timeout: Duration) -> String {
    format!(
        "isolated retry hung (timed out after {} s): {name}",
        timeout.as_secs()
    )
}

/// Run the isolated retry of `name` under its bounded guard.
async fn run_isolated_retry(
    worktree: &Path,
    spec: &GuardSpec,
    command: &GuardCommand,
    name: &str,
) -> Result<rsid::integration::GuardCommandReport, String> {
    let isolated = bounded_isolated_retry_command(command, name)?;
    let report = run_one_guard(worktree, spec, &isolated).await?;
    if report.status == rsid::integration::GuardStatus::TimedOut {
        return Err(retry_hung_error(name, isolated.timeout));
    }
    Ok(report)
}

async fn run_one_guard(
    worktree: &Path,
    spec: &GuardSpec,
    command: &GuardCommand,
) -> Result<rsid::integration::GuardCommandReport, String> {
    let mut env = spec.env.clone();
    if command.program == "cargo" || command.program == "scripts/run-rsid-test-shards.sh" {
        // Each gate side builds once into its own target; incremental state
        // there is never reused (#1063).
        env.entry("CARGO_INCREMENTAL".into())
            .or_insert_with(|| "0".into());
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
    let wrapped = slot_wrapped_command(
        command,
        std::env::var_os("RSI_LANDER_LOCAL_SLOT_WRAPPER").as_deref(),
    );
    let report = run_guard(
        worktree,
        &GuardSpec {
            commands: vec![wrapped],
            env,
            output_tail_bytes: spec.output_tail_bytes,
        },
    )
    .await;
    let mut report = report
        .commands
        .into_iter()
        .next()
        .ok_or_else(|| "landing guard executed no command".to_string())?;
    // Receipts and comparisons name the gate command, not the slot wrapper.
    report.program.clone_from(&command.program);
    report.args.clone_from(&command.args);
    Ok(report)
}

/// A remote-gate run holds no desktop lander slot (#969), so each local
/// build or test it still runs (non-shard crates, focused filters, isolated
/// retries, base comparisons) takes a desktop build slot through
/// `RSI_LANDER_LOCAL_SLOT_WRAPPER` (for example `~/.rsi/bin/cargo-slot`).
/// Only Cargo and the shard runner are wrapped; git checks stay direct. The
/// wrapper applies at execution time only, so cache keys and shard detection
/// still see the unwrapped command.
fn slot_wrapped_command(command: &GuardCommand, wrapper: Option<&std::ffi::OsStr>) -> GuardCommand {
    let Some(wrapper) = wrapper.filter(|wrapper| !wrapper.is_empty()) else {
        return command.clone();
    };
    if command.program != "cargo" && command.program != "scripts/run-rsid-test-shards.sh" {
        return command.clone();
    }
    let mut args = Vec::with_capacity(command.args.len() + 1);
    args.push(command.program.clone());
    args.extend(command.args.iter().cloned());
    GuardCommand {
        program: wrapper.to_string_lossy().into_owned(),
        args,
        timeout: command.timeout,
    }
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

/// The package of a `cargo test -p <package> ...` guard; `None` for shard
/// scripts, `cargo check` and every other command shape.
fn cargo_test_package(command: &GuardCommand) -> Option<&str> {
    match command.args.as_slice() {
        [test, flag, package, ..]
            if command.program == "cargo" && test == "test" && flag == "-p" =>
        {
            Some(package.as_str())
        }
        _ => None,
    }
}

/// A package the base workspace provably does not declare. Unknown base
/// membership (`None`) never classifies a package as absent.
fn base_absent_package<'a>(
    command: &'a GuardCommand,
    base_packages: Option<&BTreeSet<String>>,
) -> Option<&'a str> {
    let package = cargo_test_package(command)?;
    (!base_packages?.contains(package)).then_some(package)
}

/// Package names declared by the base workspace, read from Git objects so no
/// cargo process runs on the base. Returns `None` when membership cannot be
/// proven (no `[workspace]`, glob members, unreadable or malformed manifests).
fn base_workspace_packages(repo: &Path, base: &str) -> Option<BTreeSet<String>> {
    let manifest = |path: &str| -> Option<toml::Value> {
        toml::from_str(&git_text(repo, &["show", &format!("{base}:{path}")]).ok()?).ok()
    };
    let package_name = |manifest: &toml::Value| -> Option<String> {
        Some(manifest.get("package")?.get("name")?.as_str()?.to_owned())
    };
    let root = manifest("Cargo.toml")?;
    let mut names = BTreeSet::new();
    if root.get("package").is_some() {
        names.insert(package_name(&root)?);
    }
    for member in root.get("workspace")?.get("members")?.as_array()? {
        let member = member.as_str()?.trim_end_matches('/');
        if member.is_empty() || member.contains(['*', '?', '[']) {
            return None;
        }
        names.insert(package_name(&manifest(&format!("{member}/Cargo.toml"))?)?);
    }
    Some(names)
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

/// Filesystem class of the TMPDIR a full shard command will actually receive.
/// A `--disk-scratch-shard` command gets disk scratch even when the gate
/// scratch itself is tmpfs; everything else gets the gate scratch class.
fn shard_tmpdir_class(
    scratch: Option<&GateScratch>,
    command: &GuardCommand,
    disk_scratch_shards: &BTreeSet<String>,
) -> &'static str {
    let disk_marked_shard = command.program == "scripts/run-rsid-test-shards.sh"
        && command
            .args
            .get(1)
            .is_some_and(|shard| disk_scratch_shards.contains(shard));
    match scratch {
        Some(GateScratch::Tmpfs(_)) if !disk_marked_shard => "tmpfs",
        _ => "disk",
    }
}

/// A digest of the environment class a full shard command runs under. A base
/// result is only comparable with a candidate that sees the same environment,
/// so every `spec.env` entry that reaches the test binaries is hashed; the
/// concrete TMPDIR path varies per run and is replaced by its filesystem class.
fn shard_env_class(spec_env: &BTreeMap<String, String>, tmpdir_class: &str) -> String {
    use sha2::Digest as _;

    let mut canonical = String::new();
    for (key, value) in spec_env {
        if key == "TMPDIR" {
            continue;
        }
        canonical.push_str(key);
        canonical.push('=');
        canonical.push_str(value);
        canonical.push('\n');
    }
    canonical.push_str("TMPDIR=");
    canonical.push_str(tmpdir_class);
    canonical.push('\n');
    format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(canonical.as_bytes()))
    )
}

/// Whether `value` is a `sha256:` digest the base cache can key on.
fn valid_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

/// Base-cache key for a full shard: the shard fingerprint folded with the
/// environment class, so a result measured under another scratch filesystem
/// or test environment is a miss (#988).
fn shard_cache_key(fingerprint: &str, env_class: &str) -> Result<String, String> {
    use sha2::Digest as _;

    if !valid_sha256_digest(fingerprint) || !valid_sha256_digest(env_class) {
        return Err(format!(
            "cannot key base shard cache from invalid digests: {fingerprint} {env_class}"
        ));
    }
    let folded = format!("{fingerprint}\n{env_class}");
    Ok(format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(folded.as_bytes()))
    ))
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
    let prefetched = gate
        .remote_results
        .lock()
        .map_err(|_| "remote_missing_evidence: prefetched results lock poisoned".to_string())?
        .remove(&(sha.clone(), shard.to_owned()));
    if let Some(result) = prefetched {
        return result;
    }
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

const DEFAULT_REMOTE_GATE_PARALLEL: usize = 4;
const MAX_REMOTE_GATE_PARALLEL: usize = 8;

/// Concurrent remote shard runs on one gate host (`RSI_REMOTE_GATE_PARALLEL`,
/// clamped 1..8, default 4). Each run keeps its fingerprinted `--jobs`, so the
/// value changes wall time, not the proof (#970).
fn remote_gate_parallelism() -> usize {
    parse_remote_gate_parallelism(std::env::var("RSI_REMOTE_GATE_PARALLEL").ok().as_deref())
}

fn parse_remote_gate_parallelism(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map_or(DEFAULT_REMOTE_GATE_PARALLEL, |value| {
            value.clamp(1, MAX_REMOTE_GATE_PARALLEL)
        })
}

/// Runs `work` over `items` on the blocking pool with at most `parallel` in
/// flight, returning results in input order.
async fn run_bounded<T, R>(
    items: Vec<T>,
    parallel: usize,
    work: impl Fn(T) -> R + Send + Sync + 'static,
) -> Result<Vec<R>, String>
where
    T: Send + 'static,
    R: Send + 'static,
{
    let semaphore = Arc::new(tokio::sync::Semaphore::new(parallel.max(1)));
    let work = Arc::new(work);
    let mut handles = Vec::with_capacity(items.len());
    for item in items {
        let permit = Arc::clone(&semaphore)
            .acquire_owned()
            .await
            .map_err(|error| format!("remote_missing_evidence: scheduler closed: {error}"))?;
        let work = Arc::clone(&work);
        handles.push(tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work(item)
        }));
    }
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        results.push(
            handle.await.map_err(|error| {
                format!("remote_missing_evidence: executor task failed: {error}")
            })?,
        );
    }
    Ok(results)
}

/// Plans every remote full shard for this gate under the executor lock
/// (bundle upload, fingerprint proof, base-versus-candidate host check), then
/// runs base and candidate shards concurrently on the gate host (#970). A
/// serial remote gate left a 32-vCPU host at load 3-4 and took as long as
/// the desktop gate. The sequential gate loop consumes these results through
/// `run_shard_or_local`, so failure comparison, isolated retries and base
/// reds are unchanged. Base results already cached for this base are not run.
async fn prefetch_remote_shards(
    candidate_worktree: &Path,
    base: &str,
    spec: &GuardSpec,
    gate: &TestGate,
) -> Result<(), String> {
    let Some(remote) = gate.remote.clone() else {
        return Ok(());
    };
    let candidate = git_text(candidate_worktree, &["rev-parse", "HEAD"])?;
    let mut sides = Vec::new();
    for command in &spec.commands {
        let Some((shard, jobs)) = full_shard(command) else {
            continue;
        };
        if !gate
            .base_cache
            .contains_key(&(base.to_owned(), format!("{command:?}")))
        {
            sides.push((base.to_owned(), shard.to_owned(), jobs, command.clone()));
        }
        sides.push((candidate.clone(), shard.to_owned(), jobs, command.clone()));
    }
    if sides.is_empty() {
        return Ok(());
    }
    // A candidate's plan compares against its base proof, so plan bases first.
    sides.sort_by_key(|(sha, ..)| sha != base);
    let mut runs = Vec::with_capacity(sides.len());
    for (sha, shard, jobs, command) in sides {
        let remote = Arc::clone(&remote);
        let candidate = candidate.clone();
        let base = base.to_owned();
        let run = tokio::task::spawn_blocking(move || {
            remote
                .lock()
                .map_err(|_| "remote_missing_evidence: executor lock poisoned".to_string())?
                .plan_full_shard(&sha, &candidate, &base, &shard, jobs, &command)
        })
        .await
        .map_err(|error| format!("remote_missing_evidence: executor task failed: {error}"))??;
        runs.push(run);
    }
    let parallel = remote_gate_parallelism();
    println!("remote_gate_prefetch={} parallel={parallel}", runs.len());
    let results = run_bounded(runs, parallel, |run| {
        (
            (run.sha().to_owned(), run.shard().to_owned()),
            run.execute(),
        )
    })
    .await?;
    for ((sha, shard), result) in &results {
        let side = if sha.as_str() == base {
            "base"
        } else {
            "candidate"
        };
        println!(
            "{}",
            test_outcome_line(side, shard, result.as_ref().map_err(String::as_str))
        );
    }
    gate.remote_results
        .lock()
        .map_err(|_| "remote_missing_evidence: prefetched results lock poisoned".to_string())?
        .extend(results);
    Ok(())
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
    if !gate.base_packages.contains_key(base) {
        let packages = base_workspace_packages(repo, base);
        gate.base_packages.insert(base.to_owned(), packages);
    }
    let base_packages = gate.base_packages.get(base).and_then(Option::as_ref);
    if let Some(package) = base_absent_package(command, base_packages) {
        // A crate the candidate adds cannot run on the base, so it has no
        // base failures. The candidate side still runs and must pass.
        gate.base_absent_packages.insert(package.to_owned());
        return Ok(BTreeSet::new());
    }
    let base_worktree = ensure_base_worktree(repo, base, gate)?;
    let report = run_shard_or_local(
        &base_worktree,
        fingerprint_source,
        base,
        spec,
        command,
        gate,
    )
    .await?;
    if let (Some(reason), Some(direct)) = (
        static_inventory_refusal(&report),
        direct_base_shard_command(command),
    ) {
        // A base whose shard inventory is statically red (for example an
        // ungated test) refuses before any test runs. That must not make the
        // fix unlandable: run the same bounded harness the script would have
        // run and compare against those real base results. The candidate
        // still runs the script and must pass its static check.
        gate.base_static_inventory_reds
            .insert(format!("{base}:{reason}"));
        let direct_report = run_one_guard(&base_worktree, spec, &direct).await?;
        return record_base_failures(gate, &direct_report);
    }
    record_base_failures(gate, &report)
}

/// Base failures of one guard report, remembering each failure's output text
/// for the known-failure annotation (#1016).
fn record_base_failures(
    gate: &mut TestGate,
    report: &rsid::integration::GuardCommandReport,
) -> Result<BTreeSet<String>, String> {
    let failures = base_failures_from_report(report)?;
    let output = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
    for name in &failures {
        gate.base_red_texts
            .entry(name.clone())
            .or_insert_with(|| failure_text_for(&output, name));
    }
    Ok(failures)
}

/// The reason the rsid shard script's static inventory check refused, when
/// that check (not a test) failed the command.
fn static_inventory_refusal(report: &rsid::integration::GuardCommandReport) -> Option<String> {
    if !matches!(report.status, rsid::integration::GuardStatus::Failed { .. }) {
        return None;
    }
    format!("{}\n{}", report.stdout_tail, report.stderr_tail)
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("rsid test shard check failed: ")
                .map(str::to_owned)
        })
}

/// The bounded feature harness `scripts/run-rsid-test-shards.sh shard` runs
/// after its static check, as a direct command for a base whose check
/// refused. `None` for any other command shape.
fn direct_base_shard_command(command: &GuardCommand) -> Option<GuardCommand> {
    if command.program != "scripts/run-rsid-test-shards.sh"
        || command.args.first().map(String::as_str) != Some("shard")
    {
        return None;
    }
    let shard = command.args.get(1)?;
    if !RSID_SHARDS.contains(&shard.as_str()) {
        return None;
    }
    let flag = |name: &str| {
        command
            .args
            .iter()
            .position(|arg| arg == name)
            .and_then(|index| command.args.get(index + 1))
            .cloned()
    };
    let mut args: Vec<String> = [
        "nextest",
        "run",
        "--profile",
        "rsid-fast",
        "-p",
        "rsid",
        "--lib",
        "--no-default-features",
        "--features",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    args.push(format!("test-shard-{shard}"));
    args.extend(["--status-level", "all", "--final-status-level", "all", "-j"].map(String::from));
    args.push(flag("--jobs").unwrap_or_else(|| "4".into()));
    if let Some(filterset) = flag("--filterset") {
        args.extend(["--filterset".into(), filterset]);
    }
    Some(GuardCommand {
        program: "cargo".into(),
        args,
        timeout: command.timeout,
    })
}

/// Nextest exits 4 with `error: no tests to run` when a filter selects no
/// tests (its default `--no-tests=fail`).
fn nextest_ran_no_tests(report: &rsid::integration::GuardCommandReport) -> bool {
    report.status == rsid::integration::GuardStatus::Failed { code: Some(4) }
        && format!("{}\n{}", report.stdout_tail, report.stderr_tail)
            .lines()
            .any(|line| line.trim() == "error: no tests to run")
}

/// A focused filter can select only tests the candidate adds, so the base
/// runs none and nextest refuses. The base then has no failures to compare
/// against; the candidate side still runs the filter and must pass through
/// `observed_test_failures`, which keeps treating "no tests" as a failure.
fn base_failures_from_report(
    report: &rsid::integration::GuardCommandReport,
) -> Result<BTreeSet<String>, String> {
    if nextest_ran_no_tests(report) {
        return Ok(BTreeSet::new());
    }
    observed_test_failures(report)
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
    // Cheap static guards (git diff --check) run before any remote or local
    // build or test time is spent (#1052).
    for command in spec
        .commands
        .iter()
        .filter(|command| is_static_guard(command))
    {
        let report = run_one_guard(candidate_worktree, spec, command).await?;
        if report.status != rsid::integration::GuardStatus::Passed {
            return Err(format!(
                "static guard failed before any test ran ({:?}) running {} {:?}: {} {}",
                report.status, report.program, report.args, report.stdout_tail, report.stderr_tail
            ));
        }
    }
    if !gate.skip_tests {
        prefetch_remote_shards(candidate_worktree, base, spec, gate).await?;
    }
    for command in spec
        .commands
        .iter()
        .filter(|command| !is_static_guard(command))
    {
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
                let env_class = shard_env_class(
                    &scratch_spec.env,
                    shard_tmpdir_class(gate.scratch.as_ref(), command, disk_scratch_shards),
                );
                let slot_key = shard_cache_key(&fingerprint, &env_class)?;
                // The holder runs one base guard (bounded by guard_timeout), so a wait
                // beyond twice that means the holder is wedged: refuse, do not hang.
                let slot = base_cache::BaseShardSlot::acquire_within(
                    root,
                    base,
                    shard,
                    &slot_key,
                    guard_timeout() * 2,
                )
                .await?;
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
                        &slot_key,
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
        let shard_name = full_shard(command)
            .map(|(shard, _)| shard.to_owned())
            .unwrap_or_else(|| format!("{} {:?}", command.program, command.args));
        println!(
            "{}",
            test_outcome_line("candidate", &shard_name, Ok(&report))
        );
        let candidate_failures = observed_test_failures(&report)?;
        let new_failures: BTreeSet<_> = candidate_failures
            .difference(&base_failures)
            .cloned()
            .collect();
        for name in &new_failures {
            let retry =
                run_isolated_retry(candidate_worktree, &scratch_spec, command, name).await?;
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
                        &bounded_isolated_retry_command(command, name)?,
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
                            return Err(new_failures_message(
                                base,
                                std::slice::from_ref(name),
                                &format!(
                                    "QA cached base {provenance}; isolated local base passed {name}"
                                ),
                            ));
                        }
                        Err(error) => {
                            return Err(format!(
                                "QA cached base {provenance}; local base confirmation failed closed for {name}: {error}"
                            ));
                        }
                    }
                }
                let retry_output = format!("{}\n{}", retry.stdout_tail, retry.stderr_tail);
                let failing = persistent
                    .iter()
                    .map(|name| {
                        format!(
                            "{name}{}",
                            known_red_suffix(
                                name,
                                &failure_text_for(&retry_output, name),
                                gate.known_failures.as_ref(),
                            )
                        )
                    })
                    .collect::<Vec<_>>();
                let evidence = retry_failure_evidence(&retry);
                return Err(new_failures_message(
                    base,
                    &failing,
                    &format!(
                        "base reds: {}{}",
                        base_failures.iter().cloned().collect::<Vec<_>>().join(", "),
                        if evidence.is_empty() {
                            String::new()
                        } else {
                            format!("; isolated retry evidence: {evidence}")
                        }
                    ),
                ));
            }
            gate.flakes.insert(name.clone());
        }
    }
    Ok(())
}

/// A guard that only inspects git objects: it needs no build and no test
/// host, so it runs before any of them.
fn is_static_guard(command: &GuardCommand) -> bool {
    command.program == "git"
}

/// One log line per test shard outcome, so a refusal that lands after the
/// tests ran still records what each shard did (#1052).
fn test_outcome_line(
    side: &str,
    shard: &str,
    result: Result<&rsid::integration::GuardCommandReport, &str>,
) -> String {
    match result {
        Ok(report) => {
            let verdict = if report.status == rsid::integration::GuardStatus::Passed {
                "pass"
            } else {
                "fail"
            };
            format!("test_outcome shard={shard} side={side} result={verdict}")
        }
        Err(error) => {
            let first = error.lines().next().unwrap_or_default();
            format!("test_outcome shard={shard} side={side} result=error detail={first}")
        }
    }
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
        // A test filter may name any workspace package: a reader of a changed
        // doc or script can live in a crate the diff did not touch.
        if !workspace_has_package(metadata, package) || value.is_empty() || value.starts_with('-') {
            return Err(format!("invalid test filter: {filter}"));
        }
        selected.entry(package).or_default().push(value);
    }
    let touched_packages = packages;
    let mut gated: Vec<String> = packages.to_vec();
    for package in selected.keys() {
        if !gated.iter().any(|name| name == package) {
            gated.push((*package).to_owned());
        }
    }
    let packages = gated.as_slice();
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
        if let Some(package_filters) = selected.get(package.as_str()) {
            // Operator P0 (2026-09-29): a --test-filter scopes the package's
            // test gate to the modules the source touched; the QA sweep of
            // rolling runs the rest. Every target of the package must still
            // compile.
            let check = cargo_check_command(package);
            if !commands.contains(&check) {
                commands.push(check);
            }
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
        } else if package == "rsid" {
            // Unfiltered: without a proven file-to-shard map, gate all
            // shards for an rsid change.
            for shard in RSID_SHARDS {
                commands.push(rsid_shard_guard_command(shard)?);
            }
        } else {
            commands.push(cargo_guard_command(package, None));
        }
    }
    for dependent in reverse_workspace_dependents(metadata, touched_packages)? {
        let check = cargo_check_command(&dependent);
        if !commands.contains(&check) {
            commands.push(check);
        }
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

fn workspace_has_package(metadata: &WorkspaceMetadata, name: &str) -> bool {
    let members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    metadata
        .packages
        .iter()
        .any(|package| package.name == name && members.contains(package.id.as_str()))
}

fn cargo_guard_command(package: &str, filter: Option<&str>) -> GuardCommand {
    let mut args = vec!["test".into(), "-p".into(), package.into()];
    // `bin:NAME` and `test:NAME` select a cargo target (rsid main.rs bin
    // tests, an integration test binary) instead of a lib test filter.
    match filter.and_then(|filter| {
        filter
            .strip_prefix("bin:")
            .map(|name| ("--bin", name))
            .or_else(|| filter.strip_prefix("test:").map(|name| ("--test", name)))
    }) {
        Some((flag, name)) => args.extend([flag.into(), name.into()]),
        None => {
            args.push("--lib".into());
            if let Some(filter) = filter {
                args.push(filter.into());
            }
        }
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
            &original.oid,
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
    original_oid: &str,
    fetched_tip: &str,
    observed: &str,
) -> Result<String, String> {
    let patch = git_output(repo, &["diff", "--binary", original_oid, fetched_tip, "--"])?;
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
    publish_oid(repo, &candidate.oid, fetched_tip).await
}

/// Fast-forward-only publication of one commit over `fetched_tip`.
async fn publish_oid(repo: &Path, oid: &str, fetched_tip: &str) -> Result<String, LandFailure> {
    let observed = remote_tip(repo, "publish").await.map_err(|error| {
        LandFailure::oid(PublicationState::NotPublished, error, oid, fetched_tip)
    })?;
    if observed.as_deref() != Some(fetched_tip) {
        let mut failure = LandFailure::oid(
            PublicationState::NotPublished,
            format!(
                "stale target before push: fetched {fetched_tip}, remote now resolves to {observed:?}"
            ),
            oid,
            fetched_tip,
        );
        failure.observed_tip = observed;
        return Err(failure);
    }

    // A normal push has Git's default fast-forward-only behavior. The full
    // candidate OID is the source, so no branch name or force refspec is used.
    let refspec = format!("{oid}:refs/heads/rolling");
    let mut push = remote_git_command(repo, &["push", "--porcelain", "publish", &refspec]);
    push.env("RSI_ROLLING_LANDER", "1");
    push.stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut push = push.spawn().map_err(|error| {
        LandFailure::oid(
            PublicationState::NotPublished,
            format!("cannot push candidate to remote: {error}"),
            oid,
            fetched_tip,
        )
    })?;
    let push_pid = child_pid(&push, "push candidate to remote")
        .map_err(|error| LandFailure::oid(PublicationState::Unknown, error, oid, fetched_tip))?;
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
    confirm_post_push(repo, oid, fetched_tip, &context, reported_success).await
}

async fn confirm_post_push(
    repo: &Path,
    oid: &str,
    fetched_tip: &str,
    context: &str,
    reported_success: bool,
) -> Result<String, LandFailure> {
    match remote_tip(repo, "publish").await {
        Ok(Some(tip)) if tip == oid => Ok(tip),
        Ok(Some(tip)) if tip == fetched_tip && !reported_success => Err(LandFailure::oid(
            PublicationState::NotPublished,
            format!("{context}; remote rolling remained at {fetched_tip}"),
            oid,
            fetched_tip,
        )),
        Ok(Some(tip)) if !reported_success && tip != fetched_tip => {
            if remote_fetch(repo).await.is_ok()
                && git_text(repo, &["rev-parse", "refs/heads/rolling"])
                    .ok()
                    .as_deref()
                    == Some(tip.as_str())
                && git_is_ancestor(repo, fetched_tip, &tip).unwrap_or(false)
                && !git_is_ancestor(repo, oid, &tip).unwrap_or(true)
            {
                let mut failure = LandFailure::oid(
                    PublicationState::NotPublished,
                    format!("{context}; stale target advanced to {tip} before publication"),
                    oid,
                    fetched_tip,
                );
                failure.observed_tip = Some(tip);
                return Err(failure);
            }
            let mut failure = LandFailure::oid(
                PublicationState::Unknown,
                format!("{context}; remote rolling advanced to {tip}, publication is unconfirmed"),
                oid,
                fetched_tip,
            );
            failure.observed_tip = Some(tip);
            Err(failure)
        }
        Ok(observed) => {
            let mut failure = LandFailure::oid(
                PublicationState::Unknown,
                format!(
                    "{context}; remote rolling resolves to {observed:?}, publication is unconfirmed"
                ),
                oid,
                fetched_tip,
            );
            failure.observed_tip = observed;
            Err(failure)
        }
        Err(error) => Err(LandFailure::oid(
            PublicationState::Unknown,
            format!("{context}; remote verification failed: {error}"),
            oid,
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

    /// The environment lock, for sibling modules' tests that change the
    /// process environment.
    pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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

    #[test]
    fn cargo_test_package_reads_only_cargo_test_guards() {
        assert_eq!(
            cargo_test_package(&cargo_guard_command("rsi-remote", None)),
            Some("rsi-remote")
        );
        assert_eq!(
            cargo_test_package(&cargo_guard_command("rsi-remote", Some("tests"))),
            Some("rsi-remote")
        );
        assert_eq!(
            cargo_test_package(&rsid_shard_guard_command("store-04").unwrap()),
            None
        );
        assert_eq!(cargo_test_package(&cargo_check_command("rsi")), None);
        let diff_check = GuardCommand {
            program: "git".into(),
            args: vec!["diff".into(), "--check".into(), "a".into(), "b".into()],
            timeout: Duration::from_secs(30),
        };
        assert_eq!(cargo_test_package(&diff_check), None);
    }

    #[test]
    fn base_absent_package_requires_proven_base_membership() {
        let command = cargo_guard_command("rsi-remote", None);
        let without = BTreeSet::from(["rsid".to_owned(), "rsi-common".to_owned()]);
        let with = BTreeSet::from(["rsid".to_owned(), "rsi-remote".to_owned()]);
        assert_eq!(
            base_absent_package(&command, Some(&without)),
            Some("rsi-remote")
        );
        assert_eq!(base_absent_package(&command, Some(&with)), None);
        assert_eq!(base_absent_package(&command, None), None);
        let shard = rsid_shard_guard_command("store-04").unwrap();
        assert_eq!(base_absent_package(&shard, Some(&without)), None);
    }

    #[test]
    fn base_workspace_packages_reads_members_from_git_objects() {
        let fixture = Fixture::new();
        assert_eq!(
            base_workspace_packages(&fixture.repo, &fixture.base),
            Some(BTreeSet::from(["demo".to_owned()]))
        );
        let glob = fixture.commit(
            &fixture.base,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
            "glob members",
        );
        assert_eq!(base_workspace_packages(&fixture.repo, &glob), None);
    }

    #[tokio::test]
    async fn paired_test_gate_runs_new_workspace_crate_on_candidate_only() {
        for candidate_fails in [false, true] {
            let fixture = Fixture::new();
            let manifest = fixture.commit(
                &fixture.base,
                "crates/fresh/Cargo.toml",
                "[package]\nname = \"fresh\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                "fresh manifest",
            );
            let candidate = fixture.commit(
                &manifest,
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/demo\", \"crates/fresh\"]\nresolver = \"2\"\n",
                "fresh member",
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
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s %s\\n' \"$oid\" \"$3\" >> '{log}'\nif [ \"$oid\" = '{candidate}' ] && [ \"$3\" = fresh ] && [ '{candidate_fails}' = true ]; then echo 'test fresh::red ... FAILED'; exit 1; fi",
                log = log.display(),
            ));
            let spec = GuardSpec {
                commands: vec![
                    cargo_guard_command("demo", None),
                    cargo_guard_command("fresh", None),
                ],
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
            let runs = fs::read_to_string(&log).unwrap();
            let count = |line: String| runs.lines().filter(|run| *run == line).count();
            assert_eq!(
                gate.base_absent_packages,
                BTreeSet::from(["fresh".to_owned()])
            );
            assert_eq!(count(format!("{} demo", fixture.base)), 1);
            assert_eq!(count(format!("{candidate} demo")), 1);
            assert_eq!(
                runs.lines()
                    .filter(|run| run.starts_with(fixture.base.as_str()))
                    .count(),
                1
            );
            if candidate_fails {
                assert!(
                    result
                        .unwrap_err()
                        .contains("new test failures relative to rolling")
                );
                assert_eq!(count(format!("{candidate} fresh")), 2);
            } else {
                result.expect("a passing new crate is gated on the candidate only");
                assert_eq!(count(format!("{candidate} fresh")), 1);
            }
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
                "#!/bin/sh\noid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> \"$RSI_QA899_RUN_LOG\"\nif [ -f rsi-shard-fail-marker ] || [ \"$oid\" = \"$RSI_QA899_FAIL_ON_OID\" ]; then echo 'test demo::red ... FAILED'; exit 1; fi\nexit 0\n",
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
            self.add_fake_canary_cargo_when(
                &format!(
                    "[ \"$remote\" != '{prepublish_tip}' ] && [ \"$head\" != '{prepublish_tip}' ]"
                ),
                action,
            );
        }

        /// A fake cargo whose `action` runs when the shell condition holds;
        /// `$head` is the worktree commit under test and `$remote` the bare
        /// repository's current `rolling`.
        fn add_fake_canary_cargo_when(&self, condition: &str, action: &str) {
            self.add_fake_cargo(&format!(
                "remote=$(/usr/bin/git --git-dir='{}' rev-parse refs/heads/rolling)\nhead=$(/usr/bin/git rev-parse HEAD)\nif {condition}; then\n{action}\nfi",
                self.bare.display(),
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
                    expected_tip: None,
                },
                TestGate {
                    skip_tests: true,
                    ..TestGate::default()
                },
            )
            .await
        }

        async fn land_expecting(
            &self,
            accepted: Vec<AcceptedPair>,
            expected_tip: &str,
        ) -> Result<LandReport, LandFailure> {
            land(Options {
                repo: self.repo.clone(),
                remote: "origin".into(),
                accepted,
                test_filters: Vec::new(),
                cargo_build_jobs: 1,
                remote_gate: None,
                tmpfs_min_free_gb: 12,
                disk_scratch_shards: BTreeSet::new(),
                expected_tip: Some(expected_tip.to_string()),
            })
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
                expected_tip: None,
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

    /// The base-cache key `run_affected_gate` derives for a full shard
    /// command with no gate scratch, so tests seed entries at that exact key.
    fn spec_shard_cache_key(spec: &GuardSpec, fingerprint: &str) -> String {
        let command = spec.commands.first().expect("spec has a shard command");
        shard_cache_key(
            fingerprint,
            &shard_env_class(
                &spec.env,
                shard_tmpdir_class(None, command, &BTreeSet::new()),
            ),
        )
        .expect("spec env yields a cache key")
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

    /// Commit an executable file (the fake shard script) on top of `parent`.
    fn commit_executable(fixture: &Fixture, parent: &str, file: &str, content: &str) -> String {
        let path = fixture
            .root
            .path()
            .join(format!("builder-{}", uuid::Uuid::new_v4()));
        git_run(
            &fixture.repo,
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
        git_run(&path, &["update-index", "--chmod=+x", file]);
        git_run(&path, &["commit", "-q", "-m", "fake shard script"]);
        let oid = git_value(&path, &["rev-parse", "HEAD"]);
        git_run(
            &fixture.repo,
            &["worktree", "remove", path.to_str().unwrap()],
        );
        oid
    }

    /// A base whose shard script refuses its static inventory (an ungated
    /// test) cannot make the fix unlandable. The gate runs the same bounded
    /// harness directly on the base, compares the candidate against those real
    /// results and reports `base_static_inventory_red`. A candidate that is
    /// itself statically red is still refused.
    #[tokio::test]
    async fn base_static_inventory_red_uses_direct_base_harness() {
        const REFUSAL: &str = "demo.rs:9: ungated test blocks a bounded lib harness";
        let red_script =
            format!("#!/bin/sh\necho 'rsid test shard check failed: {REFUSAL}' >&2\nexit 1\n");
        let green_script = "#!/bin/sh\nexec cargo nextest run --via-shard-script \"$@\"\n";
        // The candidate's red script differs from the base's only by this
        // marker line, so the candidate commit has a change to record.
        let candidate_red_script = format!("{red_script}# candidate\n");
        for candidate_green in [true, false] {
            let fixture = Fixture::new();
            let script = "scripts/run-rsid-test-shards.sh";
            let base = commit_executable(&fixture, &fixture.base, script, &red_script);
            let candidate = commit_executable(
                &fixture,
                &base,
                script,
                if candidate_green {
                    green_script
                } else {
                    &candidate_red_script
                },
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
            // The base's direct harness reports one baseline red; the
            // candidate's script run is green.
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s %s\\n' \"$oid\" \"$*\" >> '{log}'\nif [ \"$oid\" = '{base}' ]; then echo '        FAIL [   0.100s] (1/2) rsid demo::red'; exit 100; fi",
                log = log.display(),
                base = base,
            ));
            let spec = GuardSpec {
                commands: vec![rsid_shard_guard_command("store-01").unwrap()],
                env: BTreeMap::new(),
                output_tail_bytes: 16 * 1024,
            };
            let mut gate = TestGate::default();
            let old_path = use_fake_path(&fixture);
            let result = run_affected_gate(
                &fixture.repo,
                &candidate_tree,
                &base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            restore_path(old_path);
            let runs = fs::read_to_string(&log).unwrap_or_default();
            let base_direct = runs
                .lines()
                .find(|line| line.starts_with(&base))
                .expect("the base ran the direct bounded harness");
            assert!(
                base_direct.contains("nextest run --profile rsid-fast -p rsid --lib")
                    && base_direct.contains("--features test-shard-store-01")
                    && base_direct.contains("-j 4"),
                "{base_direct}"
            );
            assert!(
                gate.base_static_inventory_reds
                    .contains(&format!("{base}:{REFUSAL}"))
            );
            if candidate_green {
                result.expect("a statically red base must not refuse a green candidate");
                assert!(gate.base_reds.contains("demo::red"));
            } else {
                assert!(
                    result.unwrap_err().contains("rsid test shard check failed"),
                    "a statically red candidate is still refused"
                );
            }
        }
    }

    #[test]
    fn direct_base_shard_command_mirrors_the_shard_script_harness() {
        let plain =
            direct_base_shard_command(&rsid_shard_guard_command("session-05").unwrap()).unwrap();
        assert_eq!(plain.program, "cargo");
        assert_eq!(
            plain.args.join(" "),
            "nextest run --profile rsid-fast -p rsid --lib --no-default-features --features \
             test-shard-session-05 --status-level all --final-status-level all -j 4"
        );
        let filtered = direct_base_shard_command(
            &rsid_shard_guard_command("store-01:test(demo_filter)").unwrap(),
        )
        .unwrap();
        assert!(
            filtered
                .args
                .ends_with(&["--filterset".to_string(), "test(demo_filter)".to_string()])
        );
        assert!(direct_base_shard_command(&cargo_guard_command("demo", None)).is_none());
    }

    /// A base red named in a snapshot prints one `known_red=` annotation; an
    /// absent snapshot annotates nothing.
    #[tokio::test]
    async fn base_red_is_annotated_from_the_known_failure_snapshot() {
        // Fixture::new holds ENV_LOCK for the test; taking it again deadlocks.
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
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\nif [ \"$oid\" = '{base}' ]; then echo 'test demo::red ... FAILED'; exit 1; fi",
            base = fixture.base,
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
        result.expect("base red is allowed");
        assert!(gate.base_reds.contains("demo::red"));

        let snapshot: Snapshot = serde_json::from_str(
            r#"{"schema_version":1,"exported_at":"2026-09-28T00:00:00Z","records":[{"record":{"test_id":"demo::red","matcher":{"contains":["demo::red"]},"issue":77,"class":"regression"},"issue_status":"Open"}]}"#,
        )
        .expect("snapshot");
        assert_eq!(
            known_red_lines(&gate.base_reds, &gate.base_red_texts, Some(&snapshot)),
            vec!["known_red=demo::red:#77:regression".to_string()]
        );
        assert!(known_red_lines(&gate.base_reds, &gate.base_red_texts, None).is_empty());
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
        let qa_key = spec_shard_cache_key(&spec, &fingerprint);
        let qa_root = fixture.root.path().join("qa-cache");
        let slot = base_cache::BaseShardSlot::acquire(&qa_root, &fixture.base, "store-01", &qa_key)
            .await
            .unwrap();
        slot.write(&base_cache::BaseShardEntry::new(
            &fixture.base,
            "store-01",
            &qa_key,
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

        let candidate_red_spec = spec.clone();
        // Fail the candidate shard through a worktree marker rather than a
        // spec env entry: a changed environment class is itself a cache miss,
        // and this case must exercise reuse of the same-env base result.
        let candidate_marker = candidate_tree.join("rsi-shard-fail-marker");
        fs::write(&candidate_marker, "candidate only\n").unwrap();
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
        fs::remove_file(&candidate_marker).unwrap();
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

    /// #988: a base result measured under one gate environment is never
    /// reused for a full shard that sees another. The key folds the shard
    /// fingerprint with the environment class: the scratch filesystem class
    /// plus the sorted spec env, with the per-run scratch path excluded.
    #[test]
    fn shard_cache_key_folds_the_gate_environment_class() {
        let fingerprint = format!("sha256:{}", "a".repeat(64));
        let shard = rsid_shard_guard_command("store-01").unwrap();
        let marked = BTreeSet::from(["store-01".to_string()]);
        let env = BTreeMap::from([
            ("CARGO_BUILD_JOBS".to_string(), "8".to_string()),
            (
                "CARGO_PROFILE_DEV_DEBUG".to_string(),
                "line-tables-only".to_string(),
            ),
            (
                "TMPDIR".to_string(),
                "/run/user/1000/rsi-landing-gate-aaa".to_string(),
            ),
        ]);
        let tmpfs_scratch = GateScratch::Tmpfs(tempfile::tempdir().unwrap());
        let disk_scratch = GateScratch::Disk(std::env::temp_dir().join("rsi-disk-gate"));
        assert_eq!(
            shard_tmpdir_class(Some(&tmpfs_scratch), &shard, &BTreeSet::new()),
            "tmpfs"
        );
        assert_eq!(
            shard_tmpdir_class(Some(&disk_scratch), &shard, &BTreeSet::new()),
            "disk"
        );
        // A marked disk-scratch shard gets disk scratch even under a tmpfs gate.
        assert_eq!(
            shard_tmpdir_class(Some(&tmpfs_scratch), &shard, &marked),
            "disk"
        );
        assert_eq!(shard_tmpdir_class(None, &shard, &BTreeSet::new()), "disk");

        let tmpfs_key = shard_cache_key(
            &fingerprint,
            &shard_env_class(
                &env,
                shard_tmpdir_class(Some(&tmpfs_scratch), &shard, &BTreeSet::new()),
            ),
        )
        .unwrap();
        let disk_key = shard_cache_key(
            &fingerprint,
            &shard_env_class(
                &env,
                shard_tmpdir_class(Some(&disk_scratch), &shard, &BTreeSet::new()),
            ),
        )
        .unwrap();
        assert_ne!(
            tmpfs_key, disk_key,
            "a tmpfs base result must not serve a disk-scratch candidate"
        );
        assert_ne!(
            disk_key, tmpfs_key,
            "a disk base result must not serve a tmpfs-scratch candidate"
        );

        // The same env and class is the same key though the concrete scratch
        // path moved with the run.
        let moved = BTreeMap::from([
            ("CARGO_BUILD_JOBS".to_string(), "8".to_string()),
            (
                "CARGO_PROFILE_DEV_DEBUG".to_string(),
                "line-tables-only".to_string(),
            ),
            (
                "TMPDIR".to_string(),
                "/run/user/1000/rsi-landing-gate-bbb".to_string(),
            ),
        ]);
        assert_eq!(
            shard_cache_key(&fingerprint, &shard_env_class(&moved, "tmpfs")).unwrap(),
            tmpfs_key
        );
        // A changed job count or shard fingerprint is another measurement.
        let mut more_jobs = env.clone();
        more_jobs.insert("CARGO_BUILD_JOBS".into(), "16".into());
        assert_ne!(
            shard_cache_key(&fingerprint, &shard_env_class(&more_jobs, "tmpfs")).unwrap(),
            tmpfs_key
        );
        assert_ne!(
            shard_cache_key(
                &format!("sha256:{}", "b".repeat(64)),
                &shard_env_class(&env, "tmpfs")
            )
            .unwrap(),
            tmpfs_key
        );
        assert!(shard_cache_key("not-a-digest", &shard_env_class(&env, "tmpfs")).is_err());
    }

    /// #994: scripts/import-rolling-qa-cache.py mirrors `shard_env_class` and
    /// `shard_cache_key`, so a QA sweep result imported under the environment
    /// it ran in lands at the key a landing in that environment looks up.
    /// scripts/tests/test_import_rolling_qa_cache.py pins the same vectors for
    /// the gate's default spec env; change both sides together.
    #[test]
    fn shard_cache_key_matches_the_qa_importer_vectors() {
        let fingerprint = format!("sha256:{}", "a".repeat(64));
        let env = BTreeMap::from([
            (
                "CARGO_BUILD_JOBS".to_string(),
                parse_cargo_build_jobs(None).unwrap().to_string(),
            ),
            (
                "CARGO_PROFILE_DEV_DEBUG".to_string(),
                "line-tables-only".to_string(),
            ),
            (
                "TMPDIR".to_string(),
                "/dev/shm/rsi-landing-gate-x".to_string(),
            ),
        ]);
        for (class, env_class, key) in [
            (
                "tmpfs",
                "sha256:d5da322b638d61ee4a9b2061389d35f57037e881a38e64fb305942e8e63636b9",
                "sha256:5c13edf5b414383bf368da17364ebf409c1d775cce03f5fb9aabc1ce4f25a6ea",
            ),
            (
                "disk",
                "sha256:a68fc171a80ec57da8f351dcf6d5c0edbcdb4f1e682d5508514a59ebdfc645d5",
                "sha256:e5c3dd61019cd7a754751ebca2e9a3f6473c53d1a3aa8a77ee0f9bd90b4c84cc",
            ),
        ] {
            assert_eq!(shard_env_class(&env, class), env_class, "{class}");
            assert_eq!(
                shard_cache_key(&fingerprint, env_class).unwrap(),
                key,
                "{class}"
            );
        }
    }

    /// #988: the same full shard cached from a tmpfs-scratch gate is a miss
    /// for a disk-scratch candidate, and its base is measured again there.
    #[tokio::test]
    async fn base_cached_under_tmpfs_scratch_is_a_miss_for_a_disk_scratch_shard() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let candidate_tree = fixture.root.path().join("scratch-class-candidate");
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
        let log = fixture.root.path().join("scratch-class-shard.log");
        let spec = GuardSpec {
            commands: vec![rsid_shard_guard_command("store-01").unwrap()],
            env: BTreeMap::from([
                ("RSI_QA899_RUN_LOG".into(), log.display().to_string()),
                ("RSI_QA899_FAIL_ON_OID".into(), "none".into()),
            ]),
            output_tail_bytes: 16 * 1024,
        };
        let cache_root = fixture.root.path().join("scratch-class-cache");
        let target = fixture.root.path().join("target");
        fs::create_dir(&target).unwrap();
        let mut tmpfs_gate = TestGate {
            cache_root: Some(cache_root.clone()),
            scratch: Some(GateScratch::Tmpfs(
                tempfile::tempdir_in(fixture.root.path()).unwrap(),
            )),
            ..TestGate::default()
        };
        run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &spec,
            &mut tmpfs_gate,
            &BTreeSet::new(),
        )
        .await
        .expect("tmpfs-scratch gate");
        let base_tree = tmpfs_gate
            .base_worktrees
            .get(&fixture.base)
            .cloned()
            .expect("tmpfs gate created the base worktree");
        git_run(
            &fixture.repo,
            &["worktree", "remove", base_tree.to_str().unwrap()],
        );
        let disk_scratch = target.join(".rsi-tmp");
        fs::create_dir_all(&disk_scratch).unwrap();
        let mut disk_gate = TestGate {
            cache_root: Some(cache_root),
            scratch: Some(GateScratch::Disk(disk_scratch)),
            ..TestGate::default()
        };
        run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &spec,
            &mut disk_gate,
            &BTreeSet::from(["store-01".to_string()]),
        )
        .await
        .expect("disk-scratch gate");
        let runs = fs::read_to_string(&log).unwrap();
        assert_eq!(
            runs.lines().filter(|oid| *oid == fixture.base).count(),
            2,
            "the disk-scratch candidate must not reuse the tmpfs base result"
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
            // The QA entry is keyed by the environment class of this mode.
            let qa_key = spec_shard_cache_key(&spec, &fingerprint);
            let slot =
                base_cache::BaseShardSlot::acquire(&cache_root, &fixture.base, "store-01", &qa_key)
                    .await
                    .unwrap();
            slot.write(&base_cache::BaseShardEntry::new(
                &fixture.base,
                "store-01",
                &qa_key,
                BTreeSet::new(),
                "qa:same-host:QA.json",
            ))
            .unwrap();
            drop(slot);
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
        let last_key = spec_shard_cache_key(&spec, &fingerprint);
        let slot =
            base_cache::BaseShardSlot::acquire(&cache_root, &fixture.base, "store-01", &last_key)
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
        let log = fixture.root.path().join("class-shard-runs.log");
        let spec = GuardSpec {
            commands: vec![rsid_shard_guard_command("store-01").unwrap()],
            env: BTreeMap::from([
                ("RSI_QA899_RUN_LOG".into(), log.display().to_string()),
                ("RSI_QA899_FAIL_ON_OID".into(), "none".into()),
            ]),
            output_tail_bytes: 16 * 1024,
        };
        let desktop_slot_fingerprint = spec_shard_cache_key(&spec, &desktop_fingerprint);
        let slot = base_cache::BaseShardSlot::acquire(
            &cache_root,
            &fixture.base,
            "store-01",
            &desktop_slot_fingerprint,
        )
        .await
        .unwrap();
        slot.write(&base_cache::BaseShardEntry::new(
            &fixture.base,
            "store-01",
            &desktop_slot_fingerprint,
            BTreeSet::new(),
            "qa:desktop:QA.json",
        ))
        .unwrap();
        drop(slot);
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
    async fn test_filter_scopes_the_gate_to_the_named_tests() {
        // Operator P0 (2026-09-29): a filter is the package's test gate; the
        // QA sweep of rolling runs the rest. A red in the named tests blocks.
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
        let pair = AcceptedPair {
            base: fixture.base.clone(),
            source: source.clone(),
        };
        let named = fixture
            .land_with_filters(vec![pair.clone()], vec!["demo=demo::regression".into()])
            .await;
        let scoped = match &named {
            Err(_) => Some(
                fixture
                    .land_with_filters(vec![pair], vec!["demo=unrelated_green_test".into()])
                    .await,
            ),
            Ok(_) => None,
        };
        restore_path(old_path);
        let failure = named.expect_err("a new red in the named tests blocks the landing");
        assert_eq!(failure.state, PublicationState::NotPublished);
        assert!(failure.message.contains("demo::regression"), "{failure:?}");
        let report = scoped
            .expect("second landing ran")
            .expect("a scoped gate publishes without the unrelated tests");
        assert_eq!(report.published_tip, source);
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
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

    /// #963: a shard that fails only through nextest timeouts or signals
    /// names those tests, so the gate isolates and compares them instead of
    /// refusing the whole shard as unparseable.
    #[test]
    fn nextest_timeouts_and_signals_are_retryable_failures() {
        let output = concat!(
            "        PASS [   2.022s] ( 1/409) rsid store::tests::passed\n",
            "        SLOW [ 120.576s] ( 2/409) rsid store::tests::slow_but_passed\n",
            "        SKIP [         ] (───────) rsid store::tests::skipped\n",
            "     TIMEOUT [ 244.093s] (176/409) rsid store::tests::h1_trrev_v88_timed_out\n",
            "     SIGSEGV [   0.512s] (177/409) rsid store::tests::crashed\n",
            "   LEAK-FAIL [   1.000s] (178/409) rsid store::tests::leaked\n",
            "     Summary [1815.647s] 409 tests run: 405 passed (1 slow), 1 timed out, 5 skipped\n",
        );
        let expected = BTreeSet::from([
            "store::tests::crashed".to_string(),
            "store::tests::h1_trrev_v88_timed_out".to_string(),
            "store::tests::leaked".to_string(),
        ]);
        assert_eq!(test_failure_names(output), expected);
        let report = rsid::integration::GuardCommandReport {
            program: "scripts/run-rsid-test-shards.sh".into(),
            args: vec!["shard".into(), "store-01".into()],
            status: rsid::integration::GuardStatus::Failed { code: Some(100) },
            stdout_tail: output.into(),
            stderr_tail: "error: test run failed\n".into(),
            output_truncated: false,
            duration: Duration::from_secs(1815),
        };
        assert_eq!(observed_test_failures(&report).unwrap(), expected);
        let retry = isolated_retry_command(
            &rsid_shard_guard_command("store-01").unwrap(),
            "store::tests::h1_trrev_v88_timed_out",
        )
        .unwrap();
        assert_eq!(retry.program, "cargo");
        assert!(
            retry
                .args
                .contains(&"store::tests::h1_trrev_v88_timed_out".to_string())
        );
    }

    /// #969: local gate builds and tests of a remote-gate run take a desktop
    /// build slot through the wrapper; git checks and unset wrappers do not.
    #[test]
    fn local_gate_commands_take_a_build_slot_only_when_a_wrapper_is_set() {
        let cargo = GuardCommand {
            program: "cargo".into(),
            args: vec!["test".into(), "-p".into(), "demo".into(), "--lib".into()],
            timeout: Duration::from_secs(60),
        };
        assert_eq!(slot_wrapped_command(&cargo, None), cargo);
        assert_eq!(
            slot_wrapped_command(&cargo, Some(std::ffi::OsStr::new(""))),
            cargo
        );
        let wrapped = slot_wrapped_command(
            &cargo,
            Some(std::ffi::OsStr::new("/home/op/.rsi/bin/cargo-slot")),
        );
        assert_eq!(wrapped.program, "/home/op/.rsi/bin/cargo-slot");
        assert_eq!(wrapped.args, ["cargo", "test", "-p", "demo", "--lib"]);
        assert_eq!(wrapped.timeout, cargo.timeout);
        let shard = rsid_shard_guard_command("store-01:test(one_test)").unwrap();
        let wrapped_shard =
            slot_wrapped_command(&shard, Some(std::ffi::OsStr::new("/usr/local/bin/slot")));
        assert_eq!(wrapped_shard.args[0], "scripts/run-rsid-test-shards.sh");
        assert_eq!(&wrapped_shard.args[1..], shard.args.as_slice());
        let git = GuardCommand {
            program: "git".into(),
            args: vec!["diff".into(), "--check".into()],
            timeout: Duration::from_secs(30),
        };
        assert_eq!(
            slot_wrapped_command(&git, Some(std::ffi::OsStr::new("/usr/local/bin/slot"))),
            git
        );
    }

    #[tokio::test]
    async fn slot_wrapped_local_command_reports_the_gate_command() {
        let fixture = Fixture::new();
        let wrapper = fixture.bin.join("slot-wrapper");
        let marker = fixture.root.path().join("wrapper-ran");
        fs::write(
            &wrapper,
            format!("#!/bin/sh\n: > '{}'\nexec \"$@\"\n", marker.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wrapper, permissions).unwrap();
        fixture.add_fake_cargo("echo 'test demo::ok ... ok'");
        let old_path = use_fake_path(&fixture);
        unsafe { std::env::set_var("RSI_LANDER_LOCAL_SLOT_WRAPPER", &wrapper) };
        let command = cargo_guard_command("demo", None);
        let report = run_one_guard(
            &fixture.repo,
            &GuardSpec {
                commands: Vec::new(),
                env: BTreeMap::new(),
                output_tail_bytes: 64 * 1024,
            },
            &command,
        )
        .await;
        unsafe { std::env::remove_var("RSI_LANDER_LOCAL_SLOT_WRAPPER") };
        restore_path(old_path);
        let report = report.unwrap();
        assert!(
            marker.exists(),
            "the local command ran through the slot wrapper"
        );
        assert_eq!(report.status, rsid::integration::GuardStatus::Passed);
        assert_eq!(report.program, "cargo");
        assert_eq!(report.args, command.args);
    }

    /// #988: the lander's own daemon and custody environment must not reach
    /// the guarded test binaries, or base and candidate would not see the
    /// same environment. `RSI_*` keys are scrubbed before spawn.
    #[tokio::test]
    async fn guard_children_do_not_see_the_lander_rsi_environment() {
        let fixture = Fixture::new();
        let record = fixture.root.path().join("guard-child-env.txt");
        fixture.add_fake_cargo(&format!("env > '{}'", record.display()));
        let old_path = use_fake_path(&fixture);
        let previous = std::env::var_os("RSI_PROCESS_OWNERSHIP_NAMESPACE");
        unsafe { std::env::set_var("RSI_PROCESS_OWNERSHIP_NAMESPACE", "lander-custody") };
        let report = run_one_guard(
            &fixture.repo,
            &GuardSpec {
                commands: Vec::new(),
                env: BTreeMap::new(),
                output_tail_bytes: 64 * 1024,
            },
            &cargo_guard_command("demo", None),
        )
        .await;
        unsafe {
            match previous {
                Some(value) => std::env::set_var("RSI_PROCESS_OWNERSHIP_NAMESPACE", value),
                None => std::env::remove_var("RSI_PROCESS_OWNERSHIP_NAMESPACE"),
            }
        }
        restore_path(old_path);
        assert_eq!(
            report.unwrap().status,
            rsid::integration::GuardStatus::Passed
        );
        let child = fs::read_to_string(&record).expect("fake cargo recorded its environment");
        assert!(
            !child
                .lines()
                .any(|line| line.starts_with("RSI_PROCESS_OWNERSHIP_NAMESPACE=")),
            "the lander RSI_* environment leaked into the guard child"
        );
    }

    /// #970: the remote gate runs shards concurrently, bounded and in input
    /// order, with an operator knob clamped to a safe range.
    #[test]
    fn remote_gate_parallelism_is_clamped_with_a_default() {
        assert_eq!(parse_remote_gate_parallelism(None), 4);
        assert_eq!(parse_remote_gate_parallelism(Some("2")), 2);
        assert_eq!(parse_remote_gate_parallelism(Some(" 6 ")), 6);
        assert_eq!(parse_remote_gate_parallelism(Some("0")), 1);
        assert_eq!(parse_remote_gate_parallelism(Some("99")), 8);
        assert_eq!(parse_remote_gate_parallelism(Some("many")), 4);
    }

    #[tokio::test]
    async fn run_bounded_overlaps_work_within_the_bound_and_keeps_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (running_in, peak_in) = (Arc::clone(&running), Arc::clone(&peak));
        let results = run_bounded((0..10).collect(), 3, move |item: usize| {
            let now = running_in.fetch_add(1, Ordering::SeqCst) + 1;
            peak_in.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(60));
            running_in.fetch_sub(1, Ordering::SeqCst);
            item * 10
        })
        .await
        .unwrap();
        assert_eq!(results, (0..10).map(|item| item * 10).collect::<Vec<_>>());
        let peak = peak.load(Ordering::SeqCst);
        assert!(peak <= 3, "at most 3 runs in flight, saw {peak}");
        assert!(peak >= 2, "runs must overlap, saw {peak}");
        assert_eq!(running.load(Ordering::SeqCst), 0);
    }

    /// #971: a focused filter that selects only candidate-new tests runs no
    /// base tests. The base contributes no failures, but the candidate side
    /// still refuses a run that executed nothing.
    #[test]
    fn base_no_tests_to_run_is_an_empty_base_failure_set() {
        // Exact nextest 0.9.137 output for a filter with no matches (exit 4).
        let no_tests = rsid::integration::GuardCommandReport {
            program: "scripts/run-rsid-test-shards.sh".into(),
            args: vec![
                "shard".into(),
                "session-05".into(),
                "--jobs".into(),
                "4".into(),
                "--filterset".into(),
                "test(context_succession)".into(),
            ],
            status: rsid::integration::GuardStatus::Failed { code: Some(4) },
            stdout_tail: concat!(
                " Nextest run ID 4c35f785 with nextest profile: rsid-fast\n",
                "    Starting 0 tests across 1 binary (309 tests skipped)\n",
                "     Summary [   0.000s] 0 tests run: 0 passed, 309 skipped\n",
            )
            .into(),
            stderr_tail: "error: no tests to run\n(hint: use `--no-tests` to customize)\n".into(),
            output_truncated: false,
            duration: Duration::from_secs(1),
        };
        assert!(nextest_ran_no_tests(&no_tests));
        assert_eq!(
            base_failures_from_report(&no_tests).unwrap(),
            BTreeSet::new()
        );
        assert!(
            observed_test_failures(&no_tests)
                .unwrap_err()
                .contains("affected-crate guard failed"),
            "the candidate side must still refuse a run with no tests"
        );

        // Any other exit 4, or an unparseable failure, stays an error.
        let other = rsid::integration::GuardCommandReport {
            stderr_tail: "error: building test binaries failed\n".into(),
            ..no_tests.clone()
        };
        assert!(!nextest_ran_no_tests(&other));
        assert!(base_failures_from_report(&other).is_err());
        let exit_100 = rsid::integration::GuardCommandReport {
            status: rsid::integration::GuardStatus::Failed { code: Some(100) },
            ..no_tests.clone()
        };
        assert!(base_failures_from_report(&exit_100).is_err());
        // Named base failures still flow through unchanged.
        let named = rsid::integration::GuardCommandReport {
            status: rsid::integration::GuardStatus::Failed { code: Some(100) },
            stdout_tail: "        FAIL [   0.1s] (1/2) rsid store::tests::red\n".into(),
            stderr_tail: String::new(),
            ..no_tests
        };
        assert_eq!(
            base_failures_from_report(&named).unwrap(),
            BTreeSet::from(["store::tests::red".to_string()])
        );
    }

    /// A refused isolated retry carries its assertion into the receipt.
    #[test]
    fn retry_failure_evidence_names_the_failing_assertion() {
        let stderr = concat!(
            "thread 'session::rotation::fence_tests::moved_tip' (4242) panicked at crates/rsid/src/session/rotation/fence_tests.rs:929:5:\n",
            "assertion `left == right` failed\n",
            "  left: 0\n",
            " right: 1\n",
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n",
        );
        let report = rsid::integration::GuardCommandReport {
            program: "cargo".into(),
            args: vec!["test".into()],
            status: rsid::integration::GuardStatus::Failed { code: Some(101) },
            stdout_tail:
                "test session::rotation::fence_tests::moved_tip ... FAILED\nError: settle refused\n"
                    .into(),
            stderr_tail: stderr.into(),
            output_truncated: false,
            duration: Duration::from_secs(5),
        };
        assert_eq!(
            retry_failure_evidence(&report),
            "thread 'session::rotation::fence_tests::moved_tip' (4242) panicked at \
             crates/rsid/src/session/rotation/fence_tests.rs:929:5: | \
             assertion `left == right` failed | left: 0 | right: 1 | Error: settle refused"
        );
        let flood = rsid::integration::GuardCommandReport {
            stderr_tail: "thread 't' panicked at x.rs:1:1:\nboom\n".repeat(400),
            ..report
        };
        assert!(retry_failure_evidence(&flood).len() <= 2048);
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

    /// #988: the isolated retry has its own guard, clamped to 60 s..=the guard
    /// it isolates, so it can never outlive the command it retries.
    #[test]
    fn retry_timeout_is_clamped_to_the_guard_it_isolates() {
        assert_eq!(
            retry_timeout_from(None, Duration::from_secs(1200)),
            Duration::from_secs(600)
        );
        assert_eq!(
            retry_timeout_from(Some("not-a-number"), Duration::from_secs(1200)),
            Duration::from_secs(600)
        );
        assert_eq!(
            retry_timeout_from(Some(" 900 "), Duration::from_secs(1200)),
            Duration::from_secs(900)
        );
        assert_eq!(
            retry_timeout_from(Some("30"), Duration::from_secs(1200)),
            Duration::from_secs(60)
        );
        assert_eq!(
            retry_timeout_from(Some("3600"), Duration::from_secs(1200)),
            Duration::from_secs(1200),
            "a retry must never outlive the guard it isolates"
        );
        assert_eq!(
            retry_timeout_from(None, Duration::from_secs(600)),
            Duration::from_secs(600)
        );
    }

    /// #988: a hung isolated retry refuses with its own bounded guard and a
    /// message that names the hang, instead of looking like an ordinary test
    /// failure.
    #[tokio::test]
    async fn hung_isolated_retry_reports_its_bounded_timeout() {
        let fixture = Fixture::new();
        fixture.add_fake_cargo("sleep 60");
        let old_path = use_fake_path(&fixture);
        let mut command = cargo_guard_command("demo", None);
        command.timeout = Duration::from_secs(1);
        let result = run_isolated_retry(
            &fixture.repo,
            &GuardSpec {
                commands: Vec::new(),
                env: BTreeMap::new(),
                output_tail_bytes: 4096,
            },
            &command,
            "demo::tests::hung",
        )
        .await;
        restore_path(old_path);
        let error = result.expect_err("a hung isolated retry must fail the gate");
        assert!(
            error.contains("isolated retry hung (timed out after 1 s)"),
            "{error}"
        );
        assert!(error.contains("demo::tests::hung"), "{error}");
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

    /// #1007 (S4 races, finding 3): rolling advanced after the queue probed
    /// its tip but before the lander's initial fetch. The lander gates the
    /// fetched tip and charges that advance as a re-gated stale retry.
    #[tokio::test]
    async fn expected_tip_mismatch_spends_a_regated_retry() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/mine.rs",
            "pub fn mine() {}\n",
            "queued source",
        );
        let external = fixture.commit(
            &fixture.base,
            "crates/demo/src/other.rs",
            "pub fn other() {}\n",
            "external push",
        );
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{external}:refs/heads/rolling"),
            ],
        );
        fixture.add_fake_cargo("true");
        let old_path = use_fake_path(&fixture);
        unsafe {
            std::env::set_var("RSI_LANDER_MAX_REGATED_STALE_RETRIES", "1");
            std::env::set_var("RSI_LANDER_EXACT_GATE", "1");
        }
        let result = fixture
            .land_expecting(
                vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source,
                }],
                &fixture.base,
            )
            .await;
        unsafe {
            std::env::remove_var("RSI_LANDER_MAX_REGATED_STALE_RETRIES");
            std::env::remove_var("RSI_LANDER_EXACT_GATE");
        }
        restore_path(old_path);
        let report = result.expect("the advance is charged and the fetched tip is gated");
        assert_eq!(
            report.stale_retries,
            expected_stale_history(&fixture.base, std::slice::from_ref(&external), false)
        );
        assert_eq!(report.fetched_tip, external);
        let lines = stale_retry_lines(&report.stale_retries);
        assert!(
            lines.contains(&format!(
                "stale_retry_1={}..{external}:regated",
                fixture.base
            )),
            "{lines:?}"
        );
    }

    /// With no re-gated retry left the mismatch refuses before any gate runs.
    #[tokio::test]
    async fn expected_tip_mismatch_with_no_budget_is_refused_as_stale_exhausted() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/mine.rs",
            "pub fn mine() {}\n",
            "queued source",
        );
        let external = fixture.commit(
            &fixture.base,
            "crates/demo/src/other.rs",
            "pub fn other() {}\n",
            "external push",
        );
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{external}:refs/heads/rolling"),
            ],
        );
        fixture.add_fake_cargo("true");
        let old_path = use_fake_path(&fixture);
        unsafe { std::env::set_var("RSI_LANDER_MAX_REGATED_STALE_RETRIES", "0") };
        let result = fixture
            .land_expecting(
                vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source,
                }],
                &fixture.base,
            )
            .await;
        unsafe { std::env::remove_var("RSI_LANDER_MAX_REGATED_STALE_RETRIES") };
        restore_path(old_path);
        let failure = result.expect_err("no budget remains for the advance");
        assert_eq!(failure.state, PublicationState::NotPublished, "{failure:?}");
        assert!(
            failure.message.contains("stale retries exhausted"),
            "{failure:?}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            external,
            "nothing was published"
        );
    }

    /// A matching expected tip and the absence of the argument charge nothing.
    #[tokio::test]
    async fn a_matching_expected_tip_charges_no_retry() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/mine.rs",
            "pub fn mine() {}\n",
            "queued source",
        );
        fixture.add_fake_cargo("true");
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_expecting(
                vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source,
                }],
                &fixture.base,
            )
            .await;
        restore_path(old_path);
        let report = result.expect("the probed tip is still the tip");
        assert!(
            report.stale_retries.is_empty(),
            "{:?}",
            report.stale_retries
        );
    }

    #[test]
    fn expected_tip_argument_parses_and_rejects_non_hex() {
        let parsed = parse_args(
            ["--accepted", "abc", "--expected-tip", "ABCDEF0123"].map(String::from),
            None,
        )
        .unwrap();
        assert_eq!(parsed.expected_tip.as_deref(), Some("abcdef0123"));
        let bare = parse_args(["--accepted", "abc"].map(String::from), None).unwrap();
        assert_eq!(bare.expected_tip, None);
        assert!(
            parse_args(
                ["--accepted", "abc", "--expected-tip", "not-a-sha"].map(String::from),
                None
            )
            .is_err()
        );
    }

    /// #1007: a landing of several sources is one candidate, so its test gate
    /// runs once, while every source still gets its own static guard plan.
    #[tokio::test]
    async fn two_source_landing_gates_the_candidate_once_and_plans_each_source() {
        let fixture = Fixture::new();
        let first = fixture.commit(
            &fixture.base,
            "crates/demo/src/first.rs",
            "pub fn first() {}\n",
            "first source",
        );
        let second = fixture.commit(
            &fixture.base,
            "crates/demo/src/second.rs",
            "pub fn second() {}\n",
            "second source",
        );
        let log = fixture.root.path().join("tested-commits");
        fixture.add_fake_cargo(&format!(
            "printf '%s\\n' \"$(/usr/bin/git rev-parse HEAD)\" >> '{}'",
            log.display()
        ));
        let plans = fixture.root.path().join("guard-plans");
        let python = fixture.bin.join("python3");
        // Wrap the interpreter the host's `python3` resolves to (the cloud sweep
        // shims 3.11 ahead of AL2023's 3.9 /usr/bin/python3), not a hard-coded path.
        let real_python = Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("/usr/bin/python3"));
        fs::write(
            &python,
            format!(
                "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = --plan-only ]; then printf '%s\\n' \"$*\" >> '{}'; fi\ndone\nexec '{}' \"$@\"\n",
                plans.display(),
                real_python.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&python, fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![
                AcceptedPair {
                    base: fixture.base.clone(),
                    source: first.clone(),
                },
                AcceptedPair {
                    base: fixture.base.clone(),
                    source: second.clone(),
                },
            ])
            .await;
        restore_path(old_path);
        let report = result.expect("two independent sources land as one candidate");
        let tested = fs::read_to_string(&log).unwrap();
        assert_eq!(
            tested
                .lines()
                .filter(|oid| *oid == report.candidate)
                .count(),
            1,
            "the candidate test command runs once for the whole landing: {tested}"
        );
        let planned = fs::read_to_string(&plans).unwrap();
        assert!(
            planned.lines().any(|line| line.contains(&first))
                && planned.lines().any(|line| line.contains(&second)),
            "each source keeps its own static plan: {planned}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
    }

    /// Stages two disjoint rolling advances that each win the race against the
    /// candidate the lander just gated, and returns the lander's outcome plus
    /// the commits the fake cargo ran tests on.
    async fn land_against_disjoint_advances(
        exact_gate: bool,
        advance_count: usize,
    ) -> (
        Fixture,
        String,
        Vec<String>,
        String,
        Result<LandReport, LandFailure>,
    ) {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"source\" }\n",
            "source crate",
        );
        let contents = (0..advance_count)
            .map(|index| (format!("incoming-{index}.txt"), "incoming\n".to_string()))
            .collect::<Vec<_>>();
        let advances = stage_advances(&fixture, &fixture.base, &contents);
        let pending = fixture.root.path().join("pending-advances");
        fs::write(&pending, format!("{}\n", advances.join("\n"))).unwrap();
        // Rolling tips run only as base-side comparisons; mark them seen.
        let seen = fixture.root.path().join("gated-commits");
        fs::write(
            &seen,
            format!("{}\n{}\n", fixture.base, advances.join("\n")),
        )
        .unwrap();
        let log = fixture.root.path().join("tested-commits");
        // Every newly gated candidate loses the race to the next advance.
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\nnext=$(head -n 1 '{pending}')\nif [ -n \"$next\" ] && ! grep -qx \"$oid\" '{seen}'; then\n  printf '%s\\n' \"$oid\" >> '{seen}'\n  sed -i 1d '{pending}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling \"$next\" || exit 43\nfi",
            log = log.display(),
            pending = pending.display(),
            seen = seen.display(),
            bare = fixture.bare.display(),
        ));
        let old_path = use_fake_path(&fixture);
        // Tests serialize all environment changes through ENV_LOCK.
        unsafe {
            std::env::set_var("RSI_LANDER_MAX_REGATED_STALE_RETRIES", "1");
            if exact_gate {
                std::env::set_var("RSI_LANDER_EXACT_GATE", "1");
            }
        }
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        unsafe {
            std::env::remove_var("RSI_LANDER_MAX_REGATED_STALE_RETRIES");
            std::env::remove_var("RSI_LANDER_EXACT_GATE");
        }
        restore_path(old_path);
        let tested = fs::read_to_string(&log).unwrap();
        (fixture, source, advances, tested, result)
    }

    /// #1007: under `RSI_LANDER_EXACT_GATE=1` a disjoint advance no longer
    /// reuses the earlier gate. The remade candidate is tested, and the next
    /// advance is refused by the re-gated cap instead of publishing an
    /// untested tip.
    #[tokio::test]
    async fn exact_gate_regates_a_disjoint_stale_advance_and_the_cap_refuses_the_next() {
        let (fixture, source, advances, tested, result) =
            land_against_disjoint_advances(true, 2).await;
        let failure = result.expect_err("the second advance exceeds the re-gated budget");
        assert_eq!(failure.state, PublicationState::NotPublished, "{failure:?}");
        assert!(
            failure.message.contains("stale retries exhausted"),
            "{failure:?}"
        );
        assert_eq!(
            failure.stale_retries,
            expected_stale_history(&fixture.base, &advances[..1], false)
        );
        let candidates = tested
            .lines()
            .filter(|oid| *oid != fixture.base && !advances.iter().any(|advance| advance == oid))
            .collect::<BTreeSet<_>>();
        assert!(
            candidates.iter().any(|oid| *oid != source),
            "the remade candidate is tested: {tested}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            advances[1],
            "nothing was published"
        );
    }

    /// Without the opt-in, manual landings keep reusing the earlier gate for a
    /// disjoint advance and publish the remade candidate untested.
    #[tokio::test]
    async fn without_exact_gate_a_disjoint_stale_advance_still_reuses_the_gate() {
        let (fixture, _source, advances, tested, result) =
            land_against_disjoint_advances(false, 1).await;
        let report = result.expect("a disjoint advance reuses the earlier gate");
        assert!(
            report.stale_retries.iter().all(|retry| retry.reused_gate),
            "{:?}",
            report.stale_retries
        );
        assert_eq!(report.stale_retries.len(), 1, "{:?}", report.stale_retries);
        assert_eq!(&report.fetched_tip, advances.last().unwrap());
        assert_eq!(
            tested
                .lines()
                .filter(|oid| *oid == report.candidate)
                .count(),
            1,
            "the remade candidate runs only its post-publication canary: {tested}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
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
                expected_tip: None,
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
    fn hermetic_hot_file_needs_no_claim_even_when_another_work_claims_it() {
        // Operator P0 (2026-09-29): no exclusive hot-file claims; fast-forward
        // publication serializes landings.
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
        let other = check(vec![policy_work(&fixture.base, "AGENTS.md", 125)])
            .expect("another Work's claim does not block a hot file");
        assert_eq!(other[0].state, "unbound");
        let own = check(vec![policy_work(&source, "AGENTS.md", 125)]).expect("own Work passes");
        assert_eq!(own[0].state, "bound");
    }

    #[test]
    fn hermetic_migration_must_be_exactly_tip_plus_one_with_its_block() {
        let fixture = Fixture::new();
        let store = "crates/rsid/src/store/mod.rs";
        let block = |version: u32| {
            format!(
                "pub const LATEST_SCHEMA_VERSION: i32 = {version};\nfn migrate(version: i32) {{\n    if version < {version} {{\n    }}\n}}\n"
            )
        };
        let v124 = fixture.commit(
            &fixture.base,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 124;\n",
            "schema 124",
        );
        let v125 = fixture.commit(&v124, store, &block(125), "schema 125");
        let v126 = fixture.commit(&v124, store, &block(126), "schema 126");
        let bare_v125 = fixture.commit(
            &v124,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 125;\n",
            "schema 125 without a block",
        );
        // The released-migration script has its own fixture suite. This fixture
        // isolates the landing policy's migration-number rule.
        write(
            fixture.root.path(),
            "tools/check-released-migrations.py",
            "import sys\nsys.exit(0)\n",
        );
        let check = |source: &str, proved: &[u32]| {
            landing_policy::check_with_work_and_proof(
                &fixture.repo,
                fixture.root.path(),
                &v124,
                source,
                &[AcceptedPair {
                    base: v124.clone(),
                    source: source.to_string(),
                }],
                proved,
                || Ok(vec![]),
            )
        };
        let bindings = check(&v125, &[]).expect("tip + 1 with its block lands without a ledger");
        assert_eq!(bindings[0].state, "unbound");
        check(&v125, &[125]).expect("a proved tip + 1 passes");
        let skipped = check(&v126, &[]).expect_err("a skipped number refuses");
        assert_eq!(skipped.fence, landing_policy::PolicyFence::MigrationNumber);
        assert!(
            skipped.message.contains("migration must be V125"),
            "{}",
            skipped.message
        );
        let missing = check(&bare_v125, &[]).expect_err("a version without its block refuses");
        assert_eq!(missing.fence, landing_policy::PolicyFence::MigrationNumber);
        assert!(
            missing.message.contains("if version < 125"),
            "{}",
            missing.message
        );
        let outside = check(&v125, &[127]).expect_err("proof outside the appended range refuses");
        assert_eq!(outside.fence, landing_policy::PolicyFence::SchemaVersion);
    }

    #[test]
    fn hermetic_per_file_migration_must_be_exactly_tip_plus_one_with_its_block() {
        let fixture = Fixture::new();
        let file = |version: u32| format!("crates/rsid/src/store/migrations/v{version:03}.rs");
        let gate = |version: u32| {
            format!(
                "impl Store {{\n    fn migrate_v{version:03}(&self, version: i32) {{\n        if version < {version} {{\n        }}\n    }}\n}}\n"
            )
        };
        let v124 = fixture.commit(&fixture.base, &file(124), &gate(124), "migration 124");
        let v125 = fixture.commit(&v124, &file(125), &gate(125), "migration 125");
        let v126 = fixture.commit(&v124, &file(126), &gate(126), "migration 126");
        let bare_v125 = fixture.commit(
            &v124,
            &file(125),
            "impl Store {}\n",
            "migration 125 without a gate",
        );
        write(
            fixture.root.path(),
            "tools/check-released-migrations.py",
            "import sys\nsys.exit(0)\n",
        );
        let check = |source: &str| {
            landing_policy::check_with_work_and_proof(
                &fixture.repo,
                fixture.root.path(),
                &v124,
                source,
                &[AcceptedPair {
                    base: v124.clone(),
                    source: source.to_string(),
                }],
                &[],
                || Ok(vec![]),
            )
        };
        check(&v125).expect("tip + 1 as its own file lands");
        let skipped = check(&v126).expect_err("a skipped number refuses");
        assert_eq!(skipped.fence, landing_policy::PolicyFence::MigrationNumber);
        assert!(
            skipped.message.contains("migration must be V125"),
            "{}",
            skipped.message
        );
        let missing = check(&bare_v125).expect_err("a file without its gate refuses");
        assert_eq!(missing.fence, landing_policy::PolicyFence::MigrationNumber);
        assert!(
            missing.message.contains("if version < 125"),
            "{}",
            missing.message
        );
    }

    #[test]
    fn hermetic_per_file_migrations_may_land_as_one_contiguous_run_from_tip_plus_one() {
        let fixture = Fixture::new();
        let file = |version: u32| format!("crates/rsid/src/store/migrations/v{version:03}.rs");
        let gate = |version: u32| {
            format!(
                "impl Store {{\n    fn migrate_v{version:03}(&self, version: i32) {{\n        if version < {version} {{\n        }}\n    }}\n}}\n"
            )
        };
        let v124 = fixture.commit(&fixture.base, &file(124), &gate(124), "migration 124");
        let v125 = fixture.commit(&v124, &file(125), &gate(125), "migration 125");
        let run = fixture.commit(&v125, &file(126), &gate(126), "migration 126");
        let gap = fixture.commit(&v125, &file(127), &gate(127), "migration 127 over a gap");
        let bare_v125 = fixture.commit(
            &v124,
            &file(125),
            "impl Store {}\n",
            "migration 125 without a gate",
        );
        let unanchored = fixture.commit(&bare_v125, &file(126), &gate(126), "migration 126");
        write(
            fixture.root.path(),
            "tools/check-released-migrations.py",
            "import sys\nsys.exit(0)\n",
        );
        let check = |source: &str| {
            landing_policy::check_with_work_and_proof(
                &fixture.repo,
                fixture.root.path(),
                &v124,
                source,
                &[AcceptedPair {
                    base: v124.clone(),
                    source: source.to_string(),
                }],
                &[],
                || Ok(vec![]),
            )
        };
        check(&run).expect("V125 and V126 land together as one contiguous run");
        let gapped = check(&gap).expect_err("a run with a gap refuses");
        assert_eq!(gapped.fence, landing_policy::PolicyFence::MigrationNumber);
        assert!(
            gapped.message.contains("if version < 126"),
            "{}",
            gapped.message
        );
        let unanchored = check(&unanchored).expect_err("a run must start at tip + 1");
        assert_eq!(
            unanchored.fence,
            landing_policy::PolicyFence::MigrationNumber
        );
        assert!(
            unanchored.message.contains("migration must be V125"),
            "{}",
            unanchored.message
        );
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
    fn target_filters_run_cargo_targets_and_may_name_untouched_packages() {
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &[],
            &[
                "rsid=bin:rsid".into(),
                "rsi-common=test:handoff_corpus".into(),
            ],
            false,
            &guard_workspace_metadata(),
            4,
        )
        .unwrap();
        let rendered: Vec<String> = spec
            .commands
            .iter()
            .map(|command| command.args.join(" "))
            .collect();
        assert!(
            rendered
                .iter()
                .any(|args| args.starts_with("test -p rsid --bin rsid"))
        );
        assert!(
            rendered
                .iter()
                .any(|args| args.starts_with("test -p rsi-common --test handoff_corpus"))
        );
        assert!(
            affected_crate_guard_spec(
                "1111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222",
                &[],
                &["not-a-crate=x".into()],
                false,
                &guard_workspace_metadata(),
                4,
            )
            .is_err()
        );
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
        // A filter scopes the rsid gate: diff check, compile check, and the
        // named shard (operator P0, 2026-09-29).
        let commands = spec
            .commands
            .iter()
            .map(|command| format!("{} {}", command.program, command.args.join(" ")))
            .collect::<Vec<_>>();
        assert_eq!(
            commands[1..],
            [
                "cargo check -p rsid --all-targets",
                "scripts/run-rsid-test-shards.sh shard session-01 --jobs 4",
            ]
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

    #[tokio::test]
    async fn base_shard_lock_wait_is_bounded_and_names_the_holder() {
        let root = tempfile::tempdir().unwrap();
        let base = "1".repeat(40);
        let key = "sha256:".to_owned() + &"b".repeat(64);
        let holder = base_cache::BaseShardSlot::acquire(root.path(), &base, "store-01", &key)
            .await
            .unwrap();
        // A second lander for the same base shard must refuse (fail closed)
        // with a diagnostic once its bound expires, not wedge behind the holder.
        let error = match base_cache::BaseShardSlot::acquire_within(
            root.path(),
            &base,
            "store-01",
            &key,
            Duration::from_millis(300),
        )
        .await
        {
            Ok(_) => panic!("lock held elsewhere must not be acquired"),
            Err(error) => error,
        };
        assert!(
            error.contains("store-01.lock") && error.contains("still held"),
            "{error}"
        );
        // A different shard of the same base is an independent lock.
        base_cache::BaseShardSlot::acquire_within(
            root.path(),
            &base,
            "session-01",
            &key,
            Duration::from_millis(300),
        )
        .await
        .expect("other shard lock is independent");
        drop(holder);
        base_cache::BaseShardSlot::acquire_within(
            root.path(),
            &base,
            "store-01",
            &key,
            Duration::from_millis(300),
        )
        .await
        .expect("released lock is acquirable");
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
            .filter(|command| command.program == "cargo" && command.args[0] == "test")
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
                "check -p rsi-common --all-targets",
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
    async fn hot_file_lands_unbound_without_ledger_access() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "AGENTS.md", "policy\n", "hot file");
        fixture.add_fake_cargo("");
        let old_token = std::env::var_os("RSI_SESSION_TOKEN");
        unsafe { std::env::remove_var("RSI_SESSION_TOKEN") };
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        if let Some(token) = old_token {
            unsafe { std::env::set_var("RSI_SESSION_TOKEN", token) };
        }
        let report = result.expect("a hot-file source lands without a lead token");
        assert_eq!(report.source_bindings[0].source, source);
        assert_eq!(report.source_bindings[0].state, "unknown");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
    }

    #[tokio::test]
    async fn policy_refusal_runs_before_any_test_gate() {
        let fixture = Fixture::new();
        let store = "crates/rsid/src/store/mod.rs";
        // Released DDL lives elsewhere in this fixture, so the store change
        // reaches the migration-number rule rather than the released guard.
        let manifest = fixture.commit(
            &fixture.base,
            "tools/released-migrations.json",
            "{\"migration_file\":\"crates/rsid/src/store/released.rs\",\"protected_sections\":{}}\n",
            "released DDL elsewhere",
        );
        let tip = fixture.commit(
            &manifest,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 124;\n",
            "schema 124",
        );
        git_run(
            &fixture.repo,
            &["push", "-q", "origin", &format!("{tip}:refs/heads/rolling")],
        );
        let source = fixture.commit(
            &tip,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 126;\nfn migrate(version: i32) {\n    if version < 126 {\n    }\n}\n",
            "skip V125",
        );
        let cargo_log = fixture.add_fake_cargo_log();
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: tip.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let refusal = result.expect_err("a skipped migration number refuses");
        assert_eq!(refusal.state, PublicationState::NotPublished);
        assert_eq!(refusal.exit_code(), 8);
        assert!(
            refusal
                .evidence_lines()
                .contains(&"policy_fence=migration_number".into()),
            "{refusal:?}"
        );
        assert!(
            refusal.message.contains("migration must be V125"),
            "{refusal:?}"
        );
        let cargo_args = fs::read_to_string(&cargo_log).unwrap_or_default();
        assert!(
            cargo_args
                .lines()
                .all(|arg| arg != "test" && arg != "check"),
            "the refusal must precede every gate command: {cargo_args}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            tip
        );
    }

    #[tokio::test]
    async fn whitespace_error_refuses_before_any_gate_command_runs() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() {}\n\n",
            "blank line at EOF",
        );
        let cargo_log = fixture.add_fake_cargo_log();
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let refusal = result.expect_err("a whitespace error refuses");
        assert_eq!(refusal.state, PublicationState::NotPublished);
        assert!(
            refusal
                .message
                .contains("static guard failed before any test ran"),
            "{refusal:?}"
        );
        assert!(
            refusal.message.contains("new blank line at EOF"),
            "{refusal:?}"
        );
        let cargo_args = fs::read_to_string(&cargo_log).unwrap_or_default();
        assert!(
            cargo_args
                .lines()
                .all(|arg| !matches!(arg, "test" | "check" | "nextest")),
            "no build or test may start before the static guard: {cargo_args}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
    }

    #[test]
    fn test_outcome_line_records_pass_fail_and_error_per_shard() {
        let mut report = rsid::integration::GuardCommandReport {
            program: "cargo".into(),
            args: Vec::new(),
            status: rsid::integration::GuardStatus::Passed,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            output_truncated: false,
            duration: Duration::ZERO,
        };
        assert_eq!(
            test_outcome_line("candidate", "store-01", Ok(&report)),
            "test_outcome shard=store-01 side=candidate result=pass"
        );
        report.status = rsid::integration::GuardStatus::Failed { code: Some(1) };
        assert_eq!(
            test_outcome_line("base", "store-01", Ok(&report)),
            "test_outcome shard=store-01 side=base result=fail"
        );
        assert_eq!(
            test_outcome_line("base", "rpc-02", Err("host lost\nmore")),
            "test_outcome shard=rpc-02 side=base result=error detail=host lost"
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
            expected_tip: None,
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
    async fn handed_off_canaries_for_two_publications_run_one_covering_gate() {
        let fixture = Fixture::new();
        let registry_root = fixture.root.path().join("canary-registry");
        let gate_log = fixture.root.path().join("canary-gate.log");
        // A red inline canary would fail these landings: the handoff must
        // publish without running it, and the runner's single gate is green.
        fixture.add_fake_cargo(&format!("pwd -P >> '{}'", gate_log.display()));
        let old_path = use_fake_path(&fixture);
        // Tests serialize all environment changes through ENV_LOCK (held by
        // the fixture).
        unsafe {
            std::env::set_var("RSI_LANDER_CANARY_MODE", "handoff");
            std::env::set_var("RSI_ROLLING_CANARY_DIR", &registry_root);
            std::env::set_var("RSI_LANDER_CANARY_SPAWN", "0");
        }
        let first_source = fixture.commit(
            &fixture.base,
            "crates/demo/src/first.rs",
            "pub fn first() {}\n",
            "first accepted source",
        );
        let first = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: first_source,
            }])
            .await
            .expect("first landing publishes and hands off its canary");
        let second_source = fixture.commit(
            &fixture.base,
            "crates/demo/src/second.rs",
            "pub fn second() {}\n",
            "second accepted source",
        );
        let second = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: second_source,
            }])
            .await
            .expect("second landing publishes and hands off its canary");
        let registry = canary::Registry::open(&registry_root).expect("registry");
        let queued = registry.pending().expect("pending canaries");
        let remote_url = git_value(&fixture.repo, &["remote", "get-url", "--push", "origin"]);
        let mut gate =
            canary_runner::LanderCanaryGate::new(&registry_root, &fixture.repo, &remote_url)
                .expect("runner gate");
        let summary = canary::run_queue(&registry, &mut gate)
            .await
            .expect("runner drains the queue");
        let first_verdict = registry
            .verdict(&first.published_tip)
            .expect("verdict read");
        let second_verdict = registry
            .verdict(&second.published_tip)
            .expect("verdict read");
        let gate_runs = fs::read_to_string(&gate_log).unwrap_or_default();
        unsafe {
            std::env::remove_var("RSI_LANDER_CANARY_MODE");
            std::env::remove_var("RSI_ROLLING_CANARY_DIR");
            std::env::remove_var("RSI_LANDER_CANARY_SPAWN");
        }
        restore_path(old_path);

        let registry_text = registry_root.display().to_string();
        assert_eq!(first.canary_queued.as_deref(), Some(registry_text.as_str()));
        assert_eq!(
            second.canary_queued.as_deref(),
            Some(registry_text.as_str())
        );
        assert_eq!(second.fetched_tip, first.published_tip);
        assert_eq!(queued.len(), 2);
        // Issue #951 acceptance 4: one full canary gate covers both landings.
        assert_eq!(summary.gates_run, 1);
        assert!(!gate_runs.is_empty(), "the covering gate executed tests");
        assert_eq!(
            first_verdict.expect("first landing verified").verdict,
            canary::Verdict::Green {
                gate_base: fixture.base.clone(),
                gate_tip: second.published_tip.clone(),
                covered_by: Some(second.published_tip.clone()),
            }
        );
        assert_eq!(
            second_verdict.expect("second landing verified").verdict,
            canary::Verdict::Green {
                gate_base: fixture.base.clone(),
                gate_tip: second.published_tip.clone(),
                covered_by: None,
            }
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            second.published_tip
        );
    }

    /// #1081: through the real runner adapter, a base with a red test and a
    /// clean candidate is `base_red`: the verdict names the red test, the
    /// landing is not reverted, and exactly one alert is raised.
    #[tokio::test]
    async fn canary_runner_reports_a_red_base_with_a_clean_candidate_as_base_red() {
        let fixture = Fixture::new();
        let registry_root = fixture.root.path().join("canary-registry");
        // Red only when the gate tests the base commit; every candidate is clean.
        fixture.add_fake_canary_cargo_when(
            &format!("[ \"$head\" = '{}' ]", fixture.base),
            "echo 'test demo::red ... FAILED'; exit 1",
        );
        let old_path = use_fake_path(&fixture);
        // Tests serialize all environment changes through ENV_LOCK (held by
        // the fixture).
        unsafe {
            std::env::set_var("RSI_LANDER_CANARY_MODE", "handoff");
            std::env::set_var("RSI_ROLLING_CANARY_DIR", &registry_root);
            std::env::set_var("RSI_LANDER_CANARY_SPAWN", "0");
        }
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let landed = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await
            .expect("landing publishes and hands off its canary");
        let registry = canary::Registry::open(&registry_root).expect("registry");
        let remote_url = git_value(&fixture.repo, &["remote", "get-url", "--push", "origin"]);
        let mut gate =
            canary_runner::LanderCanaryGate::new(&registry_root, &fixture.repo, &remote_url)
                .expect("runner gate");
        let summary = canary::run_queue(&registry, &mut gate)
            .await
            .expect("runner drains the queue");
        let verdict = registry
            .verdict(&landed.published_tip)
            .expect("verdict read");
        unsafe {
            std::env::remove_var("RSI_LANDER_CANARY_MODE");
            std::env::remove_var("RSI_ROLLING_CANARY_DIR");
            std::env::remove_var("RSI_LANDER_CANARY_SPAWN");
        }
        restore_path(old_path);

        assert_eq!(summary.gates_run, 1);
        assert!(summary.reverted.is_empty() && summary.red.is_empty());
        assert!(summary.verified.is_empty());
        assert_eq!(summary.base_red.len(), 1);
        let (tip, error) = &summary.base_red[0];
        assert_eq!(tip, &landed.published_tip);
        assert!(error.contains("demo::red"), "{error}");
        match verdict.expect("landing has a verdict").verdict {
            canary::Verdict::BaseRed {
                gate_base,
                gate_tip,
                error,
            } => {
                assert_eq!(gate_base, fixture.base);
                assert_eq!(gate_tip, landed.published_tip);
                assert!(error.contains("demo::red"), "{error}");
            }
            other => panic!("expected a base_red verdict, got {other:?}"),
        }
        let alerts = canary_runner::alert_lines(&summary);
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert!(
            alerts[0].starts_with("ALERT canary base_red")
                && alerts[0].contains(&landed.published_tip)
                && alerts[0].contains("demo::red"),
            "{alerts:?}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            landed.published_tip,
            "a red base never reverts the landing"
        );
    }

    /// #1025: a canary that could not produce a test result (here a cargo that
    /// crashes before naming any test) is not a test red: the landing stays
    /// published, nothing is reverted, and the failure says so.
    #[tokio::test]
    async fn canary_environment_error_does_not_forward_revert() {
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
        let failure = result.expect_err("an unjudged canary reports, it does not pass");
        assert_eq!(failure.state, PublicationState::Published);
        assert_ne!(failure.kind, FailureKind::CanaryRed);
        assert_eq!(failure.forward_revert_status, None);
        assert_eq!(failure.forward_revert_id, None);
        assert!(failure.message.contains("could not run, no revert"));
        assert_eq!(failure.outcome_label(), "published_unverified");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
        );
    }

    /// #1025: a canary whose base is already red on the same test is not a new
    /// failure: no revert, the landing completes.
    #[tokio::test]
    async fn canary_red_base_does_not_forward_revert() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        // Red at every commit once the landing is published, base included.
        fixture.add_fake_canary_cargo_when(
            &format!("[ \"$remote\" != '{}' ]", fixture.base),
            "echo 'test demo::red ... FAILED'; exit 1",
        );
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_ungated_candidate(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let report = result.expect("a red base is not a new failure");
        assert!(report.base_reds.contains("demo::red"));
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
        );
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
        fixture.add_fake_canary_cargo(&fixture.base, "echo 'test demo::red ... FAILED'; exit 42");
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
        fixture.add_fake_canary_cargo(&target, "echo 'test demo::red ... FAILED'; exit 42");
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
        fixture.add_fake_canary_cargo_when(
            &format!(
                "[ \"$head\" != '{}' ] && [ \"$head\" != '{}' ]",
                fixture.base, other
            ),
            &format!(
                "/usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43\necho 'test demo::red ... FAILED'; exit 42",
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
        fixture.add_fake_canary_cargo_when(&format!("[ \"$head\" = '{source}' ]"), &format!(
            "if [ ! -e '{}' ]; then /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43; : > '{}'; fi\necho 'test demo::red ... FAILED'; exit 42",
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
            "count=$(cat '{}' 2>/dev/null || echo 0)\ncount=$((count + 1))\necho \"$count\" > '{}'\ncase $count in\n  2) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43; echo 'test demo::red ... FAILED'; exit 42;;\n  3) echo 'test demo::red ... FAILED'; exit 42;;\n  4) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43;;\n  6) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43;;\nesac",
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
        assert_eq!(fs::read_to_string(marker).unwrap().trim(), "7");
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
                "/usr/bin/git -C '{}' remote set-url --push origin '{}' || exit 43\necho 'test demo::red ... FAILED'; exit 42",
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
        // A filtered package runs its compile check (candidate only) and the
        // named tests on both sides; the unfiltered suite is the QA sweep's.
        let expected_pass = concat!(
            "metadata\n--no-deps\n--format-version\n1\n--offline\n",
            "check\n-p\ndemo\n--all-targets\n",
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
        // One compile check plus four focused test runs.
        for _ in 0..5 {
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
        let cargo_args = fs::read_to_string(args_log).unwrap_or_default();
        assert!(
            cargo_args
                .lines()
                .all(|arg| !matches!(arg, "test" | "check" | "nextest")),
            "a lost hunk is a static finding: it refuses before any build or test: {cargo_args}"
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
                    expected_tip: None,
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
    fn refusal_receipt_carries_one_failing_test_line_per_new_failure() {
        // The queue parses `failing_test=` lines from the real receipt.
        let message = new_failures_message(
            "abc123",
            &[
                "rsid::store::a_test [new]".to_string(),
                "rsid::store::b_test [known #7 flake]".to_string(),
                "rsid::c_test".to_string(),
            ],
            "base reds: rsid::old_red; isolated retry evidence: boom",
        );
        let failure = LandFailure::from(message);
        let lines: Vec<String> = failure
            .evidence_lines()
            .into_iter()
            .filter(|line| line.starts_with("failing_test="))
            .collect();
        assert_eq!(
            lines,
            [
                "failing_test=rsid::store::a_test",
                "failing_test=rsid::store::b_test",
                "failing_test=rsid::c_test"
            ]
        );
        // The queue's parser reads exactly what the lander printed.
        let receipt = failure.evidence_lines().join("\n");
        assert_eq!(
            rsid::rolling_queue::failing_tests(&receipt),
            ["rsid::c_test", "rsid::store::a_test", "rsid::store::b_test"]
        );
        let single = LandFailure::from(new_failures_message(
            "abc123",
            &["rsid::only".to_string()],
            "QA cached base p; isolated local base passed rsid::only",
        ));
        assert!(
            single
                .evidence_lines()
                .contains(&"failing_test=rsid::only".to_string())
        );
        assert!(
            LandFailure::from("gate exploded".to_string())
                .evidence_lines()
                .iter()
                .all(|line| !line.starts_with("failing_test="))
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
            parse_args(Vec::<String>::new(), None)
                .unwrap_err()
                .contains("at least one --accepted")
        );
    }

    #[test]
    fn parses_source_only_and_explicit_accepted_pairs() {
        let options = parse_args(
            [
                "--accepted".to_string(),
                "source".to_string(),
                "--accepted".to_string(),
                "base:other".to_string(),
            ],
            None,
        )
        .expect("accepted pairs");
        assert_eq!(options.accepted[0].base, "");
        assert_eq!(options.accepted[0].source, "source");
        assert_eq!(options.accepted[1].base, "base");
        assert_eq!(options.accepted[1].source, "other");
        assert_eq!(options.tmpfs_min_free_gb, 12);
        let configured = parse_args(
            [
                "--accepted".to_string(),
                "source".to_string(),
                "--tmpfs-min-free-gb".to_string(),
                "24".to_string(),
                "--disk-scratch-shard".to_string(),
                "store-01".to_string(),
            ],
            None,
        )
        .expect("configured minimum");
        assert_eq!(configured.tmpfs_min_free_gb, 24);
        assert!(configured.disk_scratch_shards.contains("store-01"));
        assert!(
            parse_args(
                [
                    "--accepted".to_string(),
                    "source".to_string(),
                    "--tmpfs-min-free-gb".to_string(),
                    "0".to_string(),
                ],
                None
            )
            .is_err()
        );
        assert!(
            parse_args(
                [
                    "--accepted".to_string(),
                    "source".to_string(),
                    "--disk-scratch-shard".to_string(),
                    "not-a-shard".to_string(),
                ],
                None
            )
            .is_err()
        );
        for malformed in ["", ":source", "base:", "a:b:c"] {
            assert!(
                parse_args(["--accepted".to_string(), malformed.to_string()], None).is_err(),
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
        let options = parse_args(args.clone(), None).expect("complete remote configuration");
        assert_eq!(options.remote_gate.unwrap().target, "ec2-user@example.com");
        assert!(parse_args(args[..6].to_vec(), None).is_err());
    }
}
