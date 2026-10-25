//! Staged retained desktop executor (#363). This is a library, not a canonical
//! command: full runner/receipt integration and remaining acceptance tests must
//! finish before callers switch. Legacy scripts are reference text only.
use crate::pid_isolation::Isolation;
use crate::re::Re;
use crate::scoped_runner::process;
use crate::scratch_git::{LOCATION_VARIABLES, Policy};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub type Environment = BTreeMap<String, String>;
pub const LOCK_FIX: &str = "refresh desktop/src-tauri/Cargo.lock: run `cargo update --workspace --manifest-path desktop/src-tauri/Cargo.toml` and commit the refreshed lock with the dependency change";

/// Read-only candidate adapter. Git location overrides cannot select another
/// checkout. Binary filenames and NUL separators survive the bounded transport.
pub fn candidate_files(repo: &Path, base: &Environment) -> io::Result<Vec<PathBuf>> {
    candidate_files_with(repo, base, &AtomicUsize::new(0))
}

fn check_cancel(cancel: &AtomicUsize) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) == 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "desktop lane cancelled",
        ))
    }
}

fn capture(
    argv: &[String],
    cwd: &Path,
    env: &Environment,
    timeout: Duration,
    cancel: &AtomicUsize,
) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
    check_cancel(cancel)?;
    let mut streams = [Vec::new(), Vec::new()];
    let mut overflow = false;
    let done = process::stream_bytes(
        argv,
        cwd,
        env,
        Instant::now() + timeout,
        cancel,
        |index, bytes| {
            if streams[index].len().saturating_add(bytes.len()) <= 8 * 1024 * 1024 {
                streams[index].extend_from_slice(bytes);
            } else {
                overflow = true;
            }
        },
    )?;
    check_cancel(cancel)?;
    if done.timed_out {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "desktop metadata/preflight command timed out",
        ));
    }
    if done.cancelled.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "desktop metadata/preflight command cancelled",
        ));
    }
    if !done.output_complete || overflow {
        return Err(io::Error::other(
            "incomplete or oversized desktop metadata/preflight output",
        ));
    }
    let [stdout, stderr] = streams;
    Ok((done.code, stdout, stderr))
}

fn candidate_files_with(
    repo: &Path,
    base: &Environment,
    cancel: &AtomicUsize,
) -> io::Result<Vec<PathBuf>> {
    check_cancel(cancel)?;
    let repo = repo.canonicalize()?;
    let mut environment = base.clone();
    for key in LOCATION_VARIABLES {
        environment.remove(*key);
    }
    if let Some(parent) = repo.parent() {
        environment.insert(
            "GIT_CEILING_DIRECTORIES".into(),
            parent.to_string_lossy().into_owned(),
        );
    }
    let repo_text = repo
        .to_str()
        .ok_or_else(|| io::Error::other("desktop repository root must be UTF-8"))?;
    let command = [
        "git",
        "-C",
        repo_text,
        "-c",
        "core.fsmonitor=false",
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ]
    .map(str::to_owned);
    match capture(
        &command,
        &repo,
        &environment,
        Duration::from_secs(30),
        cancel,
    ) {
        Ok((0, stdout, _)) => return candidate_paths(&stdout),
        Ok(_) => (), // A directory without a Git repository is walked.
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error), // Timeout, cancellation or lost output never falls back.
    }
    fn walk(
        root: &Path,
        relative: &Path,
        files: &mut Vec<PathBuf>,
        cancel: &AtomicUsize,
    ) -> io::Result<()> {
        for entry in fs::read_dir(root.join(relative))? {
            check_cancel(cancel)?;
            let entry = entry?;
            let path = relative.join(entry.file_name());
            if left_out(&path) {
                continue;
            }
            // Directory symlinks are entries, never traversed.
            if entry.file_type()?.is_dir() {
                walk(root, &path, files, cancel)?;
            } else {
                files.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(&repo, Path::new(""), &mut files, cancel)?;
    files.sort();
    Ok(files)
}

fn candidate_paths(stdout: &[u8]) -> io::Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    for bytes in stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStrExt;
            PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
        };
        #[cfg(not(unix))]
        let path = PathBuf::from(std::str::from_utf8(bytes).map_err(io::Error::other)?);
        safe_relative(&path)?;
        files.insert(path);
    }
    Ok(files.into_iter().collect())
}

