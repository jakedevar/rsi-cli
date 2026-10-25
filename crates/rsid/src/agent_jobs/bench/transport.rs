//! The hub side of the Rust TB2 runner (#3681 R3b, design #3628 "Architecture and
//! transport"): the stamped `rsi-eval` binary the job unit runs, the [`RunnerPlanV1`] it
//! reads, the verdict of its preflight and the outcomes it leaves for the record.
//!
//! The runner (`rsi-eval bench tb2 preflight|run --plan`, crates/rsi-eval) owns every Docker and
//! ssh step; the daemon owns every decision. The plan is the only input: the daemon writes it from
//! the admitted run, so the runner never reads a policy from the repository. No Python, Harbor, uv
//! or venv is involved, and nothing here compiles or looks anything up on `PATH`.

use super::config::ResolvedRun;
use rsi_common::agent_jobs::{
    BENCH_CONFIG_INVALID, BENCH_PIN_CHECK_FAILED, BENCH_PREFLIGHT_FAILED, BENCH_TRANSPORT_FAILED,
    BenchShardResultV1,
};
use rsi_common::bench_runner::{
    PlannedTrial, RUNNER_VERSION_DISCRIMINATOR, RemotePlanV1, RunnerMode, RunnerPlanV1,
    RunnerSourceManifest, TrialException, TrialExceptionKind, TrialIdentity, TrialOutcomeV1,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The runner executable's file stem, installed beside `rsid` (`make release-install`).
pub(crate) const RUNNER_EXECUTABLE: &str = "rsi-eval";

/// Exit codes of `rsi-eval bench tb2 preflight|run` (its documented contract, the family
/// `scripts/bench-transport` used): 20 preflight, 21 pin check, 22 trial operation, 23 ssh.
pub(crate) const EXIT_PREFLIGHT: i32 = 20;
pub(crate) const EXIT_PIN_CHECK: i32 = 21;
pub(crate) const EXIT_SHARD: i32 = 22;
pub(crate) const EXIT_SSH: i32 = 23;
/// `run` finished every trial but one did not pass or was not cleaned up: the outcomes are the
/// result, so the job still settles and the record carries the verdicts.
pub(crate) const EXIT_NOT_ALL_PASSED: i32 = 2;

/// A typed refusal of a bench gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BenchRefusal {
    pub code: &'static str,
    pub detail: String,
}

impl BenchRefusal {
    pub(crate) fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// `<code>: <detail>`, the text a submit refusal and a settled job carry.
    pub(crate) fn text(&self) -> String {
        format!("{}: {}", self.code, self.detail)
    }
}

/// What `rsi-eval bench tb2 stamp` prints.
#[derive(Debug, Clone, Deserialize)]
struct StampJson {
    runner_version: String,
    source_digest: String,
    evaluator_digest: String,
    source_manifest: String,
    #[serde(default)]
    missing: Vec<String>,
}

/// The runner binary admission verified: where it is and the evaluator identity it was built
/// with. The record's `adapter_tree_hash` is [`RunnerBinary::evaluator_digest`] and its
/// `sampling.runner` is derived from [`RunnerBinary::manifest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunnerBinary {
    pub path: PathBuf,
    pub manifest: RunnerSourceManifest,
    pub evaluator_digest: String,
}

impl RunnerBinary {
    /// Judge a binary by its printed stamp. The binary is trusted only when the stamp names this
    /// runner version, lists no missing source, parses, and its digests are the ones the shared
    /// functions derive from the manifest it printed (a stamp that disagrees with itself is not
    /// admissible).
    ///
    /// # Errors
    /// Why the stamp is unusable.
    pub(crate) fn from_stamp(path: PathBuf, stdout: &str) -> Result<Self, String> {
        let stamp: StampJson = serde_json::from_str(stdout.trim())
            .map_err(|e| format!("the runner's stamp is unreadable: {e}"))?;
        if stamp.runner_version != RUNNER_VERSION_DISCRIMINATOR {
            return Err(format!(
                "the runner is version {} but this daemon runs {RUNNER_VERSION_DISCRIMINATOR}",
                stamp.runner_version
            ));
        }
        if !stamp.missing.is_empty() {
            return Err(format!(
                "the runner was built without source(s) {}",
                stamp.missing.join(", ")
            ));
        }
        let manifest = RunnerSourceManifest::parse(&stamp.source_manifest)
            .map_err(|e| format!("the runner's source manifest is invalid: {e}"))?;
        if !manifest.missing().is_empty() {
            return Err("the runner's source manifest names a missing source".to_string());
        }
        if manifest.source_digest() != stamp.source_digest
            || manifest.evaluator_digest() != stamp.evaluator_digest
        {
            return Err("the runner's stamp disagrees with its own source manifest".to_string());
        }
        Ok(Self {
            path,
            evaluator_digest: stamp.evaluator_digest,
            manifest,
        })
    }
}

