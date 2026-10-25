//! `Contracts` backed by the sibling conversions that have published staged,
//! callable Rust interfaces in this crate: #362's shard inventory and static
//! gates, #361's released-migration gate and renumber tool. What no sibling has
//! published (#426 derivation, #366 residual suites) is delegated to `Staged`
//! and stays red. Nothing here re-implements a sibling's policy.
use super::{
    contracts::{Contracts, Derivation, Incomplete, Renumbered, SlowTier, Staged, gap},
    derivation,
    gitx::Git,
    process,
};
use crate::Ctx;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::{Duration, Instant};

/// The retained bound on one renumber-tool step (the lander gives it 300 s).
pub const RENUMBER_TIMEOUT: Duration = Duration::from_secs(300);
/// The retained bound on one static gate (#447 sets the lander's static budget at
/// 30 minutes); `guard-regressions` compares at most a few guard versions, each
/// once capped at 120 s, so it gets a tighter hard stop.
pub const GATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const GUARD_REGRESSIONS_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const GATE_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const RENUMBER_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const REASON_LIMIT: usize = 500;

pub struct Converged {
    environment: BTreeMap<String, String>,
    runner: PathBuf,
    cancel: Arc<AtomicUsize>,
    staged: Staged,
    /// Tests only: run a gate inside this process instead of as a killable child.
    in_process_gates: bool,
}
impl Converged {
    pub fn new(ctx: &Ctx, runner: PathBuf) -> io::Result<Self> {
        Ok(Self {
            environment: process::build_env(ctx),
            runner,
            cancel: process::install_signals()?,
            staged: Staged,
            in_process_gates: false,
        })
    }
    fn ctx(&self, repo: &Path) -> Ctx {
        let vars: Vec<(&str, &str)> = self
            .environment
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Ctx::new(repo, &vars)
    }
    /// One static gate as a killable child of this very binary: its deadline and
    /// cancellation never depend on the gate's output, and a gate that wedges is
    /// stopped with its whole process group instead of hanging the run.
    fn gate(
        &self,
        cli: &str,
        main: crate::GuardMain,
        repo: &Path,
        args: &[String],
        timeout: Duration,
    ) -> Value {
        if self.in_process_gates {
            return run_guard(main, &self.ctx(repo), args);
        }
        let mut argv = vec![self.runner.to_string_lossy().into_owned(), cli.to_string()];
        argv.extend(args.iter().cloned());
        run_supervised_gate(
            cli,
            &argv,
            repo,
            &self.environment,
            Instant::now() + timeout,
            &self.cancel,
        )
    }
    /// One killable step of this very binary's `rolling-migration-renumber`
    /// entry: its deadline and cancellation never depend on the tool's output.
    fn renumber_step(&self, repo: &Path, args: &[&str]) -> Result<Value, String> {
        let mut argv: Vec<String> = vec![
            self.runner.to_string_lossy().into_owned(),
            "rolling-migration-renumber".into(),
            "--repo".into(),
            repo.to_string_lossy().into_owned(),
        ];
        argv.extend(args.iter().map(|a| (*a).into()));
        let (mut stdout, mut stderr) = (String::new(), String::new());
        let mut oversized = false;
        let outcome = process::stream_channels(
            &argv,
            repo,
            &self.environment,
            Instant::now() + RENUMBER_TIMEOUT,
            &self.cancel,
            |pipe, line| {
                let sink = if pipe == 0 { &mut stdout } else { &mut stderr };
                if sink.len() + line.len() <= RENUMBER_OUTPUT_LIMIT {
                    sink.push_str(line);
                } else {
                    oversized = true;
                }
            },
            |_, _| {},
        )
        .map_err(|error| error.to_string())?;
        if outcome.timed_out || outcome.cancelled.is_some() || !outcome.output_complete || oversized
        {
            return Err("the renumber step did not finish within its bounds".into());
        }
        if outcome.code != 0 {
            let text = if stderr.trim().is_empty() {
                &stdout
            } else {
                &stderr
            };
            let text = text.trim();
            return Err(if text.is_empty() {
                format!("exit {}", outcome.code)
            } else {
                text.chars().take(REASON_LIMIT).collect()
            });
        }
        serde_json::from_str(stdout.trim()).map_err(|error| error.to_string())
    }
}