fn safe_relative(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        Err(io::Error::other(
            "desktop candidate path must be a nonempty repository-relative path",
        ))
    } else {
        Ok(())
    }
}

pub fn left_out(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("target" | ".git" | "node_modules")
        )
    }) || path.starts_with("desktop/dist")
        || path.starts_with("desktop/src-tauri/gen")
}

/// Copy the complete candidate, including workspace manifests, unrelated crates
/// and compile-time includes. The destination must be a fresh, empty sibling
/// or external directory. Do not follow symlink parents while writing it.
pub fn stage_tree(repo: &Path, stage: &Path, files: &[PathBuf]) -> io::Result<()> {
    stage_tree_with_cancel(repo, stage, files, &AtomicUsize::new(0))
}

/// Production staging shares the parent's signal flag, including while copying
/// large assets. The convenience wrapper above is for isolated callers/tests.
pub fn stage_tree_with_cancel(
    repo: &Path,
    stage: &Path,
    files: &[PathBuf],
    cancel: &AtomicUsize,
) -> io::Result<()> {
    check_cancel(cancel)?;
    let repo = repo.canonicalize()?;
    let stage = stage.canonicalize()?;
    if stage.starts_with(&repo)
        || repo.starts_with(&stage)
        || fs::read_dir(&stage)?.next().is_some()
    {
        return Err(io::Error::other(
            "desktop stage must be empty and outside the candidate tree",
        ));
    }
    for relative in files.iter().collect::<BTreeSet<_>>() {
        check_cancel(cancel)?;
        safe_relative(relative)?;
        if left_out(relative) {
            continue;
        }
        let source = repo.join(relative);
        let metadata = match fs::symlink_metadata(&source) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !(metadata.is_file() || metadata.file_type().is_symlink()) {
            continue;
        }
        let parent = relative.parent().unwrap_or(Path::new(""));
        let mut source_parent = repo.clone();
        let mut stage_parent = stage.clone();
        for component in parent.components() {
            check_cancel(cancel)?;
            source_parent.push(component);
            stage_parent.push(component);
            if fs::symlink_metadata(&source_parent)?
                .file_type()
                .is_symlink()
            {
                return Err(io::Error::other(
                    "desktop candidate file has a symlink parent",
                ));
            }
            match fs::create_dir(&stage_parent) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if !fs::symlink_metadata(&stage_parent)?.is_dir() {
                        return Err(io::Error::other("desktop stage parent is not a directory"));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let target = stage.join(relative);
        if metadata.file_type().is_symlink() {
            let link = fs::read_link(&source)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, target)?;
            #[cfg(windows)]
            if source.is_dir() {
                std::os::windows::fs::symlink_dir(link, target)?;
            } else {
                std::os::windows::fs::symlink_file(link, target)?;
            }
            #[cfg(not(any(unix, windows)))]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "desktop staging symlinks unsupported",
            ));
        } else {
            let mut input = fs::File::open(source)?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)?;
            copy_with_cancel(&mut input, &mut output, cancel)?;
            output.set_permissions(metadata.permissions())?;
        }
    }
    check_cancel(cancel)
}

fn copy_with_cancel(
    input: &mut impl Read,
    output: &mut impl Write,
    cancel: &AtomicUsize,
) -> io::Result<()> {
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let count = input.read(&mut buffer)?;
        check_cancel(cancel)?;
        if count == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..count])?;
    }
}