/// The runner installed beside `exe` (the running `rsid`): never a copy found on `PATH`, never a
/// build at trial time.
///
/// # Errors
/// There is no such file.
pub(crate) fn sibling_runner(exe: &Path) -> Result<PathBuf, String> {
    let real = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let dir = real
        .parent()
        .ok_or_else(|| "the daemon executable has no directory".to_string())?;
    let candidate = dir.join(format!(
        "{RUNNER_EXECUTABLE}{}",
        std::env::consts::EXE_SUFFIX
    ));
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!(
            "{} is not installed beside the daemon; `make release-install` builds and installs it",
            candidate.display()
        ))
    }
}

/// `rsi-eval bench tb2 <verb> --plan <plan>`: the argv the job unit and the preflight run.
pub(crate) fn runner_argv(runner: &Path, verb: &str, plan: &Path) -> Vec<String> {
    vec![
        runner.display().to_string(),
        "bench".to_string(),
        "tb2".to_string(),
        verb.to_string(),
        "--plan".to_string(),
        plan.display().to_string(),
    ]
}

/// `rsi-eval bench tb2 stamp`.
pub(crate) fn stamp_argv(runner: &Path) -> Vec<String> {
    vec![
        runner.display().to_string(),
        "bench".to_string(),
        "tb2".to_string(),
        "stamp".to_string(),
    ]
}

/// Where the runner and the checkout it grades against live.
pub(crate) struct PlanPaths<'a> {
    /// The pinned Terminal-Bench checkout on the hub (`<bench root>/tb2/<commit>`).
    pub checkout_dir: &'a Path,
    /// The pinned `dataset.toml` the plan is checked against.
    pub dataset_path: &'a Path,
    /// The runner writes `<output_root>/<run id>/trials/<key>/outcome.json` here.
    pub output_root: &'a Path,
}

/// The operator limits and host the plan carries.
pub(crate) struct PlanHosts<'a> {
    pub laptop_host: &'a str,
    pub docker_hosts: &'a [String],
    /// The job's effective concurrency, already `min(config, bench.max_concurrency)`.
    pub concurrency: u32,
    pub max_concurrency: u32,
}

fn whole_seconds(label: &str, task: &str, value: f64) -> Result<u64, BenchRefusal> {
    if !value.is_finite() || value <= 0.0 || value.fract() != 0.0 || value >= 1.8e19 {
        return Err(BenchRefusal::new(
            BENCH_CONFIG_INVALID,
            format!(
                "dataset.toml: {task} {label} {value} is not a positive whole number of seconds"
            ),
        ));
    }
    // The checks above make the cast exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(value as u64)
}

