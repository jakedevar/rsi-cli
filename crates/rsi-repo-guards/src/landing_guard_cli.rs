//! `rsi-repo-guard rolling-landing-guard`: the Rust replacement for
//! `scripts/rolling-landing-guard.py` (#363). Same flags, JSON report, exit codes
//! (0 clean, 1 lost hunks, 2 refusal) and `landing guard: <reason>` stderr line.
//!
//! The migration proof is recomputed in-process with the #361 `inspect`/`prove`
//! APIs by this very executable, so the process the caller launches is the
//! killable boundary (a signal or the caller's deadline stops owned children).
//! The candidate's own static target-reclaim policy is checked by *this*
//! executable's code over an exported copy of the committed candidate; no
//! candidate-authored code ever decides it. Which executable the lander
//! launches (trusted materialization) belongs to hub #3621, not to this module.
use crate::cli::{self, Opt};
use crate::landing_guard::{Accepted, Backend, Proof, Repository, ValidatedProof, evaluate_with};
use crate::qa_fingerprint::sha256;
use crate::scoped_runner::process;
use crate::scratch_git::{Environment, Policy};
use crate::{Ctx, migration_renumber, target_reclaim};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

/// Static Git/matching/proof work shares this bound (the legacy guard had none;
/// the lander's own deadline still applies to the whole child).
pub const STATIC_BOUND: Duration = Duration::from_secs(30 * 60);
/// One `cargo test` command of the affected-crate gate.
pub const TEST_COMMAND_BOUND: Duration = Duration::from_secs(4 * 60 * 60);
const TARGET_RECLAIM_MARKERS: [&str; 2] = [
    "scripts/check-target-reclaim.py",
    "crates/rsi-repo-guards/src/target_reclaim.rs",
];
const TARGET_RECLAIM_PATHS: [&str; 3] = ["crates", "scripts", "desktop/scripts"];

const OPTIONS: [Opt; 9] = [
    Opt {
        name: "--repo",
        value: true,
    },
    Opt {
        name: "--base",
        value: true,
    },
    Opt {
        name: "--source",
        value: true,
    },
    Opt {
        name: "--target",
        value: true,
    },
    Opt {
        name: "--candidate",
        value: true,
    },
    Opt {
        name: "--worktree",
        value: true,
    },
    Opt {
        name: "--test-filter",
        value: true,
    },
    Opt {
        name: "--proof",
        value: true,
    },
    Opt {
        name: "--plan-only",
        value: false,
    },
];
const USAGE: &str = "[-h] [--repo REPO] --base BASE --source SOURCE --target TARGET --candidate CANDIDATE [--worktree WORKTREE] [--plan-only] [--test-filter PACKAGE=FILTER] [--proof PROOF]";

fn refuse(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}

/// The caller's environment as Git and Cargo should see it: no repository
/// location or RSI daemon variable can steer either one.
pub fn ambient(ctx: &Ctx) -> Environment {
    ctx.vars()
        .filter(|(key, _)| {
            let upper = key.to_ascii_uppercase();
            !upper.starts_with("GIT_") && !upper.starts_with("RSI_")
        })
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
}

/// `json.dumps(indent=2)`: two-space pretty output with every non-ASCII
/// character escaped, so a consumer reading the legacy bytes sees the same text.
pub fn pretty_ascii(value: &Value) -> String {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            escaped.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                escaped.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    escaped
}

struct Production<'w> {
    worktree: PathBuf,
    environment: Environment,
    cancel: &'w AtomicUsize,
    out: &'w mut dyn Write,
    err: &'w mut dyn Write,
}

impl Production<'_> {
    fn string_environment(&self) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = self
            .environment
            .iter()
            .filter_map(|(k, v)| Some((k.to_str()?.to_owned(), v.to_str()?.to_owned())))
            .collect();
        env.insert("CARGO_BUILD_JOBS".into(), "6".into());
        env.insert("CARGO_PROFILE_DEV_DEBUG".into(), "line-tables-only".into());
        env
    }
}