/// Run `argv` to completion within `deadline` and fold it into the receipt's
/// `{status, detail}` gate shape (stdout then stderr, as the legacy gate printed).
pub(super) fn run_supervised_gate(
    gate: &str,
    argv: &[String],
    repo: &Path,
    environment: &BTreeMap<String, String>,
    deadline: Instant,
    cancel: &AtomicUsize,
) -> Value {
    let mut stdout = [Vec::new(), Vec::new()];
    let mut overflowed = false;
    let outcome =
        process::stream_bytes(argv, repo, environment, deadline, cancel, |pipe, bytes| {
            if stdout[0].len() + stdout[1].len() + bytes.len() <= GATE_OUTPUT_LIMIT {
                stdout[pipe.min(1)].extend_from_slice(bytes);
            } else {
                overflowed = true;
            }
        });
    let fail = |detail: String| json!({"status": "fail", "detail": detail});
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return fail(format!("{gate}: cannot start: {error}")),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&stdout[0]),
        String::from_utf8_lossy(&stdout[1])
    );
    let text = text.trim();
    if outcome.timed_out {
        return fail(format!(
            "{gate}: stopped at its deadline before finishing{}",
            if text.is_empty() {
                String::new()
            } else {
                format!("; output so far: {text}")
            }
        ));
    }
    if let Some(signal) = outcome.cancelled {
        return fail(format!("{gate}: cancelled by signal {signal}"));
    }
    if overflowed || !outcome.output_complete {
        return fail(format!(
            "{gate}: its output was incomplete or over the limit"
        ));
    }
    json!({"status": if outcome.code == 0 { "ok" } else { "fail" }, "detail": text})
}

fn run_guard(main: crate::GuardMain, ctx: &Ctx, args: &[String]) -> Value {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = main(args, ctx, &mut out, &mut err);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    json!({"status": if code == 0 { "ok" } else { "fail" }, "detail": text.trim()})
}

