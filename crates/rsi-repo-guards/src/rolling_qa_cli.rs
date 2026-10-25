//! `rsi-repo-guard run-rolling-qa`: the Rust replacement for
//! `scripts/run-rolling-qa.py` (#363). It runs every QA lane against the exact
//! fetched rolling tree and never publishes refs.
//!
//! The orchestration is `orchestrate`, generic over [`Qa`] so the source pin,
//! resume, checkpoint and report logic is tested without a build. The production
//! [`Host`] runs each lane on the shared owned process pump (deadline, signal
//! cancellation, drained output) under the run's one `Isolation` probe. Honest
//! limits, all fail-closed:
//! * the shard lanes call `scripts/run-rsid-test-shards.sh`, which still shells
//!   out to Python in this tree (#362 inventory, #448 tracks the conversion); the lane
//!   refuses to start until that script is Python-free (`incomplete_contract`);
//! * the report is the existing v1 compatibility schema with this runner's own
//!   provenance; it is not a daemon-attested scoped receipt (hub #3621);
//! * Linux namespace/cgroup behaviour of the wrapped lanes is unverified here.
use crate::cli::{self, Opt};
use crate::pid_isolation::{self, Isolation};
use crate::qa_fingerprint;
use crate::rolling_qa::{
    self, Backend, Lane, ReportContext, State, evidence_directory, execute_pending, plan,
};
use crate::rsid_test_shards;
use crate::scoped_runner::{
    contracts::{Incomplete, gap},
    gitx::Git,
    process,
};
use crate::{Ctx, scratch_git::LOCATION_VARIABLES};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::{Duration, Instant};

/// The runner name recorded in `provenance.runner`.
pub const RUNNER: &str = "rsi-repo-guard run-rolling-qa";
/// Bounds (the legacy script had none; the daemon job timeout still applies).
pub const STATIC_BOUND: Duration = Duration::from_secs(30 * 60);
pub const LANE_BOUND: Duration = Duration::from_secs(3 * 60 * 60);
const CLASSIFIER_BUILD_BOUND: Duration = Duration::from_secs(20 * 60);
const RPC_BOUND: Duration = Duration::from_secs(60);
const RPC_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
const USAGE: &str =
    "[-h] --evidence-dir EVIDENCE_DIR [--jobs JOBS] [--closed-since CLOSED_SINCE] [--resume]";
const OPTIONS: [Opt; 4] = [
    Opt {
        name: "--evidence-dir",
        value: true,
    },
    Opt {
        name: "--jobs",
        value: true,
    },
    Opt {
        name: "--closed-since",
        value: true,
    },
    Opt {
        name: "--resume",
        value: false,
    },
];

fn fail(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub evidence_dir: PathBuf,
    pub jobs: u32,
    pub closed_since: Option<String>,
    pub resume: bool,
}

/// argparse-equivalent parsing; a usage problem is `Err(exit code)` after printing.
pub fn parse_args(args: &[String], err: &mut dyn Write) -> Result<Args, i32> {
    let parsed = cli::parse("run-rolling-qa", USAGE, args, &OPTIONS, &[], Some(0), err)
        .map_err(|exit| exit.code())?;
    let usage_error = |err: &mut dyn Write, message: &str| {
        let _ = writeln!(err, "usage: run-rolling-qa {USAGE}");
        let _ = writeln!(err, "run-rolling-qa: error: {message}");
        2
    };
    let Some(evidence_dir) = parsed.value("--evidence-dir") else {
        return Err(usage_error(
            err,
            "the following arguments are required: --evidence-dir",
        ));
    };
    let jobs = match parsed.value("--jobs") {
        None => 4,
        Some(text) => match text.trim().parse::<i64>() {
            Ok(value) => value,
            Err(_) => {
                return Err(usage_error(
                    err,
                    &format!("argument --jobs: invalid int value: '{text}'"),
                ));
            }
        },
    };
    if !(1..=4).contains(&jobs) {
        return Err(usage_error(err, "--jobs must be 1..4"));
    }
    Ok(Args {
        evidence_dir: PathBuf::from(evidence_dir),
        jobs: u32::try_from(jobs).unwrap_or(4),
        closed_since: parsed.value("--closed-since").map(str::to_owned),
        resume: parsed.flag("--resume"),
    })
}