/// The plan of a run: every selected task times `k` slots, each with its own pinned image and
/// deadlines. An oracle config plans [`RunnerMode::Oracle`]; a model-driven one plans
/// [`RunnerMode::Harness`] and names the run-local socket and the stop marker (R6). The runner refuses
/// a Harness plan until its trial driver lands, so a paid run is turned away at preflight, before
/// any run is opened or any money is reserved.
///
/// # Errors
/// A task missing from the pinned dataset or with a fractional or non-positive deadline.
pub(crate) fn runner_plan(
    run_id: Uuid,
    resolved: &ResolvedRun,
    hosts: &PlanHosts<'_>,
    paths: &PlanPaths<'_>,
) -> Result<RunnerPlanV1, BenchRefusal> {
    let run = run_id.to_string();
    let mut trials = Vec::new();
    for name in &resolved.tasks {
        let task = resolved.task(name).ok_or_else(|| {
            BenchRefusal::new(
                BENCH_CONFIG_INVALID,
                format!("{name} is not in the pinned dataset"),
            )
        })?;
        let agent_timeout_sec = whole_seconds("agent_timeout_sec", name, task.agent_timeout_sec)?;
        let verifier_timeout_sec =
            whole_seconds("verifier_timeout_sec", name, task.verifier_timeout_sec)?;
        for slot in 0..resolved.file.k {
            trials.push(PlannedTrial {
                identity: TrialIdentity {
                    run_id: run.clone(),
                    task: name.clone(),
                    slot,
                    retry: 0,
                },
                image: task.image.clone(),
                image_digest: task.image_digest.clone(),
                cpus: task.cpus,
                memory_mb: task.memory_mb,
                agent_timeout_sec,
                verifier_timeout_sec,
                solution_env: std::collections::BTreeMap::new(),
                verifier_env: std::collections::BTreeMap::new(),
                agent_user: None,
                verifier_user: None,
            });
        }
    }
    let mode = if resolved.is_paid() {
        RunnerMode::Harness
    } else {
        RunnerMode::Oracle
    };
    // R6: a Harness plan drives the run-local socket the daemon binds for this run (the oracle
    // holds none); both paths are the run's own, under the bench root the daemon owns.
    let run_dir = paths.output_root.join(&run);
    let socket_path = (mode == RunnerMode::Harness).then(|| {
        run_dir
            .join(crate::bench::SOCKET_NAME)
            .display()
            .to_string()
    });
    let stop_file = Some(run_dir.join(super::STOP_FILE).display().to_string());
    let mut plan = RunnerPlanV1::new(run, mode, trials);
    plan.remote = Some(RemotePlanV1 {
        docker_host: resolved.file.host.clone(),
        laptop_host: hosts.laptop_host.to_string(),
        allowed_docker_hosts: hosts.docker_hosts.to_vec(),
        concurrency: hosts.concurrency,
        max_concurrency: hosts.max_concurrency,
        checkout_dir: paths.checkout_dir.display().to_string(),
        dataset_path: paths.dataset_path.display().to_string(),
        output_root: paths.output_root.display().to_string(),
        socket_path,
        stop_file,
    });
    Ok(plan)
}

/// How a runner process ended when it did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunnerFailure {
    /// The exit status; `None` when the process could not run or was killed.
    pub exit: Option<i32>,
    /// The runner's stderr (or the reason it could not run).
    pub detail: String,
}

/// The `kind: detail` of the runner's last `tb2_runner error [<kind>]: <detail>` line, else its
/// last stderr line.
fn error_line(stderr: &str) -> String {
    const MARK: &str = "tb2_runner error [";
    if let Some(line) = stderr
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix(MARK))
        && let Some((kind, detail)) = line.split_once("]: ")
    {
        return format!("{kind}: {detail}");
    }
    stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map_or_else(|| "no output".to_string(), |line| line.trim().to_string())
}

impl RunnerFailure {
    /// A process that ended with `exit` and wrote `stderr`.
    pub(crate) fn exited(exit: Option<i32>, stderr: &str) -> Self {
        Self {
            exit,
            detail: error_line(stderr),
        }
    }
}

/// The preflight verdict (design: "Rust preflight"): the runner's own checks (hosts, pin, Docker,
/// platform, measured disk and memory) either pass or name a typed reason. A pin drift is its own
/// code; every other failure, including an unrunnable binary, is a preflight failure.
///
/// # Errors
/// The typed refusal.
pub(crate) fn evaluate_preflight(result: Result<(), RunnerFailure>) -> Result<(), BenchRefusal> {
    match result {
        Ok(()) => Ok(()),
        Err(failure) if failure.exit == Some(EXIT_PIN_CHECK) => {
            Err(BenchRefusal::new(BENCH_PIN_CHECK_FAILED, failure.detail))
        }
        Err(failure) => Err(BenchRefusal::new(BENCH_PREFLIGHT_FAILED, failure.detail)),
    }
}

