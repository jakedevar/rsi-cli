// One shard inventory for the lander and the impact selector.
use rsi_codegraph::impact::{RSID_SHARDS, is_rsid_shard_package, rsid_shard_packages};
use rsi_common::failure_signature::{Classification, Snapshot, classify, failure_text_for};
use rsid::integration::{
    Candidate, CandidateKind, CommitIdentity, GuardCommand, GuardSpec, IntegrationConfig,
    IntegrationError, Prepared, Refusal, discard_candidate, prepare_candidate, run_guard_with_env,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;

#[path = "rsi-rolling-land/base_cache.rs"]
mod base_cache;
#[path = "rsi-rolling-land/candidate_receipt.rs"]
mod candidate_receipt;
#[path = "rsi-rolling-land/known_red_timeout.rs"]
mod known_red_timeout;
#[path = "rsi-rolling-land/lander_env.rs"]
mod lander_env;
#[path = "rsi-rolling-land/landing_policy.rs"]
mod landing_policy;
#[path = "rsi-rolling-land/remote_gate.rs"]
mod remote_gate;
#[path = "rsi-rolling-land/stale_delta.rs"]
mod stale_delta;
#[path = "rsi-rolling-land/zero_tests.rs"]
mod zero_tests;

#[path = "rsi-rolling-land/canary.rs"]
mod canary;
#[path = "rsi-rolling-land/canary_runner.rs"]
mod canary_runner;
#[cfg(test)]
#[path = "../provisional_fixture.rs"]
mod provisional_fixture;
#[path = "rsi-rolling-land/test_impact.rs"]
mod test_impact;

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
        lander_env::var("RSI_LANDER_GUARD_TIMEOUT_SECS")
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
        lander_env::var("RSI_LANDER_RETRY_TIMEOUT_SECS")
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
    /// #2167: tests the landing claims to fix (`--fixes-test`). The gate must
    /// run each one green on the candidate, whatever the base says.
    fixes_tests: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LandingOutcome {
    Published,
    AlreadyOnRolling,
}

impl LandingOutcome {
    const fn label(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::AlreadyOnRolling => "already_on_rolling",
        }
    }
}

#[derive(Debug)]
struct LandReport {
    outcome: LandingOutcome,
    candidate: String,
    kind: CandidateKind,
    fetched_tip: String,
    published_tip: String,
    provisional: Vec<ProvisionalLanding>,
    base_reds: BTreeSet<String>,
    /// `known_red=` annotations for `base_reds`, empty without a snapshot.
    known_red_lines: Vec<String>,
    /// The shard command that observed each base red (#2698).
    base_red_commands: BTreeMap<String, String>,
    flakes: BTreeSet<String>,
    /// Candidate-only failures whose isolated retry could not run (#1167).
    unverified: BTreeSet<String>,
    base_reused: BTreeSet<String>,
    local_base_confirmed: BTreeSet<String>,
    base_absent_packages: BTreeSet<String>,
    base_absent_targets: BTreeSet<String>,
    base_static_inventory_reds: BTreeSet<String>,
    base_test_baseline_unavailable: BTreeSet<String>,
    canary_reused_tree: Option<String>,
    /// Registry root when the post-push canary was handed to the runner.
    canary_queued: Option<String>,
    /// Why no canary ran: every stale advance since the last full gate
    /// re-ran nothing of it (#1957).
    canary_skipped: Option<String>,
    gate_scratch: String,
    gate_disk_scratch: BTreeSet<String>,
    stale_retries: Vec<StaleRetry>,
}

// A disjoint advance reuses the earlier gate, so losing the publish race to
// one costs a fetch, a merge and a push. Several landers publishing within
// seconds of each other is normal, so those retries get a generous budget.
// An overlapping advance re-runs what it overlaps: without a wall-time budget
// (`regate_budget`) it keeps the tight count budget below.
const MAX_STALE_RETRIES: usize = 8;
const MAX_REGATED_STALE_RETRIES: usize = 2;

/// Without a wall-time budget the re-gated retries are counted. The value of
/// `RSI_LANDER_MAX_REGATED_STALE_RETRIES` can only tighten the default, never
/// raise it.
fn max_regated_stale_retries() -> usize {
    lander_env::var("RSI_LANDER_MAX_REGATED_STALE_RETRIES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map_or(MAX_REGATED_STALE_RETRIES, |value| {
            value.min(MAX_REGATED_STALE_RETRIES)
        })
}

/// #1940: `RSI_LANDER_REGATE_BUDGET_SECS` (set by the merge queue to the wall
/// time its batch has left) replaces the count of re-gated retries by a
/// wall-time budget: a stale advance re-runs only what it overlaps, any number
/// of times, while the time already spent plus the expected cost still fits.
/// Without it the count budget above applies.
fn regate_budget() -> Option<Duration> {
    lander_env::var("RSI_LANDER_REGATE_BUDGET_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// `RSI_LANDER_EXACT_GATE=1` (set by the merge queue, #1007) makes the lander
/// reuse the earlier gate across a moved tip only for what the advance provably
/// does not touch (#1940): an advance that overlaps the candidate's diff, or
/// selects tests or compile checks of the candidate's gate, re-runs exactly
/// those, so the published tip is the tree that was tested wherever it could
/// differ. Manual landings keep the file-disjoint reuse of the whole gate.
fn exact_gate_required() -> bool {
    lander_env::var("RSI_LANDER_EXACT_GATE").is_ok_and(|value| value.trim() == "1")
}

/// How much of the earlier gate a remade candidate reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StaleReuse {
    /// Every test: the advance shares no file with the candidate (manual
    /// landings; the new tree is not tested).
    Whole,
    /// Every test: the advance provably selects none of the candidate's
    /// (#1940); only the commands it can break ran again (compile checks
    /// included: a reused gate never takes the test-only omission, #2546).
    Proven,
    /// The tests, checks and suites the advance selects ran again; the rest
    /// kept their earlier result (#1940).
    Partial,
    /// Nothing: the whole gate ran again.
    Regated,
}

/// One lost publish race: the tip this attempt was built on, the newer tip it
/// was remade on, and how much of the earlier gate the remade candidate reused.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StaleRetry {
    fetched: String,
    observed: String,
    reuse: StaleReuse,
    /// Why the advance reused or re-ran what it did (`stale_reason=`); empty
    /// for an advance seen before the first gate.
    reason: String,
}

impl StaleRetry {
    /// No test of the earlier gate ran again.
    fn reused_gate(&self) -> bool {
        matches!(self.reuse, StaleReuse::Whole | StaleReuse::Proven)
    }

    fn label(&self) -> String {
        let gate = match self.reuse {
            StaleReuse::Whole => "gate_reused",
            StaleReuse::Proven => "delta_reused",
            StaleReuse::Partial => "delta_partial",
            StaleReuse::Regated => "regated",
        };
        format!("{}..{}:{gate}", self.fetched, self.observed)
    }
}

/// #1957: how many stale advances came since the last full gate (a re-gated
/// retry, or the first gate), when there were some and each re-ran nothing of
/// it, so the tree published now has that gate's results.
fn proven_since_full_gate(retries: &[StaleRetry]) -> Option<usize> {
    let since: Vec<&StaleRetry> = retries
        .iter()
        .rev()
        .take_while(|retry| retry.reuse != StaleReuse::Regated)
        .collect();
    (!since.is_empty() && since.iter().all(|retry| retry.reuse == StaleReuse::Proven))
        .then_some(since.len())
}

fn stale_retry_lines(retries: &[StaleRetry]) -> Vec<String> {
    let mut lines = vec![format!("stale_attempts={}", retries.len())];
    for (index, retry) in retries.iter().enumerate() {
        lines.push(format!("stale_retry_{}={}", index + 1, retry.label()));
        if !retry.reason.is_empty() {
            lines.push(format!("stale_reason_{}={}", index + 1, retry.reason));
        }
    }
    lines
}

type RemoteShardResults =
    BTreeMap<(String, String), Result<rsid::integration::GuardCommandReport, String>>;

enum BaseTestEvidence {
    Observed(BTreeSet<String>),
    CompilationUnavailable,
}

/// #2467: candidate worktree -> the `(test, check)` targets of the warm slot
/// bound to it. Only a bound worktree builds there; the base side and every
/// other tree keep their private targets.
fn warm_bindings() -> std::sync::MutexGuard<'static, BTreeMap<PathBuf, (PathBuf, PathBuf)>> {
    static BINDINGS: Mutex<BTreeMap<PathBuf, (PathBuf, PathBuf)>> = Mutex::new(BTreeMap::new());
    BINDINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
struct TestGate {
    // An entry is reusable only for this exact rolling commit and command.
    base_cache: BTreeMap<(String, String), BTreeSet<String>>,
    // Preserve QA origin when a base result is reused again in this process.
    qa_cache_provenance: BTreeMap<(String, String), String>,
    base_reds: BTreeSet<String>,
    /// Failure text per base red, from a base report or its persistent cache.
    base_red_texts: BTreeMap<String, String>,
    /// The test command that observed each base red (#2698).
    base_red_commands: BTreeMap<String, String>,
    // Known-failure snapshot loaded once per landing (annotation only).
    known_failures: Option<Snapshot>,
    flakes: BTreeSet<String>,
    unverified: BTreeSet<String>,
    base_reused: BTreeSet<String>,
    local_base_confirmed: BTreeSet<String>,
    // Workspace package names per base commit; `None` when unproven.
    base_packages: BTreeMap<String, Option<BTreeSet<String>>>,
    base_absent_packages: BTreeSet<String>,
    base_absent_targets: BTreeSet<String>,
    // Every target classification a real base run made, in order, so the caller
    // can attach the reason to the empty baseline it caches (#1812).
    base_absent_log: Vec<String>,
    // `<base>:<package>:<target>` behind a cached empty baseline, by cache key.
    base_cache_absent: BTreeMap<(String, String), BTreeSet<String>>,
    // `<base>:<reason>` for each base whose shard-script static inventory
    // check refused; its results come from the direct bounded harness.
    base_static_inventory_reds: BTreeSet<String>,
    // Compilation failure is unavailable evidence, never a cached empty set.
    base_test_baseline_unavailable: BTreeSet<String>,
    base_worktrees: BTreeMap<String, PathBuf>,
    /// #2833: base commits whose worktree target was given the e2e prebuild,
    /// so an e2e test on the base finds `rsid` beside `rsi`. `None` is a built
    /// prebuild; `Some(detail)` is a failed one (#2863), which leaves the base
    /// e2e result unavailable, never red.
    base_e2e_prebuilt: BTreeMap<String, Option<String>>,
    cache_root: Option<PathBuf>,
    /// Where green candidate-gate receipts live (#1312); `None` disables them.
    receipt_root: Option<PathBuf>,
    /// True only while `land_candidate` runs the candidate's first full gate:
    /// the one gate a receipt may stand in for. A stale re-gate and the
    /// post-publication canary always run.
    candidate_receipts: bool,
    /// A stale re-gate that keeps part of the candidate's earlier gate
    /// (#1940): the commands it does not name are not run again.
    reuse: Option<GateReuse>,
    /// `(reused, rerun)` commands of the gate run under `reuse`, for the
    /// `gate_timing=` evidence.
    reuse_counts: Option<(usize, usize)>,
    /// When this landing began: the re-gate wall-time budget counts from it.
    started: Option<Instant>,
    /// Wall seconds of the last gate run that executed its whole selection:
    /// what running it again is expected to cost.
    full_gate_secs: u64,
    remote: Option<Arc<Mutex<remote_gate::Executor>>>,
    // Remote shard results run ahead of the sequential gate loop (#970),
    // keyed by (commit, shard) and consumed once.
    remote_results: Mutex<RemoteShardResults>,
    scratch: Option<GateScratch>,
    /// Fixture admission result, fixed across publication retries (#490).
    #[cfg(test)]
    scratch_override: Option<GateScratch>,
    disk_scratch_used: BTreeSet<String>,
    /// Base guards actually executed (cache misses) in this gate (#1132).
    base_runs: usize,
    /// Seconds spent compiling shard binaries ahead of the gate loop (#1132).
    prebuild_secs: u64,
    /// Seconds in candidate test runs and in base comparisons (#1244).
    candidate_secs: u64,
    base_secs: u64,
    /// Wall seconds of the compile checks run beside the prebuild (#1244).
    checks_secs: u64,
    /// Wall seconds of the non-test release-build checks (#1970), a subset of
    /// `checks_secs`.
    release_check_secs: u64,
    /// #2424: wall seconds of each step of the `checks_secs` phase (static
    /// guards, Python guards, every compile-check batch, the beside-running
    /// prebuild), in execution order, so the dominant step is visible.
    check_steps: Vec<(String, u64)>,
    /// #2092: library selection evidence retained in the queue outcome.
    selection_evidence: String,
    /// #3887: what the source touched, so a gate that runs no test command says so.
    zero_test_scope: zero_tests::ZeroTestScope,
    /// Test guards whose green, proven candidate needed no base (#1244).
    base_skipped: usize,
    /// Test hook: run the prebuild even under `cfg(test)`.
    prebuild_forced: bool,
    /// The explicit focused rsid filters of this gate, for the empty-filter
    /// refusal and the failure attribution of the combined run (#1261).
    focused_filters: Vec<FocusedFilter>,
    /// #2167: the tests the landing claims to fix, and those of them a
    /// candidate run of this landing has seen pass (kept across re-gates).
    claimed_fixes: BTreeSet<String>,
    claimed_green: BTreeSet<String>,
    /// #2467: the warm target slot this lander leased, when one was free.
    warm: Option<rsid::lander_warm_target::WarmSlot>,
    /// #2467: a prebuild compile failed (the gate loop then reports it), so
    /// the build phase did not prove the warm slot's contents.
    prebuild_failed: bool,
}

/// What a stale re-gate keeps of the candidate's earlier gate (#1940).
#[derive(Clone, Debug)]
struct GateReuse {
    verdict: stale_delta::Verdict,
    /// The Python suites the advance selects; every other suite keeps its
    /// earlier result unless the verdict re-runs them all.
    python: Vec<GuardCommand>,
    /// #1957: the rsid-family tests the advance can observe. A family test
    /// command the verdict selects runs only for these; `None` re-runs every
    /// family command the verdict selects.
    family: Option<FamilyObservers>,
}

impl GateReuse {
    /// Every test and Python suite keeps its result and the compile checks
    /// run again: a manual landing's file-disjoint advance (#1007).
    fn every_test(reason: &str) -> Self {
        Self {
            verdict: stale_delta::Verdict {
                reason: reason.to_owned(),
                tests: stale_delta::Scope::Nothing,
                checks: stale_delta::Scope::Everything,
                python_everything: false,
                family_closed: false,
                family_filters: Vec::new(),
            },
            python: Vec::new(),
            family: None,
        }
    }
}

/// #1957: the rsid-family tests a stale advance can observe: the whole
/// call-graph closure ([`test_impact::Budget::WHOLE`]) of the difference
/// between the gated tree and the remade one, plus the advance's selector
/// filters on the family (a reader of a changed file outside the crates).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FamilyObservers {
    /// Library tests, by full libtest name.
    names: BTreeSet<String>,
    /// Focused atoms of the selector's library filters.
    atoms: Vec<String>,
    /// Binary and integration test targets (`rsid:bin:NAME`, `rsid:test:NAME`).
    targets: BTreeSet<String>,
}

impl FamilyObservers {
    /// Whether a focused atom can select a test the advance observes. An atom
    /// the lander cannot match always can.
    fn observes_atom(&self, atom: &str) -> bool {
        let Some(mine) = FocusedMatch::parse(atom) else {
            return true;
        };
        let named = match &mine {
            FocusedMatch::Exact(name) => self.names.contains(name),
            _ => self.names.iter().any(|name| mine.matches(name)),
        };
        named
            || self.atoms.iter().any(|theirs| {
                FocusedMatch::parse(theirs).is_none_or(|theirs| mine.may_overlap(&theirs))
            })
    }

    /// The candidate atoms a re-run keeps, in their order.
    fn observed_atoms(&self, atoms: &[String]) -> Vec<String> {
        atoms
            .iter()
            .filter(|atom| self.observes_atom(atom))
            .cloned()
            .collect()
    }

    fn evidence(&self) -> String {
        format!(
            "names={} atoms={} targets={}",
            self.names.len(),
            self.atoms.len(),
            self.targets.len()
        )
    }
}

/// #1957: the [`FamilyObservers`] of the advance that turned the gated tree
/// `tested` into `remade`. Both trees carry the candidate, so a call edge the
/// candidate adds into code the advance changed is in the closure. `Err`
/// names why the closure cannot stand for the family: the family re-runs
/// whole.
fn family_observers(
    worktree: &Path,
    tested: &str,
    remade: &str,
    filters: &[String],
) -> Result<FamilyObservers, String> {
    let mut observers = FamilyObservers::default();
    for filter in filters {
        let target = stale_delta::filter_target(filter)
            .ok_or_else(|| format!("unreadable_filter:{filter}"))?;
        if target != "rsid:lib" {
            observers.targets.insert(target);
            continue;
        }
        let (package, value) = filter
            .split_once('=')
            .ok_or_else(|| format!("unreadable_filter:{filter}"))?;
        let command = rsid_family_filter_command(package, value)
            .map_err(|_| format!("unreadable_filter:{filter}"))?;
        // A whole shard has no atom to match.
        let filterset =
            rsid_lib_filterset(&command).ok_or_else(|| format!("whole_shard:{filter}"))?;
        for atom in focused_atoms(filterset) {
            if !observers.atoms.contains(&atom) {
                observers.atoms.push(atom);
            }
        }
    }
    let selection =
        test_impact::select_within(worktree, tested, remade, test_impact::Budget::WHOLE)
            .ok_or("crate_wide_or_unreadable_diff")?;
    match selection.lib {
        test_impact::LibTests::Everything(reason) => {
            return Err(format!("closure_everything:{}", reason.replace(' ', "_")));
        }
        test_impact::LibTests::Tests(names) => observers.names = names,
    }
    for (package, target) in selection.targets {
        observers
            .targets
            .insert(format!("{}:{target}", stale_delta::family(&package)));
    }
    Ok(observers)
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
    /// Print and keep one step of the checks phase (#2424).
    fn record_check_step(&mut self, name: &str, secs: u64) {
        println!("gate_step name={name} secs={secs}");
        self.check_steps.push((name.to_owned(), secs));
    }

    /// #2467: lease a warm target slot for this lander when its cargo target is
    /// the daemon queue's (outside the repository). No free slot, a disabled
    /// feature or an unusable directory leaves the gate on private targets.
    fn lease_warm_slot(&mut self, cargo_target_dir: &Path, workspace_parent: &Path) {
        if self.warm.is_some()
            || cfg!(test)
            || !rsid::lander_warm_target::enabled()
            || workspace_parent != cargo_target_dir
        {
            return;
        }
        let root = cargo_target_dir.join(rsid::lander_warm_target::WARM_DIR);
        match rsid::lander_warm_target::WarmSlot::acquire(
            &root,
            rsid::lander_warm_target::configured_slots(),
        ) {
            Some(slot) => {
                println!("warm_slot={}", slot.index());
                self.warm = Some(slot);
            }
            None => println!("warm_slot=none"),
        }
    }

    /// Bind the slot and return its fixed checkout for all gate commands.
    /// A slot that cannot be bound is given up for the rest of the landing.
    fn bind_warm_targets(&mut self, worktree: &Path) -> PathBuf {
        let Some(slot) = self.warm.as_mut() else {
            return worktree.to_path_buf();
        };
        match slot.bind(worktree) {
            Ok(outcome) => {
                println!("{}", outcome.evidence(slot.index()));
                warm_bindings().insert(
                    outcome.worktree.clone(),
                    (slot.test_target(), slot.check_target()),
                );
                outcome.worktree
            }
            Err(error) => {
                println!("warm_target=unavailable reason={error}");
                // Do not leave a previous bind pointing at targets whose lease
                // is about to be released.
                let targets = (slot.test_target(), slot.check_target());
                warm_bindings().retain(|_, pair| *pair != targets);
                self.warm = None;
                worktree.to_path_buf()
            }
        }
    }

    /// #2467: the compile checks and the prebuild succeeded in the bound
    /// worktree, so the slot can hand its unchanged workspace crates to the
    /// next gate.
    fn commit_warm_targets(&mut self) {
        let Some(slot) = self.warm.as_mut() else {
            return;
        };
        match slot.commit() {
            Ok(0) => {}
            Ok(files) => println!("warm_target=committed files={files}"),
            Err(error) => println!("warm_target=commit_failed reason={error}"),
        }
    }

    fn for_landing() -> Result<Self, String> {
        // Hermetic binary tests opt in to a temporary cache explicitly.
        let cache_root = if cfg!(test) {
            None
        } else {
            Some(base_cache::default_root()?)
        };
        let receipt_root = if cfg!(test) {
            None
        } else {
            Some(candidate_receipt::default_root()?)
        };
        Ok(Self {
            cache_root,
            receipt_root,
            known_failures: load_known_failures(),
            ..Self::default()
        })
    }
}

/// Snapshot path: `RSI_KNOWN_FAILURES_SNAPSHOT`, else `$HOME/.rsi/qa/known-failures.v1.json`.
fn known_failures_path() -> PathBuf {
    lander_env::var_os("RSI_KNOWN_FAILURES_SNAPSHOT").map_or_else(
        || {
            let home = lander_env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
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

/// #2167: the refusal for claimed fixes the gate did not see run green.
fn claimed_fix_still_red_message(names: &[String], why: &str) -> String {
    format!(
        "{}: {}; {why}",
        rsi_common::rolling_queue::QUEUE_CLAIMED_FIX_STILL_RED,
        names.join(", ")
    )
}

/// #3320: why claimed tests the gate never saw run green were not proven, and
/// the next step that fits their kind. A Rust test needs a filter that selects
/// it; no filter can select a Python test, so a Python claim is run by id.
fn unproven_claim_next_step(unproven: &[String]) -> String {
    const BASE: &str = "no candidate run of the gate ran it green";
    let (python, rust): (Vec<_>, Vec<_>) = unproven.iter().partition(|name| is_python_claim(name));
    let rust_step = "its selection did not select it; add a test filter that selects it";
    let python_step = "the gate runs a claimed Python test by its full unittest id \
        (scripts.tests.<module>.<Class>.<method>, or tools.tests.) when that test module \
        exists on the candidate, and it did not pass; no test filter selects a Python test, \
        so name the full id";
    match (rust.is_empty(), python.is_empty()) {
        (_, true) => format!("{BASE} ({rust_step})"),
        (true, false) => format!("{BASE} ({python_step})"),
        (false, false) => format!("{BASE} (Rust: {rust_step}; Python: {python_step})"),
    }
}

/// The test names in a refusal built by `claimed_fix_still_red_message`.
fn claimed_fix_names(message: &str) -> Vec<String> {
    let marker = format!(
        "{}: ",
        rsi_common::rolling_queue::QUEUE_CLAIMED_FIX_STILL_RED
    );
    let mut names = Vec::new();
    for line in message.lines() {
        let Some((_, after)) = line.split_once(&marker) else {
            continue;
        };
        let list = after.split_once("; ").map_or(after, |(names, _)| names);
        for name in list.split(", ").map(str::trim) {
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
    /// #2705: the base reds the gate had observed before it failed.
    base_red_evidence: BaseRedEvidence,
    message: String,
}

/// The base reds a gate observed and the commands that observed them (#2698).
/// A failed gate reports them too: a red the base shares is evidence whatever
/// else the gate found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct BaseRedEvidence {
    base_reds: BTreeSet<String>,
    known_red_lines: Vec<String>,
    commands: BTreeMap<String, String>,
}

impl BaseRedEvidence {
    fn of(gate: &TestGate) -> Self {
        Self {
            base_reds: gate.base_reds.clone(),
            known_red_lines: known_red_lines(
                &gate.base_reds,
                &gate.base_red_texts,
                gate.known_failures.as_ref(),
            ),
            commands: gate.base_red_commands.clone(),
        }
    }

    /// The `base_red=`, `known_red=` and `base_red_command=` report lines.
    fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .base_reds
            .iter()
            .map(|name| format!("base_red={name}"))
            .collect();
        lines.extend(self.known_red_lines.iter().cloned());
        lines.extend(
            self.commands
                .iter()
                .map(|(name, command)| format!("base_red_command={name}\t{command}")),
        );
        lines
    }
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
            base_red_evidence: BaseRedEvidence::default(),
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
            base_red_evidence: BaseRedEvidence::default(),
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

    /// Hub #3873: the gate could not read the remote `rolling` ref (`ls-remote`
    /// or `fetch` failed after its retries). That is infrastructure, never a
    /// verdict on the sources, so the queue requeues instead of bisecting. Only
    /// a failure before publication qualifies, and one that also names a failing
    /// test is a real red.
    fn is_remote_read_failure(&self) -> bool {
        const MARKERS: [&str; 6] = [
            "cannot read remote rolling tip",
            "remote rolling fetch failed (exit",
            "remote rolling fetch timed out",
            "remote rolling lookup timed out",
            "cannot fetch remote rolling",
            "cannot wait for remote rolling",
        ];
        self.state == PublicationState::NotPublished
            && self.kind == FailureKind::General
            && MARKERS.iter().any(|marker| self.message.contains(marker))
            && failing_test_names(&self.message).is_empty()
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
        if self.is_remote_read_failure() {
            lines.push(format!(
                "failure_class={}",
                rsi_common::rolling_queue::LANDER_FAILURE_CLASS_REMOTE_READ
            ));
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
        lines.extend(self.base_red_evidence.lines());
        lines.extend(
            failing_test_names(&self.message)
                .into_iter()
                .chain(claimed_fix_names(&self.message))
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

/// `rsi-rolling-land --explain-selection BASE CANDIDATE [--repo PATH] [--whole]`: print
/// the unfiltered rsid-family selection with call distances, mandatory
/// entries and the bounded cut. Read-only: no fetch, build, gate or publish.
fn explain_selection_main() -> i32 {
    let mut args = std::env::args().skip(2);
    let (mut positional, mut repo) = (Vec::new(), PathBuf::from("."));
    let mut budget = test_impact::Budget::GATE;
    while let Some(arg) = args.next() {
        if arg == "--whole" {
            budget = test_impact::Budget::WHOLE;
            continue;
        }
        if arg != "--repo" {
            positional.push(arg);
            continue;
        }
        let Some(path) = args.next() else {
            eprintln!("--repo expects a path");
            return 2;
        };
        repo = PathBuf::from(path);
    }
    let [base, candidate] = positional.as_slice() else {
        eprintln!(
            "usage: rsi-rolling-land --explain-selection BASE CANDIDATE [--repo PATH] [--whole]"
        );
        return 2;
    };
    let selection = test_impact::select_explained(&repo, base, candidate, budget);
    print!("{}", test_impact::render_explanation(selection.as_ref()));
    0
}

fn main() {
    // The detached single canary runner (#951) shares this binary.
    match std::env::args().nth(1).as_deref() {
        Some("--canary-runner") => std::process::exit(canary_runner::run_main()),
        Some("--canary-status") => {
            std::process::exit(canary_runner::status_main(std::env::args().nth(2)))
        }
        Some("--explain-selection") => std::process::exit(explain_selection_main()),
        _ => {}
    }
    // #192: every knob and child environment below reads this one snapshot.
    lander_env::init();
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
            println!("landing_outcome={}", report.outcome.label());
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
            if let Some(reason) = &report.canary_skipped {
                println!(
                    "canary: skipped tip={} reason={reason}",
                    report.published_tip
                );
            }
            for name in &report.base_reds {
                println!("base_red={name}");
            }
            for line in &report.known_red_lines {
                println!("{line}");
            }
            for (name, command) in &report.base_red_commands {
                println!("base_red_command={name}\t{command}");
            }
            for name in &report.flakes {
                println!("candidate_flake={name}");
            }
            for name in &report.unverified {
                println!("candidate_unverified={name}");
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
            for entry in &report.base_absent_targets {
                println!("base_target_absent={entry}");
            }
            for entry in &report.base_static_inventory_reds {
                println!("base_static_inventory_red={entry}");
            }
            for entry in &report.base_test_baseline_unavailable {
                println!("base_test_baseline_unavailable={entry}");
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
    rsid_core::process_control::track_bounded_process_groups();
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
        terminate_tracked_child_groups();
    })
    .await
}

fn terminate_tracked_child_groups() {
    use nix::sys::signal::{Signal, killpg};
    let groups = rsid_core::process_control::tracked_bounded_process_groups();
    if groups.is_empty() {
        return;
    }
    for group in &groups {
        let _ = killpg(*group, Signal::SIGTERM);
    }
    // The lander runs a current-thread runtime. A synchronous grace period
    // pauses publication while TERM handlers in the child groups run.
    std::thread::sleep(Duration::from_secs(1));
    for group in groups {
        let _ = killpg(group, Signal::SIGKILL);
    }
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
    parse_args(args, lander_env::var_os("CARGO_BUILD_JOBS").as_deref())
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
    let mut fixes_tests: Vec<String> = Vec::new();
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
            "--test-filter" => {
                let value = next_value(&mut args, "--test-filter")?;
                if let Some(detail) = rsi_common::rolling_queue::oversize_filter_detail(
                    "--test-filter",
                    std::slice::from_ref(&value),
                ) {
                    return Err(format!("queue_filter_invalid: {detail}"));
                }
                test_filters.push(value);
            }
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
            "--fixes-test" => {
                let value = next_value(&mut args, "--fixes-test")?;
                if !rsi_common::rolling_queue::valid_test_name(&value) {
                    return Err(format!("--fixes-test expects a test name: {value}"));
                }
                if !fixes_tests.contains(&value) {
                    fixes_tests.push(value);
                }
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
        fixes_tests,
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
    "usage: rsi-rolling-land --explain-selection BASE CANDIDATE [--repo PATH] [--whole] (read-only selection report) | rsi-rolling-land [--repo PATH] [--remote NAME] --accepted SOURCE|BASE:SOURCE [--accepted SOURCE|BASE:SOURCE ...] [--test-filter PACKAGE=FILTER ...] [--tmpfs-min-free-gb N] [--disk-scratch-shard SHARD ...] [--expected-tip SHA] [--fixes-test NAME ...] [--remote-gate-host USER@HOST --remote-gate-dir PATH --remote-gate-identity PATH [--remote-gate-run-as USER]]\nFor a landing candidate, omitted BASE is merge-base(SOURCE, current landing target tip); an explicit BASE must equal it. Already-integrated sources require the explicit historical BASE.\nAn rsid shard filter is rsid=shard:SHARD[:FILTERSET], for example rsid=shard:session-02:test(manager_recovery_). The full shard inventory is checked before any focused run.\n--tmpfs-min-free-gb requires N GiB free on /dev/shm before private gate TMPDIR is used (default 12); otherwise gates use sandbox target scratch. --disk-scratch-shard gives a named shard disk scratch on both sides.\nRemote shard execution is opt-in and refuses publication on missing evidence or mismatched commit/fingerprint. The SSH host must already be in known_hosts. Only full rsid shards run remotely; local regression comparison and isolated retries are preserved.\n--expected-tip is used by the merge queue: an initial fetch that differs from SHA is an out-of-band advance charged against the re-gated retry budget.\n--fixes-test NAME claims the landing fixes that test (#2167): the gate must run it green on the candidate, even when the base is red on it too, or the landing is refused claimed_fix_still_red.\nCARGO_BUILD_JOBS: positive integer from 1 to 6; defaults to 4 when unset."
}

/// #1140/#1150: the lander's startup sweep of dead owners' workspaces (on after
/// the hardened scratch-reclaim core's final check). Registration of the owner and
/// the private clone records each workspace.
const LANDER_STARTUP_SWEEP_ENABLED: bool = true;

#[allow(clippy::too_many_lines)] // Keeps fetch, preparation, and owned cleanup visibly ordered.
async fn land(options: Options) -> Result<LandReport, LandFailure> {
    land_with_gate(options, TestGate::for_landing()?).await
}

async fn land_with_gate(
    options: Options,
    mut test_gate: TestGate,
) -> Result<LandReport, LandFailure> {
    let mut outcome = land_with_gate_ref(options, &mut test_gate).await;
    // #2705: a failed gate keeps the base reds it observed first.
    if let Err(failure) = &mut outcome {
        failure.base_red_evidence = BaseRedEvidence::of(&test_gate);
    }
    outcome
}

#[allow(clippy::too_many_lines)] // Keeps fetch, preparation, and owned cleanup visibly ordered.
async fn land_with_gate_ref(
    options: Options,
    test_gate: &mut TestGate,
) -> Result<LandReport, LandFailure> {
    let mut options = options;
    // #2167: only the landing's own gate carries its claims; a forward revert
    // or the canary builds a gate of its own.
    test_gate.claimed_fixes = options.fixes_tests.iter().cloned().collect();
    let cargo_target_dir = validate_cargo_target_dir()?;
    #[cfg(not(test))]
    let selected_scratch = select_gate_scratch(&cargo_target_dir, options.tmpfs_min_free_gb)?;
    #[cfg(test)]
    let selected_scratch = match test_gate.scratch_override.take() {
        Some(scratch) => scratch,
        None => select_gate_scratch(&cargo_target_dir, options.tmpfs_min_free_gb)?,
    };
    println!("gate_scratch={}", selected_scratch.report());
    test_gate.scratch = Some(selected_scratch);
    test_gate.started.get_or_insert_with(Instant::now);
    let repo = options
        .repo
        .canonicalize()
        .map_err(|error| format!("cannot resolve repository: {error}"))?;
    let remote_url = git_text(&repo, &["remote", "get-url", "--push", &options.remote])?;
    let workspace_parent = landing_workspace_parent(&repo, &cargo_target_dir)?;
    test_gate.lease_warm_slot(&cargo_target_dir, &workspace_parent);
    // #932: a killed lander (every daemon restart, SIGKILL, reboot) leaves its
    // private workspace behind. Sweep recorded, dead-owner leftovers before
    // taking disk (#1140: bounded, off the async executor).
    if LANDER_STARTUP_SWEEP_ENABLED {
        let parent = workspace_parent.clone();
        if let Ok(swept) = tokio::task::spawn_blocking(lander_env::propagate(move || {
            rsid::agent_scratch_reclaim::sweep_lander_workspace(&parent)
        }))
        .await
            && swept.reclaimed > 0
        {
            println!(
                "lander_scratch_swept={} bytes={}",
                swept.reclaimed, swept.reclaimed_bytes
            );
        }
    }
    let temp = tempfile::Builder::new()
        .prefix(rsid::agent_scratch_reclaim::LANDER_SCRATCH_PREFIX)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&workspace_parent)
        .map_err(|error| format!("cannot create private landing workspace: {error}"))?;
    if let Err(error) = rsid::agent_scratch_reclaim::register_lander_owner(temp.path(), &repo) {
        eprintln!("lander: cannot register scratch owner (workspace will not be swept): {error}");
    }
    // #1140: the private clone is the one repository a sweep may delete without
    // proving its commits published, so the lander creates its (empty) directory
    // first and registers that exact directory before cloning into it.
    let private_repo = temp
        .path()
        .join(rsid::agent_scratch_reclaim::LANDER_CLONE_DIR);
    std::fs::create_dir(&private_repo)
        .map_err(|error| format!("cannot create private clone directory: {error}"))?;
    if let Err(error) =
        rsid::agent_scratch_reclaim::register_lander_clone(temp.path(), &private_repo)
    {
        eprintln!("lander: cannot register private clone (workspace will not be swept): {error}");
    }
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
    // Preserve the committed landing guard without checking out the full source
    // tree. The released-migration guard is compiled into this binary (#361).
    if !landing_policy::native_guard_installed() {
        copy_committed_script(&private_repo, "scripts/rolling-landing-guard.py")?;
    }
    for pair in &mut options.accepted {
        if !pair.base.is_empty() {
            pair.base = resolve_accepted_commit(&private_repo, &repo, "BASE", &pair.base)?;
        }
        pair.source = resolve_accepted_commit(&private_repo, &repo, "SOURCE", &pair.source)?;
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
        if regate_budget().is_none() && max_regated_stale_retries() == 0 {
            return Err(format!(
                "stale retries exhausted: rolling advanced from the probed {probed} to \
                 {fetched_tip} before the initial fetch and no re-gated retry remains"
            )
            .into());
        }
        stale.push(StaleRetry {
            fetched: probed.clone(),
            observed: fetched_tip.clone(),
            reuse: StaleReuse::Regated,
            reason: "moved_before_first_gate".to_owned(),
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
        test_gate.receipt_root = None;
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
            if provisional_plain_merge(&unit, false) {
                // Already rolling head + 1 (#1655), or the identical migration
                // landed through another source (#1768): merge the source as is.
                None
            } else {
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
            }
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
                let prepared = prepare_candidate(
                    &config,
                    &private_repo,
                    TARGET,
                    &expected_tip,
                    &pair.source,
                    &scratch,
                )
                .await
                .map_err(|error| stale_error(error, &fetched_tip))?;
                with_derived_pin(&config, &private_repo, &expected_tip, prepared).await
            }
        }
        .await;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                // A batch caller needs the culprit: this source conflicted with
                // the target plus the sources merged before it (#1119).
                let error = rsid::rolling_queue::mark_conflict_source(error, &pair.source);
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
                        test_gate,
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
        return Ok(already_on_rolling_report(&fetched_tip, test_gate, &stale));
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
        test_gate,
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
                format!(
                    "landing completed but candidate cleanup failed: {}",
                    cleanup_failure(&candidate.handle.worktree, &error)
                ),
                &candidate,
                &fetched_tip,
            );
            failure.kind = FailureKind::Cleanup;
            Err(failure)
        }
        (Err(error), Err(cleanup)) => Err(LandFailure {
            message: format!(
                "{}; candidate cleanup also failed: {}",
                error.message,
                cleanup_failure(&candidate.handle.worktree, &cleanup)
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
            // No historical delta was supplied. Ancestry already proves that
            // this exact source landed; an explicit base still gets its full
            // lost-hunk check below.
            pair.base = pair.source.clone();
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

/// Whether the source ADDS a provisional migration declaration (#3337). A
/// source that only edits or deletes an already released declaration appends no
/// migration, so it is an ordinary source: counting it made the renumber tool
/// refuse "provisional declaration has no appended migration".
fn has_provisional_declaration(repo: &Path, pair: &AcceptedPair) -> Result<bool, String> {
    let output = git_output(
        repo,
        &[
            "diff",
            "--no-renames",
            "--diff-filter=A",
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

/// The commit a provisional-tool step judges against: its `--target`, or the
/// `target` its unit file records. Empty when neither names one, which an
/// authenticated guard refuses.
fn provisional_target(args: &[&str]) -> String {
    let after = |flag: &str| -> Option<&str> {
        args.iter()
            .position(|arg| *arg == flag)
            .and_then(|index| args.get(index + 1))
            .copied()
    };
    if let Some(target) = after("--target") {
        return target.to_owned();
    }
    after("--unit-file")
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|unit| unit["target"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// One step of the provisional-migration tool, run as a bounded child of the
/// lander (#361): the `rolling-migration-renumber` guard of the executable
/// installed beside the lander, never a committed or candidate-authored tool.
/// The child gets the lander environment without `GIT_*` and `RSI_*`.
fn provisional_command(repo: &Path, args: &[&str]) -> Result<Value, String> {
    let target = provisional_target(args);
    let mut argv = vec![
        "--repo",
        repo.to_str()
            .ok_or("provisional repository path is not UTF-8")?,
    ];
    argv.extend_from_slice(args);
    let run =
        landing_policy::run_repo_guard(repo, &target, repo, "rolling-migration-renumber", &argv);
    match run {
        rsid::repo_guard_call::GuardRun::Exited {
            code: 0, stdout, ..
        } => serde_json::from_slice(&stdout)
            .map_err(|error| format!("provisional migration tool returned invalid JSON: {error}")),
        other => Err(format!("provisional migration refused: {}", other.reason())),
    }
}

fn provisional_version(value: &Value, name: &str) -> Result<u32, String> {
    value[name]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| format!("provisional migration result lacks {name}"))
}

fn provisional_plain_merge(unit: &Value, has_original_proof: bool) -> bool {
    unit["renumber_noop"].as_bool() == Some(true)
        && (!has_original_proof || unit["migration_already_landed"].as_bool() == Some(true))
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
    let configured = lander_env::var_os("CARGO_TARGET_DIR").ok_or_else(|| {
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
    // This user's room, not the filesystem's: systemd gives each user a block
    // quota of 80% of the /tmp and /dev/shm tmpfs mounts, and statvfs reports
    // free space the quota may not allow (gates failed with "Disk quota
    // exceeded", #938).
    let enough_space = user_writable_bytes(tmpfs)
        .is_some_and(|free| free >= u128::from(min_free_gb) * 1024 * 1024 * 1024);
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

/// One user's block quota on one filesystem, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlockQuota {
    hard_limit: u128,
    used: u128,
}

/// Bytes this process's user may still write under `path`: the filesystem's
/// free space, capped by the user's block quota there when one is set.
fn user_writable_bytes(path: &Path) -> Option<u128> {
    let stats = nix::sys::statvfs::statvfs(path).ok()?;
    let free =
        u128::from(stats.blocks_available()).saturating_mul(u128::from(stats.fragment_size()));
    Some(cap_by_quota(free, user_block_quota(path)))
}

/// `free`, capped by what `quota` leaves; a zero hard limit means no limit.
fn cap_by_quota(free: u128, quota: Option<BlockQuota>) -> u128 {
    match quota {
        Some(quota) if quota.hard_limit > 0 => {
            free.min(quota.hard_limit.saturating_sub(quota.used))
        }
        _ => free,
    }
}

/// `struct if_dqblk` from `<linux/quota.h>`, filled by `Q_GETQUOTA`.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default)]
struct IfDqblk {
    /// `dqb_bhardlimit`, `dqb_bsoftlimit`, `dqb_curspace`, `dqb_ihardlimit`,
    /// `dqb_isoftlimit`, `dqb_curinodes`, `dqb_btime`, `dqb_itime`.
    fields: [u64; 8],
    valid: u32,
}

/// This user's block quota on the filesystem holding `path`, read with
/// `quotactl_fd(2)` (Linux 5.14+; tmpfs keeps quotas from 6.6). `None` when the
/// filesystem keeps no quota for the user or the kernel cannot report one.
#[cfg(target_os = "linux")]
fn user_block_quota(path: &Path) -> Option<BlockQuota> {
    use std::os::fd::AsRawFd;
    const Q_GETQUOTA: u32 = 0x80_0007;
    const USRQUOTA: u32 = 0;
    const QIF_BLIMITS: u32 = 1;
    const QIF_SPACE: u32 = 4;
    // `dqb_bhardlimit` counts 1 KiB quota blocks (`QIF_DQBLKSIZE`).
    const QUOTA_BLOCK_BYTES: u128 = 1024;
    let dir = std::fs::File::open(path).ok()?;
    let uid = nix::unistd::geteuid().as_raw();
    let mut quota = IfDqblk::default();
    // SAFETY: quotactl_fd(2) with Q_GETQUOTA writes one `struct if_dqblk`
    // through the last argument, which points at a live `repr(C)` value of
    // that layout; `dir` keeps the descriptor open for the call.
    let status = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_quotactl_fd,
            dir.as_raw_fd(),
            (Q_GETQUOTA << 8) | USRQUOTA,
            uid,
            &raw mut quota,
        )
    };
    let wanted = QIF_BLIMITS | QIF_SPACE;
    (status == 0 && (quota.valid & wanted) == wanted).then(|| BlockQuota {
        hard_limit: u128::from(quota.fields[0]) * QUOTA_BLOCK_BYTES,
        used: u128::from(quota.fields[2]),
    })
}

#[cfg(not(target_os = "linux"))]
const fn user_block_quota(_path: &Path) -> Option<BlockQuota> {
    None
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
    landing_policy::check_with_proof(repo, repo, fetched_tip, &candidate.oid, &proved_versions)
        .map_err(|error| LandFailure::policy(error, candidate, fetched_tip))?;
    // A merge can contain a source's ancestry without adding any content:
    // its patch reached rolling through another source. Check every accepted
    // delta before returning the tip that already contains the result.
    if same_tree(repo, fetched_tip, &candidate.oid)? {
        for pair in &options.accepted {
            let proof = provisional
                .iter()
                .find(|unit| unit.source == pair.source && unit.base == pair.base)
                .map(|unit| unit.proof_path.as_path());
            run_guard_pair_static(repo, pair, fetched_tip, &candidate.oid, false, proof).await?;
        }
        return Ok(already_on_rolling_report(fetched_tip, test_gate, stale));
    }
    test_gate.reuse_counts = None;
    // #1312: only this first full gate may be answered by a receipt a failed
    // earlier publication left; the policy checks above and the publication
    // below stay live either way.
    test_gate.candidate_receipts = provisional.is_empty();
    let gated = run_guard_pairs_once(
        repo,
        options,
        provisional,
        fetched_tip,
        &candidate.oid,
        &candidate.handle.worktree,
        test_gate,
    )
    .await;
    test_gate.candidate_receipts = false;
    gated.map_err(|error| {
        LandFailure::candidate(
            PublicationState::NotPublished,
            error,
            candidate,
            fetched_tip,
        )
    })?;
    // A stale advance may reuse part of the earlier gate without testing its
    // new tree (#1940). Only a gate executed in full for this candidate can
    // stand in for the post-publication canary.
    let candidate_was_gated = test_gate.reuse.is_none();
    // #1957: a reduced re-gate that ran no command beside the static guards
    // kept every earlier result: the advance was inert for this gate.
    if !candidate_was_gated
        && test_gate.reuse_counts.is_some_and(|(_, rerun)| rerun == 0)
        && let Some(last) = stale.last_mut()
        && last.reuse == StaleReuse::Partial
    {
        last.reuse = StaleReuse::Proven;
    }
    // When every advance since the last full gate was, the published tree's
    // gate results are that gate's: its canary would re-run exactly the
    // commands the advances provably cannot change. Provisional migrations
    // keep their canary.
    let canary_skipped = proven_since_full_gate(stale)
        .filter(|_| !candidate_was_gated && provisional.is_empty())
        .map(|advances| format!("stale_delta_proven advances={advances}"));
    test_gate.reuse = None;

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
        && canary_skipped.is_none()
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
        && canary_skipped.is_none()
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
        outcome: LandingOutcome::Published,
        candidate: candidate.oid.clone(),
        kind: candidate.kind,
        fetched_tip: fetched_tip.to_string(),
        published_tip,
        provisional: provisional.to_vec(),
        base_reds: test_gate.base_reds.clone(),
        known_red_lines: known_red_lines(
            &test_gate.base_reds,
            &test_gate.base_red_texts,
            test_gate.known_failures.as_ref(),
        ),
        base_red_commands: test_gate.base_red_commands.clone(),
        flakes: test_gate.flakes.clone(),
        unverified: test_gate.unverified.clone(),
        base_reused: test_gate.base_reused.clone(),
        local_base_confirmed: test_gate.local_base_confirmed.clone(),
        base_absent_packages: test_gate.base_absent_packages.clone(),
        base_absent_targets: test_gate.base_absent_targets.clone(),
        base_static_inventory_reds: test_gate.base_static_inventory_reds.clone(),
        base_test_baseline_unavailable: test_gate.base_test_baseline_unavailable.clone(),
        canary_reused_tree,
        canary_queued,
        canary_skipped,
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

// No push or gate ran: keep the containing tip as the publication evidence.
fn already_on_rolling_report(tip: &str, gate: &TestGate, stale: &[StaleRetry]) -> LandReport {
    LandReport {
        outcome: LandingOutcome::AlreadyOnRolling,
        candidate: tip.into(),
        kind: CandidateKind::FastForward,
        fetched_tip: tip.into(),
        published_tip: tip.into(),
        provisional: Vec::new(),
        base_reds: BTreeSet::new(),
        known_red_lines: Vec::new(),
        base_red_commands: BTreeMap::new(),
        flakes: BTreeSet::new(),
        unverified: BTreeSet::new(),
        base_reused: BTreeSet::new(),
        local_base_confirmed: BTreeSet::new(),
        base_absent_packages: BTreeSet::new(),
        base_absent_targets: BTreeSet::new(),
        base_static_inventory_reds: BTreeSet::new(),
        base_test_baseline_unavailable: BTreeSet::new(),
        canary_reused_tree: None,
        canary_queued: None,
        canary_skipped: Some("already_on_rolling".into()),
        gate_scratch: gate
            .scratch
            .as_ref()
            .map(|scratch| scratch.report())
            .unwrap_or_default(),
        gate_disk_scratch: BTreeSet::new(),
        stale_retries: stale.to_vec(),
    }
}

fn same_tree(repo: &Path, left: &str, right: &str) -> Result<bool, String> {
    Ok(git_text(repo, &["rev-parse", &format!("{left}^{{tree}}")])?
        == git_text(repo, &["rev-parse", &format!("{right}^{{tree}}")])?)
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
    // Keep the fixed entries aligned with `released_migrations::tracked_source_paths`.
    // Manifest sections supply its remaining paths.
    let mut surface = BTreeSet::from([
        b"crates/rsid-store/src/store/mod.rs".to_vec(),
        b"crates/rsid-store/src/store/cohort_settlement.rs".to_vec(),
        b"crates/rsid-store/src/store/tests.rs".to_vec(),
        // The layout before the #1021 S4 crate split: a stale source touches it.
        b"crates/rsid/src/store/mod.rs".to_vec(),
        b"crates/rsid/src/store/cohort_settlement.rs".to_vec(),
        b"crates/rsid/src/store/tests.rs".to_vec(),
        b"tools/released-migrations.json".to_vec(),
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

/// #1949: the released-migrations pin is derived and committed on the
/// candidate where the lander publishes, so a worker never edits the shared
/// pin file. An already integrated source has nothing to pin; a derivation
/// error discards the candidate worktree and is the caller's refusal.
async fn with_derived_pin(
    config: &IntegrationConfig,
    repo: &Path,
    tip: &str,
    prepared: Prepared,
) -> Result<Prepared, String> {
    let Prepared::Candidate(mut next) = prepared else {
        return Ok(prepared);
    };
    match landing_policy::derive_released_pin(repo, &next.handle.worktree, tip, &next.oid) {
        Ok(derived) => {
            if let Some(oid) = derived {
                next.oid = oid;
            }
            Ok(Prepared::Candidate(next))
        }
        Err(error) => Err(match discard_candidate(config, repo, &next.handle).await {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; candidate cleanup failed: {cleanup}"),
        }),
    }
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
            // A source that landed as is at rolling head + 1 (#1655) has no
            // recorded proof, but a stale advance can move the head under it:
            // re-inspect every declared source so it is renumbered when needed.
            let declared = original.is_some() || has_provisional_declaration(repo, pair)?;
            let built = if declared {
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
                if provisional_plain_merge(&unit, original.is_some()) {
                    None
                } else {
                    let version = provisional_version(&unit, "new_version")?;
                    if let Some(original) = original
                        && version != original.assigned_version
                    {
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
                    std::fs::write(&unit_path, unit.to_string()).map_err(|error| {
                        format!("cannot record stale provisional unit: {error}")
                    })?;
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
                    Some((
                        unit_path,
                        provisional_oid(&built, "candidate")?.to_owned(),
                        version,
                    ))
                }
            } else {
                None
            };
            let source = built
                .as_ref()
                .map_or(pair.source.as_str(), |(_, oid, _)| oid.as_str());
            let prepared = prepare_candidate(config, repo, TARGET, &tip, source, scratch)
                .await
                .map_err(|error| format!("stale target could not merge cleanly: {error}"))?;
            // A renumbered unit carries its own pin (the provisional tool);
            // every other source gets its pin derived (#1949).
            let prepared = if built.is_none() {
                with_derived_pin(config, repo, &tip, prepared).await?
            } else {
                prepared
            };
            let next = match prepared {
                Prepared::Candidate(mut next) => {
                    if built.is_some() {
                        next.kind = CandidateKind::Merge;
                    }
                    next
                }
                Prepared::AlreadyIntegrated => {
                    return Err("accepted source became integrated during stale retry".into());
                }
            };
            let checked = (|| -> Result<Option<ProvisionalLanding>, String> {
                let landing = if let Some((unit_path, _, assigned_version)) = built {
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
                        assigned_version,
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

/// What an advance of rolling leaves valid of the candidate's earlier gate.
struct DeltaPlan {
    kind: StaleReuse,
    reuse: GateReuse,
}

/// #1940: classify the advance `fetched_tip..current` against the candidate's
/// gate. `Err` carries why it could not be classified; the whole gate then
/// runs again.
async fn delta_reuse(
    candidate: &Candidate,
    fetched_tip: &str,
    current: &str,
    candidate_paths: &BTreeSet<Vec<u8>>,
    incoming_paths: &BTreeSet<Vec<u8>>,
) -> Result<DeltaPlan, String> {
    let lossy = |paths: &BTreeSet<Vec<u8>>| -> BTreeSet<String> {
        paths
            .iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect()
    };
    let (candidate_paths, incoming_paths) = (lossy(candidate_paths), lossy(incoming_paths));
    let worktree = candidate.handle.worktree.as_path();
    let mut verdict = if incoming_paths
        .iter()
        .any(|path| candidate_paths.contains(path))
    {
        stale_delta::Verdict::everything("files_overlap")
    } else {
        let metadata =
            workspace_metadata(worktree).map_err(|_| "metadata_unavailable".to_owned())?;
        let members: BTreeSet<&str> = metadata
            .workspace_members
            .iter()
            .map(String::as_str)
            .collect();
        let packages: BTreeSet<String> = metadata
            .packages
            .iter()
            .filter(|package| members.contains(package.id.as_str()))
            .map(|package| package.name.clone())
            .collect();
        let selected = delta_selected_filters(worktree, fetched_tip, current).await;
        let dependents = |changed: &[String]| reverse_workspace_dependents(&metadata, changed).ok();
        stale_delta::classify(&stale_delta::Advance {
            paths: &incoming_paths,
            candidate_paths: &candidate_paths,
            packages: &packages,
            dependents: &dependents,
            selected_filters: selected.as_deref(),
        })
    };
    // The Python suites the advance selects. An unreadable plan runs them all.
    let python = if verdict.python_everything {
        Vec::new()
    } else {
        python_test_commands(worktree, fetched_tip, current, &BTreeSet::new()).unwrap_or_else(
            |_| {
                verdict.python_everything = true;
                Vec::new()
            },
        )
    };
    let python_selected = verdict.python_everything || !python.is_empty();
    let kind = if verdict.reuses_every_test() && !python_selected {
        StaleReuse::Proven
    } else if verdict.tests == stale_delta::Scope::Everything && verdict.python_everything {
        StaleReuse::Regated
    } else {
        StaleReuse::Partial
    };
    Ok(DeltaPlan {
        kind,
        reuse: GateReuse {
            verdict,
            python,
            family: None,
        },
    })
}

/// The `PACKAGE=FILTER` landing filters `scripts/check-touched-shards` suggests
/// for the advance `base..head`, the tests it selects (#1940). `None` when the
/// selector is missing, fails or prints something unreadable: the caller then
/// cannot call any test independent of the advance.
async fn delta_selected_filters(worktree: &Path, base: &str, head: &str) -> Option<Vec<String>> {
    let script = worktree.join("scripts/check-touched-shards");
    if !script.is_file() {
        return None;
    }
    let report = run_guard_with_env(
        worktree,
        &GuardSpec {
            commands: vec![GuardCommand {
                program: "python3".into(),
                args: vec![
                    // `-B`: never write bytecode into the candidate worktree.
                    "-B".into(),
                    script.to_string_lossy().into_owned(),
                    "--filters-only".into(),
                    "--json".into(),
                    "--repo".into(),
                    worktree.to_string_lossy().into_owned(),
                    "--base".into(),
                    base.into(),
                    "--head".into(),
                    head.into(),
                ],
                timeout: Duration::from_secs(180),
            }],
            env: BTreeMap::new(),
            output_tail_bytes: 1024 * 1024,
        },
        lander_env::current().as_map(),
    )
    .await;
    parse_selector_filters(&report.commands.first()?.stdout_tail)
}

/// The `suggested_filters` of a `check-touched-shards --json` receipt.
fn parse_selector_filters(stdout: &str) -> Option<Vec<String>> {
    let receipt: Value = serde_json::from_str(stdout).ok()?;
    receipt
        .get("suggested_filters")?
        .as_array()?
        .iter()
        .map(|filter| filter.as_str().map(str::to_owned))
        .collect()
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
    // A declared source that landed as is at head + 1 (#1655) has no recorded
    // proof, yet an advance of the head can still force its renumbering.
    let mut provisional_in_play = !provisional.is_empty();
    for pair in &options.accepted {
        if !provisional_in_play && has_provisional_declaration(repo, pair).map_err(fail)? {
            provisional_in_play = true;
        }
    }
    let touches_provisional = provisional_in_play
        && touches_provisional_gate(repo, options, fetched_tip, &current, &incoming_paths)
            .map_err(fail)?;
    let (reuse, reason, gate_reuse) = if exact_gate_required() {
        if touches_provisional {
            (StaleReuse::Regated, "provisional_gate".to_owned(), None)
        } else {
            match delta_reuse(
                candidate,
                fetched_tip,
                &current,
                &candidate_paths,
                &incoming_paths,
            )
            .await
            {
                Ok(plan) => (
                    plan.kind,
                    plan.reuse.verdict.reason.clone(),
                    Some(plan.reuse),
                ),
                Err(reason) => (StaleReuse::Regated, reason, None),
            }
        }
    } else if candidate_paths.is_disjoint(&incoming_paths) && !touches_provisional {
        (
            StaleReuse::Whole,
            "files_disjoint".to_owned(),
            Some(GateReuse::every_test("files_disjoint")),
        )
    } else {
        let reason = if touches_provisional {
            "provisional_gate"
        } else {
            "files_overlap"
        };
        (StaleReuse::Regated, reason.to_owned(), None)
    };
    let mut gate_reuse = match reuse {
        StaleReuse::Regated => None,
        _ => gate_reuse,
    };
    // A re-gate that runs tests costs time: bound it by the wall-time budget
    // the queue gave this run (#1940), else by the count of re-gated retries.
    if matches!(reuse, StaleReuse::Partial | StaleReuse::Regated) {
        let exhausted = |detail: String| {
            let mut failure = fail(format!("stale retries exhausted: {detail}"));
            failure.observed_tip = Some(current.clone());
            failure
        };
        if let Some(budget) = regate_budget() {
            let spent = test_gate.started.map_or(Duration::ZERO, |at| at.elapsed());
            let needed = if reuse == StaleReuse::Regated {
                Duration::from_secs(test_gate.full_gate_secs)
            } else {
                Duration::ZERO
            };
            if spent + needed > budget {
                return Err(exhausted(format!(
                    "rolling moved to {current} ({reason}) with {}s of the {}s re-gate wall-time \
                     budget spent and about {}s needed to run the gate again",
                    spent.as_secs(),
                    budget.as_secs(),
                    needed.as_secs()
                )));
            }
        } else {
            let regated = stale.iter().filter(|retry| !retry.reused_gate()).count();
            if regated >= max_regated_stale_retries() {
                return Err(exhausted(format!(
                    "rolling moved to {current} ({reason}) after {regated} re-gated retries ({} total)",
                    stale.len()
                )));
            }
        }
    }
    stale.push(StaleRetry {
        fetched: fetched_tip.to_string(),
        observed: current.clone(),
        reuse,
        reason,
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
    let (remade, remade_provisional) = if !provisional_in_play {
        // The earlier candidate's derived pin is dropped: the advance may
        // carry a sibling's pin, and the pin is derived again below (#1949).
        let source = landing_policy::pin_commit_parent(repo, &candidate.oid)
            .map_err(fail)?
            .unwrap_or_else(|| candidate.oid.clone());
        let prepared = prepare_candidate(&config, repo, TARGET, &current, &source, &scratch)
            .await
            .map_err(|error| fail(format!("stale target could not merge cleanly: {error}")))?;
        let remade = match with_derived_pin(&config, repo, &current, prepared)
            .await
            .map_err(fail)?
        {
            Prepared::Candidate(next) => next,
            Prepared::AlreadyIntegrated => {
                return Err(fail(
                    "candidate already integrated during stale retry".into(),
                ));
            }
        };
        (remade, Vec::new())
    } else {
        remake_stale_provisional(repo, options, &current, provisional, &config, &scratch)
            .await
            .map_err(fail)?
    };
    // #1957: a family re-run covers only what the advance can observe, read
    // from the gated tree and the remade one (both carry the candidate).
    if let Some(gate_reuse) = gate_reuse.as_mut()
        && gate_reuse.verdict.family_closed
        && gate_reuse.verdict.tests.selects_family("rsid")
    {
        match family_observers(
            &remade.handle.worktree,
            &candidate.oid,
            &remade.oid,
            &gate_reuse.verdict.family_filters,
        ) {
            Ok(observers) => gate_reuse.family = Some(observers),
            Err(reason) => println!("stale_family=whole reason={reason}"),
        }
    }
    let previous_reuse = std::mem::replace(&mut test_gate.reuse, gate_reuse);
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
    test_gate.reuse = previous_reuse;
    let cleanup = discard_candidate(&config, repo, &remade.handle).await;
    match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(report), Err(error)) => {
            let mut failure = LandFailure::candidate(
                PublicationState::Published,
                format!(
                    "stale candidate published but cleanup failed: {}",
                    cleanup_failure(&remade.handle.worktree, &error)
                ),
                &remade,
                &current,
            );
            failure.kind = FailureKind::Cleanup;
            failure.published_tip = Some(report.published_tip);
            Err(failure)
        }
        (Err(mut error), Err(cleanup)) => {
            let _ = write!(
                error.message,
                "; stale candidate cleanup failed: {}",
                cleanup_failure(&remade.handle.worktree, &cleanup)
            );
            Err(error)
        }
    }
}

/// #1940: `discard_candidate` refuses with one code, `NotEngineCandidate`, for
/// a worktree that is gone, not registered, marked for another candidate, or
/// not clean. Names which, so a cleanup refusal on the stale path explains
/// itself. The private clone holding the worktree is deleted with the
/// landing's workspace either way, so the refusal never leaks it.
fn cleanup_failure(worktree: &Path, error: &IntegrationError) -> String {
    let message = error.to_string();
    if !matches!(
        error,
        IntegrationError::Refused(Refusal::NotEngineCandidate)
    ) {
        return message;
    }
    if !worktree.exists() {
        return format!("{message} (the worktree {} is gone)", worktree.display());
    }
    match git_text(
        worktree,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    ) {
        Ok(status) if !status.is_empty() => {
            let entries = status.lines().take(5).collect::<Vec<_>>().join("; ");
            format!("{message} (the worktree is not clean: {entries})")
        }
        Ok(_) => format!(
            "{message} (the worktree is clean but is not a registered detached worktree \
             carrying this candidate's marker)"
        ),
        Err(error) => format!("{message} (the worktree cannot be inspected: {error})"),
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
    let output = lander_env::command("cargo")
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
    let plan =
        run_guard_pair_static(repo, pair, fetched_tip, candidate, allow_lost_hunks, proof).await?;
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
        let plan = run_guard_pair_static(repo, pair, fetched_tip, candidate, false, proof).await?;
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

/// Stdout tail the plan-only guard run keeps (#3785). The guard prints a
/// report that grows with its inputs; the plan JSON is parsed from this tail,
/// so a tail shorter than the report starts mid-JSON and fails the gate.
const GUARD_PLAN_TAIL_BYTES: usize = 8 * 1024 * 1024;

/// Arguments of the plan-only guard run. It never receives `--test-filter`
/// (#3785): the plan reads no test command, and the guard echoes every filter
/// into its report, so thousands of derived filters only bloat argv and stdout.
fn plan_only_guard_args(
    leading: Vec<String>,
    repo: &Path,
    pair: &AcceptedPair,
    fetched_tip: &str,
    candidate: &str,
    proof: Option<&Path>,
) -> Vec<String> {
    let mut args = leading;
    args.extend([
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
    ]);
    if let Some(proof) = proof {
        args.extend(["--proof".into(), proof.to_string_lossy().into_owned()]);
    }
    args
}

/// Parse the guard's plan report. A failure names the byte count of the
/// stdout it was given, so a truncated tail reads as one (#3785).
fn parse_guard_plan(stdout: &str) -> Result<GuardPlan, String> {
    serde_json::from_str(stdout).map_err(|error| {
        format!(
            "invalid landing guard plan: {error} (guard stdout tail is {} bytes; \
             a tail at the {GUARD_PLAN_TAIL_BYTES}-byte capture limit is truncated)",
            stdout.len()
        )
    })
}

/// The per-source checks that need no build: plan parse, lost hunks, guard
/// status and the released-migration check. Returns the guard plan.
async fn run_guard_pair_static(
    repo: &Path,
    pair: &AcceptedPair,
    fetched_tip: &str,
    candidate: &str,
    allow_lost_hunks: bool,
    proof: Option<&Path>,
) -> Result<GuardPlan, String> {
    // #361/#452: the trusted native guard runs the landing guard when one is
    // installed; without it the committed legacy script still does until that
    // script is deleted (#363).
    let (program, leading) = if let Some(exe) = landing_policy::trusted_guard_exe(repo, fetched_tip)
    {
        (
            exe.to_string_lossy().into_owned(),
            vec!["rolling-landing-guard".to_owned()],
        )
    } else if landing_policy::native_guard_installed() {
        return Err(format!(
            "the installed rsi-repo-guard is not authenticated for target {fetched_tip}"
        ));
    } else {
        let script = repo.join("scripts/rolling-landing-guard.py");
        if !script.is_file() {
            return Err(format!(
                "landing guard script is missing: {}",
                script.display()
            ));
        }
        (
            "python3".to_owned(),
            vec![script.to_string_lossy().into_owned()],
        )
    };
    let args = plan_only_guard_args(leading, repo, pair, fetched_tip, candidate, proof);
    let plan_report = run_guard_with_env(
        repo,
        &GuardSpec {
            commands: vec![GuardCommand {
                program,
                args,
                timeout: Duration::from_secs(120),
            }],
            env: BTreeMap::new(),
            output_tail_bytes: GUARD_PLAN_TAIL_BYTES,
        },
        lander_env::current().as_map(),
    )
    .await;
    let Some(plan_command) = plan_report.commands.first() else {
        return Err("rolling landing guard did not run".into());
    };
    // Older guards refuse an empty accepted delta without a JSON report.
    // Prove the no-op independently instead of parsing empty stdout or
    // trusting the guard's error text as a success signal.
    let plan: GuardPlan = if plan_command.stdout_tail.trim().is_empty()
        && same_tree(repo, &pair.base, &pair.source)?
        && git_is_ancestor(repo, &pair.base, &pair.source)?
        && git_is_ancestor(repo, &pair.source, candidate)?
        && git_is_ancestor(repo, fetched_tip, candidate)?
    {
        return Ok(GuardPlan {
            accepted_source: pair.source.clone(),
            candidate: candidate.into(),
            lost_hunks: Vec::new(),
            affected_crates: Vec::new(),
        });
    } else if plan_command.stdout_tail.trim().is_empty() {
        return Err(format!(
            "{}; landing guard returned no plan for accepted pair {}:{} ({:?})",
            plan_command.stderr_tail.trim(),
            pair.base,
            pair.source,
            plan_command.status
        ));
    } else {
        parse_guard_plan(&plan_command.stdout_tail).map_err(|error| {
            format!(
                "{}; {error}; accepted pair {}:{} ({:?})",
                plan_command.stderr_tail.trim(),
                pair.base,
                pair.source,
                plan_command.status
            )
        })?
    };
    if plan.accepted_source != pair.source || plan.candidate != candidate {
        return Err("landing guard plan does not match the accepted source and candidate".into());
    }
    // Every cheap static guard refuses before any build or test runs (#1052).
    if !plan.lost_hunks.is_empty() && !allow_lost_hunks {
        return Err(format!(
            "{}; rolling landing guard rejected accepted pair {}:{} before any test ran{}",
            plan.lost_hunks.join("; "),
            pair.base,
            pair.source,
            guard_refusal_evidence(repo, pair, fetched_tip, candidate)
        ));
    }
    if !plan_report.passed && (!allow_lost_hunks || plan.lost_hunks.is_empty()) {
        return Err(format!(
            "{}; rolling landing guard rejected accepted pair {}:{} ({:?})",
            plan_command.stderr_tail.trim(),
            pair.base,
            pair.source,
            plan_command.status
        ));
    }
    if proof.is_some() {
        landing_policy::check_released_migrations(repo, repo, fetched_tip, candidate)?;
    }
    Ok(plan)
}

/// The inputs a refused guard pair needs to be replayed by hand (#3452): the
/// lander's private clone and its candidate are discarded, so the refusal
/// carries the guard's own arguments, every merge base of the source against
/// the tip, and the command that rebuilds the candidate from the tip and the
/// source. Best effort: a git failure only drops that part.
fn guard_refusal_evidence(
    repo: &Path,
    pair: &AcceptedPair,
    fetched_tip: &str,
    candidate: &str,
) -> String {
    let merge_bases = git_text(repo, &["merge-base", "--all", &pair.source, fetched_tip])
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(","))
        .unwrap_or_default();
    format!(
        " (guard argv: --base {base} --source {source} --target {fetched_tip} \
         --candidate {candidate} --plan-only; merge bases of source and target: \
         [{merge_bases}]; rebuild the candidate tree with \
         `git merge-tree --write-tree {fetched_tip} {source}`)",
        base = pair.base,
        source = pair.source,
    )
}

/// The queue-derived filters (#3615): read from the file the queue names, else
/// the inline variable older queues set. A bad file is a typed refusal, never
/// an empty set (which would read as caller-named filters).
fn derived_gate_filters() -> Result<BTreeSet<String>, String> {
    rsi_common::rolling_queue::read_derived_filters(
        std::env::var_os(rsi_common::rolling_queue::DERIVED_FILTERS_FILE_ENV).as_deref(),
        std::env::var(rsi_common::rolling_queue::DERIVED_FILTERS_INLINE_ENV)
            .ok()
            .as_deref(),
    )
}

/// A digest of the derived set, small enough for a child's environment.
fn derived_filters_digest(derived: &BTreeSet<String>) -> String {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    for filter in derived {
        hash.update(filter.as_bytes());
        hash.update([0]);
    }
    format!("sha256:{}", hex::encode(hash.finalize()))
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
    // #1243: a filter that provably selects no test would fail the gate only
    // after the build; refuse it first (the queue checks at enqueue, a manual
    // landing only here).
    let derived = derived_gate_filters()?;
    let strict: Vec<String> = options
        .test_filters
        .iter()
        .filter(|filter| !derived.contains(*filter))
        .cloned()
        .collect();
    if let Some(filter) =
        rsid::rolling_queue::filter_selecting_no_tests(worktree, candidate, &strict)
    {
        let hint = rsid::rolling_queue::no_test_filter_hint(
            worktree,
            candidate,
            std::slice::from_ref(&filter),
        );
        return Err(format!(
            "test filter selects no test at the candidate, refused before any build: {filter}{hint}"
        ));
    }
    // #2086: the gate's test threads follow the host's idle capacity now.
    let (test_threads, inputs) = set_gate_test_threads();
    println!(
        "gate_test_threads={test_threads} host_threads={} load1={} {}",
        inputs
            .host_threads
            .map_or_else(|| "unknown".to_owned(), |threads| threads.to_string()),
        inputs
            .load1
            .map_or_else(|| "unknown".to_owned(), |load| format!("{load:.2}")),
        inputs.evidence(),
    );
    // #2270: the released-migration tests run only when the diff needs them.
    let (slow_tier, slow_tier_reason) = slow_tier_for_gate(worktree, fetched_tip, candidate);
    store_gate_slow_tier(slow_tier);
    println!("slow_tier={slow_tier:?} reason={slow_tier_reason}");
    // Scripts-only and docs-only gates need no Cargo metadata or build.
    let metadata = if affected_crates.is_empty() && options.test_filters.is_empty() && !provisional
    {
        WorkspaceMetadata {
            workspace_members: Vec::new(),
            packages: Vec::new(),
        }
    } else {
        workspace_metadata(worktree)?
    };
    // #3887: record what this source touched for the zero-test warning.
    let changed_paths =
        git_text(worktree, &["diff", "--name-only", fetched_tip, candidate]).unwrap_or_default();
    test_gate.zero_test_scope =
        zero_tests::ZeroTestScope::from_diff(affected_crates, changed_paths.lines());
    // #2410: the daemon-side observers of a changed leaf crate live in rsid.
    let observed_crates = with_leaf_observers(affected_crates, &options.test_filters);
    let affected_crates = observed_crates.as_slice();
    // #1280: an unfiltered rsid/rsid-store change gates every library test
    // that can observe it (or all of them); an unreadable diff or a crate-wide
    // change keeps every shard.
    let rsid_unfiltered = test_impact::select(worktree, fetched_tip, candidate);
    // #1937, #1650: an unfiltered package outside the rsid family also runs
    // the bin and integration test targets its diff changes.
    let filtered_packages: BTreeSet<&str> = options
        .test_filters
        .iter()
        .filter_map(|filter| filter.split_once('=').map(|(package, _)| package))
        .collect();
    let other_packages: Vec<String> = affected_crates
        .iter()
        .filter(|package| {
            !is_rsid_shard_package(package) && !filtered_packages.contains(package.as_str())
        })
        .cloned()
        .collect();
    let other_targets =
        test_impact::other_package_targets(worktree, fetched_tip, candidate, &other_packages)
            .map_err(|error| format!("cannot select the targets of changed packages: {error}"))?;
    for (package, info) in &other_targets {
        println!(
            "unfiltered_targets package={package} lib={} check={} targets={}",
            info.has_lib,
            info.check,
            info.targets.iter().cloned().collect::<Vec<_>>().join(",")
        );
    }
    let evidence = with_slow_tier_evidence(
        gate_selection_evidence(
            rsid_unfiltered.as_ref(),
            filtered_packages
                .iter()
                .any(|package| is_rsid_shard_package(package)),
        ),
        slow_tier,
        affected_crates,
    );
    println!("rsid_unfiltered_{evidence}");
    // A later stale re-gate must not erase a bounded landing's QA obligation.
    if obliges_full_qa(&evidence) || !obliges_full_qa(&test_gate.selection_evidence) {
        test_gate.selection_evidence = evidence;
    }
    let mut spec = affected_crate_guard_spec(
        fetched_tip,
        candidate,
        affected_crates,
        &options.test_filters,
        provisional,
        &metadata,
        options.cargo_build_jobs,
        rsid_unfiltered.as_ref(),
        &other_targets,
    )?;
    if slow_tier == SlowTier::Full {
        // The shard script (a whole shard of a crate-wide change) reads it.
        spec.env
            .insert("RSI_LANDER_SLOW_TIER".into(), "full".into());
    }
    if !release_checks_enabled() {
        spec.commands
            .retain(|command| release_check_package(command).is_none());
    }
    // #2512: a diff proven to change no production build owes no compile check
    // its selected test compilation already makes. Not on a stale re-gate
    // that reuses an earlier gate (#2546): `reuse_spec` then drops the focused
    // runs after this proof is checked, so the checks are the only compile
    // left and must stay.
    if !affected_crates.is_empty()
        && test_only_omission_allowed(test_gate.reuse.as_ref())
        && let Some(proof) = test_impact::test_only_diff(worktree, fetched_tip, candidate)
    {
        let omitted = omit_proven_test_only_checks(&mut spec.commands, &proof);
        println!(
            "test_only_diff packages={} omitted_checks={omitted}",
            proof.packages.iter().cloned().collect::<Vec<_>>().join(",")
        );
    }
    if collapse_whole_shards_to_one_build(&mut spec.commands, &options.disk_scratch_shards) {
        println!(
            "rsid_build=unsharded reason=crate-wide shards={}",
            RSID_SHARDS.len()
        );
    }
    spec.commands.extend(python_test_commands(
        worktree,
        fetched_tip,
        candidate,
        &test_gate.claimed_fixes,
    )?);
    test_gate.focused_filters = explicit_focused_filters(&options.test_filters, provisional);
    for filter in &mut test_gate.focused_filters {
        if derived.contains(&filter.label) {
            filter.explicit = false;
        }
    }
    // Only a digest rides in the guard commands' environment: the full set can
    // pass the 128 KiB one-string cap and no child reads it (#3615). It still
    // keys the receipt identity.
    spec.env.insert(
        "RSI_LANDER_DERIVED_FILTERS_SHA256".into(),
        derived_filters_digest(&derived),
    );
    run_affected_gate_receipted(
        repo,
        worktree,
        fetched_tip,
        candidate,
        &spec,
        test_gate,
        options,
    )
    .await
}

/// What a candidate receipt is keyed by, or `None` when none may stand in for
/// this gate: receipts are off, this is not the candidate's first full gate, a
/// remote executor or an earlier partial gate is in play, or the identity could
/// not be read. A `None` always means "run the gate".
struct ReceiptSlot {
    root: PathBuf,
    tree: String,
    key: String,
}

/// `sha256` hex of the running lander binary: the gate logic is part of what a
/// verdict depends on, so another build never reads this build's receipts.
fn lander_identity() -> Result<String, String> {
    use sha2::Digest as _;
    use std::io::Read as _;

    static IDENTITY: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            let path = std::env::current_exe()
                .map_err(|error| format!("cannot locate the lander binary: {error}"))?;
            let mut file = std::fs::File::open(&path)
                .map_err(|error| format!("cannot read the lander binary: {error}"))?;
            let mut hasher = sha2::Sha256::new();
            let mut buffer = vec![0_u8; 1 << 20];
            loop {
                let read = file
                    .read(&mut buffer)
                    .map_err(|error| format!("cannot read the lander binary: {error}"))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            Ok(hex::encode(hasher.finalize()))
        })
        .clone()
}

fn tool_version(worktree: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = lander_env::command(program)
        .args(args)
        .current_dir(worktree)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{program} {args:?} failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The text a receipt key digests: the candidate's tree, the base tip, the
/// resolved gate (every command with its limits, the environment without the
/// per-run scratch path) and the toolchain, scratch class, claims and lander
/// build the verdict was measured under.
#[allow(clippy::too_many_arguments)]
fn receipt_identity(
    tree: &str,
    base: &str,
    candidate: &str,
    inherited_env: &BTreeMap<String, String>,
    tool_inputs: &str,
    spec: &GuardSpec,
    options: &Options,
    scratch_class: &str,
    slow_tier: SlowTier,
    claimed: &BTreeSet<String>,
    toolchain: &str,
    lander: &str,
) -> String {
    use std::fmt::Write as _;

    let mut identity = String::new();
    let _ = writeln!(identity, "candidate-receipt-v1");
    let _ = writeln!(identity, "tree={tree}");
    let _ = writeln!(identity, "base={base}");
    let _ = writeln!(identity, "lander={lander}");
    let _ = writeln!(identity, "toolchain={toolchain}");
    let _ = writeln!(identity, "scratch={scratch_class}");
    let _ = writeln!(identity, "slow_tier={slow_tier:?}");
    let _ = writeln!(identity, "build_jobs={}", options.cargo_build_jobs);
    let _ = writeln!(identity, "filters={:?}", options.test_filters);
    // An unreadable set already refused the gate before any identity is taken.
    let _ = writeln!(
        identity,
        "derived_filters={:?}",
        derived_gate_filters().unwrap_or_default()
    );
    let _ = writeln!(identity, "disk_scratch={:?}", options.disk_scratch_shards);
    let _ = writeln!(identity, "claimed={claimed:?}");
    for (key, value) in &spec.env {
        if key != "TMPDIR" {
            let _ = writeln!(identity, "env {key}={value}");
        }
    }
    // The inherited compiler/target/test settings the guard commands run
    // under but `spec.env` does not carry (#2729).
    for (key, value) in inherited_env {
        let _ = writeln!(identity, "inherited_env {key}={value}");
    }
    // The resolved executables and Cargo configuration the commands run with
    // (#2738).
    let _ = writeln!(identity, "tool_inputs {tool_inputs}");
    let _ = writeln!(identity, "tail={}", spec.output_tail_bytes);
    for command in &spec.commands {
        // The adaptive `-j` is how fast a run goes, never what it runs.
        let _ = writeln!(
            identity,
            "command {}",
            receipt_command_key(command, candidate)
        );
    }
    identity
}

/// A command's identity inside a receipt key. The candidate commit is not a
/// dependency of the verdict: a retry after a declined publication re-makes
/// the merge with a new commit time, so a different SHA names the same tree
/// (#2728). The static diff check names the candidate by SHA, so that
/// argument is replaced by a fixed marker; the tree (and the base, which the
/// other argument still carries) are digested separately.
fn receipt_command_key(command: &GuardCommand, candidate: &str) -> String {
    let mut normalized = command.clone();
    if !candidate.is_empty() {
        for arg in &mut normalized.args {
            if arg == candidate {
                *arg = "<candidate>".into();
            }
        }
    }
    base_cache_command_key(&normalized)
}

/// Whether an inherited variable is part of what a guard command compiles or
/// tests under (#2729). A fixed list of the compiler, target, profile and test
/// runner settings: build locations (`CARGO_TARGET_DIR`; `CARGO_HOME` and
/// `PATH` enter the key through their resolved contents, `receipt_tool_inputs`),
/// the thread/job throttles (how fast a run goes, never what it runs) and
/// anything that looks like a credential are never part of the key.
fn receipt_env_relevant(key: &str) -> bool {
    const EXACT: [&str; 14] = [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTDOCFLAGS",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTC_BOOTSTRAP",
        "RUSTDOC",
        "RUSTUP_TOOLCHAIN",
        "CARGO_INCREMENTAL",
        "RUST_MIN_STACK",
        "RUST_TEST_NOCAPTURE",
        "RUST_TEST_SHUFFLE",
    ];
    const PREFIXES: [&str; 4] = [
        "CARGO_BUILD_",
        "CARGO_TARGET_",
        "CARGO_PROFILE_",
        "NEXTEST_",
    ];
    const NOT_IDENTITY: [&str; 6] = [
        "CARGO_BUILD_JOBS",
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_TARGET_DIR",
        "NEXTEST_TEST_THREADS",
        "NEXTEST_BUILD_JOBS",
        "NEXTEST_VERSION",
    ];
    const SECRET_WORDS: [&str; 6] = ["TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH", "KEY"];
    if NOT_IDENTITY.contains(&key) || SECRET_WORDS.iter().any(|word| key.contains(word)) {
        return false;
    }
    EXACT.contains(&key) || PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// The receipt-relevant part of an environment, as the guard commands see it
/// (a variable the guard withholds does not reach them). Values compare
/// verbatim; a non-UTF-8 value is digested through its lossy form.
fn receipt_inherited_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> BTreeMap<String, String> {
    vars.into_iter()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            (receipt_env_relevant(&key) && !rsid::integration::guard_withholds_env(&key))
                .then(|| (key, value.to_string_lossy().into_owned()))
        })
        .collect()
}

/// `sha256` hex of a file's bytes, or `None` when it cannot be read.
fn file_sha256(path: &Path) -> Option<String> {
    use sha2::Digest as _;
    use std::io::Read as _;

    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hex::encode(hasher.finalize()))
}

/// The suffixes a `PATHEXT` value lists, each with its leading dot; the
/// defaults `.exe`, `.cmd`, `.bat` and `.com` when it is unset or empty.
fn parse_pathext(pathext: Option<&std::ffi::OsStr>) -> Vec<String> {
    let listed: Vec<String> = pathext
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|suffix| !suffix.is_empty())
        .map(|suffix| {
            if suffix.starts_with('.') {
                suffix.to_owned()
            } else {
                format!(".{suffix}")
            }
        })
        .collect();
    if listed.is_empty() {
        [".exe", ".cmd", ".bat", ".com"].map(str::to_owned).to_vec()
    } else {
        listed
    }
}

/// The executable suffixes a bare program name may resolve through (#2739):
/// Windows resolves `cargo` through `PATHEXT` (`cargo.exe`, ...); other
/// platforms add none.
fn executable_suffixes(pathext: Option<&std::ffi::OsStr>) -> Vec<String> {
    if cfg!(windows) {
        parse_pathext(pathext)
    } else {
        Vec::new()
    }
}

/// The file `program` names in one directory. The name is tried as is first
/// when it already carries an extension or the platform adds no suffix, then
/// with each executable `suffixes` entry in order, so the keyed file is the
/// one a spawn runs (#2739).
fn resolve_in_dir(program: &str, dir: &Path, suffixes: &[String]) -> Option<PathBuf> {
    let try_as_is = suffixes.is_empty() || Path::new(program).extension().is_some();
    try_as_is
        .then(|| dir.join(program))
        .into_iter()
        .chain(
            suffixes
                .iter()
                .map(|suffix| dir.join(format!("{program}{suffix}"))),
        )
        .find(|candidate| candidate.is_file())
}

/// The file a bare program name resolves to under `path`, the way
/// `Command::new` searches it (see `resolve_in_dir` for the suffixes). A name
/// with a separator is a tree path and is covered by the tree digest.
fn resolve_on_path(program: &str, path: &std::ffi::OsStr, suffixes: &[String]) -> Option<PathBuf> {
    if program.contains('/') || program.contains(std::path::MAIN_SEPARATOR) {
        return None;
    }
    std::env::split_paths(path).find_map(|dir| resolve_in_dir(program, &dir, suffixes))
}

/// The executable and Cargo-configuration inputs of a guard run that no
/// version string shows (#2738). `PATH` and `CARGO_HOME` are inherited by the
/// guard commands (`spec.env` overrides win): `Command::new` resolves each
/// program through `PATH`, and Cargo reads configuration from `CARGO_HOME`.
/// A retry with the same tree and base but a replaced executable (even one
/// that reports the same `-V`) or changed Cargo configuration must not reuse
/// a verdict measured under the old one, so the key names the effective
/// `PATH`, a content digest of every program the gate resolves through it
/// (the commands' programs, the toolchain probes and the sysroot `rustc` and
/// `cargo` a rustup proxy hands off to) and a digest of the effective Cargo
/// home's `config.toml`/`config`. The `CARGO_HOME` location itself stays out
/// of the key: only what Cargo reads there counts, and credentials are never
/// read.
fn receipt_tool_inputs(
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    spec: &GuardSpec,
    sysroot: Option<&Path>,
) -> String {
    use std::fmt::Write as _;

    let effective = |name: &str| {
        spec.env
            .get(name)
            .map(std::ffi::OsString::from)
            .or_else(|| var(name))
    };
    let mut out = String::new();
    let path = effective("PATH").unwrap_or_default();
    let suffixes = executable_suffixes(effective("PATHEXT").as_deref());
    let _ = write!(out, "path={};", path.to_string_lossy());
    let programs: BTreeSet<&str> = spec
        .commands
        .iter()
        .map(|command| command.program.as_str())
        .chain(["cargo", "rustc", "python3"])
        .collect();
    for program in programs {
        let digest = resolve_on_path(program, &path, &suffixes)
            .and_then(|resolved| file_sha256(&resolved))
            .unwrap_or_else(|| "unresolved".into());
        let _ = write!(out, "program {program}={digest};");
    }
    if let Some(sysroot) = sysroot {
        for tool in ["rustc", "cargo"] {
            let digest = resolve_in_dir(tool, &sysroot.join("bin"), &suffixes)
                .and_then(|resolved| file_sha256(&resolved))
                .unwrap_or_else(|| "unresolved".into());
            let _ = write!(out, "sysroot {tool}={digest};");
        }
    }
    let cargo_home = effective("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| effective("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    for name in ["config.toml", "config"] {
        let digest = cargo_home
            .as_ref()
            .and_then(|home| file_sha256(&home.join(name)))
            .unwrap_or_else(|| "absent".into());
        let _ = write!(out, "cargo_config {name}={digest};");
    }
    out
}

fn receipt_slot(
    worktree: &Path,
    base: &str,
    candidate: &str,
    spec: &GuardSpec,
    gate: &TestGate,
    options: &Options,
) -> Option<ReceiptSlot> {
    if !gate.candidate_receipts || gate.reuse.is_some() || gate.remote.is_some() {
        return None;
    }
    let root = gate.receipt_root.clone()?;
    let slot = (|| -> Result<ReceiptSlot, String> {
        let tree = git_text(worktree, &["rev-parse", "HEAD^{tree}"])?;
        let scratch_class = match gate.scratch {
            Some(GateScratch::Tmpfs(_)) => "tmpfs",
            _ => "disk",
        };
        let toolchain = format!(
            "{}{}{}",
            tool_version(worktree, "rustc", &["-vV"])?,
            tool_version(worktree, "cargo", &["-V"])?,
            tool_version(worktree, "python3", &["-V"]).unwrap_or_default(),
        );
        let sysroot = tool_version(worktree, "rustc", &["--print", "sysroot"])
            .ok()
            .map(|text| PathBuf::from(text.trim()));
        let tool_inputs =
            receipt_tool_inputs(&|name| lander_env::var_os(name), spec, sysroot.as_deref());
        let identity = receipt_identity(
            &tree,
            base,
            candidate,
            &receipt_inherited_env(lander_env::vars_os()),
            &tool_inputs,
            spec,
            options,
            scratch_class,
            gate_slow_tier(),
            &gate.claimed_fixes,
            &toolchain,
            &lander_identity()?,
        );
        Ok(ReceiptSlot {
            root,
            tree,
            key: candidate_receipt::key_of(&identity),
        })
    })();
    match slot {
        Ok(slot) => Some(slot),
        Err(error) => {
            println!("candidate_receipt=unavailable detail={error}");
            None
        }
    }
}

impl TestGate {
    /// The state of a green full gate the landing report reads afterwards.
    fn green_outcome(&self) -> candidate_receipt::GateOutcome {
        candidate_receipt::GateOutcome {
            base_reds: self.base_reds.clone(),
            base_red_texts: self.base_red_texts.clone(),
            base_red_commands: self.base_red_commands.clone(),
            flakes: self.flakes.clone(),
            base_reused: self.base_reused.clone(),
            local_base_confirmed: self.local_base_confirmed.clone(),
            base_absent_packages: self.base_absent_packages.clone(),
            base_absent_targets: self.base_absent_targets.clone(),
            base_static_inventory_reds: self.base_static_inventory_reds.clone(),
            disk_scratch_used: self.disk_scratch_used.clone(),
            claimed_green: self.claimed_green.clone(),
            full_gate_secs: self.full_gate_secs,
        }
    }

    /// Whether a green gate's evidence is complete enough to stand in for a
    /// later gate: a run that left something unverified, or a base comparison
    /// that could not be made, is never recorded.
    fn green_is_reusable(&self) -> bool {
        self.unverified.is_empty() && self.base_test_baseline_unavailable.is_empty()
    }

    fn adopt_green_outcome(&mut self, outcome: candidate_receipt::GateOutcome) {
        self.base_reds.extend(outcome.base_reds);
        self.base_red_texts.extend(outcome.base_red_texts);
        self.base_red_commands.extend(outcome.base_red_commands);
        self.flakes.extend(outcome.flakes);
        self.base_reused.extend(outcome.base_reused);
        self.local_base_confirmed
            .extend(outcome.local_base_confirmed);
        self.base_absent_packages
            .extend(outcome.base_absent_packages);
        self.base_absent_targets.extend(outcome.base_absent_targets);
        self.base_static_inventory_reds
            .extend(outcome.base_static_inventory_reds);
        self.disk_scratch_used.extend(outcome.disk_scratch_used);
        self.claimed_green.extend(outcome.claimed_green);
        self.full_gate_secs = outcome.full_gate_secs;
    }
}

/// #1312: the candidate's full gate, answered by a green receipt a failed
/// earlier publication left when one matches this exact tree, base, gate and
/// environment, and recorded when it passes in full. Everything before this
/// point (policy, static guards, selection, the spec) already ran live.
async fn run_affected_gate_receipted(
    repo: &Path,
    candidate_worktree: &Path,
    base: &str,
    candidate: &str,
    spec: &GuardSpec,
    gate: &mut TestGate,
    options: &Options,
) -> Result<(), String> {
    // #3923: independent of Cargo selection and reuse. A cached Cargo verdict
    // never substitutes for the shell lane's own typed receipt.
    let shell_started = Instant::now();
    let shell_result = run_shell_suite_gate(candidate_worktree, base, candidate, spec, gate).await;
    gate.checks_secs += shell_started.elapsed().as_secs();
    gate.record_check_step("shell-tests", shell_started.elapsed().as_secs());
    shell_result?;
    let slot = receipt_slot(candidate_worktree, base, candidate, spec, gate, options);
    if let Some(slot) = &slot
        && let Some((outcome, age_secs)) =
            candidate_receipt::load(&slot.root, &slot.tree, &slot.key)
    {
        gate.adopt_green_outcome(outcome);
        println!(
            "candidate_receipt=reused tree={} key={} age_secs={age_secs} candidate={candidate} reason=gate_already_passed",
            slot.tree, slot.key
        );
        println!(
            "gate_timing=shards=0 build_mode=receipt base_hits=0 base_runs=0 base_skipped=0 prebuild_secs=0 checks_secs=0 release_check_secs=0 candidate_secs=0 base_secs=0 wall_secs=0 test_threads={} thread_inputs={} check_steps=none candidate_receipt=reused {}",
            gate_test_threads(),
            gate_thread_inputs_evidence(),
            gate.selection_evidence,
        );
        return Ok(());
    }
    run_affected_gate(
        repo,
        candidate_worktree,
        base,
        spec,
        gate,
        &options.disk_scratch_shards,
    )
    .await?;
    if let Some(slot) = slot
        && gate.reuse.is_none()
        && gate.green_is_reusable()
    {
        match candidate_receipt::record(&slot.root, &slot.tree, &slot.key, &gate.green_outcome()) {
            Ok(()) => println!(
                "candidate_receipt=recorded tree={} key={}",
                slot.tree, slot.key
            ),
            // The receipt only saves time; a failed write never fails a gate.
            Err(error) => println!("candidate_receipt=unrecorded detail={error}"),
        }
    }
    Ok(())
}

/// Select and execute shell suites through the trusted installed guard. The
/// daemon package never links the guard crate, including for this lane.
async fn run_shell_suite_gate(
    worktree: &Path,
    base: &str,
    candidate: &str,
    spec: &GuardSpec,
    gate: &TestGate,
) -> Result<(), String> {
    #[cfg(not(test))]
    let exe = landing_policy::trusted_guard_exe(worktree, base)
        .ok_or_else(|| "no trusted rsi-repo-guard is installed for the shell gate".to_owned())?;
    // Older lander fixtures stub the static guards independently. This lane
    // crosses the real executable boundary unless a test supplies its own stub.
    #[cfg(test)]
    let exe = SHELL_GUARD_EXE
        .with(|path| path.borrow().clone())
        .unwrap_or_else(provisional_fixture::real_guard);

    let mut env = spec.env.clone();
    env.retain(|key, _| !rsid::integration::guard_withholds_env(key) && key != "TAP_SELECT");
    if let Some(scratch) = &gate.scratch {
        env.insert(
            "TMPDIR".into(),
            scratch.path().to_string_lossy().into_owned(),
        );
    }
    let mut inherited = lander_env::vars_os()
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    inherited.retain(|key, _| key != "TAP_SELECT");
    let root = gate
        .receipt_root
        .clone()
        .unwrap_or_else(|| worktree.parent().unwrap_or(worktree).join("gate-receipts"))
        .join("shell-suites");
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("cannot create shell receipt directory: {error}"))?;
    let logs = tempfile::Builder::new()
        .prefix("gate-")
        .tempdir_in(root)
        .map_err(|error| format!("cannot allocate shell receipt: {error}"))?
        .keep();
    let path = logs.join("receipt.json");
    let report = run_guard_with_env(
        worktree,
        &GuardSpec {
            commands: vec![GuardCommand {
                program: exe.to_string_lossy().into_owned(),
                args: vec![
                    "residual-lane".into(),
                    "--base".into(),
                    base.into(),
                    "--head".into(),
                    candidate.into(),
                    "--repo".into(),
                    worktree.to_string_lossy().into_owned(),
                    "--receipt".into(),
                    path.to_string_lossy().into_owned(),
                    "--timeout-secs".into(),
                    guard_timeout().as_secs().to_string(),
                ],
                // Leave time for the lane to reap its children and persist a red
                // timeout receipt before the outer supervisor enforces its ceiling.
                timeout: guard_timeout() + Duration::from_secs(10),
            }],
            env,
            output_tail_bytes: GUARD_PLAN_TAIL_BYTES,
        },
        &inherited,
    )
    .await;
    for command in &report.commands {
        print!("{}", command.stdout_tail);
        eprint!("{}", command.stderr_tail);
    }
    let receipt: Value = serde_json::from_slice(
        &std::fs::read(&path)
            .map_err(|error| format!("shell suite gate has no receipt: {error}; {report:?}"))?,
    )
    .map_err(|error| format!("invalid shell gate receipt: {error}"))?;
    println!("shell_gate_receipt={} {receipt}", path.display());
    validate_shell_receipt(&receipt, base, candidate)?;
    if !report.passed {
        return Err(format!("shell suite guard failed: {report:?}"));
    }
    Ok(())
}

/// Check the transport envelope as well as every verdict: an exited guard or
/// an affirmative top-level flag alone cannot turn a partial/red lane green.
fn validate_shell_receipt(receipt: &Value, base: &str, candidate: &str) -> Result<(), String> {
    let selected = receipt["selected"].as_array();
    let entries = receipt["residual"].as_array();
    let complete = selected.zip(entries).is_some_and(|(selected, entries)| {
        selected.len() == entries.len()
            && selected.iter().zip(entries).all(|(test, entry)| {
                test.as_str().is_some_and(|path| !path.is_empty())
                    && entry["test_file"] == *test
                    && entry["status"] == "passed"
                    && entry["completed"] == true
                    && entry["executed_tests"]
                        .as_u64()
                        .is_some_and(|count| count > 0)
                    && entry["exit_code"] == 0
                    && entry["failed_tests"] == 0
            })
    });
    if receipt["schema_version"] != 1
        || receipt["base"] != base
        || receipt["receipt_head"] != candidate
        || receipt["ok"] != true
        || !receipt.get("error").is_some_and(Value::is_null)
        || !complete
    {
        return Err(format!("shell suite gate failed: {receipt}"));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static SHELL_GUARD_EXE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// #2546: the test-only omission applies to a gate executed in full for this
/// candidate. A stale re-gate that reuses an earlier gate (`reuse` is set)
/// drops focused runs in `reuse_spec` after the omission's coverage check, so
/// it keeps its compile checks.
fn test_only_omission_allowed(reuse: Option<&GateReuse>) -> bool {
    reuse.is_none()
}

/// #2512: drop every compile check (`cargo check -p P --all-targets` and the
/// non-test `cargo check -p P`) of a gate whose whole diff `proof` shows
/// production-unchanged, provided the commands that remain compile each test
/// target the diff touches. An rsid-family library edit needs the focused lib
/// test build: a shard run builds only its own `test-shard-*` features, so it
/// does not compile the whole test tree. Returns the number of commands
/// dropped; zero leaves the spec as it was.
fn omit_proven_test_only_checks(
    commands: &mut Vec<GuardCommand>,
    proof: &test_impact::TestOnlyDiff,
) -> usize {
    let is_check = |command: &GuardCommand| {
        dev_check_package(command).is_some() || release_check_package(command).is_some()
    };
    let cargo_test = |command: &GuardCommand, package: &str| {
        command.program == "cargo"
            && command.args.first().map(String::as_str) == Some("test")
            && command.args.get(1).map(String::as_str) == Some("-p")
            && command.args.get(2).map(String::as_str) == Some(package)
    };
    let covered = proof.targets.iter().all(|target| match target {
        test_impact::TestTarget::Lib(package) => commands.iter().any(|command| {
            if is_rsid_shard_package(package) {
                rsid_lib_filterset(command).is_some()
            } else {
                cargo_test(command, package) && command.args.iter().any(|arg| arg == "--lib")
            }
        }),
        test_impact::TestTarget::Bin(package, name) => commands.iter().any(|command| {
            cargo_test(command, package)
                && command
                    .args
                    .windows(2)
                    .any(|pair| pair[0] == "--bin" && pair[1] == *name)
        }),
        test_impact::TestTarget::Test(package, name) => commands.iter().any(|command| {
            cargo_test(command, package)
                && command
                    .args
                    .windows(2)
                    .any(|pair| pair[0] == "--test" && pair[1] == *name)
        }),
    });
    if !covered || proof.targets.is_empty() {
        return 0;
    }
    let before = commands.len();
    commands.retain(|command| !is_check(command));
    before - commands.len()
}

/// #2410: a changed leaf crate (`rsid-remote-read`) is observed by daemon code
/// it cannot name, so an unfiltered gate also treats `rsid` as changed: the
/// closure over the leaf and rsid sources then selects the rsid tests, bin and
/// integration targets that name the changed items. Explicit filters override
/// the closure, as they do for the rest of the rsid family.
fn with_leaf_observers(affected: &[String], filters: &[String]) -> Vec<String> {
    let mut crates = affected.to_vec();
    let leaf_changed = affected
        .iter()
        .any(|package| test_impact::LEAF_PACKAGES.contains(&package.as_str()));
    if leaf_changed && filters.is_empty() && !crates.iter().any(|package| package == "rsid") {
        crates.push("rsid".to_owned());
        crates.sort();
    }
    crates
}

/// Whether the gate omitted reached library tests (#2092) or reached targets
/// (#2094), which the merge queue's async full run must then cover.
fn obliges_full_qa(evidence: &str) -> bool {
    evidence.starts_with("selection=bounded ")
        || evidence.contains(" targets=bounded ")
        || evidence.contains(SLOW_TIER_EXCLUDED_EVIDENCE)
}

/// Evidence suffix for a gate that skipped the released-migration history tier
/// over rsid/rsid-store library code (#2286); the full-library QA runs the tier.
const SLOW_TIER_EXCLUDED_EVIDENCE: &str = " slow_tier=excluded";

/// #2286: a gate that excluded the slow tier while the diff touched rsid or
/// rsid-store library code leaves the tier unrun whatever its selection was
/// (explicit filters and an unbounded closure included), so it owes the async
/// full-library run too.
fn with_slow_tier_evidence(
    mut evidence: String,
    slow_tier: SlowTier,
    affected_crates: &[String],
) -> String {
    let touched = affected_crates
        .iter()
        .any(|package| package == "rsid" || package == "rsid-store");
    if slow_tier == SlowTier::Excluded && touched && !evidence.contains(SLOW_TIER_EXCLUDED_EVIDENCE)
    {
        evidence.push_str(SLOW_TIER_EXCLUDED_EVIDENCE);
    }
    evidence
}

/// Explicit family filters override the computed closure. Do not record an
/// unused bounded selection as the gate policy or create a spurious QA job.
fn gate_selection_evidence(
    selection: Option<&test_impact::UnfilteredSelection>,
    explicit: bool,
) -> String {
    if explicit {
        "selection=closure reason=explicit_filters".into()
    } else {
        selection.map_or_else(
            || "selection=everything reason=crate-wide_or_unreadable_diff".to_owned(),
            test_impact::UnfilteredSelection::evidence,
        )
    }
}

/// The Python suites a candidate selects, in gate order. They are strict
/// candidate checks run before Cargo; the stale re-gate reuse treats them as one
/// list (`GateReuse::python`).
fn python_test_commands(
    worktree: &Path,
    base: &str,
    candidate: &str,
    claimed: &BTreeSet<String>,
) -> Result<Vec<GuardCommand>, String> {
    python_suite_commands(worktree, base, candidate, claimed)
}

/// Wall-clock bound for one Python test command of the gate (#3018). A passing
/// file is never refused for running long while it is within this bound; a hung
/// file is still stopped. scripts/tests/test_scoped_test.py alone spawns many
/// subprocesses and outgrew the former 120 s on a loaded hub, refusing landings
/// that a repair cannot fix. The per-command elapsed time is reported in
/// `gate_timing` (`python-test:<file>@secs`) so growth stays visible.
const PYTHON_TEST_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

/// Space-free `gate_timing` step name of one Python test command: its test
/// target (the last argument, a file path or module) after `python-test:`.
fn python_test_step_name(command: &GuardCommand) -> String {
    let target = command.args.last().map_or("", String::as_str);
    let target: String = target.chars().filter(|c| !c.is_whitespace()).collect();
    format!("python-test:{target}")
}

/// Share Python test selection with scoped-test (#1606). Pure docs/thoughts
/// changes never launch the selector; every selected Python suite is a strict,
/// bounded candidate check, so a failure refuses publication before Cargo.
fn python_suite_commands(
    worktree: &Path,
    base: &str,
    candidate: &str,
    claimed: &BTreeSet<String>,
) -> Result<Vec<GuardCommand>, String> {
    let mut commands = changed_python_suite_commands(worktree, base, candidate)?;
    // #3320: a claimed Python test is run whatever the diff selects, because no
    // queue filter can select it (test filters are PACKAGE=FILTER for Cargo).
    for claim in claimed {
        let Some((file, runner)) = claimed_python_suite(worktree, claim) else {
            continue;
        };
        if commands
            .iter()
            .any(|command| command.args.last().is_some_and(|last| *last == file))
        {
            continue;
        }
        commands.push(GuardCommand {
            program: "python3".into(),
            args: ["-B", "-m", runner, "-v"]
                .into_iter()
                .map(str::to_owned)
                .chain([file])
                .collect(),
            timeout: PYTHON_TEST_COMMAND_TIMEOUT,
        });
    }
    Ok(commands)
}

/// The test module a claimed fix (#2167) names, as `(repo-relative file,
/// runner)`, when it is a Python unittest id
/// `scripts.tests.<module>.<Class>.<method>` (or `tools.tests.`) whose module
/// exists on the candidate. A Rust name or a bare `Class.method` (no module to
/// run) is `None`.
fn claimed_python_suite(worktree: &Path, claimed: &str) -> Option<(String, &'static str)> {
    let parts: Vec<&str> = claimed.split('.').collect();
    let identifier =
        |part: &&str| !part.is_empty() && part.chars().all(|c| c.is_alphanumeric() || c == '_');
    if parts.len() < 3
        || !matches!(parts[0], "scripts" | "tools")
        || parts[1] != "tests"
        || !parts.iter().all(identifier)
    {
        return None;
    }
    (3..=parts.len()).rev().find_map(|length| {
        let file = format!("{}.py", parts[..length].join("/"));
        worktree.join(&file).is_file().then_some((file, "unittest"))
    })
}

/// Whether a claimed test name reads as a Python unittest id rather than a Rust
/// path, for the refusal's next step.
fn is_python_claim(claimed: &str) -> bool {
    !claimed.contains("::") && claimed.contains('.')
}

/// The selection a diff owes, before any claimed test is added.
fn changed_python_suite_commands(
    worktree: &Path,
    base: &str,
    candidate: &str,
) -> Result<Vec<GuardCommand>, String> {
    let paths = changed_paths(worktree, base, candidate)?;
    // A path anywhere else (a committed manifest or lockfile a Python suite
    // reads) is observed by the suite that names it, so only a docs/thoughts
    // landing skips the selector (#3205). Such a landing owes the selector only
    // when the candidate ships one; a scripts/tools change still requires it.
    let own_paths = paths
        .iter()
        .any(|path| path.starts_with(b"scripts/") || path.starts_with(b"tools/"));
    let other_paths = paths
        .iter()
        .any(|path| !path.starts_with(b"docs/") && !path.starts_with(b"thoughts/"));
    if !(own_paths || other_paths && worktree.join("scripts/python-test-gate.py").is_file())
        || !(worktree.join("scripts/tests").is_dir() || worktree.join("tools/tests").is_dir())
    {
        return Ok(Vec::new());
    }
    let output = lander_env::command("python3")
        .args([
            "scripts/python-test-gate.py",
            "--base",
            base,
            "--head",
            candidate,
        ])
        .current_dir(worktree)
        .output()
        .map_err(|error| format!("cannot select Python tests: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot select Python tests: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let commands: Vec<Vec<String>> = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid Python test plan: {error}"))?;
    commands
        .into_iter()
        .map(|args| {
            let command = GuardCommand {
                program: "python3".into(),
                // `-B`: never write bytecode into the candidate worktree, or
                // its cleanup refuses the dirty tree after publication.
                args: std::iter::once("-B".to_owned())
                    .chain(args.into_iter().skip(1))
                    .collect(),
                timeout: PYTHON_TEST_COMMAND_TIMEOUT,
            };
            if !is_python_test_guard(&command) {
                return Err(format!("invalid Python test command: {command:?}"));
            }
            Ok(command)
        })
        .collect()
}

fn is_python_test_guard(command: &GuardCommand) -> bool {
    command.program == "python3"
        && command.args.first().is_some_and(|arg| arg == "-B")
        && command.args.get(1).is_some_and(|arg| arg == "-m")
        && command
            .args
            .get(2)
            .is_some_and(|arg| arg == "unittest" || arg == "pytest")
}

/// The failing tests of a Python suite run, as ids an isolated rerun accepts
/// (#2576): a unittest `module.Class.method` from its `FAIL:`/`ERROR:` summary
/// lines, or a pytest node id from its `FAILED`/`ERROR` short-summary lines.
/// A failure that names no single test (a module that cannot import, a
/// `setUpClass`/`setUpModule` error) is `Err`: nothing narrower than the whole
/// suite can be rerun, so the caller keeps the strict refusal.
fn python_failure_ids(output: &str) -> Result<BTreeSet<String>, String> {
    let mut ids = BTreeSet::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("FAIL: ")
            .or_else(|| line.strip_prefix("ERROR: "))
        {
            // `name (dotted.id)`, optionally followed by ` (subtest params)`.
            let (name, after) = rest
                .split_once(" (")
                .ok_or_else(|| format!("unparsed Python failure line: {line}"))?;
            let dotted = after
                .split_once(')')
                .map(|(dotted, _)| dotted)
                .ok_or_else(|| format!("unparsed Python failure line: {line}"))?;
            let plain = !name.is_empty()
                && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !name.starts_with("setUp")
                && !name.starts_with("tearDown")
                && !dotted.starts_with("unittest.")
                && dotted
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '.');
            if !plain {
                return Err(format!("Python failure names no single test: {line}"));
            }
            ids.insert(if dotted.ends_with(&format!(".{name}")) {
                dotted.to_owned()
            } else {
                // Python before 3.11 prints the class only.
                format!("{dotted}.{name}")
            });
        } else if let Some(rest) = line
            .strip_prefix("FAILED ")
            .or_else(|| line.strip_prefix("ERROR "))
            // unittest's closing `FAILED (failures=1)` is a verdict, not a test.
            .filter(|rest| !rest.starts_with('('))
        {
            let id = rest.split(" - ").next().unwrap_or(rest).trim();
            if !id.contains("::") {
                return Err(format!("Python failure names no single test: {line}"));
            }
            ids.insert(id.to_owned());
        }
    }
    Ok(ids)
}

/// The passing unittest ids of a verbose run, for the #2167 claimed-fix check.
fn python_pass_ids(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim().strip_suffix(" ... ok")?;
            let (name, rest) = line.split_once(" (")?;
            let dotted = rest.strip_suffix(')')?;
            Some(if dotted.ends_with(&format!(".{name}")) {
                dotted.to_owned()
            } else {
                format!("{dotted}.{name}")
            })
        })
        .collect()
}

/// Whether the claimed fix `claimed` names the Python test `id`: the full
/// dotted id, or a `Class.method` / bare `method` suffix of it.
fn python_claim_matches(claimed: &str, id: &str) -> bool {
    claimed == id || id.ends_with(&format!(".{claimed}"))
}

/// The command that runs only `id` of the Python suite `command` runs.
fn python_isolated_command(command: &GuardCommand, id: &str) -> GuardCommand {
    let runner = command.args.get(2).map_or("unittest", String::as_str);
    let flag = if runner == "pytest" { "-q" } else { "-v" };
    GuardCommand {
        program: command.program.clone(),
        args: vec![
            "-B".into(),
            "-m".into(),
            runner.into(),
            flag.into(),
            id.into(),
        ],
        timeout: command.timeout,
    }
}

/// Whether an isolated Python rerun passed and proves it ran exactly the test.
fn python_isolated_passed(report: &rsid::integration::GuardCommandReport) -> bool {
    if report.status != rsid::integration::GuardStatus::Passed {
        return false;
    }
    let output = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
    output.contains("Ran 1 test") || output.contains("1 passed")
}

/// #2576: the landing rule is "no new failures relative to rolling" for the
/// Python suites as for the Rust shards. A failing suite is triaged per test:
/// a test that passes when rerun alone is a flake (recorded, not refused); one
/// that still fails is run on the base, and a base that fails it too is a
/// baseline red (recorded, not refused). Only a failure the base does not share
/// refuses. A failure that names no single test, a hung or unavailable runner
/// and a claimed fix (#2167) that is red refuse as before.
async fn triage_python_failure(
    repo: &Path,
    candidate_worktree: &Path,
    base: &str,
    spec: &GuardSpec,
    command: &GuardCommand,
    report: &rsid::integration::GuardCommandReport,
    gate: &mut TestGate,
) -> Result<(), String> {
    let strict = || {
        format!(
            "Python test gate failed ({:?}) running {} {:?}: {} {}",
            report.status, report.program, report.args, report.stdout_tail, report.stderr_tail
        )
    };
    if !matches!(report.status, rsid::integration::GuardStatus::Failed { .. }) {
        return Err(strict());
    }
    let output = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
    let ids = match python_failure_ids(&output) {
        Ok(ids) if !ids.is_empty() => ids,
        _ => return Err(strict()),
    };
    let still_red: Vec<String> = ids
        .iter()
        .filter(|id| {
            gate.claimed_fixes
                .iter()
                .any(|claimed| python_claim_matches(claimed, id))
        })
        .cloned()
        .collect();
    if !still_red.is_empty() {
        return Err(claimed_fix_still_red_message(
            &still_red,
            "the claimed fix still fails on the candidate (python tests)",
        ));
    }
    let mut new_reds = Vec::new();
    let mut evidence: Vec<(String, Vec<String>)> = Vec::new();
    for id in &ids {
        let isolated = python_isolated_command(command, id);
        let retry = run_one_guard(candidate_worktree, spec, &isolated).await?;
        if python_isolated_passed(&retry) {
            println!("candidate_flake={id}");
            gate.flakes.insert(id.clone());
            continue;
        }
        let base_worktree = ensure_base_worktree(repo, base, gate)?;
        let on_base = run_one_guard(&base_worktree, spec, &isolated).await?;
        let base_output = format!("{}\n{}", on_base.stdout_tail, on_base.stderr_tail);
        let base_red = matches!(
            on_base.status,
            rsid::integration::GuardStatus::Failed { .. }
        ) && python_failure_ids(&base_output)
            .is_ok_and(|failures| failures.contains(id));
        if base_red {
            println!("python_base_red={id}");
            gate.base_reds.insert(id.clone());
            gate.base_red_commands
                .entry(id.clone())
                .or_insert_with(|| guard_command_line(&isolated));
        } else {
            new_reds.push(id.clone());
            // #2793: the isolated rerun's assertion and traceback, whole in the
            // lander log and condensed into the refusal the queue stores.
            let block =
                python_failure_block(&format!("{}\n{}", retry.stdout_tail, retry.stderr_tail), id);
            eprintln!(
                "{}",
                python_failure_log_entry(id, &guard_command_line(&isolated), &block)
            );
            evidence.push((id.clone(), block));
        }
    }
    if new_reds.is_empty() {
        return Ok(());
    }
    Err(python_new_failures_message(base, &new_reds, &evidence))
}

/// One failing test's whole block as the lander log carries it (#2793).
fn python_failure_log_entry(id: &str, command: &str, block: &[String]) -> String {
    format!(
        "python_failure_evidence id={id} command={command}\n{}",
        block.join("\n")
    )
}

/// The refusal for new-red Python tests (#2793, #2813). The queue outcome
/// detail keeps only the newest [`rsid::rolling_queue::OUTCOME_TAIL_BYTES`] of
/// the lander's stderr, so the whole message is held inside that limit (less
/// [`PYTHON_MESSAGE_SLACK`] for whatever the lander prints around it): every
/// failing test id leads the message, and the evidence budget left is shared
/// across the tests. A test whose share would be too small to carry an
/// assertion is named as cut instead of being silently dropped; the whole
/// blocks stay in the lander log.
fn python_new_failures_message(
    base: &str,
    new_reds: &[String],
    evidence: &[(String, Vec<String>)],
) -> String {
    const WHY: &str = "Python tests that fail on the candidate (also alone) and not on the base";
    const EVIDENCE: &str = "; isolated rerun evidence: ";
    const SEPARATOR: &str = " || ";
    if evidence.is_empty() {
        return new_failures_message(base, new_reds, WHY);
    }
    let bare = new_failures_message(base, new_reds, WHY).len();
    let budget = rsid::rolling_queue::OUTCOME_TAIL_BYTES
        .saturating_sub(PYTHON_MESSAGE_SLACK)
        .saturating_sub(bare + EVIDENCE.len());
    // Per entry: the id, `: `, and the separator that follows it.
    let overhead = |id: &str| id.len() + 2 + SEPARATOR.len();
    let cut_note = |cut: usize| {
        format!("{cut} more failing test(s) cut here, named above; full evidence in the lander log")
    };
    // The most tests that each keep a useful share of the budget.
    let mut kept = evidence.len();
    while kept > 0 {
        let cut = evidence.len() - kept;
        let reserved = if cut == 0 { 0 } else { cut_note(cut).len() };
        let fixed: usize = evidence[..kept].iter().map(|(id, _)| overhead(id)).sum();
        let room = budget.saturating_sub(reserved).saturating_sub(fixed);
        if room / kept >= PYTHON_MIN_SUMMARY_BYTES {
            break;
        }
        kept -= 1;
    }
    let cut = evidence.len() - kept;
    let reserved = if cut == 0 { 0 } else { cut_note(cut).len() };
    let fixed: usize = evidence[..kept].iter().map(|(id, _)| overhead(id)).sum();
    let share = budget
        .saturating_sub(reserved)
        .saturating_sub(fixed)
        .checked_div(kept)
        .unwrap_or(0);
    let mut entries: Vec<String> = evidence[..kept]
        .iter()
        .map(|(id, block)| format!("{id}: {}", python_failure_summary(block, share)))
        .collect();
    if cut > 0 {
        entries.push(cut_note(cut));
    }
    new_failures_message(
        base,
        new_reds,
        &format!("{WHY}{EVIDENCE}{}", entries.join(SEPARATOR)),
    )
}

/// Lines of one failing Python test kept whole in the lander log (#2793).
const PYTHON_EVIDENCE_LINES: usize = 60;
/// Bytes left unused in the refusal message for what the lander prints around
/// it before the queue keeps the newest `OUTCOME_TAIL_BYTES` (#2813).
const PYTHON_MESSAGE_SLACK: usize = 200;
/// Fewest bytes a test's condensed evidence may get; below it the test is named
/// as cut rather than shown as a fragment (#2813).
const PYTHON_MIN_SUMMARY_BYTES: usize = 80;

/// The unittest failure/error text for `id` in a run's output: the lines after
/// its `FAIL:`/`ERROR:` header (the traceback and the assertion message, with
/// anything the test printed) up to the next header or the `Ran N tests`
/// footer, newest [`PYTHON_EVIDENCE_LINES`] only. Output with no such header
/// (a pytest run, a crash) falls back to the newest lines of the whole output,
/// so a red never reaches the log without its text.
fn python_failure_block(output: &str, id: &str) -> Vec<String> {
    let lines: Vec<&str> = output.lines().collect();
    let is_rule = |line: &str| {
        let line = line.trim();
        line.len() >= 10 && (line.chars().all(|c| c == '=') || line.chars().all(|c| c == '-'))
    };
    let header = lines.iter().position(|line| {
        let line = line.trim();
        (line.starts_with("FAIL: ") || line.starts_with("ERROR: "))
            && python_failure_ids(line).is_ok_and(|ids| ids.contains(id))
    });
    let block: Vec<&str> = match header {
        Some(start) => lines[start + 1..]
            .iter()
            .copied()
            .take_while(|line| {
                let line = line.trim();
                !(line.chars().all(|c| c == '=') && line.len() >= 10)
                    && !line.starts_with("Ran ")
                    && !line.starts_with("FAILED (")
            })
            .filter(|line| !is_rule(line))
            .collect(),
        None => lines.clone(),
    };
    let mut block: Vec<&str> = block
        .into_iter()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .into_iter()
        .skip_while(|line| line.is_empty())
        .collect();
    while block.last().is_some_and(|line| line.is_empty()) {
        block.pop();
    }
    let skip = block.len().saturating_sub(PYTHON_EVIDENCE_LINES);
    block[skip..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

/// A test's failure block as one line for the refusal message: the newest
/// lines (the assertion ends a traceback) joined with ` | `, cut to
/// `max_bytes` from the front.
fn python_failure_summary(block: &[String], max_bytes: usize) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in block
        .iter()
        .rev()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
    {
        if bytes + line.len() + 3 > max_bytes {
            if parts.is_empty() {
                // One line longer than the budget: keep its newest bytes.
                let mut start = line.len() - max_bytes.min(line.len());
                while !line.is_char_boundary(start) {
                    start += 1;
                }
                parts.push(&line[start..]);
            }
            break;
        }
        bytes += line.len() + 3;
        parts.push(line);
    }
    if parts.is_empty() {
        return "no failure text in the isolated rerun output".to_owned();
    }
    parts.reverse();
    parts.join(" | ")
}

/// The environment a gate's commands run in, once per gate (#2793): an
/// environment-only red cannot be explained without it. Key NAMES only for the
/// `RSI_`/`CARGO_` families (a value may be a credential); `TMPDIR` is a path.
fn gate_environment_line(
    cwd: &Path,
    spec_env: &BTreeMap<String, String>,
    process_env: &BTreeMap<String, String>,
    unit: &str,
) -> String {
    let tmpdir = spec_env
        .get("TMPDIR")
        .or_else(|| process_env.get("TMPDIR"))
        .map_or("unset", String::as_str);
    let keys: BTreeSet<&str> = process_env
        .keys()
        .chain(spec_env.keys())
        .map(String::as_str)
        .filter(|key| key.starts_with("RSI_") || key.starts_with("CARGO_"))
        .filter(|key| *key != "RSI_SESSION_TOKEN")
        .collect();
    let overridden: Vec<&str> = spec_env.keys().map(String::as_str).collect();
    format!(
        "gate_env=cwd={} tmpdir={tmpdir} unit={unit} spec_env_keys=[{}] rsi_cargo_keys=[{}]",
        cwd.display(),
        overridden.join(","),
        keys.into_iter().collect::<Vec<_>>().join(",")
    )
}

/// The systemd unit (the last cgroup path component) this process runs in.
fn current_systemd_unit() -> String {
    std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| {
            text.lines()
                .filter_map(|line| line.rsplit(':').next())
                .filter_map(|path| path.trim().rsplit('/').next())
                .find(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Print [`gate_environment_line`] once per lander process.
fn print_gate_environment_once(cwd: &Path, spec: &GuardSpec) {
    static PRINTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if PRINTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let process_env: BTreeMap<String, String> = lander_env::vars_os()
        .into_iter()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    println!(
        "{}",
        gate_environment_line(cwd, &spec.env, &process_env, &current_systemd_unit())
    );
}

fn test_failure_names(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("test ") {
                // libtest prints a `#[should_panic]` test as `NAME - should
                // panic`. The suffix is not part of the test identity: left in,
                // the name was never a plain test name, so the narrowed base
                // run (#1525) fell back to the whole selection (#2059).
                return rest.strip_suffix(" ... FAILED").map(|name| {
                    name.strip_suffix(" - should panic")
                        .unwrap_or(name)
                        .to_owned()
                });
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

/// #2167: the tests a run reports passing: libtest `test NAME ... ok` lines
/// and nextest `PASS [...] ... NAME` lines (the gate's nextest runs print every
/// status), named exactly as [`test_failure_names`] names a failure.
fn test_pass_names(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("test ") {
                return rest.strip_suffix(" ... ok").map(|name| {
                    name.strip_suffix(" - should panic")
                        .unwrap_or(name)
                        .to_owned()
                });
            }
            let (status, rest) = line.split_once(' ')?;
            if status != "PASS" || !rest.trim_start().starts_with('[') {
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
    /// Lines of one panic message kept verbatim; the rest are counted.
    const PANIC_LINES: usize = 4;
    /// Room kept for the elision marker so the result stays within `LIMIT`.
    const MARKER_RESERVE: usize = 96;
    let mut evidence = String::new();
    let mut full = false;
    let mut omitted = 0usize;
    for stream in [&report.stderr_tail, &report.stdout_tail] {
        let lines: Vec<&str> = stream.lines().map(str::trim).collect();
        for (index, line) in lines.iter().enumerate() {
            let details: Vec<&str> = if line.contains("panicked at") {
                // The whole message, up to the next panic, the backtrace note
                // or the libtest summary: a multi-line list is one message.
                lines[index..]
                    .iter()
                    .enumerate()
                    .take_while(|(offset, text)| {
                        *offset == 0
                            || !(text.contains("panicked at")
                                || text.starts_with("stack backtrace")
                                || text.starts_with("---- ")
                                || text.starts_with("failures:"))
                    })
                    .map(|(_, text)| *text)
                    .filter(|text| !text.is_empty() && !text.starts_with("note: run with"))
                    .collect()
            } else if line.starts_with("Error: ") {
                vec![*line]
            } else {
                continue;
            };
            let cap = if line.contains("panicked at") {
                PANIC_LINES
            } else {
                1
            };
            for (position, detail) in details.iter().enumerate() {
                if full || position >= cap {
                    omitted += 1;
                    continue;
                }
                if evidence.len() + detail.len() + 3 > LIMIT - MARKER_RESERVE {
                    full = true;
                    omitted += 1;
                    continue;
                }
                if !evidence.is_empty() {
                    evidence.push_str(" | ");
                }
                evidence.push_str(detail);
            }
        }
    }
    if omitted > 0 {
        // The lander does not know the queue log path; the refusal is stored
        // beside it, so name the log by role.
        evidence.push_str(&format!(
            " (+{omitted} more lines; bounded output in the queue gate log)"
        ));
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

/// Where a shard command reads its package list from: the tree it will run in.
/// The candidate binary's own static map names the packages of the *current*
/// layout; a base cut before the #1021 S4 crate split declares every shard in
/// `rsid` alone, so its direct and retry commands follow its own manifests.
#[derive(Clone, Copy)]
enum ShardTree<'a> {
    Worktree(&'a Path),
    Revision {
        repo: &'a Path,
        revision: &'a str,
    },
    /// The lander's own static map (unit tests with no tree to read).
    #[cfg(test)]
    Static,
}

impl ShardTree<'_> {
    fn manifest(&self, package: &str) -> Option<String> {
        match self {
            Self::Worktree(root) => {
                std::fs::read_to_string(root.join(format!("crates/{package}/Cargo.toml"))).ok()
            }
            Self::Revision { repo, revision } => git_text(
                repo,
                &["show", &format!("{revision}:crates/{package}/Cargo.toml")],
            )
            .ok(),
            #[cfg(test)]
            Self::Static => None,
        }
    }

    /// The packages whose manifest declares `test-shard-<shard>`; the static
    /// map when the tree has no readable manifest declaring it.
    fn packages(&self, shard: &str) -> Vec<String> {
        let needle = format!("test-shard-{shard} =");
        let declared: Vec<String> = ["rsid", "rsid-store"]
            .into_iter()
            .filter(|package| {
                self.manifest(package).is_some_and(|text| {
                    text.lines()
                        .any(|line| line.trim_start().starts_with(&needle))
                })
            })
            .map(str::to_owned)
            .collect();
        if declared.is_empty() {
            rsid_shard_packages(shard)
                .iter()
                .map(|package| (*package).to_owned())
                .collect()
        } else {
            declared
        }
    }
}

fn isolated_retry_command(
    command: &GuardCommand,
    name: &str,
    tree: ShardTree<'_>,
) -> Result<GuardCommand, String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
    {
        return Err(format!("cannot isolate test failure: {name}"));
    }
    let mut retry = command.clone();
    if rsid_lib_filterset(command).is_some() {
        // The shared lib build: the same packages and features, one test.
        let mut args = vec!["test".to_owned()];
        for package in RSID_LIB_TEST_PACKAGES {
            args.extend(["-p".to_owned(), package.to_owned()]);
        }
        args.extend([
            "--lib".into(),
            name.into(),
            "--".into(),
            "--exact".into(),
            "--test-threads=4".into(),
        ]);
        retry.args = args;
    } else if retry.program == "scripts/run-rsid-test-shards.sh" {
        let shard = retry.args.get(1).ok_or("rsid shard command has no shard")?;
        retry.program = "cargo".into();
        let shard_packages = tree.packages(shard);
        if shard_packages.is_empty() {
            return Err(format!("unknown rsid shard for isolated retry: {shard}"));
        }
        let mut args = vec!["test".to_owned()];
        for package in shard_packages {
            args.extend(["-p".to_owned(), package]);
        }
        args.extend([
            "--lib".into(),
            "--no-default-features".into(),
            "--features".into(),
            format!("test-shard-{shard}"),
            name.into(),
            "--".into(),
            "--exact".into(),
            "--test-threads=4".into(),
        ]);
        retry.args = args;
    } else if retry.program == "cargo" {
        let separator = retry
            .args
            .iter()
            .position(|arg| arg == "--")
            .ok_or("cargo test command has no argument separator")?;
        if separator < 4 || retry.args.first().is_none_or(|arg| arg != "test") {
            return Err("unsupported cargo test command for isolated retry".into());
        }
        // Keep the target selector and its operand; a named target takes one
        // more argument than --lib. Only replace the optional test filter.
        let target_end = match retry.args[3].as_str() {
            "--lib" => 4,
            "--test" | "--bin" if separator >= 5 => 5,
            _ => return Err("unsupported cargo test target for isolated retry".into()),
        };
        retry.args.truncate(target_end);
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

/// True when an isolated retry ran no test. A shard retry spans every package
/// that declares the shard feature (`-p rsid -p rsid-store`), and the package
/// without the test prints its own `running 0 tests`, so that line alone proves
/// nothing: a retry ran the test when any package ran one (`running N tests`,
/// N > 0) or printed the test's own result line (#1167).
fn retry_ran_no_test(output: &str, name: &str) -> bool {
    let mut saw_zero = false;
    for line in output.lines().map(str::trim) {
        if line
            .strip_prefix("test ")
            .is_some_and(|rest| rest.starts_with(&format!("{name} ... ")))
        {
            return false;
        }
        if let Some(count) = line
            .strip_prefix("running ")
            .and_then(|rest| rest.strip_suffix(" tests").or(rest.strip_suffix(" test")))
        {
            match count.parse::<u64>() {
                Ok(0) => saw_zero = true,
                Ok(_) => return false,
                Err(_) => {}
            }
        }
    }
    saw_zero
}

/// Whether the source's diff can have changed the module a failing test lives
/// in. The first segment of the test path names its module; a changed file
/// under `src/` whose directory or file stem equals it touches it. Anything
/// that cannot be read (no diff, a manifest or build-script change) counts as
/// touched, so an unknown never lets a failure through (#1167).
fn source_touches_test_module(changed: Option<&[String]>, name: &str) -> bool {
    let Some(changed) = changed else {
        return true;
    };
    let module = name.split("::").next().unwrap_or(name);
    changed.iter().any(|path| {
        let path = Path::new(path);
        let file = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default();
        if matches!(file, "Cargo.toml" | "Cargo.lock" | "build.rs") {
            return true;
        }
        let mut under_src = false;
        for component in path.components() {
            let component = component.as_os_str().to_str().unwrap_or_default();
            if under_src
                && (component == module
                    || Path::new(component).file_stem().and_then(|s| s.to_str()) == Some(module))
            {
                return true;
            }
            under_src |= component == "src";
        }
        false
    })
}

/// The isolated retry of one failure with its own bounded guard, so a hung
/// retry fails the gate quickly instead of holding it for the shard timeout.
fn bounded_isolated_retry_command(
    command: &GuardCommand,
    name: &str,
    tree: ShardTree<'_>,
) -> Result<GuardCommand, String> {
    let mut isolated = isolated_retry_command(command, name, tree)?;
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
    let isolated = bounded_isolated_retry_command(command, name, ShardTree::Worktree(worktree))?;
    let report = run_one_guard(worktree, spec, &isolated).await?;
    if report.status == rsid::integration::GuardStatus::TimedOut {
        return Err(retry_hung_error(name, isolated.timeout));
    }
    Ok(report)
}

/// Build id every gate build embeds as `RSI_BUILD_SHA` (#2508).
const GATE_BUILD_ID: &str = "gate-build";

/// Gate binaries are never deployed, so a gate build does not need the
/// candidate's commit in `RSI_BUILD_SHA`. Left to `crates/rsid/build.rs` it
/// reads `git rev-parse HEAD` of the checkout, which differs for every
/// candidate and makes the build script rerun, so `rsid` and every crate above
/// it recompile in a warm slot whose sources did not change. An id the spec
/// already carries stays.
fn pin_gate_build_id(env: &mut BTreeMap<String, String>) {
    env.entry("RSI_BUILD_SHA".into())
        .or_insert_with(|| GATE_BUILD_ID.into());
}

/// Hub #3483: the complete selected-test inventory of one guard command, or
/// `None` without a live known-red signature, a listable command or a clean
/// listing.
async fn list_selected_tests(
    worktree: &Path,
    spec: &GuardSpec,
    command: &GuardCommand,
    gate: &TestGate,
) -> Option<BTreeSet<String>> {
    let has_live_signature = gate
        .known_failures
        .as_ref()
        .is_some_and(|snapshot| snapshot.live_records().next().is_some());
    if !has_live_signature {
        return None;
    }
    let list = known_red_timeout::list_command(command, ShardTree::Worktree(worktree))?;
    let report = run_one_guard(worktree, spec, &list).await.ok()?;
    known_red_timeout::selected_tests(&report, spec.output_tail_bytes)
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
        pin_gate_build_id(&mut env);
        // Cargo fingerprints use file mtimes. A candidate worktree can be
        // older than artifacts produced by the base run, so sharing one target
        // may compile candidate rsid against base rsi-common metadata.
        let parent = worktree.parent().ok_or("guard worktree has no parent")?;
        let name = worktree
            .file_name()
            .ok_or("guard worktree has no file name")?
            .to_string_lossy();
        // `cargo check` (dev profile) shares no artifacts with the test
        // builds, so it gets its own target and can run beside them (#1244).
        let is_check =
            command.program == "cargo" && command.args.first().is_some_and(|arg| arg == "check");
        let suffix = if is_check {
            "check-target"
        } else {
            "cargo-target"
        };
        // #2467: the candidate worktree of a gate that leased a warm slot
        // builds into the slot's persistent targets; every other tree keeps
        // its private pair.
        let target = match warm_bindings().get(worktree) {
            Some((test, check)) => (if is_check { check } else { test }).clone(),
            None => parent.join(format!("{name}-{suffix}")),
        };
        env.insert(
            "CARGO_TARGET_DIR".into(),
            target.to_string_lossy().into_owned(),
        );
    }
    let (executable, _filter_config) = nextest_filter_config(worktree, command)?;
    let wrapped = isolated_slot_command(
        worktree,
        &executable,
        lander_env::var_os("RSI_LANDER_LOCAL_SLOT_WRAPPER").as_deref(),
    );
    let report = run_guard_with_env(
        worktree,
        &GuardSpec {
            commands: vec![wrapped],
            env,
            output_tail_bytes: spec.output_tail_bytes,
        },
        lander_env::current().as_map(),
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
    persist_guard_test_timings(worktree, &report);
    Ok(report)
}

fn persist_guard_test_timings(worktree: &Path, report: &rsid::integration::GuardCommandReport) {
    let Some(path) = lander_env::var_os(rsid::integration::TEST_TIMINGS_ENV) else {
        return;
    };
    static RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tree = worktree.file_name().unwrap_or_default().to_string_lossy();
    let run = format!(
        "{tree}-{}",
        RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    if let Err(error) = rsid::integration::append_test_timings(
        Path::new(&path),
        &run,
        &report.stdout_tail,
        &report.stderr_tail,
    ) {
        eprintln!(
            "test_timings=unavailable path={} detail={error}",
            Path::new(&path).display()
        );
    }
}

/// Windows has a 32 KiB command line, and 400 long test names can exceed it.
/// Nextest supports a profile default-filter in its config file. Copy the
/// repository config, intersect its default with our expression, and pass a
/// short `default()` argv filter. Keep the file alive through run/list execution.
fn nextest_filter_config(
    worktree: &Path,
    command: &GuardCommand,
) -> Result<(GuardCommand, Option<tempfile::NamedTempFile>), String> {
    if command.program != "cargo" || command.args.first().map(String::as_str) != Some("nextest") {
        return Ok((command.clone(), None));
    }
    let Some(at) = command.args.iter().position(|arg| arg == "--filterset") else {
        return Ok((command.clone(), None));
    };
    let filter = command
        .args
        .get(at + 1)
        .ok_or("nextest filterset missing its expression")?;
    if filter.len() < 8 * 1024 {
        return Ok((command.clone(), None));
    }
    let profile = command
        .args
        .windows(2)
        .find(|args| args[0] == "--profile")
        .map(|args| args[1].as_str())
        .unwrap_or("default");
    let path = worktree.join(".config/nextest.toml");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut config: toml::Value =
        toml::from_str(&text).map_err(|e| format!("read nextest config: {e}"))?;
    let profiles = config
        .get_mut("profile")
        .and_then(toml::Value::as_table_mut)
        .ok_or("nextest config has no profiles")?;
    let mut current = profile;
    let mut seen = BTreeSet::new();
    let prior = loop {
        if !seen.insert(current.to_owned()) {
            return Err("nextest profile inheritance cycle".into());
        }
        let selected = profiles
            .get(current)
            .and_then(toml::Value::as_table)
            .ok_or("nextest config has no selected profile")?;
        if let Some(filter) = selected.get("default-filter").and_then(toml::Value::as_str) {
            break filter.to_owned();
        }
        if current == "default" {
            break "all()".to_owned();
        }
        current = selected
            .get("inherits")
            .and_then(toml::Value::as_str)
            .unwrap_or("default");
    };
    // #2270: a run that ignores the default-filter (the slow tier) must not get it back
    // through `default()`, which names this very expression.
    let prior = if command
        .args
        .iter()
        .any(|arg| arg == "--ignore-default-filter")
    {
        "all()".to_owned()
    } else {
        prior
    };
    let expression = format!("({prior}) & ({filter})");
    profiles
        .get_mut(profile)
        .and_then(toml::Value::as_table_mut)
        .ok_or("nextest config has no selected profile")?
        .insert("default-filter".into(), toml::Value::String(expression));
    // Platform overrides can replace the profile default. Intersect those
    // too, including inherited overrides, to retain the same selection.
    for (_, settings) in profiles.iter_mut() {
        if let Some(overrides) = settings
            .get_mut("overrides")
            .and_then(toml::Value::as_array_mut)
        {
            for settings in overrides {
                if let Some(default) = settings.get_mut("default-filter") {
                    let prior = default
                        .as_str()
                        .ok_or("nextest default-filter is not a string")?;
                    *default = toml::Value::String(format!("({prior}) & ({filter})"));
                }
            }
        }
    }
    let mut file = tempfile::Builder::new()
        .prefix("rsi-nextest-filter-")
        .suffix(".toml")
        .tempfile()
        .map_err(|e| format!("create nextest filter config: {e}"))?;
    use std::io::Write;
    file.write_all(
        toml::to_string(&config)
            .map_err(|e| format!("encode nextest config: {e}"))?
            .as_bytes(),
    )
    .map_err(|e| format!("write nextest filter config: {e}"))?;
    let mut executable = command.clone();
    executable.args[at + 1] = "default()".into();
    executable
        .args
        .extend(["--config-file".into(), file.path().display().to_string()]);
    Ok((executable, Some(file)))
}

/// The helper that runs a test command in its own PID namespace (#3478).
const PID_ISOLATE_SCRIPT: &str = "scripts/pid-isolate.py";

/// A command that executes tests: cargo test/nextest, the shard runner and a
/// Python suite. Static guards and git checks run as before.
fn runs_tests(command: &GuardCommand) -> bool {
    is_test_guard(command)
        || is_python_test_guard(command)
        || (command.program == "cargo" && command.args.first().is_some_and(|arg| arg == "nextest"))
}

/// #3478: a test the gate runs must not see or signal host processes. A Python
/// test that ran `install-release.sh --link-only --now` found the real hub
/// daemon with a host-wide `pgrep` and interrupted it in every gate (#3476).
/// The candidate's `scripts/pid-isolate.py` starts the command in a user + PID
/// namespace with a fresh `/proc` and prints `pid_isolation=namespaced` or
/// `pid_isolation=unavailable reason=...` into the gate log; where the host
/// refuses user namespaces it runs the command unwrapped, as before. A tree
/// without the helper (a base side older than this change) is left alone.
fn pid_isolated_command(worktree: &Path, command: &GuardCommand) -> Option<GuardCommand> {
    if !cfg!(unix) || !runs_tests(command) || !worktree.join(PID_ISOLATE_SCRIPT).is_file() {
        return None;
    }
    let mut args = Vec::with_capacity(command.args.len() + 2);
    args.push("--".to_owned());
    args.push(command.program.clone());
    args.extend(command.args.iter().cloned());
    Some(GuardCommand {
        program: PID_ISOLATE_SCRIPT.into(),
        args,
        timeout: command.timeout,
    })
}

/// The command as the lander executes it: the build slot (when a wrapper is
/// set) stays outermost, because the slot queue reads host pids, and the PID
/// namespace wraps only the command inside it.
fn isolated_slot_command(
    worktree: &Path,
    command: &GuardCommand,
    wrapper: Option<&std::ffi::OsStr>,
) -> GuardCommand {
    let slotted = slot_wrapped_command(command, wrapper);
    let Some(isolated) = pid_isolated_command(worktree, command) else {
        return slotted;
    };
    match wrapper.filter(|wrapper| !wrapper.is_empty()) {
        Some(wrapper) if slotted != *command => run_under_slot(&isolated, wrapper),
        _ => isolated,
    }
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
    run_under_slot(command, wrapper)
}

/// `command` as the argument of the slot `wrapper`.
fn run_under_slot(command: &GuardCommand, wrapper: &std::ffi::OsStr) -> GuardCommand {
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

/// `program arg ...` for a gate command, as printed beside a base red.
fn guard_command_line(command: &GuardCommand) -> String {
    std::iter::once(command.program.as_str())
        .chain(command.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_test_guard(command: &GuardCommand) -> bool {
    (command.program == "cargo" && command.args.first().is_some_and(|arg| arg == "test"))
        || command.program == "scripts/run-rsid-test-shards.sh"
        || rsid_lib_filterset(command).is_some()
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

/// The test target a gate command runs, in the terms of `stale_delta`
/// (`<family>:lib`, `<family>:bin:NAME`, `<family>:test:NAME`; `rsid` and
/// `rsid-store` are one family). `None` for a command that is not a test run
/// or whose shape is not recognized: it is never shown independent of an
/// advance (#1940).
fn command_test_target(command: &GuardCommand) -> Option<String> {
    if let Some(package) = cargo_test_package(command) {
        let family = stale_delta::family(package);
        let flag = command
            .args
            .iter()
            .position(|arg| arg == "--bin" || arg == "--test");
        return match flag {
            Some(index) => {
                let kind = if command.args[index] == "--bin" {
                    "bin"
                } else {
                    "test"
                };
                let name = command.args.get(index + 1)?;
                Some(format!("{family}:{kind}:{name}"))
            }
            None => Some(format!("{family}:lib")),
        };
    }
    (command.program == "scripts/run-rsid-test-shards.sh" || rsid_lib_filterset(command).is_some())
        .then(|| "rsid:lib".to_owned())
}

/// The package of a `cargo check -p <package> ...` compile check.
fn check_package(command: &GuardCommand) -> Option<&str> {
    match command.args.as_slice() {
        [check, flag, package, ..]
            if command.program == "cargo" && check == "check" && flag == "-p" =>
        {
            Some(package.as_str())
        }
        _ => None,
    }
}

/// The commands of `spec` a stale re-gate still runs (#1940), with how many it
/// keeps from the earlier gate and how many run again. Static guards always
/// run. A command the verdict does not select keeps the candidate's earlier
/// result: the advance provably touches nothing it exercises.
///
/// #1957: with the advance's [`FamilyObservers`], a selected rsid-family
/// command runs only for what the advance observes: the combined focused run
/// keeps the atoms that can select an observed test (none: it is reused), and
/// a binary or integration target runs only when observed. A whole-shard run
/// or any other family command keeps the verdict's answer.
fn reuse_spec(spec: &GuardSpec, reuse: &GateReuse) -> (GuardSpec, usize, usize) {
    let verdict = &reuse.verdict;
    let runs_test = |command: &GuardCommand| {
        let target = command_test_target(command);
        if !verdict.tests.runs_test(target.as_deref()) {
            return false;
        }
        match (&reuse.family, target.as_deref()) {
            (Some(observers), Some(target))
                if target.starts_with("rsid:") && target != "rsid:lib" =>
            {
                observers.targets.contains(target)
            }
            (Some(observers), _) => rsid_lib_filterset(command).is_none_or(|filterset| {
                !observers
                    .observed_atoms(&focused_atoms(filterset))
                    .is_empty()
            }),
            (None, _) => true,
        }
    };
    // The e2e prebuild only serves the e2e tests: it follows them.
    let e2e_runs = spec.commands.iter().any(|command| {
        is_test_guard(command)
            && runs_test(command)
            && command.args.iter().any(|arg| arg == E2E_TARGET)
    });
    let prebuild = e2e_prebuild_command();
    let mut commands = Vec::new();
    let (mut reused, mut rerun) = (0, 0);
    for command in &spec.commands {
        if is_static_guard(command) {
            commands.push(command.clone());
            continue;
        }
        let keep = if is_python_test_guard(command) {
            verdict.python_everything || reuse.python.contains(command)
        } else if is_test_guard(command) {
            runs_test(command)
        } else if let Some(package) = check_package(command) {
            verdict.checks.runs_check(Some(package))
        } else if *command == prebuild {
            e2e_runs
        } else {
            true
        };
        if !keep {
            reused += 1;
            continue;
        }
        rerun += 1;
        match (&reuse.family, rsid_lib_filterset(command)) {
            (Some(observers), Some(filterset)) => {
                let atoms = focused_atoms(filterset);
                let observed = observers.observed_atoms(&atoms);
                if observed.len() == atoms.len() {
                    commands.push(command.clone());
                } else {
                    commands.push(rsid_focused_run_command(&observed));
                }
            }
            _ => commands.push(command.clone()),
        }
    }
    (
        GuardSpec {
            commands,
            ..spec.clone()
        },
        reused,
        rerun,
    )
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

/// Cargo proves a selected target is absent before building or running tests.
/// Only the base may use this as an empty failure baseline; other Cargo errors
/// (and an absent candidate target) still refuse the gate. A command may select
/// several targets (`--test a --test b`, `--bin c`, `--lib`): every selected
/// target whose absence the output proves is classified, not just the first.
fn base_absent_targets(
    command: &GuardCommand,
    report: &rsid::integration::GuardCommandReport,
) -> Vec<String> {
    if report.output_truncated
        || report.status != (rsid::integration::GuardStatus::Failed { code: Some(101) })
    {
        return Vec::new();
    }
    let Some(package) = cargo_test_package(command) else {
        return Vec::new();
    };
    let Some(args) = command.args.split(|arg| arg == "--").next() else {
        return Vec::new();
    };
    let mut selected: Vec<(String, String)> = args
        .windows(2)
        .filter(|pair| matches!(pair[0].as_str(), "--test" | "--bin"))
        .map(|pair| {
            let kind = pair[0].trim_start_matches("--");
            (
                format!("{kind}:{}", pair[1]),
                format!("error: no {kind} target named `{}`", pair[1]),
            )
        })
        .collect();
    if args.iter().any(|arg| arg == "--lib") {
        selected.push((
            "lib".to_owned(),
            format!("error: no library targets found in package `{package}`"),
        ));
    }
    let output = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
    let mut absent = Vec::new();
    for (target, diagnostic) in selected {
        let proven = output.lines().any(|line| {
            line.trim()
                .strip_prefix(&diagnostic)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(" in "))
        });
        let entry = format!("{package}:{target}");
        if proven && !absent.contains(&entry) {
            absent.push(entry);
        }
    }
    absent
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
    let output = lander_env::command("python3")
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

/// Where a test guard's base result lives in the persistent base cache.
struct BaseCacheIdentity {
    /// Slot file stem (`[A-Za-z0-9-]+`).
    slot_label: String,
    /// `sha256:` digest naming the slot directory under the base SHA.
    slot_key: String,
    /// Human label printed in `base_reused=`.
    display: String,
}

/// `(shard, jobs, filterset)` of a rsid shard command with or without
/// `--filterset`; full shards have `None` for the filterset.
fn shard_command_parts(command: &GuardCommand) -> Option<(&str, u32, Option<&str>)> {
    if let Some((shard, jobs)) = full_shard(command) {
        return Some((shard, jobs, None));
    }
    if command.program != "scripts/run-rsid-test-shards.sh" || command.args.len() != 6 {
        return None;
    }
    let args = &command.args;
    if args[0] != "shard"
        || args[2] != "--jobs"
        || args[4] != "--filterset"
        || !RSID_SHARDS.contains(&args[1].as_str())
    {
        return None;
    }
    Some((&args[1], args[3].parse().ok()?, Some(&args[5])))
}

fn sha256_digest_of(text: &str) -> String {
    use sha2::Digest as _;
    format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(text.as_bytes()))
    )
}

/// The persistent base-cache identity of a test guard (#1132). The base only
/// changes when a batch publishes, so the same base SHA, command and
/// environment class always reproduces the same base failures: a second batch
/// on that base reads the earlier result instead of recompiling and rerunning
/// the base side. `Ok(None)` means the command shape is not cacheable.
fn base_cache_identity(
    candidate_worktree: &Path,
    base: &str,
    command: &GuardCommand,
    env_class: &str,
) -> Result<Option<BaseCacheIdentity>, String> {
    if let Some((shard, jobs, filterset)) = shard_command_parts(command) {
        let fingerprint = shard_fingerprint(candidate_worktree, base, shard, jobs)?;
        let shard_key = shard_cache_key(&fingerprint, env_class)?;
        return Ok(Some(match filterset {
            None => BaseCacheIdentity {
                slot_label: shard.to_owned(),
                slot_key: shard_key,
                display: shard.to_owned(),
            },
            Some(filterset) => BaseCacheIdentity {
                slot_label: shard.to_owned(),
                slot_key: sha256_digest_of(&format!("{shard_key}\nfilterset={filterset}")),
                display: format!("{shard}[{filterset}]"),
            },
        }));
    }
    if command.program != "cargo" || cargo_test_package(command).is_none() {
        return Ok(None);
    }
    let Ok(rustc) = lander_env::command("rustc")
        .arg("-vV")
        .current_dir(candidate_worktree)
        .output()
    else {
        return Ok(None);
    };
    if !rustc.status.success() {
        return Ok(None);
    }
    let canonical = format!(
        "cargo-test\nrustc={}\nargs={:?}\nenv={env_class}",
        String::from_utf8_lossy(&rustc.stdout),
        command.args
    );
    Ok(Some(BaseCacheIdentity {
        slot_label: "cargo-test".into(),
        slot_key: sha256_digest_of(&canonical),
        display: format!("cargo {}", command.args.join(" ")),
    }))
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
        if let Ok(report) = &result {
            persist_guard_test_timings(worktree, report);
        }
        return result;
    }
    let fingerprint_source_sha = git_text(fingerprint_source, &["rev-parse", "HEAD"])?;
    let shard = shard.to_owned();
    let command = command.clone();
    let base_sha = base_sha.to_owned();
    let result = tokio::task::spawn_blocking(lander_env::propagate(move || {
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
    }))
    .await
    .map_err(|error| format!("remote_missing_evidence: executor task failed: {error}"))?;
    if let Ok(report) = &result {
        persist_guard_test_timings(worktree, report);
    }
    result
}

const DEFAULT_REMOTE_GATE_PARALLEL: usize = 4;
const MAX_REMOTE_GATE_PARALLEL: usize = 8;

/// Concurrent remote shard runs on one gate host (`RSI_REMOTE_GATE_PARALLEL`,
/// clamped 1..8, default 4). Each run keeps its fingerprinted `--jobs`, so the
/// value changes wall time, not the proof (#970).
fn remote_gate_parallelism() -> usize {
    parse_remote_gate_parallelism(lander_env::var("RSI_REMOTE_GATE_PARALLEL").ok().as_deref())
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
        handles.push(tokio::task::spawn_blocking(lander_env::propagate(
            move || {
                let _permit = permit;
                work(item)
            },
        )));
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
        let run = tokio::task::spawn_blocking(lander_env::propagate(move || {
            remote
                .lock()
                .map_err(|_| "remote_missing_evidence: executor lock poisoned".to_string())?
                .plan_full_shard(&sha, &candidate, &base, &shard, jobs, &command)
        }))
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

/// Run `command` on the base. `reported` is the gate command it answers for
/// (differs from `command` when the run is narrowed, #1525); receipts and the
/// inventory checks name it.
async fn run_base_guard(
    repo: &Path,
    base: &str,
    spec: &GuardSpec,
    command: &GuardCommand,
    reported: &GuardCommand,
    gate: &mut TestGate,
    fingerprint_source: &Path,
) -> Result<(BaseTestEvidence, BTreeMap<String, String>), String> {
    if !gate.base_packages.contains_key(base) {
        let packages = base_workspace_packages(repo, base);
        gate.base_packages.insert(base.to_owned(), packages);
    }
    let base_packages = gate.base_packages.get(base).and_then(Option::as_ref);
    if let Some(package) = base_absent_package(command, base_packages) {
        // A crate the candidate adds cannot run on the base, so it has no
        // base failures. The candidate side still runs and must pass.
        gate.base_absent_packages.insert(package.to_owned());
        return Ok((BaseTestEvidence::Observed(BTreeSet::new()), BTreeMap::new()));
    }
    let base_worktree = ensure_base_worktree(repo, base, gate)?;
    if runs_e2e_target(command)
        && let Some(detail) = prebuild_base_e2e(&base_worktree, base, spec, gate).await
    {
        // #2863: without the siblings every base e2e test fails with
        // "e2e prebuild incomplete". That is missing infrastructure, not a
        // base red, so it excuses no candidate failure: the base side is
        // unavailable and the candidate's selected run must pass.
        let entry = format!(
            "{base}:e2e_prebuild_failed:{} {:?} {detail}",
            command.program, command.args
        );
        println!("base_test_baseline_unavailable={entry}");
        gate.base_test_baseline_unavailable.insert(entry);
        return Ok((BaseTestEvidence::CompilationUnavailable, BTreeMap::new()));
    }
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
        direct_base_shard_command(command, ShardTree::Worktree(&base_worktree)),
    ) {
        // A base whose shard inventory is statically red (for example an
        // ungated test) refuses before any test runs. That must not make the
        // fix unlandable: run the same bounded harness the script would have
        // run and compare against those real base results. The candidate
        // still runs the script and must pass its static check.
        gate.base_static_inventory_reds
            .insert(format!("{base}:{reason}"));
        let direct_report = run_one_guard(&base_worktree, spec, &direct).await?;
        return base_test_evidence(repo, base, reported, gate, &direct_report);
    }
    base_test_evidence(repo, base, reported, gate, &report)
}

/// A test guard that runs the `e2e_tui` target.
fn runs_e2e_target(command: &GuardCommand) -> bool {
    is_test_guard(command) && command.args.iter().any(|arg| arg == E2E_TARGET)
}

/// #2833: the e2e harness spawns `rsid` (and its trusted siblings) from beside
/// `CARGO_BIN_EXE_rsi`, which `cargo test -p rsi --test e2e_tui` never builds.
/// The candidate gets them from the e2e prebuild, but the base side builds into
/// its own private target and used to run only the test command, so every e2e
/// test failed on the base with "e2e prebuild incomplete" and was reported as a
/// base red. Build the same prebuild into the base target once per base before
/// its first e2e run. A failed or timed-out prebuild is not fatal to the gate
/// but is no comparison evidence either (#2863): it returns the failure detail
/// (for this and every later e2e run on that base) and the caller treats the
/// base e2e result as unavailable.
async fn prebuild_base_e2e(
    base_worktree: &Path,
    base: &str,
    spec: &GuardSpec,
    gate: &mut TestGate,
) -> Option<String> {
    if let Some(done) = gate.base_e2e_prebuilt.get(base) {
        return done.clone();
    }
    let failure = match run_one_guard(base_worktree, spec, &e2e_prebuild_command()).await {
        Ok(report) if report.status == rsid::integration::GuardStatus::Passed => None,
        Ok(report) => Some(format!("fail status={:?}", report.status)),
        Err(error) => Some(format!(
            "error detail={}",
            error.lines().next().unwrap_or_default()
        )),
    };
    println!(
        "base_e2e_prebuild base={base} result={}",
        failure.as_deref().unwrap_or("pass")
    );
    gate.base_e2e_prebuilt
        .insert(base.to_owned(), failure.clone());
    failure
}

/// Exit 101 is nextest's BUILD_FAILED, but Cargo also uses it for metadata,
/// dependency, build-script and other infrastructure errors. Require a rustc
/// source diagnostic followed by its compilation summary, not just that code.
fn baseline_compilation_failed(report: &rsid::integration::GuardCommandReport) -> bool {
    if report.output_truncated
        || report.status != (rsid::integration::GuardStatus::Failed { code: Some(101) })
    {
        return false;
    }
    let output = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
    let lines: Vec<_> = output.lines().map(str::trim).collect();
    test_failure_names(&output).is_empty()
        && lines.iter().any(|line| {
            line.starts_with("error: could not compile `") && line.contains("previous error")
        })
        && lines.windows(2).any(|pair| {
            (pair[0].starts_with("error[E") || pair[0].starts_with("error: "))
                && pair[1].starts_with("--> ")
        })
}

/// Prove the requested package/shard exists in the base; the static fallback
/// used by old-tree retry commands is not inventory evidence for this path.
fn base_test_inventory_known(
    repo: &Path,
    base: &str,
    command: &GuardCommand,
    packages: Option<&BTreeSet<String>>,
) -> bool {
    let Some(packages) = packages else {
        return false;
    };
    if let Some(package) = cargo_test_package(command) {
        return packages.contains(package);
    }
    let Some((shard, _, _)) = shard_command_parts(command) else {
        return false;
    };
    let tree = ShardTree::Revision {
        repo,
        revision: base,
    };
    ["rsid", "rsid-store"].into_iter().any(|package| {
        packages.contains(package)
            && tree.manifest(package).is_some_and(|text| {
                toml::from_str::<toml::Value>(&text)
                    .ok()
                    .is_some_and(|manifest| {
                        manifest
                            .get("features")
                            .and_then(|features| features.get(format!("test-shard-{shard}")))
                            .is_some_and(|feature| feature.is_array())
                    })
            })
    })
}

fn base_test_evidence(
    repo: &Path,
    base: &str,
    command: &GuardCommand,
    gate: &mut TestGate,
    report: &rsid::integration::GuardCommandReport,
) -> Result<(BaseTestEvidence, BTreeMap<String, String>), String> {
    let absent = base_absent_targets(command, report);
    if !absent.is_empty() {
        for target in absent {
            let entry = format!("{base}:{target}");
            println!("base_target_absent={entry}");
            gate.base_absent_log.push(target);
            gate.base_absent_targets.insert(entry);
        }
        return Ok((BaseTestEvidence::Observed(BTreeSet::new()), BTreeMap::new()));
    }
    if baseline_compilation_failed(report) {
        if !base_test_inventory_known(
            repo,
            base,
            command,
            gate.base_packages.get(base).and_then(Option::as_ref),
        ) {
            return Err(
                "baseline_compile_repair_inventory_unknown: cannot prove base test inventory"
                    .into(),
            );
        }
        let entry = format!(
            "{base}:compile_failed:{} {:?}",
            command.program, command.args
        );
        println!("base_test_baseline_unavailable={entry}");
        gate.base_test_baseline_unavailable.insert(entry);
        return Ok((BaseTestEvidence::CompilationUnavailable, BTreeMap::new()));
    }
    let (failures, texts) = record_base_failures(gate, report)?;
    Ok((BaseTestEvidence::Observed(failures), texts))
}

/// A green status alone (or a zero-test run) does not prove selected tests ran
/// when there is no runnable baseline. Accept Cargo/nextest's positive counts.
fn report_proves_tests_ran(report: &rsid::integration::GuardCommandReport) -> bool {
    format!("{}\n{}", report.stdout_tail, report.stderr_tail)
        .lines()
        .any(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            (words.first() == Some(&"running")
                && words
                    .get(1)
                    .and_then(|count| count.parse::<usize>().ok())
                    .is_some_and(|count| count > 0)
                && matches!(words.get(2), Some(&"test" | &"tests")))
                || (words.first() == Some(&"Summary")
                    && words.windows(3).any(|words| {
                        words[0].parse::<usize>().is_ok_and(|count| count > 0)
                            && matches!(words[1], "test" | "tests")
                            && words[2] == "run:"
                    }))
        })
}

/// Base failures of one guard report, remembering each failure's output text
/// for the known-failure annotation (#1016).
fn record_base_failures(
    gate: &mut TestGate,
    report: &rsid::integration::GuardCommandReport,
) -> Result<(BTreeSet<String>, BTreeMap<String, String>), String> {
    let failures = base_failures_from_report(report)?;
    let output = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
    let mut texts = BTreeMap::new();
    for name in &failures {
        let text = failure_text_for(&output, name);
        texts.insert(name.clone(), text.clone());
        gate.base_red_texts.entry(name.clone()).or_insert(text);
    }
    // Cache fills use this report's text, never the first-seen gate annotation.
    Ok((failures, texts))
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
fn direct_base_shard_command(command: &GuardCommand, tree: ShardTree<'_>) -> Option<GuardCommand> {
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
    // The shard's packages: the ones that declare its feature (rsid, rsid-store).
    let mut args: Vec<String> = ["nextest", "run", "--profile", "rsid-fast"]
        .into_iter()
        .map(String::from)
        .collect();
    for package in tree.packages(shard) {
        args.extend(["-p".into(), package]);
    }
    args.extend(
        ["--lib", "--no-default-features", "--features"]
            .into_iter()
            .map(String::from),
    );
    args.push(format!("test-shard-{shard}"));
    if gate_slow_tier() == SlowTier::Full {
        args.push("--ignore-default-filter".into());
    }
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
    command: &GuardCommand,
    name: &str,
    gate: &mut TestGate,
) -> Result<bool, String> {
    let base_worktree = ensure_base_worktree(repo, base, gate)?;
    // The packages come from the base's own manifests: a base cut before the
    // #1021 S4 crate split has no rsid-store package.
    let isolated =
        bounded_isolated_retry_command(command, name, ShardTree::Worktree(&base_worktree))?;
    let report = run_one_guard(&base_worktree, spec, &isolated).await?;
    let output = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
    if retry_ran_no_test(&output, name) {
        return Err(format!("local base isolated test did not run {name}"));
    }
    let failures = observed_test_failures(&report)?;
    if failures.contains(name) {
        if failures.len() != 1 {
            return Err(format!(
                "local base isolated test returned extra failures: {failures:?}"
            ));
        }
        gate.base_red_commands
            .entry(name.to_string())
            .or_insert_with(|| guard_command_line(&isolated));
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

/// Build commands for the test guards of a gate: the harness's own nextest
/// invocation with `--no-run`, so the later real run finds every binary built.
/// Every focused rsid lib filter shares one build (#1244), so it appears once.
fn shard_build_commands(spec: &GuardSpec, tree: ShardTree<'_>) -> Vec<GuardCommand> {
    let mut builds: Vec<GuardCommand> = Vec::new();
    for command in &spec.commands {
        let build = if shard_command_parts(command).is_some() {
            direct_base_shard_command(command, tree).map(|mut build| {
                // `cargo nextest run --no-run`: compile only.
                build.args.insert(2, "--no-run".into());
                build
            })
        } else if rsid_lib_filterset(command).is_some() {
            Some(rsid_lib_build_command())
        } else {
            None
        };
        if let Some(build) = build
            && !builds.contains(&build)
        {
            builds.push(build);
        }
    }
    builds.extend(package_test_build_commands(spec));
    builds
}

/// #2467: one compile-only build per non-rsid package that a plain
/// `cargo test -p PKG ...` guard runs (`rsi-common --lib`, `rsi --test ...`).
/// Such a package's tests pull dev-dependencies (`criterion`, `insta`, ...) that
/// switch on features of crates the shared nextest build compiled without, so
/// cargo compiles those crates again, serially, after the compile checks end
/// (about 35 s for `rsi-common`). Building them in the prebuild, which runs
/// beside the checks, takes that compile off the gate's critical path; the
/// later guard finds every artifact fresh. One command per package lists all
/// its targets, so they share one feature set. The `rsid`/`rsid-store` guards
/// are covered by the shared nextest build, whose bin test units are cheap
/// links of the already-built library.
fn package_test_build_commands(spec: &GuardSpec) -> Vec<GuardCommand> {
    let mut packages: Vec<(String, Vec<Vec<String>>)> = Vec::new();
    for command in &spec.commands {
        let Some(package) = cargo_test_package(command) else {
            continue;
        };
        if RSID_LIB_TEST_PACKAGES.contains(&package) {
            continue;
        }
        let mut targets: Vec<Vec<String>> = Vec::new();
        let mut args = command.args.iter().skip(3);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--" => break,
                "--lib" => targets.push(vec![arg.clone()]),
                "--bin" | "--test" => {
                    if let Some(name) = args.next() {
                        targets.push(vec![arg.clone(), name.clone()]);
                    }
                }
                // A positional test-name filter does not change what builds.
                _ => {}
            }
        }
        let known = match packages.iter().position(|(name, _)| name == package) {
            Some(at) => &mut packages[at].1,
            None => {
                packages.push((package.to_owned(), Vec::new()));
                &mut packages.last_mut().expect("just pushed").1
            }
        };
        for target in targets {
            if !known.contains(&target) {
                known.push(target);
            }
        }
    }
    packages
        .into_iter()
        .filter(|(_, targets)| !targets.is_empty())
        .map(|(package, targets)| {
            let mut args = vec!["test".to_owned(), "--no-run".into(), "-p".into(), package];
            args.extend(targets.into_iter().flatten());
            GuardCommand {
                program: "cargo".into(),
                args,
                timeout: guard_timeout(),
            }
        })
        .collect()
}

/// Whether the gate may compile the candidate's test binaries before the
/// gate loop runs them (`RSI_LANDER_PREBUILD=0` turns it off). Each build
/// still goes through the build-slot wrapper, so the memory and load limits
/// keep applying.
fn prebuild_enabled(gate: &TestGate) -> bool {
    // Unit tests with fake cargo shims count invocations, so they opt in.
    gate.prebuild_forced
        || (!cfg!(test)
            && lander_env::var("RSI_LANDER_PREBUILD").map_or(true, |value| value != "0"))
}

async fn prebuild_stream(
    worktree: PathBuf,
    spec: GuardSpec,
    builds: Vec<GuardCommand>,
) -> Result<bool, String> {
    for build in &builds {
        let report = run_one_guard(&worktree, &spec, build).await?;
        if report.status != rsid::integration::GuardStatus::Passed {
            // The gate loop reports the real failure; stop wasting compiles.
            return Ok(false);
        }
    }
    Ok(true)
}

/// Compile the candidate's shard and focused-filter test binaries ahead of
/// the gate loop (#1132), so `prebuild_secs` separates build from test time.
/// The base side is no longer prebuilt (#1244): it is only built when a
/// candidate failure needs a base comparison, which a green gate never does.
/// Best effort: a failed prebuild only means the gate loop compiles as before.
async fn prebuild_shard_binaries(candidate_worktree: &Path, spec: &GuardSpec, gate: &mut TestGate) {
    if !prebuild_enabled(gate) {
        return;
    }
    let candidate_builds = shard_build_commands(spec, ShardTree::Worktree(candidate_worktree));
    if candidate_builds.is_empty() {
        return;
    }
    let mut scratch_spec = spec.clone();
    if let Some(scratch) = &gate.scratch {
        scratch_spec.env.insert(
            "TMPDIR".into(),
            scratch.path().to_string_lossy().into_owned(),
        );
    }
    let started = Instant::now();
    match prebuild_stream(
        candidate_worktree.to_path_buf(),
        scratch_spec,
        candidate_builds,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => gate.prebuild_failed = true,
        Err(error) => {
            gate.prebuild_failed = true;
            eprintln!("prebuild skipped: {error}");
        }
    }
    gate.prebuild_secs += started.elapsed().as_secs();
}

/// The gate with its #1132 timing evidence: shards, base cache hits, base
/// runs, prebuild and wall seconds, printed as one `gate_timing=` line the
/// merge queue keeps in the batch outcome so the manager can read the ETA.
async fn run_affected_gate(
    repo: &Path,
    candidate_worktree: &Path,
    base: &str,
    spec: &GuardSpec,
    gate: &mut TestGate,
    disk_scratch_shards: &BTreeSet<String>,
) -> Result<(), String> {
    let started = Instant::now();
    // #1940: a stale re-gate runs only the commands its advance selects.
    let reduced = gate.reuse.as_ref().map(|reuse| {
        let (reduced, reused, rerun) = reuse_spec(spec, reuse);
        (reduced, reused, rerun, reuse.verdict.reason.clone())
    });
    gate.reuse_counts = reduced
        .as_ref()
        .map(|(_, reused, rerun, _)| (*reused, *rerun));
    // #1957: a narrowed focused run no longer names the explicit filters it
    // dropped; they keep their earlier result and cannot select nothing now.
    if let Some(observers) = gate.reuse.as_ref().and_then(|reuse| reuse.family.as_ref()) {
        gate.focused_filters.retain(|filter| {
            focused_atoms(&filter.filterset)
                .iter()
                .any(|atom| observers.observes_atom(atom))
        });
        let atoms = |spec: &GuardSpec| {
            spec.commands
                .iter()
                .filter_map(rsid_lib_filterset)
                .map(|filterset| focused_atoms(filterset).len())
                .sum::<usize>()
        };
        if let Some((narrowed, ..)) = &reduced {
            println!(
                "stale_family=closure {} focused_atoms={}/{}",
                observers.evidence(),
                atoms(narrowed),
                atoms(spec)
            );
        }
    }
    let spec = reduced.as_ref().map_or(spec, |(reduced, ..)| reduced);
    let result = run_affected_gate_inner(
        repo,
        candidate_worktree,
        base,
        spec,
        gate,
        disk_scratch_shards,
    )
    .await;
    let shards = spec
        .commands
        .iter()
        .filter(|command| is_test_guard(command))
        .count();
    let wall_secs = started.elapsed().as_secs();
    let stale = reduced
        .as_ref()
        .map(|(_, reused, rerun, reason)| {
            format!(" stale_reason={reason} reused={reused} rerun={rerun}")
        })
        .unwrap_or_default();
    if reduced.is_none() {
        gate.full_gate_secs = wall_secs;
    }
    // #3887: a gate that ran no test command for testable changes is a warning the
    // manager reads in the outcome; a stale re-gate reuses its earlier results.
    let zero_tests = if reduced.is_none() {
        gate.zero_test_scope.warning(shards)
    } else {
        None
    };
    if let Some(warning) = &zero_tests {
        println!("{warning}");
    }
    let stale = format!(
        "{stale}{}",
        zero_tests
            .map(|warning| format!(" {warning}"))
            .unwrap_or_default()
    );
    println!(
        "gate_timing=shards={shards} build_mode={} base_hits={} base_runs={} base_skipped={} prebuild_secs={} checks_secs={} release_check_secs={} candidate_secs={} base_secs={} wall_secs={wall_secs} test_threads={} thread_inputs={} check_steps={} {}{stale}",
        gate_build_mode(spec),
        gate.base_reused.len(),
        gate.base_runs,
        gate.base_skipped,
        gate.prebuild_secs,
        gate.checks_secs,
        gate.release_check_secs,
        gate.candidate_secs,
        gate.base_secs,
        gate_test_threads(),
        gate_thread_inputs_evidence(),
        check_steps_evidence(&gate.check_steps),
        gate.selection_evidence,
    );
    result
}

async fn run_affected_gate_inner(
    repo: &Path,
    candidate_worktree: &Path,
    base: &str,
    spec: &GuardSpec,
    gate: &mut TestGate,
    disk_scratch_shards: &BTreeSet<String>,
) -> Result<(), String> {
    // #2490/#2491: build and test at the slot's fixed checkout path, so Cargo's
    // external dep-info and generated OUT_DIR includes keep naming this tree.
    let gate_worktree = gate.bind_warm_targets(candidate_worktree);
    let candidate_worktree = gate_worktree.as_path();
    print_gate_environment_once(candidate_worktree, spec);
    // Cheap static guards (git diff --check) run before any remote or local
    // build or test time is spent (#1052).
    let static_started = Instant::now();
    for command in spec
        .commands
        .iter()
        .filter(|command| is_static_guard(command))
    {
        if *command == rsid_shard_inventory_check()
            && !candidate_worktree
                .join("scripts/check-rsid-test-shards.py")
                .is_file()
        {
            // A tree without the shard partition has no inventory to check.
            continue;
        }
        let report = run_one_guard(candidate_worktree, spec, command).await?;
        if report.status != rsid::integration::GuardStatus::Passed {
            return Err(format!(
                "static guard failed before any test ran ({:?}) running {} {:?}: {} {}",
                report.status, report.program, report.args, report.stdout_tail, report.stderr_tail
            ));
        }
    }
    gate.record_check_step("static-guards", static_started.elapsed().as_secs());
    {
        let python_checks = spec
            .commands
            .iter()
            .filter(|command| is_python_test_guard(command))
            .collect::<Vec<_>>();
        let checks_started = Instant::now();
        let result = async {
            for command in python_checks {
                let command_started = Instant::now();
                let report = run_one_guard(candidate_worktree, spec, command).await;
                gate.record_check_step(
                    &python_test_step_name(command),
                    command_started.elapsed().as_secs(),
                );
                let report = report?;
                println!(
                    "{}",
                    test_outcome_line(
                        "candidate",
                        &format!("python {:?}", command.args),
                        Ok(&report)
                    )
                );
                // #2583: a claimed test that ran green counts as proven even
                // when a separate test in the same suite is red (a base-shared
                // red that triage accepts), so read the passes from every run.
                if !gate.claimed_fixes.is_empty() {
                    let output = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
                    for id in python_pass_ids(&output) {
                        for claimed in gate.claimed_fixes.clone() {
                            if python_claim_matches(&claimed, &id) {
                                gate.claimed_green.insert(claimed);
                            }
                        }
                    }
                }
                if report.status != rsid::integration::GuardStatus::Passed {
                    triage_python_failure(
                        repo,
                        candidate_worktree,
                        base,
                        spec,
                        command,
                        &report,
                        gate,
                    )
                    .await?;
                }
            }
            Ok::<(), String>(())
        }
        .await;
        gate.checks_secs += checks_started.elapsed().as_secs();
        gate.record_check_step("python-tests", checks_started.elapsed().as_secs());
        result?;
        prefetch_remote_shards(candidate_worktree, base, spec, gate).await?;
    }
    // #1244: the compile checks build into their own target (a different
    // profile shares no artifacts with the test builds), so they run while the
    // prebuild compiles the test binaries instead of after it.
    let checks_with_prebuild = gate.remote.is_none() && prebuild_enabled(gate);
    if gate.remote.is_none() {
        let checks: Vec<&GuardCommand> = if checks_with_prebuild {
            spec.commands
                .iter()
                .filter(|command| {
                    !is_static_guard(command)
                        && !is_test_guard(command)
                        && !is_python_test_guard(command)
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut checks_spec = spec.clone();
        if let Some(scratch) = &gate.scratch {
            checks_spec.env.insert(
                "TMPDIR".into(),
                scratch.path().to_string_lossy().into_owned(),
            );
        }
        let checks_started = Instant::now();
        let mut timings = CheckTimings::default();
        let prebuild_before = gate.prebuild_secs;
        gate.prebuild_failed = false;
        let (checks_result, ()) = tokio::join!(
            run_compile_checks(candidate_worktree, &checks_spec, &checks, &mut timings),
            prebuild_shard_binaries(candidate_worktree, spec, gate),
        );
        // #2467: the build phase is the whole build of this tree only when the
        // checks ran beside the prebuild and neither failed.
        if checks_with_prebuild && checks_result.is_ok() && !gate.prebuild_failed {
            gate.commit_warm_targets();
        }
        gate.checks_secs += checks_started.elapsed().as_secs();
        gate.release_check_secs += timings.release_secs;
        gate.check_steps.extend(timings.steps);
        // The prebuild ran beside the compile checks, so its step is the
        // time the phase could not go below (#2424).
        gate.record_check_step(
            "prebuild-beside",
            gate.prebuild_secs.saturating_sub(prebuild_before),
        );
        checks_result?;
    }
    // #1261/#1262: a focused filter that selects no test is refused once the
    // shared build exists and before any test runs, never as a test failure.
    let mut resolved_spec = spec.clone();
    if let Some(command) = spec
        .commands
        .iter()
        .find(|command| rsid_lib_filterset(command).is_some())
    {
        let (replacements, omit_shared) =
            refuse_focused_filters_selecting_nothing(candidate_worktree, spec, command, gate)
                .await?;
        if omit_shared {
            resolved_spec
                .commands
                .retain(|command| rsid_lib_filterset(command).is_none());
        }
        for command in replacements {
            if !resolved_spec.commands.contains(&command) {
                resolved_spec.commands.push(command);
            }
        }
    }
    let spec = &resolved_spec;
    refuse_lib_filters_selecting_nothing(candidate_worktree, spec, gate).await?;
    // A compile-repair gate requires every selected candidate run to pass,
    // including runs whose individual baselines were runnable. Remember an
    // earlier red/empty run in case a later command discovers an unavailable
    // baseline; normal runnable-baseline comparison remains unchanged.
    let mut compilation_unavailable = false;
    let mut repair_candidate_refusal = None;
    // Green, proven candidate runs whose base was not consulted (#1244).
    let mut base_skipped: Vec<(&GuardCommand, GuardSpec)> = Vec::new();
    for command in spec
        .commands
        .iter()
        .filter(|command| !is_static_guard(command) && !is_python_test_guard(command))
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
        if !is_test {
            if !checks_with_prebuild {
                let mut timings = CheckTimings::default();
                let result =
                    run_compile_checks(candidate_worktree, &scratch_spec, &[command], &mut timings)
                        .await;
                gate.release_check_secs += timings.release_secs;
                gate.check_steps.extend(timings.steps);
                result?;
            }
            continue;
        }
        let key = (base.to_owned(), base_cache_command_key(command));
        // Hub #3483: a complete listing, rather than the timeout's partial
        // output or a substring filter, is the authority for this exemption.
        // Only an e2e target needs it before the candidate runs (to cap its
        // timeout). Every other guard lists after a timeout, so a candidate
        // that finishes pays no extra cargo invocation.
        let mut selected = if runs_e2e_target(command) {
            list_selected_tests(candidate_worktree, &scratch_spec, command, gate).await
        } else {
            None
        };
        let mut early_base = None;
        let baseline_unavailable_before = gate.base_test_baseline_unavailable.clone();
        if runs_e2e_target(command)
            && known_red_timeout::all_known(selected.as_ref(), gate.known_failures.as_ref())
        {
            // Establish the base proof before shortening an e2e run. A live
            // signature by itself never excuses a candidate timeout.
            let started = Instant::now();
            early_base = Some(
                base_evidence_for(
                    repo,
                    candidate_worktree,
                    base,
                    &scratch_spec,
                    command,
                    gate,
                    disk_scratch_shards,
                    &key,
                    selected.as_ref(),
                )
                .await,
            );
            gate.base_secs += started.elapsed().as_secs();
        }
        let timeout_reds = known_red_timeout::proven_reds(
            selected.as_ref(),
            early_base.as_ref().and_then(|result| result.as_ref().ok()),
            gate.known_failures.as_ref(),
            &gate.base_red_texts,
            &gate.claimed_fixes,
        );
        let candidate_command =
            known_red_timeout::candidate_command(command, timeout_reds.is_some());
        if runs_e2e_target(command) && timeout_reds.is_some() {
            println!(
                "known_red_e2e_timeout_secs={}",
                candidate_command.timeout.as_secs()
            );
        }
        // #1244: ordinarily the candidate runs first; only an all-known-red
        // e2e inventory establishes its base proof early (hub #3483).
        let candidate_started = Instant::now();
        let report = run_shard_or_local(
            candidate_worktree,
            candidate_worktree,
            base,
            &scratch_spec,
            &candidate_command,
            gate,
        )
        .await?;
        gate.candidate_secs += candidate_started.elapsed().as_secs();
        let shard_name = full_shard(command)
            .map(|(shard, _)| shard.to_owned())
            .unwrap_or_else(|| format!("{} {:?}", command.program, command.args));
        println!(
            "{}",
            test_outcome_line("candidate", &shard_name, Ok(&report))
        );
        // Non-e2e guards have no timeout cap to establish in advance. Consult
        // their base only if the candidate actually needs timeout attribution.
        if report.status == rsid::integration::GuardStatus::TimedOut && !runs_e2e_target(command) {
            selected = list_selected_tests(candidate_worktree, &scratch_spec, command, gate).await;
        }
        if report.status == rsid::integration::GuardStatus::TimedOut
            && early_base.is_none()
            && known_red_timeout::all_known(selected.as_ref(), gate.known_failures.as_ref())
        {
            let started = Instant::now();
            early_base = Some(
                base_evidence_for(
                    repo,
                    candidate_worktree,
                    base,
                    &scratch_spec,
                    command,
                    gate,
                    disk_scratch_shards,
                    &key,
                    selected.as_ref(),
                )
                .await,
            );
            gate.base_secs += started.elapsed().as_secs();
        }
        let timeout_reds = known_red_timeout::proven_reds(
            selected.as_ref(),
            early_base.as_ref().and_then(|result| result.as_ref().ok()),
            gate.known_failures.as_ref(),
            &gate.base_red_texts,
            &gate.claimed_fixes,
        );
        let candidate_failures = if report.status == rsid::integration::GuardStatus::TimedOut
            && let Some(reds) = timeout_reds
        {
            println!(
                "known_red_timeout names={}",
                reds.iter().cloned().collect::<Vec<_>>().join(",")
            );
            Ok(reds)
        } else {
            observed_test_failures(&report)
        };
        // Name the failures now, before the base side starts: the base run can
        // take as long as the candidate's and the log should not wait for it.
        match &candidate_failures {
            Ok(failures) if !failures.is_empty() => println!(
                "candidate_failures shard={shard_name} count={} names={}",
                failures.len(),
                failures.iter().cloned().collect::<Vec<_>>().join(",")
            ),
            Err(error) => println!(
                "candidate_failures_unparsed shard={shard_name} detail={}",
                error.lines().next().unwrap_or_default()
            ),
            Ok(_) => {}
        }
        // #2167: a claimed fix must run green on the candidate. A base that is
        // red on the same test excuses nothing, so the base is not consulted.
        if !gate.claimed_fixes.is_empty() {
            let green: Vec<String> =
                test_pass_names(&format!("{}\n{}", report.stdout_tail, report.stderr_tail))
                    .into_iter()
                    .filter(|name| gate.claimed_fixes.contains(name))
                    .collect();
            gate.claimed_green.extend(green);
            if let Ok(failures) = &candidate_failures {
                let still_red: Vec<String> = failures
                    .intersection(&gate.claimed_fixes)
                    .cloned()
                    .collect();
                if !still_red.is_empty() {
                    println!(
                        "{}",
                        rsid::rolling_queue::gate_failure_excerpt_line(
                            &still_red.join(", "),
                            &report.stdout_tail,
                            &report.stderr_tail,
                        )
                    );
                    return Err(claimed_fix_still_red_message(
                        &still_red,
                        &format!("the claimed fix still fails on the candidate ({shard_name})"),
                    ));
                }
            }
        }
        if report.status == rsid::integration::GuardStatus::Passed
            && matches!(&candidate_failures, Ok(failures) if failures.is_empty())
            && report_proves_tests_ran(&report)
        {
            // An optional early probe cannot make a positively green
            // candidate's receipt unusable: its verdict never used the base.
            gate.base_test_baseline_unavailable = baseline_unavailable_before;
            gate.base_skipped += 1;
            base_skipped.push((command, scratch_spec));
            continue;
        }
        let base_started = Instant::now();
        let base_evidence = if let Some(evidence) = early_base {
            evidence?
        } else {
            base_evidence_for(
                repo,
                candidate_worktree,
                base,
                &scratch_spec,
                command,
                gate,
                disk_scratch_shards,
                &key,
                candidate_failures.as_ref().ok(),
            )
            .await?
        };
        gate.base_secs += base_started.elapsed().as_secs();
        if let BaseTestEvidence::Observed(failures) = &base_evidence {
            gate.base_reds.extend(failures.iter().cloned());
            for name in failures {
                gate.base_red_commands
                    .entry(name.clone())
                    .or_insert_with(|| guard_command_line(command));
            }
        }
        compilation_unavailable |=
            matches!(&base_evidence, BaseTestEvidence::CompilationUnavailable);
        if repair_candidate_refusal.is_none() {
            if report.status != rsid::integration::GuardStatus::Passed
                || !matches!(&candidate_failures, Ok(failures) if failures.is_empty())
            {
                repair_candidate_refusal = Some(format!(
                    "baseline_compile_repair_candidate_failed: base {base} test baseline unavailable; selected candidate command {} {:?}: {candidate_failures:?}",
                    command.program, command.args,
                ));
            } else if !report_proves_tests_ran(&report) {
                repair_candidate_refusal = Some(format!(
                    "baseline_compile_repair_candidate_evidence_missing: base {base} test baseline unavailable; selected candidate command {} {:?} did not prove a positive run",
                    command.program, command.args,
                ));
            }
        }
        if compilation_unavailable {
            if let Some(error) = repair_candidate_refusal {
                return Err(error);
            }
        }
        let base_failures = match base_evidence {
            BaseTestEvidence::CompilationUnavailable => {
                // No baseline comparison or isolated-retry exemptions can
                // establish safety here: the entire selected run must pass.
                continue;
            }
            BaseTestEvidence::Observed(failures) => failures,
        };
        let candidate_failures = candidate_failures?;
        let new_failures: BTreeSet<_> = candidate_failures
            .difference(&base_failures)
            .cloned()
            .collect();
        if !new_failures.is_empty() {
            println!("candidate_only_failures={new_failures:?}");
        }
        // #1261: a combined focused run names, per failing test, the filters
        // that select it.
        let run_filters = rsid_lib_filterset(command)
            .map(|filterset| focused_filters_of_run(filterset, &gate.focused_filters))
            .or_else(|| cargo_lib_filter_labels(command))
            .unwrap_or_default();
        for name in &new_failures {
            let labels = focused_filters_selecting(&run_filters, name);
            if !labels.is_empty() {
                println!("failure_filters test={name} filters={}", labels.join(" | "));
            }
        }
        let mut changed_files: Option<Option<Vec<String>>> = None;
        for name in &new_failures {
            let mut retry =
                run_isolated_retry(candidate_worktree, &scratch_spec, command, name).await?;
            let ran_no_test = |retry: &rsid::integration::GuardCommandReport| {
                retry_ran_no_test(
                    &format!("{}\n{}", retry.stdout_tail, retry.stderr_tail),
                    name,
                )
            };
            if ran_no_test(&retry) {
                // One more attempt before the failure is called unverifiable.
                retry =
                    run_isolated_retry(candidate_worktree, &scratch_spec, command, name).await?;
            }
            if ran_no_test(&retry) {
                // The gate could not verify this failure. It refuses only when
                // the source can have caused it; a failure in a module the
                // source does not touch is reported, not charged (#1167).
                if changed_files.is_none() {
                    changed_files = Some(
                        git_text(candidate_worktree, &["diff", "--name-only", base, "HEAD"])
                            .ok()
                            .map(|text| text.lines().map(str::to_owned).collect()),
                    );
                }
                let changed = changed_files.as_ref().and_then(|files| files.as_deref());
                if source_touches_test_module(changed, name) {
                    return Err(format!(
                        "unverified candidate failure (isolated retry did not run twice, module touched by the source): {name}{}",
                        focused_attribution_suffix(&run_filters, name)
                    ));
                }
                gate.unverified.insert(name.clone());
                continue;
            }
            let retry_failures = observed_test_failures(&retry)?;
            let persistent: BTreeSet<_> =
                retry_failures.difference(&base_failures).cloned().collect();
            if !persistent.is_empty() {
                let excerpt = rsid::rolling_queue::gate_failure_excerpt_line(
                    name,
                    &retry.stdout_tail,
                    &retry.stderr_tail,
                );
                if let Some(provenance) = gate.qa_cache_provenance.get(&key).cloned() {
                    if persistent.len() != 1 || !persistent.contains(name) {
                        println!("{excerpt}");
                        return Err(format!(
                            "QA cached base {provenance} has unexpected isolated failures: {persistent:?}"
                        ));
                    }
                    match confirm_local_base_failure(repo, base, &scratch_spec, command, name, gate)
                        .await
                    {
                        Ok(true) => {
                            gate.base_reds.insert(name.clone());
                            gate.local_base_confirmed
                                .insert(format!("{base}:{name}:red:{provenance}"));
                            continue;
                        }
                        Ok(false) => {
                            println!("{excerpt}");
                            return Err(new_failures_message(
                                base,
                                std::slice::from_ref(name),
                                &format!(
                                    "QA cached base {provenance}; isolated local base passed {name}"
                                ),
                            ));
                        }
                        Err(error) => {
                            println!("{excerpt}");
                            return Err(format!(
                                "QA cached base {provenance}; local base confirmation failed closed for {name}: {error}"
                            ));
                        }
                    }
                }
                println!("{excerpt}");
                let retry_output = format!("{}\n{}", retry.stdout_tail, retry.stderr_tail);
                let failing = persistent
                    .iter()
                    .map(|name| {
                        format!(
                            "{name}{}{}",
                            known_red_suffix(
                                name,
                                &failure_text_for(&retry_output, name),
                                gate.known_failures.as_ref(),
                            ),
                            focused_attribution_suffix(&run_filters, name)
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
    // A compile repair (#1210) still needs every selected run green: when a
    // candidate run was red or unproven, a skipped green run's base must not
    // be one that cannot compile. Probe compilation only: rerunning these
    // green commands on base can double a wide gate for an unrelated red or
    // zero-test target (#3839), and their test results change no comparison.
    if let Some(error) = repair_candidate_refusal
        && !compilation_unavailable
    {
        for (command, scratch_spec) in &base_skipped {
            let build = base_compile_probe(
                command,
                ShardTree::Revision {
                    repo,
                    revision: base,
                },
            )
            .ok_or("base_compile_probe_unavailable: unrecognised test guard")?;
            let base_started = Instant::now();
            println!("base_compile_probe={} {:?}", build.program, build.args);
            let (evidence, _) = run_base_guard(
                repo,
                base,
                scratch_spec,
                &build,
                command,
                gate,
                candidate_worktree,
            )
            .await?;
            gate.base_secs += base_started.elapsed().as_secs();
            if matches!(evidence, BaseTestEvidence::CompilationUnavailable) {
                return Err(error);
            }
        }
    }
    // #2167: every claimed test must have run green on the candidate; one the
    // gate's selection never ran is not proven fixed.
    let unproven: Vec<String> = gate
        .claimed_fixes
        .difference(&gate.claimed_green)
        .cloned()
        .collect();
    if !unproven.is_empty() {
        return Err(claimed_fix_still_red_message(
            &unproven,
            &unproven_claim_next_step(&unproven),
        ));
    }
    if !gate.claimed_fixes.is_empty() {
        println!(
            "claimed_fix_green={}",
            gate.claimed_fixes
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    Ok(())
}

/// Check whether a green candidate's test target compiles on base without
/// running its tests. Keep target/features and discard libtest-only arguments.
/// These probes establish compile-repair policy, never cached test outcomes.
fn base_compile_probe(command: &GuardCommand, tree: ShardTree<'_>) -> Option<GuardCommand> {
    let mut build = if shard_command_parts(command).is_some() {
        direct_base_shard_command(command, tree)?
    } else if cargo_test_package(command).is_some() || rsid_lib_filterset(command).is_some() {
        command.clone()
    } else {
        return None;
    };
    if let Some(separator) = build.args.iter().position(|arg| arg == "--") {
        build.args.truncate(separator);
    }
    build.args.push("--no-run".into());
    Some(build)
}

/// Wall seconds of the compile checks (#2424): the release-build seconds
/// (#1970) and one entry per cargo invocation, in execution order.
#[derive(Default)]
struct CheckTimings {
    release_secs: u64,
    steps: Vec<(String, u64)>,
}

/// One cargo invocation of the compile-check phase (#2424).
#[derive(Debug, PartialEq, Eq)]
struct CompileBatch {
    /// Space-free step name for `gate_step` and `check_steps`.
    name: String,
    command: GuardCommand,
    /// The packages of a merged dev-profile check or release-build check.
    packages: Vec<String>,
    release: bool,
}

/// The package of a [`cargo_check_command`] (`cargo check -p P --all-targets`).
fn dev_check_package(command: &GuardCommand) -> Option<&str> {
    match command.args.as_slice() {
        [check, flag, package, all]
            if command.program == "cargo"
                && check == "check"
                && flag == "-p"
                && all == "--all-targets" =>
        {
            Some(package.as_str())
        }
        _ => None,
    }
}

/// #2424: the gate ran one `cargo check` per package, each ending in the serial
/// tail of its largest crate while the other cores idled, and compiled shared
/// dependencies again under each invocation's own feature set. The dev-profile
/// checks (`-p P --all-targets`) become one invocation naming every package,
/// and so do the release-build checks (`-p P`): cargo schedules independent
/// crates together and builds a shared dependency once. The two kinds stay
/// apart (they need different feature sets, the point of #1970), every other
/// command keeps its place, and a single check stays byte-identical.
fn coalesce_compile_checks(checks: &[&GuardCommand]) -> Vec<CompileBatch> {
    let mut batches: Vec<CompileBatch> = Vec::new();
    let mut dev_at: Option<usize> = None;
    let mut release_at: Option<usize> = None;
    for command in checks {
        let (slot, package, release) = if let Some(package) = dev_check_package(command) {
            (&mut dev_at, package, false)
        } else if let Some(package) = release_check_package(command) {
            (&mut release_at, package, true)
        } else {
            batches.push(CompileBatch {
                name: format!(
                    "cargo-{}",
                    command.args.first().map_or("command", String::as_str)
                ),
                command: (*command).clone(),
                packages: Vec::new(),
                release: false,
            });
            continue;
        };
        match *slot {
            Some(at) => {
                let batch = &mut batches[at];
                if !batch.packages.iter().any(|name| name == package) {
                    batch.packages.push(package.to_owned());
                }
                batch.command.timeout = batch.command.timeout.max(command.timeout);
            }
            None => {
                *slot = Some(batches.len());
                batches.push(CompileBatch {
                    name: String::new(),
                    command: (*command).clone(),
                    packages: vec![package.to_owned()],
                    release,
                });
            }
        }
    }
    for batch in &mut batches {
        if batch.packages.is_empty() {
            continue;
        }
        let mut args = vec!["check".to_owned()];
        for package in &batch.packages {
            args.push("-p".into());
            args.push(package.clone());
        }
        if !batch.release {
            args.push("--all-targets".into());
        }
        batch.command.args = args;
        batch.name = format!(
            "{}:{}",
            if batch.release {
                "release-check"
            } else {
                "check"
            },
            batch.packages.join("+")
        );
    }
    batches
}

/// The crate a failed merged release-build check names: cargo reports
/// ``error: could not compile `X` ``; otherwise the first package of the batch.
fn failed_release_package<'a>(packages: &'a [String], report_text: &str) -> &'a str {
    let named = report_text
        .split("could not compile `")
        .skip(1)
        .filter_map(|rest| rest.split('`').next())
        .find_map(|name| packages.iter().find(|package| package.as_str() == name));
    named
        .or_else(|| packages.first())
        .map_or("", String::as_str)
}

/// `name@secs,...` of the checks phase for the `gate_timing` line (#2424).
fn check_steps_evidence(steps: &[(String, u64)]) -> String {
    if steps.is_empty() {
        return "none".into();
    }
    steps
        .iter()
        .map(|(name, secs)| format!("{name}@{secs}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Run the gate's non-test guards (compile checks) in order; the first that
/// does not pass refuses the gate. Same-kind cargo checks run as one
/// invocation (#2424), and every invocation's wall seconds are kept in
/// `timings.steps`; the release-build checks' seconds (#1970) also go to
/// `timings.release_secs`, also when one fails.
async fn run_compile_checks(
    candidate_worktree: &Path,
    spec: &GuardSpec,
    checks: &[&GuardCommand],
    timings: &mut CheckTimings,
) -> Result<(), String> {
    for batch in coalesce_compile_checks(checks) {
        let started = Instant::now();
        let report = run_one_guard(candidate_worktree, spec, &batch.command).await;
        let secs = started.elapsed().as_secs();
        if batch.release {
            timings.release_secs += secs;
        }
        println!("gate_step name={} secs={secs}", batch.name);
        timings.steps.push((batch.name.clone(), secs));
        let report = report?;
        if report.status != rsid::integration::GuardStatus::Passed {
            if batch.release {
                let text = format!("{}\n{}", report.stdout_tail, report.stderr_tail);
                // #2445: cargo's output need not name the crate that broke the
                // merged build. Failure path only: rerun the release check per
                // package and blame the first that fails standalone.
                if batch.packages.len() > 1 {
                    for package in &batch.packages {
                        let single = cargo_release_check_command(package);
                        let started = Instant::now();
                        let rerun = run_one_guard(candidate_worktree, spec, &single).await;
                        let secs = started.elapsed().as_secs();
                        timings.release_secs += secs;
                        println!("gate_step name=release-check-rerun:{package} secs={secs}");
                        timings
                            .steps
                            .push((format!("release-check-rerun:{package}"), secs));
                        if let Ok(rerun) = rerun {
                            if rerun.status != rsid::integration::GuardStatus::Passed {
                                return Err(release_build_failure(package, &rerun));
                            }
                        }
                    }
                }
                // No package fails alone (a feature-unification effect of the
                // merged build): fall back to the crate cargo names.
                let package = failed_release_package(&batch.packages, &text);
                return Err(release_build_failure(package, &report));
            }
            return Err(format!(
                "affected-crate guard failed ({:?}) running {} {:?}: {} {}",
                report.status, report.program, report.args, report.stdout_tail, report.stderr_tail
            ));
        }
    }
    Ok(())
}

/// The refusal of a failed release-build check (#1970). The leading
/// `release_build_failed:<crate>` token is the typed code the merge queue maps
/// to a refusal (`rolling_queue::release_build_failed_crate`).
fn release_build_failure(package: &str, report: &rsid::integration::GuardCommandReport) -> String {
    format!(
        "{RELEASE_BUILD_FAILED}:{package}: the non-test build of crate {package} failed ({:?}) running {} {:?}; \
         dev-dependencies enable features (test-seam) that this build does not: {} {}",
        report.status, report.program, report.args, report.stdout_tail, report.stderr_tail
    )
}

/// Most failing tests a narrowed base filterset names; beyond it the full
/// command is the cheaper and safer base side.
const MAX_NARROWED_BASE_TESTS: usize = 200;

/// #1525: the base side of a red candidate run, restricted to exactly the
/// tests that failed on the candidate (`test(=A) | test(=B) ...`). Whatever
/// else fails on the base cannot change `candidate - base`, so running the
/// whole command there only costs wall time. `None` (run the full command)
/// when no failure is known, a name is not a plain test name, the list is
/// large, or the command is not a nextest filterset or shard run.
fn narrowed_base_command(
    command: &GuardCommand,
    failures: &BTreeSet<String>,
) -> Option<GuardCommand> {
    if failures.is_empty()
        || failures.len() > MAX_NARROWED_BASE_TESTS
        || !failures.iter().all(|name| plain_test_name(name))
    {
        return None;
    }
    let filterset = failures
        .iter()
        .map(|name| format!("test(={name})"))
        .collect::<Vec<_>>()
        .join(" | ");
    let mut narrowed = command.clone();
    if rsid_lib_filterset(command).is_some() {
        *narrowed.args.last_mut()? = filterset;
        return Some(narrowed);
    }
    let (shard, jobs, _) = shard_command_parts(command)?;
    narrowed.args = vec![
        "shard".into(),
        shard.into(),
        "--jobs".into(),
        jobs.to_string(),
        "--filterset".into(),
        filterset,
    ];
    Some(narrowed)
}

/// A cached empty baseline must keep saying why it is empty: report the absent
/// targets behind it the way the real base run did, tagged with the cache tier.
fn note_cached_absent_targets(
    gate: &mut TestGate,
    base: &str,
    absent: &BTreeSet<String>,
    source: &str,
) {
    for target in absent {
        let entry = format!("{base}:{target}");
        println!("base_target_absent={entry} cache={source}");
        gate.base_absent_targets.insert(entry);
    }
}

/// The key of a command's per-test base outcomes (#2059): the command shape
/// with its filterset removed, so every narrowed run of one command shares an
/// outcome file, folded with the environment class the tests ran under.
fn per_test_outcome_class(command: &GuardCommand, env_class: &str) -> String {
    let shape = match (rsid_lib_filterset(command), shard_command_parts(command)) {
        (Some(_), _) => {
            let mut args = command.args.clone();
            args.pop();
            format!("lib {args:?}")
        }
        (None, Some((shard, jobs, _))) => format!("shard {shard} --jobs {jobs}"),
        (None, None) => format!("{} {:?}", command.program, command.args),
    };
    sha256_digest_of(&format!("per-test\n{shape}\nenv={env_class}"))
}

/// #2059: the base side of a red candidate run, one answer per failing test.
/// A test the rolling tip's outcome file already holds costs nothing (counted
/// in `base_hits`); only the unknown names are run, by exact name, and their
/// outcomes are recorded for the next gate on the same tip. A narrowed result
/// is not the command's full base result, so it never fills the full caches.
#[allow(clippy::too_many_arguments)]
async fn run_narrowed_base(
    repo: &Path,
    base: &str,
    scratch_spec: &GuardSpec,
    command: &GuardCommand,
    narrowed: &GuardCommand,
    failures: &BTreeSet<String>,
    class: &str,
    gate: &mut TestGate,
    fingerprint_source: &Path,
) -> Result<BaseTestEvidence, String> {
    let cache_root = if gate.remote.is_some() {
        None
    } else {
        gate.cache_root.clone()
    };
    let known = cache_root
        .as_deref()
        .map(|root| base_cache::load_test_outcomes(root, base, class))
        .unwrap_or_default();
    let mut reds = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    for name in failures {
        match known.get(name) {
            Some(outcome) => {
                if outcome.red {
                    reds.insert(name.clone());
                    if let Some(text) = &outcome.failure_text {
                        gate.base_red_texts
                            .entry(name.clone())
                            .or_insert_with(|| text.clone());
                    }
                }
                gate.base_reused.insert(format!("{base}:{name}:per-test"));
            }
            None => {
                unknown.insert(name.clone());
            }
        }
    }
    if unknown.is_empty() {
        println!(
            "base_per_test_cache tests={} base_reds={}",
            failures.len(),
            reds.len()
        );
        return Ok(BaseTestEvidence::Observed(reds));
    }
    let (to_run, ran) = if unknown.len() == failures.len() {
        (narrowed.clone(), failures)
    } else {
        match narrowed_base_command(command, &unknown) {
            Some(smaller) => {
                println!(
                    "base_narrowed_further tests={} cached={}",
                    unknown.len(),
                    failures.len() - unknown.len()
                );
                (smaller, &unknown)
            }
            None => (narrowed.clone(), failures),
        }
    };
    gate.base_runs += 1;
    let (evidence, failure_texts) = run_base_guard(
        repo,
        base,
        scratch_spec,
        &to_run,
        command,
        gate,
        fingerprint_source,
    )
    .await?;
    let BaseTestEvidence::Observed(observed) = evidence else {
        return Ok(evidence);
    };
    if let Some(root) = cache_root.as_deref() {
        let green: BTreeSet<String> = ran.difference(&observed).cloned().collect();
        let red: BTreeSet<String> = observed.intersection(ran).cloned().collect();
        if let Err(error) =
            base_cache::record_test_outcomes(root, base, class, &red, &green, &failure_texts)
        {
            // The cache only saves time; a failed write never fails a gate.
            println!("base_per_test_cache_unrecorded detail={error}");
        }
    }
    reds.extend(observed);
    Ok(BaseTestEvidence::Observed(reds))
}

/// The base side of one test guard (#1244: only asked for when the candidate
/// failed or did not prove a positive run): the in-process cache, then the
/// persistent base cache, then a real base run whose result fills both.
#[allow(clippy::too_many_arguments)]
async fn base_evidence_for(
    repo: &Path,
    candidate_worktree: &Path,
    base: &str,
    scratch_spec: &GuardSpec,
    command: &GuardCommand,
    gate: &mut TestGate,
    disk_scratch_shards: &BTreeSet<String>,
    key: &(String, String),
    candidate_failures: Option<&BTreeSet<String>>,
) -> Result<BaseTestEvidence, String> {
    // #1525: the base side only decides which of the candidate's failures are
    // pre-existing, so a known failure list narrows it to exactly those tests.
    // A narrowed result is not the command's full base result, so it never
    // fills a cache. A remote full shard keeps its prefetched full run.
    let narrowed = candidate_failures
        .filter(|_| !(gate.remote.is_some() && full_shard(command).is_some()))
        .and_then(|failures| narrowed_base_command(command, failures));
    if let Some(narrowed) = &narrowed {
        println!(
            "base_narrowed tests={} command={} {:?}",
            candidate_failures.map_or(0, BTreeSet::len),
            narrowed.program,
            narrowed.args
        );
    }
    let per_test_class = per_test_outcome_class(
        command,
        &shard_env_class(
            &scratch_spec.env,
            shard_tmpdir_class(gate.scratch.as_ref(), command, disk_scratch_shards),
        ),
    );
    let absent_mark = gate.base_absent_log.len();
    let mut absent: BTreeSet<String> = BTreeSet::new();
    Ok(if let Some(cached) = gate.base_cache.get(key) {
        if let Some((shard, _, filterset)) = shard_command_parts(command) {
            let display = match filterset {
                Some(filterset) => format!("{shard}[{filterset}]"),
                None => shard.to_owned(),
            };
            gate.base_reused
                .insert(format!("{base}:{display}:in-process"));
        }
        let cached = cached.clone();
        if let Some(reasons) = gate.base_cache_absent.get(key).cloned() {
            note_cached_absent_targets(gate, base, &reasons, "in-process");
        }
        BaseTestEvidence::Observed(cached)
    } else {
        let cache_root = if gate.remote.is_some() {
            None
        } else {
            gate.cache_root.clone()
        };
        let identity = match cache_root.as_deref() {
            Some(_) => {
                let env_class = shard_env_class(
                    &scratch_spec.env,
                    shard_tmpdir_class(gate.scratch.as_ref(), command, disk_scratch_shards),
                );
                base_cache_identity(candidate_worktree, base, command, &env_class)?
            }
            None => None,
        };
        let failures = if let (Some(root), Some(identity)) = (cache_root.as_deref(), identity) {
            // The holder runs one base guard (bounded by guard_timeout), so a wait
            // beyond twice that means the holder is wedged: refuse, do not hang.
            let slot = base_cache::BaseShardSlot::acquire_within(
                root,
                base,
                &identity.slot_label,
                &identity.slot_key,
                guard_timeout() * 2,
            )
            .await?;
            if let Some(entry) = slot.read()? {
                gate.base_reused
                    .insert(format!("{base}:{}:{}", identity.display, entry.provenance));
                if entry.provenance.starts_with("qa:") {
                    gate.qa_cache_provenance
                        .insert(key.clone(), entry.provenance.clone());
                }
                note_cached_absent_targets(gate, base, &entry.absent_targets, &entry.provenance);
                for (name, text) in entry.failure_texts {
                    if entry.failures.contains(&name) {
                        gate.base_red_texts.entry(name).or_insert(text);
                    }
                }
                absent = entry.absent_targets;
                BaseTestEvidence::Observed(entry.failures)
            } else {
                // The slot only serialises full-result fills; a narrowed run
                // writes nothing, so it must not hold other landers behind it.
                if let (Some(narrowed), Some(failures)) = (&narrowed, candidate_failures) {
                    drop(slot);
                    return run_narrowed_base(
                        repo,
                        base,
                        scratch_spec,
                        command,
                        narrowed,
                        failures,
                        &per_test_class,
                        gate,
                        candidate_worktree,
                    )
                    .await;
                }
                gate.base_runs += 1;
                let (failures, failure_texts) = run_base_guard(
                    repo,
                    base,
                    scratch_spec,
                    command,
                    command,
                    gate,
                    candidate_worktree,
                )
                .await?;
                if let BaseTestEvidence::Observed(observed) = &failures {
                    absent = gate.base_absent_log[absent_mark..]
                        .iter()
                        .cloned()
                        .collect();
                    slot.write(
                        &base_cache::BaseShardEntry::new(
                            base,
                            &identity.slot_label,
                            &identity.slot_key,
                            observed.clone(),
                            "lander",
                        )
                        .with_absent_targets(absent.clone())
                        .with_failure_texts(&failure_texts),
                    )?;
                }
                failures
            }
        } else if let (Some(narrowed), Some(failures)) = (&narrowed, candidate_failures) {
            return run_narrowed_base(
                repo,
                base,
                scratch_spec,
                command,
                narrowed,
                failures,
                &per_test_class,
                gate,
                candidate_worktree,
            )
            .await;
        } else {
            gate.base_runs += 1;
            run_base_guard(
                repo,
                base,
                scratch_spec,
                command,
                command,
                gate,
                candidate_worktree,
            )
            .await?
            .0
        };
        if narrowed.is_none()
            && let BaseTestEvidence::Observed(observed) = &failures
        {
            if absent.is_empty() {
                absent = gate.base_absent_log[absent_mark..]
                    .iter()
                    .cloned()
                    .collect();
            }
            if !absent.is_empty() {
                gate.base_cache_absent.insert(key.clone(), absent);
            }
            gate.base_cache.insert(key.clone(), observed.clone());
        }
        failures
    })
}

/// A guard that only inspects git objects or source text: it needs no build
/// and no test host, so it runs before any of them.
fn is_static_guard(command: &GuardCommand) -> bool {
    command.program == "git" || *command == rsid_shard_inventory_check()
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
    rsid_unfiltered: Option<&test_impact::UnfilteredSelection>,
    other_targets: &BTreeMap<String, test_impact::PackageTargets>,
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
        for filter in PROVISIONAL_STORE_TESTS {
            commands.push(rsid_family_filter_command(
                "rsid",
                &format!("shard:store-01:test({filter})"),
            )?);
        }
    }
    // rsid and rsid-store share the shards (#1021 S4): a filter on either one
    // scopes both, so the unfiltered sibling only needs its compile check
    // instead of every shard (#1244).
    let rsid_family_filtered = selected
        .keys()
        .any(|package| is_rsid_shard_package(package));
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
            // #1282: every plain lib filter of another package runs in one
            // libtest invocation (libtest takes several filters).
            let mut lib_filters: Vec<&str> = Vec::new();
            for filter in package_filters {
                let focused = if is_rsid_shard_package(package) {
                    rsid_family_filter_command(package, filter)?
                } else if filter.starts_with("bin:")
                    || filter.starts_with("test:")
                    || filter.starts_with("test(=")
                {
                    if is_e2e_filter(filter) {
                        // #1596: build the rsid the harness spawns before the
                        // gated tests run; RSI_E2E=1 is set in the spec env.
                        let prebuild = e2e_prebuild_command();
                        if !commands.contains(&prebuild) {
                            commands.push(prebuild);
                        }
                    }
                    cargo_guard_command(package, Some(filter))
                } else {
                    if !lib_filters.contains(filter) {
                        lib_filters.push(filter);
                    }
                    continue;
                };
                if !commands.contains(&focused) {
                    commands.push(focused);
                }
            }
            if !lib_filters.is_empty() {
                let combined = cargo_lib_filters_command(package, &lib_filters);
                if !commands.contains(&combined) {
                    commands.push(combined);
                }
            }
        } else if is_rsid_shard_package(package) {
            let check = cargo_check_command(package);
            if rsid_family_filtered {
                if !commands.contains(&check) {
                    commands.push(check);
                }
                continue;
            }
            match rsid_unfiltered {
                // Unfiltered: every library test that can observe the change
                // on the shared lib build (#1280), the tests of a changed
                // binary or integration test target, and the compile check.
                Some(selection) => {
                    if !commands.contains(&check) {
                        commands.push(check);
                    }
                    let filterset = match &selection.lib {
                        test_impact::LibTests::Everything(_) => Some("all()".to_owned()),
                        // #2410: a leaf crate's own package run covers its tests; the
                        // shared lib build gets only the daemon-side observers.
                        test_impact::LibTests::Tests(tests) => {
                            let shared: Vec<String> = tests
                                .iter()
                                .filter(|name| !selection.leaf_tests.contains(*name))
                                .map(|name| format!("test(={name})"))
                                .collect();
                            (!shared.is_empty()).then(|| shared.join(" | "))
                        }
                    };
                    if let Some(filterset) = filterset {
                        let focused = rsid_lib_filter_command(&filterset);
                        if !commands.contains(&focused) {
                            commands.push(focused);
                        }
                    }
                    for (target_package, target) in &selection.targets {
                        if target_package != package {
                            continue;
                        }
                        let run = cargo_guard_command(package, Some(target));
                        if !commands.contains(&run) {
                            commands.push(run);
                        }
                    }
                }
                // A crate-wide change (root, manifest, build script) or an
                // unknown diff: gate all shards (the store's library tests
                // run inside the shared shards).
                None => {
                    for shard in RSID_SHARDS {
                        let shard_command = rsid_shard_guard_command(shard)?;
                        if !commands.contains(&shard_command) {
                            commands.push(shard_command);
                        }
                    }
                }
            }
        } else if let Some(info) = other_targets.get(package.as_str()) {
            // #1937, #1650: an unfiltered package gates its library tests and
            // the tests of every bin or integration test target whose sources,
            // mounted modules or read data changed. A package without a
            // library target has no `--lib` run. #1952: when its production
            // library code changed, every target of the package must still
            // compile, including the ones that declare no test.
            if info.check {
                let check = cargo_check_command(package);
                if !commands.contains(&check) {
                    commands.push(check);
                }
            }
            if info.has_lib {
                commands.push(cargo_guard_command(package, None));
            }
            for target in &info.targets {
                if is_e2e_filter(target) {
                    // #1596: the e2e harness spawns a prebuilt rsid.
                    let prebuild = e2e_prebuild_command();
                    if !commands.contains(&prebuild) {
                        commands.push(prebuild);
                    }
                }
                let run = cargo_guard_command(package, Some(target));
                if !commands.contains(&run) {
                    commands.push(run);
                }
            }
        } else {
            commands.push(cargo_guard_command(package, None));
        }
    }
    // #1261: every focused filter of the shared lib build runs in one nextest
    // invocation (combined, deduplicated filterset) at the place of the first.
    if let Some(first) = commands
        .iter()
        .position(|command| rsid_lib_filterset(command).is_some())
    {
        let atoms = combined_focused_atoms(commands.iter().filter_map(rsid_lib_filterset));
        commands.retain(|command| rsid_lib_filterset(command).is_none());
        commands.insert(first, rsid_focused_run_command(&atoms));
    }
    if commands
        .iter()
        .any(|command| rsid_lib_filterset(command).is_some())
    {
        // The shard script checks its static inventory before any build;
        // the shared lib build skips the script, so check it once here.
        commands.insert(1, rsid_shard_inventory_check());
    }
    let dependents = reverse_workspace_dependents(metadata, touched_packages)?;
    for dependent in &dependents {
        let check = cargo_check_command(dependent);
        if !commands.contains(&check) {
            commands.push(check);
        }
    }
    // #1970: the non-test build of every changed crate, plus the shipped
    // binaries' crate when a change reaches it. They run after the dev-profile
    // checks, in the same target.
    let shipped = SHIPPED_BINS_PACKAGE.to_owned();
    let reaches_shipped = dependents.contains(&shipped);
    for package in touched_packages
        .iter()
        .chain(reaches_shipped.then_some(&shipped))
    {
        let check = cargo_release_check_command(package);
        if !commands.contains(&check) {
            commands.push(check);
        }
    }
    // #1596: the e2e_tui tests are no-ops without RSI_E2E=1; set it only when
    // a gate filter selects them, so every other gate costs what it did.
    let mut e2e_env = BTreeMap::new();
    let derived_e2e = other_targets
        .iter()
        .filter(|(package, _)| !selected.contains_key(package.as_str()))
        .any(|(_, info)| info.targets.iter().any(|target| is_e2e_filter(target)));
    if derived_e2e
        || selected
            .values()
            .flatten()
            .any(|filter| is_e2e_filter(filter))
    {
        e2e_env.insert("RSI_E2E".to_owned(), "1".to_owned());
    }
    let mut env = BTreeMap::from([
        ("CARGO_BUILD_JOBS".into(), cargo_build_jobs.to_string()),
        ("CARGO_PROFILE_DEV_DEBUG".into(), "line-tables-only".into()),
        // #1244: the shard script builds its tests with no debug info.
        // Every other gate build (prebuild, focused runs, isolated
        // retries) must use the same test profile, or each one compiles
        // the whole dependency graph again under different flags.
        ("CARGO_PROFILE_TEST_DEBUG".into(), "0".into()),
        // The gate's target is private and discarded: a per-shard clean
        // there only deletes binaries the prebuild compiled.
        ("RSI_LANDER_KEEP_RSID_ARTIFACTS".into(), "1".into()),
    ]);
    env.extend(e2e_env);
    Ok(GuardSpec {
        commands,
        env,
        output_tail_bytes: 8 * 1024 * 1024,
    })
}

/// The typed refusal code of a failed release-build check (#1970); the
/// refusal is `release_build_failed:<crate>`.
const RELEASE_BUILD_FAILED: &str = rsi_common::rolling_queue::QUEUE_RELEASE_BUILD_FAILED;

/// #1970: the non-test build of `package` (`cargo check -p <package>`: its
/// library and shipped binaries, no tests and so no dev-dependencies). Test
/// builds enable `test-seam` through dev-dependency self edges, so a `cfg`
/// mistake that only a release or deploy build sees passes every test gate.
/// Debug-profile `check` is enough to catch it, in the compile checks' target,
/// where third-party dependencies are already built.
fn cargo_release_check_command(package: &str) -> GuardCommand {
    GuardCommand {
        program: "cargo".into(),
        args: vec!["check".into(), "-p".into(), package.into()],
        timeout: guard_timeout(),
    }
}

/// Whether the gate runs its release-build checks. Always, except in unit
/// tests whose fake `cargo` models test runs only: those opt in with
/// [`FORCE_RELEASE_CHECKS`] (#1970).
fn release_checks_enabled() -> bool {
    #[cfg(test)]
    {
        FORCE_RELEASE_CHECKS.load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(not(test))]
    {
        true
    }
}

/// Test hook of [`release_checks_enabled`]; set only while holding `ENV_LOCK`.
#[cfg(test)]
static FORCE_RELEASE_CHECKS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The package of a [`cargo_release_check_command`]; `None` for every other
/// command (the dev-profile check names `--all-targets`).
fn release_check_package(command: &GuardCommand) -> Option<&str> {
    match command.args.as_slice() {
        [check, flag, package]
            if command.program == "cargo" && check == "check" && flag == "-p" =>
        {
            Some(package.as_str())
        }
        _ => None,
    }
}

/// The package holding the shipped daemon binaries (`rsid`,
/// `rsi-rolling-land`) the release install builds (#1970).
const SHIPPED_BINS_PACKAGE: &str = "rsid";

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

/// The integration test target whose tests return `Ok` at once unless
/// `RSI_E2E=1` and that spawn a prebuilt `rsid` (#1596).
const E2E_TARGET: &str = "e2e_tui";

/// `test:e2e_tui` or `test:e2e_tui:test(ATOM)`: a filter that selects the
/// gated end-to-end target, which would silently no-op under plain
/// `cargo test` (#1596).
fn is_e2e_filter(filter: &str) -> bool {
    filter
        .strip_prefix("test:")
        .is_some_and(|rest| rest.split(':').next() == Some(E2E_TARGET))
}

/// The single definition of the e2e prebuild (#1651, #1637): one
/// `PACKAGE BINARY` pair per line. `scripts/scoped-test` reads the same file.
const E2E_PREBUILD_LIST: &str = include_str!("../../../../scripts/e2e-prebuild-bins.txt");

/// `(package, binary)` pairs parsed from [`E2E_PREBUILD_LIST`].
fn e2e_prebuild_bins(list: &str) -> Vec<(&str, &str)> {
    list.lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            let mut words = line.split_whitespace();
            Some((words.next()?, words.next()?))
        })
        .collect()
}

/// The prebuild an e2e run needs: the harness spawns `target/debug/rsid`,
/// which `cargo test -p rsi --test e2e_tui` does not build (#1596), and the
/// daemon looks its trusted siblings (`rsi-agent-mcp`, ...) up beside itself,
/// so every binary in the shared list is built together.
fn e2e_prebuild_command() -> GuardCommand {
    let pairs = e2e_prebuild_bins(E2E_PREBUILD_LIST);
    let mut args = vec!["build".to_owned()];
    let mut packages: Vec<&str> = Vec::new();
    for (package, _) in &pairs {
        if !packages.contains(package) {
            packages.push(package);
            args.extend(["-p".to_owned(), (*package).to_owned()]);
        }
    }
    for (_, binary) in &pairs {
        args.extend(["--bin".to_owned(), (*binary).to_owned()]);
    }
    GuardCommand {
        program: "cargo".into(),
        args,
        timeout: guard_timeout(),
    }
}

fn cargo_guard_command(package: &str, filter: Option<&str>) -> GuardCommand {
    let mut args = vec!["test".into(), "-p".into(), package.into()];
    // `bin:NAME` and `test:NAME` select a cargo target (rsid main.rs bin
    // tests, an integration test binary) instead of a lib test filter. A
    // named target also takes `:test(ATOM)` (plain substring, or `=EXACT`).
    let mut atom = None;
    match filter.and_then(|filter| {
        filter
            .strip_prefix("bin:")
            .map(|name| ("--bin", name))
            .or_else(|| filter.strip_prefix("test:").map(|name| ("--test", name)))
    }) {
        Some((flag, name)) => {
            let (name, selector) = match name.split_once(':') {
                Some((name, selector)) => (name, Some(selector)),
                None => (name, None),
            };
            atom = selector.and_then(|selector| selector.strip_prefix("test(")?.strip_suffix(')'));
            args.extend([flag.into(), name.into()]);
        }
        None => {
            args.push("--lib".into());
            if let Some(filter) = filter {
                if let Some(exact) = filter
                    .strip_prefix("test(")
                    .and_then(|s| s.strip_suffix(')'))
                    .filter(|s| s.starts_with('='))
                {
                    // Public-item consumers name exact library tests (#3408).
                    // Keep these separate from the plain substring batch.
                    atom = Some(exact);
                } else {
                    args.push(filter.into());
                }
            }
        }
    }
    // The e2e tests each spawn their own daemon and PTY; run them serially.
    let threads = if filter.is_some_and(is_e2e_filter) {
        "--test-threads=1"
    } else {
        "--test-threads=4"
    };
    args.extend(["--".into(), threads.into()]);
    if let Some(atom) = atom {
        match atom.strip_prefix('=') {
            Some(exact) => args.extend([exact.to_owned(), "--exact".into()]),
            None => args.push(atom.to_owned()),
        }
    }
    GuardCommand {
        program: "cargo".into(),
        args,
        timeout: guard_timeout(),
    }
}

/// #1282: one `cargo test -p PKG --lib` run of every plain lib filter of a
/// package that is not an rsid shard package. One filter keeps the
/// `cargo test -p PKG --lib FILTER` shape; several go after `--`, where
/// libtest runs every test any of them selects (substring match). The guard
/// grows with the number of filters it replaces (up to four guards).
fn cargo_lib_filters_command(package: &str, filters: &[&str]) -> GuardCommand {
    if let [filter] = filters {
        return cargo_guard_command(package, Some(filter));
    }
    let mut command = cargo_guard_command(package, None);
    command
        .args
        .extend(filters.iter().map(|filter| (*filter).to_owned()));
    let factor = u32::try_from(filters.len().clamp(1, 4)).unwrap_or(4);
    command.timeout = command
        .timeout
        .saturating_mul(factor)
        .min(Duration::from_secs(MAX_GUARD_TIMEOUT_SECS).max(command.timeout));
    command
}

/// The package and lib filters of a `cargo_lib_filters_command`; `None` for
/// any other shape (an unfiltered lib run, a bin or integration target).
fn cargo_lib_filters(command: &GuardCommand) -> Option<(&str, Vec<&str>)> {
    if command.program != "cargo" {
        return None;
    }
    match command.args.as_slice() {
        [test, p, package, lib, filter, separator, threads]
            if test == "test"
                && p == "-p"
                && lib == "--lib"
                && !filter.starts_with('-')
                && separator == "--"
                && threads == "--test-threads=4" =>
        {
            Some((package.as_str(), vec![filter.as_str()]))
        }
        [test, p, package, lib, separator, threads, filters @ ..]
            if test == "test"
                && p == "-p"
                && lib == "--lib"
                && separator == "--"
                && threads == "--test-threads=4"
                && filters.len() > 1
                && filters.iter().all(|filter| !filter.starts_with('-')) =>
        {
            Some((
                package.as_str(),
                filters.iter().map(String::as_str).collect(),
            ))
        }
        _ => None,
    }
}

/// #1282: the per-filter labels (`PKG=FILTER`) a combined lib run attributes
/// a failing test to.
fn cargo_lib_filter_labels(command: &GuardCommand) -> Option<Vec<FocusedFilter>> {
    let (package, filters) = cargo_lib_filters(command)?;
    Some(
        filters
            .into_iter()
            .map(|filter| FocusedFilter {
                label: format!("{package}={filter}"),
                filterset: format!("test({filter})"),
                explicit: true,
            })
            .collect(),
    )
}

/// `cargo test -p PKG --lib -- --list FILTER...`: the tests the run selects,
/// listed after the build and before any test runs.
fn cargo_lib_list_command(package: &str, filters: &[&str]) -> GuardCommand {
    let mut args: Vec<String> = ["test", "-p", package, "--lib", "--", "--list"]
        .into_iter()
        .map(String::from)
        .collect();
    args.extend(filters.iter().map(|filter| (*filter).to_owned()));
    GuardCommand {
        program: "cargo".into(),
        args,
        timeout: guard_timeout(),
    }
}

/// The test names of a libtest `--list` (`name: test` lines), or `None`
/// when the report is no complete listing (libtest ends one with its
/// `N tests, M benchmarks` count).
fn libtest_listed_names(
    report: &rsid::integration::GuardCommandReport,
) -> Option<BTreeSet<String>> {
    if report.status != rsid::integration::GuardStatus::Passed || report.output_truncated {
        return None;
    }
    let counted = report.stdout_tail.lines().any(|line| {
        let Some((tests, benchmarks)) = line.trim().split_once(", ") else {
            return false;
        };
        let count = |text: &str, singular: &str, plural: &str| {
            text.strip_suffix(plural)
                .or_else(|| text.strip_suffix(singular))
                .is_some_and(|number| number.trim().parse::<u64>().is_ok())
        };
        count(tests, " test", " tests") && count(benchmarks, " benchmark", " benchmarks")
    });
    counted.then(|| {
        report
            .stdout_tail
            .lines()
            .filter_map(|line| line.trim().strip_suffix(": test"))
            .map(str::to_owned)
            .collect()
    })
}

/// The lib filters of a combined run that select no listed test (libtest's
/// substring match).
fn lib_filters_selecting_nothing<'a>(
    filters: &[&'a str],
    listed: &BTreeSet<String>,
) -> Vec<&'a str> {
    filters
        .iter()
        .copied()
        .filter(|filter| !listed.iter().any(|name| name.contains(filter)))
        .collect()
}

/// #1282: list each combined lib run of another package after its build and
/// refuse the filters that select nothing, naming them, before any test
/// runs. A listing that cannot be read (a failed build reports itself in the
/// run) refuses nothing.
async fn refuse_lib_filters_selecting_nothing(
    worktree: &Path,
    spec: &GuardSpec,
    gate: &TestGate,
) -> Result<(), String> {
    let mut list_spec = spec.clone();
    if let Some(scratch) = &gate.scratch {
        list_spec.env.insert(
            "TMPDIR".into(),
            scratch.path().to_string_lossy().into_owned(),
        );
    }
    let mut empty = Vec::new();
    for command in &spec.commands {
        let Some((package, filters)) = cargo_lib_filters(command) else {
            continue;
        };
        let report = run_one_guard(
            worktree,
            &list_spec,
            &cargo_lib_list_command(package, &filters),
        )
        .await?;
        let Some(listed) = libtest_listed_names(&report) else {
            println!(
                "lib_filter_list=unavailable package={package} status={:?}",
                report.status
            );
            continue;
        };
        println!(
            "lib_filter_list=package={package} selected={} filters={}",
            listed.len(),
            filters.len()
        );
        empty.extend(
            lib_filters_selecting_nothing(&filters, &listed)
                .into_iter()
                .map(|filter| format!("{package}={filter}")),
        );
    }
    if empty.is_empty() {
        return Ok(());
    }
    let hint = rsid::rolling_queue::no_test_filter_hint(worktree, "HEAD", &empty);
    Err(format!(
        "test filter selects no test at the candidate, refused after the build and before any test ran: {}{hint}",
        empty.join(" ")
    ))
}

/// The packages whose library tests the rsid shards partition (#1021 S4).
const RSID_LIB_TEST_PACKAGES: [&str; 2] = ["rsid", "rsid-store"];

/// The argument list of one nextest run over every rsid library test with no
/// shard feature, so the lib test binaries are built once for all of them.
fn rsid_lib_nextest_args() -> Vec<String> {
    let mut args: Vec<String> = ["nextest", "run", "--profile", "rsid-fast"]
        .into_iter()
        .map(String::from)
        .collect();
    for package in RSID_LIB_TEST_PACKAGES {
        args.extend(["-p".into(), package.into()]);
    }
    args.push("--lib".into());
    args.extend(slow_tier_run_args());
    args
}

/// #1244: a focused `rsid`/`rsid-store` filter runs on the one lib test build
/// without shard features (every test compiled, the filterset selects), so N
/// focused filters cost one build per side instead of one build per shard.
/// The shard features stay for whole shards and the QA sweep.
fn rsid_lib_filter_command(filterset: &str) -> GuardCommand {
    let mut args = rsid_lib_nextest_args();
    args.extend(
        [
            "--status-level",
            "all",
            "--final-status-level",
            "all",
            "-j",
            "4",
            "--filterset",
        ]
        .map(String::from),
    );
    args.push(filterset.into());
    GuardCommand {
        program: "cargo".into(),
        args,
        timeout: guard_timeout(),
    }
}

/// Test threads of the combined focused rsid run when the host's threads or
/// load are unknown (#1261): the `rsid-fast` profile's historical eight.
const RSID_FOCUSED_TEST_THREADS: u32 = 8;

/// Bounds of the load-adaptive test threads (#2086).
const MIN_GATE_TEST_THREADS: u32 = 4;
const MAX_GATE_TEST_THREADS: u32 = 16;

/// The test threads this gate runs with, set once per gate by
/// [`set_gate_test_threads`]; 0 until then (a unit test, or a path that never
/// ran the gate), which reads as [`RSID_FOCUSED_TEST_THREADS`].
#[cfg(not(test))]
static GATE_TEST_THREADS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

// Unit tests run in parallel inside one process and several of them run a whole
// gate, which records the host-adaptive threads. A process-wide value made the
// tests that pin the unrecorded default (eight) fail whenever another test's
// gate had recorded the host's value (#2101), so under test it is per thread
// (every test, and its current-thread runtime, owns one).
#[cfg(test)]
thread_local! {
    static GATE_TEST_THREADS: std::sync::atomic::AtomicU32 = const { std::sync::atomic::AtomicU32::new(0) };
}

/// #2270: which tier of the rsid-store library tests a gate runs. The tests that
/// re-prove released migrations (`.config/slow-tier.toml`) are left out of the
/// `rsid-fast` profile by its default-filter; a diff that touches a migration,
/// the migration runner or such a test's own source needs them (`Full`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlowTier {
    /// Not decided (a unit test, or a path that never ran the gate): commands keep
    /// their legacy shape and the profile's default-filter applies as it is.
    Unset,
    /// The default tier. A focused filterset that selects only slow-tier tests then
    /// selects nothing, which must warn, not fail (`--no-tests warn`).
    Excluded,
    /// The slow tier runs too: `--ignore-default-filter`, and `RSI_LANDER_SLOW_TIER=full`
    /// for the shard script.
    Full,
}

#[cfg(not(test))]
static GATE_SLOW_TIER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

// Per thread under test for the reason GATE_TEST_THREADS is (#2101).
#[cfg(test)]
thread_local! {
    static GATE_SLOW_TIER: std::sync::atomic::AtomicU8 = const { std::sync::atomic::AtomicU8::new(0) };
}

fn store_gate_slow_tier(tier: SlowTier) {
    let value = match tier {
        SlowTier::Unset => 0,
        SlowTier::Excluded => 1,
        SlowTier::Full => 2,
    };
    #[cfg(not(test))]
    GATE_SLOW_TIER.store(value, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    GATE_SLOW_TIER.with(|cell| cell.store(value, std::sync::atomic::Ordering::Relaxed));
}

fn gate_slow_tier() -> SlowTier {
    #[cfg(not(test))]
    let value = GATE_SLOW_TIER.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    let value = GATE_SLOW_TIER.with(|cell| cell.load(std::sync::atomic::Ordering::Relaxed));
    match value {
        1 => SlowTier::Excluded,
        2 => SlowTier::Full,
        _ => SlowTier::Unset,
    }
}

/// The nextest flags that carry the gate's tier on a `run` of the rsid library
/// tests: none when undecided.
fn slow_tier_run_args() -> Vec<String> {
    match gate_slow_tier() {
        SlowTier::Unset => Vec::new(),
        SlowTier::Excluded => vec!["--no-tests".into(), "warn".into()],
        SlowTier::Full => vec!["--ignore-default-filter".into()],
    }
}

/// #2299: whether `.config/nextest.toml` provably leaves no test out of the default
/// profile: the file is absent, or every `default-filter` it holds (in any profile or
/// override) is `all()`. An unreadable or unparsable file, or any other filter, is not
/// proof: that filter may be the slow tier's generated exclusion (#2270).
fn nextest_config_excludes_no_tests(worktree: &Path) -> bool {
    fn selects_everything(value: &toml::Value) -> bool {
        match value {
            toml::Value::Table(table) => table.iter().all(|(key, value)| {
                if key == "default-filter" {
                    value
                        .as_str()
                        .is_some_and(|filter| filter.trim() == "all()")
                } else {
                    selects_everything(value)
                }
            }),
            toml::Value::Array(items) => items.iter().all(selects_everything),
            _ => true,
        }
    }
    match std::fs::read_to_string(worktree.join(".config/nextest.toml")) {
        Ok(text) => toml::from_str::<toml::Value>(&text)
            .map(|config| selects_everything(&config))
            .unwrap_or(false),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// #2270: the gate's tier for the diff `base..candidate`, from the one decision
/// `scripts/check-touched-shards --slow-tier-only` makes (a trigger path or a line
/// of a slow test's own source runs the tier). A tree without `.config/slow-tier.toml`
/// has no tier (`Unset`) only when its nextest config provably excludes no test
/// (#2299); otherwise the exclusion is still in force and the tier runs in full, as
/// the selector decides for an absent manifest. Anything but a clear `excluded` runs
/// the tier: a missing script, a failure or unreadable output.
fn slow_tier_for_gate(worktree: &Path, base: &str, candidate: &str) -> (SlowTier, String) {
    if !worktree.join(".config/slow-tier.toml").is_file() {
        // A tree with no tier and no default-filter exclusion keeps the legacy commands.
        // One whose nextest config still filters tests out must not run with that
        // exclusion unrecorded: it runs everything.
        return if nextest_config_excludes_no_tests(worktree) {
            (SlowTier::Unset, "no_slow_tier_config".into())
        } else {
            (
                SlowTier::Full,
                "no_slow_tier_config_with_default_filter".into(),
            )
        };
    }
    let script = worktree.join("scripts/check-touched-shards");
    let output = lander_env::command("python3")
        // `-B`: never write bytecode into the candidate worktree.
        .arg("-B")
        .arg(&script)
        .args(["--slow-tier-only", "--repo"])
        .arg(worktree)
        .args(["--base", base, "--head", candidate])
        .output();
    let decision = output
        .as_ref()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok());
    let Some(decision) = decision else {
        return (SlowTier::Full, "decision_unavailable".into());
    };
    let reason = decision
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    match decision.get("mode").and_then(Value::as_str) {
        Some("excluded") => (SlowTier::Excluded, reason),
        _ => (SlowTier::Full, reason),
    }
}

/// The CPU bound the lander's cgroup lives under and how much of it is in use
/// now (#3225): the tightest `cpu.max` among the lander scope's ancestors (the
/// `rsi-build.slice` thermal cap, 8 CPUs by default) and the CPUs that cgroup
/// used over a short sample (`cpu.stat` `usage_usec`).
#[derive(Clone, Copy, Debug, PartialEq)]
struct SliceHeadroom {
    quota_cpus: f64,
    used_cpus: f64,
}

/// What the gate's test-thread choice was made from, for `gate_timing`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct GateThreadInputs {
    host_threads: Option<usize>,
    load1: Option<f64>,
    slice: Option<SliceHeadroom>,
}

impl GateThreadInputs {
    /// `quota:8.00,slice_use:0.40,host_load:9.10,host_threads:8,basis:slice`
    /// (no spaces: `gate_timing=` is one space-separated key=value line).
    fn evidence(&self) -> String {
        let number =
            |value: Option<f64>| value.map_or_else(|| "unknown".to_owned(), |v| format!("{v:.2}"));
        format!(
            "quota:{},slice_use:{},host_load:{},host_threads:{},basis:{}",
            number(self.slice.map(|slice| slice.quota_cpus)),
            number(self.slice.map(|slice| slice.used_cpus)),
            number(self.load1),
            self.host_threads
                .map_or_else(|| "unknown".to_owned(), |threads| threads.to_string()),
            if self.slice.is_some() {
                "slice"
            } else if self.host_threads.is_some() && self.load1.is_some() {
                "host"
            } else {
                "default"
            },
        )
    }
}

/// The CPU quota of a cgroup v2 `cpu.max` (`"800000 100000"` is eight CPUs);
/// `max` (no quota) or nonsense is `None`.
#[cfg(any(test, target_os = "linux"))]
fn parse_cpu_max_cpus(text: &str) -> Option<f64> {
    let mut fields = text.split_whitespace();
    let quota: f64 = fields.next()?.parse().ok()?;
    let period: f64 = fields.next().unwrap_or("100000").parse().ok()?;
    (quota.is_finite() && period.is_finite() && quota > 0.0 && period > 0.0).then(|| quota / period)
}

/// `usage_usec` of a cgroup v2 `cpu.stat`.
#[cfg(any(test, target_os = "linux"))]
fn parse_cpu_usage_usec(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == "usage_usec")
            .then(|| fields.next()?.parse().ok())
            .flatten()
    })
}

/// The cgroup path of a `/proc/<pid>/cgroup` read: the unified (`0::`) line.
#[cfg(any(test, target_os = "linux"))]
fn parse_unified_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().to_owned())
        .filter(|path| path.starts_with('/'))
}

/// How long the slice's CPU use is sampled at gate start (Linux only: the sampler is cgroup v2).
#[cfg(target_os = "linux")]
const SLICE_SAMPLE: Duration = Duration::from_millis(1000);

/// #3225: measure the headroom inside the bound the lander's threads come from.
/// Walks the lander's cgroup upward for the tightest numeric `cpu.max` and samples
/// that cgroup's `cpu.stat` `usage_usec` over [`SLICE_SAMPLE`]. `None` off Linux,
/// without cgroup v2, with no quota anywhere or with an unreadable file.
#[cfg(target_os = "linux")]
fn read_slice_headroom() -> Option<SliceHeadroom> {
    let root = Path::new("/sys/fs/cgroup");
    let own = parse_unified_cgroup(&std::fs::read_to_string("/proc/self/cgroup").ok()?)?;
    let mut best: Option<(f64, PathBuf)> = None;
    let mut dir = root.join(own.trim_start_matches('/'));
    while dir.starts_with(root) {
        if let Some(cpus) = std::fs::read_to_string(dir.join("cpu.max"))
            .ok()
            .and_then(|text| parse_cpu_max_cpus(&text))
            && best.as_ref().is_none_or(|(tightest, _)| cpus < *tightest)
        {
            best = Some((cpus, dir.clone()));
        }
        if !dir.pop() {
            break;
        }
    }
    let (quota_cpus, bound) = best?;
    let usage = || {
        std::fs::read_to_string(bound.join("cpu.stat"))
            .ok()
            .and_then(|text| parse_cpu_usage_usec(&text))
    };
    let before = usage()?;
    let started = Instant::now();
    std::thread::sleep(SLICE_SAMPLE);
    let after = usage()?;
    let wall_usec = started.elapsed().as_micros() as f64;
    if wall_usec <= 0.0 || after < before {
        return None;
    }
    Some(SliceHeadroom {
        quota_cpus,
        used_cpus: (after - before) as f64 / wall_usec,
    })
}

#[cfg(not(target_os = "linux"))]
fn read_slice_headroom() -> Option<SliceHeadroom> {
    None
}

/// #3225: with the slice measured, `clamp(floor(quota - slice use), 4, 16)`
/// and never above the slice's own quota (the thermal cap holds): the headroom
/// is read inside the same bound the work runs under, so host load outside the
/// slice (ollama, the TUI) no longer pins every gate to the minimum. Without a
/// slice reading it is #2086's `clamp((host threads - 1-minute load) / 2, 4, 16)`,
/// the idle part of the host halved because the build slot and the compile still
/// share it; no load average (non-Linux) or no thread count keeps eight.
fn adaptive_test_threads(inputs: &GateThreadInputs) -> u32 {
    if let Some(slice) = inputs.slice
        && slice.quota_cpus.is_finite()
        && slice.used_cpus.is_finite()
        && slice.used_cpus >= 0.0
    {
        let idle = (slice.quota_cpus - slice.used_cpus).max(0.0);
        // Truncation is the intent: whole threads, rounded down.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let whole = idle.floor() as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cap =
            (slice.quota_cpus.floor() as u32).clamp(MIN_GATE_TEST_THREADS, MAX_GATE_TEST_THREADS);
        return whole.clamp(MIN_GATE_TEST_THREADS, cap);
    }
    let (Some(threads), Some(load)) = (inputs.host_threads, inputs.load1) else {
        return RSID_FOCUSED_TEST_THREADS;
    };
    if !load.is_finite() || load < 0.0 {
        return RSID_FOCUSED_TEST_THREADS;
    }
    let idle = (threads as f64 - load).max(0.0);
    // Truncation is the intent: whole threads, rounded down.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let half = (idle / 2.0).floor() as u32;
    half.clamp(MIN_GATE_TEST_THREADS, MAX_GATE_TEST_THREADS)
}

/// Read the slice's headroom, the host's threads and 1-minute load now, record
/// the gate's test threads and the inputs, and return them for the log line.
fn set_gate_test_threads() -> (u32, GateThreadInputs) {
    let inputs = GateThreadInputs {
        host_threads: std::thread::available_parallelism().ok().map(usize::from),
        load1: match rsid::host_load::read_host_load() {
            rsid::host_load::LoadReading::Load1(load) => Some(load),
            rsid::host_load::LoadReading::Unsupported => None,
        },
        slice: read_slice_headroom(),
    };
    let chosen = adaptive_test_threads(&inputs);
    store_gate_test_threads(chosen);
    store_gate_thread_inputs(&inputs);
    (chosen, inputs)
}

/// The inputs of the recorded choice, for `gate_timing`; per thread under test
/// for the reason GATE_TEST_THREADS is (#2101).
#[cfg(not(test))]
static GATE_THREAD_INPUTS: Mutex<Option<GateThreadInputs>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    static GATE_THREAD_INPUTS: std::cell::Cell<Option<GateThreadInputs>> = const { std::cell::Cell::new(None) };
}

fn store_gate_thread_inputs(inputs: &GateThreadInputs) {
    #[cfg(not(test))]
    {
        *GATE_THREAD_INPUTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(*inputs);
    }
    #[cfg(test)]
    GATE_THREAD_INPUTS.with(|cell| cell.set(Some(*inputs)));
}

/// `thread_inputs=...` evidence for `gate_timing`: what the recorded test-thread
/// choice was made from, or `unrecorded` when no gate chose one.
fn gate_thread_inputs_evidence() -> String {
    #[cfg(not(test))]
    let inputs = *GATE_THREAD_INPUTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(test)]
    let inputs = GATE_THREAD_INPUTS.with(std::cell::Cell::get);
    inputs.map_or_else(|| "unrecorded".to_owned(), |inputs| inputs.evidence())
}

fn store_gate_test_threads(threads: u32) {
    #[cfg(not(test))]
    GATE_TEST_THREADS.store(threads, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    GATE_TEST_THREADS.with(|cell| cell.store(threads, std::sync::atomic::Ordering::Relaxed));
}

fn load_gate_test_threads() -> u32 {
    #[cfg(not(test))]
    return GATE_TEST_THREADS.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    GATE_TEST_THREADS.with(|cell| cell.load(std::sync::atomic::Ordering::Relaxed))
}

/// The recorded gate test threads, or the unknown-host default.
fn gate_test_threads() -> u32 {
    match load_gate_test_threads() {
        0 => RSID_FOCUSED_TEST_THREADS,
        threads => threads,
    }
}

/// A command's identity for the base-result cache: the `-j` test thread count
/// is how fast a run goes, never what it runs, so a gate whose adaptive
/// threads differ from an earlier gate's still reuses its base results.
fn base_cache_command_key(command: &GuardCommand) -> String {
    let mut normalized = command.clone();
    if normalized.args.first().map(String::as_str) == Some("nextest")
        && let Some(at) = normalized.args.iter().position(|arg| arg == "-j")
        && let Some(value) = normalized.args.get_mut(at + 1)
    {
        *value = "*".into();
    }
    format!("{normalized:?}")
}

/// #1261: the one run of every focused rsid filter of a gate. The atoms are
/// joined into one filterset (`test(a) | test(b)`), so nextest lists the
/// binaries once and runs every selected test under one `-j` pool instead of
/// one startup, listing and serial run per filter. The run is bound by test
/// threads, not by invocations (30 real filters, 1776 tests: 1225 s at
/// `-j 4`, 440 s at `-j 12`), so it uses the gate's load-adaptive test threads
/// ([`adaptive_test_threads`], #2086; eight when unrecorded); `CARGO_BUILD_JOBS` stays an env
/// throttle that never changes a command. Its guard grows with the number of
/// atoms (up to four guards), because it replaces that many runs.
fn rsid_focused_run_command(atoms: &[String]) -> GuardCommand {
    let mut command = rsid_lib_filter_command(&atoms.join(" | "));
    if let Some(jobs) = command.args.iter().position(|arg| arg == "-j") {
        command.args[jobs + 1] = gate_test_threads().to_string();
    }
    // #1280: `all()` runs every library test, the work of every shard, so it
    // gets the guards of all of them (one guard timed it out at 6374 of 6684
    // tests on a loaded host).
    let replaced = if atoms.iter().any(|atom| atom == "all()") {
        RSID_SHARDS.len()
    } else {
        atoms.len().clamp(1, 4)
    };
    let factor = u32::try_from(replaced).unwrap_or(4);
    command.timeout = command
        .timeout
        .saturating_mul(factor)
        .min(Duration::from_secs(MAX_GUARD_TIMEOUT_SECS).max(command.timeout));
    command
}

/// #2631: a crate-wide gate (all 16 whole shards) compiles the rsid test code
/// once. Each shard feature set recompiles the whole `rsid` lib (about 82 s
/// each, one after another: 1394 s of prebuild in a 2052 s gate), while the
/// build without shard features already holds every test (#1244, #1280). The
/// shard commands are replaced by one `all()` run on that build, so the
/// prebuild is a single compile and the same tests run. Returns whether it
/// collapsed. Left alone: a gate that does not hold every whole shard (a
/// focused or closure gate builds only what it needs), and a gate with
/// `--disk-scratch-shard` marks, whose shards need their own private disk
/// scratch that a single combined run cannot give.
fn collapse_whole_shards_to_one_build(
    commands: &mut Vec<GuardCommand>,
    disk_scratch_shards: &BTreeSet<String>,
) -> bool {
    if !disk_scratch_shards.is_empty() {
        return false;
    }
    let whole: BTreeSet<&str> = commands
        .iter()
        .filter_map(|command| full_shard(command).map(|(shard, _)| shard))
        .collect();
    if !RSID_SHARDS.iter().all(|shard| whole.contains(shard)) {
        return false;
    }
    let Some(first) = commands
        .iter()
        .position(|command| full_shard(command).is_some() || rsid_lib_filterset(command).is_some())
    else {
        return false;
    };
    // `all()` selects what every other atom could, so it replaces any focused
    // run already in the spec too.
    commands
        .retain(|command| full_shard(command).is_none() && rsid_lib_filterset(command).is_none());
    commands.insert(first, rsid_focused_run_command(&["all()".to_owned()]));
    // The shard script checks its static inventory before any build; the
    // shared lib build skips the script, so check it once here.
    let inventory = rsid_shard_inventory_check();
    if !commands.contains(&inventory) {
        commands.insert(1.min(commands.len()), inventory);
    }
    true
}

/// Which rsid test build a gate ran, for `gate_timing` (#2631): `sharded`
/// (feature-gated builds, one per shard), `unsharded` (one build holding every
/// library test, run in full), `focused` (that same build, a filterset
/// selects) or `none`.
fn gate_build_mode(spec: &GuardSpec) -> &'static str {
    if spec
        .commands
        .iter()
        .any(|command| shard_command_parts(command).is_some())
    {
        return "sharded";
    }
    let mut mode = "none";
    for filterset in spec.commands.iter().filter_map(rsid_lib_filterset) {
        if focused_atoms(filterset).iter().any(|atom| atom == "all()") {
            return "unsharded";
        }
        mode = "focused";
    }
    mode
}

/// How a generated focused filterset atom selects test names: nextest's
/// `test(NAME)` matches any test whose name contains `NAME`; a module atom
/// `test(/^module::/)` matches the names under that module; `test(=NAME)`
/// matches that one test and `all()` every test (#1280).
#[derive(Clone, Debug, PartialEq, Eq)]
enum FocusedMatch {
    Contains(String),
    Module(String),
    Exact(String),
    All,
}

impl FocusedMatch {
    /// The matcher of one atom, `None` for a shape the lander does not
    /// generate (never deduplicated, never refused).
    fn parse(atom: &str) -> Option<Self> {
        if atom == "all()" {
            return Some(Self::All);
        }
        let inner = atom.strip_prefix("test(")?.strip_suffix(')')?;
        if let Some(name) = inner.strip_prefix('=') {
            return plain_test_name(name).then(|| Self::Exact(name.to_owned()));
        }
        if let Some(module) = inner
            .strip_prefix("/^")
            .and_then(|rest| rest.strip_suffix("::/"))
        {
            return (!module.is_empty()
                && module
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
            .then(|| Self::Module(module.to_owned()));
        }
        plain_test_name(inner).then(|| Self::Contains(inner.to_owned()))
    }

    fn matches(&self, name: &str) -> bool {
        match self {
            Self::Contains(pattern) => name.contains(pattern.as_str()),
            Self::Module(module) => name
                .strip_prefix(module.as_str())
                .is_some_and(|rest| rest.starts_with("::")),
            Self::Exact(exact) => name == exact,
            Self::All => true,
        }
    }

    /// Whether every test this matcher selects is also selected by `other`.
    fn covered_by(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Contains(mine), Self::Contains(theirs)) => mine.contains(theirs.as_str()),
            (Self::Module(module), Self::Contains(theirs)) => {
                format!("{module}::").contains(theirs.as_str())
            }
            (Self::Module(mine), Self::Module(theirs)) => mine == theirs,
            (Self::Exact(mine), theirs) => theirs.matches(mine),
            (_, Self::All) => true,
            (Self::Contains(_) | Self::Module(_) | Self::All, Self::Exact(_))
            | (Self::Contains(_), Self::Module(_))
            | (Self::All, _) => false,
        }
    }

    /// Whether some test name could be selected by both matchers (#1957).
    /// An exact name or `all()` decides it, two one-segment modules overlap
    /// only when equal, and any other pair is assumed to.
    fn may_overlap(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => true,
            (Self::Exact(name), other) | (other, Self::Exact(name)) => other.matches(name),
            (Self::Module(mine), Self::Module(theirs)) => mine == theirs,
            (Self::Contains(_) | Self::Module(_), Self::Contains(_) | Self::Module(_)) => true,
        }
    }
}

/// The atoms of a focused filterset (`a | b | c`).
fn focused_atoms(filterset: &str) -> Vec<String> {
    filterset
        .split(" | ")
        .map(str::trim)
        .filter(|atom| !atom.is_empty())
        .map(str::to_owned)
        .collect()
}

/// #1261: the atoms of several focused filtersets, in first-seen order, with
/// duplicates removed and every atom dropped whose tests another atom already
/// selects (`test()` matches substrings, so `test(a::b)` adds nothing beside
/// `test(a::)`). Shard names no longer limit a focused run (#1244), so two
/// filters in different shards can be the same run.
fn combined_focused_atoms<'a>(filtersets: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut atoms: Vec<String> = Vec::new();
    for filterset in filtersets {
        for atom in focused_atoms(filterset) {
            if !atoms.contains(&atom) {
                atoms.push(atom);
            }
        }
    }
    let matchers: Vec<Option<FocusedMatch>> =
        atoms.iter().map(|atom| FocusedMatch::parse(atom)).collect();
    atoms
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            let Some(mine) = &matchers[*index] else {
                return true;
            };
            !matchers.iter().enumerate().any(|(other, theirs)| {
                other != *index
                    && theirs
                        .as_ref()
                        .is_some_and(|theirs| mine.covered_by(theirs))
            })
        })
        .map(|(_, atom)| atom.clone())
        .collect()
}

/// One focused rsid filter of a gate, named as the operator wrote it (the
/// `--test-filter`, a provisional proof, or a touched-module atom).
#[derive(Clone, Debug, PartialEq, Eq)]
struct FocusedFilter {
    label: String,
    filterset: String,
    /// Caller-named filters stay strict. Queue-derived shard filters may
    /// widen to the full shard when their focused listing is empty.
    explicit: bool,
}

/// The provisional proofs a provisional landing always runs.
const PROVISIONAL_STORE_TESTS: [&str; 2] = [
    "store::tests::rewind_tears_down_the_non_idempotent_migration_tail",
    "store::tests::every_recovered_migration_step_actually_executes",
];

/// #1261: the explicit focused filters of a gate (the ones that run on the
/// shared rsid lib build), with the labels a refusal and an attribution name.
fn explicit_focused_filters(filters: &[String], provisional: bool) -> Vec<FocusedFilter> {
    let mut focused = Vec::new();
    if provisional {
        for name in PROVISIONAL_STORE_TESTS {
            focused.push(FocusedFilter {
                label: format!("provisional:{name}"),
                filterset: format!("test({name})"),
                explicit: true,
            });
        }
    }
    for filter in filters {
        let Some((package, value)) = filter.split_once('=') else {
            continue;
        };
        if !is_rsid_shard_package(package) {
            continue;
        }
        if let Ok(command) = rsid_family_filter_command(package, value)
            && let Some(filterset) = rsid_lib_filterset(&command)
        {
            focused.push(FocusedFilter {
                label: filter.clone(),
                filterset: filterset.to_owned(),
                explicit: true,
            });
        }
    }
    focused
}

/// Every focused filter behind one combined run: the explicit ones the gate
/// recorded, plus each run atom no explicit filter names (a touched-module
/// atom, or every atom when the gate recorded none).
fn focused_filters_of_run(run_filterset: &str, explicit: &[FocusedFilter]) -> Vec<FocusedFilter> {
    let mut filters = explicit.to_vec();
    for atom in focused_atoms(run_filterset) {
        if explicit.iter().any(|filter| filter.filterset == atom) {
            continue;
        }
        let explicit = matches!(FocusedMatch::parse(&atom), Some(FocusedMatch::Contains(_)));
        filters.push(FocusedFilter {
            label: atom.clone(),
            filterset: atom,
            explicit,
        });
    }
    filters
}

/// The labels of the focused filters that select test `name`.
fn focused_filters_selecting(filters: &[FocusedFilter], name: &str) -> Vec<String> {
    filters
        .iter()
        .filter(|filter| {
            FocusedMatch::parse(&filter.filterset).is_some_and(|matcher| matcher.matches(name))
        })
        .map(|filter| filter.label.clone())
        .collect()
}

/// The ` [filter a | b]` attribution a failing test of a combined focused run
/// carries in a refusal (`failing_test_names` keeps only the name).
fn focused_attribution_suffix(filters: &[FocusedFilter], name: &str) -> String {
    let labels = focused_filters_selecting(filters, name);
    if labels.is_empty() {
        String::new()
    } else {
        format!(" [filter {}]", labels.join(" | "))
    }
}

/// `cargo nextest list` over the shared rsid lib build with a run's
/// filterset, as JSON: every selected test name, read after the prebuild.
fn rsid_lib_list_command(filterset: &str) -> GuardCommand {
    let mut args = rsid_lib_nextest_args();
    args[1] = "list".into();
    // #2270: `list` takes no `--no-tests`, and a filter is judged by the tests that
    // exist, in either tier: the listing always ignores the default-filter.
    if let Some(at) = args.iter().position(|arg| arg == "--no-tests") {
        args.remove(at);
        args.remove(at);
    }
    if gate_slow_tier() != SlowTier::Unset
        && !args.iter().any(|arg| arg == "--ignore-default-filter")
    {
        args.push("--ignore-default-filter".into());
    }
    args.extend(
        ["--message-format", "json", "--filterset", filterset]
            .into_iter()
            .map(String::from),
    );
    GuardCommand {
        program: "cargo".into(),
        args,
        timeout: guard_timeout(),
    }
}

/// The test names a `cargo nextest list --message-format json` report says
/// its filterset selects (matching and not ignored), or `None` when the
/// report is no complete nextest listing.
fn listed_selected_tests(
    report: &rsid::integration::GuardCommandReport,
) -> Option<BTreeSet<String>> {
    if report.status != rsid::integration::GuardStatus::Passed || report.output_truncated {
        return None;
    }
    let listing: serde_json::Value = serde_json::from_str(report.stdout_tail.trim()).ok()?;
    let mut selected = BTreeSet::new();
    for suite in listing.get("rust-suites")?.as_object()?.values() {
        for (name, case) in suite.get("testcases")?.as_object()? {
            let matches = case
                .get("filter-match")
                .and_then(|status| status.get("status"))
                .and_then(serde_json::Value::as_str)
                == Some("matches");
            let ignored = case
                .get("ignored")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if matches && !ignored {
                selected.insert(name.clone());
            }
        }
    }
    Some(selected)
}

/// #1261/#1262: the focused filters a listing proves select no test. An
/// explicit filter that selects none is named; a touched-module atom only
/// counts when the whole run selects nothing. A filter whose shape the lander
/// cannot match is never named.
fn focused_filters_selecting_nothing(
    filters: &[FocusedFilter],
    selected: &BTreeSet<String>,
) -> Vec<String> {
    if selected.is_empty() {
        return filters.iter().map(|filter| filter.label.clone()).collect();
    }
    filters
        .iter()
        .filter(|filter| filter.explicit || focused_filter_shard(filter).is_some())
        .filter(|filter| {
            FocusedMatch::parse(&filter.filterset)
                .is_some_and(|matcher| !selected.iter().any(|name| matcher.matches(name)))
        })
        .map(|filter| filter.label.clone())
        .collect()
}

fn focused_filter_shard(filter: &FocusedFilter) -> Option<GuardCommand> {
    let (package, value) = filter.label.split_once('=')?;
    if !is_rsid_shard_package(package) {
        return None;
    }
    rsid_shard_guard_command(value.strip_prefix("shard:")?).ok()
}

/// List the shared focused run before testing. Widen a proven derived-empty
/// shard filter to its whole shard; handwritten empty filters stay refused.
/// The cheap source pre-check (#1243) misses names that exist only as a file
/// mounted elsewhere; the listing is the authority. A listing that cannot be
/// read (a failed build reports itself in the run) refuses nothing.
async fn refuse_focused_filters_selecting_nothing(
    worktree: &Path,
    spec: &GuardSpec,
    command: &GuardCommand,
    gate: &TestGate,
) -> Result<(Vec<GuardCommand>, bool), String> {
    let Some(filterset) = rsid_lib_filterset(command) else {
        return Ok((Vec::new(), false));
    };
    let mut list_spec = spec.clone();
    if let Some(scratch) = &gate.scratch {
        list_spec.env.insert(
            "TMPDIR".into(),
            scratch.path().to_string_lossy().into_owned(),
        );
    }
    let started = Instant::now();
    let report = run_one_guard(worktree, &list_spec, &rsid_lib_list_command(filterset)).await?;
    let filters = focused_filters_of_run(filterset, &gate.focused_filters);
    let Some(selected) = listed_selected_tests(&report) else {
        println!(
            "focused_list=unavailable status={:?} filters={}",
            report.status,
            filters.len()
        );
        return Ok((Vec::new(), false));
    };
    println!(
        "focused_list=selected={} filters={} atoms={} list_secs={}",
        selected.len(),
        filters.len(),
        focused_atoms(filterset).len(),
        started.elapsed().as_secs()
    );
    // #1280: a generated exact atom the listing lacks (a test compiled out
    // on this platform) is reported, never refused.
    let unlisted: Vec<&str> = filters
        .iter()
        .filter(|filter| !filter.explicit)
        .filter(|filter| {
            matches!(FocusedMatch::parse(&filter.filterset), Some(FocusedMatch::Exact(name)) if !selected.contains(&name))
        })
        .map(|filter| filter.label.as_str())
        .collect();
    if !unlisted.is_empty() {
        println!("focused_unlisted={}", unlisted.join(" "));
    }
    let mut empty = focused_filters_selecting_nothing(&filters, &selected);
    let mut replacements = Vec::new();
    let mut refused = Vec::new();
    for label in empty.drain(..) {
        let filter = filters.iter().find(|filter| filter.label == label).unwrap();
        let Some(mut shard) = focused_filter_shard(filter) else {
            refused.push(label);
            continue;
        };
        if filter.explicit {
            refused.push(label);
            continue;
        }
        shard.args.truncate(4);
        println!("focused_fallback=unfocused_shard filter={label}");
        if !replacements.contains(&shard) {
            replacements.push(shard);
        }
    }
    if refused.is_empty() {
        return Ok((replacements, selected.is_empty()));
    }
    let empty = refused;
    let hint = rsid::rolling_queue::no_test_filter_hint(worktree, "HEAD", &empty);
    Err(format!(
        "test filter selects no test at the candidate, refused after the build and before any test ran: {}{hint}",
        empty.join(" ")
    ))
}

/// Compile-only form of every `rsid_lib_filter_command` (they share it).
fn rsid_lib_build_command() -> GuardCommand {
    let mut args = rsid_lib_nextest_args();
    args.insert(2, "--no-run".into());
    GuardCommand {
        program: "cargo".into(),
        args,
        timeout: guard_timeout(),
    }
}

/// The filterset of an `rsid_lib_filter_command`, `None` for any other shape.
fn rsid_lib_filterset(command: &GuardCommand) -> Option<&str> {
    let base = rsid_lib_nextest_args();
    let args = &command.args;
    (command.program == "cargo"
        && args.len() == base.len() + 8
        && args[..base.len()] == base[..]
        && args[args.len() - 2] == "--filterset")
        .then(|| args[args.len() - 1].as_str())
}

/// A test name a nextest `test(...)` filterset and an exact retry accept.
fn plain_test_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
}

/// Whether `atom` is one nextest test atom the lander passes to a shard run
/// as a single argv element: `test(NAME)` (substring), `test(=NAME)` (exact)
/// or `test(/REGEX/)`, the shapes `scripts/check-touched-shards` emits. A
/// regex may hold any printable ASCII with balanced parentheses (an escaped
/// one does not count); control characters, an empty body and the ` | ` atom
/// separator are refused.
fn valid_test_atom(atom: &str) -> bool {
    let Some(inner) = atom
        .strip_prefix("test(")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return false;
    };
    if let Some(name) = inner.strip_prefix('=') {
        return plain_test_name(name);
    }
    if let Some(regex) = inner
        .strip_prefix('/')
        .and_then(|value| value.strip_suffix('/'))
    {
        return valid_test_regex(regex);
    }
    plain_test_name(inner)
}

fn valid_test_regex(regex: &str) -> bool {
    if regex.is_empty()
        || regex.contains(" | ")
        || !regex.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        return false;
    }
    let (mut depth, mut escaped) = (0usize, false);
    for byte in regex.bytes() {
        match (escaped, byte) {
            (true, _) => escaped = false,
            (false, b'\\') => escaped = true,
            (false, b'(') => depth += 1,
            (false, b')') => match depth.checked_sub(1) {
                Some(rest) => depth = rest,
                None => return false,
            },
            _ => {}
        }
    }
    depth == 0 && !escaped
}

/// The gate command of one filter value on an rsid shard package (#1244). A
/// whole shard keeps its feature-selected harness; a focused `shard:S:test(N)`
/// or plain `N` filter runs on the shared lib test build. The shard name is
/// still validated, but no longer limits which tests the name selects.
fn rsid_family_filter_command(package: &str, filter: &str) -> Result<GuardCommand, String> {
    if let Some(shard) = filter.strip_prefix("shard:") {
        let command = rsid_shard_guard_command(shard)?;
        return Ok(match shard_command_parts(&command) {
            Some((_, _, Some(filterset))) => rsid_lib_filter_command(filterset),
            _ => command,
        });
    }
    if filter.starts_with("test(") && valid_test_atom(filter) {
        return Ok(rsid_lib_filter_command(filter));
    }
    if plain_test_name(filter) && !filter.starts_with("bin:") && !filter.starts_with("test:") {
        return Ok(rsid_lib_filter_command(&format!("test({filter})")));
    }
    Ok(cargo_guard_command(package, Some(filter)))
}

/// The static rsid shard inventory check the shard script runs first; a gate
/// that runs focused filters outside the script runs it once (#1244).
fn rsid_shard_inventory_check() -> GuardCommand {
    GuardCommand {
        program: "python3".into(),
        args: vec![
            "scripts/check-rsid-test-shards.py".into(),
            "--require-gates".into(),
        ],
        timeout: Duration::from_secs(120),
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
        if !valid_test_atom(filterset) {
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
        Ok(Ok(status)) if status.success() => {
            rsid_core::process_control::untrack_child_process_group(push_pid);
            ("push reported success".to_string(), true)
        }
        Ok(Ok(status)) => {
            rsid_core::process_control::untrack_child_process_group(push_pid);
            (format!("push exited {}", status_code(status)), false)
        }
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

/// Retry reads only. Each attempt retains its own timeout and child cleanup;
/// a push must instead be settled by confirm_post_push before any new action.
async fn retry_remote_read<T, F, Fut>(operation: &str, mut read: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut attempt = 1;
    loop {
        match read().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < 3 => {
                // Thirty seconds of backoff total, in addition to the bounded
                // command timeouts. Tests exercise the same attempts quickly.
                #[cfg(not(test))]
                let delay = Duration::from_secs(10 * attempt);
                #[cfg(test)]
                let delay = Duration::from_millis(10 * attempt);
                eprintln!(
                    "remote_read_retry operation={operation} attempt={}/3 delay_ms={} error={error}",
                    attempt + 1,
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn remote_fetch(repo: &Path) -> Result<(), String> {
    retry_remote_read("fetch", || remote_fetch_once(repo)).await
}

async fn remote_fetch_once(repo: &Path) -> Result<(), String> {
    let mut command = remote_git_command(
        repo,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
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
            rsid_core::process_control::untrack_child_process_group(pid);
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
        Ok(Ok(status)) => {
            rsid_core::process_control::untrack_child_process_group(pid);
            Err(format!(
                "remote rolling fetch failed (exit {})",
                status_code(status)
            ))
        }
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
    retry_remote_read("ls-remote", || remote_tip_once(repo, remote)).await
}

async fn remote_tip_once(repo: &Path, remote: &str) -> Result<Option<String>, String> {
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
        Ok(Ok(status)) => {
            rsid_core::process_control::untrack_child_process_group(pid);
            status
        }
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
    let mut command = lander_env::tokio_command("git");
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
    for (key, _) in lander_env::vars_os() {
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
    let pid = child
        .id()
        .map(|pid| nix::unistd::Pid::from_raw(pid.cast_signed()))
        .ok_or_else(|| format!("cannot identify {operation} process"))?;
    rsid_core::process_control::track_child_process_group(pid);
    Ok(pid)
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
    rsid_core::process_control::untrack_child_process_group(process_group);
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

/// #1192: resolve an `--accepted` id (full or abbreviated SHA, or a ref) to a
/// full commit id. The private clone is tried first; the caller's own
/// repository is the fallback, and a miss names the id and says what to pass
/// instead of surfacing git's bare "Needed a single revision".
fn resolve_accepted_commit(
    private_repo: &Path,
    caller_repo: &Path,
    role: &str,
    rev: &str,
) -> Result<String, String> {
    let spec = format!("{rev}^{{commit}}");
    let mut last_error = String::new();
    for repo in [private_repo, caller_repo] {
        match git_output(repo, &["rev-parse", "--verify", "--quiet", &spec]) {
            Ok(output) if output.status.success() => {
                let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !id.is_empty() {
                    return Ok(id);
                }
            }
            Ok(output) => {
                last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            }
            Err(error) => return Err(error),
        }
    }
    let detail = if last_error.is_empty() {
        String::new()
    } else {
        format!(" ({last_error})")
    };
    Err(format!(
        "--accepted {role} `{rev}` does not name exactly one commit in this repository{detail}; \
         an abbreviation may be unknown or ambiguous, so pass the full 40-hex commit id"
    ))
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
    git_output_with_env(repo, args, &[])
}

/// [`git_output`] with `envs` set on the child only (never the process
/// environment). Used where a commit must carry a fixed identity: the
/// `GIT_AUTHOR_*`/`GIT_COMMITTER_*` variables an operator, a sweep or a CI host
/// exports win over `-c user.name`/`-c user.email`.
fn git_output_with_env(
    repo: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<Output, String> {
    lander_env::command("git")
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
        .envs(envs.iter().copied())
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
    use std::process::{Command, Stdio};
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
    fn gate_builds_pin_one_build_id_unless_the_spec_names_one() {
        let mut env = BTreeMap::new();
        pin_gate_build_id(&mut env);
        assert_eq!(
            env.get("RSI_BUILD_SHA").map(String::as_str),
            Some("gate-build")
        );
        // A printable, space-free id passes the satellite health validation.
        assert!(GATE_BUILD_ID.bytes().all(|byte| byte.is_ascii_graphic()));

        let mut named = BTreeMap::from([("RSI_BUILD_SHA".to_owned(), "spec-id".to_owned())]);
        pin_gate_build_id(&mut named);
        assert_eq!(
            named.get("RSI_BUILD_SHA").map(String::as_str),
            Some("spec-id")
        );
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
    fn base_absent_target_requires_cargos_selected_target_diagnostic() {
        use rsid::integration::{GuardCommandReport, GuardStatus};

        for (filter, target, diagnostic) in [
            (
                Some("test:fresh"),
                "demo:test:fresh",
                "error: no test target named `fresh` in `demo` package\n",
            ),
            (
                Some("bin:fresh"),
                "demo:bin:fresh",
                "error: no bin target named `fresh` in default-run packages\n",
            ),
            (
                None,
                "demo:lib",
                "error: no library targets found in package `demo`\n",
            ),
        ] {
            let command = cargo_guard_command("demo", filter);
            let report = GuardCommandReport {
                program: command.program.clone(),
                args: command.args.clone(),
                status: GuardStatus::Failed { code: Some(101) },
                stdout_tail: String::new(),
                stderr_tail: diagnostic.into(),
                output_truncated: false,
                duration: Duration::from_millis(1),
            };
            assert_eq!(
                base_absent_targets(&command, &report),
                vec![target.to_owned()]
            );
            // The same diagnostic on the candidate is still a setup refusal.
            assert!(observed_test_failures(&report).is_err());
            for invalid in [
                GuardCommandReport {
                    status: GuardStatus::Passed,
                    ..report.clone()
                },
                GuardCommandReport {
                    output_truncated: true,
                    ..report.clone()
                },
                GuardCommandReport {
                    stderr_tail: "error: no test target named `other` in `demo` package".into(),
                    ..report.clone()
                },
                GuardCommandReport {
                    stderr_tail: "error: failed to download dependency".into(),
                    ..report.clone()
                },
            ] {
                assert_eq!(
                    base_absent_targets(&command, &invalid),
                    Vec::<String>::new()
                );
            }
            assert_eq!(
                base_absent_targets(&cargo_check_command("demo"), &report),
                Vec::<String>::new()
            );
        }
    }

    #[test]
    fn base_absent_targets_classifies_every_target_of_a_multi_target_command() {
        use rsid::integration::{GuardCommandReport, GuardStatus};

        let mut command = cargo_guard_command("demo", Some("test:old"));
        command.args = [
            "test", "-p", "demo", "--test", "old", "--test", "fresh", "--bin", "tool",
        ]
        .map(String::from)
        .to_vec();
        let report = |stderr: &str| GuardCommandReport {
            program: command.program.clone(),
            args: command.args.clone(),
            status: GuardStatus::Failed { code: Some(101) },
            stdout_tail: String::new(),
            stderr_tail: stderr.into(),
            output_truncated: false,
            duration: Duration::from_millis(1),
        };
        // A later target absent on the base is classified, not only the first.
        assert_eq!(
            base_absent_targets(
                &command,
                &report("error: no test target named `fresh` in `demo` package\n")
            ),
            vec!["demo:test:fresh".to_owned()]
        );
        assert_eq!(
            base_absent_targets(
                &command,
                &report(
                    "error: no test target named `fresh` in `demo` package\n\
                     error: no bin target named `tool` in default-run packages\n"
                )
            ),
            vec!["demo:test:fresh".to_owned(), "demo:bin:tool".to_owned()]
        );
        // A diagnostic for a target the command never selected proves nothing.
        assert_eq!(
            base_absent_targets(
                &command,
                &report("error: no test target named `other` in `demo` package\n")
            ),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn cached_empty_baseline_reports_the_absent_target_behind_it() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/tests/fresh.rs",
            "#[test]\nfn fresh() {}\n",
            "add integration test target",
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
            r#"oid=$(/usr/bin/git rev-parse HEAD)
if [ "$oid" = '{base}' ]; then
    echo 'error: no test target named `fresh` in `demo` package' >&2
    exit 101
fi
echo 'test fresh::ok ... ok'"#,
            base = fixture.base,
        ));
        let spec = GuardSpec {
            commands: vec![cargo_guard_command("demo", Some("test:fresh"))],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let expected = BTreeSet::from([format!("{}:demo:test:fresh", fixture.base)]);
        let cache_root = fixture.root.path().join("shared-cache");
        let old_path = use_fake_path(&fixture);
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
        .expect("first landing");
        assert_eq!(first.base_runs, 1);
        assert_eq!(first.base_absent_targets, expected);
        // The in-process cache hit still names why the baseline is empty.
        first.base_absent_targets.clear();
        run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &spec,
            &mut first,
            &BTreeSet::new(),
        )
        .await
        .expect("in-process reuse");
        assert_eq!(first.base_runs, 1, "the second pass reuses the baseline");
        assert_eq!(first.base_absent_targets, expected);
        // So does a later landing that reads the persistent cached baseline.
        let mut later = TestGate {
            cache_root: Some(cache_root),
            ..TestGate::default()
        };
        let result = run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &spec,
            &mut later,
            &BTreeSet::new(),
        )
        .await;
        restore_path(old_path);
        result.expect("persistent reuse");
        assert_eq!(later.base_runs, 0, "the baseline came from the cache");
        assert_eq!(later.base_absent_targets, expected);
    }

    #[tokio::test]
    async fn paired_test_gate_classifies_new_targets_and_existing_target_failures() {
        // The new target can be green, or introduce a real new failure. The
        // existing target must still distinguish its baseline red from a new red.
        for (fresh_fails, existing_new_failure) in [(false, false), (false, true), (true, false)] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/tests/fresh.rs",
                "#[test]\nfn fresh() {}\n",
                "add integration test target",
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
                r#"oid=$(/usr/bin/git rev-parse HEAD)
printf '%s %s\n' "$oid" "$4" >> '{log}'
if [ "$4" = --test ]; then
    if [ "$oid" = '{base}' ]; then
        echo 'error: no test target named `fresh` in `demo` package' >&2
        exit 101
    fi
    if [ '{fresh_fails}' = true ]; then
        echo 'test fresh::red ... FAILED'
        exit 1
    fi
    echo 'test fresh::ok ... ok'
else
    echo 'test demo::baseline_red ... FAILED'
    if [ "$oid" = '{candidate}' ] && [ '{existing_new_failure}' = true ]; then
        echo 'test demo::new_red ... FAILED'
    fi
    exit 1
fi"#,
                log = log.display(),
                base = fixture.base,
            ));
            let existing = cargo_guard_command("demo", None);
            let fresh = cargo_guard_command("demo", Some("test:fresh"));
            // Compare the existing red before the new-target red can refuse.
            // For a green new target, compare it first to prove subsequent
            // filters still run after the base target is found absent.
            let spec = GuardSpec {
                commands: if fresh_fails {
                    vec![existing, fresh]
                } else {
                    vec![fresh, existing]
                },
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
            assert_eq!(
                gate.base_absent_targets,
                BTreeSet::from([format!("{}:demo:test:fresh", fixture.base)]),
            );
            assert_eq!(
                gate.base_reds,
                BTreeSet::from(["demo::baseline_red".into()])
            );
            let runs = fs::read_to_string(log).unwrap();
            assert!(
                runs.lines()
                    .any(|line| line == format!("{} --lib", fixture.base))
            );
            if fresh_fails || existing_new_failure {
                let error = result.unwrap_err();
                assert!(error.contains(NEW_TEST_FAILURES), "{error}");
                assert!(
                    error.contains(if fresh_fails {
                        "fresh::red"
                    } else {
                        "demo::new_red"
                    }),
                    "{error}"
                );
            } else {
                result.expect("a new target does not abort classification of baseline failures");
            }
        }
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
            let source_guard = PathBuf::from(rsi_common::manifest_dir!())
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
            write(
                &repo,
                "tools/released-migrations.json",
                "{\"migration_file\":\"crates/rsid-store/src/store/mod.rs\",\"protected_sections\":{}}\n",
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
                    fixes_tests: Vec::new(),
                },
                TestGate {
                    reuse: Some(GateReuse::every_test("fixture")),
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
                fixes_tests: Vec::new(),
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
                fixes_tests: Vec::new(),
            })
            .await
        }

        /// #2167: a landing that claims to fix `fixes_tests`.
        async fn land_with_fixes(
            &self,
            accepted: Vec<AcceptedPair>,
            fixes_tests: Vec<String>,
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
                expected_tip: None,
                fixes_tests,
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

    /// Put the fixture's fake-bin directory first on the lander's `PATH` for
    /// the current thread (#192): the lander's children resolve their
    /// programs there, and the process `PATH` is left alone.
    fn use_fake_path(fixture: &Fixture) -> lander_env::Scope {
        with_lander_vars([("PATH", fake_path(fixture))])
    }

    fn fake_path(fixture: &Fixture) -> std::ffi::OsString {
        let mut dirs = vec![fixture.bin.clone()];
        dirs.extend(std::env::split_paths(
            &lander_env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(dirs).expect("fixture PATH")
    }

    fn restore_path(scope: lander_env::Scope) {
        drop(scope);
    }

    /// The current lander environment with `vars` set, installed for the
    /// current thread until the scope drops (#192).
    fn with_lander_vars<'a, V: AsRef<std::ffi::OsStr>>(
        vars: impl IntoIterator<Item = (&'a str, V)>,
    ) -> lander_env::Scope {
        vars.into_iter()
            .fold(
                lander_env::current().as_ref().clone(),
                |env, (name, value)| env.with_var(name, value.as_ref()),
            )
            .enter()
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
                base_direct.contains("nextest run --profile rsid-fast -p rsid-store --lib")
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
        let plain = direct_base_shard_command(
            &rsid_shard_guard_command("session-05").unwrap(),
            ShardTree::Static,
        )
        .unwrap();
        assert_eq!(plain.program, "cargo");
        assert_eq!(
            plain.args.join(" "),
            "nextest run --profile rsid-fast -p rsid --lib --no-default-features --features \
             test-shard-session-05 --status-level all --final-status-level all -j 4"
        );
        let filtered = direct_base_shard_command(
            &rsid_shard_guard_command("store-01:test(demo_filter)").unwrap(),
            ShardTree::Static,
        )
        .unwrap();
        assert!(
            filtered
                .args
                .ends_with(&["--filterset".to_string(), "test(demo_filter)".to_string()])
        );
        assert!(
            direct_base_shard_command(&cargo_guard_command("demo", None), ShardTree::Static)
                .is_none()
        );
    }

    /// A base cut before the #1021 S4 crate split declares every shard in
    /// `rsid` alone: its direct-harness and isolated-retry commands must name
    /// that package, not the candidate binary's current-layout map (which has
    /// store-01 in rsid-store only).
    #[test]
    fn base_shard_commands_follow_the_manifests_of_the_tree_they_run_in() {
        let root = tempfile::tempdir().unwrap();
        let manifest = |package: &str, features: &[&str]| {
            let mut text = String::from("[features]\ndefault = []\ntest-shard-mode = []\n");
            for feature in features {
                text.push_str(&format!("test-shard-{feature} = [\"test-shard-mode\"]\n"));
            }
            write(root.path(), &format!("crates/{package}/Cargo.toml"), &text);
        };
        // Pre-split: rsid declares store-01 and session-05, no rsid-store.
        manifest("rsid", &["store-01", "session-05"]);
        let command = rsid_shard_guard_command("store-01").unwrap();
        let pre_split = ShardTree::Worktree(root.path());
        let direct = direct_base_shard_command(&command, pre_split).unwrap();
        assert!(
            direct
                .args
                .join(" ")
                .contains("--profile rsid-fast -p rsid --lib --no-default-features --features test-shard-store-01"),
            "{:?}",
            direct.args
        );
        assert!(!direct.args.contains(&"rsid-store".to_string()));
        let retry = isolated_retry_command(&command, "store::tests::red", pre_split).unwrap();
        assert_eq!(
            retry.args[..4],
            ["test", "-p", "rsid", "--lib"],
            "{:?}",
            retry.args
        );
        // Split: rsid-store declares store-01, and store-04 spans both.
        manifest("rsid", &["store-04", "session-05"]);
        manifest("rsid-store", &["store-01", "store-04"]);
        let split = ShardTree::Worktree(root.path());
        let direct = direct_base_shard_command(&command, split).unwrap();
        assert!(
            direct.args.join(" ").contains("-p rsid-store --lib"),
            "{:?}",
            direct.args
        );
        let shared = rsid_shard_guard_command("store-04").unwrap();
        let direct = direct_base_shard_command(&shared, split).unwrap();
        assert!(
            direct
                .args
                .join(" ")
                .contains("-p rsid -p rsid-store --lib"),
            "{:?}",
            direct.args
        );
        // A tree with no readable manifest falls back to the current map.
        let empty = tempfile::tempdir().unwrap();
        let fallback =
            direct_base_shard_command(&command, ShardTree::Worktree(empty.path())).unwrap();
        assert!(fallback.args.join(" ").contains("-p rsid-store --lib"));
    }

    /// #2698: the shard command that observed a base red is printed whole, so
    /// the Issue the queue files can name it.
    #[test]
    fn a_base_red_command_is_the_program_and_its_arguments() {
        let command = cargo_guard_command("rsi-common", Some("test:research_corpus"));
        assert_eq!(
            guard_command_line(&command),
            format!("cargo {}", command.args.join(" "))
        );
        assert!(guard_command_line(&command).starts_with("cargo test -p rsi-common"));
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

    /// Hub #3483's one-test e2e guard: an actual whole-command timeout is
    /// accepted only with complete selection and known, observed base reds.
    #[tokio::test]
    async fn known_red_guard_timeout_gate_fails_closed() {
        const RED: &str = "fresh_install::test_e2e_fresh_install_appoint_project_manager";
        for case in [
            "known",
            "mixed",
            "unknown",
            "base_green",
            "claimed",
            "candidate_green",
            "green_prebuild_unavailable",
            "non_e2e",
            "non_e2e_green",
            "cached_text",
            "cached_old",
            "cached_mismatch",
        ] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"candidate\" }\n",
                "candidate",
            );
            let tree = fixture.root.path().join("timeout-candidate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            let listing = match case {
                "mixed" => {
                    format!("{RED}: test\nfresh_install::green: test\n\n2 tests, 0 benchmarks")
                }
                "unknown" => "unavailable inventory".into(),
                _ => format!("{RED}: test\n\n1 test, 0 benchmarks"),
            };
            let e2e = !case.starts_with("non_e2e");
            let target = if e2e { "e2e_tui" } else { "ordinary" };
            fixture.add_fake_cargo(&format!(
                "if [ '{case}' = green_prebuild_unavailable ] && [ \"$1\" = build ] && [ \"$(/usr/bin/git rev-parse HEAD)\" = '{base}' ]; then echo 'error: linker failed' >&2; exit 1; fi\ncase \" $* \" in\n*' --list '*) printf '%s\\n' '{listing}'; exit 0;;\nesac\n\
                 case \" $* \" in *' --test {target} '*)\n\
                 oid=$(/usr/bin/git rev-parse HEAD)\n\
                 if [ \"$oid\" = '{base}' ]; then\n\
                   if [ '{case}' = candidate_green ]; then echo 'base infrastructure unavailable'; exit 1; fi\n\
                   if [ '{case}' = base_green ]; then echo 'running 1 test'; echo 'test {RED} ... ok'; exit 0; fi\n\
                   echo 'test {RED} ... FAILED'; echo 'fixture base failure'; exit 1\n\
                 fi\n\
                 if [ '{case}' = candidate_green ] || [ '{case}' = green_prebuild_unavailable ] || [ '{case}' = non_e2e_green ]; then echo 'running 1 test'; echo 'test {RED} ... ok'; exit 0; fi\n\
                 echo 'running 1 test'; sleep 2;;\nesac",
                base = fixture.base,
            ));
            let mut command = cargo_guard_command(
                "demo",
                Some(&format!("test:{target}:test(fresh_install::)")),
            );
            command.timeout = Duration::from_secs(1);
            let spec = GuardSpec {
                commands: vec![command.clone()],
                env: BTreeMap::new(),
                output_tail_bytes: 16384,
            };
            let cache_root = fixture.root.path().join("timeout-cache");
            let mut gate = TestGate::default();
            if case == "known" || case.starts_with("cached_") {
                gate.cache_root = Some(cache_root.clone());
            }
            gate.known_failures = Some(
                serde_json::from_value(serde_json::json!({
                    "schema_version": 1, "exported_at": "2026-10-21T00:00:00Z",
                    "records": [{"record": {"test_id": RED,
                        "matcher": {"contains": ["fixture base failure"]},
                        "issue": 3431, "class": "env"}, "issue_status": "Open"}]
                }))
                .unwrap(),
            );
            if case == "claimed" {
                gate.claimed_fixes.insert(RED.into());
            }
            let old_path = use_fake_path(&fixture);
            if case.starts_with("cached_") {
                let identity = base_cache_identity(
                    &tree,
                    &fixture.base,
                    &command,
                    &shard_env_class(&spec.env, "disk"),
                )
                .unwrap()
                .unwrap();
                let slot = base_cache::BaseShardSlot::acquire(
                    &cache_root,
                    &fixture.base,
                    &identity.slot_label,
                    &identity.slot_key,
                )
                .await
                .unwrap();
                let mut entry = base_cache::BaseShardEntry::new(
                    &fixture.base,
                    &identity.slot_label,
                    &identity.slot_key,
                    BTreeSet::from([RED.into()]),
                    "fixture",
                );
                if case != "cached_old" {
                    entry.failure_texts.insert(
                        RED.into(),
                        if case == "cached_mismatch" {
                            "different failure".into()
                        } else {
                            "fixture base failure".into()
                        },
                    );
                }
                slot.write(&entry).unwrap();
                // Empty evidence is omitted from JSON, exercising the old
                // wire format and the serde default when the gate reads it.
                assert_eq!(slot.read().unwrap(), Some(entry));
            }
            let result = run_affected_gate(
                &fixture.repo,
                &tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            if matches!(case, "known" | "cached_text" | "non_e2e") {
                result.expect("a timeout selecting only a known base red continues");
                assert!(gate.base_reds.contains(RED));
                assert_eq!(
                    known_red_lines(
                        &gate.base_reds,
                        &gate.base_red_texts,
                        gate.known_failures.as_ref()
                    ),
                    [format!("known_red={RED}:#3431:env")]
                );
                if case == "known" {
                    // A fresh gate must restore the persisted text, and a
                    // later run in that same gate must retain it in-process.
                    let mut cached = TestGate {
                        cache_root: Some(cache_root.clone()),
                        known_failures: gate.known_failures.clone(),
                        ..TestGate::default()
                    };
                    for _ in 0..2 {
                        run_affected_gate(
                            &fixture.repo,
                            &tree,
                            &fixture.base,
                            &spec,
                            &mut cached,
                            &BTreeSet::new(),
                        )
                        .await
                        .expect("cached signature evidence excuses the timeout");
                        assert_eq!(cached.base_runs, 0);
                        assert!(cached.base_red_texts[RED].contains("fixture base failure"));
                    }
                }
            } else if matches!(
                case,
                "candidate_green" | "green_prebuild_unavailable" | "non_e2e_green"
            ) {
                result.expect("a green candidate needs no base exemption");
                assert!(gate.green_is_reusable());
                if case == "non_e2e_green" {
                    assert_eq!(gate.base_runs, 0);
                }
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .contains("affected-crate guard failed (TimedOut)"),
                    "{case}"
                );
            }
            if case.starts_with("cached_") {
                assert_eq!(gate.base_runs, 0, "{case} must use the prefilled slot");
                assert!(!gate.base_reused.is_empty());
            }
            restore_path(old_path);
        }
    }

    /// A non-e2e timeout can use a narrowed cache hit only when that outcome
    /// carries the actual base signature; legacy names alone still refuse.
    #[tokio::test]
    async fn narrowed_cached_signature_excuses_only_known_red_timeouts() {
        const RED: &str = "demo::shared_red";
        for case in ["captured", "old", "mismatch"] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn candidate() {}\n",
                "candidate",
            );
            let tree = fixture.root.path().join("narrow-timeout-candidate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            let listing = serde_json::json!({"rust-suites": {"demo": {"testcases": {
                (RED): {"filter-match": {"status": "matches"}, "ignored": false}
            }}}});
            fixture.add_fake_cargo(&format!(
                "case \" $* \" in *' --no-run '*) exit 0;; esac\n\
                 if [ \"$1\" = nextest ] && [ \"$2\" = list ]; then printf '%s\\n' '{listing}'; exit 0; fi\n\
                 if [ \"$1\" = nextest ]; then\n\
                   if [ \"$(/usr/bin/git rev-parse HEAD)\" = '{base}' ]; then\n\
                     echo '        FAIL [   0.010s] (1/1) rsid {RED}'; echo 'fixture base failure'; exit 100;\n\
                   fi\n\
                   echo 'running 1 test'; sleep 2;\n\
                 fi",
                base = fixture.base,
            ));
            let mut command = rsid_lib_filter_command("all()");
            command.timeout = Duration::from_secs(1);
            let spec = GuardSpec {
                commands: vec![command.clone()],
                env: BTreeMap::new(),
                output_tail_bytes: 16384,
            };
            let cache_root = fixture.root.path().join("narrow-timeout-cache");
            let selected = BTreeSet::from([RED.to_owned()]);
            let class = per_test_outcome_class(&command, &shard_env_class(&spec.env, "disk"));
            let old_path = use_fake_path(&fixture);
            if case == "captured" {
                let mut fill = TestGate {
                    cache_root: Some(cache_root.clone()),
                    ..TestGate::default()
                };
                fill.base_red_texts
                    .insert(RED.into(), "earlier command's annotation".into());
                let key = (fixture.base.clone(), base_cache_command_key(&command));
                let evidence = base_evidence_for(
                    &fixture.repo,
                    &tree,
                    &fixture.base,
                    &spec,
                    &command,
                    &mut fill,
                    &BTreeSet::new(),
                    &key,
                    Some(&selected),
                )
                .await
                .unwrap();
                assert!(matches!(evidence, BaseTestEvidence::Observed(names) if names == selected));
                assert_eq!(fill.base_runs, 1);
                assert!(
                    base_cache::load_test_outcomes(&cache_root, &fixture.base, &class)[RED]
                        .failure_text
                        .as_ref()
                        .unwrap()
                        .contains("fixture base failure")
                );
            } else {
                let texts = if case == "old" {
                    BTreeMap::new()
                } else {
                    BTreeMap::from([(RED.into(), "different failure".into())])
                };
                // Empty text is omitted on disk, exactly the legacy format.
                base_cache::record_test_outcomes(
                    &cache_root,
                    &fixture.base,
                    &class,
                    &selected,
                    &BTreeSet::new(),
                    &texts,
                )
                .unwrap();
            }
            let mut gate = TestGate {
                cache_root: Some(cache_root),
                known_failures: Some(
                    serde_json::from_value(serde_json::json!({
                        "schema_version": 1, "exported_at": "2026-10-21T00:00:00Z",
                        "records": [{"record": {"test_id": RED,
                            "matcher": {"contains": ["fixture base failure"]},
                            "issue": 3431, "class": "env"}, "issue_status": "Open"}]
                    }))
                    .unwrap(),
                ),
                ..TestGate::default()
            };
            let result = run_affected_gate(
                &fixture.repo,
                &tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            restore_path(old_path);
            assert_eq!(gate.base_runs, 0, "{case}: only the per-test cache answers");
            assert_eq!(
                gate.base_reused,
                BTreeSet::from([format!("{}:{RED}:per-test", fixture.base)])
            );
            if case == "captured" {
                result.expect("cached base signature excuses the all-known selection's timeout");
                assert_eq!(gate.base_reds, selected);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .contains("affected-crate guard failed (TimedOut)"),
                    "{case}"
                );
            }
        }
    }

    /// Shared test names cannot copy a gate's first-seen text into another
    /// command's or another base's persistent full-result slot.
    #[tokio::test]
    async fn base_slots_persist_each_commands_own_failure_text() {
        const RED: &str = "demo::shared_red";
        let fixture = Fixture::new();
        let other_base = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn other_base() {}\n",
            "other base",
        );
        fixture.add_fake_cargo(&format!(
            "case \" $* \" in *' --test alpha '*) label=alpha;; *' --test beta '*) label=beta;; *) exit 0;; esac\n\
             echo 'test {RED} ... FAILED'\n\
             echo \"own-$label-$(/usr/bin/git rev-parse HEAD)\"; exit 1"
        ));
        let cache_root = fixture.root.path().join("own-text-cache");
        let mut gate = TestGate {
            cache_root: Some(cache_root.clone()),
            ..TestGate::default()
        };
        let old_path = use_fake_path(&fixture);
        for base in [&fixture.base, &other_base] {
            for label in ["alpha", "beta"] {
                let command =
                    cargo_guard_command("demo", Some(&format!("test:{label}:test(demo::)")));
                let spec = GuardSpec {
                    commands: vec![command.clone()],
                    env: BTreeMap::new(),
                    output_tail_bytes: 16384,
                };
                let key = (base.clone(), base_cache_command_key(&command));
                let evidence = base_evidence_for(
                    &fixture.repo,
                    &fixture.repo,
                    base,
                    &spec,
                    &command,
                    &mut gate,
                    &BTreeSet::new(),
                    &key,
                    None,
                )
                .await
                .unwrap();
                assert!(
                    matches!(evidence, BaseTestEvidence::Observed(names) if names == BTreeSet::from([RED.into()]))
                );
                let identity = base_cache_identity(
                    &fixture.repo,
                    base,
                    &command,
                    &shard_env_class(&spec.env, "disk"),
                )
                .unwrap()
                .unwrap();
                let slot = base_cache::BaseShardSlot::acquire(
                    &cache_root,
                    base,
                    &identity.slot_label,
                    &identity.slot_key,
                )
                .await
                .unwrap();
                let entry = slot.read().unwrap().unwrap();
                assert!(
                    entry.failure_texts[RED].contains(&format!("own-{label}-{base}")),
                    "{label} on {base}: {:?}",
                    entry.failure_texts
                );
            }
        }
        restore_path(old_path);
        assert_eq!(gate.base_runs, 4);
    }

    /// #1167: a shard retry spans `-p rsid -p rsid-store`; the package without
    /// the test prints `running 0 tests` even though the other ran it.
    #[test]
    fn a_zero_test_package_does_not_hide_the_package_that_ran_the_test() {
        let name = "remote_read::owner::tests::kept";
        let split =
            "running 1 test\ntest remote_read::owner::tests::kept ... ok\n\nrunning 0 tests\n";
        assert!(!retry_ran_no_test(split, name));
        assert!(!retry_ran_no_test(
            "running 0 tests\nrunning 1 test\n",
            name
        ));
        assert!(retry_ran_no_test(
            "running 0 tests\nrunning 0 tests\n",
            name
        ));
        // No count line at all (a quiet stub) is not evidence of an empty run.
        assert!(!retry_ran_no_test("", name));
    }

    #[test]
    fn module_touch_follows_the_failing_tests_first_path_segment() {
        let changed = |paths: &[&str]| paths.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        let edit = changed(&["crates/rsid/src/session/mod.rs", "docs/a.md"]);
        assert!(source_touches_test_module(Some(&edit), "session::tests::x"));
        assert!(!source_touches_test_module(
            Some(&edit),
            "remote_read::owner::tests::x"
        ));
        let file = changed(&["crates/rsid/src/remote_read/owner.rs"]);
        assert!(source_touches_test_module(
            Some(&file),
            "remote_read::owner::tests::x"
        ));
        let stem = changed(&["crates/rsid/src/remote_read.rs"]);
        assert!(source_touches_test_module(
            Some(&stem),
            "remote_read::owner::tests::x"
        ));
        let manifest = changed(&["crates/rsid/Cargo.toml"]);
        assert!(source_touches_test_module(
            Some(&manifest),
            "remote_read::x"
        ));
        assert!(source_touches_test_module(None, "remote_read::x"));
    }

    /// #1167: a candidate-only failure whose isolated retry cannot run is
    /// retried once more, then reported `unverified` without refusing when the
    /// source does not touch its module, and refused when it does. A real,
    /// reproducible new failure still refuses.
    #[tokio::test]
    async fn an_unrunnable_retry_is_unverified_not_new() {
        for (scenario, touched_file) in [
            ("untouched", "crates/demo/src/elsewhere.rs"),
            ("touched", "crates/demo/src/other.rs"),
        ] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                touched_file,
                "pub fn value() {}\n",
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
            let retries = fixture.root.path().join("retries");
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\nif [ \"$oid\" = '{candidate}' ]; then\n  case \"$*\" in\n    *--exact*) echo x >> '{retries}'; echo 'running 0 tests'; exit 0;;\n    *) echo 'test other::red ... FAILED'; exit 1;;\n  esac\nfi",
                candidate = candidate,
                retries = retries.display(),
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
            let attempts = fs::read_to_string(&retries).unwrap().lines().count();
            assert_eq!(attempts, 2, "{scenario}: one retry plus one more attempt");
            if scenario == "untouched" {
                result.expect("an unverifiable untouched-module failure does not refuse");
                assert!(gate.unverified.contains("other::red"));
                assert!(gate.flakes.is_empty());
            } else {
                let error = result.unwrap_err();
                assert!(error.contains("unverified candidate failure"), "{error}");
                assert!(!error.contains("new test failures"), "{error}");
            }
        }
    }

    const COMPILE_ERROR: &str = "error[E0412]: cannot find type `Missing` in this scope\n --> crates/demo/src/lib.rs:1:15\n  |\n1 | fn broken(_: Missing) {}\n  |               ^^^^^^^ not found\nerror: could not compile `demo` (lib test) due to 1 previous error\n";

    #[test]
    fn baseline_compile_detection_requires_source_diagnostics_and_build_exit() {
        use rsid::integration::{GuardCommandReport, GuardStatus};
        let report = GuardCommandReport {
            program: "cargo".into(),
            args: vec!["test".into()],
            status: GuardStatus::Failed { code: Some(101) },
            stdout_tail: String::new(),
            stderr_tail: COMPILE_ERROR.into(),
            output_truncated: false,
            duration: Duration::ZERO,
        };
        assert!(baseline_compilation_failed(&report));
        let syntax = GuardCommandReport {
            stderr_tail: "error: expected item, found `}`\n --> crates/demo/src/lib.rs:1:1\nerror: could not compile `demo` (lib) due to 1 previous error\n".into(),
            ..report.clone()
        };
        assert!(baseline_compilation_failed(&syntax));
        for code in [1, 4, 100, 102, 104, 105] {
            assert!(!baseline_compilation_failed(&GuardCommandReport {
                status: GuardStatus::Failed { code: Some(code) },
                ..report.clone()
            }));
        }
        for stderr in [
            "error: failed to download dependency: connection refused",
            "error: No space left on device (os error 28)",
            "error: failed to run custom build command for `demo`",
            "error: could not compile `demo` (lib test) due to 1 previous error",
        ] {
            assert!(!baseline_compilation_failed(&GuardCommandReport {
                stderr_tail: stderr.into(),
                ..report.clone()
            }));
        }
        assert!(!baseline_compilation_failed(&GuardCommandReport {
            output_truncated: true,
            ..report.clone()
        }));
        assert!(!baseline_compilation_failed(&GuardCommandReport {
            stdout_tail: "test demo::red ... FAILED".into(),
            ..report
        }));
    }

    #[tokio::test]
    async fn baseline_compile_repair_requires_green_candidate_and_known_inventory() {
        for shape in ["cargo", "shard", "static-red-shard"] {
            for scenario in [
                "repair",
                "candidate-red",
                "candidate-infra",
                "no-tests",
                "unchanged-compile",
                "empty",
                "unknown",
                "infra",
                "unknown-inventory",
                "absent-shard-inventory",
            ] {
                if shape == "cargo" && scenario == "absent-shard-inventory" {
                    continue;
                }
                let fixture = Fixture::new();
                let mut base = fixture.base.clone();
                if shape != "cargo" {
                    base = fixture.commit(&base, "crates/rsid/Cargo.toml",
                        "[package]\nname = \"rsid\"\nversion = \"0.1.0\"\n[features]\ntest-shard-other-02 = []\n", "shard manifest");
                    base = fixture.commit(
                        &base,
                        "Cargo.toml",
                        "[workspace]\nmembers = [\"crates/demo\", \"crates/rsid\"]\n",
                        "shard inventory",
                    );
                    if scenario == "absent-shard-inventory" {
                        base = fixture.commit(
                            &base,
                            "crates/rsid/Cargo.toml",
                            "[package]\nname = \"rsid\"\nversion = \"0.1.0\"\n",
                            "no shard feature",
                        );
                    }
                    base = commit_executable(
                        &fixture,
                        &base,
                        "scripts/run-rsid-test-shards.sh",
                        "#!/bin/sh\nexec cargo nextest run \"$@\"\n",
                    );
                }
                if scenario == "unknown-inventory" {
                    base = fixture.commit(
                        &base,
                        "Cargo.toml",
                        "[workspace]\nmembers = [\"crates/*\"]\n",
                        "unproven inventory",
                    );
                }
                let candidate = fixture.commit(
                    &base,
                    "crates/demo/src/elsewhere.rs",
                    "pub fn repair() {}\n",
                    "repair",
                );
                // Static-red bases still use their direct harness; the candidate
                // must run the shard script including its own inventory check.
                let (base, candidate) = if shape == "static-red-shard" {
                    let red = commit_executable(
                        &fixture,
                        &base,
                        "scripts/run-rsid-test-shards.sh",
                        "#!/bin/sh\necho 'rsid test shard check failed: ungated test' >&2\nexit 1\n",
                    );
                    let fixed = fixture.commit(
                        &candidate,
                        "crates/demo/src/lib.rs",
                        "pub fn fixed() {}\n",
                        "fixed",
                    );
                    (red, fixed)
                } else {
                    (base, candidate)
                };
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
                let log = fixture.root.path().join("repair-runs");
                let base_output = if scenario == "infra" {
                    "error: failed to download dependency: connection refused"
                } else {
                    COMPILE_ERROR
                };
                let candidate_action = match scenario {
                    // #1244: the base is consulted only for a red or unproven
                    // candidate, so the base-side refusals need a red one.
                    "candidate-red" | "infra" | "unknown-inventory" | "absent-shard-inventory" => {
                        "echo 'test elsewhere::red ... FAILED'; exit 1;"
                    }
                    "candidate-infra" => "echo 'error: failed to start runner'; exit 102;",
                    "no-tests" => "echo 'error: no tests to run'; exit 4;",
                    "unchanged-compile" => "printf '%s' \"$compile_error\" >&2; exit 101;",
                    "empty" => "echo 'running 0 tests'; exit 0;",
                    "unknown" => "exit 0;",
                    _ => {
                        if shape == "cargo" {
                            "echo 'running 1 test'; echo 'test demo::repair ... ok'; exit 0;"
                        } else {
                            "echo 'Summary [ 0.01s] 1 test run: 1 passed, 0 skipped'; exit 0;"
                        }
                    }
                };
                fixture.add_fake_cargo(&format!(
                    "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s %s\\n' \"$oid\" \"$*\" >> '{log}'\ncompile_error='{compile_error}'\nif [ \"$oid\" = '{base}' ]; then printf '%s' '{base_output}' >&2; exit 101; fi\ncase \"$*\" in *--exact*) echo 'running 1 test'; exit 0;; esac\n{candidate_action}",
                    log = log.display(), compile_error = COMPILE_ERROR,
                ));
                let command = if shape == "cargo" {
                    cargo_guard_command("demo", Some("bin:demo"))
                } else {
                    rsid_shard_guard_command("other-02:test(repair)").unwrap()
                };
                let spec = GuardSpec {
                    commands: vec![command],
                    env: BTreeMap::new(),
                    output_tail_bytes: 16 * 1024,
                };
                let cache_root = fixture.root.path().join("repair-cache");
                let mut gate = TestGate {
                    cache_root: Some(cache_root.clone()),
                    ..TestGate::default()
                };
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
                if scenario == "repair" {
                    result.expect("compile repair passes with green selected tests");
                    // Retry in-process and in a new gate: neither failure-set
                    // cache may treat unavailable evidence as an observed pass.
                    run_affected_gate(
                        &fixture.repo,
                        &candidate_tree,
                        &base,
                        &spec,
                        &mut gate,
                        &BTreeSet::new(),
                    )
                    .await
                    .unwrap();
                    for tree in gate.base_worktrees.values() {
                        git_run(
                            &fixture.repo,
                            &["worktree", "remove", tree.to_str().unwrap()],
                        );
                    }
                    let mut fresh = TestGate {
                        cache_root: Some(cache_root),
                        ..TestGate::default()
                    };
                    run_affected_gate(
                        &fixture.repo,
                        &candidate_tree,
                        &base,
                        &spec,
                        &mut fresh,
                        &BTreeSet::new(),
                    )
                    .await
                    .unwrap();
                    // #1244: a green, proven candidate never needs the base,
                    // so the broken base is not even built.
                    assert_eq!(fresh.base_runs, 0);
                    assert_eq!(fresh.base_skipped, 1);
                    assert!(fresh.base_reused.is_empty());
                    assert!(fresh.base_cache.is_empty());
                } else {
                    let error = result.unwrap_err();
                    let expected = match scenario {
                        "candidate-red" | "candidate-infra" | "no-tests" | "unchanged-compile" => {
                            "baseline_compile_repair_candidate_failed"
                        }
                        "empty" | "unknown" => "baseline_compile_repair_candidate_evidence_missing",
                        "unknown-inventory" | "absent-shard-inventory" => {
                            "baseline_compile_repair_inventory_unknown"
                        }
                        "infra" => "affected-crate guard failed",
                        _ => unreachable!(),
                    };
                    assert!(error.contains(expected), "{shape}/{scenario}: {error}");
                }
                restore_path(old_path);
                assert!(gate.base_cache.is_empty());
                assert!(gate.base_reds.is_empty());
                assert!(gate.flakes.is_empty());
                assert!(gate.unverified.is_empty());
                let runs = fs::read_to_string(&log).unwrap();
                assert_eq!(
                    runs.lines()
                        .filter(|line| line.starts_with(&candidate))
                        .count(),
                    if scenario == "repair" { 3 } else { 1 },
                    "{shape}/{scenario}: no isolated retry can exempt a candidate red"
                );
                if !matches!(
                    scenario,
                    "repair" | "infra" | "unknown-inventory" | "absent-shard-inventory"
                ) {
                    assert!(gate.base_test_baseline_unavailable.contains(&format!(
                        "{base}:compile_failed:{} {:?}",
                        spec.commands[0].program, spec.commands[0].args
                    )));
                }
            }
        }
    }

    #[tokio::test]
    async fn baseline_compile_repair_requires_all_selected_runs_green_in_either_order() {
        for unavailable_first in [true, false] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn repaired() {}\n",
                "repair",
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
                "case \"$*\" in *known_red*) echo 'test demo::known_red ... FAILED'; exit 1;; esac\nif [ \"$(/usr/bin/git rev-parse HEAD)\" = '{}' ]; then printf '%s' '{}' >&2; exit 101; fi\necho 'running 1 test'\necho 'test demo::repair ... ok'",
                fixture.base, COMPILE_ERROR,
            ));
            let mut commands = vec![
                cargo_guard_command("demo", Some("repair")),
                cargo_guard_command("demo", Some("known_red")),
            ];
            if !unavailable_first {
                commands.reverse();
            }
            let spec = GuardSpec {
                commands,
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
            let error = result.unwrap_err();
            assert!(
                error.contains("baseline_compile_repair_candidate_failed"),
                "{error}"
            );
            assert!(error.contains("known_red"), "{error}");
        }
    }

    #[tokio::test]
    async fn baseline_compile_repair_runs_real_cargo_tests() {
        let fixture = Fixture::new();
        let base = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn broken(_: Missing) {}\n",
            "baseline compile error",
        );
        let candidate = fixture.commit(&base, "crates/demo/src/lib.rs",
            "pub fn repaired() -> usize { 42 }\n#[cfg(test)] mod tests { #[test] fn repair() { assert_eq!(super::repaired(), 42); } }\n", "repair");
        let candidate_tree = fixture.root.path().join("real-candidate");
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
        let spec = GuardSpec {
            commands: vec![
                cargo_check_command("demo"),
                cargo_guard_command("demo", Some("tests::repair")),
            ],
            env: BTreeMap::from([("CARGO_BUILD_JOBS".into(), "2".into())]),
            output_tail_bytes: 16 * 1024,
        };
        let mut gate = TestGate::default();
        run_affected_gate(
            &fixture.repo,
            &candidate_tree,
            &base,
            &spec,
            &mut gate,
            &BTreeSet::new(),
        )
        .await
        .expect("real rustc repair and selected test pass");
        // #1244: the selected test is green and proven, so the uncompilable
        // base is never consulted.
        assert_eq!(gate.base_skipped, 1);
        assert_eq!(gate.base_runs, 0);
        assert!(gate.base_cache.is_empty());
        assert!(gate.base_reds.is_empty());
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

    /// #1132: a filtered shard gate and a `cargo test -p` gate persist their
    /// base result per (base tip, command), so a later batch on the same base
    /// skips the base side; another base tip or another filterset re-runs it.
    #[tokio::test]
    async fn persistent_base_cache_covers_filtered_shards_and_cargo_tests() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let candidate_tree = fixture.root.path().join("filtered-candidate");
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
        let log = fixture.root.path().join("filtered-runs.log");
        let cache_root = fixture.root.path().join("filtered-cache");
        let spec_for = |filter: &str| GuardSpec {
            commands: vec![rsid_shard_guard_command(&format!("store-01:{filter}")).unwrap()],
            env: BTreeMap::from([
                ("RSI_QA899_RUN_LOG".into(), log.display().to_string()),
                ("RSI_QA899_FAIL_ON_OID".into(), "none".into()),
            ]),
            output_tail_bytes: 16 * 1024,
        };
        let base_runs = |oid: &str| {
            fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == oid)
                .count()
        };
        let gate_once = |spec: GuardSpec, base: String, tree: PathBuf| {
            let repo = fixture.repo.clone();
            let cache_root = cache_root.clone();
            async move {
                let mut gate = TestGate {
                    cache_root: Some(cache_root),
                    ..TestGate::default()
                };
                run_affected_gate(&repo, &tree, &base, &spec, &mut gate, &BTreeSet::new())
                    .await
                    .expect("gate");
                for tree in gate.base_worktrees.values() {
                    git_run(
                        &repo,
                        &["worktree", "remove", "--force", tree.to_str().unwrap()],
                    );
                }
                gate
            }
        };
        let first = gate_once(
            spec_for("test(demo)"),
            fixture.base.clone(),
            candidate_tree.clone(),
        )
        .await;
        assert!(first.base_reused.is_empty());
        assert_eq!(base_runs(&fixture.base), 1);
        let second = gate_once(
            spec_for("test(demo)"),
            fixture.base.clone(),
            candidate_tree.clone(),
        )
        .await;
        assert_eq!(
            base_runs(&fixture.base),
            1,
            "same base and filter skip the base side"
        );
        assert!(
            second
                .base_reused
                .contains(&format!("{}:store-01[test(demo)]:lander", fixture.base)),
            "{:?}",
            second.base_reused
        );
        assert_eq!(base_runs(&candidate), 2, "the candidate side still runs");
        gate_once(
            spec_for("test(other)"),
            fixture.base.clone(),
            candidate_tree.clone(),
        )
        .await;
        assert_eq!(
            base_runs(&fixture.base),
            2,
            "another filterset re-runs the base"
        );

        let new_base = fixture.commit(
            &fixture.base,
            "crates/demo/src/other.rs",
            "pub fn other() {}\n",
            "a published batch moves the base",
        );
        let new_candidate = fixture.commit(
            &new_base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate2\" }\n",
            "candidate on the new base",
        );
        let new_tree = fixture.root.path().join("filtered-candidate-2");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                new_tree.to_str().unwrap(),
                &new_candidate,
            ],
        );
        let moved = gate_once(spec_for("test(demo)"), new_base.clone(), new_tree).await;
        assert_eq!(
            base_runs(&new_base),
            1,
            "a different base tip re-runs the base"
        );
        assert!(moved.base_reused.is_empty());
    }

    /// #1132/#1244: the gate compiles the candidate shard binaries up front
    /// (`--no-run`); the base side is never prebuilt, only built when a
    /// candidate failure needs it.
    #[tokio::test]
    async fn gate_prebuilds_only_the_candidate_shard_binaries() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let tree = fixture.root.path().join("prebuild-candidate");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                tree.to_str().unwrap(),
                &candidate,
            ],
        );
        let builds = fixture.root.path().join("prebuilds.log");
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\ncase \"$*\" in *--no-run*) printf '%s %s\\n' \"$oid\" \"$*\" >> '{}';; esac",
            builds.display()
        ));
        let shard_log = fixture.root.path().join("prebuild-shard-runs.log");
        let spec = GuardSpec {
            commands: vec![rsid_shard_guard_command("store-01:test(demo)").unwrap()],
            env: BTreeMap::from([
                ("RSI_QA899_RUN_LOG".into(), shard_log.display().to_string()),
                ("RSI_QA899_FAIL_ON_OID".into(), "none".into()),
            ]),
            output_tail_bytes: 16 * 1024,
        };
        let cache_root = fixture.root.path().join("prebuild-cache");
        let old_path = use_fake_path(&fixture);
        let mut gates = Vec::new();
        for round in 0..2 {
            let mut gate = TestGate {
                cache_root: Some(cache_root.clone()),
                prebuild_forced: true,
                ..TestGate::default()
            };
            run_affected_gate(
                &fixture.repo,
                &tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await
            .expect("gate");
            if round == 0 {
                for path in gate.base_worktrees.values() {
                    git_run(
                        &fixture.repo,
                        &["worktree", "remove", "--force", path.to_str().unwrap()],
                    );
                }
            }
            gates.push(gate);
        }
        restore_path(old_path);
        let log = fs::read_to_string(&builds).unwrap();
        let count = |oid: &str| log.lines().filter(|line| line.starts_with(oid)).count();
        assert_eq!(count(&fixture.base), 0, "{log}");
        assert_eq!(count(&candidate), 2, "{log}");
        assert!(log.contains("--features test-shard-store-01"), "{log}");
        // The fake shard run proves no test ran, so the base is still
        // consulted once and then read from the persistent cache.
        assert_eq!(gates[0].base_runs, 1);
        assert_eq!(gates[1].base_runs, 0);
    }

    #[test]
    fn shard_build_commands_compile_only_the_nextest_shard_binaries() {
        let spec = GuardSpec {
            commands: vec![
                rsid_shard_guard_command("store-01").unwrap(),
                rsid_shard_guard_command("session-02:test(rolling_queue)").unwrap(),
                cargo_guard_command("demo", None),
            ],
            env: BTreeMap::new(),
            output_tail_bytes: 1024,
        };
        let builds = shard_build_commands(&spec, ShardTree::Static);
        // Two nextest shard builds, then the plain `cargo test -p demo` package.
        assert_eq!(builds.len(), 3);
        for build in &builds[..2] {
            assert_eq!(&build.args[..3], ["nextest", "run", "--no-run"]);
        }
        assert!(builds[1].args.join(" ").contains("test-shard-session-02"));
        assert_eq!(builds[2].args, ["test", "--no-run", "-p", "demo", "--lib"]);
    }

    /// #2467: a plain `cargo test -p PKG ...` guard of a non-rsid package is
    /// compiled in the prebuild beside the checks, once per package with every
    /// target it runs and no test-name filter, so the later guard compiles
    /// nothing; rsid and rsid-store stay on the shared nextest build.
    #[test]
    fn package_test_guards_are_prebuilt_once_per_package_without_filters() {
        let spec = GuardSpec {
            commands: vec![
                cargo_guard_command("rsi-common", Some("agent_landing_candidate")),
                cargo_guard_command("rsi-common", Some("test:manifest_dir_guard")),
                cargo_guard_command("rsi-common", Some("bin:tool:test(=exact_name)")),
                cargo_guard_command("rsi-common", Some("other_filter")),
                cargo_guard_command("rsi", Some("test:e2e_tui")),
                cargo_guard_command("rsid", Some("bin:rsi-rolling-land")),
            ],
            env: BTreeMap::new(),
            output_tail_bytes: 1024,
        };
        let builds: Vec<String> = package_test_build_commands(&spec)
            .iter()
            .map(|build| format!("{} {}", build.program, build.args.join(" ")))
            .collect();
        assert_eq!(
            builds,
            [
                "cargo test --no-run -p rsi-common --lib --test manifest_dir_guard --bin tool",
                "cargo test --no-run -p rsi --test e2e_tui",
            ]
        );
        // A gate with only rsid guards adds no package build.
        let rsid_only = GuardSpec {
            commands: vec![cargo_guard_command("rsid", Some("bin:rsid"))],
            env: BTreeMap::new(),
            output_tail_bytes: 1024,
        };
        assert!(package_test_build_commands(&rsid_only).is_empty());
    }

    /// #2833: a red candidate e2e run consults the base, whose private target
    /// has none of the e2e siblings. The base side builds the e2e prebuild
    /// into that target before its e2e test command, so the base observes the
    /// tests rather than "e2e prebuild incomplete" for every one of them.
    #[tokio::test]
    async fn the_base_side_of_an_e2e_test_runs_the_e2e_prebuild_first() {
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
        let log = fixture.root.path().join("e2e-runs");
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\t%s\\t%s\\n' \"$oid\" \"$CARGO_TARGET_DIR\" \"$*\" >> '{log}'\nif [ \"$1\" = test ] && [ \"$oid\" = '{candidate}' ]; then echo 'test editing_mode::red ... FAILED'; exit 1; fi",
            log = log.display(),
        ));
        let spec = GuardSpec {
            commands: vec![cargo_guard_command("demo", Some("test:e2e_tui"))],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let mut gate = TestGate::default();
        let old_path = use_fake_path(&fixture);
        let _ = run_affected_gate(
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
        let base_runs: Vec<(&str, &str)> = runs
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(3, '\t');
                let oid = fields.next()?;
                let target = fields.next()?;
                let args = fields.next()?;
                (oid == fixture.base).then_some((target, args))
            })
            .collect();
        let prebuild = e2e_prebuild_command().args.join(" ");
        let prebuild_at = base_runs
            .iter()
            .position(|(_, args)| *args == prebuild)
            .unwrap_or_else(|| panic!("no base e2e prebuild in {runs}"));
        let test_at = base_runs
            .iter()
            .position(|(_, args)| args.starts_with("test ") && args.contains("e2e_tui"))
            .unwrap_or_else(|| panic!("no base e2e test in {runs}"));
        assert!(prebuild_at < test_at, "{runs}");
        assert_eq!(
            base_runs[prebuild_at].0, base_runs[test_at].0,
            "the prebuild and the test share the base target: {runs}"
        );
        assert_eq!(
            base_runs
                .iter()
                .filter(|(_, args)| *args == prebuild)
                .count(),
            1,
            "the base prebuild runs once per base: {runs}"
        );
        assert_eq!(gate.base_e2e_prebuilt.get(&fixture.base), Some(&None));
    }

    /// #2863: a base e2e prebuild that fails leaves the base without its
    /// siblings, so the base e2e run fails with "e2e prebuild incomplete" for
    /// every test. That is unavailable evidence, never a base red: the
    /// candidate's own red stays a new failure and the gate refuses it.
    #[tokio::test]
    async fn a_failed_base_e2e_prebuild_excuses_no_candidate_red() {
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
        // The base build fails; the base test (as the harness would without
        // its siblings) reports the very test the candidate also fails.
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\nif [ \"$1\" = build ] && [ \"$oid\" = '{base}' ]; then echo 'error: linker failed' >&2; exit 1; fi\nif [ \"$1\" = test ]; then echo 'test editing_mode::red ... FAILED'; echo 'e2e prebuild incomplete'; exit 1; fi",
            base = fixture.base,
        ));
        let spec = GuardSpec {
            commands: vec![cargo_guard_command("demo", Some("test:e2e_tui"))],
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
        let error = result.expect_err("the candidate red must not be excused");
        assert!(
            error.contains("baseline_compile_repair_candidate_failed"),
            "{error}"
        );
        assert!(gate.base_reds.is_empty(), "{:?}", gate.base_reds);
        assert!(
            gate.base_test_baseline_unavailable
                .iter()
                .any(|entry| entry.contains(&format!("{}:e2e_prebuild_failed:", fixture.base))),
            "{:?}",
            gate.base_test_baseline_unavailable
        );
        assert!(
            matches!(gate.base_e2e_prebuilt.get(&fixture.base), Some(Some(_))),
            "{:?}",
            gate.base_e2e_prebuilt
        );
        assert!(!gate.green_is_reusable());
    }

    #[tokio::test]
    async fn persistent_base_cache_reuses_cargo_test_base_results() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let candidate_tree = fixture.root.path().join("cargo-cache-candidate");
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
        let log = fixture.root.path().join("cargo-cache-runs");
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\nif [ \"$oid\" = '{base}' ]; then echo 'test demo::red ... FAILED'; exit 1; fi",
            log = log.display(),
            base = fixture.base,
        ));
        let spec = GuardSpec {
            commands: vec![cargo_guard_command("demo", None)],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let cache_root = fixture.root.path().join("cargo-cache");
        let old_path = use_fake_path(&fixture);
        let mut reds = Vec::new();
        for _ in 0..2 {
            let mut gate = TestGate {
                cache_root: Some(cache_root.clone()),
                ..TestGate::default()
            };
            let result = run_affected_gate(
                &fixture.repo,
                &candidate_tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            result.expect("an identical base red is allowed");
            reds.push(gate.base_reds.clone());
        }
        restore_path(old_path);
        let lines = fs::read_to_string(log).unwrap();
        assert_eq!(lines.lines().filter(|oid| *oid == fixture.base).count(), 1);
        assert_eq!(lines.lines().filter(|oid| *oid == candidate).count(), 2);
        assert!(reds.iter().all(|red| red.contains("demo::red")), "{reds:?}");
    }

    fn receipt_test_options(fixture: &Fixture) -> Options {
        Options {
            repo: fixture.repo.clone(),
            remote: "origin".into(),
            accepted: Vec::new(),
            test_filters: Vec::new(),
            cargo_build_jobs: 1,
            remote_gate: None,
            tmpfs_min_free_gb: 12,
            disk_scratch_shards: BTreeSet::new(),
            expected_tip: None,
            fixes_tests: Vec::new(),
        }
    }

    fn receipt_files(root: &Path) -> usize {
        fs::read_dir(root).map_or(0, |trees| {
            trees
                .flatten()
                .filter_map(|tree| fs::read_dir(tree.path()).ok())
                .flat_map(|files| files.flatten())
                .filter(|file| file.path().extension().is_some_and(|ext| ext == "json"))
                .count()
        })
    }

    /// #1312: a green candidate gate leaves a receipt that answers a later
    /// identical gate; any difference in what the verdict depends on, or a gate
    /// that is not the candidate's first full gate, runs the tests again.
    #[tokio::test]
    async fn green_candidate_receipt_answers_only_an_identical_later_gate() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let candidate_tree = fixture.root.path().join("receipt-candidate");
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
        let log = fixture.root.path().join("receipt-runs");
        fixture.add_fake_cargo(&format!(
            "if [ \"$1\" = -V ]; then echo 'cargo fake'; exit 0; fi\noid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\nif [ \"$oid\" = '{base}' ]; then echo 'test demo::red ... FAILED'; exit 1; fi",
            log = log.display(),
            base = fixture.base,
        ));
        let spec = GuardSpec {
            commands: vec![cargo_guard_command("demo", None)],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let options = receipt_test_options(&fixture);
        let receipts = fixture.root.path().join("receipts");
        let candidate_runs = |log: &Path| {
            fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .filter(|oid| *oid == candidate)
                .count()
        };
        let fresh_worktree = |name: &str| {
            // A gate that runs the base leaves its worktree beside the repo.
            let base_tree = fixture.root.path().join(format!("base-{}", fixture.base));
            if base_tree.exists() {
                git_run(
                    &fixture.repo,
                    &["worktree", "remove", "--force", base_tree.to_str().unwrap()],
                );
            }
            let parent = fixture.root.path().join(name);
            fs::create_dir_all(&parent).unwrap();
            let tree = parent.join("candidate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            tree
        };
        let old_path = use_fake_path(&fixture);
        let gate_with = |enabled: bool| TestGate {
            receipt_root: Some(receipts.clone()),
            candidate_receipts: enabled,
            ..TestGate::default()
        };
        let mut first = gate_with(true);
        run_affected_gate_receipted(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &candidate,
            &spec,
            &mut first,
            &options,
        )
        .await
        .expect("first gate is green");
        let ran_first = candidate_runs(&log);
        assert!(ran_first >= 1, "the first gate ran the candidate");
        assert_eq!(receipt_files(&receipts), 1, "the green gate left a receipt");

        // The same candidate, base, gate and environment: no test runs, and
        // the report state is the original run's.
        let mut second = gate_with(true);
        run_affected_gate_receipted(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &candidate,
            &spec,
            &mut second,
            &options,
        )
        .await
        .expect("receipt answers the gate");
        assert_eq!(candidate_runs(&log), ran_first, "nothing ran again");
        assert_eq!(second.base_reds, first.base_reds);
        assert_eq!(second.base_red_commands, first.base_red_commands);
        assert_eq!(second.flakes, first.flakes);
        assert!(
            second.base_reds.contains("demo::red"),
            "{:?}",
            second.base_reds
        );

        // A different gate (another command), another base commit, other
        // filters and claims, and a gate that may not use receipts all run.
        let other_spec = GuardSpec {
            commands: vec![cargo_guard_command("demo", Some("test:other"))],
            ..spec.clone()
        };
        let mut other_gate = gate_with(true);
        let before = candidate_runs(&log);
        run_affected_gate_receipted(
            &fixture.repo,
            &fresh_worktree("other"),
            &fixture.base,
            &candidate,
            &other_spec,
            &mut other_gate,
            &options,
        )
        .await
        .expect("another gate is green");
        assert!(candidate_runs(&log) > before, "another gate runs");
        let mut filtered = receipt_test_options(&fixture);
        filtered.test_filters = vec!["demo=anything".into()];
        let before = candidate_runs(&log);
        let mut filtered_gate = gate_with(true);
        run_affected_gate_receipted(
            &fixture.repo,
            &fresh_worktree("filtered"),
            &fixture.base,
            &candidate,
            &spec,
            &mut filtered_gate,
            &filtered,
        )
        .await
        .expect("other filters are green");
        assert!(candidate_runs(&log) > before, "other filters run");
        let before = candidate_runs(&log);
        let mut claimed_gate = gate_with(true);
        claimed_gate.claimed_fixes = BTreeSet::from(["demo::red".to_owned()]);
        let _ = run_affected_gate_receipted(
            &fixture.repo,
            &fresh_worktree("claimed"),
            &fixture.base,
            &candidate,
            &spec,
            &mut claimed_gate,
            &options,
        )
        .await;
        assert!(candidate_runs(&log) > before, "other claims run");
        let before = candidate_runs(&log);
        let mut disabled = gate_with(false);
        run_affected_gate_receipted(
            &fixture.repo,
            &fresh_worktree("disabled"),
            &fixture.base,
            &candidate,
            &spec,
            &mut disabled,
            &options,
        )
        .await
        .expect("a gate that may not use receipts is green");
        assert!(candidate_runs(&log) > before, "a stale or canary gate runs");
        let before = candidate_runs(&log);
        let mut stale = gate_with(true);
        stale.reuse = Some(GateReuse::every_test("fixture"));
        let _ = run_affected_gate_receipted(
            &fixture.repo,
            &fresh_worktree("stale"),
            &fixture.base,
            &candidate,
            &spec,
            &mut stale,
            &options,
        )
        .await;
        restore_path(old_path);
        assert_eq!(
            candidate_runs(&log),
            before,
            "a reduced stale gate is not a full gate and its reused tests are not re-run"
        );
        assert_eq!(
            receipt_files(&receipts),
            3,
            "one receipt per distinct gate that passed"
        );
    }

    /// #1312: a red gate leaves no receipt.
    #[tokio::test]
    async fn red_candidate_gate_leaves_no_receipt() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let candidate_tree = fixture.root.path().join("red-candidate");
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
            "if [ \"$1\" = -V ]; then echo 'cargo fake'; exit 0; fi\noid=$(/usr/bin/git rev-parse HEAD)\nif [ \"$oid\" = '{candidate}' ]; then echo 'test demo::new_red ... FAILED'; exit 1; fi",
        ));
        let spec = GuardSpec {
            commands: vec![cargo_guard_command("demo", None)],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let receipts = fixture.root.path().join("receipts");
        let old_path = use_fake_path(&fixture);
        let mut gate = TestGate {
            receipt_root: Some(receipts.clone()),
            candidate_receipts: true,
            ..TestGate::default()
        };
        let result = run_affected_gate_receipted(
            &fixture.repo,
            &candidate_tree,
            &fixture.base,
            &candidate,
            &spec,
            &mut gate,
            &receipt_test_options(&fixture),
        )
        .await;
        restore_path(old_path);
        assert!(result.is_err(), "a new candidate failure refuses the gate");
        assert_eq!(receipt_files(&receipts), 0);
    }

    /// Inject admission rather than asking the live host on each retry. The
    /// fake cargo does no real build, so a private directory models admitted
    /// tmpfs without requiring /dev/shm or depending on its current capacity.
    fn receipt_reuse_gate(fixture: &Fixture, receipts: &Path, tmpfs_admitted: bool) -> TestGate {
        let scratch = if tmpfs_admitted {
            GateScratch::Tmpfs(tempfile::tempdir_in(fixture.root.path()).expect("fixture tmpfs"))
        } else {
            let path = fixture.root.path().join("disk-scratch");
            fs::create_dir_all(&path).expect("fixture disk scratch");
            GateScratch::Disk(path)
        };
        TestGate {
            receipt_root: Some(receipts.to_path_buf()),
            scratch_override: Some(scratch),
            ..TestGate::default()
        }
    }

    /// #1312: publication fails after a green gate; the next invocation on the
    /// same content reuses the verdict, still publishes by fast-forward and
    /// still reports the original gate's evidence.
    #[tokio::test]
    async fn failed_publication_leaves_a_receipt_the_next_landing_reuses() {
        for tmpfs_admitted in [true, false] {
            failed_publication_leaves_a_receipt_the_next_landing_reuses_with_scratch(
                tmpfs_admitted,
            )
            .await;
        }
    }

    async fn failed_publication_leaves_a_receipt_the_next_landing_reuses_with_scratch(
        tmpfs_admitted: bool,
    ) {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"source\" }\n",
            "source crate",
        );
        let log = fixture.root.path().join("tested-commits");
        fixture.add_fake_cargo(&format!(
            "if [ \"$1\" = -V ]; then echo 'cargo fake'; exit 0; fi\noid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'",
            log = log.display(),
        ));
        // The decline hangs on a flag file, not on the `git` file itself: a
        // replaced PATH executable is part of the receipt key (#2738).
        let decline = fixture.root.path().join("decline-push");
        fs::write(&decline, "").expect("decline flag");
        fixture.add_fake_git_behaviors(
            "",
            &format!(
                "if [ -e '{flag}' ]; then echo 'declined by a pre-receive hook' >&2; exit 1; fi\nexec /usr/bin/git \"$@\"",
                flag = decline.display()
            ),
        );
        let receipts = fixture.root.path().join("receipts");
        let accepted = vec![AcceptedPair {
            base: fixture.base.clone(),
            source: source.clone(),
        }];
        let options = |accepted: Vec<AcceptedPair>| Options {
            repo: fixture.repo.clone(),
            remote: "origin".into(),
            accepted,
            test_filters: Vec::new(),
            cargo_build_jobs: 1,
            remote_gate: None,
            tmpfs_min_free_gb: 12,
            disk_scratch_shards: BTreeSet::new(),
            expected_tip: None,
            fixes_tests: Vec::new(),
        };
        let old_path = use_fake_path(&fixture);
        let failed = land_with_gate(
            options(accepted.clone()),
            receipt_reuse_gate(&fixture, &receipts, tmpfs_admitted),
        )
        .await;
        let failure = failed.expect_err("the push is declined");
        assert_eq!(failure.state, PublicationState::NotPublished);
        assert_eq!(receipt_files(&receipts), 1, "the green gate left a receipt");
        let gated = fs::read_to_string(&log).unwrap();
        assert!(gated.lines().any(|oid| oid == source), "{gated}");

        fs::remove_file(&decline).expect("let the push through");
        let report = land_with_gate(
            options(accepted),
            receipt_reuse_gate(&fixture, &receipts, tmpfs_admitted),
        )
        .await;
        restore_path(old_path);
        let report = report.expect("the reused verdict publishes");
        let expected_class = if tmpfs_admitted { "tmpfs:" } else { "disk:" };
        assert!(
            report.gate_scratch.starts_with(expected_class),
            "{}",
            report.gate_scratch
        );
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            gated,
            "no test ran for the second landing"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip,
            "publication stayed a live fast-forward"
        );
        assert!(
            report.canary_reused_tree.is_some(),
            "the reused gate counts as the candidate's gate for the canary"
        );
    }

    /// #2728: when rolling advanced past the accepted base, each invocation
    /// makes a new merge commit (a new commit time, so a new SHA) of the same
    /// content. A retry after a declined publication still reuses the verdict.
    #[tokio::test]
    async fn failed_publication_of_a_merge_candidate_leaves_a_receipt_the_retry_reuses() {
        for tmpfs_admitted in [true, false] {
            failed_publication_of_a_merge_candidate_leaves_a_receipt_the_retry_reuses_with_scratch(
                tmpfs_admitted,
            )
            .await;
        }
    }

    async fn failed_publication_of_a_merge_candidate_leaves_a_receipt_the_retry_reuses_with_scratch(
        tmpfs_admitted: bool,
    ) {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"source\" }\n",
            "source crate",
        );
        let advanced = fixture.commit(&fixture.base, "other.txt", "other\n", "rolling moved");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{advanced}:refs/heads/rolling"),
            ],
        );
        let log = fixture.root.path().join("tested-commits");
        fixture.add_fake_cargo(&format!(
            "if [ \"$1\" = -V ]; then echo 'cargo fake'; exit 0; fi\noid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'",
            log = log.display(),
        ));
        // The decline hangs on a flag file, not on the `git` file itself: a
        // replaced PATH executable is part of the receipt key (#2738).
        let decline = fixture.root.path().join("decline-push");
        fs::write(&decline, "").expect("decline flag");
        fixture.add_fake_git_behaviors(
            "",
            &format!(
                "if [ -e '{flag}' ]; then echo 'declined by a pre-receive hook' >&2; exit 1; fi\nexec /usr/bin/git \"$@\"",
                flag = decline.display()
            ),
        );
        let receipts = fixture.root.path().join("receipts");
        let accepted = vec![AcceptedPair {
            base: fixture.base.clone(),
            source: source.clone(),
        }];
        let options = |accepted: Vec<AcceptedPair>| Options {
            accepted,
            ..receipt_test_options(&fixture)
        };
        let gate = || receipt_reuse_gate(&fixture, &receipts, tmpfs_admitted);
        let old_path = use_fake_path(&fixture);
        let failed = land_with_gate(options(accepted.clone()), gate()).await;
        let failure = failed.expect_err("the push is declined");
        assert_eq!(failure.state, PublicationState::NotPublished);
        let first_candidate = failure.candidate.expect("the failure names its candidate");
        assert_ne!(first_candidate, source, "rolling moved, so this is a merge");
        assert_eq!(receipt_files(&receipts), 1, "the green gate left a receipt");
        let gated = fs::read_to_string(&log).unwrap();
        assert!(gated.lines().any(|oid| oid == first_candidate), "{gated}");

        // The retry's merge commit gets a later commit time, hence a new SHA.
        std::thread::sleep(Duration::from_millis(1100));
        fs::remove_file(&decline).expect("let the push through");
        let report = land_with_gate(options(accepted), gate()).await;
        restore_path(old_path);
        let report = report.expect("the reused verdict publishes");
        let expected_class = if tmpfs_admitted { "tmpfs:" } else { "disk:" };
        assert!(
            report.gate_scratch.starts_with(expected_class),
            "{}",
            report.gate_scratch
        );
        assert_ne!(
            report.candidate, first_candidate,
            "the retry made a different merge commit of the same content"
        );
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            gated,
            "no test ran for the retried merge landing"
        );
        assert_eq!(
            receipt_files(&receipts),
            1,
            "the retry reused the one receipt"
        );
        assert!(report.canary_reused_tree.is_some());
    }

    /// #2728: the candidate commit is not part of a command's receipt identity,
    /// but the base, the tree and every other argument still are.
    #[test]
    fn receipt_identity_keys_the_tree_and_base_not_the_candidate_commit() {
        let candidate = "1".repeat(40);
        let remade = "2".repeat(40);
        let base = "b".repeat(40);
        let spec_for = |candidate: &str, base: &str| GuardSpec {
            commands: vec![GuardCommand {
                program: "git".into(),
                args: vec![
                    "diff".into(),
                    "--check".into(),
                    base.into(),
                    candidate.into(),
                ],
                timeout: Duration::from_secs(30),
            }],
            env: BTreeMap::new(),
            output_tail_bytes: 1,
        };
        let options = Options {
            repo: PathBuf::new(),
            remote: "origin".into(),
            accepted: Vec::new(),
            test_filters: Vec::new(),
            cargo_build_jobs: 1,
            remote_gate: None,
            tmpfs_min_free_gb: 12,
            disk_scratch_shards: BTreeSet::new(),
            expected_tip: None,
            fixes_tests: Vec::new(),
        };
        let key = |tree: &str,
                   base: &str,
                   candidate: &str,
                   env: &BTreeMap<String, String>,
                   scratch: &str| {
            candidate_receipt::key_of(&receipt_identity(
                tree,
                base,
                candidate,
                env,
                "",
                &spec_for(candidate, base),
                &options,
                scratch,
                SlowTier::Excluded,
                &BTreeSet::new(),
                "toolchain",
                "lander",
            ))
        };
        let none = BTreeMap::new();
        let original = key("tree-a", &base, &candidate, &none, "disk");
        assert_eq!(
            original,
            key("tree-a", &base, &remade, &none, "disk"),
            "a re-made merge of the same tree and base is the same gate"
        );
        assert_ne!(
            original,
            key("tree-a", &base, &candidate, &none, "tmpfs"),
            "a changed scratch class invalidates the receipt"
        );
        assert_ne!(original, key("tree-b", &base, &candidate, &none, "disk"));
        assert_ne!(
            original,
            key("tree-a", &"c".repeat(40), &candidate, &none, "disk")
        );
    }

    /// #2729: the inherited compiler, target and test settings are part of the
    /// key; locations, throttles and anything credential-like are not.
    #[test]
    fn receipt_identity_keys_the_inherited_build_environment() {
        let vars = |pairs: &[(&str, &str)]| {
            receipt_inherited_env(
                pairs
                    .iter()
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect::<Vec<(std::ffi::OsString, std::ffi::OsString)>>(),
            )
        };
        let spec = GuardSpec {
            commands: Vec::new(),
            env: BTreeMap::new(),
            output_tail_bytes: 1,
        };
        let options = Options {
            repo: PathBuf::new(),
            remote: "origin".into(),
            accepted: Vec::new(),
            test_filters: Vec::new(),
            cargo_build_jobs: 1,
            remote_gate: None,
            tmpfs_min_free_gb: 12,
            disk_scratch_shards: BTreeSet::new(),
            expected_tip: None,
            fixes_tests: Vec::new(),
        };
        let key = |env: &BTreeMap<String, String>| {
            candidate_receipt::key_of(&receipt_identity(
                "tree",
                "base",
                "candidate",
                env,
                "",
                &spec,
                &options,
                "disk",
                SlowTier::Excluded,
                &BTreeSet::new(),
                "toolchain",
                "lander",
            ))
        };
        for name in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_TARGET",
            "RUSTDOCFLAGS",
        ] {
            let before = key(&vars(&[(name, "one")]));
            assert_eq!(
                before,
                key(&vars(&[(name, "one")])),
                "{name} unchanged hits"
            );
            assert_ne!(
                before,
                key(&vars(&[(name, "two")])),
                "{name} changed misses"
            );
            assert_ne!(before, key(&vars(&[])), "{name} unset misses");
        }
        let plain = key(&vars(&[]));
        for (name, value) in [
            ("CARGO_TARGET_DIR", "/elsewhere"),
            ("CARGO_BUILD_JOBS", "3"),
            ("NEXTEST_TEST_THREADS", "9"),
            ("CARGO_REGISTRY_TOKEN", "secret"),
            ("CARGO_REGISTRIES_PRIVATE_TOKEN", "secret"),
            ("RSI_SESSION_TOKEN", "secret"),
            ("HOME", "/home/other"),
        ] {
            assert_eq!(
                plain,
                key(&vars(&[(name, value)])),
                "{name} is not part of the key"
            );
        }
        let captured = vars(&[
            ("RUSTFLAGS", "-Dwarnings"),
            ("CARGO_REGISTRY_TOKEN", "secret"),
        ]);
        assert_eq!(
            captured,
            BTreeMap::from([("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned())])
        );
    }

    /// #2739: the executable suffixes (`PATHEXT` on Windows) are tried so the
    /// keyed file is the one a spawn runs; a miss stays unresolved.
    #[test]
    fn resolve_on_path_expands_executable_suffixes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().to_path_buf();
        let path = dir.as_os_str().to_owned();
        let suffixes = parse_pathext(Some(std::ffi::OsStr::new(".COM;.EXE;cmd")));
        assert_eq!(suffixes, [".COM", ".EXE", ".cmd"]);
        assert_eq!(
            parse_pathext(None),
            [".exe", ".cmd", ".bat", ".com"],
            "unset PATHEXT falls back to the defaults"
        );
        assert_eq!(parse_pathext(Some(std::ffi::OsStr::new(""))).len(), 4);

        let digest = |suffixes: &[String]| {
            resolve_on_path("cargo", &path, suffixes)
                .and_then(|resolved| file_sha256(&resolved))
                .unwrap_or_else(|| "unresolved".into())
        };
        let exe = vec![".exe".to_owned(), ".cmd".to_owned()];
        assert_eq!(digest(&exe), "unresolved", "no file with any suffix");
        std::fs::write(dir.join("cargo.exe"), "one").unwrap();
        assert_eq!(
            resolve_on_path("cargo", &path, &exe),
            Some(dir.join("cargo.exe"))
        );
        let first = digest(&exe);
        assert_ne!(first, "unresolved");
        std::fs::write(dir.join("cargo.exe"), "two").unwrap();
        assert_ne!(
            first,
            digest(&exe),
            "a replaced cargo.exe changes the digest"
        );
        // Suffix order wins; a name with an extension is tried as is first.
        std::fs::write(dir.join("cargo.cmd"), "cmd").unwrap();
        assert_eq!(
            resolve_on_path("cargo", &path, &exe),
            Some(dir.join("cargo.exe"))
        );
        assert_eq!(
            resolve_on_path("cargo.cmd", &path, &exe),
            Some(dir.join("cargo.cmd"))
        );
        // Without suffixes (Linux) only the exact name resolves.
        assert_eq!(resolve_on_path("cargo", &path, &[]), None);
        assert_eq!(
            resolve_on_path("cargo.exe", &path, &[]),
            Some(dir.join("cargo.exe"))
        );
        // A tree path is not resolved through PATH.
        assert_eq!(resolve_on_path("./cargo", &path, &exe), None);
    }

    /// #2738: the executables a guard resolves through `PATH` and the Cargo
    /// configuration under `CARGO_HOME` are part of the key by content.
    #[cfg(unix)]
    #[test]
    fn receipt_tool_inputs_key_resolved_executables_and_cargo_config() {
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let home = root.path().join("cargo-home");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let write_tool = |name: &str, body: &str| {
            let path = bin.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        write_tool("cargo", "#!/bin/sh\necho cargo 1.0\n");
        write_tool("rustc", "#!/bin/sh\necho rustc 1.0\n");
        write_tool("python3", "#!/bin/sh\necho Python 3\n");
        let spec = GuardSpec {
            commands: vec![GuardCommand {
                program: "cargo".into(),
                args: vec!["test".into()],
                timeout: std::time::Duration::from_secs(1),
            }],
            env: BTreeMap::new(),
            output_tail_bytes: 1,
        };
        let path_value = bin.as_os_str().to_owned();
        let home_value = home.as_os_str().to_owned();
        let inputs = |path: &OsString, cargo_home: &OsString, spec: &GuardSpec| {
            receipt_tool_inputs(
                &|name| match name {
                    "PATH" => Some(path.clone()),
                    "CARGO_HOME" => Some(cargo_home.clone()),
                    _ => None,
                },
                spec,
                None,
            )
        };
        let original = inputs(&path_value, &home_value, &spec);
        assert_eq!(
            original,
            inputs(&path_value, &home_value, &spec),
            "unchanged PATH and CARGO_HOME hit"
        );

        // A replaced executable at the same PATH entry that reports the same
        // `-V` still misses: the digest is of the file, not its version.
        write_tool("cargo", "#!/bin/sh\necho cargo 1.0\n# rebuilt\n");
        let replaced = inputs(&path_value, &home_value, &spec);
        assert_ne!(original, replaced, "changed PATH executable misses");

        // Another directory first on PATH resolves another executable.
        let other = root.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("cargo"), "#!/bin/sh\necho other\n").unwrap();
        let shadowed = std::env::join_paths([other.clone(), bin.clone()]).unwrap();
        assert_ne!(replaced, inputs(&shadowed, &home_value, &spec));

        // A spec override of PATH wins over the inherited value.
        let mut overridden = spec.clone();
        overridden
            .env
            .insert("PATH".into(), shadowed.to_string_lossy().into_owned());
        assert_eq!(
            inputs(&shadowed, &home_value, &spec),
            inputs(&path_value, &home_value, &overridden)
        );

        // Cargo configuration: absent, present, changed, and at another home.
        let none = inputs(&path_value, &home_value, &spec);
        std::fs::write(home.join("config.toml"), "[build]\nrustflags = []\n").unwrap();
        let configured = inputs(&path_value, &home_value, &spec);
        assert_ne!(none, configured, "a new Cargo config misses");
        assert_eq!(configured, inputs(&path_value, &home_value, &spec));
        std::fs::write(
            home.join("config.toml"),
            "[build]\nrustflags = [\"-Cfoo\"]\n",
        )
        .unwrap();
        assert_ne!(
            configured,
            inputs(&path_value, &home_value, &spec),
            "a changed Cargo config misses"
        );
        let other_home = root.path().join("other-home");
        std::fs::create_dir_all(&other_home).unwrap();
        std::fs::write(other_home.join("config"), "legacy").unwrap();
        assert_ne!(
            configured,
            inputs(&path_value, &other_home.as_os_str().to_owned(), &spec)
        );
        // An identical config under a different CARGO_HOME location hits.
        let twin = root.path().join("twin-home");
        std::fs::create_dir_all(&twin).unwrap();
        std::fs::copy(home.join("config.toml"), twin.join("config.toml")).unwrap();
        assert_eq!(
            inputs(&path_value, &home_value, &spec),
            inputs(&path_value, &twin.as_os_str().to_owned(), &spec)
        );

        // The inputs reach the receipt key.
        let options = Options {
            repo: PathBuf::new(),
            remote: "origin".into(),
            accepted: Vec::new(),
            test_filters: Vec::new(),
            cargo_build_jobs: 1,
            remote_gate: None,
            tmpfs_min_free_gb: 12,
            disk_scratch_shards: BTreeSet::new(),
            expected_tip: None,
            fixes_tests: Vec::new(),
        };
        let key = |tool_inputs: &str| {
            candidate_receipt::key_of(&receipt_identity(
                "tree",
                "base",
                "candidate",
                &BTreeMap::new(),
                tool_inputs,
                &spec,
                &options,
                "disk",
                SlowTier::Excluded,
                &BTreeSet::new(),
                "toolchain",
                "lander",
            ))
        };
        assert_eq!(key(&original), key(&original));
        assert_ne!(key(&original), key(&replaced));
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
                    // #2703: the isolated local-confirmation command is recorded.
                    let command = gate.base_red_commands.get("demo::red");
                    assert!(
                        command.is_some_and(|command| command.contains("demo::red")),
                        "{command:?}"
                    );
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
        let host_class = with_lander_vars([("RSI_QA_HOST_CLASS", "desktop")]);
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
        drop(host_class);
        let host_class = with_lander_vars([("RSI_QA_HOST_CLASS", "cloud")]);
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
        drop(host_class);
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
                    // A name the source has (#1243 refuses one it lacks).
                    .land_with_filters(vec![pair], vec!["demo=value".into()])
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

    /// #1243: a filter that provably selects no test is refused before the
    /// gate builds or runs anything.
    #[tokio::test]
    async fn a_filter_selecting_no_test_is_refused_before_any_build() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"changed\" }\n",
            "change",
        );
        let log = fixture.root.path().join("cargo-calls");
        fixture.add_fake_cargo(&format!("printf '%s\\n' \"$1\" >> '{}'", log.display()));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_filters(
                vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source,
                }],
                vec!["demo=no_such_test_anywhere".into()],
            )
            .await;
        restore_path(old_path);
        let failure = result.expect_err("an empty filter is refused");
        assert_eq!(failure.state, PublicationState::NotPublished);
        assert!(
            failure.message.contains("selects no test")
                && failure.message.contains("demo=no_such_test_anywhere"),
            "{failure:?}"
        );
        let calls = fs::read_to_string(&log).unwrap_or_default();
        assert!(
            calls.lines().all(|call| call == "metadata"),
            "no build ran: {calls}"
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

    /// #2059: a failing `#[should_panic]` test is printed `NAME - should panic`
    /// by libtest; its identity is `NAME`, a plain name the narrowed base run
    /// and the exact retry both accept.
    #[test]
    fn should_panic_failures_are_named_without_the_libtest_suffix() {
        let output = "    FAIL [ 0.1s] ( 1/2) rsid-store store::tests::rewind\ntest store::tests::rewind - should panic ... FAILED\ntest store::tests::plain ... FAILED\n";
        let names = test_failure_names(output);
        assert_eq!(
            names,
            BTreeSet::from([
                "store::tests::plain".to_string(),
                "store::tests::rewind".to_string(),
            ])
        );
        assert!(names.iter().all(|name| plain_test_name(name)));
        assert!(narrowed_base_command(&rsid_lib_filter_command("all()"), &names).is_some());
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
            ShardTree::Static,
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

    /// #3478: a gate test command runs in its own PID namespace, under the
    /// build slot, and only when the tree under test ships the helper; static
    /// guards, git checks and `cargo check` run as before.
    #[test]
    fn gate_test_commands_run_in_a_pid_namespace_inside_the_build_slot() {
        let tree = tempfile::tempdir().unwrap();
        let bare = tempfile::tempdir().unwrap();
        fs::create_dir_all(tree.path().join("scripts")).unwrap();
        fs::write(
            tree.path().join(PID_ISOLATE_SCRIPT),
            "#!/usr/bin/env python3\n",
        )
        .unwrap();
        let timeout = Duration::from_secs(60);
        let command = |program: &str, args: &[&str]| GuardCommand {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            timeout,
        };
        let slot = std::ffi::OsStr::new("/home/op/.rsi/bin/cargo-slot");

        let cargo = command("cargo", &["test", "-p", "demo", "--lib"]);
        let isolated = isolated_slot_command(tree.path(), &cargo, None);
        assert_eq!(isolated.program, PID_ISOLATE_SCRIPT);
        assert_eq!(
            isolated.args,
            ["--", "cargo", "test", "-p", "demo", "--lib"]
        );
        assert_eq!(isolated.timeout, timeout);

        let slotted = isolated_slot_command(tree.path(), &cargo, Some(slot));
        assert_eq!(slotted.program, "/home/op/.rsi/bin/cargo-slot");
        assert_eq!(
            slotted.args,
            [
                PID_ISOLATE_SCRIPT,
                "--",
                "cargo",
                "test",
                "-p",
                "demo",
                "--lib"
            ]
        );

        let shard = rsid_shard_guard_command("store-01:test(one_test)").unwrap();
        let slotted_shard = isolated_slot_command(tree.path(), &shard, Some(slot));
        assert_eq!(slotted_shard.program, "/home/op/.rsi/bin/cargo-slot");
        assert_eq!(
            slotted_shard.args[..3],
            [PID_ISOLATE_SCRIPT, "--", "scripts/run-rsid-test-shards.sh"]
        );

        let python = command(
            "python3",
            &["-B", "-m", "unittest", "-v", "scripts/tests/test_x.py"],
        );
        let isolated_python = isolated_slot_command(tree.path(), &python, Some(slot));
        assert_eq!(
            isolated_python.program, PID_ISOLATE_SCRIPT,
            "no cargo slot for Python"
        );
        assert_eq!(isolated_python.args[..3], ["--", "python3", "-B"]);

        for unchanged in [
            command("cargo", &["check", "-p", "demo"]),
            command("git", &["diff", "--check"]),
            command("python3", &["scripts/check-store-boundary.py"]),
        ] {
            assert_eq!(
                isolated_slot_command(tree.path(), &unchanged, None),
                unchanged
            );
        }
        // A tree without the helper (a base side older than this change) runs the
        // command exactly as before.
        assert_eq!(isolated_slot_command(bare.path(), &cargo, None), cargo);
        assert_eq!(
            isolated_slot_command(bare.path(), &cargo, Some(slot)),
            slot_wrapped_command(&cargo, Some(slot))
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
        let slot_wrapper = with_lander_vars([("RSI_LANDER_LOCAL_SLOT_WRAPPER", &wrapper)]);
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
        drop(slot_wrapper);
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
        let namespace = with_lander_vars([("RSI_PROCESS_OWNERSHIP_NAMESPACE", "lander-custody")]);
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
        drop(namespace);
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

    /// #192: the lander's children get its explicit environment, never the
    /// process environment: a fake `git` on the explicit `PATH` is the one
    /// that runs, and it sees the explicit value, also on a `spawn_blocking`
    /// thread the environment was carried to.
    #[tokio::test]
    async fn lander_children_get_the_explicit_environment_not_the_process_one() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let record = root.path().join("git-env.txt");
        let git = bin.join("git");
        fs::write(
            &git,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"${{RSI_LANDER_ENV_PROBE_192:-unset}}\" >> '{}'\n",
                record.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
            &lander_env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        let scope = with_lander_vars([
            ("PATH", path.as_os_str()),
            ("RSI_LANDER_ENV_PROBE_192", std::ffi::OsStr::new("explicit")),
        ]);
        let output = git_output(root.path(), &["status"]).expect("the fake git starts");
        assert!(output.status.success());
        let repo = root.path().to_path_buf();
        let blocking = tokio::task::spawn_blocking(lander_env::propagate(move || {
            git_output(&repo, &["status"])
        }))
        .await
        .unwrap()
        .expect("the fake git starts on the blocking thread");
        assert!(blocking.status.success());
        drop(scope);
        assert_eq!(fs::read_to_string(&record).unwrap(), "explicit\nexplicit\n");
        assert_eq!(std::env::var_os("RSI_LANDER_ENV_PROBE_192"), None);
    }

    /// #192, #3391: with no test environment installed, the lander environment
    /// is the process environment reduced to the allowlist, so production
    /// children inherit what the gate needs and no credential.
    #[test]
    fn the_default_lander_environment_is_the_allowlisted_process_environment() {
        let unscoped = std::thread::spawn(lander_env::current).join().unwrap();
        let process: BTreeMap<std::ffi::OsString, std::ffi::OsString> =
            rsid::lander_env_policy::filter(std::env::vars_os())
                .into_iter()
                .collect();
        assert_eq!(unscoped.as_map(), &process);
        assert!(process.contains_key(std::ffi::OsStr::new("PATH")));
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
        let flooded = retry_failure_evidence(&flood);
        assert!(flooded.len() <= 2048, "{}", flooded.len());
        assert!(
            flooded.contains("more lines; bounded output in the queue gate log)"),
            "{flooded}"
        );
    }

    const PYTHON_RED_OUTPUT: &str = concat!(
        "test_a (pkg.mod.Case.test_a) ... FAIL\n",
        "test_b (pkg.mod.Case.test_b) ... ok\n",
        "\n",
        "======================================================================\n",
        "FAIL: test_a (pkg.mod.Case.test_a)\n",
        "----------------------------------------------------------------------\n",
        "Traceback (most recent call last):\n",
        "  File \"/w/pkg/mod.py\", line 12, in test_a\n",
        "    self.assertEqual(unit, 'x')\n",
        "AssertionError: 'y' != 'x'\n",
        "\n",
        "======================================================================\n",
        "FAIL: test_c (pkg.mod.Case.test_c)\n",
        "----------------------------------------------------------------------\n",
        "AssertionError: other\n",
        "\n",
        "----------------------------------------------------------------------\n",
        "Ran 3 tests in 0.01s\n",
        "\n",
        "FAILED (failures=2)\n",
    );

    /// #2793: a failing Python test's own traceback and assertion are kept,
    /// not the neighbouring test's, and the summary ends on the assertion.
    #[test]
    fn python_failure_block_holds_the_traceback_of_that_test() {
        let block = python_failure_block(PYTHON_RED_OUTPUT, "pkg.mod.Case.test_a");
        assert_eq!(
            block,
            vec![
                "Traceback (most recent call last):",
                "  File \"/w/pkg/mod.py\", line 12, in test_a",
                "    self.assertEqual(unit, 'x')",
                "AssertionError: 'y' != 'x'",
            ]
        );
        let last = python_failure_block(PYTHON_RED_OUTPUT, "pkg.mod.Case.test_c");
        assert_eq!(last, vec!["AssertionError: other"]);
        let summary = python_failure_summary(&block, 560);
        assert!(summary.ends_with("AssertionError: 'y' != 'x'"), "{summary}");
        assert!(summary.contains("line 12, in test_a"), "{summary}");
    }

    #[test]
    fn python_failure_evidence_is_bounded() {
        let mut output = String::from(
            "FAIL: test_a (pkg.mod.Case.test_a)\n----------------------------------------------------------------------\n",
        );
        for line in 0..500 {
            output.push_str(&format!("  frame {line}\n"));
        }
        output.push_str("AssertionError: end\n\nRan 1 test in 0.1s\n");
        let block = python_failure_block(&output, "pkg.mod.Case.test_a");
        assert_eq!(block.len(), PYTHON_EVIDENCE_LINES);
        assert_eq!(
            block.last().map(String::as_str),
            Some("AssertionError: end")
        );
        let summary = python_failure_summary(&block, 560);
        assert!(summary.len() <= 560, "{}", summary.len());
        assert!(summary.ends_with("AssertionError: end"), "{summary}");
        // Output with no header falls back to its newest lines.
        let fallback = python_failure_block("boom\nlast line\n", "pkg.x.y");
        assert_eq!(fallback, vec!["boom", "last line"]);
    }

    /// #2813: six new-red tests with long tracebacks still fit the queue's
    /// outcome-detail tail, every id and each final assertion survives, and the
    /// whole blocks stay in the lander log.
    #[test]
    fn python_refusal_fits_the_queue_tail_with_every_test_named() {
        let evidence: Vec<(String, Vec<String>)> = (0..6)
            .map(|n| {
                let mut block = vec!["Traceback (most recent call last):".to_owned()];
                for frame in 0..40 {
                    block.push(format!(
                        "  File \"/w/tools/suite_{n}.py\", line {frame}, in helper_{frame}"
                    ));
                }
                block.push(format!("AssertionError: distinct assertion text {n}"));
                (format!("tools.test_suite_{n}.Case.test_member_{n}"), block)
            })
            .collect();
        let ids: Vec<String> = evidence.iter().map(|(id, _)| id.clone()).collect();
        let message = python_new_failures_message("origin/rolling", &ids, &evidence);
        let limit = rsid::rolling_queue::OUTCOME_TAIL_BYTES;
        assert!(message.len() <= limit, "{} > {limit}", message.len());
        let kept = rsid::rolling_queue::tail(&message);
        assert_eq!(kept, message.trim());
        for (n, (id, block)) in evidence.iter().enumerate() {
            assert!(kept.contains(id.as_str()), "{id} missing: {kept}");
            let assertion = format!("AssertionError: distinct assertion text {n}");
            assert!(
                kept.contains(&assertion) || kept.contains("cut here"),
                "{assertion} missing: {kept}"
            );
            let log = python_failure_log_entry(id, "python3 -m unittest x", block);
            assert!(block.iter().all(|line| log.contains(line.as_str())));
        }
        assert_eq!(failing_test_names(&kept), ids);
    }

    /// #2813: too many failing tests to give each a useful share are named as
    /// cut, with their count, rather than dropped silently.
    #[test]
    fn python_refusal_marks_tests_whose_evidence_is_cut() {
        let evidence: Vec<(String, Vec<String>)> = (0..32)
            .map(|n| {
                (
                    format!("tools.test_suite.Case.test_member_with_a_long_name_{n:02}"),
                    vec![format!("AssertionError: assertion {n}")],
                )
            })
            .collect();
        let ids: Vec<String> = evidence.iter().map(|(id, _)| id.clone()).collect();
        let message = python_new_failures_message("origin/rolling", &ids, &evidence);
        assert!(message.len() <= rsid::rolling_queue::OUTCOME_TAIL_BYTES);
        assert!(message.contains("more failing test(s) cut here, named above"));
        assert_eq!(failing_test_names(&message), ids);
    }

    #[test]
    fn gate_environment_line_names_keys_never_values() {
        let spec_env = BTreeMap::from([("TMPDIR".to_owned(), "/dev/shm/g".to_owned())]);
        let process_env = BTreeMap::from([
            ("RSI_SESSION_TOKEN".to_owned(), "secret".to_owned()),
            ("RSI_JOB_ID".to_owned(), "j-secret-value".to_owned()),
            ("CARGO_HOME".to_owned(), "/c".to_owned()),
            ("HOME".to_owned(), "/h".to_owned()),
        ]);
        let line =
            gate_environment_line(Path::new("/w/tree"), &spec_env, &process_env, "u.service");
        assert_eq!(
            line,
            "gate_env=cwd=/w/tree tmpdir=/dev/shm/g unit=u.service spec_env_keys=[TMPDIR] \
             rsi_cargo_keys=[CARGO_HOME,RSI_JOB_ID]"
        );
    }

    #[test]
    fn retry_failure_evidence_reports_the_cut_of_a_long_panic_list() {
        let mut stderr = String::from(
            "thread 't' (1) panicked at scan.rs:9:5:\nclassified sites that no longer name x: [\n",
        );
        for site in 0..30 {
            stderr.push_str(&format!("    site_{site}.rs::f,\n"));
        }
        stderr.push_str("]\nnote: run with `RUST_BACKTRACE=1` environment variable\n");
        let report = rsid::integration::GuardCommandReport {
            program: "cargo".into(),
            args: vec!["test".into()],
            status: rsid::integration::GuardStatus::Failed { code: Some(101) },
            stdout_tail: String::new(),
            stderr_tail: stderr,
            output_truncated: false,
            duration: Duration::from_secs(1),
        };
        let evidence = retry_failure_evidence(&report);
        assert!(
            evidence.starts_with("thread 't' (1) panicked at scan.rs:9:5: | classified sites"),
            "{evidence}"
        );
        // 33 message lines (panic line, opener, 30 sites, closer); 4 kept, 29 cut.
        assert!(
            evidence.ends_with("(+29 more lines; bounded output in the queue gate log)"),
            "{evidence}"
        );
    }

    #[test]
    fn cargo_retry_preserves_the_selected_target() {
        for (filter, target) in [
            (None, vec!["--lib"]),
            (Some("existing_filter"), vec!["--lib"]),
            (
                Some("test:tui_integration"),
                vec!["--test", "tui_integration"],
            ),
            (
                Some("bin:rsi-rolling-land"),
                vec!["--bin", "rsi-rolling-land"],
            ),
        ] {
            let command = cargo_guard_command("rsid", filter);
            let retry =
                isolated_retry_command(&command, "tests::new_red", ShardTree::Static).unwrap();
            let mut expected = vec!["test", "-p", "rsid"];
            expected.extend(target);
            expected.extend(["tests::new_red", "--", "--exact", "--test-threads=4"]);
            assert_eq!(retry.program, "cargo");
            assert_eq!(retry.args, expected);
            assert_eq!(retry.timeout, command.timeout);
        }
    }

    #[test]
    fn cargo_retry_rejects_a_missing_target_name() {
        for selector in ["--test", "--bin"] {
            let mut command = cargo_guard_command("rsid", None);
            command.args[3] = selector.into();
            assert!(isolated_retry_command(&command, "tests::new_red", ShardTree::Static).is_err());
        }
    }

    #[test]
    fn shard_retry_targets_only_the_candidate_failure() {
        let command = rsid_shard_guard_command("store-01:test(existing_filter)").unwrap();
        let retry =
            isolated_retry_command(&command, "store::tests::new_red", ShardTree::Static).unwrap();
        assert_eq!(retry.program, "cargo");
        assert_eq!(
            retry.args,
            [
                "test",
                "-p",
                "rsid-store",
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

    /// The retries with their `stale_reason` cleared, to compare their shape.
    fn bare(retries: &[StaleRetry]) -> Vec<StaleRetry> {
        retries
            .iter()
            .map(|retry| StaleRetry {
                reason: String::new(),
                ..retry.clone()
            })
            .collect()
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
                reuse: if reused_gate {
                    StaleReuse::Whole
                } else {
                    StaleReuse::Regated
                },
                reason: String::new(),
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
                    "next=$(head -n 1 '{races}')\nif [ -n \"$next\" ]; then\n  tail -n +2 '{races}' > '{races}.tmp'; mv '{races}.tmp' '{races}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling \"$next\" || exit 44\n  echo 'remote rejected: cannot lock ref refs/heads/rolling' >&2\n  exit 1\nfi\nexec /usr/bin/git \"$@\"",
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
                bare(&report.stale_retries),
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
            "oid=$(/usr/bin/git rev-parse HEAD)\nnext=$(head -n 1 '{pending}')\nif [ -n \"$next\" ] && ! grep -qx \"$oid\" '{seen}'; then\n  printf '%s\\n' \"$oid\" >> '{seen}'\n  tail -n +2 '{pending}' > '{pending}.tmp'; mv '{pending}.tmp' '{pending}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling \"$next\" || exit 43\nfi",
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
            bare(&failure.stale_retries),
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
        let knobs = with_lander_vars([
            ("RSI_LANDER_MAX_REGATED_STALE_RETRIES", "1"),
            ("RSI_LANDER_EXACT_GATE", "1"),
        ]);
        let result = fixture
            .land_expecting(
                vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source,
                }],
                &fixture.base,
            )
            .await;
        drop(knobs);
        restore_path(old_path);
        let report = result.expect("the advance is charged and the fetched tip is gated");
        assert_eq!(
            bare(&report.stale_retries),
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
        let knobs = with_lander_vars([("RSI_LANDER_MAX_REGATED_STALE_RETRIES", "0")]);
        let result = fixture
            .land_expecting(
                vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source,
                }],
                &fixture.base,
            )
            .await;
        drop(knobs);
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

    /// #3785: the plan-only guard run never carries the derived test filters.
    #[test]
    fn plan_only_guard_args_carry_no_test_filters() {
        let pair = AcceptedPair {
            base: "b".repeat(40),
            source: "s".repeat(40),
        };
        let args = plan_only_guard_args(
            vec!["/repo/scripts/rolling-landing-guard.py".to_owned()],
            Path::new("/repo"),
            &pair,
            "tip",
            "cand",
            Some(Path::new("/proof.json")),
        );
        assert!(args.contains(&"--plan-only".to_owned()), "{args:?}");
        assert!(args.contains(&"--proof".to_owned()), "{args:?}");
        assert!(!args.iter().any(|arg| arg == "--test-filter"), "{args:?}");
    }

    /// #3785: a plan report of a source with 10000+ derived filters (the guard
    /// echoes one `test_commands` string per filter) is far larger than the old
    /// 256 KiB tail, and still parses through the lander's capture.
    ///
    /// hub #3795: the fixture prints the plan file with `cat`, which does not
    /// exist on Windows, so the test is unix-only. The lander's guard capture
    /// is program-agnostic; the tail-size behaviour it proves is the same on
    /// every platform.
    #[cfg(unix)]
    #[tokio::test]
    async fn guard_plan_larger_than_the_old_tail_parses() {
        let dir = tempfile::tempdir().unwrap();
        let source = "a".repeat(40);
        let candidate = "c".repeat(40);
        let commands = (0..12_000)
            .map(|index| {
                format!(
                    "cargo test -p rsid --lib shard:session-02:test(=session::topology_ops::tests::case_{index})"
                )
            })
            .collect::<Vec<_>>();
        let report = serde_json::json!({
            "outcome": "candidate",
            "accepted_source": source,
            "candidate": candidate,
            "lost_hunks": [],
            "affected_crates": ["rsid"],
            "test_commands": commands,
            "tests_executed": false,
        });
        let text = serde_json::to_string_pretty(&report).unwrap();
        assert!(text.len() > 256 * 1024, "fixture is large: {}", text.len());
        let file = dir.path().join("plan.json");
        fs::write(&file, &text).unwrap();
        let run = |tail: usize| {
            let file = file.clone();
            let dir = dir.path().to_path_buf();
            async move {
                run_guard_with_env(
                    &dir,
                    &GuardSpec {
                        commands: vec![GuardCommand {
                            program: "cat".into(),
                            args: vec![file.to_string_lossy().into_owned()],
                            timeout: Duration::from_secs(30),
                        }],
                        env: BTreeMap::new(),
                        output_tail_bytes: tail,
                    },
                    lander_env::current().as_map(),
                )
                .await
            }
        };
        let kept = run(GUARD_PLAN_TAIL_BYTES).await;
        let plan = parse_guard_plan(&kept.commands[0].stdout_tail)
            .expect("a large plan report parses from the lander's tail");
        assert_eq!(plan.accepted_source, source);
        assert_eq!(plan.candidate, candidate);
        assert_eq!(plan.affected_crates, vec!["rsid".to_owned()]);

        // The old 256 KiB tail cut the report mid-JSON; the error says how
        // many bytes it was handed instead of only "invalid plan".
        let cut = run(256 * 1024).await;
        let error = match parse_guard_plan(&cut.commands[0].stdout_tail) {
            Ok(_) => panic!("a truncated tail must not parse"),
            Err(error) => error,
        };
        assert!(
            error.contains("invalid landing guard plan")
                && error.contains(&format!("{} bytes", cut.commands[0].stdout_tail.len())),
            "{error}"
        );
    }

    #[test]
    fn oversize_test_filter_argument_names_filter_and_limit() {
        let long = format!("rsid={}", "y".repeat(300));
        let error = parse_args(
            ["--accepted", "abc", "--test-filter", long.as_str()].map(String::from),
            None,
        )
        .unwrap_err();
        assert!(
            error.starts_with("queue_filter_invalid: --test-filter[0] is 305 bytes"),
            "{error}"
        );
        assert!(error.contains("256-byte limit"), "{error}");
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

    /// #2167: `--fixes-test` takes test names only, once each.
    #[test]
    fn fixes_test_argument_parses_and_rejects_non_test_names() {
        let parsed = parse_args(
            [
                "--accepted",
                "abc",
                "--fixes-test",
                "tests::red",
                "--fixes-test",
                "tests::red",
                "--fixes-test",
                "a::b",
            ]
            .map(String::from),
            None,
        )
        .unwrap();
        assert_eq!(parsed.fixes_tests, vec!["tests::red", "a::b"]);
        let bare = parse_args(["--accepted", "abc"].map(String::from), None).unwrap();
        assert!(bare.fixes_tests.is_empty());
        let error = parse_args(
            ["--accepted", "abc", "--fixes-test", "a b"].map(String::from),
            None,
        )
        .unwrap_err();
        assert!(
            error.contains("--fixes-test expects a test name"),
            "{error}"
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

    /// Exercise the production receipted gate with a committed shell diff and
    /// optional selected Rust test. No Python tooling or fixtures are involved.
    async fn shell_gate_regression(changed: &str, rust: bool, red: bool) {
        let _lock = env_lock();
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| provisional_fixture::git(&repo, args);
        git(&["init", "-q"]);
        let suite = "scripts/tests/test_shell_tests.sh";
        // The wrapper observes both its legacy shell test and that test's subject.
        let body = "# suite-gate: tap\n# test_widget.sh covers widget.sh\nprintf '1..1\\nok 1 - wrapper case\\n'\n";
        write(&repo, suite, body);
        write(
            &repo,
            "scripts/tests/test_widget.sh",
            "# legacy exit-status test\nexit 0\n",
        );
        write(&repo, "scripts/widget.sh", "exit 0\n");
        write(&repo, "crates/demo/src/lib.rs", "pub fn value() {}\n");
        git(&[
            "add",
            "scripts/tests/test_shell_tests.sh",
            "scripts/tests/test_widget.sh",
            "scripts/widget.sh",
            "crates/demo/src/lib.rs",
        ]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        if red {
            // Exit zero deliberately: TAP owns the verdict, not the shell status.
            write(
                &repo,
                suite,
                "# suite-gate: tap\nprintf '1..1\\nnot ok 1 - deliberate red\\n'\n",
            );
        } else {
            let contents = fs::read_to_string(repo.join(changed)).unwrap();
            write(&repo, changed, &format!("{contents}# changed\n"));
        }
        if rust {
            write(
                &repo,
                "crates/demo/src/lib.rs",
                "pub fn value() { let _changed = 1; }\n",
            );
        }
        git(&[
            "add",
            "scripts/tests/test_shell_tests.sh",
            changed,
            "crates/demo/src/lib.rs",
        ]);
        git(&["commit", "-qm", "candidate"]);
        let candidate = git(&["rev-parse", "HEAD"]);
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let cargo = bin.join("cargo");
        fs::write(&cargo, "#!/bin/sh\nprintf 'test tests::selected_rust ... ok\\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'\nprintf ran > rust-ran\n").unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        let _environment = lander_env::LanderEnv::from_process_vars(std::env::vars_os())
            .with_var("PATH", path)
            .with_var("RSI_PID_ISOLATION", "off")
            .enter();
        let guard = root.path().join("guard-stub");
        let real = provisional_fixture::real_guard();
        // Deliberately exit zero even for a red receipt: the lander must
        // inspect the typed verdict independently of the guard exit status.
        fs::write(&guard, format!(
            "#!/bin/sh\n[ \"$1\" = residual-lane ] || exit 93\nprintf '%s\\n' \"$@\" > '{}'\n'{}' \"$@\"\nexit 0\n",
            root.path().join("guard-args").display(), real.display(),
        )).unwrap();
        fs::set_permissions(&guard, fs::Permissions::from_mode(0o755)).unwrap();
        struct RestoreShellGuard;
        impl Drop for RestoreShellGuard {
            fn drop(&mut self) {
                SHELL_GUARD_EXE.with(|path| *path.borrow_mut() = None);
            }
        }
        SHELL_GUARD_EXE.with(|path| *path.borrow_mut() = Some(guard));
        let _guard = RestoreShellGuard;
        let mut gate = TestGate {
            receipt_root: Some(root.path().join("receipts")),
            ..TestGate::default()
        };
        let options = Options {
            repo: repo.clone(),
            remote: "origin".into(),
            accepted: Vec::new(),
            test_filters: Vec::new(),
            cargo_build_jobs: 1,
            remote_gate: None,
            tmpfs_min_free_gb: 12,
            disk_scratch_shards: BTreeSet::new(),
            expected_tip: None,
            fixes_tests: Vec::new(),
        };
        let spec = GuardSpec {
            commands: if rust {
                vec![cargo_guard_command("demo", Some("tests::selected_rust"))]
            } else {
                vec![]
            },
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let result = run_affected_gate_receipted(
            &repo, &repo, &base, &candidate, &spec, &mut gate, &options,
        )
        .await;
        let args = fs::read_to_string(root.path().join("guard-args")).unwrap();
        assert!(args.lines().any(|arg| arg == candidate), "{args}");
        assert!(args.lines().any(|arg| arg == base), "{args}");
        let logs = fs::read_dir(root.path().join("receipts/shell-suites"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let receipt: Value =
            serde_json::from_slice(&fs::read(logs.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(receipt["receipt_head"], candidate);
        assert_eq!(receipt["selected"], serde_json::json!([suite]));
        assert_eq!(receipt["residual"][0]["test_file"], suite);
        assert_eq!(receipt["residual"][0]["executed_tests"], 1);
        let log = fs::read_to_string(logs.join("residual-0.log")).unwrap();
        assert!(log.contains("1..1"), "{log}");
        if red {
            let error = result.expect_err("a red shell suite must refuse the gate");
            assert!(error.contains(suite), "{error}");
            assert_eq!(receipt["ok"], false);
            assert_eq!(receipt["residual"][0]["status"], "failed");
            assert_eq!(receipt["residual"][0]["exit_code"], 0);
            assert_eq!(receipt["residual"][0]["failed_tests"], 1);
        } else {
            result.expect("selected shell suites pass alongside any Rust filters");
            assert_eq!(receipt["ok"], true);
            assert_eq!(receipt["residual"][0]["status"], "passed");
            if rust {
                assert_eq!(fs::read_to_string(repo.join("rust-ran")).unwrap(), "ran");
            }
        }
    }

    #[tokio::test]
    async fn shell_only_diff_runs_tap_suite_at_gate() {
        shell_gate_regression("scripts/tests/test_shell_tests.sh", false, false).await;
    }

    #[tokio::test]
    async fn rust_and_changed_shell_test_run_wrapper_at_gate() {
        shell_gate_regression("scripts/tests/test_widget.sh", true, false).await;
    }

    #[tokio::test]
    async fn rust_and_changed_covered_script_run_wrapper_at_gate() {
        shell_gate_regression("scripts/widget.sh", true, false).await;
    }

    #[tokio::test]
    async fn failing_shell_suite_refuses_gate_with_typed_receipt() {
        shell_gate_regression("scripts/tests/test_shell_tests.sh", true, true).await;
    }

    #[test]
    fn shell_receipt_requires_matching_identity_and_complete_green_results() {
        let green = serde_json::json!({
            "schema_version": 1, "base": "base", "receipt_head": "head",
            "ok": true, "error": null, "selected": ["scripts/tests/test_suite.sh"],
            "residual": [{"test_file": "scripts/tests/test_suite.sh", "status": "passed",
                "completed": true, "executed_tests": 1, "failed_tests": 0, "exit_code": 0}],
        });
        validate_shell_receipt(&green, "base", "head").unwrap();
        let mut red = green.clone();
        red["residual"][0]["status"] = "failed".into();
        assert!(validate_shell_receipt(&red, "base", "head").is_err());
        let mut partial = green.clone();
        partial["residual"] = serde_json::json!([]);
        assert!(validate_shell_receipt(&partial, "base", "head").is_err());
        assert!(validate_shell_receipt(&green, "base", "other-head").is_err());
        assert!(validate_shell_receipt(&green, "other-base", "head").is_err());
        for field in ["completed", "executed_tests", "failed_tests", "exit_code"] {
            let mut malformed = green.clone();
            malformed["residual"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(validate_shell_receipt(&malformed, "base", "head").is_err());
        }
    }

    /// #1606: Python failures refuse a scripts/tools landing, and successful
    /// suites and pure docs landings need no Cargo invocation at all. #3205: a
    /// path outside scripts/tools that a suite names runs that suite too.
    #[tokio::test]
    async fn python_gate_checks_scripts_tools_and_tests_before_publication_without_cargo() {
        for (changed, passes) in [
            ("scripts/widget.py", true),
            ("tools/widget.py", true),
            ("config/widget.lock", true),
            ("config/unnamed.lock", true),
            ("scripts/tests/test_widget.py", true),
            ("scripts/tests/test_widget.py", false),
            ("docs/widget.md", true),
            ("thoughts/widget.md", true),
        ] {
            let fixture = Fixture::new();
            fs::copy(
                PathBuf::from(rsi_common::manifest_dir!())
                    .join("../../scripts/python-test-gate.py"),
                fixture.repo.join("scripts/python-test-gate.py"),
            )
            .unwrap();
            let log = fixture.root.path().join("python-runs");
            let suite_with = |value: i32| {
                format!(
                    "import unittest\nfrom pathlib import Path\n# config/widget.lock\nclass Test(unittest.TestCase):\n    def test_widget(self):\n        Path({:?}).write_text('ran')\n        self.assertEqual({value}, 1)\n",
                    log.to_str().unwrap(),
                )
            };
            // #2576: a red the base shares no longer refuses, so the base
            // suite is green and the candidate introduces the failure.
            let suite = suite_with(1);
            let candidate_suite = suite_with(if passes { 1 } else { 2 });
            write(&fixture.repo, "scripts/tests/test_widget.py", &suite);
            git_run(
                &fixture.repo,
                &[
                    "add",
                    "scripts/python-test-gate.py",
                    "scripts/tests/test_widget.py",
                ],
            );
            git_run(&fixture.repo, &["commit", "-qm", "Python coverage"]);
            git_run(&fixture.repo, &["push", "-q", "origin", "rolling"]);
            let base = git_value(&fixture.repo, &["rev-parse", "HEAD"]);
            let contents = if changed.starts_with("scripts/tests/") {
                format!("{candidate_suite}# changed\n")
            } else {
                "# changed\n".into()
            };
            let source = fixture.commit(&base, changed, &contents, "source");
            let cargo = fixture.bin.join("cargo");
            fs::write(&cargo, "#!/bin/sh\necho 'unexpected cargo' >&2\nexit 99\n").unwrap();
            fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
            let old_path = use_fake_path(&fixture);
            let result = fixture
                .land(vec![AcceptedPair {
                    base: base.clone(),
                    source,
                }])
                .await;
            restore_path(old_path);
            if passes {
                let report = result.unwrap_or_else(|error| panic!("{changed}: {error:?}"));
                assert_eq!(
                    git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
                    report.published_tip
                );
                assert_eq!(
                    log.is_file(),
                    changed.starts_with("scripts/")
                        || changed.starts_with("tools/")
                        || changed == "config/widget.lock"
                );
            } else {
                let failure = result.expect_err("a failing Python suite refuses landing");
                assert!(failure.message.contains("test_widget"), "{failure:?}");
                assert_eq!(
                    git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
                    base
                );
                assert_eq!(fs::read_to_string(log).unwrap(), "ran");
            }
        }
    }

    /// #2576: lands a candidate through the Python gate. `base_suite` is the
    /// committed `scripts/tests/test_widget.py`; the candidate edits
    /// `scripts/widget.py` (the suite is selected through its stem), or the
    /// suite itself when `candidate_suite` is given.
    async fn land_python_candidate(
        base_suite: &str,
        candidate_suite: Option<&str>,
        fixes: &[&str],
    ) -> (Fixture, String, Result<LandReport, LandFailure>) {
        land_python_candidate_with(base_suite, candidate_suite, fixes, &[]).await
    }

    /// `land_python_candidate` with more committed files (#3320: a second test
    /// module the candidate's diff does not select).
    async fn land_python_candidate_with(
        base_suite: &str,
        candidate_suite: Option<&str>,
        fixes: &[&str],
        extra_files: &[(&str, &str)],
    ) -> (Fixture, String, Result<LandReport, LandFailure>) {
        let fixture = Fixture::new();
        fs::copy(
            PathBuf::from(rsi_common::manifest_dir!()).join("../../scripts/python-test-gate.py"),
            fixture.repo.join("scripts/python-test-gate.py"),
        )
        .unwrap();
        write(&fixture.repo, "scripts/tests/test_widget.py", base_suite);
        git_run(
            &fixture.repo,
            &[
                "add",
                "scripts/python-test-gate.py",
                "scripts/tests/test_widget.py",
            ],
        );
        for (path, text) in extra_files {
            write(&fixture.repo, path, text);
            git_run(&fixture.repo, &["add", path]);
        }
        git_run(&fixture.repo, &["commit", "-qm", "Python coverage"]);
        git_run(&fixture.repo, &["push", "-q", "origin", "rolling"]);
        let base = git_value(&fixture.repo, &["rev-parse", "HEAD"]);
        let source = match candidate_suite {
            Some(suite) => fixture.commit(&base, "scripts/tests/test_widget.py", suite, "source"),
            None => fixture.commit(&base, "scripts/widget.py", "# changed\n", "source"),
        };
        let cargo = fixture.bin.join("cargo");
        fs::write(&cargo, "#!/bin/sh\necho 'unexpected cargo' >&2\nexit 99\n").unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_fixes(
                vec![AcceptedPair {
                    base: base.clone(),
                    source,
                }],
                fixes.iter().map(|name| (*name).to_owned()).collect(),
            )
            .await;
        restore_path(old_path);
        (fixture, base, result)
    }

    const PYTHON_GREEN_TEST: &str = "    def test_green(self):\n        self.assertEqual(1, 1)\n";

    fn python_suite(extra_tests: &str) -> String {
        format!("import unittest\nclass Test(unittest.TestCase):\n{PYTHON_GREEN_TEST}{extra_tests}")
    }

    /// #2576: a test that fails in the suite run but passes alone (the #2575
    /// `.git/index` flake) is recorded as a flake and does not refuse.
    #[tokio::test]
    async fn python_gate_lands_a_flake_that_passes_on_an_isolated_rerun() {
        let marker = tempfile::tempdir().unwrap();
        let marker = marker.path().join("seen");
        let suite = python_suite(&format!(
            "    def test_flaky(self):\n        import os\n        first = not os.path.exists({0:?})\n        open({0:?}, 'w').close()\n        self.assertFalse(first)\n",
            marker.to_str().unwrap()
        ));
        let (fixture, _base, result) = land_python_candidate(&suite, None, &[]).await;
        let report = result.expect("a flake that passes alone does not refuse the landing");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
        assert!(
            report
                .flakes
                .iter()
                .any(|name| name.ends_with("Test.test_flaky")),
            "{:?}",
            report.flakes
        );
    }

    /// #2576: a Python test red on the base too is a baseline red.
    #[tokio::test]
    async fn python_gate_lands_a_red_the_base_shares() {
        let suite = python_suite("    def test_known_red(self):\n        self.assertEqual(1, 2)\n");
        let (fixture, _base, result) = land_python_candidate(&suite, None, &[]).await;
        let report = result.expect("a base-shared Python red does not refuse the landing");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
        assert!(report.flakes.is_empty(), "{:?}", report.flakes);
        assert!(
            report
                .base_reds
                .iter()
                .any(|name| name.ends_with("Test.test_known_red")),
            "{:?}",
            report.base_reds
        );
        // #2703: the isolated Python command that observed it reaches the report.
        let (_, command) = report
            .base_red_commands
            .iter()
            .find(|(name, _)| name.ends_with("Test.test_known_red"))
            .unwrap_or_else(|| panic!("no base_red_command: {:?}", report.base_red_commands));
        assert!(
            command.contains("-m unittest") && command.contains("Test.test_known_red"),
            "{command}"
        );
    }

    /// #2705: a gate that finds a base-shared red (A) and a new candidate red
    /// (B) fails, yet still reports A with its command and never B.
    #[tokio::test]
    async fn a_failed_python_gate_still_reports_the_base_red_it_observed() {
        let base_suite =
            python_suite("    def test_red_a(self):\n        self.assertEqual(1, 2)\n");
        let candidate_suite = python_suite(
            "    def test_red_a(self):\n        self.assertEqual(1, 2)\n    def test_red_b(self):\n        self.assertEqual(1, 2)\n",
        );
        let (_fixture, _base, result) =
            land_python_candidate(&base_suite, Some(&candidate_suite), &[]).await;
        let failure = result.expect_err("the new Python red refuses the landing");
        assert!(failure.message.contains("Test.test_red_b"), "{failure:?}");
        let lines = failure.evidence_lines();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("base_red=") && line.ends_with("Test.test_red_a")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("base_red_command=")
                    && line.contains("Test.test_red_a\t")
                    && line.contains("-m unittest")),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with("base_red") && line.contains("test_red_b")),
            "{lines:?}"
        );
    }

    /// #2705: whatever fails after the gate observed a base red (a command or
    /// push error), the failure carries that red.
    #[tokio::test]
    async fn any_failed_landing_keeps_the_base_reds_its_gate_observed() {
        let fixture = Fixture::new();
        let gate = TestGate {
            base_reds: BTreeSet::from(["demo::red".to_owned()]),
            base_red_commands: BTreeMap::from([(
                "demo::red".to_owned(),
                "cargo test -p demo".to_owned(),
            )]),
            ..TestGate::default()
        };
        let failure = land_with_gate(
            Options {
                repo: fixture.repo.clone(),
                remote: "origin".into(),
                accepted: vec![AcceptedPair {
                    base: String::new(),
                    source: "0".repeat(40),
                }],
                test_filters: Vec::new(),
                cargo_build_jobs: 1,
                remote_gate: None,
                tmpfs_min_free_gb: 12,
                disk_scratch_shards: BTreeSet::new(),
                expected_tip: None,
                fixes_tests: Vec::new(),
            },
            gate,
        )
        .await
        .expect_err("an unknown source fails the landing");
        let lines = failure.evidence_lines();
        assert!(
            lines.contains(&"base_red=demo::red".to_owned()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"base_red_command=demo::red\tcargo test -p demo".to_owned()),
            "{lines:?}"
        );
    }

    /// #2576: a Python test the candidate turns red still refuses, naming it.
    #[tokio::test]
    async fn python_gate_refuses_a_new_red() {
        let base_suite = python_suite("");
        let candidate_suite =
            python_suite("    def test_new_red(self):\n        self.assertEqual(1, 2)\n");
        let (fixture, base, result) =
            land_python_candidate(&base_suite, Some(&candidate_suite), &[]).await;
        let failure = result.expect_err("a new Python red refuses the landing");
        assert!(failure.message.contains("Test.test_new_red"), "{failure:?}");
        assert!(failure.message.contains(NEW_FAILURES_PREFIX), "{failure:?}");
        // #2793: the refusal carries the assertion the isolated rerun raised.
        assert!(
            failure.message.contains("AssertionError: 1 != 2"),
            "{failure:?}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            base
        );
    }

    /// #2576 + #2167: a claimed Python fix that is still red refuses even when
    /// the base is red on it, and one that runs green is proven.
    #[tokio::test]
    async fn python_gate_keeps_claimed_fix_semantics() {
        let suite = python_suite("    def test_known_red(self):\n        self.assertEqual(1, 2)\n");
        let (fixture, _base, result) =
            land_python_candidate(&suite, None, &["Test.test_known_red"]).await;
        // A fixture holds the environment lock until it drops.
        drop(fixture);
        let failure = result.expect_err("a claimed Python fix that is red refuses");
        assert!(failure.message.contains("test_known_red"), "{failure:?}");
        assert!(
            failure
                .message
                .contains(rsi_common::rolling_queue::QUEUE_CLAIMED_FIX_STILL_RED),
            "{failure:?}"
        );
        let (_fixture, _base, result) =
            land_python_candidate(&python_suite(""), None, &["Test.test_green"]).await;
        result.expect("a claimed Python fix that runs green lands");
    }

    /// #3320: a claimed Python test in a module the diff does not select is run
    /// by the gate itself: green lands, red refuses, and a test that is not in
    /// the module is refused with the Python next step (no filter exists).
    #[tokio::test]
    async fn python_gate_runs_the_module_of_a_claimed_python_test() {
        let guard = python_suite("");
        let guard_files = [("scripts/tests/test_guard.py", guard.as_str())];
        let claim = "scripts.tests.test_guard.Test.test_green";
        let (fixture, _base, result) =
            land_python_candidate_with(&python_suite(""), None, &[claim], &guard_files).await;
        // A fixture holds the environment lock until it drops.
        drop(fixture);
        result.expect("a claimed Python test outside the diff's selection runs green and lands");

        let red = python_suite("    def test_red(self):\n        self.assertEqual(1, 2)\n");
        let red_files = [("scripts/tests/test_guard.py", red.as_str())];
        let (fixture, _base, result) = land_python_candidate_with(
            &python_suite(""),
            None,
            &["scripts.tests.test_guard.Test.test_red"],
            &red_files,
        )
        .await;
        drop(fixture);
        let failure = result.expect_err("a claimed Python test that is red refuses");
        assert!(
            failure
                .message
                .contains(rsi_common::rolling_queue::QUEUE_CLAIMED_FIX_STILL_RED),
            "{failure:?}"
        );
        assert!(failure.message.contains("test_red"), "{failure:?}");

        let (_fixture, _base, result) = land_python_candidate_with(
            &python_suite(""),
            None,
            &["scripts.tests.test_guard.Test.test_missing"],
            &guard_files,
        )
        .await;
        let failure = result.expect_err("a claimed Python test that does not exist is unproven");
        assert!(
            failure.message.contains("full unittest id")
                && !failure.message.contains("add a test filter"),
            "{failure:?}"
        );
    }

    /// #3320: the claimed module resolves only for a real unittest id under
    /// scripts/tests or tools/tests; the refusal's next step follows the kind.
    #[test]
    fn a_claimed_python_test_resolves_to_its_module_only() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "scripts/tests/test_guard.py", "");
        write(dir.path(), "tools/tests/sub/test_deep.py", "");
        write(dir.path(), "scripts/other.py", "");
        let resolve = |claim: &str| claimed_python_suite(dir.path(), claim);
        assert_eq!(
            resolve("scripts.tests.test_guard.Test.test_one"),
            Some(("scripts/tests/test_guard.py".to_owned(), "unittest"))
        );
        assert_eq!(
            resolve("tools.tests.sub.test_deep.Test.test_one"),
            Some(("tools/tests/sub/test_deep.py".to_owned(), "unittest"))
        );
        for none in [
            "Test.test_one",
            "scripts.other.Test.test_one",
            "scripts.tests.test_absent.Test.test_one",
            "scripts.tests..test_guard.Test.test_one",
            "demo::tests::red",
        ] {
            assert_eq!(resolve(none), None, "{none}");
        }
        let python = unproven_claim_next_step(&["scripts.tests.test_guard.Test.t".to_owned()]);
        assert!(python.contains("full unittest id"), "{python}");
        assert!(!python.contains("add a test filter"), "{python}");
        let rust = unproven_claim_next_step(&["demo::tests::red".to_owned()]);
        assert!(rust.contains("add a test filter that selects it"), "{rust}");
        let both = unproven_claim_next_step(&["demo::red".to_owned(), "a.b.c".to_owned()]);
        assert!(both.contains("Rust:") && both.contains("Python:"), "{both}");
    }

    /// #2583: a claimed Python fix that passes in a suite which also holds a
    /// base-shared red is proven green, so the landing is not refused as unproven.
    #[tokio::test]
    async fn python_gate_proves_a_claimed_green_beside_a_base_shared_red() {
        let suite = python_suite("    def test_known_red(self):\n        self.assertEqual(1, 2)\n");
        let (_fixture, _base, result) =
            land_python_candidate(&suite, None, &["Test.test_green"]).await;
        let report = result.expect("a green claimed test beside a base-shared red lands");
        assert!(
            report
                .base_reds
                .iter()
                .any(|name| name.ends_with("Test.test_known_red")),
            "{:?}",
            report.base_reds
        );
    }

    #[test]
    fn python_failure_ids_name_single_tests_only() {
        let output = "FAIL: test_a (scripts.tests.test_x.Class.test_a)\nFAILED (failures=2, errors=1)\nERROR: test_b (scripts.tests.test_x.Class)\nFAIL: test_c (scripts.tests.test_x.Class.test_c) (i=1)\nFAILED scripts/tests/test_p.py::test_d - assert 0\n";
        assert_eq!(
            python_failure_ids(output).unwrap(),
            BTreeSet::from([
                "scripts.tests.test_x.Class.test_a".to_owned(),
                "scripts.tests.test_x.Class.test_b".to_owned(),
                "scripts.tests.test_x.Class.test_c".to_owned(),
                "scripts/tests/test_p.py::test_d".to_owned(),
            ])
        );
        for unnamed in [
            "ERROR: setUpClass (scripts.tests.test_x.Class)",
            "ERROR: scripts.tests.test_x (unittest.loader._FailedTest.scripts.tests.test_x)",
        ] {
            assert!(python_failure_ids(unnamed).is_err(), "{unnamed}");
        }
        assert_eq!(
            python_pass_ids(
                "test_a (scripts.tests.test_x.Class.test_a) ... ok\ntest_b (scripts.tests.test_x.Class) ... ok\n"
            ),
            BTreeSet::from([
                "scripts.tests.test_x.Class.test_a".to_owned(),
                "scripts.tests.test_x.Class.test_b".to_owned(),
            ])
        );
        assert!(python_claim_matches(
            "Class.test_a",
            "scripts.tests.test_x.Class.test_a"
        ));
        assert!(!python_claim_matches(
            "est_a",
            "scripts.tests.test_x.Class.test_a"
        ));
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
            "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\nnext=$(head -n 1 '{pending}')\nif [ -n \"$next\" ] && ! grep -qx \"$oid\" '{seen}'; then\n  printf '%s\\n' \"$oid\" >> '{seen}'\n  tail -n +2 '{pending}' > '{pending}.tmp'; mv '{pending}.tmp' '{pending}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling \"$next\" || exit 43\nfi",
            log = log.display(),
            pending = pending.display(),
            seen = seen.display(),
            bare = fixture.bare.display(),
        ));
        let old_path = use_fake_path(&fixture);
        let mut vars = vec![("RSI_LANDER_MAX_REGATED_STALE_RETRIES", "1")];
        if exact_gate {
            vars.push(("RSI_LANDER_EXACT_GATE", "1"));
        }
        let knobs = with_lander_vars(vars);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        drop(knobs);
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
            bare(&failure.stale_retries),
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
            report.stale_retries.iter().all(StaleRetry::reused_gate),
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

    /// Commits a fake `scripts/check-touched-shards` that prints `filters` (a
    /// JSON array of `PACKAGE=FILTER`) as the advance's suggested landing
    /// filters, on top of the fixture's base (#1940).
    fn add_fake_selector(fixture: &mut Fixture, filters: &str) {
        write(
            &fixture.repo,
            "scripts/check-touched-shards",
            &format!("import json\nprint(json.dumps({{\"suggested_filters\": {filters}}}))\n"),
        );
        git_run(&fixture.repo, &["add", "scripts/check-touched-shards"]);
        git_run(&fixture.repo, &["commit", "-q", "-m", "fake selector"]);
        git_run(&fixture.repo, &["push", "-q", "origin", "rolling"]);
        fixture.base = git_value(&fixture.repo, &["rev-parse", "HEAD"]);
    }

    /// Lands a candidate that edits `crates/demo/src/lib.rs` as the merge queue
    /// does (exact gate, a wall-time re-gate budget of `budget` seconds) while
    /// each of `advances` (file, content) wins the publish race against the
    /// gate just run. `selector` is what the fake `check-touched-shards`
    /// suggests for every advance. Returns the fixture, the advance commits,
    /// the commits the fake cargo ran, and the outcome.
    async fn land_exact_against(
        selector: &str,
        advances: &[(&str, &str)],
        budget: &str,
    ) -> (
        Fixture,
        Vec<String>,
        String,
        Result<LandReport, LandFailure>,
    ) {
        let mut fixture = Fixture::new();
        add_fake_selector(&mut fixture, selector);
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"source\" }\n",
            "source crate",
        );
        let contents = advances
            .iter()
            .map(|(file, content)| ((*file).to_string(), (*content).to_string()))
            .collect::<Vec<_>>();
        let advances = stage_advances(&fixture, &fixture.base, &contents);
        let pending = fixture.root.path().join("pending-advances");
        fs::write(&pending, format!("{}\n", advances.join("\n"))).unwrap();
        let seen = fixture.root.path().join("gated-commits");
        fs::write(
            &seen,
            format!("{}\n{}\n", fixture.base, advances.join("\n")),
        )
        .unwrap();
        let log = fixture.root.path().join("tested-commits");
        // Every newly gated candidate loses the race to the next advance.
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\nnext=$(head -n 1 '{pending}')\nif [ -n \"$next\" ] && ! grep -qx \"$oid\" '{seen}'; then\n  printf '%s\\n' \"$oid\" >> '{seen}'\n  tail -n +2 '{pending}' > '{pending}.tmp'; mv '{pending}.tmp' '{pending}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling \"$next\" || exit 43\nfi",
            log = log.display(),
            pending = pending.display(),
            seen = seen.display(),
            bare = fixture.bare.display(),
        ));
        let old_path = use_fake_path(&fixture);
        let knobs = with_lander_vars([
            ("RSI_LANDER_EXACT_GATE", "1"),
            ("RSI_LANDER_REGATE_BUDGET_SECS", budget),
        ]);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        drop(knobs);
        restore_path(old_path);
        let tested = fs::read_to_string(&log).unwrap();
        (fixture, advances, tested, result)
    }

    /// #1940: rolling moves under the gate by a doc the selector says no test
    /// reads. The remade candidate reuses the earlier gate whole: it is not
    /// tested before publication, only by the post-publication canary.
    #[tokio::test]
    async fn exact_gate_reuses_the_whole_gate_for_an_advance_that_selects_nothing() {
        let (fixture, advances, tested, result) =
            land_exact_against("[]", &[("docs-0.md", "one\n")], "3600").await;
        let report = result.expect("a doc advance reuses the earlier gate");
        assert_eq!(&report.fetched_tip, advances.last().unwrap());
        assert_eq!(report.stale_retries.len(), 1, "{:?}", report.stale_retries);
        let retry = &report.stale_retries[0];
        assert_eq!(retry.reuse, StaleReuse::Proven, "{retry:?}");
        assert_eq!(retry.reason, "disjoint");
        assert!(retry.reused_gate());
        let lines = stale_retry_lines(&report.stale_retries);
        assert_eq!(
            lines[1],
            format!(
                "stale_retry_1={}..{}:delta_reused",
                fixture.base, advances[0]
            )
        );
        assert_eq!(lines[2], "stale_reason_1=disjoint");
        // #1957: nothing of the gate ran again, so neither does its canary.
        assert_eq!(
            tested
                .lines()
                .filter(|oid| *oid == report.candidate)
                .count(),
            0,
            "the remade candidate is never tested, not even by a canary: {tested}"
        );
        assert_eq!(
            report.canary_skipped.as_deref(),
            Some("stale_delta_proven advances=1")
        );
        assert_eq!(report.canary_queued, None);
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
    }

    /// #1957: an advance whose selected tests all lie outside the gate (a doc
    /// another crate's test reads) re-runs nothing but the static guards: the
    /// retry counts as proven and the published tip needs no canary.
    #[tokio::test]
    async fn exact_gate_proves_an_advance_whose_selection_misses_the_gate() {
        let (fixture, advances, tested, result) =
            land_exact_against("[\"rsi=app\"]", &[("docs-0.md", "one\n")], "3600").await;
        let report = result.expect("an advance outside the gate reuses it");
        assert_eq!(report.stale_retries.len(), 1, "{:?}", report.stale_retries);
        let retry = &report.stale_retries[0];
        assert_eq!(retry.reuse, StaleReuse::Proven, "{retry:?}");
        assert_eq!(retry.reason, "overlap:rsi:lib");
        assert_eq!(
            retry.label(),
            format!("{}..{}:delta_reused", fixture.base, advances[0])
        );
        assert_eq!(
            tested
                .lines()
                .filter(|oid| *oid == report.candidate)
                .count(),
            0,
            "{tested}"
        );
        assert!(report.canary_skipped.is_some(), "{report:?}");
    }

    /// An advance in the candidate's own crate selects the candidate's tests:
    /// the remade candidate runs exactly those again, then publishes.
    #[tokio::test]
    async fn exact_gate_reruns_only_what_an_overlapping_advance_selects() {
        for (selector, file, reason) in [
            ("[]", "crates/demo/src/other.rs", "overlap:demo:*"),
            ("[\"demo=value\"]", "docs-0.md", "overlap:demo:lib"),
        ] {
            let (fixture, advances, tested, result) =
                land_exact_against(selector, &[(file, "pub fn other() {}\n")], "3600").await;
            let report = result.unwrap_or_else(|failure| panic!("{reason}: {failure:?}"));
            assert_eq!(report.stale_retries.len(), 1, "{:?}", report.stale_retries);
            let retry = &report.stale_retries[0];
            assert_eq!(retry.reuse, StaleReuse::Partial, "{retry:?}");
            assert_eq!(retry.reason, reason);
            assert!(!retry.reused_gate());
            assert_eq!(
                retry.label(),
                format!("{}..{}:delta_partial", fixture.base, advances[0])
            );
            assert_eq!(
                tested
                    .lines()
                    .filter(|oid| *oid == report.candidate)
                    .count(),
                2,
                "the remade candidate's demo tests run again, then its canary: {tested}"
            );
            assert_eq!(report.canary_skipped, None, "{reason}");
        }
    }

    /// #1940: an advance that shares a file with the candidate re-runs the whole
    /// gate, and the re-gates are bounded by wall time, not by a count of one:
    /// three overlapping advances in a row still publish.
    #[tokio::test]
    async fn exact_gate_regates_overlapping_advances_within_the_wall_time_budget() {
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
        let seen = fixture.root.path().join("gated-commits");
        fs::write(&seen, format!("{setup}\n{}\n", advances.join("\n"))).unwrap();
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\nnext=$(head -n 1 '{pending}')\nif [ -n \"$next\" ] && ! grep -qx \"$oid\" '{seen}'; then\n  printf '%s\\n' \"$oid\" >> '{seen}'\n  tail -n +2 '{pending}' > '{pending}.tmp'; mv '{pending}.tmp' '{pending}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling \"$next\" || exit 43\nfi",
            pending = pending.display(),
            seen = seen.display(),
            bare = fixture.bare.display(),
        ));
        let old_path = use_fake_path(&fixture);
        let knobs = with_lander_vars([
            ("RSI_LANDER_EXACT_GATE", "1"),
            ("RSI_LANDER_REGATE_BUDGET_SECS", "3600"),
        ]);
        let result = fixture
            .land(vec![AcceptedPair {
                base: setup.clone(),
                source,
            }])
            .await;
        drop(knobs);
        restore_path(old_path);
        let report = result.expect("each overlapping advance is re-gated within the budget");
        assert_eq!(&report.fetched_tip, advances.last().unwrap());
        assert_eq!(report.stale_retries.len(), 3, "{:?}", report.stale_retries);
        assert!(
            report
                .stale_retries
                .iter()
                .all(|retry| retry.reuse == StaleReuse::Regated && retry.reason == "files_overlap"),
            "{:?}",
            report.stale_retries
        );
        assert_eq!(
            bare(&report.stale_retries),
            expected_stale_history(&setup, &advances, false)
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
    }

    /// A re-gate the wall-time budget cannot cover is refused before it starts,
    /// with the code the queue settles as `queue_out_of_band_regate_exhausted`,
    /// so the lander never runs into the queue's hard timeout.
    #[tokio::test]
    async fn exact_gate_refuses_a_regate_the_wall_time_budget_cannot_cover() {
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
        let advances = stage_advances(
            &fixture,
            &setup,
            &[(
                "race.txt".to_string(),
                original.replace("line 18\n", "incoming line 18\n"),
            )],
        );
        let pending = fixture.root.path().join("pending-advances");
        fs::write(&pending, format!("{}\n", advances.join("\n"))).unwrap();
        let seen = fixture.root.path().join("gated-commits");
        fs::write(&seen, format!("{setup}\n{}\n", advances.join("\n"))).unwrap();
        // Every cargo call takes a second, so the gate has a measurable cost.
        fixture.add_fake_cargo(&format!(
            "sleep 1\noid=$(/usr/bin/git rev-parse HEAD)\nnext=$(head -n 1 '{pending}')\nif [ -n \"$next\" ] && ! grep -qx \"$oid\" '{seen}'; then\n  printf '%s\\n' \"$oid\" >> '{seen}'\n  tail -n +2 '{pending}' > '{pending}.tmp'; mv '{pending}.tmp' '{pending}'\n  /usr/bin/git --git-dir='{bare}' update-ref refs/heads/rolling \"$next\" || exit 43\nfi",
            pending = pending.display(),
            seen = seen.display(),
            bare = fixture.bare.display(),
        ));
        let old_path = use_fake_path(&fixture);
        let knobs = with_lander_vars([
            ("RSI_LANDER_EXACT_GATE", "1"),
            ("RSI_LANDER_REGATE_BUDGET_SECS", "1"),
        ]);
        let result = fixture
            .land(vec![AcceptedPair {
                base: setup.clone(),
                source,
            }])
            .await;
        drop(knobs);
        restore_path(old_path);
        let failure = result.expect_err("the gate cannot run again inside one second");
        assert_eq!(failure.state, PublicationState::NotPublished, "{failure:?}");
        assert!(
            failure.message.contains("stale retries exhausted")
                && failure.message.contains("re-gate wall-time budget"),
            "{failure:?}"
        );
        assert_eq!(
            failure.observed_tip.as_deref(),
            advances.last().map(String::as_str)
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            *advances.last().unwrap(),
            "nothing was published"
        );
    }

    fn spec_of(commands: Vec<GuardCommand>) -> GuardSpec {
        GuardSpec {
            commands,
            env: BTreeMap::new(),
            output_tail_bytes: 1024,
        }
    }

    fn reuse_of(tests: stale_delta::Scope, checks: stale_delta::Scope) -> GateReuse {
        GateReuse {
            verdict: stale_delta::Verdict {
                reason: "test".into(),
                tests,
                checks,
                python_everything: false,
                family_closed: false,
                family_filters: Vec::new(),
            },
            python: Vec::new(),
            family: None,
        }
    }

    fn scope(items: &[&str]) -> stale_delta::Scope {
        stale_delta::Scope::Only(items.iter().map(|item| (*item).to_owned()).collect())
    }

    #[test]
    fn a_stale_regate_keeps_only_the_commands_its_advance_selects() {
        let python = GuardCommand {
            program: "python3".into(),
            args: vec!["-B".into(), "-m".into(), "unittest".into(), "x".into()],
            timeout: Duration::from_secs(1),
        };
        let other_python = GuardCommand {
            args: vec!["-B".into(), "-m".into(), "pytest".into(), "y".into()],
            ..python.clone()
        };
        let static_guard = GuardCommand {
            program: "git".into(),
            args: vec!["diff".into(), "--check".into(), "a".into(), "b".into()],
            timeout: Duration::from_secs(1),
        };
        let spec = spec_of(vec![
            static_guard.clone(),
            cargo_check_command("rsid"),
            cargo_check_command("rsi"),
            rsid_focused_run_command(&["test(session)".to_owned()]),
            cargo_guard_command("rsi-common", None),
            cargo_guard_command("rsid", Some("bin:rsid")),
            cargo_guard_command("rsi", Some("test:e2e_tui")),
            e2e_prebuild_command(),
            python.clone(),
            other_python.clone(),
        ]);
        // Only the rsid bin target and one Python suite are selected.
        let mut reuse = reuse_of(scope(&["rsid:bin:rsid"]), stale_delta::Scope::Nothing);
        reuse.python = vec![python.clone()];
        let (reduced, reused, rerun) = reuse_spec(&spec, &reuse);
        assert_eq!(
            reduced.commands,
            vec![
                static_guard.clone(),
                cargo_guard_command("rsid", Some("bin:rsid")),
                python,
            ]
        );
        assert_eq!((reused, rerun), (7, 2));

        // A changed rsid crate: its family's tests and its compile check run,
        // the e2e prebuild follows the e2e test only when that runs.
        let reuse = reuse_of(scope(&["rsid:*", "rsi:test:e2e_tui"]), scope(&["rsid"]));
        let (reduced, ..) = reuse_spec(&spec, &reuse);
        assert!(reduced.commands.contains(&cargo_check_command("rsid")));
        assert!(!reduced.commands.contains(&cargo_check_command("rsi")));
        assert!(reduced.commands.contains(&e2e_prebuild_command()));
        assert!(
            !reduced
                .commands
                .contains(&cargo_guard_command("rsi-common", None))
        );
        assert!(
            reduced
                .commands
                .iter()
                .all(|command| *command != other_python)
        );

        // Nothing selected: only the static guard remains.
        let reuse = reuse_of(stale_delta::Scope::Nothing, stale_delta::Scope::Nothing);
        let (reduced, reused, rerun) = reuse_spec(&spec, &reuse);
        assert_eq!(reduced.commands, vec![static_guard]);
        assert_eq!((reused, rerun), (9, 0));

        // Everything selected: nothing is dropped.
        let mut reuse = reuse_of(
            stale_delta::Scope::Everything,
            stale_delta::Scope::Everything,
        );
        reuse.verdict.python_everything = true;
        let (reduced, reused, _) = reuse_spec(&spec, &reuse);
        assert_eq!(reduced.commands, spec.commands);
        assert_eq!(reused, 0);
    }

    /// #2546: a stale re-gate that reuses an earlier gate never takes the
    /// test-only omission, so `reuse_spec` still leaves a compile check.
    #[test]
    fn a_reusing_regate_keeps_its_compile_checks_despite_a_test_only_proof() {
        let mut reuse = reuse_of(scope(&["rsid:*"]), scope(&["rsid"]));
        reuse.family = Some(FamilyObservers::default());
        assert!(!test_only_omission_allowed(Some(&reuse)));
        assert!(!test_only_omission_allowed(Some(&GateReuse::every_test(
            "fixture"
        ))));
        assert!(test_only_omission_allowed(None));

        let mut spec = spec_for(&["rsid"], &["rsid=test(=session::tests::case)"]);
        if test_only_omission_allowed(Some(&reuse)) {
            omit_proven_test_only_checks(&mut spec.commands, &lib_proof("rsid"));
        }
        let (reduced, _, rerun) = reuse_spec(&spec, &reuse);
        assert!(
            reduced.commands.contains(&cargo_check_command("rsid")),
            "{:?}",
            reduced.commands
        );
        assert!(rerun >= 1, "{rerun}");
    }

    fn observers(names: &[&str], atoms: &[&str], targets: &[&str]) -> FamilyObservers {
        FamilyObservers {
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            atoms: atoms.iter().map(|atom| (*atom).to_owned()).collect(),
            targets: targets.iter().map(|target| (*target).to_owned()).collect(),
        }
    }

    fn atoms_of(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    /// #1957: an rsid advance re-runs only the focused atoms and family
    /// targets it can observe; what it cannot observe keeps its result.
    #[test]
    fn a_stale_family_advance_reruns_only_the_atoms_and_targets_it_observes() {
        let static_guard = GuardCommand {
            program: "git".into(),
            args: vec!["diff".into(), "--check".into(), "a".into(), "b".into()],
            timeout: Duration::from_secs(1),
        };
        let atoms = atoms_of(&[
            "test(=session::tests::spawns)",
            "test(=store::tests::rewinds)",
            "test(rolling_queue)",
            "test(/^topology::/)",
        ]);
        let spec = spec_of(vec![
            static_guard.clone(),
            cargo_check_command("rsid"),
            rsid_focused_run_command(&atoms),
            cargo_guard_command("rsid", Some("bin:rsid")),
            cargo_guard_command("rsid", Some("test:merge_queue")),
            cargo_guard_command("rsi", None),
        ]);
        let mut reuse = reuse_of(scope(&["rsid:*"]), scope(&["rsid"]));
        reuse.family = Some(observers(
            &["session::tests::spawns", "rolling_queue::tests::x"],
            &[],
            &["rsid:test:merge_queue"],
        ));
        let (reduced, reused, rerun) = reuse_spec(&spec, &reuse);
        assert_eq!(
            reduced.commands,
            vec![
                static_guard.clone(),
                cargo_check_command("rsid"),
                rsid_focused_run_command(&atoms_of(&[
                    "test(=session::tests::spawns)",
                    "test(rolling_queue)",
                ])),
                cargo_guard_command("rsid", Some("test:merge_queue")),
            ]
        );
        assert_eq!((reused, rerun), (2, 3));

        // The selector's family filters observe through their atoms: a
        // module atom overlaps only its own module, a substring any atom.
        reuse.family = Some(observers(&[], &["test(/^store::/)"], &[]));
        let (reduced, ..) = reuse_spec(&spec, &reuse);
        assert!(
            reduced
                .commands
                .contains(&rsid_focused_run_command(&atoms_of(&[
                    "test(=store::tests::rewinds)",
                    "test(rolling_queue)",
                ])))
        );
        assert!(
            !reduced
                .commands
                .contains(&cargo_guard_command("rsid", Some("bin:rsid")))
        );

        // An advance no family test observes (a bin-only or comment change
        // nothing reads) keeps the whole focused run; the check still runs.
        reuse.family = Some(FamilyObservers::default());
        let (reduced, reused, rerun) = reuse_spec(&spec, &reuse);
        assert_eq!(
            reduced.commands,
            vec![static_guard.clone(), cargo_check_command("rsid")]
        );
        assert_eq!((reused, rerun), (4, 1));

        // Every atom observed: the very command the gate ran.
        reuse.family = Some(observers(&[], &["test(tests)"], &["rsid:bin:rsid"]));
        let (reduced, ..) = reuse_spec(&spec, &reuse);
        assert!(reduced.commands.contains(&rsid_focused_run_command(&atoms)));
        assert!(
            reduced
                .commands
                .contains(&cargo_guard_command("rsid", Some("bin:rsid")))
        );

        // No observers (a crate the family builds against changed, or the
        // closure was unavailable): every selected family command runs whole.
        reuse.family = None;
        let (reduced, reused, rerun) = reuse_spec(&spec, &reuse);
        assert_eq!(reduced.commands, spec.commands[..5].to_vec());
        assert_eq!((reused, rerun), (1, 4));

        // Observers never widen what the verdict leaves alone.
        let mut other = reuse_of(scope(&["rsi:*"]), stale_delta::Scope::Nothing);
        other.family = Some(observers(
            &["session::tests::spawns"],
            &[],
            &["rsid:bin:rsid"],
        ));
        let (reduced, ..) = reuse_spec(&spec, &other);
        assert_eq!(
            reduced.commands,
            vec![static_guard, cargo_guard_command("rsi", None)]
        );
    }

    #[test]
    fn focused_matchers_overlap_only_where_one_test_name_could_match_both() {
        let parse = |atom: &str| FocusedMatch::parse(atom).unwrap();
        for (mine, theirs, overlap) in [
            ("test(=a::b)", "test(=a::b)", true),
            ("test(=a::b)", "test(=a::c)", false),
            ("test(=a::b)", "test(a::)", true),
            ("test(=a::b)", "test(/^b::/)", false),
            ("test(/^a::/)", "test(/^a::/)", true),
            ("test(/^a::/)", "test(/^b::/)", false),
            ("test(/^a::/)", "test(x)", true),
            ("test(x)", "test(y)", true),
            ("all()", "test(=a::b)", true),
            ("test(=a::b)", "all()", true),
        ] {
            assert_eq!(
                parse(mine).may_overlap(&parse(theirs)),
                overlap,
                "{mine} {theirs}"
            );
            assert_eq!(
                parse(theirs).may_overlap(&parse(mine)),
                overlap,
                "{theirs} {mine}"
            );
        }
        let observed = observers(&["a::b"], &["test(/^m::/)"], &[]);
        assert!(observed.observes_atom("test(=a::b)"));
        assert!(observed.observes_atom("test(a::)"));
        assert!(observed.observes_atom("all()"));
        assert!(observed.observes_atom("test(=m::x)"));
        assert!(!observed.observes_atom("test(=c::d)"));
        assert!(!observed.observes_atom("test(/^c::/)"));
        assert!(
            observed.observes_atom("test(/^a.*b/)"),
            "an atom the lander cannot match is always kept"
        );
        // An advance filter the lander cannot match overlaps everything.
        assert!(observers(&[], &["test(/x|y/)"], &[]).observes_atom("test(=c::d)"));
        assert!(!FamilyObservers::default().observes_atom("all()"));
    }

    #[test]
    fn only_advances_that_reran_nothing_since_the_last_full_gate_skip_the_canary() {
        let retry = |reuse| StaleRetry {
            fetched: "a".into(),
            observed: "b".into(),
            reuse,
            reason: String::new(),
        };
        use StaleReuse::{Partial, Proven, Regated, Whole};
        for (chain, skipped) in [
            (vec![], None),
            (vec![Proven], Some(1)),
            (vec![Proven, Proven], Some(2)),
            (vec![Partial, Regated, Proven], Some(1)),
            (vec![Regated], None),
            (vec![Partial, Proven], None),
            (vec![Proven, Partial], None),
            (vec![Whole], None),
        ] {
            let retries: Vec<StaleRetry> = chain.iter().copied().map(retry).collect();
            assert_eq!(proven_since_full_gate(&retries), skipped, "{chain:?}");
        }
    }

    #[test]
    fn gate_commands_name_the_test_target_they_run() {
        for (command, target) in [
            (
                cargo_guard_command("rsi-common", None),
                Some("rsi-common:lib"),
            ),
            (cargo_guard_command("rsid-store", None), Some("rsid:lib")),
            (
                cargo_guard_command("rsid", Some("bin:rsid")),
                Some("rsid:bin:rsid"),
            ),
            (
                cargo_guard_command("rsi", Some("test:e2e_tui:test(=x)")),
                Some("rsi:test:e2e_tui"),
            ),
            (
                rsid_focused_run_command(&["test(a)".to_owned()]),
                Some("rsid:lib"),
            ),
            (cargo_check_command("rsid"), None),
        ] {
            assert_eq!(
                command_test_target(&command).as_deref(),
                target,
                "{command:?}"
            );
        }
        assert_eq!(check_package(&cargo_check_command("rsi")), Some("rsi"));
        assert_eq!(check_package(&cargo_guard_command("rsi", None)), None);
    }

    #[test]
    fn selector_receipts_parse_to_their_suggested_filters() {
        assert_eq!(
            parse_selector_filters("{\"suggested_filters\": [\"rsid=bin:rsid\", \"rsi=x\"]}"),
            Some(vec!["rsid=bin:rsid".to_owned(), "rsi=x".to_owned()])
        );
        assert_eq!(
            parse_selector_filters("{\"suggested_filters\": []}"),
            Some(Vec::new())
        );
        for unreadable in ["", "not json", "{}", "{\"suggested_filters\": [1]}"] {
            assert_eq!(parse_selector_filters(unreadable), None, "{unreadable}");
        }
    }

    /// #1940 (c): a refused candidate cleanup says why.
    #[test]
    fn a_not_engine_candidate_refusal_names_its_cause() {
        let refused = IntegrationError::Refused(Refusal::NotEngineCandidate);
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("candidate");
        fs::create_dir_all(&worktree).unwrap();
        git_run(&worktree, &["init", "-q"]);
        let clean = cleanup_failure(&worktree, &refused);
        assert!(
            clean.contains("is clean but is not a registered"),
            "{clean}"
        );
        write(&worktree, "left-behind.txt", "x");
        let dirty = cleanup_failure(&worktree, &refused);
        assert!(
            dirty.contains("not clean") && dirty.contains("left-behind.txt"),
            "{dirty}"
        );
        let gone = cleanup_failure(&root.path().join("missing"), &refused);
        assert!(gone.contains("is gone"), "{gone}");
        let other = IntegrationError::Refused(Refusal::NotFastForward);
        assert_eq!(cleanup_failure(&worktree, &other), other.to_string());
    }

    #[test]
    fn only_an_added_declaration_makes_a_source_provisional() {
        let root = TempDir::new().unwrap();
        let repo = root.path();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        let declaration = repo.join("tools/provisional-migrations/released.json");
        fs::create_dir_all(declaration.parent().unwrap()).unwrap();
        fs::write(&declaration, "{}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        let base = git(&["rev-parse", "HEAD"]);

        fs::write(&declaration, "{\"path_template\": \"x\"}\n").unwrap();
        git(&["commit", "-q", "-am", "repair released declaration"]);
        let repaired = git(&["rev-parse", "HEAD"]);
        let pair = AcceptedPair {
            base: base.clone(),
            source: repaired,
        };
        assert!(!has_provisional_declaration(repo, &pair).unwrap());

        git(&["rm", "-q", "tools/provisional-migrations/released.json"]);
        git(&["commit", "-q", "-m", "drop released declaration"]);
        let dropped = AcceptedPair {
            base: base.clone(),
            source: git(&["rev-parse", "HEAD"]),
        };
        assert!(!has_provisional_declaration(repo, &dropped).unwrap());

        let added = repo.join("tools/provisional-migrations/new.json");
        fs::create_dir_all(added.parent().unwrap()).unwrap();
        fs::write(&added, "{}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "add declaration"]);
        let adding = AcceptedPair {
            base,
            source: git(&["rev-parse", "HEAD"]),
        };
        assert!(has_provisional_declaration(repo, &adding).unwrap());
    }

    #[test]
    fn provisional_plain_merge_accepts_already_landed_migration_on_stale_retry() {
        let already_landed = serde_json::json!({
            "renumber_noop": true,
            "migration_already_landed": true,
        });
        assert!(provisional_plain_merge(&already_landed, false));
        assert!(provisional_plain_merge(&already_landed, true));

        let head_plus_one = serde_json::json!({"renumber_noop": true});
        assert!(provisional_plain_merge(&head_plus_one, false));
        assert!(!provisional_plain_merge(&head_plus_one, true));
        assert!(!provisional_plain_merge(&serde_json::json!({}), false));
    }

    /// #452: the stale-retry fixture of `provisional_stale_retry_regenerates_or_refuses_proof`,
    /// built in Rust with the real `rsi-repo-guard` (the Python builder #361 removed).
    /// Returns the source repository, then `(destination clone, bare remote, base, tip, source, incoming)`;
    /// `destination` is what the lander lands in, `incoming` is what lands on the
    /// remote while the candidate's gate runs.
    fn provisional_stale_fixture(
        fixture: &Fixture,
        guard: &Path,
        change: &str,
    ) -> (
        provisional_fixture::Provisional,
        PathBuf,
        PathBuf,
        String,
        String,
        String,
        String,
    ) {
        use provisional_fixture::{Provisional, git};
        let protected_path = "crates/rsid-store/src/store/protected_fixture.rs";
        let protected_text = "// RSI-RELEASED-MIGRATION-BEGIN: fixture-section\n// pinned\n// RSI-RELEASED-MIGRATION-END: fixture-section\n";
        let shards = "#!/bin/sh\noid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> \"$LANDER924_LOG\"\nif [ \"$oid\" != \"$LANDER924_BASE\" ] && [ ! -e \"$LANDER924_MARKER\" ]; then\n  : > \"$LANDER924_MARKER\"\n  if [ \"$LANDER924_CHANGE\" = proof ]; then\n    : > \"$LANDER924_PROOF_FAIL\"\n  fi\n  /usr/bin/git -C \"$LANDER924_REPO\" push -q origin \"$LANDER924_INCOMING:refs/heads/rolling\" || exit 43\nfi\nexit 0\n";
        let case = Provisional::new(
            guard,
            &[
                (protected_path, protected_text, false),
                ("scripts/run-rsid-test-shards.sh", shards, true),
            ],
        );
        let source = case.source("alpha", "");
        git(
            &case.repo,
            &["update-ref", "refs/heads/source-for-landing", &source],
        );
        // #1655: a source already at rolling head + 1 lands as is and has no
        // proof to go stale. Another provisional unit lands first, so alpha
        // (declared V130) is renumbered to V131 and a stale retry has a proof to
        // regenerate or refuse. `advance` is the opposite: alpha starts at head
        // + 1 (a no-op) and only a stale advance of the head renumbers it.
        let tip = if change == "advance" {
            case.base.clone()
        } else {
            case.candidate(&case.source("zero", ""), &case.base).1
        };
        git(
            &case.repo,
            &["update-ref", "refs/heads/tip-for-landing", &tip],
        );
        let incoming = if matches!(change, "version" | "advance") {
            case.candidate(&case.source("beta", ""), &tip).1
        } else {
            let worktree = case.root.path().join("incoming");
            git(
                &case.repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    worktree.to_str().unwrap(),
                    &tip,
                ],
            );
            let filename = match change {
                "store" => "crates/rsid-store/src/store/mod.rs",
                "catalog" => "tools/released-migrations.json",
                "cohort" => "crates/rsid-store/src/store/cohort_settlement.rs",
                "protected" => protected_path,
                _ => "incoming.txt",
            };
            match change {
                "store" | "catalog" => {
                    let path = worktree.join(filename);
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                }
                "cohort" | "protected" => {
                    let path = worktree.join(filename);
                    let text = fs::read_to_string(&path).unwrap();
                    fs::write(&path, format!("{text}// incoming\n")).unwrap();
                }
                _ => write(&worktree, filename, "incoming\n"),
            }
            git(&worktree, &["add", filename]);
            git(&worktree, &["commit", "-q", "-m", "incoming"]);
            git(&worktree, &["rev-parse", "HEAD"])
        };
        git(
            &case.repo,
            &["update-ref", "refs/heads/incoming-for-landing", &incoming],
        );
        let repo = fixture.root.path().join(format!("migration-{change}"));
        let bare = fixture.root.path().join(format!("migration-{change}.git"));
        git(
            fixture.root.path(),
            &["init", "--bare", "-q", bare.to_str().unwrap()],
        );
        git(
            &case.repo,
            &[
                "push",
                "-q",
                bare.to_str().unwrap(),
                &format!("{tip}:refs/heads/rolling"),
            ],
        );
        git(
            fixture.root.path(),
            &[
                "clone",
                "-q",
                case.repo.to_str().unwrap(),
                repo.to_str().unwrap(),
            ],
        );
        git(
            &repo,
            &["remote", "set-url", "origin", bare.to_str().unwrap()],
        );
        let base = case.base.clone();
        // The source repository keeps the objects and worktrees alive: the
        // caller holds `case` for the landing.
        (case, repo, bare, base, tip, source, incoming)
    }

    #[tokio::test]
    async fn provisional_stale_retry_regenerates_or_refuses_proof() {
        for change in [
            "disjoint",
            "store",
            "catalog",
            "cohort",
            "protected",
            "version",
            "proof",
            "advance",
        ] {
            let fixture = Fixture::new();
            let real = provisional_fixture::real_guard();
            let (_case, repo, bare, base, tip, source, incoming) =
                provisional_stale_fixture(&fixture, &real, change);
            let marker = fixture.root.path().join("advanced-once");
            let log = fixture.root.path().join("tested-commits");
            let proof_fail = fixture.root.path().join("proof-fails");
            // The lander runs this wrapper as the trusted guard; once the gate
            // has marked the failure, a renumber proof fails like a changed tool.
            let wrapper = fixture.root.path().join("rsi-repo-guard");
            fs::write(
                &wrapper,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = rolling-migration-renumber ] && [ -e '{}' ]; then\n  echo 'injected renumber failure' >&2\n  exit 43\nfi\nexec '{}' \"$@\"\n",
                    proof_fail.display(),
                    real.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
            let _guard = landing_policy::use_guard_exe(&wrapper);
            let metadata = serde_json::json!({
                "workspace_members": ["rsid", "rsid-store"],
                "packages": [
                    {"id":"rsid","name":"rsid","dependencies":[]},
                    {"id":"rsid-store","name":"rsid-store","dependencies":[]}
                ],
            });
            let cargo = fixture.bin.join("cargo");
            // #1244: the provisional store tests run on the shared rsid lib
            // build (`cargo nextest`), so the fixture's shard double answers it.
            fs::write(&cargo, format!(
                "#!/bin/sh\nif [ \"$1\" = metadata ]; then printf '%s\\n' '{metadata}'; exit 0; fi\nif [ \"$1\" = nextest ]; then exec ./scripts/run-rsid-test-shards.sh; fi\nexit 0\n",
            )).unwrap();
            let mut permissions = fs::metadata(&cargo).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&cargo, permissions).unwrap();
            let old_path = use_fake_path(&fixture);
            let knobs = with_lander_vars([
                ("LANDER924_LOG", log.to_string_lossy().into_owned()),
                ("LANDER924_MARKER", marker.to_string_lossy().into_owned()),
                (
                    "LANDER924_PROOF_FAIL",
                    proof_fail.to_string_lossy().into_owned(),
                ),
                ("LANDER924_BASE", tip.clone()),
                ("LANDER924_REPO", repo.to_string_lossy().into_owned()),
                ("LANDER924_INCOMING", incoming.clone()),
                ("LANDER924_CHANGE", change.to_owned()),
            ]);
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
                fixes_tests: Vec::new(),
            })
            .await;
            drop(knobs);
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
                    "{change}: {failure:?}"
                );
                assert_eq!(
                    git_value(&bare, &["rev-parse", "refs/heads/rolling"]),
                    incoming
                );
            } else {
                let report = result.unwrap_or_else(|failure| {
                    panic!("{change}: stale provisional migration must publish: {failure:?}")
                });
                assert_eq!(report.fetched_tip, incoming);
                assert_eq!(report.provisional.len(), 1);
                assert_eq!(report.provisional[0].proof["target"], incoming);
                if change == "advance" {
                    assert_eq!(report.provisional[0].assigned_version, 131);
                }
                assert_eq!(report.canary_reused_tree.is_some(), change != "disjoint");
                assert_eq!(
                    git_value(&bare, &["rev-parse", "refs/heads/rolling"]),
                    report.published_tip
                );
            }
        }
    }

    #[test]
    fn committed_landing_guard_replaces_dirty_worktree_copy() {
        let fixture = Fixture::new();
        let script = "scripts/rolling-landing-guard.py";
        let committed = git_output(&fixture.repo, &["show", &format!("HEAD:{script}")])
            .expect("committed script");
        write(&fixture.repo, script, "raise SystemExit(0)\n");
        copy_committed_script(&fixture.repo, script).expect("restore committed script");
        assert_eq!(
            fs::read(fixture.repo.join(script)).unwrap(),
            committed.stdout
        );
    }

    /// #1949: a source that never touched the pin file lands with the pin the
    /// lander derives and commits on its candidate.
    #[test]
    fn lander_derives_the_released_pin_for_a_source_that_left_it_alone() {
        const STORE: &str = "crates/rsid-store/src/store/mod.rs";
        let store = |head: u32, v1: &str| {
            let mut text = format!(
                "pub const LATEST_SCHEMA_VERSION: i32 = {head};\n// V0: Original schema\ncreate_table();\n// V1: Session metadata columns\nif version < 1 {{\n    {v1}();\n}}\n"
            );
            if head >= 2 {
                text.push_str("if version < 2 {\n    migrate_v2();\n}\n");
            }
            text
        };
        let _lock = env_lock();
        let root = tempfile::tempdir().expect("temp root");
        // A host that exports a git identity (the cloud sweep does, #4012) must not
        // change who the pin commit is attributed to (#4018).
        let _identity = with_lander_vars([
            ("GIT_AUTHOR_NAME", "rsi-sweep"),
            ("GIT_AUTHOR_EMAIL", "rsi-sweep@invalid"),
            ("GIT_COMMITTER_NAME", "rsi-sweep"),
            ("GIT_COMMITTER_EMAIL", "rsi-sweep@invalid"),
        ]);
        // The real guard is a separate executable with its own native suite in
        // `rsi-repo-guards`; this stand-in keeps the lander side under test: the
        // pin commit, its identity and parent, and what a failing refresh or
        // check does. It refuses an edited released block.
        let guard = root.path().join("rsi-repo-guard");
        fs::write(
            &guard,
            r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
[ "$1" = released-migrations ] || { echo "unexpected guard $1" >&2; exit 2; }
shift
store=crates/rsid-store/src/store/mod.rs
if [ "$1" = --refresh ]; then
  grep -q migrate_v1_edited "$store" && { echo "released block edited" >&2; exit 1; }
  head=$(sed -n "s/.*LATEST_SCHEMA_VERSION: i32 = \([0-9]*\);.*/\1/p" "$store")
  blocks="\"1\":\"pin-1\""
  [ "$head" -ge 2 ] && blocks="$blocks,\"2\":\"pin-2\""
  printf "{\"schema_version\":1,\"latest_schema_version\":%s,\"blocks\":{%s}}\n" "$head" "$blocks" > tools/released-migrations.json
  exit 0
fi
git show "$2:$store" | grep -q migrate_v1_edited && { echo "released block edited" >&2; exit 1; }
exit 0
"#,
        )
        .unwrap();
        fs::set_permissions(&guard, fs::Permissions::from_mode(0o755)).unwrap();
        let _guard = landing_policy::use_guard_exe(&guard);
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).expect("repository directory");
        git_run(&repo, &["init", "-q", "-b", "rolling"]);
        fs::create_dir_all(repo.join("tools")).expect("tools dir");
        write(&repo, STORE, &store(1, "migrate_v1"));
        git_run(&repo, &["add", "."]);
        git_run(&repo, &["commit", "-q", "-m", "base"]);
        // Bootstrap the base's pin the way a released tree already has one.
        let pinned = landing_policy::run_repo_guard(
            &repo,
            "HEAD",
            &repo,
            "released-migrations",
            &["--refresh", "HEAD"],
        );
        assert!(pinned.success(), "{}", pinned.reason());
        git_run(&repo, &["add", "tools/released-migrations.json"]);
        git_run(&repo, &["commit", "-q", "-m", "pin base"]);
        let base = git_value(&repo, &["rev-parse", "HEAD"]);
        let manifest_at = |revision: &str| {
            git_value(
                &repo,
                &[
                    "show",
                    &format!("{revision}:tools/released-migrations.json"),
                ],
            )
        };
        let candidate_in = |name: &str, source: &str| {
            let worktree = root.path().join(name);
            git_run(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    worktree.to_str().unwrap(),
                    source,
                ],
            );
            worktree
        };
        let source_on_base = |file: &str, content: &str, message: &str| {
            let worktree = candidate_in(&format!("build-{message}"), &base);
            write(&worktree, file, content);
            git_run(&worktree, &["add", file]);
            git_run(&worktree, &["commit", "-q", "-m", message]);
            let oid = git_value(&worktree, &["rev-parse", "HEAD"]);
            git_run(
                &repo,
                &["worktree", "remove", "--force", worktree.to_str().unwrap()],
            );
            oid
        };

        // A new migration with no pin edit: the guard accepts it as deferred,
        // and the lander commits the pin on top of the candidate.
        let source = source_on_base(STORE, &store(2, "migrate_v1"), "migration-2");
        assert_eq!(manifest_at(&source), manifest_at(&base));
        landing_policy::check_released_migrations(&repo, &repo, &base, &source)
            .expect("the guard defers the pin of an unpinned new migration");
        let worktree = candidate_in("candidate", &source);
        let derived = landing_policy::derive_released_pin(&repo, &worktree, &base, &source)
            .expect("derivation runs")
            .expect("a pin commit is made");
        assert_eq!(git_value(&worktree, &["rev-parse", "HEAD"]), derived);
        assert_eq!(
            git_value(&repo, &["rev-parse", &format!("{derived}^")]),
            source
        );
        let pinned: Value = serde_json::from_str(&manifest_at(&derived)).unwrap();
        assert_eq!(pinned["latest_schema_version"], 2);
        assert!(pinned["blocks"]["2"].is_string(), "{pinned}");
        landing_policy::check_released_migrations(&repo, &repo, &base, &derived)
            .expect("the derived pin passes the strict guard");
        assert_eq!(
            landing_policy::pin_commit_parent(&repo, &derived).unwrap(),
            Some(source.clone())
        );
        assert_eq!(
            landing_policy::pin_commit_parent(&repo, &source).unwrap(),
            None
        );
        // Deriving again on the derived candidate changes nothing.
        assert_eq!(
            landing_policy::derive_released_pin(&repo, &worktree, &base, &derived).unwrap(),
            None
        );
        git_run(
            &repo,
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        );

        // A change a pin cannot cover (a released block edited) derives nothing,
        // and the guard that follows still refuses it.
        let edited = source_on_base(STORE, &store(1, "migrate_v1_edited"), "edit-released");
        let worktree = candidate_in("edited", &edited);
        assert_eq!(
            landing_policy::derive_released_pin(&repo, &worktree, &base, &edited).unwrap(),
            None
        );
        assert!(landing_policy::check_released_migrations(&repo, &repo, &base, &edited).is_err());
        git_run(
            &repo,
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        );

        // A change outside the store never reaches the pin file.
        let docs = source_on_base("AGENTS.md", "policy\n", "docs");
        let worktree = candidate_in("docs", &docs);
        assert_eq!(
            landing_policy::derive_released_pin(&repo, &worktree, &base, &docs).unwrap(),
            None
        );
        git_run(
            &repo,
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        );
    }

    #[test]
    fn hermetic_hot_file_needs_no_claim() {
        // Operator P0 (2026-09-29): no exclusive hot-file claims; fast-forward
        // publication serializes landings.
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "AGENTS.md", "policy\n", "hot file");
        landing_policy::check_with_proof(&fixture.repo, &fixture.repo, &fixture.base, &source, &[])
            .expect("a hot file lands without a claim");
    }

    #[test]
    fn hermetic_migration_must_be_exactly_tip_plus_one_with_its_block() {
        let fixture = Fixture::new();
        let store = "crates/rsid-store/src/store/mod.rs";
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
        let _released_guard = landing_policy::stub_released_guard(0);
        let check = |source: &str, proved: &[u32]| {
            landing_policy::check_with_proof(
                &fixture.repo,
                fixture.root.path(),
                &v124,
                source,
                proved,
            )
        };
        check(&v125, &[]).expect("tip + 1 with its block lands without a ledger");
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
        let file =
            |version: u32| format!("crates/rsid-store/src/store/migrations/v{version:03}.rs");
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
        let _released_guard = landing_policy::stub_released_guard(0);
        let check = |source: &str| {
            landing_policy::check_with_proof(&fixture.repo, fixture.root.path(), &v124, source, &[])
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
        let file =
            |version: u32| format!("crates/rsid-store/src/store/migrations/v{version:03}.rs");
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
        let _released_guard = landing_policy::stub_released_guard(0);
        let check = |source: &str| {
            landing_policy::check_with_proof(&fixture.repo, fixture.root.path(), &v124, source, &[])
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

    /// Commit on `parent`: rename `from` to `to` and optionally rewrite `to`.
    fn commit_move(
        fixture: &Fixture,
        parent: &str,
        from: &str,
        to: &str,
        extra: Option<(&str, &str)>,
    ) -> String {
        let path = fixture
            .root
            .path()
            .join(format!("mover-{}", uuid::Uuid::new_v4()));
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
        fs::create_dir_all(path.join(to).parent().unwrap()).unwrap();
        git_run(&path, &["mv", from, to]);
        if let Some((file, content)) = extra {
            write(&path, file, content);
            git_run(&path, &["add", file]);
        }
        git_run(&path, &["commit", "-q", "-m", "move"]);
        let oid = git_value(&path, &["rev-parse", "HEAD"]);
        git_run(
            &fixture.repo,
            &["worktree", "remove", "--force", path.to_str().unwrap()],
        );
        oid
    }

    /// #1021 S4: `rsid-store/build.rs` runs only the new directory, so a
    /// versioned unit left or added in the pre-split directory is refused
    /// (the number and block checks would otherwise accept a migration the
    /// daemon never executes), while a pre-split BASE is still read from its
    /// own layout.
    #[test]
    fn hermetic_migrations_must_live_in_the_directory_the_store_build_consumes() {
        let fixture = Fixture::new();
        let old_file = |version: u32| format!("crates/rsid/src/store/migrations/v{version:03}.rs");
        let new_file =
            |version: u32| format!("crates/rsid-store/src/store/migrations/v{version:03}.rs");
        let gate = |version: u32| {
            format!(
                "impl Store {{\n    fn migrate_v{version:03}(&self, version: i32) {{\n        if version < {version} {{\n        }}\n    }}\n}}\n"
            )
        };
        let _released_guard = landing_policy::stub_released_guard(0);
        let check = |target: &str, source: &str| {
            landing_policy::check_with_proof(
                &fixture.repo,
                fixture.root.path(),
                target,
                source,
                &[],
            )
        };
        // A pre-split base at V124.
        let pre_split = fixture.commit(&fixture.base, &old_file(124), &gate(124), "pre-split V124");
        // The S4 shape: the store tree (here its V124 unit) moves to the new
        // directory; the old base's head is read from the old directory.
        let split = commit_move(&fixture, &pre_split, &old_file(124), &new_file(124), None);
        check(&pre_split, &split).expect("the layout move itself lands from a pre-split base");
        // The first migration after the split is V125 in the new directory,
        // counted from the pre-split base's head.
        let next = fixture.commit(
            &split,
            &new_file(125),
            &gate(125),
            "V125 in the new directory",
        );
        check(&pre_split, &next).expect("V124 -> V125 across the layout move lands");
        check(&split, &next).expect("V125 in the new directory lands on a split base");
        // A bare old-path addition: valid V125 block, but in the dead directory.
        let stray = fixture.commit(
            &split,
            &old_file(125),
            &gate(125),
            "V125 in the old directory",
        );
        let refused = check(&split, &stray).expect_err("an old-path migration must refuse");
        assert_eq!(refused.fence, landing_policy::PolicyFence::MigrationNumber);
        assert!(
            refused
                .message
                .contains("crates/rsid-store/src/store/migrations")
                && refused.message.contains(&old_file(125)),
            "{}",
            refused.message
        );
        // The same stray unit next to a correct new-path unit still refuses.
        let mixed = fixture.commit(
            &next,
            &old_file(126),
            &gate(126),
            "V126 in the old directory",
        );
        assert!(check(&split, &mixed).is_err());
        // A candidate that falls back to the pre-split layout on a split target.
        let regress = commit_move(&fixture, &split, &new_file(124), &old_file(124), None);
        let fell_back = check(&split, &regress).expect_err("falling back refuses");
        assert_eq!(
            fell_back.fence,
            landing_policy::PolicyFence::MigrationNumber
        );
        assert!(
            fell_back
                .message
                .contains("crates/rsid-store/src/store/migrations")
        );
        // A pre-split base and candidate (history before the split) still land.
        let old_next = fixture.commit(&pre_split, &old_file(125), &gate(125), "pre-split V125");
        check(&pre_split, &old_next).expect("both sides pre-split keep working");
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
                    None,
                    &BTreeMap::new(),
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

    /// #1970: every changed crate gets a non-test build check, and the crate of
    /// the shipped binaries gets one whenever a change reaches it, after the
    /// dev-profile checks.
    #[test]
    fn release_build_checks_cover_changed_crates_and_the_shipped_bins() {
        let derive = |packages: &[&str]| {
            let packages: Vec<String> = packages.iter().map(|name| name.to_string()).collect();
            affected_crate_guard_spec(
                "1111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222",
                &packages,
                &[],
                false,
                &guard_workspace_metadata(),
                4,
                None,
                &BTreeMap::new(),
            )
            .unwrap()
            .commands
        };
        let commands = derive(&["rsi-common"]);
        assert!(commands.contains(&cargo_release_check_command("rsi-common")));
        assert!(commands.contains(&cargo_release_check_command("rsid")));
        assert!(!commands.contains(&cargo_release_check_command("rsi-baseline")));
        let position = |wanted: &GuardCommand| commands.iter().position(|c| c == wanted).unwrap();
        assert!(
            position(&cargo_check_command("rsid")) < position(&cargo_release_check_command("rsid")),
            "the release checks follow the dev-profile checks"
        );
        // A crate nothing ships is checked alone.
        let commands = derive(&["rsi-baseline"]);
        assert!(commands.contains(&cargo_release_check_command("rsi-baseline")));
        assert!(!commands.contains(&cargo_release_check_command("rsid")));
        // A docs-only change touches no crate and builds nothing.
        assert!(
            derive(&[])
                .iter()
                .all(|c| release_check_package(c).is_none())
        );
        // The dev-profile check is never mistaken for a release check.
        assert_eq!(release_check_package(&cargo_check_command("rsid")), None);
        assert_eq!(
            release_check_package(&cargo_release_check_command("rsid")),
            Some("rsid")
        );
    }

    /// #2424: a #2381-B-shaped change (the TUI and a shared-types crate)
    /// ran one cargo invocation per package; the selection is unchanged but
    /// the execution merges same-kind checks into two invocations.
    #[test]
    fn same_kind_compile_checks_run_as_one_cargo_invocation_each() {
        let packages = vec!["rsi".to_owned(), "rsi-common".to_owned()];
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &packages,
            &[],
            false,
            &guard_workspace_metadata(),
            4,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        let checks: Vec<&GuardCommand> = spec
            .commands
            .iter()
            .filter(|c| dev_check_package(c).is_some() || release_check_package(c).is_some())
            .collect();
        let singles = checks.len();
        let batches = coalesce_compile_checks(&checks);
        assert_eq!(batches.len(), 2, "one dev-profile and one release batch");
        assert!(singles > batches.len(), "{singles} checks were separate");
        let (dev, release) = (&batches[0], &batches[1]);
        assert!(!dev.release && release.release);
        assert_eq!(
            dev.command.args.last().map(String::as_str),
            Some("--all-targets")
        );
        assert_eq!(
            release.command.args.first().map(String::as_str),
            Some("check")
        );
        assert!(!release.command.args.iter().any(|a| a == "--all-targets"));
        // No package of the selection is dropped from either kind.
        for (batch, wanted) in [(dev, true), (release, false)] {
            for check in checks
                .iter()
                .filter(|c| dev_check_package(c).is_some() == wanted)
            {
                let package = dev_check_package(check)
                    .or_else(|| release_check_package(check))
                    .unwrap();
                assert!(
                    batch.packages.iter().any(|name| name == package),
                    "{package}"
                );
                let at = batch
                    .command
                    .args
                    .iter()
                    .position(|a| a == package)
                    .unwrap();
                assert_eq!(batch.command.args[at - 1], "-p");
            }
        }
        // The dependents of the changed crates are in the dev batch; the
        // shipped bins' crate is in the release batch.
        assert!(dev.packages.iter().any(|name| name == "rsid"));
        assert!(release.packages.iter().any(|name| name == "rsid"));
    }

    fn is_compile_check(command: &GuardCommand) -> bool {
        dev_check_package(command).is_some() || release_check_package(command).is_some()
    }

    fn spec_for(packages: &[&str], filters: &[&str]) -> GuardSpec {
        let packages: Vec<String> = packages.iter().map(|name| (*name).to_owned()).collect();
        let filters: Vec<String> = filters.iter().map(|filter| (*filter).to_owned()).collect();
        affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &packages,
            &filters,
            false,
            &guard_workspace_metadata(),
            4,
            None,
            &BTreeMap::new(),
        )
        .unwrap()
    }

    fn lib_proof(package: &str) -> test_impact::TestOnlyDiff {
        test_impact::TestOnlyDiff {
            packages: BTreeSet::from([package.to_owned()]),
            targets: BTreeSet::from([test_impact::TestTarget::Lib(package.to_owned())]),
        }
    }

    /// #2512: a test-only diff whose focused test build compiles the edit keeps
    /// the test commands and drops every compile check (dev, release, dependents).
    #[test]
    fn proven_test_only_diff_drops_the_compile_checks_it_covers() {
        let mut spec = spec_for(&["rsid"], &["rsid=test(=session::tests::case)"]);
        assert!(spec.commands.iter().any(is_compile_check));
        let tests: Vec<GuardCommand> = spec
            .commands
            .iter()
            .filter(|command| !is_compile_check(command))
            .cloned()
            .collect();
        let omitted = omit_proven_test_only_checks(&mut spec.commands, &lib_proof("rsid"));
        assert!(omitted >= 2, "dev and release checks of rsid: {omitted}");
        assert!(!spec.commands.iter().any(is_compile_check));
        assert_eq!(
            spec.commands, tests,
            "selected tests and static guards stay"
        );
        assert!(
            spec.commands
                .iter()
                .any(|c| rsid_lib_filterset(c).is_some())
        );
    }

    /// #2512: no test run compiles the edited target, so every check stays.
    #[test]
    fn test_only_diff_keeps_checks_when_no_selected_test_compiles_the_target() {
        let spec = spec_for(&["rsid"], &["rsid=test(=session::tests::case)"]);
        for proof in [
            // An integration test target nothing runs.
            test_impact::TestOnlyDiff {
                packages: BTreeSet::from(["rsid".to_owned()]),
                targets: BTreeSet::from([test_impact::TestTarget::Test(
                    "rsid".to_owned(),
                    "harness".to_owned(),
                )]),
            },
            // A library other than the one under test.
            lib_proof("rsi-graph"),
            // A proof with no target proves nothing.
            test_impact::TestOnlyDiff::default(),
        ] {
            let mut commands = spec.commands.clone();
            assert_eq!(omit_proven_test_only_checks(&mut commands, &proof), 0);
            assert_eq!(commands, spec.commands, "{proof:?}");
        }
        // A whole-shard run builds only its own shard features: not covered.
        let mut commands = vec![
            cargo_check_command("rsid"),
            cargo_release_check_command("rsid"),
            rsid_shard_guard_command("store-01").unwrap(),
        ];
        let before = commands.clone();
        assert_eq!(
            omit_proven_test_only_checks(&mut commands, &lib_proof("rsid")),
            0
        );
        assert_eq!(commands, before);
    }

    /// #2512: bin and integration targets are covered by their own `--bin` or
    /// `--test` run only.
    #[test]
    fn test_only_diff_covers_bin_and_integration_targets_by_their_own_run() {
        let proof = test_impact::TestOnlyDiff {
            packages: BTreeSet::from(["rsi".to_owned()]),
            targets: BTreeSet::from([
                test_impact::TestTarget::Test("rsi".to_owned(), "e2e".to_owned()),
                test_impact::TestTarget::Bin("rsi".to_owned(), "rsi".to_owned()),
            ]),
        };
        let both = |commands: &[GuardCommand]| {
            let mut commands = commands.to_vec();
            commands.push(cargo_check_command("rsi"));
            commands.push(cargo_release_check_command("rsi"));
            commands
        };
        let mut only_test = both(&[cargo_guard_command("rsi", Some("test:e2e"))]);
        assert_eq!(omit_proven_test_only_checks(&mut only_test, &proof), 0);
        let mut covered = both(&[
            cargo_guard_command("rsi", Some("test:e2e")),
            cargo_guard_command("rsi", Some("bin:rsi")),
        ]);
        assert_eq!(omit_proven_test_only_checks(&mut covered, &proof), 2);
        assert!(!covered.iter().any(is_compile_check));
        assert_eq!(covered.len(), 2);
    }

    /// #2424: a lone check stays byte-identical, a repeated package is named
    /// once, other commands keep their place, and a merged batch keeps the
    /// longest timeout.
    #[test]
    fn coalescing_keeps_lone_checks_other_commands_and_the_longest_timeout() {
        let lone = cargo_check_command("demo");
        let release = cargo_release_check_command("demo");
        let build = e2e_prebuild_command();
        let batches = coalesce_compile_checks(&[&lone, &build, &release]);
        assert_eq!(
            batches
                .iter()
                .map(|b| b.command.clone())
                .collect::<Vec<_>>(),
            vec![lone.clone(), build.clone(), release.clone()]
        );
        assert_eq!(batches[0].name, "check:demo");
        assert_eq!(batches[1].name, "cargo-build");
        assert_eq!(batches[2].name, "release-check:demo");

        let mut slow = cargo_check_command("other");
        slow.timeout = lone.timeout + Duration::from_secs(60);
        let merged = coalesce_compile_checks(&[&lone, &build, &slow, &lone]);
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged[0].command.args,
            ["check", "-p", "demo", "-p", "other", "--all-targets"]
        );
        assert_eq!(merged[0].command.timeout, slow.timeout);
        assert_eq!(merged[1].command, build);
    }

    /// #2424: a failed merged release check still names one crate, the typed
    /// `release_build_failed:<crate>` code of #1970.
    #[test]
    fn a_failed_merged_release_check_names_the_crate_cargo_could_not_compile() {
        let packages = vec!["rsi".to_owned(), "rsid-store".to_owned(), "rsid".to_owned()];
        assert_eq!(
            failed_release_package(&packages, "error: could not compile `rsid-store` (lib)"),
            "rsid-store"
        );
        assert_eq!(
            failed_release_package(&packages, "error: could not compile `serde` (lib)"),
            "rsi"
        );
        assert_eq!(failed_release_package(&packages, ""), "rsi");
    }

    /// #2445: cargo's output for a failed merged release check need not name
    /// the broken crate; the per-package rerun blames the one that fails alone.
    #[tokio::test]
    async fn a_failed_merged_release_check_blames_the_package_that_fails_standalone() {
        let fixture = Fixture::new();
        let tree = fixture.root.path().join("rerun-tree");
        fs::create_dir_all(&tree).unwrap();
        // The merged invocation fails without naming any crate; only the
        // standalone check of `beta` fails, and its output names nothing.
        fixture.add_fake_cargo(
            "if [ \"$*\" = 'check -p alpha -p beta' ]; then echo 'error: linking failed'; exit 101; fi\n\
             if [ \"$*\" = 'check -p beta' ]; then echo 'error: linking failed'; exit 101; fi",
        );
        let old_path = use_fake_path(&fixture);
        let spec = GuardSpec {
            commands: Vec::new(),
            env: BTreeMap::new(),
            output_tail_bytes: 64 * 1024,
        };
        let alpha = cargo_release_check_command("alpha");
        let beta = cargo_release_check_command("beta");
        let mut timings = CheckTimings::default();
        let error = run_compile_checks(&tree, &spec, &[&alpha, &beta], &mut timings)
            .await
            .expect_err("the merged release check fails");
        // The fast path (merged check passing) stays one invocation.
        let mut fast = CheckTimings::default();
        let gamma = cargo_release_check_command("gamma");
        let delta = cargo_release_check_command("delta");
        let fast_result = run_compile_checks(&tree, &spec, &[&gamma, &delta], &mut fast).await;
        restore_path(old_path);
        assert!(
            error.starts_with("release_build_failed:beta:"),
            "the first package failing standalone is blamed: {error}"
        );
        assert_eq!(
            timings
                .steps
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            [
                "release-check:alpha+beta",
                "release-check-rerun:alpha",
                "release-check-rerun:beta"
            ]
        );
        fast_result.expect("a passing merged check needs no rerun");
        assert_eq!(fast.steps.len(), 1);
    }

    /// #3018: a Python test command gets the 600 s bound, and its `gate_timing`
    /// step name is a space-free token naming the test file.
    #[test]
    fn a_python_test_command_is_bounded_at_the_named_constant_and_named_by_its_file() {
        assert_eq!(PYTHON_TEST_COMMAND_TIMEOUT, Duration::from_secs(600));
        let command = GuardCommand {
            program: "python3".into(),
            args: [
                "-B",
                "-m",
                "unittest",
                "-v",
                "scripts/tests/test_scoped_test.py",
            ]
            .map(String::from)
            .to_vec(),
            timeout: PYTHON_TEST_COMMAND_TIMEOUT,
        };
        assert!(is_python_test_guard(&command));
        let name = python_test_step_name(&command);
        assert_eq!(name, "python-test:scripts/tests/test_scoped_test.py");
        assert!(!name.contains(' '));
    }

    /// #2424: the per-step breakdown is one space-free token in `gate_timing`.
    #[test]
    fn the_check_steps_evidence_lists_every_step_in_order() {
        assert_eq!(check_steps_evidence(&[]), "none");
        let steps = vec![
            ("static-guards".to_owned(), 1),
            ("check:rsi+rsid-store".to_owned(), 300),
            ("release-check:rsi+rsid-store".to_owned(), 60),
        ];
        let evidence = check_steps_evidence(&steps);
        assert_eq!(
            evidence,
            "static-guards@1,check:rsi+rsid-store@300,release-check:rsi+rsid-store@60"
        );
        assert!(!evidence.contains(' '));
    }

    /// #1970: a library that compiles only with `test-seam` (enabled for tests
    /// through the dev-dependency self edge) passes the dev-profile check and
    /// is refused by the release-build check, naming the crate.
    #[tokio::test]
    async fn a_lib_that_compiles_only_with_test_seam_is_refused_by_the_release_check() {
        let fixture = Fixture::new();
        let tree = fixture.root.path().join("seam-lib");
        fs::create_dir_all(tree.join("src")).unwrap();
        fs::write(
            tree.join("Cargo.toml"),
            "[package]\nname = \"seamlib\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
             [features]\ntest-seam = []\n\n\
             [dev-dependencies]\nseamlib = { path = \".\", features = [\"test-seam\"] }\n",
        )
        .unwrap();
        // The seam type is gated; its use is not (the #1968/#2096 shape).
        fs::write(
            tree.join("src/lib.rs"),
            "#[cfg(any(test, feature = \"test-seam\"))]\npub struct Seam;\n\
             pub fn make() -> Seam { Seam }\n",
        )
        .unwrap();
        let spec = GuardSpec {
            commands: Vec::new(),
            env: BTreeMap::new(),
            output_tail_bytes: 64 * 1024,
        };
        let mut timings = CheckTimings::default();
        // The dev-profile check enables the seam through the dev-dependency.
        let dev = cargo_check_command("seamlib");
        run_compile_checks(&tree, &spec, &[&dev], &mut timings)
            .await
            .expect("the all-targets check sees test-seam and passes");
        assert_eq!(
            timings.release_secs, 0,
            "only release checks are timed as such"
        );
        assert_eq!(timings.steps.len(), 1);
        let release = cargo_release_check_command("seamlib");
        let error = run_compile_checks(&tree, &spec, &[&release], &mut timings)
            .await
            .expect_err("the non-test build cannot compile it");
        assert!(
            error.starts_with("release_build_failed:seamlib:"),
            "{error}"
        );
    }

    /// #1970: the gate refuses a candidate whose release-build check fails with
    /// the typed `release_build_failed:<crate>` code, before any test.
    #[tokio::test]
    async fn a_failing_release_check_refuses_the_landing_with_its_typed_code() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"changed\" }\n",
            "change",
        );
        // Only the non-test `cargo check -p demo` fails.
        fixture.add_fake_cargo(
            "if [ \"$*\" = 'check -p demo' ]; then echo 'error[E0425]: cannot find value'; exit 101; fi",
        );
        let old_path = use_fake_path(&fixture);
        FORCE_RELEASE_CHECKS.store(true, std::sync::atomic::Ordering::SeqCst);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        FORCE_RELEASE_CHECKS.store(false, std::sync::atomic::Ordering::SeqCst);
        restore_path(old_path);
        let failure = result.expect_err("a release build failure refuses the landing");
        assert_eq!(failure.state, PublicationState::NotPublished);
        assert!(
            failure.message.contains("release_build_failed:demo:"),
            "{failure:?}"
        );
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
            None,
            &BTreeMap::new(),
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
                None,
                &BTreeMap::new(),
            )
            .is_err()
        );
    }

    /// #1281: a focused gate runs one lib test build without shard features,
    /// so the static inventory check it runs must refuse a test hidden behind
    /// a shard-only enclosing cfg, which that build would never compile.
    #[test]
    fn inventory_check_refuses_a_shard_only_enclosing_module() {
        let check = rsid_shard_inventory_check();
        let source_script = PathBuf::from(rsi_common::manifest_dir!())
            .join("../../scripts/check-rsid-test-shards.py");
        let gate = |shard_cfg: &str| -> (bool, String) {
            let root = tempfile::tempdir().unwrap();
            let manifest = "[package]\nname = \"p\"\nversion = \"0.1.0\"\n[features]\n\
                            default = []\ntest-shard-mode = []\n\
                            test-shard-other-03 = [\"test-shard-mode\"]\n";
            let lib = format!(
                "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-03\"))]\n\
                 #[test]\nfn visible_green() {{}}\n\n\
                 {shard_cfg}\nmod hidden {{\n    \
                 #[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-03\"))]\n    \
                 #[test]\n    fn hidden_red() {{ panic!(\"x\"); }}\n}}\n"
            );
            for package in ["rsid", "rsid-store"] {
                let directory = root.path().join("crates").join(package);
                fs::create_dir_all(directory.join("src")).unwrap();
                fs::write(directory.join("Cargo.toml"), manifest).unwrap();
                fs::write(directory.join("src/lib.rs"), &lib).unwrap();
            }
            fs::create_dir_all(root.path().join("scripts")).unwrap();
            fs::copy(
                &source_script,
                root.path().join("scripts/check-rsid-test-shards.py"),
            )
            .unwrap();
            let output = std::process::Command::new(&check.program)
                .args(&check.args)
                .current_dir(root.path())
                .output()
                .unwrap();
            (
                output.status.success(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        };
        // The canonical gate on the module keeps it in the unfeatured build.
        let (passed, stderr) = gate(
            "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-03\"))]",
        );
        assert!(passed, "{stderr}");
        // Multi-line `all(test, any(..))` is the same predicate.
        let (passed, stderr) = gate(
            "#[cfg(all(\n    test,\n    any(not(feature = \"test-shard-mode\"), \
             feature = \"test-shard-other-03\")\n))]",
        );
        assert!(passed, "{stderr}");
        // A shard-only module compiles its test only with the shard feature.
        for hidden in [
            "#[cfg(feature = \"test-shard-other-03\")]",
            "#[cfg(all(test, feature = \"test-shard-other-03\"))]",
            "#[cfg_attr(feature = \"test-shard-other-03\", allow(dead_code))]",
        ] {
            let (passed, stderr) = gate(hidden);
            assert!(!passed, "{hidden} passed the inventory check");
            assert!(
                stderr.contains("src/lib.rs:5:") && stderr.contains("unfeatured")
                    || stderr.contains("cfg_attr"),
                "{stderr}"
            );
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
            None,
            &BTreeMap::new(),
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
                "cargo check -p rsid",
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

    /// #1510: every atom shape `check-touched-shards` emits reaches the shard
    /// script as one `--filterset` argv element, and the same atom in the
    /// focused form selects the shared lib build.
    #[test]
    fn shard_filters_accept_every_nextest_atom_shape() {
        let atoms = [
            "test(manager_recovery_)",
            "test(rpc::agent_x)",
            "test(=session::tests::event_wake)",
            "test(/^session::tests::manager_actions::[A-Za-z0-9_]+$/)",
            "test(/^session::big::(?:\\w+::)*(?:t3|t9)$/)",
            "test(/^session::big::(?:\\w+::)*t3$/)",
            "test(/^store::/)",
            "test(/a b/)",
            "test(/\\(/)",
        ];
        for atom in atoms {
            let command = rsid_shard_guard_command(&format!("other-03:{atom}")).unwrap();
            assert_eq!(command.program, "scripts/run-rsid-test-shards.sh", "{atom}");
            assert_eq!(
                command.args,
                ["shard", "other-03", "--jobs", "4", "--filterset", atom]
            );
            let focused =
                rsid_family_filter_command("rsid", &format!("shard:other-03:{atom}")).unwrap();
            assert_eq!(rsid_lib_filterset(&focused), Some(atom), "{atom}");
            let bare = rsid_family_filter_command("rsid", atom).unwrap();
            assert_eq!(rsid_lib_filterset(&bare), Some(atom), "{atom}");
        }
    }

    #[test]
    fn shard_filters_refuse_control_characters_and_malformed_atoms() {
        for atom in [
            "test()",
            "test(=)",
            "test(=a b)",
            "test(=a|b)",
            "test(//)",
            "test(/)",
            "test(/a)",
            "test(/a/",
            "test(/a)b/)",
            "test(/(a/)",
            "test(/a\\/)",
            "test(/a | b/)",
            "test(/a\nb/)",
            "test(/a\tb/)",
            "test(/a\u{7f}/)",
            "test(/é/)",
            "test(a) | test(b)",
            "all()",
            "not(test(gap))",
        ] {
            assert!(
                rsid_shard_guard_command(&format!("other-03:{atom}")).is_err(),
                "{atom:?}"
            );
        }
    }

    /// #1510: the filters `scripts/check-touched-shards` emitted for the
    /// synthetic diffs of `scripts/tests/test_check_touched_shards.py`, which
    /// fails when the script emits a shape missing from this fixture.
    #[test]
    fn touched_shard_filter_fixture_is_accepted() {
        let fixture = include_str!("../../../../scripts/tests/fixtures/touched-shard-filters.txt");
        let filters: Vec<&str> = fixture.lines().filter(|line| !line.is_empty()).collect();
        assert!(filters.len() >= 4, "{filters:?}");
        for filter in filters {
            let (package, value) = filter.split_once('=').unwrap();
            assert_eq!(package, "rsid");
            let shard = value.strip_prefix("shard:").unwrap();
            let command = rsid_shard_guard_command(shard).unwrap();
            let (_, _, filterset) = shard_command_parts(&command).unwrap();
            let focused = rsid_family_filter_command(package, value).unwrap();
            assert_eq!(rsid_lib_filterset(&focused), filterset, "{filter}");
            assert!(filterset.is_some(), "{filter}");
        }
    }

    fn rsid_family_metadata() -> WorkspaceMetadata {
        let package = |name: &str, dependencies: &[&str]| WorkspacePackage {
            id: name.into(),
            name: name.into(),
            dependencies: dependencies
                .iter()
                .map(|dependency| WorkspaceDependency {
                    name: (*dependency).into(),
                    path: Some(PathBuf::from(format!("crates/{dependency}"))),
                })
                .collect(),
        };
        WorkspaceMetadata {
            workspace_members: ["rsid-store", "rsid", "rsi"].map(str::to_owned).to_vec(),
            packages: vec![
                package("rsid-store", &[]),
                package("rsid", &["rsid-store"]),
                package("rsi", &[]),
            ],
        }
    }

    fn rendered(spec: &GuardSpec) -> Vec<String> {
        spec.commands
            .iter()
            .map(|command| format!("{} {}", command.program, command.args.join(" ")))
            .collect()
    }

    #[test]
    fn explicit_family_filters_do_not_create_a_bounded_qa_obligation() {
        let computed = test_impact::UnfilteredSelection {
            lib: test_impact::LibTests::Tests(BTreeSet::from(["direct".into()])),
            reached_tests: 500,
            targets: BTreeSet::new(),
            reached_targets: 0,
            leaf_tests: BTreeSet::new(),
        };
        assert_eq!(
            gate_selection_evidence(Some(&computed), true),
            "selection=closure reason=explicit_filters"
        );
        assert_eq!(
            gate_selection_evidence(Some(&computed), false),
            "selection=bounded selected=1 reached=500"
        );
        assert_eq!(
            gate_selection_evidence(None, false),
            "selection=everything reason=crate-wide_or_unreadable_diff"
        );
    }

    /// #2094: omitted bin/integration targets oblige the async full run even
    /// when every reached library test ran; explicit filters still do not.
    #[test]
    fn bounded_targets_oblige_the_async_full_run() {
        let computed = test_impact::UnfilteredSelection {
            lib: test_impact::LibTests::Tests(BTreeSet::from(["direct".into()])),
            reached_tests: 1,
            targets: BTreeSet::from([("rsid".to_owned(), "test:direct".to_owned())]),
            reached_targets: 40,
            leaf_tests: BTreeSet::new(),
        };
        let evidence = gate_selection_evidence(Some(&computed), false);
        assert_eq!(
            evidence,
            "selection=closure selected=1 reached=1 targets=bounded selected=1 reached=40"
        );
        assert!(obliges_full_qa(&evidence));
        assert!(obliges_full_qa(
            "selection=bounded selected=400 reached=7613"
        ));
        assert!(!obliges_full_qa("selection=closure selected=1 reached=1"));
        assert!(!obliges_full_qa(&gate_selection_evidence(
            Some(&computed),
            true
        )));
    }

    /// #2286: an excluded slow tier over rsid/rsid-store code obliges the
    /// async full run, for explicit filters and a closure alike; a full tier,
    /// or an exclusion over other crates, owes nothing.
    #[test]
    fn an_excluded_slow_tier_over_library_code_obliges_the_async_full_run() {
        let crates = |names: &[&str]| -> Vec<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };
        for base in [
            "selection=closure reason=explicit_filters",
            "selection=closure selected=3 reached=3",
        ] {
            assert!(!obliges_full_qa(base));
            for touched in [&["rsid"][..], &["rsid-store"][..]] {
                let evidence =
                    with_slow_tier_evidence(base.to_owned(), SlowTier::Excluded, &crates(touched));
                assert_eq!(evidence, format!("{base} slow_tier=excluded"));
                assert!(obliges_full_qa(&evidence));
            }
            for (tier, touched) in [
                (SlowTier::Full, &["rsid-store"][..]),
                (SlowTier::Unset, &["rsid"][..]),
                (SlowTier::Excluded, &["rsi-common"][..]),
            ] {
                let evidence = with_slow_tier_evidence(base.to_owned(), tier, &crates(touched));
                assert!(!obliges_full_qa(&evidence), "{evidence}");
            }
        }
    }

    #[test]
    fn long_nextest_filtersets_use_a_config_file_for_run_and_list() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".config")).unwrap();
        std::fs::write(root.path().join(".config/nextest.toml"),
            "[profile.default]\nfail-fast = false\ndefault-filter = 'not test(skip)'\n[profile.rsid-fast]\ninherits = 'default'\n").unwrap();
        let filter = (0..400)
            .map(|i| format!("test(=very_long_module_path::test_{i})"))
            .collect::<Vec<_>>()
            .join(" | ");
        for command in [
            rsid_lib_filter_command(&filter),
            rsid_lib_list_command(&filter),
        ] {
            let (executable, file) = nextest_filter_config(root.path(), &command).unwrap();
            let file = file.unwrap();
            assert!(executable.args.iter().map(String::len).sum::<usize>() < 1024);
            assert!(
                executable
                    .args
                    .windows(2)
                    .any(|a| a == ["--filterset", "default()"])
            );
            let config: toml::Value =
                toml::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
            assert_eq!(
                config["profile"]["rsid-fast"]["default-filter"].as_str(),
                Some(format!("(not test(skip)) & ({filter})").as_str())
            );
            assert_eq!(
                config["profile"]["default"]["fail-fast"].as_bool(),
                Some(false)
            );
            assert_eq!(command.args.last(), Some(&filter));
        }
        assert!(
            nextest_filter_config(root.path(), &rsid_lib_filter_command("test(=short)"))
                .unwrap()
                .1
                .is_none()
        );
    }

    fn has_arg(command: &GuardCommand, flag: &str) -> bool {
        command.args.iter().any(|arg| arg == flag)
    }

    /// #2270: the gate's tier shapes every rsid library nextest command.
    #[test]
    fn the_gate_slow_tier_shapes_the_rsid_library_commands() {
        let undecided = rsid_lib_filter_command("test(=a)");
        assert!(
            !has_arg(&undecided, "--no-tests") && !has_arg(&undecided, "--ignore-default-filter")
        );
        assert!(!has_arg(
            &rsid_lib_list_command("test(=a)"),
            "--ignore-default-filter"
        ));

        store_gate_slow_tier(SlowTier::Excluded);
        let excluded = rsid_lib_filter_command("test(=a)");
        // A focused set that holds only slow-tier tests selects nothing: warn, not fail.
        assert!(
            excluded
                .args
                .windows(2)
                .any(|pair| pair == ["--no-tests", "warn"])
        );
        assert!(!has_arg(&excluded, "--ignore-default-filter"));
        assert_eq!(rsid_lib_filterset(&excluded), Some("test(=a)"));
        let listing = rsid_lib_list_command("test(=a)");
        assert_eq!(listing.args[1], "list");
        // `list` takes no --no-tests and judges a filter by the tests that exist.
        assert!(!has_arg(&listing, "--no-tests") && has_arg(&listing, "--ignore-default-filter"));

        store_gate_slow_tier(SlowTier::Full);
        let full = rsid_lib_filter_command("test(=a)");
        assert!(has_arg(&full, "--ignore-default-filter") && !has_arg(&full, "--no-tests"));
        assert_eq!(rsid_lib_filterset(&full), Some("test(=a)"));
        let listing = rsid_lib_list_command("test(=a)");
        assert_eq!(
            listing
                .args
                .iter()
                .filter(|arg| *arg == "--ignore-default-filter")
                .count(),
            1
        );
        let direct = direct_base_shard_command(
            &rsid_shard_guard_command("session-05").unwrap(),
            ShardTree::Static,
        )
        .unwrap();
        assert!(has_arg(&direct, "--ignore-default-filter"));
        store_gate_slow_tier(SlowTier::Unset);
    }

    /// #2270: a long filterset moves into a config file whose default-filter is
    /// the profile's own intersected with the set; the slow tier must not get the
    /// profile's exclusion back through `default()`.
    #[test]
    fn a_slow_tier_run_keeps_the_whole_filterset_when_it_moves_into_a_config_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".config")).unwrap();
        std::fs::write(root.path().join(".config/nextest.toml"),
            "[profile.default]\nfail-fast = false\ndefault-filter = 'not test(skip)'\n[profile.rsid-fast]\ninherits = 'default'\n").unwrap();
        let filter = (0..400)
            .map(|i| format!("test(=very_long_module_path::test_{i})"))
            .collect::<Vec<_>>()
            .join(" | ");
        let default_filter_of = |command: &GuardCommand| {
            let (_, file) = nextest_filter_config(root.path(), command).unwrap();
            let config: toml::Value =
                toml::from_str(&std::fs::read_to_string(file.unwrap().path()).unwrap()).unwrap();
            config["profile"]["rsid-fast"]["default-filter"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        store_gate_slow_tier(SlowTier::Full);
        assert_eq!(
            default_filter_of(&rsid_lib_filter_command(&filter)),
            format!("(all()) & ({filter})")
        );
        store_gate_slow_tier(SlowTier::Excluded);
        assert_eq!(
            default_filter_of(&rsid_lib_filter_command(&filter)),
            format!("(not test(skip)) & ({filter})")
        );
        store_gate_slow_tier(SlowTier::Unset);
    }

    /// #2270: the lander follows `check-touched-shards --slow-tier-only`; anything but
    /// a clear `excluded` runs the tier.
    #[test]
    fn the_gate_slow_tier_follows_the_selector_and_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("scripts")).unwrap();
        let script = root.path().join("scripts/check-touched-shards");
        // A tree with no tier keeps the legacy commands, whatever the selector says.
        assert_eq!(
            slow_tier_for_gate(root.path(), "base", "head"),
            (SlowTier::Unset, "no_slow_tier_config".to_owned())
        );
        std::fs::create_dir(root.path().join(".config")).unwrap();
        std::fs::write(root.path().join(".config/slow-tier.toml"), "").unwrap();
        let decide = |body: &str| {
            std::fs::write(&script, body).unwrap();
            slow_tier_for_gate(root.path(), "base", "head")
        };
        assert_eq!(
            decide(
                "import json\nprint(json.dumps({'mode': 'excluded', 'reason': 'no_trigger'}))\n"
            ),
            (SlowTier::Excluded, "no_trigger".to_owned())
        );
        assert_eq!(
            decide("import json\nprint(json.dumps({'mode': 'full', 'reason': 'trigger_path'}))\n"),
            (SlowTier::Full, "trigger_path".to_owned())
        );
        assert_eq!(
            decide("import sys\nsys.exit(1)\n"),
            (SlowTier::Full, "decision_unavailable".to_owned())
        );
        assert_eq!(
            decide("print('not json')\n"),
            (SlowTier::Full, "decision_unavailable".to_owned())
        );
        std::fs::remove_file(&script).unwrap();
        assert_eq!(
            slow_tier_for_gate(root.path(), "base", "head"),
            (SlowTier::Full, "decision_unavailable".to_owned())
        );
    }

    /// #2299: a tree with no `.config/slow-tier.toml` keeps the legacy commands only
    /// when its nextest config provably excludes no test; a default-filter that is
    /// left over (or cannot be read) runs the whole suite.
    #[test]
    fn a_missing_slow_tier_manifest_fails_closed_while_nextest_still_excludes_tests() {
        let nextest_decision = |nextest: Option<&str>| {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join(".config")).unwrap();
            if let Some(text) = nextest {
                std::fs::write(root.path().join(".config/nextest.toml"), text).unwrap();
            }
            slow_tier_for_gate(root.path(), "base", "head")
        };
        let unset = (SlowTier::Unset, "no_slow_tier_config".to_owned());
        let full = (
            SlowTier::Full,
            "no_slow_tier_config_with_default_filter".to_owned(),
        );
        // Provably nothing is excluded: the legacy shape.
        assert_eq!(nextest_decision(None), unset);
        assert_eq!(
            nextest_decision(Some("[profile.default]\nretries = 0\n")),
            unset
        );
        assert_eq!(
            nextest_decision(Some(
                "[profile.default]\nretries = 0\n[profile.ci-full]\ndefault-filter = \"all()\"\n"
            )),
            unset
        );
        // The generated slow-tier exclusion without its manifest, any other filter, a
        // filter in an override and a config that cannot be read are not proof.
        assert_eq!(
            nextest_decision(Some(
                "[profile.default]\ndefault-filter = 'not (package(rsid-store) & test(/^(?:store::tests::a)$/))'\n"
            )),
            full
        );
        assert_eq!(
            nextest_decision(Some(
                "[profile.default]\n[[profile.default.overrides]]\nfilter = 'all()'\ndefault-filter = 'test(x)'\n"
            )),
            full
        );
        assert_eq!(
            nextest_decision(Some("[profile.default]\ndefault-filter = 7\n")),
            full
        );
        assert_eq!(nextest_decision(Some("this is = not [toml")), full);
        let unreadable = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(unreadable.path().join(".config/nextest.toml")).unwrap();
        assert_eq!(slow_tier_for_gate(unreadable.path(), "base", "head"), full);

        // The Full tier is the one that runs the excluded tests.
        store_gate_slow_tier(full.0);
        assert_eq!(
            slow_tier_run_args(),
            vec!["--ignore-default-filter".to_owned()]
        );
        store_gate_slow_tier(SlowTier::Unset);
    }

    fn targets(has_lib: bool, targets: &[&str]) -> test_impact::PackageTargets {
        test_impact::PackageTargets {
            has_lib,
            targets: targets.iter().map(|target| (*target).to_owned()).collect(),
            check: false,
        }
    }

    /// #1937, #1650: an unfiltered package outside the rsid family gates its
    /// library tests and every changed bin or integration test target; a
    /// package with no library target has no `--lib` run, and a filtered
    /// package keeps exactly its filters.
    #[test]
    fn unfiltered_packages_run_their_changed_bin_and_integration_targets() {
        let derived = BTreeMap::from([
            (
                "rsi-common".to_owned(),
                targets(true, &["test:command_validator_portability", "bin:rsi-rpc"]),
            ),
            ("rsi-graph".to_owned(), targets(false, &["bin:rsi-graph"])),
            // Ignored: rsi's explicit filter below scopes its gate.
            ("rsi".to_owned(), targets(true, &["test:flow"])),
        ]);
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsi-common".into(), "rsi-graph".into(), "rsi".into()],
            &["rsi=settings".into()],
            false,
            &guard_workspace_metadata(),
            4,
            None,
            &derived,
        )
        .unwrap();
        let commands = rendered(&spec);
        for expected in [
            "cargo test -p rsi-common --lib -- --test-threads=4",
            "cargo test -p rsi-common --test command_validator_portability -- --test-threads=4",
            "cargo test -p rsi-common --bin rsi-rpc -- --test-threads=4",
            "cargo test -p rsi-graph --bin rsi-graph -- --test-threads=4",
            "cargo test -p rsi --lib settings -- --test-threads=4",
        ] {
            assert!(
                commands.iter().any(|c| c == expected),
                "{expected}: {commands:#?}"
            );
        }
        assert!(
            !commands
                .iter()
                .any(|c| c.starts_with("cargo test -p rsi-graph --lib")),
            "{commands:#?}"
        );
        assert!(
            !commands.iter().any(|c| c.contains("--test flow")),
            "{commands:#?}"
        );
    }

    /// #1952: an unfiltered package whose production library code changed
    /// compiles every target of the package (`check --all-targets`) before its
    /// tests run, once, and a package that owes no check gets none.
    #[test]
    fn a_changed_library_compiles_every_target_of_its_package_first() {
        let mut checked = targets(true, &["test:corpus"]);
        checked.check = true;
        let derived = BTreeMap::from([
            ("rsi-common".to_owned(), checked),
            ("rsi-graph".to_owned(), targets(true, &[])),
        ]);
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsi-common".into(), "rsi-graph".into()],
            &[],
            false,
            &guard_workspace_metadata(),
            4,
            None,
            &derived,
        )
        .unwrap();
        let commands = rendered(&spec);
        let position = |wanted: &str| {
            commands
                .iter()
                .position(|c| c == wanted)
                .unwrap_or_else(|| panic!("{wanted}: {commands:#?}"))
        };
        let check = position("cargo check -p rsi-common --all-targets");
        assert!(check < position("cargo test -p rsi-common --lib -- --test-threads=4"));
        assert!(
            check < position("cargo test -p rsi-common --test corpus -- --test-threads=4"),
            "{commands:#?}"
        );
        assert_eq!(
            commands
                .iter()
                .filter(|c| c.as_str() == "cargo check -p rsi-common --all-targets")
                .count(),
            1,
            "{commands:#?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| c == "cargo check -p rsi-graph --all-targets"),
            "{commands:#?}"
        );
    }

    /// An unfiltered changed `e2e_tui` target runs with the harness's prebuilt
    /// daemon and `RSI_E2E=1`, or its tests would silently do nothing (#1596).
    #[test]
    fn a_derived_e2e_target_gets_its_prebuild_and_environment() {
        let derived = BTreeMap::from([("rsi".to_owned(), targets(true, &["test:e2e_tui"]))]);
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsi".into()],
            &[],
            false,
            &guard_workspace_metadata(),
            4,
            None,
            &derived,
        )
        .unwrap();
        let commands = rendered(&spec);
        let prebuild = commands
            .iter()
            .position(|c| c.starts_with("cargo build "))
            .expect("prebuild");
        let run = commands
            .iter()
            .position(|c| c == "cargo test -p rsi --test e2e_tui -- --test-threads=1")
            .expect("e2e run");
        assert!(prebuild < run, "{commands:#?}");
        assert_eq!(spec.env.get("RSI_E2E").map(String::as_str), Some("1"));
    }

    /// #1244: focused rsid filters in different shards (and a filter on the
    /// sibling package) share one lib test build without shard features; the
    /// unfiltered sibling gets its compile check, not all sixteen shards.
    #[test]
    fn focused_rsid_filters_share_one_lib_build_across_shards() {
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsid".into(), "rsid-store".into()],
            &[
                "rsid=shard:other-01:test(rolling_queue)".into(),
                "rsid=shard:session-02:test(manager_recovery_)".into(),
                "rsid=bin:rsid".into(),
            ],
            false,
            &rsid_family_metadata(),
            4,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        let lib = "cargo nextest run --profile rsid-fast -p rsid -p rsid-store --lib --status-level all --final-status-level all -j 8 --filterset";
        assert_eq!(
            rendered(&spec)[1..],
            [
                "python3 scripts/check-rsid-test-shards.py --require-gates".to_owned(),
                "cargo check -p rsid --all-targets".to_owned(),
                format!("{lib} test(rolling_queue) | test(manager_recovery_)"),
                "cargo test -p rsid --bin rsid -- --test-threads=4".to_owned(),
                "cargo check -p rsid-store --all-targets".to_owned(),
                "cargo check -p rsid".to_owned(),
                "cargo check -p rsid-store".to_owned(),
            ]
        );
        assert!(is_static_guard(&spec.commands[1]));
        let builds = shard_build_commands(&spec, ShardTree::Static);
        assert_eq!(builds, vec![rsid_lib_build_command()]);
        assert_eq!(
            builds[0].args.join(" "),
            "nextest run --no-run --profile rsid-fast -p rsid -p rsid-store --lib"
        );
        // Every gate build uses the shard script's test profile and keeps
        // the binaries the prebuild compiled.
        assert_eq!(spec.env["CARGO_PROFILE_TEST_DEBUG"], "0");
        assert_eq!(spec.env["RSI_LANDER_KEEP_RSID_ARTIFACTS"], "1");
        // A whole shard keeps its feature-selected harness; a plain name
        // filter on rsid-store runs on the shared build too.
        assert_eq!(
            rsid_family_filter_command("rsid", "shard:store-01").unwrap(),
            rsid_shard_guard_command("store-01").unwrap()
        );
        assert_eq!(
            rsid_family_filter_command("rsid-store", "store::tests").unwrap(),
            rsid_lib_filter_command("test(store::tests)")
        );
        assert!(rsid_family_filter_command("rsid", "shard:other-99:test(x)").is_err());
        // The isolated retry runs the one test on the same packages/features.
        let retry = isolated_retry_command(
            &rsid_lib_filter_command("test(rolling_queue)"),
            "rolling_queue::tests::a_case",
            ShardTree::Static,
        )
        .unwrap();
        assert_eq!(
            retry.args.join(" "),
            "test -p rsid -p rsid-store --lib rolling_queue::tests::a_case -- --exact --test-threads=4"
        );
    }

    /// #1261: every focused rsid filter of a gate is one nextest run over a
    /// combined filterset; identical atoms and atoms a shorter substring
    /// already selects are dropped, whatever shard each filter named.
    #[test]
    fn adaptive_test_threads_follow_idle_host_capacity_within_bounds() {
        // No slice reading: #2086's host rule.
        let host = |threads: Option<usize>, load1: Option<f64>| {
            adaptive_test_threads(&GateThreadInputs {
                host_threads: threads,
                load1,
                slice: None,
            })
        };
        // An idle big host is capped at sixteen.
        assert_eq!(host(Some(32), Some(0.0)), 16);
        assert_eq!(host(Some(64), Some(1.0)), 16);
        // The idle part of the host, halved.
        assert_eq!(host(Some(24), Some(4.0)), 10);
        assert_eq!(host(Some(16), Some(3.5)), 6);
        // A busy or small host bottoms out at four, never below.
        assert_eq!(host(Some(16), Some(15.0)), 4);
        assert_eq!(host(Some(8), Some(0.0)), 4);
        assert_eq!(host(Some(4), Some(40.0)), 4);
        // Unknown threads, no load average or a nonsense reading keep eight.
        assert_eq!(host(None, Some(1.0)), 8);
        assert_eq!(host(Some(32), None), 8);
        assert_eq!(host(Some(32), Some(f64::NAN)), 8);
        assert_eq!(host(Some(32), Some(-1.0)), 8);
    }

    #[test]
    fn adaptive_test_threads_follow_the_slices_own_headroom() {
        let slice = |quota_cpus: f64, used_cpus: f64, load1: f64| {
            adaptive_test_threads(&GateThreadInputs {
                host_threads: Some(8),
                load1: Some(load1),
                slice: Some(SliceHeadroom {
                    quota_cpus,
                    used_cpus,
                }),
            })
        };
        // #3225: an idle 8-CPU slice under a loaded host (load 9) takes all
        // eight, not the four the host comparison gave.
        assert_eq!(slice(8.0, 0.0, 9.0), 8);
        assert_eq!(slice(8.0, 0.4, 11.0), 7);
        // The thermal cap holds: never above the quota, whatever the idle host.
        assert_eq!(slice(8.0, 0.0, 0.0), 8);
        assert_eq!(slice(32.0, 0.0, 0.0), 16);
        // A busy slice bottoms out at four.
        assert_eq!(slice(8.0, 6.5, 0.0), 4);
        assert_eq!(slice(8.0, 12.0, 0.0), 4);
        // A slice below the minimum still gets the minimum.
        assert_eq!(slice(2.0, 0.0, 0.0), 4);
        // A nonsense slice reading falls back to the host rule (load 9 on 8: 4).
        assert_eq!(slice(8.0, f64::NAN, 9.0), 4);
        assert_eq!(slice(8.0, -1.0, 9.0), 4);
    }

    #[test]
    fn cgroup_readings_parse_and_ignore_unlimited_or_bad_files() {
        assert_eq!(parse_cpu_max_cpus("800000 100000\n"), Some(8.0));
        assert_eq!(parse_cpu_max_cpus("150000 100000"), Some(1.5));
        assert_eq!(parse_cpu_max_cpus("max 100000"), None);
        assert_eq!(parse_cpu_max_cpus("0 100000"), None);
        assert_eq!(parse_cpu_max_cpus("100000 0"), None);
        assert_eq!(parse_cpu_max_cpus(""), None);
        assert_eq!(
            parse_cpu_usage_usec("usage_usec 123456\nuser_usec 100\nsystem_usec 23\n"),
            Some(123_456)
        );
        assert_eq!(parse_cpu_usage_usec("user_usec 5\n"), None);
        assert_eq!(parse_cpu_usage_usec("usage_usec x\n"), None);
        assert_eq!(
            parse_unified_cgroup("0::/user.slice/rsi.slice/a.scope\n"),
            Some("/user.slice/rsi.slice/a.scope".to_owned())
        );
        assert_eq!(parse_unified_cgroup("1:cpu:/x\n"), None);
    }

    #[test]
    fn gate_thread_inputs_are_recorded_for_gate_timing() {
        assert_eq!(gate_thread_inputs_evidence(), "unrecorded");
        let inputs = GateThreadInputs {
            host_threads: Some(8),
            load1: Some(9.1),
            slice: Some(SliceHeadroom {
                quota_cpus: 8.0,
                used_cpus: 0.4,
            }),
        };
        store_gate_thread_inputs(&inputs);
        assert_eq!(
            gate_thread_inputs_evidence(),
            "quota:8.00,slice_use:0.40,host_load:9.10,host_threads:8,basis:slice"
        );
        store_gate_thread_inputs(&GateThreadInputs::default());
        assert_eq!(
            gate_thread_inputs_evidence(),
            "quota:unknown,slice_use:unknown,host_load:unknown,host_threads:unknown,basis:default"
        );
    }

    #[test]
    fn base_cache_key_ignores_the_test_thread_count_only() {
        let at = |jobs: &str, filterset: &str| {
            let mut command = rsid_lib_filter_command(filterset);
            let position = command.args.iter().position(|arg| arg == "-j").unwrap();
            command.args[position + 1] = jobs.into();
            command
        };
        assert_eq!(
            base_cache_command_key(&at("4", "all()")),
            base_cache_command_key(&at("16", "all()"))
        );
        assert_ne!(
            base_cache_command_key(&at("4", "all()")),
            base_cache_command_key(&at("4", "test(a)"))
        );
        // A command that is not a nextest run keeps its full identity.
        let plain = GuardCommand {
            program: "cargo".into(),
            args: vec!["check".into(), "-j".into(), "4".into()],
            timeout: Duration::from_secs(1),
        };
        assert_ne!(
            base_cache_command_key(&plain),
            base_cache_command_key(&GuardCommand {
                args: vec!["check".into(), "-j".into(), "8".into()],
                ..plain.clone()
            })
        );
    }

    #[test]
    fn focused_rsid_filters_combine_into_one_deduplicated_run() {
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsid".into(), "rsid-store".into()],
            &[
                "rsid=shard:session-05:test(session::manager_succession_tests)".into(),
                "rsid=shard:session-02:test(session::manager_)".into(),
                "rsid-store=shard:store-01:test(store::tests::h1_)".into(),
                "rsid=store::tests::h1_".into(),
                "rsid=rolling_queue::tests::queue_".into(),
                "rsid=shard:other-01:test(rolling_queue)".into(),
            ],
            true,
            &rsid_family_metadata(),
            4,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        let runs: Vec<&GuardCommand> = spec
            .commands
            .iter()
            .filter(|command| rsid_lib_filterset(command).is_some())
            .collect();
        assert_eq!(runs.len(), 1, "{:?}", rendered(&spec));
        assert_eq!(
            rsid_lib_filterset(runs[0]),
            Some(
                "test(store::tests::rewind_tears_down_the_non_idempotent_migration_tail) | \
                 test(store::tests::every_recovered_migration_step_actually_executes) | \
                 test(session::manager_) | test(store::tests::h1_) | test(rolling_queue)"
            )
        );
        // One run replaces several: its guard grows with the atoms, capped,
        // and it runs on the profile's eight test threads.
        assert_eq!(runs[0].timeout, guard_timeout() * 4);
        let at = runs[0].args.iter().position(|arg| arg == "-j").unwrap();
        assert_eq!(runs[0].args[at + 1], "8");
        let single = rsid_focused_run_command(&["test(a)".to_owned()]);
        assert_eq!(rsid_lib_filterset(&single), Some("test(a)"));
        assert_eq!(single.timeout, guard_timeout());
        // The shared build and the static inventory check still appear once.
        assert_eq!(
            shard_build_commands(&spec, ShardTree::Static),
            vec![rsid_lib_build_command()]
        );
        assert_eq!(
            spec.commands
                .iter()
                .filter(|command| **command == rsid_shard_inventory_check())
                .count(),
            1
        );
        // The explicit filters keep their labels for refusals and attribution.
        let explicit = explicit_focused_filters(
            &[
                "rsid=shard:session-05:test(session::manager_succession_tests)".into(),
                "rsid=shard:store-01".into(),
                "rsid=bin:rsid".into(),
                "demo=value".into(),
            ],
            false,
        );
        assert_eq!(
            explicit,
            vec![FocusedFilter {
                label: "rsid=shard:session-05:test(session::manager_succession_tests)".into(),
                filterset: "test(session::manager_succession_tests)".into(),
                explicit: true,
            }]
        );
    }

    #[test]
    fn focused_atoms_deduplicate_by_substring_and_module_cover() {
        let atoms = |sets: &[&str]| combined_focused_atoms(sets.iter().copied());
        assert_eq!(
            atoms(&["test(a::b)", "test(a::b)", "test(a::)"]),
            ["test(a::)"]
        );
        // A module atom is covered by a substring of `module::`, not the
        // other way round.
        assert_eq!(
            atoms(&["test(/^store::/)", "test(store::tests::x)"]),
            ["test(/^store::/)", "test(store::tests::x)"]
        );
        assert_eq!(
            atoms(&["test(/^store::/) | test(/^sandbox::/)", "test(store)"]),
            ["test(/^sandbox::/)", "test(store)"]
        );
        // An exact atom is covered by a substring of its name (#1280).
        assert_eq!(atoms(&["test(=exact)", "test(exact)"]), ["test(exact)"]);
        // A shape the lander does not generate is kept as is.
        assert_eq!(
            atoms(&["test(#exact*)", "test(exact)"]),
            ["test(#exact*)", "test(exact)"]
        );
        let module = FocusedMatch::parse("test(/^store::/)").unwrap();
        assert!(module.matches("store::tests::x"));
        assert!(!module.matches("storefront::x"));
        assert!(FocusedMatch::parse("test(x)").unwrap().matches("a::x_y"));
        assert_eq!(FocusedMatch::parse("not(test(x))"), None);
    }

    fn listing_report(stdout: &str) -> rsid::integration::GuardCommandReport {
        rsid::integration::GuardCommandReport {
            program: "cargo".into(),
            args: Vec::new(),
            status: rsid::integration::GuardStatus::Passed,
            stdout_tail: stdout.into(),
            stderr_tail: String::new(),
            output_truncated: false,
            duration: Duration::ZERO,
        }
    }

    /// #1957 at the gate: a stale family re-run runs only the atoms the
    /// advance observes, and an explicit filter it dropped keeps its earlier
    /// result instead of being refused for selecting nothing.
    #[tokio::test]
    async fn a_stale_family_regate_runs_only_observed_atoms() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let tree = fixture.root.path().join("stale-family-candidate");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                tree.to_str().unwrap(),
                &candidate,
            ],
        );
        let listing = fixture.root.path().join("listing.json");
        fs::write(
            &listing,
            nextest_listing(&[
                ("rsid", "alpha::t", "matches", false),
                ("rsid", "beta::t", "mismatch", false),
            ]),
        )
        .unwrap();
        let runs = fixture.root.path().join("focused-runs");
        fixture.add_fake_cargo(&format!(
            "case \"$1 $2\" in\n  'nextest list') cat '{listing}'; exit 0;;\n  'nextest run') for last; do :; done; printf '%s\\n' \"$last\" >> '{runs}'\n    echo '        PASS [   0.010s] (1/1) rsid alpha::t'; echo '     Summary [   0.010s] 1 test run: 1 passed, 0 skipped'; exit 0;;\nesac",
            listing = listing.display(),
            runs = runs.display(),
        ));
        let explicit = explicit_focused_filters(&["rsid=alpha".into(), "rsid=beta".into()], false);
        let atoms = combined_focused_atoms(explicit.iter().map(|filter| filter.filterset.as_str()));
        assert_eq!(atoms, ["test(alpha)", "test(beta)"]);
        let spec = GuardSpec {
            commands: vec![rsid_focused_run_command(&atoms)],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let mut reuse = reuse_of(scope(&["rsid:*"]), stale_delta::Scope::Nothing);
        reuse.family = Some(observers(&["alpha::t"], &[], &[]));
        let mut gate = TestGate {
            focused_filters: explicit,
            reuse: Some(reuse),
            ..TestGate::default()
        };
        let old_path = use_fake_path(&fixture);
        let result = run_affected_gate(
            &fixture.repo,
            &tree,
            &fixture.base,
            &spec,
            &mut gate,
            &BTreeSet::new(),
        )
        .await;
        restore_path(old_path);
        result.expect("the narrowed run passes and the dropped filter is not refused");
        let ran = fs::read_to_string(&runs).unwrap();
        assert!(
            !ran.is_empty() && ran.lines().all(|line| line == "test(alpha)"),
            "only the observed atom runs: {ran}"
        );
        assert_eq!(gate.reuse_counts, Some((0, 1)));
        assert_eq!(
            gate.focused_filters
                .iter()
                .map(|filter| filter.label.as_str())
                .collect::<Vec<_>>(),
            ["rsid=alpha"]
        );
    }

    /// #1957: the family observers of an advance come from the gated tree and
    /// the remade one, so a test the candidate added that calls the code the
    /// advance changed is observed, and a test that calls neither is not.
    #[test]
    fn family_observers_read_the_closure_over_the_trees_that_carry_the_candidate() {
        let fixture = Fixture::new();
        let lib = fixture.commit(
            &fixture.base,
            "crates/rsid/src/lib.rs",
            "mod a;\nmod b;\n",
            "lib",
        );
        let with_a = fixture.commit(
            &lib,
            "crates/rsid/src/a.rs",
            "pub fn helper() -> u32 {\n    1\n}\n\n#[test]\nfn a_direct() {\n    assert_eq!(helper(), 1);\n}\n",
            "a",
        );
        let fetched = fixture.commit(
            &with_a,
            "crates/rsid/src/b.rs",
            "pub fn unrelated() -> u32 {\n    2\n}\n\n#[test]\nfn b_alone() {\n    assert_eq!(unrelated(), 2);\n}\n",
            "b",
        );
        // The candidate adds a caller of `helper` and a test of it.
        let tested = fixture.commit(
            &fetched,
            "crates/rsid/src/b.rs",
            "pub fn unrelated() -> u32 {\n    2\n}\n\npub fn wraps() -> u32 {\n    crate::a::helper()\n}\n\n#[test]\nfn b_alone() {\n    assert_eq!(unrelated(), 2);\n}\n\n#[test]\nfn b_new_caller() {\n    assert_eq!(wraps(), 1);\n}\n",
            "candidate",
        );
        // The advance changes `helper`, remade on top of the candidate.
        let remade = fixture.commit(
            &tested,
            "crates/rsid/src/a.rs",
            "pub fn helper() -> u32 {\n    2 - 1\n}\n\n#[test]\nfn a_direct() {\n    assert_eq!(helper(), 1);\n}\n",
            "advance",
        );
        let observed = family_observers(
            &fixture.repo,
            &tested,
            &remade,
            &["rsid=bin:rsid".into(), "rsid-store=session::x".into()],
        )
        .expect("an rsid source change has a closure");
        assert!(observed.names.contains("a::a_direct"), "{observed:?}");
        assert!(observed.names.contains("b::b_new_caller"), "{observed:?}");
        assert!(!observed.names.contains("b::b_alone"), "{observed:?}");
        assert_eq!(observed.atoms, ["test(session::x)"]);
        assert!(observed.targets.contains("rsid:bin:rsid"), "{observed:?}");
        assert!(observed.observes_atom("test(=b::b_new_caller)"));
        assert!(!observed.observes_atom("test(=b::b_alone)"));
        // A whole shard has no atom to narrow by: the family runs whole.
        let whole = family_observers(
            &fixture.repo,
            &tested,
            &remade,
            &["rsid=shard:store-01".into()],
        );
        assert!(
            whole
                .as_ref()
                .is_err_and(|reason| reason.starts_with("whole_shard:")),
            "{whole:?}"
        );
        // A crate root change cannot be narrowed either.
        let root = fixture.commit(
            &remade,
            "crates/rsid/src/lib.rs",
            "mod a;\nmod b;\nmod c;\n",
            "root",
        );
        assert_eq!(
            family_observers(&fixture.repo, &remade, &root, &[]),
            Err("crate_wide_or_unreadable_diff".to_owned())
        );
    }

    fn nextest_listing(cases: &[(&str, &str, &str, bool)]) -> String {
        let mut suites = serde_json::Map::new();
        for (binary, name, status, ignored) in cases {
            let suite = suites
                .entry((*binary).to_owned())
                .or_insert_with(|| serde_json::json!({ "binary-id": binary, "testcases": {} }));
            suite["testcases"][*name] = serde_json::json!({
                "kind": "test",
                "ignored": ignored,
                "filter-match": { "status": status },
            });
        }
        serde_json::json!({ "test-count": cases.len(), "rust-suites": suites }).to_string()
    }

    /// #1262: a name that exists only as a file mounted elsewhere selects no
    /// test; the listing names that filter, and only a whole-run miss counts
    /// against a touched-module atom.
    #[test]
    fn a_listing_names_each_focused_filter_that_selects_nothing() {
        let listing = nextest_listing(&[
            (
                "rsid",
                "session::manager::succession_tests::hands_on",
                "matches",
                false,
            ),
            (
                "rsid",
                "rolling_queue::tests::queue_lands",
                "matches",
                false,
            ),
            (
                "rsid",
                "rolling_queue::tests::ignored_case",
                "matches",
                true,
            ),
            ("rsid-store", "store::tests::other", "mismatch", false),
        ]);
        let selected = listed_selected_tests(&listing_report(&listing)).unwrap();
        assert_eq!(
            selected,
            BTreeSet::from([
                "rolling_queue::tests::queue_lands".to_owned(),
                "session::manager::succession_tests::hands_on".to_owned(),
            ])
        );
        let explicit = explicit_focused_filters(
            &[
                "rsid=shard:session-05:test(session::manager_succession_tests)".into(),
                "rsid=shard:other-01:test(rolling_queue)".into(),
            ],
            false,
        );
        let filters = focused_filters_of_run(
            "test(session::manager_succession_tests) | test(rolling_queue) | test(/^sandbox::/)",
            &explicit,
        );
        assert_eq!(
            focused_filters_selecting_nothing(&filters, &selected),
            ["rsid=shard:session-05:test(session::manager_succession_tests)"]
        );
        assert_eq!(
            focused_filters_selecting(&filters, "rolling_queue::tests::queue_lands"),
            ["rsid=shard:other-01:test(rolling_queue)"]
        );
        // Nothing selected at all names every filter, module atoms included.
        assert_eq!(
            focused_filters_selecting_nothing(&filters, &BTreeSet::new()).len(),
            3
        );
        // No complete listing (a failed build, a shim) refuses nothing.
        assert_eq!(
            listed_selected_tests(&listing_report("Summary [ 0.01s]")),
            None
        );
        let mut failed = listing_report(&listing);
        failed.status = rsid::integration::GuardStatus::Failed { code: Some(101) };
        assert_eq!(listed_selected_tests(&failed), None);
        assert_eq!(
            rsid_lib_list_command("test(a) | test(b)").args.join(" "),
            "nextest list --profile rsid-fast -p rsid -p rsid-store --lib --message-format json --filterset test(a) | test(b)"
        );
    }

    /// #1261/#1262 at the gate: an empty focused filter is refused after the
    /// build and before any test runs; a red of the combined run is retried,
    /// compared with the base, and names the filters that select it.
    #[tokio::test]
    async fn combined_focused_run_refuses_empty_filters_and_attributes_failures() {
        for scenario in ["empty-filter", "new-red"] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"candidate\" }\n",
                "candidate",
            );
            let tree = fixture.root.path().join("combined-candidate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            let listing = fixture.root.path().join("listing.json");
            fs::write(
                &listing,
                nextest_listing(&[
                    (
                        "rsid",
                        "rolling_queue::tests::queue_lands",
                        "matches",
                        false,
                    ),
                    (
                        "rsid",
                        "session::manager::succession_tests::x",
                        "mismatch",
                        false,
                    ),
                ]),
            )
            .unwrap();
            let runs = fixture.root.path().join("combined-runs");
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\ncase \"$1 $2\" in\n  'nextest list') cat '{listing}'; exit 0;;\n  'nextest run') printf 'run %s\\n' \"$oid\" >> '{runs}'\n    if [ \"$oid\" = '{candidate}' ]; then echo '        FAIL [   0.010s] (1/1) rsid rolling_queue::tests::queue_lands'; echo '     Summary [   0.010s] 1 test run: 0 passed, 1 failed, 0 skipped'; exit 100; fi\n    echo '        PASS [   0.010s] (1/1) rsid rolling_queue::tests::queue_lands'; echo '     Summary [   0.010s] 1 test run: 1 passed, 0 skipped'; exit 0;;\n  test*) printf 'retry %s\\n' \"$oid\" >> '{runs}'; echo 'running 1 test'; echo 'test rolling_queue::tests::queue_lands ... FAILED'; exit 101;;\nesac",
                listing = listing.display(),
                runs = runs.display(),
            ));
            let filters: Vec<String> = if scenario == "empty-filter" {
                vec![
                    "rsid=shard:session-05:test(session::manager_succession_tests)".into(),
                    "rsid=shard:other-01:test(rolling_queue)".into(),
                ]
            } else {
                vec![
                    "rsid=shard:other-01:test(rolling_queue)".into(),
                    "rsid=rolling_queue::tests::queue_".into(),
                ]
            };
            let explicit = explicit_focused_filters(&filters, false);
            let atoms =
                combined_focused_atoms(explicit.iter().map(|filter| filter.filterset.as_str()));
            let spec = GuardSpec {
                commands: vec![rsid_focused_run_command(&atoms)],
                env: BTreeMap::new(),
                output_tail_bytes: 16 * 1024,
            };
            let mut gate = TestGate {
                focused_filters: explicit,
                ..TestGate::default()
            };
            let old_path = use_fake_path(&fixture);
            let result = run_affected_gate(
                &fixture.repo,
                &tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            restore_path(old_path);
            let error = result.unwrap_err();
            let ran = fs::read_to_string(&runs).unwrap_or_default();
            match scenario {
                "empty-filter" => {
                    assert!(
                        error.contains(
                            "refused after the build and before any test ran: \
                             rsid=shard:session-05:test(session::manager_succession_tests)"
                        ),
                        "{error}"
                    );
                    assert!(!is_test_red(&error), "{error}");
                    assert_eq!(ran, "", "no test ran");
                }
                "new-red" => {
                    assert!(error.contains(NEW_TEST_FAILURES), "{error}");
                    assert!(
                        error.contains(
                            "rolling_queue::tests::queue_lands [filter \
                             rsid=shard:other-01:test(rolling_queue) | \
                             rsid=rolling_queue::tests::queue_]"
                        ),
                        "{error}"
                    );
                    assert_eq!(
                        failing_test_names(&error),
                        ["rolling_queue::tests::queue_lands"]
                    );
                    // One candidate run for both filters, its base, the retry.
                    assert_eq!(
                        ran,
                        format!("run {candidate}\nrun {}\nretry {candidate}\n", fixture.base)
                    );
                }
                _ => unreachable!(),
            }
        }
    }

    /// A derived empty selection widens to its full shard. Caller-named
    /// empties retain the pre-test refusal, even beside a matching filter.
    #[tokio::test]
    async fn derived_empty_focused_filter_runs_its_whole_shard_and_explicit_empty_refuses() {
        for explicit in [false, true] {
            let fixture = Fixture::new();
            let tree = fixture.root.path().join("empty-focused-candidate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &fixture.base,
                ],
            );
            let listing = fixture.root.path().join("listing.json");
            fs::write(&listing, nextest_listing(&[])).unwrap();
            let runs = fixture.root.path().join("runs");
            fixture.add_fake_cargo(&format!(
                "case \"$1 $2\" in\n 'nextest list') cat '{}';;\n 'nextest run') echo unexpected-focused-run >> '{}'; exit 101;;\nesac",
                listing.display(), runs.display(),
            ));
            let script = tree.join("scripts/run-rsid-test-shards.sh");
            fs::create_dir_all(script.parent().unwrap()).unwrap();
            fs::write(&script, format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\necho '        PASS [   0.010s] (1/1) rsid fixture::test'\necho '     Summary [   0.010s] 1 test run: 1 passed, 0 skipped'\n", runs.display(),
            )).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            let mut filters =
                explicit_focused_filters(&["rsid=shard:other-02:test(fixture)".into()], false);
            filters[0].explicit = explicit;
            let mut gate = TestGate {
                focused_filters: filters,
                ..TestGate::default()
            };
            let spec = GuardSpec {
                commands: vec![rsid_lib_filter_command("test(fixture)")],
                env: BTreeMap::new(),
                output_tail_bytes: 16 * 1024,
            };
            let old_path = use_fake_path(&fixture);
            let result = run_affected_gate(
                &fixture.repo,
                &tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            restore_path(old_path);
            if explicit {
                assert!(result.unwrap_err().contains("test filter selects no test"));
                assert_eq!(fs::read_to_string(&runs).unwrap_or_default(), "");
            } else {
                result.expect("the gate runs the whole shard instead of the empty focused run");
                assert_eq!(
                    fs::read_to_string(&runs).unwrap(),
                    "shard other-02 --jobs 4\n"
                );
            }
        }
    }

    /// #1596: an `e2e_tui` filter would no-op without `RSI_E2E=1` and a built
    /// rsid, so the gate sets the flag and prebuilds rsid, only for that filter.
    #[test]
    fn e2e_filter_sets_flag_and_prebuilds_rsid() {
        let plan = |filters: &[String]| {
            affected_crate_guard_spec(
                "1111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222",
                &["rsi".into()],
                filters,
                false,
                &rsid_family_metadata(),
                4,
                None,
                &BTreeMap::new(),
            )
            .unwrap()
        };
        let spec = plan(&["rsi=test:e2e_tui:test(=settings_layout)".into()]);
        assert_eq!(spec.env.get("RSI_E2E").map(String::as_str), Some("1"));
        assert_eq!(
            rendered(&spec)[1..],
            [
                "cargo check -p rsi --all-targets",
                "cargo build -p rsid -p rsi-common -p rsi-turn-shim --bin rsid --bin rsi-agent-mcp --bin rsi-rpc --bin rsi-turn-shim",
                "cargo test -p rsi --test e2e_tui -- --test-threads=1 settings_layout --exact",
                "cargo check -p rsi",
            ]
        );
        let whole = plan(&["rsi=test:e2e_tui".into()]);
        assert_eq!(whole.env.get("RSI_E2E").map(String::as_str), Some("1"));
        let other = plan(&["rsi=test:flow".into()]);
        assert_eq!(other.env.get("RSI_E2E"), None);
        assert!(!rendered(&other).iter().any(|c| c.contains("build")));
    }

    #[test]
    fn public_item_observers_run_exact_library_tests_in_the_gate() {
        let command = cargo_guard_command("rsi", Some("test(=consumer::tests::shape)"));
        assert_eq!(
            command.args,
            [
                "test",
                "-p",
                "rsi",
                "--lib",
                "--",
                "--test-threads=4",
                "consumer::tests::shape",
                "--exact"
            ]
        );
        assert_eq!(cargo_lib_filters(&command), None);
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsi".into()],
            &[
                "rsi=test(=consumer::tests::shape)".into(),
                "rsi=other::tests".into(),
            ],
            false,
            &rsid_family_metadata(),
            4,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(spec.commands.contains(&command));
        assert!(
            spec.commands
                .contains(&cargo_guard_command("rsi", Some("other::tests")))
        );
    }

    /// #1282: the plain lib filters of another package run in one libtest
    /// invocation; bin and integration test filters keep their own runs.
    #[test]
    fn other_package_lib_filters_share_one_libtest_run() {
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsi".into()],
            &[
                "rsi=suggestions".into(),
                "rsi=settings_registry".into(),
                "rsi=suggestions".into(),
                "rsi=test:flow".into(),
            ],
            false,
            &rsid_family_metadata(),
            4,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rendered(&spec)[1..],
            [
                "cargo check -p rsi --all-targets",
                "cargo test -p rsi --test flow -- --test-threads=4",
                "cargo test -p rsi --lib -- --test-threads=4 suggestions settings_registry",
                "cargo check -p rsi",
            ]
        );
        let combined = &spec.commands[3];
        assert_eq!(
            combined.timeout,
            guard_timeout()
                .saturating_mul(2)
                .min(Duration::from_secs(MAX_GUARD_TIMEOUT_SECS).max(guard_timeout()))
        );
        assert_eq!(
            cargo_lib_filters(combined),
            Some(("rsi", vec!["suggestions", "settings_registry"]))
        );
        let single = cargo_lib_filters_command("rsi", &["suggestions"]);
        assert_eq!(single, cargo_guard_command("rsi", Some("suggestions")));
        assert_eq!(
            cargo_lib_filters(&single),
            Some(("rsi", vec!["suggestions"]))
        );
        assert_eq!(cargo_lib_filters(&cargo_guard_command("rsi", None)), None);
        assert_eq!(
            cargo_lib_filters(&cargo_guard_command("rsi", Some("test:flow"))),
            None
        );
        let labels: Vec<String> = cargo_lib_filter_labels(combined)
            .unwrap()
            .into_iter()
            .map(|filter| filter.label)
            .collect();
        assert_eq!(labels, ["rsi=suggestions", "rsi=settings_registry"]);
        let filters = cargo_lib_filter_labels(combined).unwrap();
        assert_eq!(
            focused_filters_selecting(&filters, "app::suggestions::tests::x"),
            ["rsi=suggestions"]
        );
        let retry =
            isolated_retry_command(combined, "app::suggestions::tests::x", ShardTree::Static)
                .unwrap();
        assert_eq!(
            retry.args,
            [
                "test",
                "-p",
                "rsi",
                "--lib",
                "app::suggestions::tests::x",
                "--",
                "--exact",
                "--test-threads=4"
            ]
        );
        let listed = libtest_listed_names(&listing_report(
            "app::suggestions::tests::x: test\nbench_y: benchmark\n\n1 test, 1 benchmark\n",
        ))
        .unwrap();
        assert_eq!(
            listed,
            BTreeSet::from(["app::suggestions::tests::x".to_owned()])
        );
        // Output without libtest's count line is no listing.
        assert_eq!(libtest_listed_names(&listing_report("")), None);
        assert_eq!(
            libtest_listed_names(&listing_report("0 tests, 0 benchmarks\n")),
            Some(BTreeSet::new())
        );
        assert_eq!(
            lib_filters_selecting_nothing(&["suggestions", "settings_registry"], &listed),
            ["settings_registry"]
        );
    }

    /// #1282: a combined lib run of another package refuses a filter that
    /// selects no test after the build and before any test runs, and a red
    /// names the filters that select the failing test.
    #[tokio::test]
    async fn combined_lib_run_refuses_empty_filters_and_attributes_failures() {
        for scenario in ["empty-filter", "new-red"] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"candidate\" }\n",
                "candidate",
            );
            let tree = fixture.root.path().join("lib-candidate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            let runs = fixture.root.path().join("lib-runs");
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\ncase \" $* \" in\n  *' --list '*) echo 'x::a_test: test'; echo; echo '1 test, 0 benchmarks'; exit 0;;\n  *' --exact '*) printf 'retry %s\\n' \"$oid\" >> '{runs}'; echo 'running 1 test'; echo 'test x::a_test ... FAILED'; exit 101;;\n  *) printf 'run %s\\n' \"$oid\" >> '{runs}'\n    if [ \"$oid\" = '{candidate}' ]; then echo 'running 1 test'; echo 'test x::a_test ... FAILED'; echo; echo 'failures:'; echo '    x::a_test'; echo; echo 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out'; exit 101; fi\n    echo 'running 1 test'; echo 'test x::a_test ... ok'; echo; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'; exit 0;;\nesac",
                runs = runs.display(),
            ));
            let filters: &[&str] = if scenario == "empty-filter" {
                &["a_test", "zzz"]
            } else {
                &["a_test", "x::"]
            };
            let spec = GuardSpec {
                commands: vec![cargo_lib_filters_command("demo", filters)],
                env: BTreeMap::new(),
                output_tail_bytes: 16 * 1024,
            };
            let mut gate = TestGate::default();
            let old_path = use_fake_path(&fixture);
            let result = run_affected_gate(
                &fixture.repo,
                &tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            restore_path(old_path);
            let error = result.unwrap_err();
            let ran = fs::read_to_string(&runs).unwrap_or_default();
            match scenario {
                "empty-filter" => {
                    assert!(
                        error.contains("refused after the build and before any test ran: demo=zzz")
                            && error.contains("`PACKAGE=test:NAME`"),
                        "{error}"
                    );
                    assert!(!is_test_red(&error), "{error}");
                    assert_eq!(ran, "", "no test ran");
                }
                "new-red" => {
                    assert!(error.contains(NEW_TEST_FAILURES), "{error}");
                    assert!(
                        error.contains("x::a_test [filter demo=a_test | demo=x::]"),
                        "{error}"
                    );
                    assert_eq!(failing_test_names(&error), ["x::a_test"]);
                    // One candidate run for both filters, its base, the retry.
                    assert_eq!(
                        ran,
                        format!("run {candidate}\nrun {}\nretry {candidate}\n", fixture.base)
                    );
                }
                _ => unreachable!(),
            }
        }
    }

    /// #1244/#1280: an unfiltered rsid/rsid-store change runs the library
    /// tests that can observe it on the shared build (every library test when
    /// the mapping is uncertain, never none for changed library code), the
    /// tests of a changed binary target, and the compile checks; a crate-wide
    /// change keeps every shard.
    #[test]
    fn unfiltered_rsid_change_gates_the_tests_that_observe_it() {
        let spec_for = |selection: Option<&test_impact::UnfilteredSelection>| {
            affected_crate_guard_spec(
                "1111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222",
                &["rsid-store".into(), "rsid".into()],
                &[],
                false,
                &rsid_family_metadata(),
                4,
                selection,
                &BTreeMap::new(),
            )
            .unwrap()
        };
        let narrowed = test_impact::UnfilteredSelection {
            reached_tests: 2,
            lib: test_impact::LibTests::Tests(BTreeSet::from([
                "store::tests::a".to_owned(),
                "topology::agent_tests::t4_a5_max_created_sessions_is_charged".to_owned(),
            ])),
            targets: BTreeSet::from([("rsid".to_owned(), "bin:rsi-rolling-land".to_owned())]),
            reached_targets: 1,
            leaf_tests: BTreeSet::new(),
        };
        assert_eq!(
            rendered(&spec_for(Some(&narrowed)))[1..],
            [
                "python3 scripts/check-rsid-test-shards.py --require-gates".to_owned(),
                "cargo check -p rsid-store --all-targets".to_owned(),
                "cargo nextest run --profile rsid-fast -p rsid -p rsid-store --lib --status-level all --final-status-level all -j 8 --filterset test(=store::tests::a) | test(=topology::agent_tests::t4_a5_max_created_sessions_is_charged)".to_owned(),
                "cargo check -p rsid --all-targets".to_owned(),
                "cargo test -p rsid --bin rsi-rolling-land -- --test-threads=4".to_owned(),
                "cargo check -p rsid-store".to_owned(),
                "cargo check -p rsid".to_owned(),
            ]
        );
        let everything = test_impact::UnfilteredSelection {
            reached_tests: 0,
            lib: test_impact::LibTests::Everything("uncertain".into()),
            targets: BTreeSet::new(),
            reached_targets: 0,
            leaf_tests: BTreeSet::new(),
        };
        assert_eq!(
            rendered(&spec_for(Some(&everything)))[1..],
            [
                "python3 scripts/check-rsid-test-shards.py --require-gates".to_owned(),
                "cargo check -p rsid-store --all-targets".to_owned(),
                "cargo nextest run --profile rsid-fast -p rsid -p rsid-store --lib --status-level all --final-status-level all -j 8 --filterset all()".to_owned(),
                "cargo check -p rsid --all-targets".to_owned(),
                "cargo check -p rsid-store".to_owned(),
                "cargo check -p rsid".to_owned(),
            ]
        );
        let whole = spec_for(Some(&everything));
        let whole_run = whole
            .commands
            .iter()
            .find(|command| rsid_lib_filterset(command) == Some("all()"))
            .unwrap();
        assert_eq!(
            whole_run.timeout,
            guard_timeout()
                .saturating_mul(u32::try_from(RSID_SHARDS.len()).unwrap())
                .min(Duration::from_secs(MAX_GUARD_TIMEOUT_SECS).max(guard_timeout()))
        );
        let no_library_change = test_impact::UnfilteredSelection {
            reached_tests: 0,
            lib: test_impact::LibTests::Tests(BTreeSet::new()),
            targets: BTreeSet::new(),
            reached_targets: 0,
            leaf_tests: BTreeSet::new(),
        };
        assert_eq!(
            rendered(&spec_for(Some(&no_library_change)))[1..],
            [
                "cargo check -p rsid-store --all-targets",
                "cargo check -p rsid --all-targets",
                "cargo check -p rsid-store",
                "cargo check -p rsid",
            ]
        );
        let crate_wide = spec_for(None);
        assert_eq!(
            crate_wide
                .commands
                .iter()
                .filter(|command| full_shard(command).is_some())
                .count(),
            RSID_SHARDS.len()
        );
    }

    /// #2631: a crate-wide gate (every whole shard) compiles the rsid test code
    /// once: the 16 feature-gated shard builds collapse into one `all()` run on
    /// the build without shard features, behind the static inventory check.
    #[test]
    fn a_crate_wide_gate_builds_the_rsid_tests_once() {
        let mut spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsid-store".into(), "rsid".into()],
            &[],
            false,
            &rsid_family_metadata(),
            4,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(gate_build_mode(&spec), "sharded");
        assert!(
            shard_build_commands(&spec, ShardTree::Static).len() >= RSID_SHARDS.len(),
            "today every shard is its own compile"
        );
        let before_len = spec.commands.len();
        assert!(collapse_whole_shards_to_one_build(
            &mut spec.commands,
            &BTreeSet::new()
        ));
        assert!(spec.commands.iter().all(|c| full_shard(c).is_none()));
        assert_eq!(gate_build_mode(&spec), "unsharded");
        let runs: Vec<&str> = spec
            .commands
            .iter()
            .filter_map(rsid_lib_filterset)
            .collect();
        assert_eq!(runs, ["all()"]);
        // One compile covers every shard's tests.
        assert_eq!(
            shard_build_commands(&spec, ShardTree::Static),
            vec![rsid_lib_build_command()]
        );
        // The inventory check the shard script ran stays, ahead of the build.
        let inventory = spec
            .commands
            .iter()
            .position(|c| *c == rsid_shard_inventory_check())
            .expect("inventory check");
        let run = spec
            .commands
            .iter()
            .position(|c| rsid_lib_filterset(c).is_some())
            .unwrap();
        assert!(inventory < run);
        // The run holds the guards of every shard it replaces.
        assert_eq!(
            spec.commands[run].timeout,
            guard_timeout()
                .saturating_mul(u32::try_from(RSID_SHARDS.len()).unwrap())
                .min(Duration::from_secs(MAX_GUARD_TIMEOUT_SECS).max(guard_timeout()))
        );
        // The diff check stays first; no other guard was dropped.
        assert_eq!(spec.commands[0].program, "git");
        assert_eq!(spec.commands.len(), before_len - RSID_SHARDS.len() + 2);
    }

    /// #2631: only a gate holding every whole shard collapses; a focused or
    /// closure gate keeps its own cheaper build, and a `--disk-scratch-shard`
    /// gate keeps the per-shard private scratch.
    #[test]
    fn only_a_gate_with_every_whole_shard_collapses_to_one_build() {
        let crate_wide = || {
            affected_crate_guard_spec(
                "1111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222",
                &["rsid".into()],
                &[],
                false,
                &rsid_family_metadata(),
                4,
                None,
                &BTreeMap::new(),
            )
            .unwrap()
        };
        let mut marked = crate_wide();
        let before = marked.commands.clone();
        assert!(!collapse_whole_shards_to_one_build(
            &mut marked.commands,
            &BTreeSet::from(["session-01".to_owned()])
        ));
        assert_eq!(marked.commands, before);
        assert_eq!(gate_build_mode(&marked), "sharded");

        let mut one_shard = crate_wide();
        one_shard
            .commands
            .retain(|c| full_shard(c).is_none_or(|(shard, _)| shard == "store-01"));
        let before = one_shard.commands.clone();
        assert!(!collapse_whole_shards_to_one_build(
            &mut one_shard.commands,
            &BTreeSet::new()
        ));
        assert_eq!(one_shard.commands, before);

        let focused = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsid".into()],
            &["rsid=shard:other-01:test(rolling_queue)".into()],
            false,
            &rsid_family_metadata(),
            4,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(gate_build_mode(&focused), "focused");
        assert_eq!(
            gate_build_mode(&GuardSpec {
                commands: vec![cargo_check_command("rsid")],
                env: BTreeMap::new(),
                output_tail_bytes: 1,
            }),
            "none"
        );
    }

    /// #2410: a changed leaf crate (`rsid-remote-read`) gates its whole library
    /// in its own package run and, with no explicit filter, the daemon observers
    /// on the shared rsid lib build. The leaf's tests stay out of that filterset
    /// (they are not in those packages), and a selection of only leaf tests adds
    /// no empty nextest run.
    #[test]
    fn a_changed_leaf_crate_gates_its_tests_and_the_daemon_observers() {
        let package = |name: &str, dependencies: &[&str]| WorkspacePackage {
            id: name.into(),
            name: name.into(),
            dependencies: dependencies
                .iter()
                .map(|dependency| WorkspaceDependency {
                    name: (*dependency).into(),
                    path: Some(PathBuf::from(format!("crates/{dependency}"))),
                })
                .collect(),
        };
        let metadata = WorkspaceMetadata {
            workspace_members: ["rsid-remote-read", "rsid-store", "rsid", "rsi"]
                .map(str::to_owned)
                .to_vec(),
            packages: vec![
                package("rsid-remote-read", &[]),
                package("rsid-store", &[]),
                package("rsid", &["rsid-store", "rsid-remote-read"]),
                package("rsi", &[]),
            ],
        };
        let leaf_test = "projection::tests::projects_one_pending_item";
        let owner_test = "remote_read::owner::tests::owner_reads_the_projection";
        let spec_for = |selection: &test_impact::UnfilteredSelection| {
            let crates = with_leaf_observers(&["rsid-remote-read".to_owned()], &[]);
            assert_eq!(crates, ["rsid", "rsid-remote-read"]);
            affected_crate_guard_spec(
                "1111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222",
                &crates,
                &[],
                false,
                &metadata,
                4,
                Some(selection),
                &BTreeMap::from([("rsid-remote-read".to_owned(), targets(true, &[]))]),
            )
            .unwrap()
        };
        let selection = test_impact::UnfilteredSelection {
            reached_tests: 2,
            lib: test_impact::LibTests::Tests(BTreeSet::from([
                leaf_test.to_owned(),
                owner_test.to_owned(),
            ])),
            targets: BTreeSet::new(),
            reached_targets: 0,
            leaf_tests: BTreeSet::from([leaf_test.to_owned()]),
        };
        let spec = spec_for(&selection);
        let commands = rendered(&spec);
        assert!(
            commands
                .contains(&"cargo test -p rsid-remote-read --lib -- --test-threads=4".to_owned()),
            "{commands:?}"
        );
        let filtersets: Vec<&str> = spec
            .commands
            .iter()
            .filter_map(rsid_lib_filterset)
            .collect();
        assert_eq!(filtersets, [format!("test(={owner_test})")], "{commands:?}");
        assert!(commands.contains(&"cargo check -p rsid --all-targets".to_owned()));
        assert!(commands.contains(&"cargo check -p rsid-remote-read".to_owned()));
        let leaf_only = test_impact::UnfilteredSelection {
            reached_tests: 1,
            lib: test_impact::LibTests::Tests(BTreeSet::from([leaf_test.to_owned()])),
            targets: BTreeSet::new(),
            reached_targets: 0,
            leaf_tests: BTreeSet::from([leaf_test.to_owned()]),
        };
        let spec = spec_for(&leaf_only);
        assert!(
            spec.commands
                .iter()
                .all(|c| rsid_lib_filterset(c).is_none())
        );
        assert!(
            rendered(&spec)
                .contains(&"cargo test -p rsid-remote-read --lib -- --test-threads=4".to_owned())
        );
    }

    #[test]
    fn a_leaf_change_adds_rsid_only_to_an_unfiltered_gate() {
        let owned = |names: &[&str]| {
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        let leaf = owned(&["rsid-remote-read"]);
        assert_eq!(
            with_leaf_observers(&leaf, &[]),
            owned(&["rsid", "rsid-remote-read"])
        );
        // An explicit filter overrides the closure, as it does for the rest of the family.
        let filtered = owned(&["rsid-remote-read=projection"]);
        assert_eq!(with_leaf_observers(&leaf, &filtered), leaf);
        let both = owned(&["rsid", "rsid-remote-read"]);
        assert_eq!(with_leaf_observers(&both, &[]), both);
        let other = owned(&["rsi-common"]);
        assert_eq!(with_leaf_observers(&other, &[]), other);
        // #2416: rsi-graph (its planning module) is observed by rsid the same way.
        let graph = owned(&["rsi-graph"]);
        assert_eq!(
            with_leaf_observers(&graph, &[]),
            owned(&["rsi-graph", "rsid"])
        );
        assert_eq!(
            with_leaf_observers(
                &owned(&["rsi-graph=planning"]),
                &owned(&["rsi-graph=planning"])
            ),
            owned(&["rsi-graph=planning"])
        );
    }

    #[test]
    fn base_compile_probes_preserve_targets_without_running_tests() {
        let target = base_compile_probe(
            &cargo_guard_command("demo", Some("test:small:test(=small::red)")),
            ShardTree::Static,
        )
        .unwrap();
        assert_eq!(target.program, "cargo");
        assert_eq!(
            target.args,
            ["test", "-p", "demo", "--test", "small", "--no-run"]
        );
        for command in [
            cargo_guard_command("demo", Some("green")),
            rsid_lib_filter_command("all()"),
            rsid_shard_guard_command("store-01").unwrap(),
        ] {
            let probe = base_compile_probe(&command, ShardTree::Static).unwrap();
            assert_eq!(probe.program, "cargo");
            assert!(probe.args.contains(&"--no-run".into()), "{probe:?}");
            assert!(probe.args.contains(&"--lib".into()), "{probe:?}");
            assert_eq!(probe.timeout, command.timeout);
        }
        let shard = base_compile_probe(
            &rsid_shard_guard_command("store-01").unwrap(),
            ShardTree::Static,
        )
        .unwrap();
        assert!(
            shard
                .args
                .windows(2)
                .any(|pair| pair == ["--features", "test-shard-store-01"])
        );
        assert_eq!(
            base_compile_probe(&cargo_check_command("demo"), ShardTree::Static),
            None
        );
    }

    /// #3839: a red or empty small target must not make a proven-green wide
    /// selection execute all its tests again on base. The late compile-repair
    /// check still probes that target's compilation without caching test proof.
    #[tokio::test]
    async fn a_red_or_empty_target_only_compiles_green_selections_on_base() {
        for scenario in ["shared-red", "empty"] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn candidate() {}\n",
                "candidate",
            );
            let tree = fixture.root.path().join("wide-candidate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            let runs = fixture.root.path().join("wide-runs");
            fixture.add_fake_cargo(&format!(
                r#"oid=$(/usr/bin/git rev-parse HEAD)
printf '%s %s\n' "$oid" "$*" >> '{runs}'
case " $* " in
  *' --no-run '*) exit 0;;
esac
case "$1 $2" in
  'nextest list') exit 0;;
  'nextest run')
    if [ "$oid" = '{base}' ]; then
      echo 'FAIL [ 0.01s] rsid unrelated::base_red'
      exit 100
    fi
    echo 'Summary [ 0.01s] 10000 tests run: 10000 passed, 0 skipped'
    exit 0;;
esac
if [ '{scenario}' = empty ]; then
  echo 'running 0 tests'
  echo 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'
  exit 0
fi
echo 'running 1 test'
echo 'test small::shared_red ... FAILED'
exit 101
"#,
                runs = runs.display(),
                base = fixture.base,
            ));
            let wide = rsid_lib_filter_command("all()");
            let small = cargo_guard_command("demo", Some("test:small"));
            let spec = GuardSpec {
                commands: vec![wide.clone(), small.clone()],
                env: BTreeMap::new(),
                output_tail_bytes: 16 * 1024,
            };
            let mut gate = TestGate::default();
            let old_path = use_fake_path(&fixture);
            let result = run_affected_gate(
                &fixture.repo,
                &tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            restore_path(old_path);
            result.expect("a shared red or empty target preserves a green wide run");
            let ran = fs::read_to_string(runs).unwrap();
            let candidate_at = ran
                .lines()
                .position(|line| line == format!("{} {}", candidate, wide.args.join(" ")))
                .unwrap();
            let base_at = ran
                .lines()
                .position(|line| line.starts_with(&fixture.base))
                .unwrap();
            assert!(
                candidate_at < base_at,
                "the wide source selection runs first: {ran}"
            );
            let base_runs: Vec<_> = ran
                .lines()
                .filter(|line| line.starts_with(&fixture.base))
                .collect();
            let probe = base_compile_probe(&wide, ShardTree::Static).unwrap();
            assert_eq!(
                base_runs,
                [
                    format!("{} {}", fixture.base, guard_command_line(&small)),
                    format!("{} {}", fixture.base, guard_command_line(&probe)),
                ]
                .map(|line| line.replacen("cargo ", "", 1)),
                "{ran}"
            );
            assert_eq!(gate.base_skipped, 1);
            assert_eq!(
                gate.base_runs, 1,
                "only the small target runs tests on base"
            );
            assert_eq!(
                gate.base_cache.keys().cloned().collect::<Vec<_>>(),
                vec![(fixture.base.clone(), base_cache_command_key(&small))]
            );
            assert_eq!(
                gate.base_reds,
                if scenario == "shared-red" {
                    BTreeSet::from(["small::shared_red".into()])
                } else {
                    BTreeSet::new()
                }
            );
        }
    }

    /// #1525: the base side of a red run names exactly the candidate's failing
    /// tests; shapes it cannot narrow keep the full command.
    #[test]
    fn base_command_is_narrowed_to_the_candidate_failures() {
        let failures: BTreeSet<String> = ["a::b::one", "a::b::two"].map(String::from).into();
        let narrowed = narrowed_base_command(&rsid_lib_filter_command("all()"), &failures)
            .expect("an all() lib run narrows");
        assert_eq!(
            rsid_lib_filterset(&narrowed),
            Some("test(=a::b::one) | test(=a::b::two)")
        );
        let mut expected = rsid_lib_filter_command("all()");
        *expected.args.last_mut().unwrap() = "test(=a::b::one) | test(=a::b::two)".into();
        assert_eq!(narrowed, expected);

        let shard = GuardCommand {
            program: "scripts/run-rsid-test-shards.sh".into(),
            args: ["shard", "store-01", "--jobs", "4"]
                .map(String::from)
                .into(),
            timeout: guard_timeout(),
        };
        let narrowed = narrowed_base_command(&shard, &failures).expect("a full shard narrows");
        assert_eq!(
            shard_command_parts(&narrowed),
            Some(("store-01", 4, Some("test(=a::b::one) | test(=a::b::two)")))
        );
        let filtered = GuardCommand {
            args: [
                "shard",
                "store-01",
                "--jobs",
                "4",
                "--filterset",
                "test(a::b)",
            ]
            .map(String::from)
            .into(),
            ..shard
        };
        let narrowed = narrowed_base_command(&filtered, &failures).expect("a filtered shard");
        assert_eq!(
            shard_command_parts(&narrowed).and_then(|parts| parts.2),
            Some("test(=a::b::one) | test(=a::b::two)")
        );

        // Unknown, unparseable or oversized failure lists run the full command.
        let lib = rsid_lib_filter_command("all()");
        assert_eq!(narrowed_base_command(&lib, &BTreeSet::new()), None);
        let odd: BTreeSet<String> = ["x) | all("].map(String::from).into();
        assert_eq!(narrowed_base_command(&lib, &odd), None);
        let many: BTreeSet<String> = (0..=MAX_NARROWED_BASE_TESTS)
            .map(|index| format!("m::t{index}"))
            .collect();
        assert_eq!(narrowed_base_command(&lib, &many), None);
        assert_eq!(
            narrowed_base_command(&cargo_guard_command("demo", None), &failures),
            None
        );
    }

    /// #1525 at the gate: a red `all()` candidate run runs the base side over
    /// only the failing test, never the whole command, and the shared red is
    /// still recognised as pre-existing.
    #[tokio::test]
    async fn red_candidate_runs_the_base_over_only_its_failing_tests() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let tree = fixture.root.path().join("narrow-candidate");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                tree.to_str().unwrap(),
                &candidate,
            ],
        );
        let runs = fixture.root.path().join("narrow-runs");
        fixture.add_fake_cargo(&format!(
            "oid=$(/usr/bin/git rev-parse HEAD)\nlast=\nfor arg in \"$@\"; do last=$arg; done\ncase \"$1\" in\n  nextest) printf '%s %s\\n' \"$oid\" \"$last\" >> '{runs}'\n    echo '        FAIL [   0.010s] (1/2) rsid demo::shared_red'\n    if [ \"$last\" = 'all()' ] && [ \"$oid\" != '{candidate}' ]; then echo '        FAIL [   0.010s] (2/2) rsid demo::other_base_red'; fi\n    echo '     Summary [   0.010s] 2 tests run: 0 passed, 1 failed, 0 skipped'; exit 100;;\n  test*) printf 'retry %s\\n' \"$oid\" >> '{runs}'; echo 'running 1 test'; echo 'test demo::shared_red ... FAILED'; exit 101;;\nesac",
            runs = runs.display(),
            candidate = candidate,
        ));
        let candidate_spec = GuardSpec {
            commands: vec![rsid_lib_filter_command("all()")],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let mut gate = TestGate::default();
        let old_path = use_fake_path(&fixture);
        let result = run_affected_gate(
            &fixture.repo,
            &tree,
            &fixture.base,
            &candidate_spec,
            &mut gate,
            &BTreeSet::new(),
        )
        .await;
        restore_path(old_path);
        let ran = fs::read_to_string(&runs).unwrap_or_default();
        result.expect("a red the base shares is not new");
        let base_runs: Vec<&str> = ran
            .lines()
            .filter(|line| line.starts_with(&fixture.base))
            .collect();
        assert_eq!(
            base_runs,
            [format!("{} test(=demo::shared_red)", fixture.base)],
            "{ran}"
        );
        assert!(gate.base_reds.contains("demo::shared_red"));
        assert!(!gate.base_reds.contains("demo::other_base_red"));
        assert_eq!(gate.base_runs, 1);
        assert!(gate.base_cache.is_empty(), "a narrowed run fills no cache");
    }

    /// #2059: the base outcome of a failing test is kept per (rolling tip,
    /// test): a second gate on the same tip reads the shared red from the cache
    /// (a `base_hits` entry, no base run), and a genuine new failure is still
    /// refused whether its green base outcome was measured or cached.
    #[tokio::test]
    async fn base_outcomes_are_cached_per_tip_and_test() {
        for (scenario, failing) in [("shared", "demo::shared_red"), ("new", "demo::new_red")] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"candidate\" }\n",
                "candidate",
            );
            let tree = fixture
                .root
                .path()
                .join(format!("cache-candidate-{scenario}"));
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            let runs = fixture.root.path().join(format!("cache-runs-{scenario}"));
            // The candidate fails `failing`; the base fails only the shared red.
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\nlast=\nfor arg in \"$@\"; do last=$arg; done\ncase \"$1\" in\n  nextest) printf '%s %s\\n' \"$oid\" \"$last\" >> '{runs}'\n    if [ \"$oid\" = '{candidate}' ] || [ \"{failing}\" = demo::shared_red ]; then echo '        FAIL [   0.010s] (1/2) rsid {failing}'; echo '     Summary [   0.010s] 2 tests run: 1 passed, 1 failed, 0 skipped'; exit 100; fi\n    echo '     Summary [   0.010s] 2 tests run: 2 passed, 0 skipped'; exit 0;;\n  test*) echo 'running 1 test'; echo 'test {failing} ... FAILED'; exit 101;;\nesac",
                runs = runs.display(),
                candidate = candidate,
                failing = failing,
            ));
            let spec = GuardSpec {
                commands: vec![rsid_lib_filter_command("all()")],
                env: BTreeMap::new(),
                output_tail_bytes: 16 * 1024,
            };
            let cache_root = fixture.root.path().join(format!("cache-root-{scenario}"));
            let old_path = use_fake_path(&fixture);
            let mut gates = Vec::new();
            for _ in 0..2 {
                let mut gate = TestGate {
                    cache_root: Some(cache_root.clone()),
                    ..TestGate::default()
                };
                let result = run_affected_gate(
                    &fixture.repo,
                    &tree,
                    &fixture.base,
                    &spec,
                    &mut gate,
                    &BTreeSet::new(),
                )
                .await;
                gates.push((result, gate));
            }
            restore_path(old_path);
            let ran = fs::read_to_string(&runs).unwrap_or_default();
            let base_runs: Vec<&str> = ran
                .lines()
                .filter(|line| line.starts_with(&fixture.base))
                .collect();
            assert_eq!(
                base_runs,
                [format!("{} test(={failing})", fixture.base)],
                "only the first gate runs the base, by exact name: {ran}"
            );
            assert_eq!(gates[0].1.base_runs, 1);
            assert!(gates[0].1.base_reused.is_empty());
            assert_eq!(gates[1].1.base_runs, 0, "the second gate hits the cache");
            assert_eq!(
                gates[1].1.base_reused,
                BTreeSet::from([format!("{}:{failing}:per-test", fixture.base)])
            );
            match scenario {
                "shared" => {
                    for (result, gate) in &gates {
                        result.as_ref().expect("a base red is not new");
                        assert!(gate.base_reds.contains(failing));
                    }
                }
                _ => {
                    for (result, _) in &gates {
                        let error = result.as_ref().expect_err("a new failure is refused");
                        assert!(error.contains(failing), "{error}");
                    }
                }
            }
        }
    }

    /// #2059: a cached outcome is keyed by the rolling tip and the command's
    /// environment class, holds both colours, and drops a name whose
    /// outcome flips (flaky evidence) so the next gate measures it again.
    #[test]
    fn per_test_outcome_file_is_keyed_and_drops_flips() {
        let root = tempfile::tempdir().unwrap();
        let tip = "a".repeat(40);
        let other_tip = "b".repeat(40);
        let class = sha256_digest_of("class");
        let names = |list: &[&str]| {
            list.iter()
                .map(|name| name.to_string())
                .collect::<BTreeSet<_>>()
        };
        base_cache::record_test_outcomes(
            root.path(),
            &tip,
            &class,
            &names(&["a::red"]),
            &names(&["a::green"]),
            &BTreeMap::from([("a::red".into(), "own red text".into())]),
        )
        .unwrap();
        let loaded = base_cache::load_test_outcomes(root.path(), &tip, &class);
        assert_eq!(
            loaded["a::red"].failure_text.as_deref(),
            Some("own red text")
        );
        let colours = |outcomes: BTreeMap<String, base_cache::TestOutcome>| {
            outcomes
                .into_iter()
                .map(|(name, outcome)| (name, outcome.red))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            colours(loaded),
            BTreeMap::from([
                ("a::red".to_string(), true),
                ("a::green".to_string(), false)
            ])
        );
        assert!(base_cache::load_test_outcomes(root.path(), &other_tip, &class).is_empty());
        assert!(
            base_cache::load_test_outcomes(root.path(), &tip, &sha256_digest_of("other"))
                .is_empty()
        );
        base_cache::record_test_outcomes(
            root.path(),
            &tip,
            &class,
            &names(&["a::green"]),
            &names(&[]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            colours(base_cache::load_test_outcomes(root.path(), &tip, &class)),
            BTreeMap::from([("a::red".to_string(), true)]),
            "a flipped outcome is forgotten"
        );
        let lib = rsid_lib_filter_command("all()");
        let other = rsid_lib_filter_command("test(x)");
        assert_eq!(
            per_test_outcome_class(&lib, "env"),
            per_test_outcome_class(&other, "env"),
            "every filterset of one command shares an outcome file"
        );
        assert_ne!(
            per_test_outcome_class(&lib, "env"),
            per_test_outcome_class(&lib, "other")
        );
    }

    /// #1280: the generated atoms deduplicate against explicit filters and
    /// `all()` absorbs every other atom.
    #[test]
    fn exact_and_all_atoms_deduplicate_and_attribute() {
        let atoms = |sets: &[&str]| combined_focused_atoms(sets.iter().copied());
        assert_eq!(
            atoms(&["test(=a::b::c) | test(=a::b::d)", "test(a::b::c)"]),
            ["test(=a::b::d)", "test(a::b::c)"]
        );
        assert_eq!(atoms(&["test(=a::b) | test(x)", "all()"]), ["all()"]);
        let filters = focused_filters_of_run("test(=a::b) | all()", &[]);
        assert!(filters.iter().all(|filter| !filter.explicit));
        assert_eq!(
            focused_filters_selecting(&filters, "a::b"),
            ["test(=a::b)", "all()"]
        );
        assert_eq!(focused_filters_selecting(&filters, "z"), ["all()"]);
    }

    /// #1244: the compile check runs beside the test prebuild, in its own
    /// target. Each side waits for the other to start, so a serial gate
    /// would time the check out.
    #[tokio::test]
    async fn compile_checks_run_beside_the_prebuild() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let tree = fixture.root.path().join("overlap-candidate");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                tree.to_str().unwrap(),
                &candidate,
            ],
        );
        let marks = fixture.root.path().join("marks");
        fs::create_dir_all(&marks).unwrap();
        let wait_for = |name: &str| {
            format!(
                "i=0; while [ ! -e '{m}/{name}' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done; [ -e '{m}/{name}' ]",
                m = marks.display()
            )
        };
        fixture.add_fake_cargo(&format!(
            "case \"$1 $*\" in\n  check*) printf '%s\\n' \"$CARGO_TARGET_DIR\" > '{m}/check-target'; : > '{m}/check'; {wait_build} || exit 9;;\n  *--no-run*) : > '{m}/build'; {wait_check};;\n  nextest*) echo 'Summary [ 0.01s] 1 test run: 1 passed, 0 skipped';;\nesac",
            m = marks.display(),
            wait_build = wait_for("build"),
            wait_check = wait_for("check"),
        ));
        let spec = GuardSpec {
            commands: vec![
                cargo_check_command("demo"),
                rsid_lib_filter_command("test(demo)"),
            ],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let mut gate = TestGate {
            prebuild_forced: true,
            ..TestGate::default()
        };
        let old_path = use_fake_path(&fixture);
        let result = run_affected_gate(
            &fixture.repo,
            &tree,
            &fixture.base,
            &spec,
            &mut gate,
            &BTreeSet::new(),
        )
        .await;
        restore_path(old_path);
        result.expect("check and prebuild overlap");
        assert!(
            fs::read_to_string(marks.join("check-target"))
                .unwrap()
                .trim()
                .ends_with("overlap-candidate-check-target")
        );
        assert_eq!(gate.base_skipped, 1);
    }

    /// A fake `rustc` so the warm slot's toolchain probe is hermetic.
    fn add_fake_rustc(fixture: &Fixture) {
        let rustc = fixture.bin.join("rustc");
        fs::write(&rustc, "#!/bin/sh\necho 'rustc 0.0.0-fake'\n").expect("fake rustc");
        let mut permissions = fs::metadata(&rustc).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(rustc, permissions).expect("chmod fake rustc");
    }

    fn add_candidate_tree(fixture: &Fixture, name: &str) -> PathBuf {
        let candidate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"candidate\" }\n",
            "candidate",
        );
        let tree = fixture.root.path().join(name);
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "--detach",
                tree.to_str().unwrap(),
                &candidate,
            ],
        );
        tree
    }

    /// #2467: a gate that leased a warm slot builds the candidate worktree into
    /// the slot's persistent test and check targets, any other worktree keeps
    /// its private pair, and the slot records the tree only after the build
    /// phase (compile checks and prebuild) succeeded.
    #[tokio::test]
    async fn a_leased_warm_slot_serves_the_candidate_worktree_and_commits_after_the_build() {
        let fixture = Fixture::new();
        add_fake_rustc(&fixture);
        let tree = add_candidate_tree(&fixture, "warm-candidate");
        let marks = fixture.root.path().join("warm-marks");
        fs::create_dir_all(&marks).unwrap();
        fixture.add_fake_cargo(&format!(
            "pwd >> '{m}/worktrees'\ncase \"$1 $*\" in\n  check*) printf '%s\\n' \"$CARGO_TARGET_DIR\" >> '{m}/check-targets';;\n  *--no-run*) printf '%s\\n' \"$CARGO_TARGET_DIR\" >> '{m}/build-targets';;\n  nextest*) echo 'Summary [ 0.01s] 1 test run: 1 passed, 0 skipped';;\nesac",
            m = marks.display(),
        ));
        let warm_root = fixture.root.path().join(rsid::lander_warm_target::WARM_DIR);
        let slot = rsid::lander_warm_target::WarmSlot::acquire(&warm_root, 1).expect("slot");
        let (test_target, check_target) = (slot.test_target(), slot.check_target());
        let manifest = warm_root.join("slot-0/manifest.json");
        let spec = GuardSpec {
            commands: vec![
                cargo_check_command("demo"),
                rsid_lib_filter_command("test(demo)"),
            ],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let mut gate = TestGate {
            prebuild_forced: true,
            warm: Some(slot),
            ..TestGate::default()
        };
        let old_path = use_fake_path(&fixture);
        let result = run_affected_gate(
            &fixture.repo,
            &tree,
            &fixture.base,
            &spec,
            &mut gate,
            &BTreeSet::new(),
        )
        .await;
        let lines = |name: &str| -> Vec<String> {
            fs::read_to_string(marks.join(name))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        };
        let gate_checks = lines("check-targets");
        let gate_builds = lines("build-targets");
        let build_tree = warm_root.join("slot-0/source");
        assert!(
            !lines("worktrees").is_empty()
                && lines("worktrees")
                    .iter()
                    .all(|path| Path::new(path) == build_tree.as_path()),
            "all warm gate commands use the fixed checkout"
        );
        // A worktree that never bound a slot builds into its sibling targets.
        let other = fixture.root.path().join("unbound-tree");
        fs::create_dir_all(&other).unwrap();
        let other_run = run_one_guard(&other, &spec, &cargo_check_command("demo")).await;
        restore_path(old_path);
        result.expect("the warm gate passes");
        other_run.expect("unbound check runs");
        let check_target = check_target.to_string_lossy().into_owned();
        let test_target = test_target.to_string_lossy().into_owned();
        assert!(
            !gate_checks.is_empty() && gate_checks.iter().all(|target| *target == check_target),
            "{gate_checks:?}"
        );
        assert!(
            !gate_builds.is_empty() && gate_builds.iter().all(|target| *target == test_target),
            "{gate_builds:?}"
        );
        assert_eq!(
            lines("check-targets")[gate_checks.len()..],
            [fixture
                .root
                .path()
                .join("unbound-tree-check-target")
                .to_string_lossy()
                .into_owned()]
        );
        assert!(manifest.is_file(), "the successful build phase is recorded");
        warm_bindings().remove(&build_tree);
    }

    /// #2467: a gate whose compile check fails leaves the slot without a
    /// manifest (the previous gate's record was taken at bind), so the next gate
    /// rebuilds every workspace crate instead of trusting a half-built target.
    #[tokio::test]
    async fn a_failed_build_phase_leaves_no_manifest_in_the_warm_slot() {
        let fixture = Fixture::new();
        add_fake_rustc(&fixture);
        let tree = add_candidate_tree(&fixture, "failing-warm-candidate");
        fixture.add_fake_cargo(
            "case \"$1 $*\" in\n  check*) echo 'error[E0432]: unresolved import' >&2; exit 101;;\n  *--no-run*) :;;\n  nextest*) echo 'Summary [ 0.01s] 1 test run: 1 passed, 0 skipped';;\nesac",
        );
        let warm_root = fixture.root.path().join(rsid::lander_warm_target::WARM_DIR);
        let mut slot = rsid::lander_warm_target::WarmSlot::acquire(&warm_root, 1).expect("slot");
        let old_path = use_fake_path(&fixture);
        // The previous gate built this tree and recorded it.
        slot.bind(&tree).expect("first bind");
        slot.commit().expect("first commit");
        let manifest = warm_root.join("slot-0/manifest.json");
        assert!(manifest.is_file());
        let spec = GuardSpec {
            commands: vec![
                cargo_check_command("demo"),
                rsid_lib_filter_command("test(demo)"),
            ],
            env: BTreeMap::new(),
            output_tail_bytes: 16 * 1024,
        };
        let mut gate = TestGate {
            prebuild_forced: true,
            warm: Some(slot),
            ..TestGate::default()
        };
        let result = run_affected_gate(
            &fixture.repo,
            &tree,
            &fixture.base,
            &spec,
            &mut gate,
            &BTreeSet::new(),
        )
        .await;
        restore_path(old_path);
        assert!(result.is_err(), "the failing compile check fails the gate");
        assert!(
            !manifest.exists(),
            "a gate that did not finish its build phase records nothing"
        );
        warm_bindings().remove(&warm_root.join("slot-0/source"));
    }

    /// #1244 acceptance: the candidate runs first; a green, proven candidate
    /// never builds or runs the base, a red shared with the base still
    /// passes, and a red the base does not have still fails.
    #[tokio::test]
    async fn lazy_base_keeps_no_new_failures_semantics() {
        for scenario in ["green", "red-on-both", "new-red"] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"candidate\" }\n",
                "candidate",
            );
            let tree = fixture.root.path().join("lazy-candidate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            let log = fixture.root.path().join("lazy-runs");
            let candidate_red = scenario != "green";
            let base_red = scenario == "red-on-both";
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\necho 'running 1 test'\nif [ \"$oid\" = '{candidate}' ] && [ '{candidate_red}' = true ]; then echo 'test demo::red ... FAILED'; exit 101; fi\nif [ \"$oid\" = '{base}' ] && [ '{base_red}' = true ]; then echo 'test demo::red ... FAILED'; exit 101; fi\necho 'test demo::red ... ok'",
                log = log.display(),
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
                &tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            restore_path(old_path);
            let runs = fs::read_to_string(&log).unwrap();
            let count = |oid: &str| runs.lines().filter(|line| *line == oid).count();
            match scenario {
                "green" => {
                    result.expect("a green candidate passes");
                    assert_eq!(count(&fixture.base), 0, "{runs}");
                    assert_eq!(gate.base_runs, 0);
                    assert_eq!(gate.base_skipped, 1);
                    assert!(gate.base_worktrees.is_empty());
                }
                "red-on-both" => {
                    result.expect("a red the base shares is not new");
                    assert_eq!(count(&fixture.base), 1, "{runs}");
                    assert!(gate.base_reds.contains("demo::red"));
                    assert_eq!(gate.base_skipped, 0);
                }
                "new-red" => {
                    let error = result.unwrap_err();
                    assert!(error.contains(NEW_TEST_FAILURES), "{error}");
                    assert!(error.contains("demo::red"), "{error}");
                    assert_eq!(count(&fixture.base), 1, "{runs}");
                    // The run and its isolated retry.
                    assert_eq!(count(&candidate), 2, "{runs}");
                }
                _ => unreachable!(),
            }
        }
    }

    /// #2167: a claimed fix must run the named test green on the candidate. A
    /// red the base shares no longer excuses it (refused before the base runs),
    /// a test the selection never ran is not proven, and a gate without claims
    /// keeps "no new failures relative to rolling".
    #[tokio::test]
    async fn a_claimed_fix_must_run_the_named_test_green_on_the_candidate() {
        for scenario in ["still-red", "unclaimed", "green", "not-run"] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"candidate\" }\n",
                "candidate",
            );
            let tree = fixture.root.path().join("claimed-candidate");
            git_run(
                &fixture.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    tree.to_str().unwrap(),
                    &candidate,
                ],
            );
            let log = fixture.root.path().join("claimed-runs");
            fixture.add_fake_cargo(&format!(
                "oid=$(/usr/bin/git rev-parse HEAD)\nprintf '%s\\n' \"$oid\" >> '{log}'\necho 'running 1 test'\nif [ \"$oid\" = '{base}' ]; then echo 'test demo::red ... FAILED'; exit 101; fi\ncase '{scenario}' in\n  still-red|unclaimed) echo 'test demo::red ... FAILED'; exit 101;;\n  not-run) echo 'test demo::other ... ok'; exit 0;;\nesac\necho 'test demo::red ... ok'",
                log = log.display(),
                base = fixture.base,
            ));
            let spec = GuardSpec {
                commands: vec![cargo_guard_command("demo", None)],
                env: BTreeMap::new(),
                output_tail_bytes: 16 * 1024,
            };
            let mut gate = TestGate::default();
            if scenario != "unclaimed" {
                gate.claimed_fixes = BTreeSet::from(["demo::red".to_string()]);
            }
            let old_path = use_fake_path(&fixture);
            let result = run_affected_gate(
                &fixture.repo,
                &tree,
                &fixture.base,
                &spec,
                &mut gate,
                &BTreeSet::new(),
            )
            .await;
            restore_path(old_path);
            let runs = fs::read_to_string(&log).unwrap();
            let base_runs = runs.lines().filter(|line| *line == fixture.base).count();
            match scenario {
                "still-red" => {
                    let error = result.unwrap_err();
                    assert!(
                        error.starts_with("claimed_fix_still_red: demo::red; "),
                        "{error}"
                    );
                    assert_eq!(claimed_fix_names(&error), vec!["demo::red".to_string()]);
                    assert_eq!(base_runs, 0, "a base red excuses no claim: {runs}");
                }
                "unclaimed" => {
                    result.expect("without a claim a red the base shares is not new");
                    assert!(gate.base_reds.contains("demo::red"));
                    assert_eq!(base_runs, 1, "{runs}");
                }
                "green" => {
                    result.expect("a claimed fix that runs green passes");
                    assert!(gate.claimed_green.contains("demo::red"));
                    assert_eq!(base_runs, 0, "{runs}");
                }
                "not-run" => {
                    let error = result.unwrap_err();
                    assert!(
                        error.starts_with("claimed_fix_still_red: demo::red; ")
                            && error.contains("did not select it"),
                        "{error}"
                    );
                }
                _ => unreachable!(),
            }
        }
    }

    /// #2167 acceptance through the whole lander: base and candidate red on the
    /// claimed test refuses `claimed_fix_still_red` (named on a `failing_test=`
    /// line) and leaves rolling where it was; a candidate that turns it green
    /// publishes.
    #[tokio::test]
    async fn a_claimed_fix_publishes_only_when_the_named_test_turns_green() {
        for green in [false, true] {
            let fixture = Fixture::new();
            let body = if green { "fixed" } else { "still broken" };
            let source = fixture.commit(
                &fixture.base,
                "crates/demo/src/lib.rs",
                &format!("pub fn value() -> &'static str {{ \"{body}\" }}\n"),
                "claimed fix",
            );
            fixture.add_fake_cargo(&format!(
                "if [ \"$1\" = test ]; then\n  echo 'running 1 test'\n  oid=$(/usr/bin/git rev-parse HEAD)\n  if [ \"$oid\" = '{base}' ] || [ '{green}' = false ]; then echo 'test demo::red ... FAILED'; exit 101; fi\n  echo 'test demo::red ... ok'\nfi",
                base = fixture.base,
            ));
            let old_path = use_fake_path(&fixture);
            let result = fixture
                .land_with_fixes(
                    vec![AcceptedPair {
                        base: fixture.base.clone(),
                        source: source.clone(),
                    }],
                    vec!["demo::red".into()],
                )
                .await;
            restore_path(old_path);
            let rolling = git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]);
            if green {
                let report = result.expect("a claimed fix that turns the test green publishes");
                assert_eq!(report.published_tip, source);
                assert_eq!(rolling, source);
            } else {
                let failure = result.expect_err("a claimed fix that stays red is refused");
                assert_eq!(failure.state, PublicationState::NotPublished);
                assert!(
                    failure.message.contains("claimed_fix_still_red: demo::red"),
                    "{failure:?}"
                );
                assert!(
                    failure
                        .evidence_lines()
                        .contains(&"failing_test=demo::red".to_string()),
                    "{:?}",
                    failure.evidence_lines()
                );
                assert_eq!(rolling, fixture.base);
            }
        }
    }

    #[test]
    fn passing_test_names_follow_libtest_and_nextest_identity() {
        let output = "test a::ok_one ... ok\ntest a::bad ... FAILED\ntest a::panics - should panic ... ok\n        PASS [   0.010s] (1/2) rsid rolling_queue::tests::green\n        FAIL [   0.010s] (2/2) rsid rolling_queue::tests::red\n";
        assert_eq!(
            test_pass_names(output),
            BTreeSet::from([
                "a::ok_one".to_string(),
                "a::panics".into(),
                "rolling_queue::tests::green".into(),
            ])
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
        let root = Path::new(rsi_common::manifest_dir!()).join("../..");
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
            None,
            &BTreeMap::new(),
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
            None,
            &BTreeMap::new(),
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
                // #1970: the non-test builds of the changed crate and of the
                // shipped binaries' crate.
                "check -p rsi-common",
                "check -p rsid",
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
            None,
            &BTreeMap::new(),
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
        let protected = "crates/rsid-store/src/store/manager_ledger/facts.rs";
        let manifest = format!(
            "{{\"migration_file\":\"crates/rsid-store/src/store/mod.rs\",\"protected_sections\":{{\"facts\":{{\"path\":\"{protected}\"}}}}}}\n"
        );
        let base = fixture.commit(
            &fixture.base,
            "tools/released-migrations.json",
            &manifest,
            "pin facts",
        );
        let source = fixture.commit(&base, protected, "edited\n", "change pinned facts");
        let _released_guard = landing_policy::stub_released_guard(2);
        let error = landing_policy::check_with_proof(
            &fixture.repo,
            fixture.root.path(),
            &base,
            &source,
            &[],
        )
        .unwrap_err();
        assert_eq!(error.fence, landing_policy::PolicyFence::ReleasedMigration);
        assert!(error.message.contains("released-migration guard refused"));
    }

    #[test]
    fn lowered_schema_version_has_stable_policy_fence() {
        let fixture = Fixture::new();
        let store = "crates/rsid-store/src/store/mod.rs";
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
        let _released_guard = landing_policy::stub_released_guard(0);
        let error = landing_policy::check_with_proof(
            &fixture.repo,
            fixture.root.path(),
            &base,
            &source,
            &[],
        )
        .unwrap_err();
        assert_eq!(error.fence, landing_policy::PolicyFence::SchemaVersion);
    }

    #[tokio::test]
    async fn hot_file_lands_unbound_without_ledger_access() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "AGENTS.md", "policy\n", "hot file");
        fixture.add_fake_cargo("");
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let report = result.expect("a hot-file source lands with the fixture ledger unavailable");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
    }

    #[tokio::test]
    async fn policy_refusal_runs_before_any_test_gate() {
        let fixture = Fixture::new();
        let store = "crates/rsid-store/src/store/mod.rs";
        // Released DDL lives elsewhere in this fixture, so the store change
        // reaches the migration-number rule rather than the released guard.
        let manifest = fixture.commit(
            &fixture.base,
            "tools/released-migrations.json",
            "{\"migration_file\":\"crates/rsid-store/src/store/released.rs\",\"protected_sections\":{}}\n",
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
        let knobs = with_lander_vars([("GIT_DIR", fixture.root.path().join("not-a-git-dir"))]);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base[..12].to_string(),
                source: source[..12].to_string(),
            }])
            .await;
        drop(knobs);
        restore_path(old_path);
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
            fixes_tests: Vec::new(),
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
        let knobs = with_lander_vars([
            ("RSI_LANDER_CANARY_MODE", std::ffi::OsStr::new("handoff")),
            ("RSI_ROLLING_CANARY_DIR", registry_root.as_os_str()),
            ("RSI_LANDER_CANARY_SPAWN", std::ffi::OsStr::new("0")),
            // The sweep exports CARGO_BUILD_JOBS=16, above the lander ceiling (#4012).
            ("CARGO_BUILD_JOBS", std::ffi::OsStr::new("2")),
        ]);
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
        drop(knobs);
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
        let knobs = with_lander_vars([
            ("RSI_LANDER_CANARY_MODE", std::ffi::OsStr::new("handoff")),
            ("RSI_ROLLING_CANARY_DIR", registry_root.as_os_str()),
            ("RSI_LANDER_CANARY_SPAWN", std::ffi::OsStr::new("0")),
            // The sweep exports CARGO_BUILD_JOBS=16, above the lander ceiling (#4012).
            ("CARGO_BUILD_JOBS", std::ffi::OsStr::new("2")),
        ]);
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
        drop(knobs);
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
        // #1244: the canary's candidate runs first (1), then its base (2),
        // then the isolated retry (3).
        fixture.add_fake_cargo(&format!(
            "count=$(cat '{}' 2>/dev/null || echo 0)\ncount=$((count + 1))\necho \"$count\" > '{}'\ncase $count in\n  1) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43; echo 'test demo::red ... FAILED'; exit 42;;\n  3) echo 'test demo::red ... FAILED'; exit 42;;\n  4) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43;;\n  6) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43;;\nesac",
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
        // The filters name text the source holds (#1243 refuses absent names).
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n// accepted::focus other::focus\n",
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
        // A filtered package lists its named tests once (#1282), runs its
        // compile check (candidate only) and one libtest run of every named
        // filter on both sides; the unfiltered suite is the QA sweep's.
        let expected_pass = concat!(
            "metadata\n--no-deps\n--format-version\n1\n--offline\n",
            "test\n-p\ndemo\n--lib\n--\n--list\naccepted::focus\nother::focus\n",
            "check\n-p\ndemo\n--all-targets\n",
            "test\n-p\ndemo\n--lib\n--\n--test-threads=4\naccepted::focus\nother::focus\n",
            "test\n-p\ndemo\n--lib\n--\n--test-threads=4\naccepted::focus\nother::focus\n",
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
        // One listing, one compile check and two focused test runs.
        for _ in 0..4 {
            let guard_target = lines.next().expect("isolated guard target");
            assert_ne!(guard_target, configured_target);
            assert_eq!(lines.next(), Some("1"));
            assert_eq!(lines.next(), Some("line-tables-only"));
            let guard_worktree = lines.next().expect("guard worktree directory");
            let worktree = Path::new(guard_worktree);
            let name = worktree.file_name().unwrap().to_string_lossy();
            // The compile check has its own target (#1244).
            let expected_targets = [
                worktree.with_file_name(format!("{name}-cargo-target")),
                worktree.with_file_name(format!("{name}-check-target")),
            ];
            assert!(
                expected_targets
                    .iter()
                    .any(|expected| Path::new(guard_target) == expected),
                "{guard_target}"
            );
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

    #[test]
    fn tmpfs_room_is_capped_by_the_user_block_quota() {
        const GIB: u128 = 1024 * 1024 * 1024;
        let quota = |hard_limit, used| Some(BlockQuota { hard_limit, used });
        // No quota, or a zero hard limit: the filesystem's free space.
        assert_eq!(cap_by_quota(29 * GIB, None), 29 * GIB);
        assert_eq!(cap_by_quota(29 * GIB, quota(0, 5 * GIB)), 29 * GIB);
        // #938: systemd's 80% per-user tmpfs quota leaves less than statvfs.
        assert_eq!(cap_by_quota(29 * GIB, quota(24 * GIB, 14 * GIB)), 10 * GIB);
        // Over quota leaves nothing; a roomy quota never raises free space.
        assert_eq!(cap_by_quota(29 * GIB, quota(24 * GIB, 25 * GIB)), 0);
        assert_eq!(cap_by_quota(3 * GIB, quota(24 * GIB, GIB)), 3 * GIB);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quota_limited_tmpfs_uses_sandbox_target_scratch() {
        const GIB: u128 = 1024 * 1024 * 1024;
        let tmpfs = Path::new("/dev/shm");
        // Only a host whose /dev/shm keeps a user quota well below its free
        // space (systemd's default) can show the difference.
        let Some(quota) = user_block_quota(tmpfs) else {
            return;
        };
        let Ok(stats) = nix::sys::statvfs::statvfs(tmpfs) else {
            return;
        };
        let free_gib =
            u128::from(stats.blocks_available()) * u128::from(stats.fragment_size()) / GIB;
        let room_gib = cap_by_quota(u128::MAX, Some(quota)) / GIB;
        if quota.hard_limit == 0 || free_gib < room_gib + 4 {
            return;
        }
        // Ask for more than the quota leaves but less than statvfs reports:
        // free space alone would pick tmpfs, the quota must not.
        let root = tempfile::tempdir().expect("scratch fixture");
        let target = root.path().join("target");
        fs::create_dir(&target).expect("target directory");
        let wanted = u128::midpoint(room_gib, free_gib);
        let wanted_gb = u64::try_from(wanted).expect("GiB fit in u64");
        let scratch = select_gate_scratch(&target, wanted_gb).expect("gate scratch");
        if matches!(scratch, GateScratch::Tmpfs(_)) {
            // Other users of /dev/shm may have freed gigabytes meanwhile; a
            // tmpfs pick is right only if the quota room really grew.
            let room_now = user_writable_bytes(tmpfs).unwrap_or(0) / GIB;
            assert!(
                room_now + 1 >= wanted,
                "tmpfs picked with quota room {room_now} GiB (was {room_gib}) < wanted {wanted} GiB, free {free_gib} GiB: {}",
                scratch.report()
            );
        }
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
        let parent = PathBuf::from(rsi_common::manifest_dir!()).join("../../target");
        fs::create_dir_all(&parent).expect("workspace target directory");
        let parent = parent.canonicalize().expect("canonical workspace target");
        let fixture = Fixture::new_in(&parent);
        let target = fixture.repo.join("target");
        fs::create_dir(&target).expect("sandbox-local Cargo target");
        // #192: the lander's CARGO_TARGET_DIR, for this thread only.
        let _target_env = with_lander_vars([("CARGO_TARGET_DIR", &target)]);
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"interrupted\" }\n",
            "interrupted source",
        );
        let marker = fixture.root.path().join("cargo-started");
        let grandchild_pid = fixture.root.path().join("cargo-grandchild.pid");
        fixture.add_fake_cargo(&format!(
            "touch '{}'; sleep 30 & echo $! > '{}'; wait",
            marker.display(),
            grandchild_pid.display()
        ));
        let old_path = use_fake_path(&fixture);
        rsid_core::process_control::track_bounded_process_groups();
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
                    fixes_tests: Vec::new(),
                },
                async {
                    while !marker.exists() || !grandchild_pid.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    terminate_tracked_child_groups();
                },
            ),
        )
        .await;
        restore_path(old_path);
        let result = result.expect("landing reached its interruptible guard");
        assert_eq!(result.unwrap_err().state, PublicationState::Unknown);
        #[cfg(target_os = "linux")]
        let grandchild = fs::read_to_string(&grandchild_pid)
            .expect("fake cargo recorded its child")
            .trim()
            .parse::<i32>()
            .expect("child pid is numeric");
        #[cfg(target_os = "linux")]
        {
            let stat = fs::read_to_string(format!("/proc/{grandchild}/stat"));
            assert!(
                stat.is_err()
                    || stat.as_ref().is_ok_and(|stat| stat
                        .rsplit_once(')')
                        .and_then(|(_, fields)| fields.split_whitespace().next())
                        .is_some_and(|state| state == "Z")),
                "the lander's cargo grandchild remains running: {stat:?}"
            );
        }
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
        // The remote read/push subprocesses must ignore this inherited repo override.
        let knobs = with_lander_vars([("GIT_DIR", fixture.root.path().join("not-a-git-dir"))]);
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
        drop(knobs);
        restore_path(old_path);
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
    async fn transient_fetch_and_pre_push_lookup_recover_without_regating() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let cargo_log = fixture.root.path().join("cargo.log");
        fixture.add_fake_cargo(&format!(
            "echo \"$1\" >> '{}'\nif [ \"$1\" = test ]; then echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'; fi",
            cargo_log.display()
        ));
        let fetch_log = fixture.root.path().join("fetch.log");
        let lookup_log = fixture.root.path().join("lookup.log");
        let fail_twice = |log: &Path| {
            format!(
                "echo attempt >> '{0}'\nif [ $(wc -l < '{0}') -le 2 ]; then exit 128; fi\nexec /usr/bin/git \"$@\"",
                log.display()
            )
        };
        fixture.add_fake_git_behaviors_with_fetch(
            &fail_twice(&lookup_log),
            "",
            &fail_twice(&fetch_log),
        );
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let report = result.expect("third read attempts recover");
        assert_eq!(report.published_tip, source);
        assert!(report.stale_retries.is_empty());
        assert_eq!(fs::read_to_string(fetch_log).unwrap().lines().count(), 3);
        assert!(fs::read_to_string(lookup_log).unwrap().lines().count() >= 4);
        assert_eq!(
            fs::read_to_string(cargo_log)
                .unwrap()
                .lines()
                .filter(|line| *line == "test")
                .count(),
            1,
            "network recovery must reuse the green candidate gate"
        );
        assert_eq!(git_value(&fixture.bare, &["rev-parse", TARGET]), source);
    }

    #[test]
    fn a_failed_remote_read_is_reported_as_infrastructure_and_a_red_gate_is_not() {
        let class_line = format!(
            "failure_class={}",
            rsi_common::rolling_queue::LANDER_FAILURE_CLASS_REMOTE_READ
        );
        for message in [
            "cannot read remote rolling tip: git ls-remote exited 128",
            "remote rolling fetch failed (exit 128)",
            "remote rolling lookup timed out after 30s",
        ] {
            let failure = LandFailure::from(message.to_string());
            assert!(failure.evidence_lines().contains(&class_line), "{message}");
        }
        let red = LandFailure::from(format!(
            "{NEW_FAILURES_PREFIX}abc: demo::red; cannot read remote rolling tip"
        ));
        assert!(!red.evidence_lines().contains(&class_line));
        let plain = LandFailure::from("demo::red failed".to_string());
        assert!(!plain.evidence_lines().contains(&class_line));
        let mut published = LandFailure::from("cannot read remote rolling tip".to_string());
        published.state = PublicationState::Unknown;
        assert!(!published.evidence_lines().contains(&class_line));
    }

    #[tokio::test]
    async fn failed_remote_reads_stop_after_three_attempts_without_pushing() {
        let fixture = Fixture::new();
        let log = fixture.root.path().join("reads.log");
        let fail = format!("echo attempt >> '{}'\nexit 128", log.display());
        fixture.add_fake_git_behaviors_with_fetch(&fail, "exit 99", &fail);
        let old_path = use_fake_path(&fixture);
        let fetch_result = remote_fetch(&fixture.repo).await;
        let publish_result = publish_oid(&fixture.repo, &fixture.base, &fixture.base).await;
        restore_path(old_path);
        assert!(
            fetch_result
                .unwrap_err()
                .contains("fetch failed (exit 128)")
        );
        let failure = publish_result.unwrap_err();
        assert_eq!(failure.state, PublicationState::NotPublished);
        assert!(failure.message.contains("ls-remote exited 128"));
        assert_eq!(fs::read_to_string(log).unwrap().lines().count(), 6);
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", TARGET]),
            fixture.base
        );
    }

    #[tokio::test]
    async fn uncertain_push_retries_verification_but_never_repeats_push() {
        for (published, lookup_recovers) in [(true, true), (false, true), (true, false)] {
            let fixture = Fixture::new();
            let source = fixture.commit(&fixture.base, "source.txt", "accepted\n", "source");
            git_run(
                &fixture.repo,
                &["remote", "add", "publish", fixture.bare.to_str().unwrap()],
            );
            let lookup_log = fixture.root.path().join("lookup.log");
            let push_log = fixture.root.path().join("push.log");
            fixture.add_fake_git_behaviors(
                &format!(
                    "echo attempt >> '{0}'\nn=$(wc -l < '{0}')\nif [ \"$n\" -gt 1 ] && {1}; then exit 128; fi\nexec /usr/bin/git \"$@\"",
                    lookup_log.display(),
                    if lookup_recovers { "[ \"$n\" -le 3 ]" } else { "true" }
                ),
                &format!(
                    "echo attempt >> '{}'\n{}\nexit 128",
                    push_log.display(),
                    if published { "/usr/bin/git \"$@\" || exit 99" } else { ":" }
                ),
            );
            let old_path = use_fake_path(&fixture);
            let result = publish_oid(&fixture.repo, &source, &fixture.base).await;
            restore_path(old_path);
            match (published, lookup_recovers) {
                (true, true) => assert_eq!(result.unwrap(), source),
                (false, true) => {
                    assert_eq!(result.unwrap_err().state, PublicationState::NotPublished);
                }
                _ => assert_eq!(result.unwrap_err().state, PublicationState::Unknown),
            }
            assert_eq!(fs::read_to_string(push_log).unwrap().lines().count(), 1);
            assert_eq!(fs::read_to_string(lookup_log).unwrap().lines().count(), 4);
            assert_eq!(
                git_value(&fixture.bare, &["rev-parse", TARGET]),
                if published { source } else { fixture.base }
            );
        }
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
        assert!(elapsed < Duration::from_secs(10), "lookup took {elapsed:?}");
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
        assert!(started.elapsed() < Duration::from_secs(10));
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
    async fn already_integrated_source_without_historical_base_succeeds() {
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
        let report = fixture
            .land(vec![AcceptedPair {
                base: String::new(),
                source: source.clone(),
            }])
            .await
            .expect("already-integrated source is a successful no-op");
        assert_eq!(report.outcome, LandingOutcome::AlreadyOnRolling);
        assert_eq!(report.published_tip, source);
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
        );
    }

    #[tokio::test]
    async fn empty_guard_output_for_empty_source_is_already_on_rolling() {
        let fixture = Fixture::new();
        let tip = fixture.commit(
            &fixture.base,
            "scripts/rolling-landing-guard.py",
            "import sys\nprint('landing guard: accepted source has no changes from its base', file=sys.stderr)\nsys.exit(2)\n",
            "legacy guard that emits no JSON for an empty delta",
        );
        // The lander takes its guard from the source repository's HEAD.
        git_run(&fixture.repo, &["merge", "--ff-only", &tip]);
        git_run(
            &fixture.repo,
            &["push", "-q", "origin", &format!("{tip}:refs/heads/rolling")],
        );
        let report = fixture
            .land(vec![AcceptedPair {
                base: tip.clone(),
                source: tip.clone(),
            }])
            .await
            .expect("empty guard stdout for a proven no-op must succeed");
        assert_eq!(report.outcome, LandingOutcome::AlreadyOnRolling);
        assert_eq!(report.published_tip, tip);
    }

    #[tokio::test]
    async fn identical_patch_on_rolling_returns_the_containing_tip_without_a_push() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "same.txt", "accepted\n", "source change");
        let tip = fixture.commit(
            &fixture.base,
            "same.txt",
            "accepted\n",
            "same change landed elsewhere",
        );
        assert_ne!(source, tip);
        git_run(
            &fixture.repo,
            &["push", "-q", "origin", &format!("{tip}:refs/heads/rolling")],
        );
        let report = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await
            .expect("an identical merged tree is already on rolling");
        assert_eq!(report.outcome, LandingOutcome::AlreadyOnRolling);
        assert_eq!(report.published_tip, tip);
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            tip
        );
        assert_source_clean(&fixture.repo);
    }

    #[tokio::test]
    async fn empty_guard_error_leads_with_the_decisive_reason() {
        let fixture = Fixture::new();
        let guard = fixture.commit(
            &fixture.base,
            "scripts/rolling-landing-guard.py",
            "import sys\nprint('decisive_guard_reason: unavailable', file=sys.stderr)\nsys.exit(2)\n",
            "empty-output guard fixture",
        );
        git_run(&fixture.repo, &["merge", "--ff-only", &guard]);
        let source = fixture.commit(&guard, "source.txt", "change\n", "nonempty source");
        let error = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await
            .expect_err("an empty guard report does not admit a nonempty source");
        assert!(
            error
                .message
                .starts_with("decisive_guard_reason: unavailable"),
            "{error:?}"
        );
        assert!(
            error
                .message
                .chars()
                .take(256)
                .collect::<String>()
                .contains("landing guard returned no plan")
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
            base_red_evidence: BaseRedEvidence::default(),
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
                base_red_evidence: BaseRedEvidence::default(),
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
    fn accepted_commit_resolves_an_abbreviated_sha_and_refuses_a_miss_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let other = dir.path().join("other");
        for path in [&repo, &other] {
            std::fs::create_dir(path).expect("create repo dir");
            git_ok(path, &["init", "--quiet"]).expect("init");
            git_ok(
                path,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t.invalid",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "c",
                ],
            )
            .expect("commit");
        }
        let full = git_text(&repo, &["rev-parse", "HEAD"]).expect("head");
        let short = &full[..10];
        assert_eq!(
            resolve_accepted_commit(&other, &repo, "SOURCE", short).expect("short sha"),
            full,
            "an abbreviation known only to the caller's repository resolves there"
        );
        assert_eq!(
            resolve_accepted_commit(&repo, &repo, "SOURCE", &full).expect("full sha"),
            full
        );
        let error = resolve_accepted_commit(&repo, &repo, "SOURCE", "0123456789abcdef")
            .expect_err("unknown id");
        assert!(
            error.contains("--accepted SOURCE `0123456789abcdef`"),
            "{error}"
        );
        assert!(error.contains("full 40-hex commit id"), "{error}");
        assert!(!error.contains("Needed a single revision"), "{error}");
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
    fn terminate_tracked_child_groups_returns_early_when_empty() {
        // Verify that terminate_tracked_child_groups returns without the 1 second
        // sleep when no child groups are tracked. This test uses a timeout to
        // ensure the function does not sleep unconditionally.
        rsid_core::process_control::track_bounded_process_groups();
        let start = std::time::Instant::now();
        terminate_tracked_child_groups();
        let elapsed = start.elapsed();
        // The function should return much faster than 1 second (allow 100ms margin
        // for test overhead)
        assert!(
            elapsed < Duration::from_millis(100),
            "terminate_tracked_child_groups with empty groups took {elapsed:?}, should be < 100ms"
        );
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