pub fn lane_env(base: &Environment, scratch: &Path) -> Environment {
    let mut env = base.clone();
    let cache = base
        .get("DESKTOP_LANE_NPM_CACHE")
        .filter(|v| !v.is_empty())
        .cloned()
        .unwrap_or_else(|| scratch.join("npm-cache").to_string_lossy().into_owned());
    let target = base
        .get("DESKTOP_LANE_CARGO_TARGET")
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| {
            base.get("CARGO_TARGET_DIR")
                .filter(|v| !v.is_empty())
                .map(|v| {
                    Path::new(v)
                        .join("desktop-tauri")
                        .to_string_lossy()
                        .into_owned()
                })
        })
        .unwrap_or_else(|| scratch.join("cargo-target").to_string_lossy().into_owned());
    for (key, value) in [
        ("npm_config_cache", cache),
        ("CARGO_TARGET_DIR", target),
        ("npm_config_audit", "false".into()),
        ("npm_config_fund", "false".into()),
        ("npm_config_update_notifier", "false".into()),
    ] {
        env.insert(key.into(), value);
    }
    env.entry("CARGO_INCREMENTAL".into())
        .or_insert_with(|| "0".into());
    env.remove("RSI_PROCESS_OWNERSHIP_NAMESPACE");
    #[cfg(unix)]
    env.insert("DESKTOP_LANE_PROCESS_GROUP".into(), "1".into());
    env
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Install,
    Frontend,
    Cargo,
}
impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Self::Install => "npm ci",
            Self::Frontend => "npm test",
            Self::Cargo => "cargo test",
        }
    }
    pub fn command(self) -> Vec<String> {
        match self {
            Self::Install => vec!["npm", "ci"],
            Self::Frontend => vec!["npm", "test"],
            Self::Cargo => vec!["cargo", "test", "--locked"],
        }
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
    pub fn timeout(self, env: &Environment) -> io::Result<Duration> {
        let (variable, default) = match self {
            Self::Install => ("DESKTOP_LANE_NPM_CI_TIMEOUT_SECONDS", 900.0),
            Self::Frontend => ("DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS", 600.0),
            Self::Cargo => ("DESKTOP_LANE_CARGO_TEST_TIMEOUT_SECONDS", 1800.0),
        };
        let seconds = match env.get(variable) {
            Some(v) => v.parse::<f64>().unwrap_or(f64::NAN),
            None => default,
        };
        Duration::try_from_secs_f64(seconds)
            .ok()
            .filter(|v| !v.is_zero())
            .filter(|v| Instant::now().checked_add(*v).is_some())
            .ok_or_else(|| io::Error::other(format!("{variable} must be positive finite seconds")))
    }
}

#[derive(Debug)]
pub struct StepOutput {
    pub code: i32,
    pub output: String,
    pub timed_out: bool,
    pub cancelled: Option<usize>,
    pub output_complete: bool,
}

/// Shared #423 process ownership supplies deadline checks, cancellation and
/// output drain bounds. Caller must first apply #423's canonical PID wrapper to
/// argv; this function never implements an independent isolation probe.
pub fn run_step(
    argv: &[String],
    cwd: &Path,
    env: &Environment,
    step: Step,
    cancel: &AtomicUsize,
    emit: impl FnMut(&str),
) -> io::Result<StepOutput> {
    run_step_until(argv, cwd, env, step, cancel, None, emit)
}

/// `run_step` under an optional overall run deadline: the step's own bound
/// still applies, and the run's remaining budget can only shorten it.
pub fn run_step_until(
    argv: &[String],
    cwd: &Path,
    env: &Environment,
    step: Step,
    cancel: &AtomicUsize,
    limit: Option<Instant>,
    mut emit: impl FnMut(&str),
) -> io::Result<StepOutput> {
    let mut deadline = Instant::now()
        .checked_add(step.timeout(env)?)
        .ok_or_else(|| io::Error::other("desktop step deadline overflow"))?;
    if let Some(limit) = limit {
        deadline = deadline.min(limit);
    }
    let mut output = String::new();
    let done = process::stream(
        argv,
        cwd,
        env,
        deadline,
        cancel,
        |line| {
            emit(line);
            output.push_str(line);
        },
        |_| {},
    )?;
    Ok(StepOutput {
        code: done.code,
        output,
        timed_out: done.timed_out,
        cancelled: done.cancelled,
        output_complete: done.output_complete,
    })
}

