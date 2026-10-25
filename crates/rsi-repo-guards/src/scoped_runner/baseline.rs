//! Bounded baseline replay: rerun only the candidate's failing tests, by exact
//! identity, on a detached scratch worktree of the base (and, for failures the
//! tip does not reproduce, of the candidate's fork point). A nonzero exit alone
//! is a build/harness failure, never an observed test failure, and a replay that
//! did not finish cannot establish a baseline.
use super::{
    execution::PackagePlan,
    gitx::Git,
    orchestration::BaselineComparison,
    planner,
    provision::ScratchWorktree,
    receipt::{Failure, WorkerEvidence},
    selection::{self, Selection},
};
use crate::scratch_git::Policy;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// A run removes its baseline target when it settles; a killed or timed-out run
/// cannot, so the next one sweeps leftovers beyond any live run's budget.
pub const STALE_TARGET: Duration = Duration::from_secs(6 * 3600);

pub struct UnitOutcome {
    pub code: i32,
    pub timed_out: bool,
    pub output: String,
}
/// Runs one planned package worker in the replay's own target. The production
/// implementation wraps the unit with the run's isolation, slice and admission
/// environment; tests substitute an observed-output fake.
pub trait Unit {
    fn run(
        &mut self,
        plan: &PackagePlan,
        plan_path: &Path,
        budget: Duration,
        target: &Path,
    ) -> io::Result<UnitOutcome>;
    fn features(
        &mut self,
        worktree: &Path,
        base: &str,
        package: &str,
        selections: &[Selection],
    ) -> io::Result<Vec<String>>;
    /// Best effort: start the replay's empty target from the warm pool (never
    /// fails the replay; a cold build is the fallback). The default seeds nothing.
    fn seed(&mut self, _worktree: &Path, _target: &Path, _log_dir: &Path) {}
}

#[derive(Clone, Copy)]
pub struct Inputs<'a> {
    /// The original admitted repository (never a candidate scratch).
    pub repo: &'a Path,
    pub log_dir: &'a Path,
    pub cargo: &'a [String],
    pub tmpfs: Option<&'a Path>,
    pub e2e_prebuild: Option<&'a [String]>,
    pub cpu_quota: f64,
    pub target_dir: &'a Path,
}