impl Contracts for Converged {
    fn derive(&mut self, repo: &Path, base: &str, head: &str) -> Result<Derivation, Incomplete> {
        let environment = self
            .environment
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        derivation::filters_only(repo, base, head, environment, &self.cancel)
    }
    fn shard_packages(&mut self, repo: &Path) -> Result<BTreeMap<String, Vec<String>>, Incomplete> {
        crate::rsid_test_shards::read_shard_packages(repo)
            .map(|shards| shards.into_iter().map(|s| (s.name, s.packages)).collect())
            .map_err(|error| Incomplete {
                detail: Some(error),
                ..gap("#362", "shard ownership inventory (read_shard_packages)")
            })
    }
    fn feature_test_inventory(
        &mut self,
        repo: &Path,
        head: &str,
        package: &str,
    ) -> Result<Vec<(String, Vec<String>)>, Incomplete> {
        derivation::feature_inventory(repo, head, package)
    }
    fn load_slow_tier(&mut self, repo: &Path, head: &str) -> Result<Option<SlowTier>, Incomplete> {
        derivation::slow_tier(repo, head)
    }
    fn downstream_test_plan(
        &mut self,
        repo: &Path,
        base: &str,
        head: &str,
    ) -> Result<Value, Incomplete> {
        derivation::downstream_plan(repo, base, head, &self.cancel)
    }
    fn renumber(
        &mut self,
        repo: &Path,
        head: &str,
        rolling: &str,
    ) -> Result<Renumbered, Incomplete> {
        let env = self.environment.clone();
        let git = Git {
            repo,
            env: &env,
            cancel: &self.cancel,
        };
        let refuse = |source: &str, reason: String| Renumbered::Refused {
            source: source.to_string(),
            reason,
        };
        let (Ok(Some(source)), Ok(Some(target))) = (git.commit(head), git.commit(rolling)) else {
            return Ok(Renumbered::Absent);
        };
        let base = match git.builtin(&["merge-base", &source, &target], &[]) {
            Ok(out) if out.ok() && !out.first_line().is_empty() => out.first_line().to_string(),
            _ => return Ok(Renumbered::Absent),
        };
        let unit = match self.renumber_step(
            repo,
            &[
                "--transform",
                "--base",
                &base,
                "--source",
                &source,
                "--target",
                &target,
            ],
        ) {
            Ok(unit) => unit,
            Err(reason) => return Ok(refuse(&source, reason)),
        };
        if unit["renumber_noop"] == true {
            // The version is still free: plain inspection validates it at latest+1
            // with the same tool, without creating a transformed candidate.
            if unit["migration_already_landed"] != true {
                if let Err(reason) = self.renumber_step(
                    repo,
                    &["--base", &base, "--source", &source, "--target", &target],
                ) {
                    return Ok(refuse(&source, reason));
                }
            }
            return Ok(Renumbered::Absent);
        }
        let Some(parent) = repo.parent() else {
            return Ok(refuse(
                &source,
                "the repository has no parent directory".into(),
            ));
        };
        let scratch = match tempfile::Builder::new()
            .prefix(".rsi-scoped-renumber-")
            .tempdir_in(parent)
        {
            Ok(scratch) => scratch,
            Err(error) => return Ok(refuse(&source, error.to_string())),
        };
        let unit_file = scratch.path().join("unit.json");
        if let Err(error) = std::fs::write(&unit_file, unit.to_string()) {
            return Ok(refuse(&source, error.to_string()));
        }
        let built = match self.renumber_step(
            repo,
            &[
                "--build",
                "--unit-file",
                &unit_file.to_string_lossy(),
                "--scratch",
                &scratch.path().to_string_lossy(),
            ],
        ) {
            Ok(built) => built,
            Err(reason) => return Ok(refuse(&source, reason)),
        };
        let (Some(candidate), Some(old), Some(new)) = (
            built["candidate"].as_str(),
            unit["old_version"].as_u64(),
            unit["new_version"].as_u64(),
        ) else {
            return Ok(refuse(
                &source,
                "the renumber tool returned no candidate".into(),
            ));
        };
        Ok(Renumbered::Done {
            source,
            target,
            old_version: old,
            new_version: new,
            candidate: candidate.into(),
        })
    }
    fn guard(
        &mut self,
        name: &str,
        repo: &Path,
        base: &str,
        head: &str,
    ) -> Result<Value, Incomplete> {
        let root = repo.to_string_lossy().into_owned();
        // (cli subcommand, entry point, arguments, hard deadline)
        let (cli, main, args, timeout): (&str, crate::GuardMain, Vec<String>, Duration) = match name
        {
            "shard_gates" => (
                "rsid-test-shards",
                crate::rsid_test_shards::main,
                vec!["--require-gates".into(), "--root".into(), root],
                GATE_TIMEOUT,
            ),
            "released_migrations" => (
                "released-migrations",
                crate::released_migrations::main,
                vec![base.into(), head.into()],
                GATE_TIMEOUT,
            ),
            "target_reclaim" => (
                "target-reclaim",
                crate::target_reclaim::main,
                vec!["--root".into(), root],
                GATE_TIMEOUT,
            ),
            "guard_regressions" => (
                "guard-regressions",
                crate::guard_regressions::main,
                vec![base.into(), head.into(), "--repo".into(), root],
                GUARD_REGRESSIONS_TIMEOUT,
            ),
            "store_boundary" => (
                "store-boundary",
                crate::store_boundary::main,
                vec!["--root".into(), root],
                GATE_TIMEOUT,
            ),
            "new_python" => (
                "new-python",
                crate::new_python::main,
                vec![base.into(), head.into()],
                GATE_TIMEOUT,
            ),
            "starter_pins" => (
                "starter-pins",
                crate::starter_pins::main,
                vec![base.into(), head.into()],
                GATE_TIMEOUT,
            ),
            "settings_preflight" => (
                "settings-preflight",
                crate::settings_preflight::main,
                vec![base.into(), head.into()],
                GATE_TIMEOUT,
            ),
            "identity_assertions" => (
                "identity-assertions",
                crate::identity_assertions::main,
                vec![],
                GATE_TIMEOUT,
            ),
            "test_env_writes" => (
                "test-env-writes",
                crate::test_env_writes::main,
                vec![],
                GATE_TIMEOUT,
            ),
            "personal_paths" => (
                "personal-paths",
                crate::personal_paths::main,
                vec![],
                GATE_TIMEOUT,
            ),
            "agents_md_size" => (
                "agents-md-size",
                crate::agents_md_size::main,
                vec![],
                GATE_TIMEOUT,
            ),
            "portable_shell" => (
                "portable-shell",
                crate::portable_shell::main,
                vec!["--base".into(), base.into()],
                GATE_TIMEOUT,
            ),
            _ => return self.staged.guard(name, repo, base, head),
        };
        Ok(self.gate(cli, main, repo, &args, timeout))
    }
    fn residual(&mut self, repo: &Path, base: &str, head: &str) -> Result<Vec<Value>, Incomplete> {
        derivation::residual(repo, base, head, &self.cancel)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil;

    fn converged(dir: &Path) -> Converged {
        let ctx = Ctx::new(
            dir,
            &[
                ("PATH", &std::env::var("PATH").unwrap()),
                ("HOME", &std::env::var("HOME").unwrap()),
            ],
        );
        let mut converged = Converged::new(&ctx, PathBuf::from("/nonexistent-runner")).unwrap();
        converged.in_process_gates = true;
        converged
    }

    #[test]
    fn a_supervised_gate_reports_exit_output_and_is_stopped_at_its_deadline() {
        let sh = |script: &str| vec!["/bin/sh".to_string(), "-c".into(), script.into()];
        let run = |script: &str, wait: Duration, cancel: usize| {
            run_supervised_gate(
                "demo-gate",
                &sh(script),
                Path::new("/tmp"),
                &BTreeMap::new(),
                Instant::now() + wait,
                &AtomicUsize::new(cancel),
            )
        };
        let long = Duration::from_secs(20);
        let ok = run("echo fine", long, 0);
        assert_eq!(ok, json!({"status": "ok", "detail": "fine"}));
        let failed = run("echo out; echo oops >&2; exit 3", long, 0);
        assert_eq!(failed, json!({"status": "fail", "detail": "out\noops"}));
        let start = Instant::now();
        let stuck = run(
            "echo started; sleep 60 & wait",
            Duration::from_millis(150),
            0,
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(stuck["status"], "fail");
        let detail = stuck["detail"].as_str().unwrap();
        assert!(
            detail.starts_with("demo-gate: stopped at its deadline"),
            "{detail}"
        );
        assert!(detail.contains("started"), "{detail}");
        let cancelled = run("sleep 60", long, 15);
        assert_eq!(cancelled["detail"], "demo-gate: cancelled by signal 15");
        let missing = run_supervised_gate(
            "demo-gate",
            &["/definitely/missing/runner".to_string()],
            Path::new("/tmp"),
            &BTreeMap::new(),
            Instant::now() + long,
            &AtomicUsize::new(0),
        );
        assert_eq!(missing["status"], "fail");
        assert!(
            missing["detail"]
                .as_str()
                .unwrap()
                .starts_with("demo-gate: cannot start"),
            "{missing}"
        );
    }
    #[test]
    fn published_sibling_gates_run_in_process_and_unpublished_contracts_stay_red() {
        let dir = tempfile::tempdir().unwrap();
        testutil::git(dir.path(), &["init", "-q", "-b", "main"]);
        let mut contracts = converged(dir.path());
        for name in [
            "store_boundary",
            "target_reclaim",
            "agents_md_size",
            "personal_paths",
            "identity_assertions",
        ] {
            let value = contracts.guard(name, dir.path(), "base", "head").unwrap();
            assert!(
                ["ok", "fail"].contains(&value["status"].as_str().unwrap()),
                "{name}: {value}"
            );
        }
        assert_eq!(
            contracts
                .guard("unknown_gate", dir.path(), "b", "h")
                .unwrap_err()
                .owner,
            "#362"
        );
        // #426 is wired in process (an unresolvable revision is a named refusal, never an
        // empty selection); only #366 has published nothing callable: still red.
        let refused = contracts.derive(dir.path(), "b", "h");
        assert!(refused.is_err() || refused.unwrap().exit_code != 0);
        assert_eq!(
            contracts.residual(dir.path(), "b", "h").unwrap_err().owner,
            "#366"
        );
        // An unreadable shard manifest is a named error, not an empty inventory.
        let error = contracts.shard_packages(dir.path()).unwrap_err();
        assert_eq!(error.owner, "#362");
        assert!(error.detail.is_some() || error.value()["status"] == "incomplete_contract");
    }

    #[test]
    fn a_source_without_a_resolvable_merge_base_needs_no_renumber() {
        let dir = tempfile::tempdir().unwrap();
        testutil::git(dir.path(), &["init", "-q", "-b", "main"]);
        let mut contracts = converged(dir.path());
        assert!(matches!(
            contracts
                .renumber(dir.path(), "missing", "origin/rolling")
                .unwrap(),
            Renumbered::Absent
        ));
    }

    #[test]
    fn the_runner_plans_a_real_diff_through_the_wired_contracts() {
        let gate = |shard: &str, name: &str| {
            format!(
                "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-{shard}\"))]\n#[test]\nfn {name}() {{}}\n"
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        testutil::git(root, &["init", "-q", "-b", "main"]);
        let commit = |message: &str| {
            testutil::git(root, &["add", "-A"]);
            testutil::git(
                root,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    message,
                ],
            );
            testutil::git(root, &["rev-parse", "HEAD"])
                .trim()
                .to_string()
        };
        testutil::write(
            root,
            "crates/rsid/Cargo.toml",
            "[features]\ndefault = []\ntest-shard-mode = []\ntest-shard-store-02 = [\"test-shard-mode\"]\n",
        );
        testutil::write(
            root,
            "crates/rsid-store/Cargo.toml",
            "[features]\ndefault = []\ntest-shard-mode = []\ntest-shard-store-01 = [\"test-shard-mode\"]\n",
        );
        testutil::write(root, "crates/rsid/src/lib.rs", "mod foo;\n");
        testutil::write(root, "crates/rsid-store/src/lib.rs", &gate("store-01", "s"));
        testutil::write(root, "crates/rsid/src/foo.rs", &gate("store-02", "a"));
        testutil::write(
            root,
            "scripts/tests/test_foo.py",
            "crates/rsid/src/foo.rs\n",
        );
        let base = commit("base");
        testutil::write(
            root,
            "crates/rsid/src/foo.rs",
            &(gate("store-02", "a") + "pub fn changed() {}\n"),
        );
        let head = commit("head");
        let mut contracts = converged(root);
        let ctx = Ctx::new(
            root,
            &[
                ("PATH", &std::env::var("PATH").unwrap()),
                ("HOME", &std::env::var("HOME").unwrap()),
            ],
        );
        let args: Vec<String> = [
            "--base",
            &base,
            "--head",
            &head,
            "--dry-run",
            "--runner",
            "libtest",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let options = crate::scoped_runner::options::parse(&args, None, &mut Vec::new()).unwrap();
        let cancel = Arc::new(AtomicUsize::new(0));
        let built = crate::scoped_runner::runner::build(&options, &ctx, &mut contracts, &cancel)
            .unwrap_or_else(|refusal| panic!("planning refused: {}", refusal.diagnostic));
        // The derivation ran in process and produced a real plan (no staged gap).
        assert_eq!(built.run.head, head);
        assert!(
            !built.run.filters.is_empty(),
            "{:?}",
            built.run.receipt_fields
        );
        assert_eq!(built.run.receipt_fields["plan"], "precise");
        assert_eq!(built.run.receipt_fields["slow_tier"]["mode"], "full");
        // The residual lane selects the one suite that names the changed file, and only it.
        let lane = contracts.residual(root, &base, &head).unwrap();
        assert_eq!(lane.len(), 1, "{lane:?}");
        assert_eq!(lane[0]["test_file"], "scripts/tests/test_foo.py");
        assert_eq!(lane[0]["status"], "incomplete_contract");
    }

    #[test]
    fn a_renumber_step_that_cannot_start_is_a_refusal_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        testutil::git(dir.path(), &["init", "-q", "-b", "main"]);
        testutil::write(dir.path(), "a.txt", "one\n");
        testutil::git(dir.path(), &["add", "a.txt"]);
        testutil::git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "one",
            ],
        );
        let contracts = converged(dir.path());
        let error = contracts
            .renumber_step(dir.path(), &["--transform"])
            .unwrap_err();
        assert!(!error.is_empty());
    }
}