fn unavailable(runner: &str, reason: &str) -> Value {
    json!({"status":"runner_unavailable", "lane":"desktop", "runner":runner, "reason":reason, "executed_tests":0})
}

/// Probes are injected so unavailable runners never get substituted. Linux's
/// WebKit probe must itself be bounded to 30 seconds by the supplied adapter.
pub fn runner_status(
    platform: &str,
    mut which: impl FnMut(&str) -> bool,
    mut webkit_probe: impl FnMut() -> io::Result<i32>,
) -> Option<Value> {
    for tool in ["node", "npm", "cargo"] {
        if !which(tool) {
            return Some(unavailable(
                tool,
                &format!("{tool} is not installed (the desktop lane needs node, npm and cargo)"),
            ));
        }
    }
    if !platform.starts_with("linux") {
        return None;
    }
    if !which("pkg-config") {
        return Some(unavailable(
            "pkg-config",
            "pkg-config is not installed (needed to find webkit2gtk-4.1)",
        ));
    }
    match webkit_probe() {
        Ok(0) => None,
        Ok(_) => Some(unavailable(
            "webkit2gtk-4.1",
            "pkg-config --exists webkit2gtk-4.1 failed (the Tauri cargo tests need the webkit2gtk-4.1 development files)",
        )),
        Err(error) => Some(unavailable(
            "webkit2gtk-4.1",
            &format!("pkg-config cannot run: {error}"),
        )),
    }
}

fn count_node(output: &str) -> u64 {
    static PATTERN: LazyLock<Re> = LazyLock::new(|| Re::new(r"^(?:# |ℹ )tests (\d+)\s*$"));
    output
        .lines()
        .filter_map(|line| {
            let found = PATTERN.search(line)?;
            found.get(1)?.as_str().parse::<u64>().ok()
        })
        .fold(0, u64::saturating_add)
}
fn count_cargo(output: &str) -> u64 {
    static PATTERN: LazyLock<Re> = LazyLock::new(|| Re::new(r"^test result: .*?(\d+) passed;"));
    output
        .lines()
        .filter_map(|line| {
            let found = PATTERN.search(line)?;
            found.get(1)?.as_str().parse::<u64>().ok()
        })
        .fold(0, u64::saturating_add)
}
fn stale_lock(output: &str) -> bool {
    output.lines().any(|line| {
        let diagnostic = line.contains("cannot update the lock file")
            || (line.contains("lock file ") && line.contains(" needs to be updated"));
        diagnostic
            && (line.contains("because --locked was passed")
                || line.contains("but --locked was passed"))
    })
}

/// Both test sets are mandatory, in order. The backend owns command wrapping
/// and preflight; no default implementation can silently bypass either. The
/// private stage is removed on all normal/error/unwind paths.
pub trait Backend {
    fn preflight(&mut self) -> io::Result<Option<Value>>;
    fn step(&mut self, step: Step, cwd: &Path, env: &Environment) -> io::Result<StepOutput>;
    /// Called once with the lane environment, before the first step (progress text only).
    fn announce(&mut self, _env: &Environment) {}
}

/// A production adapter using the caller's once-per-run PID probe. It never
/// probes or holds admission itself; the parent keeps the admission holder on
/// the host and passes the same immutable Isolation to every test lane.
pub struct ProcessBackend<'a> {
    pub environment: &'a Environment,
    pub repo: &'a Path,
    pub isolation: &'a Isolation,
    pub cancel: &'a AtomicUsize,
    pub emit: &'a mut dyn FnMut(&str),
    /// The run's remaining wall-clock budget, if it has one (a step never outlives it).
    pub deadline: Option<Instant>,
}