/// Everything the orchestration needs that is not a lane: the source pin, the
/// shard/integration inventories, the optional classifier and the Issue listing.
pub trait Services {
    fn fetch(&mut self) -> io::Result<()>;
    fn rev_parse(&mut self, revision: &str) -> io::Result<String>;
    /// True when staged or unstaged tracked changes exist.
    fn tracked_dirty(&mut self) -> io::Result<bool>;
    fn fingerprint(&mut self, tip: &str, shard: &str, jobs: u32) -> io::Result<Value>;
    fn shard_names(&mut self) -> io::Result<Vec<String>>;
    fn integration_targets(&mut self) -> io::Result<Vec<String>>;
    /// Known-failure annotations; unavailable means `{}` and never breaks a sweep.
    fn signature_matches(&mut self, rows: &[Value]) -> Value;
    fn closed_issues(&mut self, since: Option<&str>) -> io::Result<Value>;
    fn host_name(&self) -> String;
    fn now(&self) -> String;
    fn say(&mut self, text: &str);
    fn warn(&mut self, text: &str);
}
pub trait Qa: Services + Backend {}
impl<T: Services + Backend> Qa for T {}

fn relative(repo: &Path, path: &Path) -> io::Result<String> {
    path.strip_prefix(repo)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .map_err(|_| fail("QA evidence path is outside the repository"))
}

/// Port of `main`: returns the exit code (0 only for a complete, all-green sweep).
pub fn orchestrate(
    args: &Args,
    repo: &Path,
    directory: &Path,
    qa: &mut impl Qa,
) -> io::Result<i32> {
    qa.fetch()?;
    let mut state = if args.resume {
        let text = fs::read_to_string(directory.join("state.json"))?;
        let state: State = serde_json::from_str(&text)
            .map_err(|e| fail(format!("QA state is unreadable: {e}")))?;
        state.validate()?;
        let fingerprint = qa.fingerprint(&state.tip, "store-01", args.jobs)?;
        let head_tree = qa.rev_parse("HEAD^{tree}")?;
        let dirty = qa.tracked_dirty()?;
        state.resume(
            args.jobs,
            args.closed_since.as_deref(),
            &fingerprint,
            &head_tree,
            dirty,
        )?;
        state
    } else {
        let tip = qa.rev_parse("origin/rolling")?;
        let tree = qa.rev_parse("origin/rolling^{tree}")?;
        if qa.rev_parse("HEAD^{tree}")? != tree {
            return Err(fail(
                "HEAD tree differs from fetched origin/rolling; integrate first",
            ));
        }
        if qa.tracked_dirty()? {
            return Err(fail(
                "tracked worktree changes would contaminate the QA sweep",
            ));
        }
        if let Some(parent) = directory.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir(directory)?;
        let state = State {
            tip: tip.clone(),
            tree,
            jobs: args.jobs,
            started_at: qa.now(),
            fingerprint: qa.fingerprint(&tip, "store-01", args.jobs)?,
            closed_since: args.closed_since.clone(),
            results: Vec::new(),
        };
        state.save(directory)?;
        state
    };
    let shards = qa.shard_names()?;
    let integrations = qa.integration_targets()?;
    let lanes = plan(&shards, &integrations, args.jobs)?;
    execute_pending(&mut state, &lanes, qa, |checkpoint| {
        checkpoint.save(directory)
    })?;
    if state
        .results
        .iter()
        .any(|row| row["label"] == "static" && row["exit"] != 0)
    {
        qa.warn("static gate failed; shard execution is unsafe");
    }
    qa.fetch()?;
    let final_tip = qa.rev_parse("origin/rolling")?;
    // Known-failure annotation (#1016) must never break a sweep.
    let signatures = qa.signature_matches(&state.results);
    let finished_at = qa.now();
    let report_ref = relative(repo, &directory.join("sweep-report.json"))?;
    let closed_issues = qa.closed_issues(state.closed_since.as_deref())?;
    let (tip, jobs, host) = (state.tip.clone(), args.jobs, qa.host_name());
    let (code, report, sweep) = rolling_qa::reports(
        &state,
        ReportContext {
            final_tip: &final_tip,
            finished_at: &finished_at,
            host: &host,
            runner: RUNNER,
            report_ref: &report_ref,
            closed_issues,
            signature_matches: signatures.clone(),
        },
        |shard| qa.fingerprint(&tip, shard, jobs),
    )?;
    rolling_qa::atomic_json(&directory.join("report.json"), &report)?;
    rolling_qa::atomic_json(&directory.join("sweep-report.json"), &sweep)?;
    let mut totals = BTreeMap::from([("known", 0u64), ("known?", 0), ("new", 0)]);
    for matches in signatures.as_object().into_iter().flat_map(|m| m.values()) {
        for found in matches.as_object().into_iter().flat_map(|m| m.values()) {
            if let Some(count) = found["state"].as_str().and_then(|s| totals.get_mut(s)) {
                *count += 1;
            }
        }
    }
    qa.say(&format!(
        "known={} known?={} new={}",
        totals["known"], totals["known?"], totals["new"]
    ));
    qa.say(&format!(
        "report: {}",
        directory.join("report.json").display()
    ));
    Ok(code)
}