fn is_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Backend for Production<'_> {
    /// The candidate's committed static target-reclaim policy, before any build.
    /// A tree older than the guard has nothing to check (the legacy skip).
    fn check_candidate(&mut self, repo: &Repository<'_>, candidate: &str) -> io::Result<()> {
        let mut present = false;
        for marker in TARGET_RECLAIM_MARKERS {
            present |= repo.exists(&format!("{candidate}:{marker}"))?;
        }
        if !present {
            return Ok(());
        }
        let mut paths = Vec::new();
        for path in TARGET_RECLAIM_PATHS {
            if repo.exists(&format!("{candidate}:{path}"))? {
                paths.push(path);
            }
        }
        let scratch = Policy::from_process(repo.root())?.tempdir("rsi-target-reclaim-guard-")?;
        repo.export(candidate, &paths, scratch.path())?;
        let ctx = Ctx::new(scratch.path(), &[]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let root = scratch.path().to_string_lossy().into_owned();
        let code = target_reclaim::main(&["--root".into(), root], &ctx, &mut out, &mut err);
        if code != 0 {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err)
            );
            return Err(refuse(format!(
                "check-target-reclaim failed: {}",
                text.trim()
            )));
        }
        Ok(())
    }

    fn validate_proof(
        &mut self,
        repo: &Repository<'_>,
        pair: &Accepted<'_>,
        proof_file: &Path,
    ) -> io::Result<ValidatedProof> {
        let bytes = std::fs::read(proof_file)?;
        let refused = |error: String| refuse(format!("provisional proof refused: {error}"));
        let proof: Value =
            serde_json::from_slice(&bytes).map_err(|error| refused(error.to_string()))?;
        let bound = |key: &str, value: &str| proof.get(key).and_then(Value::as_str) == Some(value);
        if !(bound("base", pair.base)
            && bound("source", pair.source)
            && bound("candidate", pair.candidate))
        {
            return Err(refuse(
                "provisional proof does not bind the accepted pair and candidate",
            ));
        }
        let target = proof
            .get("target")
            .and_then(Value::as_str)
            .filter(|target| is_oid(target))
            .ok_or_else(|| refuse("provisional proof has no running target"))?;
        if !repo.ancestor(pair.target, target)? {
            return Err(refuse(
                "provisional running target is outside the fetched target ancestry",
            ));
        }
        let unit = migration_renumber::inspect(repo.root(), pair.base, pair.source, target, false)
            .map_err(&refused)?
            .ok_or_else(|| refuse("provisional proof has no source migration unit"))?;
        let unit_candidate = proof
            .get("unit_candidate")
            .and_then(Value::as_str)
            .unwrap_or("");
        let expected =
            migration_renumber::prove(repo.root(), &unit, unit_candidate, pair.candidate)
                .map_err(&refused)?;
        if proof != expected {
            return Err(refuse(
                "provisional proof differs from independently derived spans and blobs",
            ));
        }
        Ok(ValidatedProof {
            proof: Proof::from_value(&proof)?,
            assigned_version: proof
                .get("assigned_version")
                .cloned()
                .unwrap_or(Value::Null),
            unit_candidate: proof.get("unit_candidate").cloned().unwrap_or(Value::Null),
            digest: format!("sha256:{}", sha256(&bytes)),
        })
    }

    fn run_tests(&mut self, candidate: &str, commands: &[Vec<String>]) -> io::Result<()> {
        let tested = Repository::new(
            &self.worktree,
            &self.environment,
            Instant::now() + STATIC_BOUND,
            self.cancel,
        )?;
        if tested.rev_parse("HEAD")? != candidate {
            return Err(refuse("test worktree HEAD does not equal candidate"));
        }
        if !tested.worktree_is_clean()? {
            return Err(refuse("test worktree is dirty"));
        }
        let env = self.string_environment();
        for argv in commands {
            process::python_free(argv)?;
            writeln!(self.out, "running: {}", argv.join(" "))?;
            self.out.flush()?;
            let sinks = RefCell::new((&mut *self.out, &mut *self.err));
            let write = |pipe: usize, text: &str| {
                let mut sinks = sinks.borrow_mut();
                let _ = if pipe == 0 {
                    sinks.0.write_all(text.as_bytes())
                } else {
                    sinks.1.write_all(text.as_bytes())
                };
            };
            let outcome = process::stream_channels(
                argv,
                &self.worktree,
                &env,
                Instant::now() + TEST_COMMAND_BOUND,
                self.cancel,
                &write,
                |_: usize, _: &str| {},
            )?;
            if let Some(signal) = outcome.cancelled {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("landing guard cancelled by signal {signal}"),
                ));
            }
            if outcome.timed_out || outcome.code != 0 || !outcome.output_complete {
                return Err(refuse(format!(
                    "affected-crate tests failed: {}",
                    argv.join(" ")
                )));
            }
        }
        Ok(())
    }
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match process::install_signals() {
        Ok(cancel) => main_with(args, ctx, out, err, &cancel),
        Err(error) => {
            let _ = writeln!(err, "landing guard: {error}");
            2
        }
    }
}