pub(crate) fn executable(name: &str, env: &Environment, cwd: &Path) -> Option<PathBuf> {
    let extensions: Vec<String> = if cfg!(windows) {
        std::iter::once(String::new())
            .chain(
                env.get("PATHEXT")
                    .map_or(".COM;.EXE;.BAT;.CMD", String::as_str)
                    .split(';')
                    .map(str::to_owned),
            )
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(env.get("PATH").map_or("", String::as_str)).find_map(|directory| {
        extensions.iter().find_map(|extension| {
            let path = cwd.join(&directory).join(format!("{name}{extension}"));
            let metadata = fs::metadata(&path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    return None;
                }
            }
            Some(path)
        })
    })
}

impl Backend for ProcessBackend<'_> {
    fn announce(&mut self, env: &Environment) {
        (self.emit)(&format!(
            "desktop lane: npm cache {}; cargo target {}\n",
            env.get("npm_config_cache").map_or("", String::as_str),
            env.get("CARGO_TARGET_DIR").map_or("", String::as_str)
        ));
    }
    fn preflight(&mut self) -> io::Result<Option<Value>> {
        check_cancel(self.cancel)?;
        Ok(runner_status(
            std::env::consts::OS,
            |name| executable(name, self.environment, self.repo).is_some(),
            || {
                let command = vec![
                    executable("pkg-config", self.environment, self.repo)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::NotFound, "pkg-config disappeared")
                        })?
                        .to_string_lossy()
                        .into_owned(),
                    "--exists".into(),
                    "webkit2gtk-4.1".into(),
                ];
                capture(
                    &command,
                    self.repo,
                    self.environment,
                    Duration::from_secs(30),
                    self.cancel,
                )
                .map(|v| v.0)
            },
        ))
    }
    fn step(&mut self, step: Step, cwd: &Path, env: &Environment) -> io::Result<StepOutput> {
        check_cancel(self.cancel)?;
        let mut command = step.command();
        command[0] = executable(&command[0], env, self.repo)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} disappeared after desktop preflight", command[0]),
                )
            })?
            .to_string_lossy()
            .into_owned();
        (self.emit)(&format!(
            "desktop lane: {}: {}\n",
            step.name(),
            command.join(" ")
        ));
        run_step_until(
            &self.isolation.wrap(&command),
            cwd,
            env,
            step,
            self.cancel,
            self.deadline,
            &mut self.emit,
        )
    }
}

pub fn run_lane(
    repo: &Path,
    base: &Environment,
    scratch_parent: &Path,
    backend: &mut impl Backend,
) -> io::Result<(i32, Value)> {
    run_lane_with_cancel(repo, base, scratch_parent, &AtomicUsize::new(0), backend)
}