/// The production host: real Git, the owned process pump and `rsi-rpc`.
pub struct Host<'a> {
    ctx: Ctx,
    repo: PathBuf,
    directory: PathBuf,
    /// Ambient environment (every lane saw `os.environ` in the legacy script).
    environment: BTreeMap<String, String>,
    git_environment: BTreeMap<String, String>,
    cancel: Arc<AtomicUsize>,
    isolation: Isolation,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    attempts: u32,
}

impl<'a> Host<'a> {
    pub fn new(
        ctx: &Ctx,
        repo: PathBuf,
        directory: PathBuf,
        runner: String,
        cancel: Arc<AtomicUsize>,
        out: &'a mut dyn Write,
        err: &'a mut dyn Write,
    ) -> Self {
        let environment: BTreeMap<String, String> = ctx
            .vars()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let git_environment = environment
            .iter()
            .filter(|(k, _)| !LOCATION_VARIABLES.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut ctx = ctx.clone();
        ctx.cwd = repo.clone();
        let isolation = pid_isolation::probe(
            &ctx,
            &[runner.clone(), "pid-isolate".into(), "--canary".into()],
        );
        Self {
            ctx,
            repo,
            directory,
            environment,
            git_environment,
            cancel,
            isolation,
            out,
            err,
            attempts: 0,
        }
    }
    fn git(&self) -> Git<'_> {
        Git {
            repo: &self.repo,
            env: &self.git_environment,
            cancel: &self.cancel,
        }
    }
    fn slot(&self) -> io::Result<PathBuf> {
        let installed = self.ctx.home().join(".rsi/bin/cargo-slot");
        installed.exists().then_some(installed).ok_or_else(|| {
            fail("cargo-slot admission unavailable: ~/.rsi/bin/cargo-slot is not installed")
        })
    }
    /// One bounded lane command: output goes to the log and to standard output.
    fn command(
        &mut self,
        argv: &[String],
        env: &BTreeMap<String, String>,
        log: &mut fs::File,
        output: &mut String,
        bound: Duration,
    ) -> io::Result<i32> {
        process::python_free(argv)?;
        let wrapped = self.isolation.wrap(argv);
        let sinks = std::cell::RefCell::new((log, output, None::<io::Error>));
        let out = std::cell::RefCell::new(&mut self.out);
        let record = |text: &str, echo: bool| {
            let mut sinks = sinks.borrow_mut();
            sinks.1.push_str(text);
            if let Err(e) = sinks.0.write_all(text.as_bytes())
                && sinks.2.is_none()
            {
                sinks.2 = Some(e);
            }
            if echo {
                let _ = out.borrow_mut().write_all(text.as_bytes());
            }
        };
        let done = process::stream(
            &wrapped,
            &self.repo,
            env,
            Instant::now() + bound,
            &self.cancel,
            |line| record(line, true),
            |_unterminated_preview| {},
        )?;
        if let Some(e) = sinks.into_inner().2 {
            return Err(e);
        }
        if done.cancelled.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "QA lane interrupted; the lane was not recorded",
            ));
        }
        if done.timed_out {
            return Ok(124);
        }
        if !done.output_complete {
            return Err(fail("QA lane output was incomplete"));
        }
        Ok(done.code)
    }
    fn rpc(&self, method: &str, params: &Value) -> io::Result<Value> {
        let argv = vec![
            "rsi-rpc".to_owned(),
            method.to_owned(),
            "--params".into(),
            params.to_string(),
        ];
        let (mut stdout, mut stderr) = (String::new(), String::new());
        let done = process::stream_channels(
            &argv,
            &self.repo,
            &self.environment,
            Instant::now() + RPC_BOUND,
            &self.cancel,
            |pipe, line| {
                let sink = if pipe == 0 { &mut stdout } else { &mut stderr };
                if sink.len() + line.len() <= RPC_OUTPUT_LIMIT {
                    sink.push_str(line);
                }
            },
            |_, _| {},
        )?;
        if done.timed_out || done.cancelled.is_some() || !done.output_complete || done.code != 0 {
            return Err(fail(format!(
                "{method}: rsi-rpc failed (exit {}): {}",
                done.code,
                stderr.trim().chars().take(300).collect::<String>()
            )));
        }
        let envelope: Value = serde_json::from_str(stdout.trim())
            .map_err(|e| fail(format!("{method}: unreadable reply: {e}")))?;
        match envelope.get("error") {
            Some(error) => Err(fail(format!("{method}: {error}"))),
            None => envelope
                .get("result")
                .cloned()
                .ok_or_else(|| fail(format!("{method}: reply has no result"))),
        }
    }
    /// The shared classifier binary, built once when missing (bounded).
    fn classifier(&mut self) -> io::Result<PathBuf> {
        let target = self
            .environment
            .get("CARGO_TARGET_DIR")
            .map(|t| Path::new(t).join("debug").join(exe("rsi-known-failure")));
        if let Some(binary) = target.as_ref().filter(|b| b.exists()) {
            return Ok(binary.clone());
        }
        let argv = vec![
            self.slot()?.to_string_lossy().into_owned(),
            "cargo".into(),
            "build".into(),
            "-p".into(),
            "rsi-common".into(),
            "--bin".into(),
            "rsi-known-failure".into(),
        ];
        let mut sink = fs::File::create(self.directory.join("known-failure-build.log"))?;
        let mut text = String::new();
        let env = self.environment.clone();
        let code = self.command(&argv, &env, &mut sink, &mut text, CLASSIFIER_BUILD_BOUND)?;
        match target.filter(|b| code == 0 && b.exists()) {
            Some(binary) => Ok(binary),
            None => Err(fail("rsi-known-failure binary missing after build")),
        }
    }
}