/// The refusal a runner exit code settles a job with.
pub(crate) fn exit_refusal(code: i32) -> &'static str {
    match code {
        EXIT_PREFLIGHT => BENCH_PREFLIGHT_FAILED,
        EXIT_PIN_CHECK => BENCH_PIN_CHECK_FAILED,
        // A failed trial operation, a dropped ssh and anything else (a signal, a crash).
        EXIT_SHARD | EXIT_SSH => BENCH_TRANSPORT_FAILED,
        _ => BENCH_TRANSPORT_FAILED,
    }
}

/// Every `<run dir>/trials/*/outcome.json` the runner wrote, and a note for each file that was
/// unreadable or belongs to another run. Sorted by directory name so the result is stable.
pub(crate) fn read_outcomes(run_dir: &Path, run_id: &str) -> (Vec<TrialOutcomeV1>, Vec<String>) {
    let mut outcomes = Vec::new();
    let mut notes = Vec::new();
    let Ok(entries) = std::fs::read_dir(run_dir.join("trials")) else {
        return (outcomes, notes);
    };
    let mut dirs: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    dirs.sort();
    for dir in dirs {
        let file = dir.join("outcome.json");
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                notes.push(format!("{}: {error}", file.display()));
                continue;
            }
        };
        match serde_json::from_slice::<TrialOutcomeV1>(&bytes) {
            Ok(outcome) if outcome.identity.run_id == run_id => outcomes.push(outcome),
            Ok(outcome) => notes.push(format!(
                "{}: belongs to run {}",
                file.display(),
                outcome.identity.run_id
            )),
            Err(error) => notes.push(format!("{}: {error}", file.display())),
        }
    }
    (outcomes, notes)
}

/// Whether the runner's outcomes leave a (task, slot) of the config empty. A trial an
/// infrastructure interruption cut short does not fill a slot: it is replaced, and the
/// replacement does.
pub(crate) fn slots_missing(resolved: &ResolvedRun, outcomes: &[TrialOutcomeV1]) -> bool {
    let infra = |o: &TrialOutcomeV1| {
        o.exceptions
            .iter()
            .any(|e: &TrialException| e.kind == TrialExceptionKind::InfraInterrupted)
    };
    resolved.tasks.iter().any(|task| {
        (0..resolved.file.k).any(|slot| {
            !outcomes
                .iter()
                .any(|o| o.identity.task == *task && o.identity.slot == slot && !infra(o))
        })
    })
}

/// One shard's line in the job result: `ok` when every selected slot of it has an outcome and
/// each was collected, `skipped` when the runner left none, `failed` otherwise.
pub(crate) fn shard_results(
    resolved: &ResolvedRun,
    outcomes: &[TrialOutcomeV1],
) -> Vec<BenchShardResultV1> {
    resolved
        .shards
        .iter()
        .map(|shard| {
            let of_shard: Vec<&TrialOutcomeV1> = outcomes
                .iter()
                .filter(|o| shard.tasks.contains(&o.identity.task))
                .collect();
            let expected = shard.tasks.len() * resolved.file.k as usize;
            let status = if of_shard.is_empty() {
                "skipped"
            } else if of_shard.len() >= expected && of_shard.iter().all(|o| o.collection.collected)
            {
                "ok"
            } else {
                "failed"
            };
            BenchShardResultV1 {
                shard: shard.name.clone(),
                status: status.to_string(),
                trials: u32::try_from(of_shard.len()).unwrap_or(u32::MAX),
            }
        })
        .collect()
}