pub fn run_lane_with_cancel(
    repo: &Path,
    base: &Environment,
    scratch_parent: &Path,
    cancel: &AtomicUsize,
    backend: &mut impl Backend,
) -> io::Result<(i32, Value)> {
    check_cancel(cancel)?;
    if let Some(missing) = backend.preflight()? {
        return Ok((1, missing));
    }
    let policy = Policy::new(
        &repo.canonicalize()?,
        &std::env::current_dir()?,
        base.iter().map(|(k, v)| (k.into(), v.into())).collect(),
        &[],
    )?;
    let scratch = policy.tempdir_in("rsi-desktop-lane-", scratch_parent)?;
    stage_tree_with_cancel(
        repo,
        scratch.path(),
        &candidate_files_with(repo, base, cancel)?,
        cancel,
    )?;
    let env = lane_env(base, scratch.path());
    backend.announce(&env);
    let mut result = json!({"status":"failed", "lane":"desktop", "runner":"npm+cargo", "npm_tests":0, "cargo_tests":0, "executed_tests":0});
    for step in [Step::Install, Step::Frontend, Step::Cargo] {
        check_cancel(cancel)?;
        // Validate even with an injected backend; invalid input never disables a deadline.
        let timeout = step.timeout(&env)?;
        let cwd = if step == Step::Cargo {
            scratch.path().join("desktop/src-tauri")
        } else {
            scratch.path().join("desktop")
        };
        let done = backend.step(step, &cwd, &env)?;
        match step {
            Step::Frontend => result["npm_tests"] = count_node(&done.output).into(),
            Step::Cargo => result["cargo_tests"] = count_cargo(&done.output).into(),
            Step::Install => (),
        }
        if done.timed_out || done.cancelled.is_some() || !done.output_complete || done.code != 0 {
            result["failed_step"] = step.name().into();
            result["exit_code"] = done.code.into();
            if let Some(signal) = done.cancelled {
                result["status"] = "cancelled".into();
                result["signal"] = signal.into();
            } else if done.timed_out {
                result["status"] = "desktop_lane_timed_out".into();
                result["timeout_seconds"] = json!(timeout.as_secs_f64());
            } else if !done.output_complete {
                result["status"] = "output_incomplete".into();
            } else if step == Step::Cargo && stale_lock(&done.output) {
                result["status"] = "desktop_lock_stale".into();
                result["reason"] = LOCK_FIX.into();
            }
            // Keep the legacy ordinary-failure count shape; typed timeout and
            // stale-lock results include the tests observed before interruption.
            if result["status"] != "failed" {
                result["executed_tests"] = total(&result).into();
            }
            return Ok((1, result));
        }
    }
    result["executed_tests"] = total(&result).into();
    let passed = result["npm_tests"].as_u64().unwrap_or(0) > 0
        && result["cargo_tests"].as_u64().unwrap_or(0) > 0;
    result["status"] = if passed {
        "passed"
    } else {
        "no_tests_executed"
    }
    .into();
    Ok((i32::from(!passed), result))
}
fn total(result: &Value) -> u64 {
    result["npm_tests"]
        .as_u64()
        .unwrap_or(0)
        .saturating_add(result["cargo_tests"].as_u64().unwrap_or(0))
}

/// Caller inputs the lane honors: the three step bounds and the trusted npm
/// cache / Cargo target overrides. The scoped runner forwards exactly these on
/// top of its build environment; every other variable stays out of the lane.
pub const LANE_ENV_NAMES: [&str; 5] = [
    "DESKTOP_LANE_NPM_CI_TIMEOUT_SECONDS",
    "DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS",
    "DESKTOP_LANE_CARGO_TEST_TIMEOUT_SECONDS",
    "DESKTOP_LANE_NPM_CACHE",
    "DESKTOP_LANE_CARGO_TARGET",
];
pub fn forward_lane_inputs<'a>(
    var: impl Fn(&str) -> Option<&'a str>,
    environment: &mut Environment,
) {
    for name in LANE_ENV_NAMES {
        if let Some(value) = var(name) {
            environment.insert(name.into(), value.into());
        }
    }
}

/// The scoped-test receipt entry for one lane result: the legacy fields
/// (`completed`, `lane_status`) around the lane's own typed verdict. Only a
/// `passed` verdict with a zero exit is `passed`; a step or budget timeout is
/// `timed_out` (what the runner's early exit keys on); every other typed
/// verdict (lock stale, cancelled, incomplete output) is `failed` and stays
/// readable in `lane_status`. `runner_unavailable` and `no_tests_executed`
/// keep their names, so nothing is mistaken for a green lane.
pub fn receipt_entry(mut result: Value, code: i32) -> Value {
    let lane_status = result["status"].as_str().unwrap_or("failed").to_owned();
    result["lane_status"] = json!(lane_status);
    result["completed"] = json!(!matches!(
        lane_status.as_str(),
        "runner_unavailable" | "cancelled"
    ));
    result["status"] = json!(match (lane_status.as_str(), code) {
        ("passed", 0) => "passed",
        ("desktop_lane_timed_out", _) => "timed_out",
        ("runner_unavailable", _) => "runner_unavailable",
        ("no_tests_executed", _) => "no_tests_executed",
        _ => "failed",
    });
    result
}

#[cfg(test)]
#[path = "desktop_execution_tests.rs"]
mod tests;