pub fn main_with(
    args: &[String],
    ctx: &Ctx,
    out: &mut dyn Write,
    err: &mut dyn Write,
    cancel: &AtomicUsize,
) -> i32 {
    let parsed = match cli::parse(
        "rolling-landing-guard",
        USAGE,
        args,
        &OPTIONS,
        &[],
        Some(0),
        err,
    ) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.code(),
    };
    let missing: Vec<&str> = ["--base", "--source", "--target", "--candidate"]
        .into_iter()
        .filter(|name| parsed.value(name).is_none())
        .collect();
    if !missing.is_empty() {
        let _ = writeln!(err, "usage: rolling-landing-guard {USAGE}");
        let _ = writeln!(
            err,
            "rolling-landing-guard: error: the following arguments are required: {}",
            missing.join(", ")
        );
        return 2;
    }
    match run(&parsed, ctx, out, err, cancel) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "landing guard: {error}");
            2
        }
    }
}

fn run(
    parsed: &cli::Parsed,
    ctx: &Ctx,
    out: &mut dyn Write,
    err: &mut dyn Write,
    cancel: &AtomicUsize,
) -> io::Result<i32> {
    let required = |name: &str| parsed.value(name).unwrap_or_default().to_owned();
    let (base, source, target, candidate) = (
        required("--base"),
        required("--source"),
        required("--target"),
        required("--candidate"),
    );
    let environment = ambient(ctx);
    let deadline = Instant::now() + STATIC_BOUND;
    let opened = ctx.path(parsed.value("--repo").unwrap_or("."));
    let top = Repository::new(&opened, &environment, deadline, cancel)?.toplevel()?;
    let repo = Repository::new(&top, &environment, deadline, cancel)?;
    let pair = Accepted {
        base: &base,
        source: &source,
        target: &target,
        candidate: &candidate,
    };
    let worktree = parsed
        .value("--worktree")
        .map_or_else(|| top.clone(), |path| ctx.path(path));
    let proof = parsed.value("--proof").map(|path| ctx.path(path));
    let mut backend = Production {
        worktree,
        environment,
        cancel,
        out: &mut *out,
        err: &mut *err,
    };
    let (code, report) = evaluate_with(
        &repo,
        &pair,
        parsed.all("--test-filter"),
        parsed.flag("--plan-only"),
        proof.as_deref(),
        &mut backend,
    )?;
    writeln!(out, "{}", pretty_ascii(&report))?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git, repo_root, scratch_repository, write};
    use serde_json::json;

    fn ctx(dir: &Path) -> Ctx {
        Ctx::new(
            dir,
            &[
                ("PATH", &std::env::var("PATH").unwrap()),
                ("HOME", &std::env::var("HOME").unwrap_or_default()),
                ("GIT_DIR", "/nonexistent"),
                ("RSI_SESSION_TOKEN", "never-forwarded"),
            ],
        )
    }
    fn head(dir: &Path) -> String {
        git(dir, &["rev-parse", "HEAD"]).trim().to_owned()
    }
    fn commit(dir: &Path, message: &str) -> String {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
        head(dir)
    }
    /// base -> accepted source (changes a default) in a repository outside the checkout.
    struct Case {
        temp: tempfile::TempDir,
        base: String,
        source: String,
    }
    fn case() -> Case {
        let temp = scratch_repository();
        let dir = temp.path();
        git(dir, &["init", "-q", "-b", "trunk"]);
        write(
            dir,
            "crates/fixture/Cargo.toml",
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        );
        write(dir, "crates/fixture/src/lib.rs", "old default\nkeep\n");
        let base = commit(dir, "base");
        write(dir, "crates/fixture/src/lib.rs", "new default\nkeep\n");
        let source = commit(dir, "accepted");
        Case { temp, base, source }
    }
    fn run_cli(dir: &Path, args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let cancel = AtomicUsize::new(0);
        let code = main_with(&args, &ctx(dir), &mut out, &mut err, &cancel);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn report_is_two_space_json_with_ascii_escapes() {
        let text = pretty_ascii(&json!({"a":["é","😀"],"b":[],"c":{}}));
        assert_eq!(
            text,
            "{\n  \"a\": [\n    \"\\u00e9\",\n    \"\\ud83d\\ude00\"\n  ],\n  \"b\": [],\n  \"c\": {}\n}"
        );
    }

    #[test]
    fn caller_environment_never_forwards_repository_or_daemon_variables() {
        let dir = tempfile::tempdir().unwrap();
        let environment = ambient(&ctx(dir.path()));
        assert!(environment.contains_key(std::ffi::OsStr::new("PATH")));
        assert!(
            !environment
                .keys()
                .any(|key| key == "GIT_DIR" || key == "RSI_SESSION_TOKEN")
        );
    }

    #[test]
    fn plan_only_reports_the_candidate_and_affected_crate_in_the_legacy_shape() {
        let case = case();
        let dir = case.temp.path();
        let dir_text = dir.to_string_lossy().into_owned();
        let (code, out, err) = run_cli(
            dir,
            &[
                "--repo",
                &dir_text,
                "--base",
                &case.base,
                "--source",
                &case.source,
                "--target",
                &case.base,
                "--candidate",
                &case.source,
                "--plan-only",
                "--test-filter",
                "fixture=a_test",
            ],
        );
        assert_eq!((code, err.as_str()), (0, ""), "{err}");
        assert!(
            out.starts_with("{\n  \"outcome\": \"candidate\",\n"),
            "{out}"
        );
        let report: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["accepted_source"], case.source);
        assert_eq!(report["affected_crates"], json!(["fixture"]));
        assert_eq!(
            report["test_commands"],
            json!(["cargo test -p fixture --lib a_test"])
        );
        assert_eq!(report["tests_executed"], false);
        assert_eq!(report["lost_hunks"], json!([]));
    }

    #[test]
    fn a_reverted_accepted_line_exits_one_and_a_bad_pair_exits_two() {
        let case = case();
        let dir = case.temp.path();
        write(dir, "crates/fixture/src/lib.rs", "old default\nkeep\n");
        let candidate = commit(dir, "bad resolution");
        let args = |base: &str, source: &str, candidate: &str| {
            [
                "--repo".to_owned(),
                dir.to_string_lossy().into_owned(),
                "--base".into(),
                base.into(),
                "--source".into(),
                source.into(),
                "--target".into(),
                case.source.clone(),
                "--candidate".into(),
                candidate.into(),
                "--plan-only".into(),
            ]
        };
        let values = args(&case.base, &case.source, &candidate);
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        let (code, out, _) = run_cli(dir, &refs);
        assert_eq!(code, 1, "{out}");
        let report: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["lost_hunks"].as_array().unwrap().len(), 1);

        let values = args(&case.source, &case.base, &candidate);
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        let (code, out, err) = run_cli(dir, &refs);
        assert_eq!(code, 2);
        assert_eq!(out, "");
        assert_eq!(
            err,
            "landing guard: accepted base is not an ancestor of source\n"
        );

        let (code, _, err) = run_cli(dir, &["--plan-only"]);
        assert_eq!(code, 2);
        assert!(
            err.ends_with(
                "the following arguments are required: --base, --source, --target, --candidate\n"
            ),
            "{err}"
        );
    }

    #[test]
    fn export_is_committed_content_only_and_handles_long_paths() {
        let case = case();
        let dir = case.temp.path();
        let long = format!("crates/fixture/{}/leaf.rs", "d".repeat(140));
        write(dir, &long, "committed\n");
        let candidate = commit(dir, "long path");
        write(dir, "crates/fixture/src/lib.rs", "dirty worktree edit\n");
        write(dir, "crates/fixture/src/untracked.rs", "untracked\n");
        let cancel = AtomicUsize::new(0);
        let environment = ambient(&ctx(dir));
        let repo = Repository::new(
            dir,
            &environment,
            Instant::now() + Duration::from_secs(60),
            &cancel,
        )
        .unwrap();
        let out = tempfile::tempdir().unwrap();
        let count = repo.export(&candidate, &["crates"], out.path()).unwrap();
        assert_eq!(count, 3);
        assert_eq!(
            std::fs::read_to_string(out.path().join(&long)).unwrap(),
            "committed\n"
        );
        assert_eq!(
            std::fs::read_to_string(out.path().join("crates/fixture/src/lib.rs")).unwrap(),
            "new default\nkeep\n"
        );
        assert!(!out.path().join("crates/fixture/src/untracked.rs").exists());
        assert!(!repo.worktree_is_clean().unwrap());
    }

    #[test]
    fn the_target_reclaim_policy_is_skipped_for_older_trees_and_fails_closed_otherwise() {
        let case = case();
        let dir = case.temp.path();
        let cancel = AtomicUsize::new(0);
        let environment = ambient(&ctx(dir));
        let mut backend = Production {
            worktree: dir.to_path_buf(),
            environment: environment.clone(),
            cancel: &cancel,
            out: &mut Vec::new(),
            err: &mut Vec::new(),
        };
        let repo = Repository::new(
            dir,
            &environment,
            Instant::now() + Duration::from_secs(60),
            &cancel,
        )
        .unwrap();
        backend.check_candidate(&repo, &case.source).unwrap();
        // A tree that carries the guard but not its policy inputs is red, never skipped.
        write(
            dir,
            "crates/rsi-repo-guards/src/target_reclaim.rs",
            "// marker\n",
        );
        let marked = commit(dir, "guard marker without policy");
        let error = backend.check_candidate(&repo, &marked).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("check-target-reclaim failed: "),
            "{error}"
        );
    }

    #[test]
    fn the_target_writer_policy_reads_the_committed_candidate_even_in_plan_only() {
        let case = case();
        let dir = case.temp.path();
        let root = repo_root();
        for relative in [
            "crates/rsi-repo-guards/src/target_reclaim.rs",
            "scripts/target-reclaim-retained.json",
            "crates/rsid-store/src/sandbox/target_reclaim.rs",
            "crates/rsi-common/src/sandbox_storage.rs",
        ] {
            write(
                dir,
                relative,
                &std::fs::read_to_string(root.join(relative)).unwrap(),
            );
        }
        let good = commit(dir, "install target writer guard");
        let cancel = AtomicUsize::new(0);
        let environment = ambient(&ctx(dir));
        let repo = Repository::new(
            dir,
            &environment,
            Instant::now() + Duration::from_secs(120),
            &cancel,
        )
        .unwrap();
        let mut backend = Production {
            worktree: dir.to_path_buf(),
            environment: environment.clone(),
            cancel: &cancel,
            out: &mut Vec::new(),
            err: &mut Vec::new(),
        };
        backend.check_candidate(&repo, &good).unwrap();
        write(
            dir,
            "desktop/scripts/build.sh",
            "mkdir -p target/new-thing\n",
        );
        let candidate = commit(dir, "unregistered target writer");
        // A clean-looking working tree must not hide the committed writer.
        write(
            dir,
            "desktop/scripts/build.sh",
            "mkdir -p target/.rsi-tmp/new-thing\n",
        );
        let dir_text = dir.to_string_lossy().into_owned();
        let (code, out, err) = run_cli(
            dir,
            &[
                "--repo",
                &dir_text,
                "--base",
                &good,
                "--source",
                &candidate,
                "--target",
                &good,
                "--candidate",
                &candidate,
                "--plan-only",
            ],
        );
        assert_eq!((code, out.as_str()), (2, ""));
        assert!(
            err.contains("desktop/scripts/build.sh:1: target/new-thing"),
            "{err}"
        );
        // The older good commit still passes with the bad working tree present.
        write(
            dir,
            "desktop/scripts/build.sh",
            "mkdir -p target/new-other\n",
        );
        backend.check_candidate(&repo, &good).unwrap();
    }

    #[test]
    fn exporting_the_real_committed_tree_yields_every_regular_file() {
        let checkout = repo_root();
        let cancel = AtomicUsize::new(0);
        let environment = ambient(&ctx(&checkout));
        let repo = Repository::new(
            &checkout,
            &environment,
            Instant::now() + Duration::from_secs(300),
            &cancel,
        )
        .unwrap();
        let tip = repo.rev_parse("HEAD").unwrap();
        let listing = git(
            &checkout,
            &[
                "ls-tree",
                "-r",
                &tip,
                "--",
                "crates",
                "scripts",
                "desktop/scripts",
            ],
        );
        let regular: Vec<&str> = listing
            .lines()
            .filter(|line| line.starts_with("100644 blob") || line.starts_with("100755 blob"))
            .collect();
        assert!(regular.len() > 1000, "{}", regular.len());
        let out = tempfile::tempdir().unwrap();
        let exported = repo
            .export(&tip, &["crates", "scripts", "desktop/scripts"], out.path())
            .unwrap();
        assert_eq!(exported, regular.len());
        // The pinned policy input is byte-identical to the committed blob.
        let committed = repo
            .blob(&tip, Path::new("crates/rsi-repo-guards/src/lib.rs"))
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(out.path().join("crates/rsi-repo-guards/src/lib.rs")).unwrap(),
            committed
        );
    }

    #[test]
    fn tests_run_only_in_a_clean_worktree_at_the_candidate_and_stop_at_the_first_red() {
        let case = case();
        let dir = case.temp.path();
        let cancel = AtomicUsize::new(0);
        let environment = ambient(&ctx(dir));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut backend = Production {
            worktree: dir.to_path_buf(),
            environment,
            cancel: &cancel,
            out: &mut out,
            err: &mut err,
        };
        let git_head = vec![vec!["git".to_owned(), "rev-parse".into(), "HEAD".into()]];
        assert!(
            backend
                .run_tests(&case.base, &git_head)
                .unwrap_err()
                .to_string()
                .contains("HEAD does not equal candidate")
        );
        write(dir, "crates/fixture/src/dirty.rs", "x\n");
        assert!(
            backend
                .run_tests(&case.source, &git_head)
                .unwrap_err()
                .to_string()
                .contains("test worktree is dirty")
        );
        std::fs::remove_file(dir.join("crates/fixture/src/dirty.rs")).unwrap();
        let red = vec![
            vec!["git".to_owned(), "rev-parse".into(), "HEAD".into()],
            vec!["git".to_owned(), "no-such-subcommand-x".into()],
            vec!["git".to_owned(), "rev-parse".into(), "never-run".into()],
        ];
        let error = backend.run_tests(&case.source, &red).unwrap_err();
        assert_eq!(
            error.to_string(),
            "affected-crate tests failed: git no-such-subcommand-x"
        );
        drop(backend);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(&format!("running: git rev-parse HEAD\n{}", case.source)));
        assert!(!out.contains("never-run"), "{out}");
    }

    #[cfg(unix)]
    #[test]
    fn an_unterminated_final_line_reaches_the_caller_exactly_once() {
        let case = case();
        let dir = case.temp.path();
        let cancel = AtomicUsize::new(0);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut backend = Production {
            worktree: dir.to_path_buf(),
            environment: ambient(&ctx(dir)),
            cancel: &cancel,
            out: &mut out,
            err: &mut err,
        };
        let commands = vec![vec![
            "sh".to_owned(),
            "-c".into(),
            "printf 'only-once'; printf 'warned' >&2".into(),
        ]];
        backend.run_tests(&case.source, &commands).unwrap();
        drop(backend);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "running: sh -c printf 'only-once'; printf 'warned' >&2\nonly-once"
        );
        assert_eq!(String::from_utf8(err).unwrap(), "warned");
    }

    #[test]
    fn python_is_refused_as_a_test_command() {
        let case = case();
        let dir = case.temp.path();
        let cancel = AtomicUsize::new(0);
        let mut backend = Production {
            worktree: dir.to_path_buf(),
            environment: ambient(&ctx(dir)),
            cancel: &cancel,
            out: &mut Vec::new(),
            err: &mut Vec::new(),
        };
        let error = backend
            .run_tests(&case.source, &[vec!["python3".to_owned(), "x".into()]])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