/// The trials whose owned resources were not cleaned up, for the job's detail.
pub(crate) fn uncleaned(outcomes: &[TrialOutcomeV1]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|o| !o.cleanup.cleaned)
        .map(|o| rsi_common::bench_runner::trial_key(&o.identity))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::config::{Dataset, RunFile, fixtures};
    use super::*;

    fn resolved(config: &str) -> ResolvedRun {
        let dataset = Dataset::parse(fixtures::DATASET_TOML, fixtures::SPLIT_JSON).unwrap();
        RunFile::parse(config)
            .unwrap()
            .resolve(config, dataset)
            .unwrap()
    }

    fn stamp_json(manifest: &RunnerSourceManifest) -> String {
        serde_json::json!({
            "runner_version": RUNNER_VERSION_DISCRIMINATOR,
            "source_digest": manifest.source_digest(),
            "evaluator_digest": manifest.evaluator_digest(),
            "source_manifest": manifest.to_text(),
            "missing": [],
        })
        .to_string()
    }

    fn manifest() -> RunnerSourceManifest {
        RunnerSourceManifest::from_entries([
            ("a.rs".to_string(), "a".repeat(64)),
            ("b.rs".to_string(), "b".repeat(64)),
        ])
        .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_self_consistent_stamp_admits_the_binary_and_carries_the_evaluator_identity() {
        let manifest = manifest();
        let runner =
            RunnerBinary::from_stamp("/install/rsi-eval".into(), &stamp_json(&manifest)).unwrap();
        assert_eq!(runner.evaluator_digest, manifest.evaluator_digest());
        assert_eq!(runner.manifest, manifest);
        assert_eq!(runner.path, PathBuf::from("/install/rsi-eval"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_stamp_that_is_foreign_incomplete_or_self_contradicting_is_refused() {
        let manifest = manifest();
        let good: serde_json::Value = serde_json::from_str(&stamp_json(&manifest)).unwrap();
        let refused = |edit: &dyn Fn(&mut serde_json::Value), why: &str| {
            let mut value = good.clone();
            edit(&mut value);
            let error = RunnerBinary::from_stamp("/x".into(), &value.to_string()).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        };
        refused(&|v| v["runner_version"] = "rsi-tb2/0".into(), "rsi-tb2/0");
        refused(&|v| v["missing"] = serde_json::json!(["x.rs"]), "x.rs");
        refused(
            &|v| v["evaluator_digest"] = format!("sha256:{}", "0".repeat(64)).into(),
            "disagrees",
        );
        refused(&|v| v["source_manifest"] = "garbage".into(), "invalid");
        assert!(RunnerBinary::from_stamp("/x".into(), "not json").is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_runner_is_the_installed_sibling_of_the_daemon_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rsid");
        std::fs::write(&exe, "").unwrap();
        let error = sibling_runner(&exe).unwrap_err();
        assert!(error.contains("make release-install"), "{error}");
        let runner = dir.path().join(format!(
            "{RUNNER_EXECUTABLE}{}",
            std::env::consts::EXE_SUFFIX
        ));
        std::fs::write(&runner, "").unwrap();
        assert_eq!(
            sibling_runner(&exe).unwrap(),
            std::fs::canonicalize(&runner).unwrap()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_runner_argv_is_fixed_and_carries_only_a_path() {
        assert_eq!(
            runner_argv(Path::new("/i/rsi-eval"), "run", Path::new("/b/plan.json")),
            [
                "/i/rsi-eval",
                "bench",
                "tb2",
                "run",
                "--plan",
                "/b/plan.json"
            ]
        );
        assert_eq!(
            stamp_argv(Path::new("/i/rsi-eval")),
            ["/i/rsi-eval", "bench", "tb2", "stamp"]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_oracle_config_plans_every_task_slot_with_its_own_pin_limits_and_deadlines() {
        let run = resolved(&fixtures::oracle_config());
        let hosts = PlanHosts {
            laptop_host: "arch-laptop",
            docker_hosts: &["ssh://arch-laptop".to_string()],
            concurrency: 2,
            max_concurrency: 4,
        };
        let paths = PlanPaths {
            checkout_dir: Path::new("/b/tb2/c"),
            dataset_path: Path::new("/b/r/inputs/dataset.toml"),
            output_root: Path::new("/b"),
        };
        let id = Uuid::new_v4();
        let plan = runner_plan(id, &run, &hosts, &paths).unwrap();
        plan.validate().unwrap();
        assert_eq!(plan.mode, RunnerMode::Oracle);
        assert_eq!(plan.run_id, id.to_string());
        assert_eq!(plan.trials.len(), 1, "the smoke split is one fixture task");
        let trial = &plan.trials[0];
        assert_eq!(trial.identity.task, "fix-git");
        assert_eq!((trial.identity.slot, trial.identity.retry), (0, 0));
        assert_eq!((trial.cpus, trial.memory_mb), (1, 2048));
        assert_eq!(
            (trial.agent_timeout_sec, trial.verifier_timeout_sec),
            (900, 900)
        );
        assert!(trial.image_digest.starts_with("sha256:1111"));
        let remote = plan.remote.unwrap();
        assert_eq!(remote.docker_host, "ssh://arch-laptop");
        assert_eq!(remote.laptop_host, "arch-laptop");
        assert_eq!(remote.allowed_docker_hosts, ["ssh://arch-laptop"]);
        assert_eq!(remote.effective_concurrency(), 2);
        assert_eq!(remote.checkout_dir, "/b/tb2/c");
        assert_eq!(remote.output_root, "/b");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_model_driven_config_plans_the_harness_mode_the_runner_does_not_yet_run() {
        let run = resolved(&fixtures::rsi_config());
        let hosts = PlanHosts {
            laptop_host: "arch-laptop",
            docker_hosts: &[],
            concurrency: 4,
            max_concurrency: 4,
        };
        let paths = PlanPaths {
            checkout_dir: Path::new("/c"),
            dataset_path: Path::new("/d"),
            output_root: Path::new("/o"),
        };
        let plan = runner_plan(Uuid::new_v4(), &run, &hosts, &paths).unwrap();
        assert_eq!(plan.mode, RunnerMode::Harness);
        assert_eq!(plan.trials.len(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_fractional_deadline_is_refused_not_truncated() {
        let mut run = resolved(&fixtures::oracle_config());
        run.dataset
            .tasks
            .get_mut("fix-git")
            .unwrap()
            .agent_timeout_sec = 900.5;
        let hosts = PlanHosts {
            laptop_host: "h",
            docker_hosts: &[],
            concurrency: 1,
            max_concurrency: 1,
        };
        let paths = PlanPaths {
            checkout_dir: Path::new("/c"),
            dataset_path: Path::new("/d"),
            output_root: Path::new("/o"),
        };
        let refusal = runner_plan(Uuid::new_v4(), &run, &hosts, &paths).unwrap_err();
        assert_eq!(refusal.code, BENCH_CONFIG_INVALID);
        assert!(refusal.detail.contains("900.5"), "{}", refusal.detail);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn each_preflight_failure_refuses_with_the_runners_typed_reason() {
        let stderr = "noise\ntb2_runner error [disk_insufficient]: 1 bytes free, 2 needed\n";
        let refusal = evaluate_preflight(Err(RunnerFailure::exited(Some(20), stderr))).unwrap_err();
        assert_eq!(refusal.code, BENCH_PREFLIGHT_FAILED);
        assert_eq!(refusal.detail, "disk_insufficient: 1 bytes free, 2 needed");

        let drift = "tb2_runner error [pin_drift]: 1 drift line(s) from the pinned dataset: x\n";
        let refusal = evaluate_preflight(Err(RunnerFailure::exited(Some(21), drift))).unwrap_err();
        assert_eq!(refusal.code, BENCH_PIN_CHECK_FAILED);
        assert!(refusal.detail.starts_with("pin_drift:"), "{refusal:?}");

        // A runner that could not run at all is a failed preflight, never a pass.
        let refusal = evaluate_preflight(Err(RunnerFailure {
            exit: None,
            detail: "No such file".into(),
        }))
        .unwrap_err();
        assert_eq!(refusal.code, BENCH_PREFLIGHT_FAILED);
        let refusal =
            evaluate_preflight(Err(RunnerFailure::exited(Some(101), "thread panicked\n")))
                .unwrap_err();
        assert_eq!(refusal.detail, "thread panicked");
        assert_eq!(evaluate_preflight(Ok(())), Ok(()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn runner_exits_map_to_their_refusals() {
        assert_eq!(exit_refusal(EXIT_PREFLIGHT), BENCH_PREFLIGHT_FAILED);
        assert_eq!(exit_refusal(EXIT_PIN_CHECK), BENCH_PIN_CHECK_FAILED);
        for code in [EXIT_SHARD, EXIT_SSH, 1, 137] {
            assert_eq!(exit_refusal(code), BENCH_TRANSPORT_FAILED);
        }
    }
}