/// A Cargo target the replay owns, never the candidate's: one target reused
/// across revisions leaves dependency artifacts from the other revision's
/// source. It lives under the assigned target tree and is removed on drop.
pub struct ReplayTarget {
    dir: tempfile::TempDir,
}
impl ReplayTarget {
    pub fn create(candidate_target: &Path) -> io::Result<Self> {
        if let Some(parent) = candidate_target.parent() {
            if let Some(name) = candidate_target.file_name() {
                sweep_stale(
                    parent,
                    &format!("{}-baseline-", name.to_string_lossy()),
                    STALE_TARGET,
                );
            }
        }
        let parent = candidate_target.join("scoped-baselines");
        std::fs::create_dir_all(&parent)?;
        sweep_stale(&parent, "run-", STALE_TARGET);
        Ok(Self {
            dir: tempfile::Builder::new().prefix("run-").tempdir_in(parent)?,
        })
    }
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Remove abandoned replay targets so disk use stays bounded.
pub fn sweep_stale(parent: &Path, prefix: &str, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(prefix) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let old = metadata
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if metadata.is_dir() && !metadata.file_type().is_symlink() && old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn unavailable(base: &str, detail: String) -> BaselineComparison {
    BaselineComparison {
        complete: false,
        failures: BTreeSet::new(),
        detail: json!({"status":"unavailable","base":base,"detail":detail,"baseline_reds":[]}),
        receipt: Map::new(),
    }
}

/// Rerun `failures` on `base`. `seconds` is the remaining comparison budget.
pub fn compare(
    git: &Git,
    policy: &Policy,
    inputs: &Inputs,
    unit: &mut dyn Unit,
    base: &str,
    failures: &[Failure],
    seconds: Duration,
) -> BaselineComparison {
    if failures.is_empty() {
        return BaselineComparison {
            complete: true,
            failures: BTreeSet::new(),
            detail: json!({"status":"not_needed","base":base,"baseline_reds":[]}),
            receipt: Map::new(),
        };
    }
    if seconds.is_zero() {
        return BaselineComparison {
            complete: false,
            failures: BTreeSet::new(),
            detail: json!({"status":"incomplete","base":base,"detail":"no comparison budget remained","baseline_reds":[]}),
            receipt: Map::new(),
        };
    }
    let worktree = match ScratchWorktree::create(git, policy, "rsi-scoped-baseline-", base) {
        Ok(worktree) => worktree,
        Err(error) => return unavailable(base, error.to_string()),
    };
    let target = match ReplayTarget::create(inputs.target_dir) {
        Ok(target) => target,
        Err(error) => return unavailable(base, format!("replay target: {error}")),
    };
    unit.seed(worktree.path(), target.path(), inputs.log_dir);
    let mut by_package = BTreeMap::<&str, Vec<&Failure>>::new();
    for failure in failures {
        by_package
            .entry(&failure.package)
            .or_default()
            .push(failure);
    }
    let started = Instant::now();
    let mut reds = BTreeSet::new();
    let mut checked: Vec<Value> = Vec::new();
    for (index, (package, expected)) in by_package.iter().enumerate() {
        let left = seconds.saturating_sub(started.elapsed());
        if index > 0 && left.is_zero() {
            break;
        }
        let selections: Vec<Selection> = expected
            .iter()
            .map(|f| (f.kind.as_str(), f.target.as_str()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|(kind, target)| Selection {
                filter: format!("baseline exact failures {kind}:{target}"),
                kind: kind.into(),
                target: if kind == "lib" {
                    String::new()
                } else {
                    target.into()
                },
                ..Selection::default()
            })
            .collect();
        let exact: Vec<Selection> = expected
            .iter()
            .map(|f| Selection {
                kind: f.kind.clone(),
                exact: Some(f.test.clone()),
                ..Selection::default()
            })
            .collect();
        let features = match unit.features(worktree.path(), base, package, &exact) {
            Ok(features) => features,
            Err(error) => {
                return unavailable(base, format!("{package}: {error}"));
            }
        };
        let plan = PackagePlan {
            repo: worktree.path().to_path_buf(),
            package: (*package).into(),
            head: base.into(),
            build: selection::build_command(inputs.cargo, package, &selections, &features, false),
            selections: selections.clone(),
            compile_only: false,
            features,
            exact_tests: Some(expected.iter().map(|f| f.test.clone()).collect()),
            changed_modules: vec![],
            direct_selections: vec![],
            slow_tier_excluded: BTreeSet::new(),
            time_box_sec: None,
            tmpfs: inputs.tmpfs.map(Path::to_path_buf),
            // The head run prebuilds rsid for e2e_tui; the base rerun must too, or
            // those tests return Ok at once and a base red is invisible.
            e2e: planner::selects_e2e(&selections)
                .then(|| {
                    inputs
                        .e2e_prebuild
                        .map(|command| json!({"prebuild": command}))
                })
                .flatten(),
            runner: None,
            nextest: None,
            test_shards: ((inputs.cpu_quota / 100.0) as usize).max(1),
            runtime_max_sec: left.as_secs_f64().max(1.0),
            tool_repo: Some(inputs.repo.to_path_buf()),
        };
        let plan_path: PathBuf = inputs.log_dir.join(format!("baseline-{package}.json"));
        let outcome = std::fs::write(
            &plan_path,
            format!("{}\n", serde_json::to_string(&plan).unwrap_or_default()),
        )
        .and_then(|()| {
            unit.run(
                &plan,
                &plan_path,
                left.max(Duration::from_secs(1)),
                target.path(),
            )
        });
        let (code, output, ran) = match outcome {
            Ok(done) => (done.code, done.output, true),
            Err(error) => (
                1,
                format!("\nscoped-test: base comparison {error}\n"),
                false,
            ),
        };
        let _ = std::fs::write(
            inputs.log_dir.join(format!("baseline-{package}.log")),
            &output,
        );
        let mut evidence = WorkerEvidence::default();
        for line in output.lines() {
            evidence.observe(package, line);
        }
        let wanted: BTreeSet<&Failure> = expected.iter().copied().collect();
        if ran && code != 0 {
            for failure in &evidence.failures {
                if wanted.contains(failure) {
                    reds.insert(failure.clone());
                }
            }
        }
        // `executed` is the worker's own end-of-run attestation, never inferred
        // from a successful-looking exit.
        if ran && evidence.executed.is_some() && !evidence.malformed {
            checked.extend(expected.iter().map(
                |f| json!({"package":f.package,"kind":f.kind,"target":f.target,"test":f.test}),
            ));
        }
    }
    let complete = checked.len() == failures.len();
    BaselineComparison {
        complete,
        failures: reds.clone(),
        detail: json!({
            "status": if complete { "complete" } else { "incomplete" },
            "base": base,
            "checked": checked,
            "baseline_reds": reds,
        }),
        receipt: Map::new(),
    }
}

/// The commit the candidate forked from `base`, or None when it is the tip
/// itself or unknown.
pub fn fork_point(git: &Git, head: &str, base: &str) -> Option<String> {
    let merge_base = git.builtin(&["merge-base", head, base], &[]).ok()?;
    let tip = git.commit(base).ok()??;
    let point = merge_base
        .ok()
        .then(|| merge_base.first_line().to_string())?;
    (!point.is_empty() && point != tip).then_some(point)
}

/// Failures the tip does not reproduce may be inherited from the candidate's
/// fork point and fixed on `base` since: rerun just those at the fork point;
/// the ones red there are not new.
#[allow(clippy::too_many_arguments)]
pub fn with_fork_point(
    git: &Git,
    policy: &Policy,
    inputs: &Inputs,
    unit: &mut dyn Unit,
    head: &str,
    base: &str,
    failures: &[Failure],
    tip: BaselineComparison,
    seconds: impl Fn() -> Duration,
) -> BaselineComparison {
    if !tip.complete {
        return tip;
    }
    let pending: Vec<Failure> = failures
        .iter()
        .filter(|f| !tip.failures.contains(*f))
        .cloned()
        .collect();
    let Some(point) = (!pending.is_empty())
        .then(|| fork_point(git, head, base))
        .flatten()
    else {
        return tip;
    };
    let nested = inputs.log_dir.join("fork-point");
    if std::fs::create_dir_all(&nested).is_err() {
        return tip;
    }
    let inner = Inputs {
        log_dir: &nested,
        ..*inputs
    };
    let check = compare(git, policy, &inner, unit, &point, &pending, seconds());
    let stale = if check.complete {
        check.failures.clone()
    } else {
        BTreeSet::new()
    };
    let mut detail = tip.detail.clone();
    detail["fork_point_comparison"] = json!({
        "merge_base": point,
        "status": check.detail["status"],
        "checked": check.detail["checked"],
    });
    let mut receipt = Map::new();
    if !stale.is_empty() {
        receipt.insert("stale_base_reds".into(), json!(stale));
        receipt.insert(
            "stale_base".into(),
            json!(format!(
                "{} failing test(s) were already red at your branch point and pass on {base}: merge {base} and re-run",
                stale.len()
            )),
        );
    }
    let mut failures = tip.failures;
    failures.extend(stale);
    BaselineComparison {
        complete: true,
        failures,
        detail,
        receipt,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil;
    use std::sync::atomic::AtomicUsize;

    struct Fake {
        script: Vec<(i32, String)>,
        plans: Vec<(String, Vec<String>, Option<BTreeSet<String>>, bool)>,
        seeded: Vec<(PathBuf, PathBuf)>,
    }
    impl Unit for Fake {
        fn run(
            &mut self,
            plan: &PackagePlan,
            _: &Path,
            _: Duration,
            target: &Path,
        ) -> io::Result<UnitOutcome> {
            assert!(target.is_dir());
            assert!(
                plan.repo.join("a.txt").is_file(),
                "runs inside the base worktree"
            );
            self.plans.push((
                plan.package.clone(),
                plan.build.clone(),
                plan.exact_tests.clone(),
                plan.e2e.is_some(),
            ));
            let (code, output) = self.script.remove(0);
            Ok(UnitOutcome {
                code,
                timed_out: false,
                output,
            })
        }
        fn features(
            &mut self,
            _: &Path,
            _: &str,
            _: &str,
            _: &[Selection],
        ) -> io::Result<Vec<String>> {
            Ok(vec![])
        }
        fn seed(&mut self, worktree: &Path, target: &Path, log_dir: &Path) {
            assert!(log_dir.is_dir());
            self.seeded
                .push((worktree.to_path_buf(), target.to_path_buf()));
        }
    }
    fn fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        String,
        BTreeMap<String, String>,
    ) {
        let repo = tempfile::tempdir().unwrap();
        testutil::git(repo.path(), &["init", "-q", "-b", "main"]);
        testutil::write(repo.path(), "a.txt", "one\n");
        testutil::git(repo.path(), &["add", "a.txt"]);
        testutil::git(
            repo.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "base",
            ],
        );
        let head = testutil::git(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        (
            repo,
            tempfile::tempdir().unwrap(),
            head,
            super::super::gitx::test_environment(),
        )
    }
    fn failure(package: &str, test: &str) -> Failure {
        Failure {
            package: package.into(),
            kind: "lib".into(),
            target: package.into(),
            test: test.into(),
        }
    }

    #[test]
    fn only_a_finished_replay_that_reproduced_the_named_failure_is_a_baseline_red() {
        let (repo, logs, base, env) = fixture();
        let target = repo.path().join("target");
        let cancel = AtomicUsize::new(0);
        let git = Git {
            repo: repo.path(),
            env: &env,
            cancel: &cancel,
        };
        let policy = Policy::new(repo.path(), repo.path(), Default::default(), &[]).unwrap();
        let cargo = vec!["cargo".to_string()];
        let inputs = Inputs {
            repo: repo.path(),
            log_dir: logs.path(),
            cargo: &cargo,
            tmpfs: None,
            e2e_prebuild: None,
            cpu_quota: 200.0,
            target_dir: &target,
        };
        let failures = vec![
            failure("pkg", "suite::red"),
            failure("pkg", "suite::fixed"),
            failure("other", "suite::other"),
        ];
        let mut unit = Fake {
            script: vec![
                // `other`: sorted first; a nonzero exit with no attestation is a harness failure.
                (1, "SCOPED_FAILURE lib:other suite::other\n".into()),
                // `pkg`: finished, reproduced `red`, not the unrelated name nor `fixed`.
                (1, "SCOPED_FAILURE lib:pkg suite::red\nSCOPED_FAILURE lib:pkg suite::unlisted\nSCOPED_EXECUTED pkg 2\n".into()),
            ],
            plans: vec![],
            seeded: vec![],
        };
        let result = compare(
            &git,
            &policy,
            &inputs,
            &mut unit,
            &base,
            &failures,
            Duration::from_secs(60),
        );
        assert!(
            !result.complete,
            "the `other` replay never attested completion"
        );
        assert_eq!(result.detail["status"], "incomplete");
        // Observed reds are reported, but an incomplete comparison is never used to classify.
        assert_eq!(
            result.failures,
            BTreeSet::from([
                failure("other", "suite::other"),
                failure("pkg", "suite::red")
            ])
        );
        // The replay target is offered the warm pool once, before the first package runs.
        assert_eq!(unit.seeded.len(), 1);
        assert!(
            unit.seeded[0]
                .1
                .starts_with(target.join("scoped-baselines"))
        );
        assert_eq!(result.detail["checked"].as_array().unwrap().len(), 2);
        assert_eq!(unit.plans[0].0, "other");
        assert_eq!(
            unit.plans[1].2,
            Some(BTreeSet::from([
                "suite::fixed".to_string(),
                "suite::red".to_string()
            ]))
        );
        // The replay's own scratch is gone and the worktree is deregistered.
        assert_eq!(
            testutil::git(repo.path(), &["worktree", "list"])
                .lines()
                .count(),
            1
        );
        assert!(
            std::fs::read_dir(target.join("scoped-baselines"))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(logs.path().join("baseline-pkg.log").is_file());
    }

    #[test]
    fn a_successful_exit_never_turns_failure_looking_output_into_a_red_and_complete_needs_all_checks()
     {
        let (repo, logs, base, env) = fixture();
        let target = repo.path().join("target");
        let cancel = AtomicUsize::new(0);
        let git = Git {
            repo: repo.path(),
            env: &env,
            cancel: &cancel,
        };
        let policy = Policy::new(repo.path(), repo.path(), Default::default(), &[]).unwrap();
        let cargo = vec!["cargo".to_string()];
        let inputs = Inputs {
            repo: repo.path(),
            log_dir: logs.path(),
            cargo: &cargo,
            tmpfs: None,
            e2e_prebuild: None,
            cpu_quota: 200.0,
            target_dir: &target,
        };
        let failures = vec![failure("pkg", "suite::flaky")];
        let mut unit = Fake {
            script: vec![(
                0,
                "SCOPED_FAILURE lib:pkg suite::flaky\nSCOPED_EXECUTED pkg 1\n".into(),
            )],
            plans: vec![],
            seeded: vec![],
        };
        let result = compare(
            &git,
            &policy,
            &inputs,
            &mut unit,
            &base,
            &failures,
            Duration::from_secs(60),
        );
        assert!(result.complete);
        assert!(result.failures.is_empty());
        assert_eq!(result.detail["status"], "complete");
        // No failures to replay and no budget are decided before any worktree exists.
        let none = compare(
            &git,
            &policy,
            &inputs,
            &mut unit,
            &base,
            &[],
            Duration::from_secs(1),
        );
        assert_eq!(none.detail["status"], "not_needed");
        let spent = compare(
            &git,
            &policy,
            &inputs,
            &mut unit,
            &base,
            &failures,
            Duration::ZERO,
        );
        assert!(!spent.complete);
        assert_eq!(spent.detail["detail"], "no comparison budget remained");
        // An unresolvable base is unavailable, never a pass.
        let missing = compare(
            &git,
            &policy,
            &inputs,
            &mut unit,
            "no-such-ref",
            &failures,
            Duration::from_secs(1),
        );
        assert_eq!(missing.detail["status"], "unavailable");
        assert!(!missing.complete);
    }

    #[test]
    fn fork_point_replay_marks_inherited_reds_stale_and_stale_targets_are_swept() {
        let (repo, logs, _base, env) = fixture();
        let cancel = AtomicUsize::new(0);
        let git = Git {
            repo: repo.path(),
            env: &env,
            cancel: &cancel,
        };
        let fork = testutil::git(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        testutil::git(repo.path(), &["checkout", "-q", "-b", "cand"]);
        testutil::write(repo.path(), "b.txt", "b\n");
        testutil::git(repo.path(), &["add", "b.txt"]);
        testutil::git(
            repo.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "cand",
            ],
        );
        let head = testutil::git(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        testutil::git(repo.path(), &["checkout", "-q", "main"]);
        testutil::write(repo.path(), "c.txt", "c\n");
        testutil::git(repo.path(), &["add", "c.txt"]);
        testutil::git(
            repo.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "tip",
            ],
        );
        let tip = testutil::git(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        assert_eq!(fork_point(&git, &head, &tip), Some(fork.clone()));
        assert_eq!(fork_point(&git, &tip, &tip), None);
        let target = repo.path().join("target");
        let policy = Policy::new(repo.path(), repo.path(), Default::default(), &[]).unwrap();
        let cargo = vec!["cargo".to_string()];
        let inputs = Inputs {
            repo: repo.path(),
            log_dir: logs.path(),
            cargo: &cargo,
            tmpfs: None,
            e2e_prebuild: None,
            cpu_quota: 100.0,
            target_dir: &target,
        };
        let failures = vec![failure("pkg", "suite::inherited")];
        let tip_result = BaselineComparison {
            complete: true,
            failures: BTreeSet::new(),
            detail: json!({"status":"complete","base":tip,"checked":[],"baseline_reds":[]}),
            receipt: Map::new(),
        };
        let mut unit = Fake {
            script: vec![(
                1,
                "SCOPED_FAILURE lib:pkg suite::inherited\nSCOPED_EXECUTED pkg 1\n".into(),
            )],
            plans: vec![],
            seeded: vec![],
        };
        let merged = with_fork_point(
            &git,
            &policy,
            &inputs,
            &mut unit,
            &head,
            &tip,
            &failures,
            tip_result,
            || Duration::from_secs(30),
        );
        assert!(merged.complete);
        assert_eq!(
            merged.failures,
            BTreeSet::from([failure("pkg", "suite::inherited")])
        );
        assert_eq!(merged.detail["fork_point_comparison"]["merge_base"], fork);
        assert_eq!(
            merged.receipt["stale_base_reds"].as_array().unwrap().len(),
            1
        );
        assert!(
            merged.receipt["stale_base"]
                .as_str()
                .unwrap()
                .contains("already red at your branch point")
        );

        let old = target.join("scoped-baselines").join("run-old");
        std::fs::create_dir_all(&old).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        sweep_stale(
            &target.join("scoped-baselines"),
            "run-",
            Duration::from_millis(5),
        );
        assert!(!old.exists());
    }
}