/// The legacy shard-lane command after the slot: the full QA is where the slow
/// tier (released-migration history tests, #2270) runs.
pub fn shard_argv(name: &str, evidence: &Path, jobs: u32, slow_tier: bool) -> Vec<String> {
    let mut argv: Vec<String> = [
        "scripts/run-rsid-test-shards.sh",
        "shard",
        name,
        "--jobs",
        &jobs.to_string(),
        "--evidence-dir",
        &evidence.to_string_lossy(),
    ]
    .map(str::to_owned)
    .into();
    if slow_tier {
        argv.push("--slow-tier".into());
    }
    argv
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is writable for its full length.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
            return String::from_utf8_lossy(&buffer[..end]).into_owned();
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

impl Services for Host<'_> {
    fn fetch(&mut self) -> io::Result<()> {
        let done = self.git().plain(&["fetch", "origin"])?;
        if done.timed_out || !done.ok() {
            return Err(fail(format!(
                "git fetch origin failed: {}",
                done.stderr.trim()
            )));
        }
        Ok(())
    }
    fn rev_parse(&mut self, revision: &str) -> io::Result<String> {
        let done = self
            .git()
            .builtin(&["rev-parse", "--verify", revision], &[])?;
        let id = done.first_line().to_owned();
        if !done.ok() || !qa_fingerprint::lowercase_hex(&id, 40) {
            return Err(fail(format!("cannot resolve {revision}")));
        }
        Ok(id)
    }
    fn tracked_dirty(&mut self) -> io::Result<bool> {
        for args in [&["diff", "--quiet"][..], &["diff", "--cached", "--quiet"]] {
            let done = self.git().builtin(args, &[])?;
            if done.timed_out || !done.ok() {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn fingerprint(&mut self, tip: &str, shard: &str, jobs: u32) -> io::Result<Value> {
        qa_fingerprint::fingerprint(&self.ctx, tip, shard, jobs).map_err(fail)
    }
    fn shard_names(&mut self) -> io::Result<Vec<String>> {
        rsid_test_shards::read_shard_packages(&self.repo)
            .map(|shards| shards.into_iter().map(|s| s.name).collect())
            .map_err(fail)
    }
    fn integration_targets(&mut self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(self.repo.join("crates/rsid/tests"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "rs")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                names.push(stem.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }
    fn signature_matches(&mut self, rows: &[Value]) -> Value {
        let mut found = serde_json::Map::new();
        let result = (|| -> io::Result<()> {
            let binary = self.classifier()?;
            for row in rows {
                if row["failures"].as_array().is_none_or(Vec::is_empty) {
                    continue;
                }
                let log = self.repo.join(row["log"].as_str().unwrap_or_default());
                if !log.exists() {
                    continue;
                }
                let argv = vec![
                    binary.to_string_lossy().into_owned(),
                    "classify".into(),
                    "--log".into(),
                    log.to_string_lossy().into_owned(),
                ];
                let (mut text, mut errors) = (String::new(), String::new());
                let done = process::stream_channels(
                    &argv,
                    &self.repo,
                    &self.environment,
                    Instant::now() + RPC_BOUND,
                    &self.cancel,
                    |pipe, line| {
                        if pipe == 0 && text.len() < RPC_OUTPUT_LIMIT {
                            text.push_str(line);
                        } else if errors.len() < 4096 {
                            errors.push_str(line);
                        }
                    },
                    |_, _| {},
                )?;
                if done.timed_out || done.cancelled.is_some() || done.code != 0 {
                    return Err(fail(format!("classifier exit {}", done.code)));
                }
                let lines: Vec<String> = text.lines().map(str::to_owned).collect();
                let matches = rolling_qa::parse_signature_matches(&lines);
                if matches.as_object().is_some_and(|m| !m.is_empty())
                    && let Some(label) = row["label"].as_str()
                {
                    found.insert(label.to_owned(), matches);
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => Value::Object(found),
            Err(error) => {
                self.warn(&format!("known_failures=unavailable: {error}"));
                json!({})
            }
        }
    }
    fn closed_issues(&mut self, since: Option<&str>) -> io::Result<Value> {
        let cutoff = since
            .map(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .map_err(|e| fail(format!("--closed-since is not RFC3339: {e}")))
            })
            .transpose()?;
        let mut cursor: Option<String> = None;
        let mut issues = Vec::new();
        loop {
            let mut params = json!({"status":"Closed","archive":"All","limit":256});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let page = self.rpc("AgentListIssues", &params)?;
            for row in page["issues"].as_array().into_iter().flatten() {
                let Some(closed_at) = row["closed_at"].as_str() else {
                    continue;
                };
                let closed = chrono::DateTime::parse_from_rfc3339(closed_at)
                    .map_err(|e| fail(format!("issue closed_at is not RFC3339: {e}")))?;
                if cutoff.is_some_and(|cutoff| closed <= cutoff) {
                    continue;
                }
                let detail = self.rpc("AgentGetIssue", &json!({"issue_id": row["id"]}))?;
                issues.push(json!({"id":row["id"],"number":row["display_number"],
                    "title":row["title"],"closed_at":closed_at,"body":detail["body"]}));
            }
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                return Ok(Value::Array(issues));
            }
        }
    }
    fn host_name(&self) -> String {
        hostname()
    }
    fn now(&self) -> String {
        timestamp()
    }
    fn say(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }
    fn warn(&mut self, text: &str) {
        let _ = writeln!(self.err, "{text}");
    }
}

impl Backend for Host<'_> {
    fn verify_source(&mut self, state: &State) -> io::Result<()> {
        if self.rev_parse("HEAD^{tree}")? != state.tree {
            return Err(fail(
                "HEAD tree differs from the recorded sweep; the QA source moved",
            ));
        }
        if self.tracked_dirty()? {
            return Err(fail(
                "tracked worktree changes would contaminate the QA sweep",
            ));
        }
        Ok(())
    }

    fn run_lane(&mut self, lane: &Lane, jobs: u32) -> io::Result<Value> {
        let label = lane.label().to_owned();
        let log_path = self.directory.join(format!("{label}.log"));
        let log_relative = relative(&self.repo, &log_path)?;
        let mut log = fs::File::create(&log_path)?;
        let mut output = String::new();
        let started = Instant::now();
        let shard = matches!(lane, Lane::Shard { .. });
        let mut environment = rolling_qa::lane_environment(&self.environment, shard);
        let code = match lane {
            Lane::Static => {
                let root = self.repo.to_string_lossy().into_owned();
                let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
                let code = rsid_test_shards::main(
                    &["--require-gates".into(), "--root".into(), root],
                    &self.ctx,
                    &mut stdout,
                    &mut stderr,
                );
                output = format!(
                    "{}{}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
                log.write_all(output.as_bytes())?;
                self.say(&format!(
                    "running {label}: check-rsid-test-shards --require-gates"
                ));
                let _ = self.out.write_all(output.as_bytes());
                code
            }
            lane => {
                let slot = self.slot()?.to_string_lossy().into_owned();
                let mut argv = vec![slot];
                if let Lane::Shard { name, slow_tier } = lane {
                    self.shard_script_is_python_free()?;
                    let mut shard_dir = self.directory.join(name);
                    if shard_dir.exists() {
                        self.attempts += 1;
                        shard_dir = self.directory.join(format!(
                            "{name}-retry-{}-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(0, |d| d.as_nanos()),
                            self.attempts
                        ));
                    }
                    argv.extend(shard_argv(name, &shard_dir, jobs, *slow_tier));
                } else {
                    argv.extend(
                        lane.cargo_command(jobs)
                            .ok_or_else(|| fail("QA lane has no command"))?,
                    );
                }
                let text = argv.join(" ");
                self.say(&format!("running {label}: {text}"));
                environment.remove("RSI_SESSION_TOKEN");
                environment.remove("RSI_PROCESS_OWNERSHIP_NAMESPACE");
                self.command(&argv, &environment, &mut log, &mut output, LANE_BOUND)?
            }
        };
        let environment = if shard {
            let scratch = self
                .environment
                .get("TMPDIR")
                .filter(|t| !t.is_empty())
                .cloned()
                .unwrap_or_else(|| "/tmp".to_owned());
            Some(rolling_qa::shard_environment(rolling_qa::tmpdir_class(
                Path::new(&scratch),
            )?)?)
        } else {
            None
        };
        let row = rolling_qa::result(
            &label,
            code,
            started.elapsed().as_secs_f64(),
            &log_relative,
            &output,
            environment,
        )?;
        let failures = row["failures"].as_array().map_or(0, Vec::len);
        self.say(&format!(
            "finished {label}: exit={code} failures={failures}"
        ));
        Ok(row)
    }
}

impl Host<'_> {
    /// `scripts/run-rsid-test-shards.sh` is #362's caller; this lane never runs a
    /// script that still invokes Python (operator directive #358).
    fn shard_script_is_python_free(&self) -> io::Result<()> {
        let script = self.repo.join("scripts/run-rsid-test-shards.sh");
        let text = fs::read_to_string(&script)?;
        if text.to_ascii_lowercase().contains("python") {
            let mut incomplete: Incomplete = gap(
                "#362",
                "scripts/run-rsid-test-shards.sh still invokes Python",
            );
            incomplete.detail = Some(format!(
                "{} calls python3 for shard listing, runtime checks or the target directory; convert it to the Rust shard commands (Issue #448) before the QA shard lanes can run",
                script.display()
            ));
            return Err(incomplete.error());
        }
        Ok(())
    }
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let args = match parse_args(args, err) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let cancel = match process::install_signals() {
        Ok(cancel) => cancel,
        Err(error) => {
            let _ = writeln!(err, "run-rolling-qa: {error}");
            return 1;
        }
    };
    let runner = std::env::current_exe().map_or_else(
        |_| "rsi-repo-guard".to_owned(),
        |p| p.to_string_lossy().into_owned(),
    );
    run(&args, ctx, runner, cancel, out, err)
}

pub fn run(
    args: &Args,
    ctx: &Ctx,
    runner: String,
    cancel: Arc<AtomicUsize>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let outcome = (|| -> io::Result<i32> {
        let environment: BTreeMap<String, String> = ctx
            .vars()
            .filter(|(k, _)| !LOCATION_VARIABLES.contains(k))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let probe = Git {
            repo: &ctx.cwd,
            env: &environment,
            cancel: &cancel,
        };
        let top = probe.builtin(&["rev-parse", "--show-toplevel"], &[])?;
        if !top.ok() {
            return Err(fail("not inside a Git repository"));
        }
        let repo = PathBuf::from(top.first_line()).canonicalize()?;
        let directory = evidence_directory(&repo, &ctx.path(&args.evidence_dir))?;
        let mut host = Host::new(
            ctx,
            repo.clone(),
            directory.clone(),
            runner,
            cancel,
            out,
            err,
        );
        orchestrate(args, &repo, &directory, &mut host)
    })();
    match outcome {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "run-rolling-qa: {error}");
            if error.kind() == io::ErrorKind::Interrupted {
                130
            } else {
                1
            }
        }
    }
}

#[cfg(test)]
#[path = "rolling_qa_cli_tests.rs"]
mod tests;
