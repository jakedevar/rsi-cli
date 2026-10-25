//! #453: `target-pool build` (the detached pool build), the `--build-missing` /
//! `refresh` request that starts it, and `seed-from` (a peer's pool over ssh) in
//! Rust. Pool markers, logs, the disk-floor events and every printed line are
//! byte-compatible with `scripts/target-pool`.
//!
//! The build runs `cargo test --workspace --no-run` in its own detached worktree
//! (through `cargo-slot` when one is installed and not already held), seeds the
//! worktree's target from the nearest ready ancestor pool first, and stops, removes
//! its partial output and reports `pool_aborted_disk_floor` the moment the free
//! space crosses the workers' floor. A tip whose tests do not compile still gets a
//! `degraded` pool from `--keep-going` plus the libraries and binaries.
use crate::target_pool::{
    BUILD_COMMAND, DEFAULT_MAX_DISTANCE, Env, Git, KEY_ENV_EXACT, KEY_ENV_PREFIX, build_key,
    choose_pool, claim_pool, dumps, normalise_mtimes, ready_pools, remember_repo, repo_dir,
    seed_tree, wrapper_unlinks_outputs,
};
use crate::target_pool_ops::{
    ABORT_FLOOR, GcOptions, SKIP_HEADROOM, admission_skip, build_lock, default_keep,
    default_min_free_gib, fields, free_gib, gc, now, positive_env, record_event, remove_pool,
    round1, scaled_floor_gib, tree_size, use_lock, worker_floor_gib, write_marker,
};
use serde_json::{Map, Value, json};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// What a tip whose tests do not compile still gets (see `degraded` pools): every
/// test target that does compile, then the libraries and binaries.
pub const KEEP_GOING_COMMAND: [&str; 5] =
    ["cargo", "test", "--workspace", "--no-run", "--keep-going"];
pub const LIBRARY_COMMAND: [&str; 3] = ["cargo", "build", "--workspace"];
const DEFAULT_WATCH_SECONDS: f64 = 5.0;
/// Outputs the linker or rustc writes in place. `.o` is one on macOS, where rustc keeps a
/// unit's object files for the debug map: the Python tool left them writable, so a build
/// without the wrapper over a pool seeded behind it rewrote the pool's shared objects (#453).
const IN_PLACE_SUFFIXES: [&str; 7] = [".dwo", ".so", ".dylib", ".dll", ".exe", ".pdb", ".o"];
const GIB: f64 = 1_073_741_824.0;
/// #2715: the detached pool build is a cargo build the harness starts on its own, so it
/// runs inside the hub build budget like every build job (#2119), with a CPU cap and
/// a bounded runtime.
pub const BUILD_SLICE: &str = "rsi-build.slice";
const DEFAULT_POOL_CPU_PERCENT: f64 = 400.0;
const DEFAULT_POOL_MAX_SEC: f64 = 7200.0;

// ---- the build slice and the detached request ----

/// The hub build slice; `RSI_BUILD_SLICE=` (empty) opts out, as for build jobs.
pub fn build_slice(env: &Env) -> String {
    env.get("RSI_BUILD_SLICE")
        .cloned()
        .unwrap_or_else(|| BUILD_SLICE.to_string())
}

/// The build slice's live CPUQuota in percent of one CPU (the operator's
/// `hub_build_cpu_percent`), or `None` when it is unknown or the slice has no quota.
pub fn slice_budget_percent(env: &Env, slice: &str) -> Option<f64> {
    if slice.is_empty() {
        return None;
    }
    let output = run_bounded(
        Command::new("systemctl")
            .args([
                "--user",
                "show",
                slice,
                "-p",
                "CPUQuotaPerSecUSec",
                "--value",
            ])
            .env_clear()
            .envs(env),
        Duration::from_secs(10),
    )?;
    if !output.0 {
        return None;
    }
    parse_quota_seconds(output.1.trim()).map(|seconds| seconds * 100.0)
}

/// `(\d+(?:\.\d+)?)(s|ms|us|min)?` in full, as seconds of CPU per second.
fn parse_quota_seconds(text: &str) -> Option<f64> {
    let digits_end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits_end);
    let mut parts = number.split('.');
    let (whole, fraction) = (parts.next()?, parts.next());
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()))
    {
        return None; // "infinity" or empty: the slice carries no quota
    }
    let scale = match unit {
        "" | "s" => 1.0,
        "ms" => 1e-3,
        "us" => 1e-6,
        "min" => 60.0,
        _ => return None,
    };
    Some(number.parse::<f64>().ok()? * scale)
}

/// (stdout, success) of a short-lived command, killed at `limit`.
fn run_bounded(command: &mut Command, limit: Duration) -> Option<(bool, String)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut text = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_string(&mut text);
                }
                return Some((status.success(), text));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// (CPUQuota percent, RuntimeMaxSec) of a pool build unit. The CPU cap is
/// `RSI_TARGET_POOL_CPU_PERCENT` (default 400), never above the build slice's own
/// budget; the runtime cap is `RSI_TARGET_POOL_MAX_SEC` (default 7200).
pub fn pool_unit_limits(env: &Env, slice: &str) -> (f64, f64) {
    let mut quota = positive_env(env, "RSI_TARGET_POOL_CPU_PERCENT", DEFAULT_POOL_CPU_PERCENT);
    if let Some(budget) = slice_budget_percent(env, slice) {
        quota = quota.min(budget.max(100.0));
    }
    (
        quota,
        positive_env(env, "RSI_TARGET_POOL_MAX_SEC", DEFAULT_POOL_MAX_SEC),
    )
}

/// Python's `f"{value:g}"` for the whole and simple fractional numbers used here.
fn g(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value:.6}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn executable_on_path(env: &Env, name: &str) -> Option<PathBuf> {
    std::env::split_paths(env.get("PATH").map_or("", String::as_str)).find_map(|dir| {
        let path = dir.join(name);
        let meta = fs::metadata(&path).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                return None;
            }
        }
        meta.is_file().then_some(path)
    })
}

fn passes_through(name: &str) -> bool {
    KEY_ENV_EXACT.contains(&name)
        || name.starts_with(KEY_ENV_PREFIX)
        || name.starts_with("SCCACHE_")
        || name.starts_with("RSI_BUILD_")
        || name.starts_with("RSI_TARGET_POOL_")
        || [
            "PATH",
            "HOME",
            "RUSTC_WRAPPER",
            "CARGO_BUILD_JOBS",
            "CARGO_HOME",
            "RUSTUP_HOME",
        ]
        .contains(&name)
}

/// Start a detached pool build for `head`; never waits for it. A build that would stop
/// at the free-space floor is not started, and the reason is reported here because the
/// detached unit's own log line is not seen by anyone. `program` is the command prefix
/// that runs `target-pool` (this binary and its subcommand name).
pub fn request_build(
    git: &Git,
    sandbox: &Path,
    head: &str,
    program: &[String],
) -> io::Result<String> {
    let env = git.env;
    let directory = repo_dir(git, env, sandbox)?;
    let key = build_key(sandbox, env)?;
    if let Some(skip) = admission_skip(
        git,
        &directory,
        sandbox,
        head,
        &key,
        default_min_free_gib(env)?,
    )? {
        record_event(&directory, "skip", Some(skip.clone()));
        return Ok(format!(
            "skipped: {}: {}",
            skip["reason"].as_str().unwrap_or_default(),
            skip["detail"].as_str().unwrap_or_default()
        ));
    }
    record_event(&directory, "skip", None);
    let mut command: Vec<String> = program.to_vec();
    command.extend([
        "build".into(),
        "--repo".into(),
        sandbox.to_string_lossy().into_owned(),
        "--sha".into(),
        head.to_string(),
    ]);
    if cfg!(target_os = "linux") && executable_on_path(env, "systemd-run").is_some() {
        let unit = format!(
            "rsi-target-pool-{}-{}",
            &head[..head.len().min(12)],
            std::process::id()
        );
        let slice = build_slice(env);
        let (cpu_quota, max_seconds) = pool_unit_limits(env, &slice);
        let mut run = Command::new("systemd-run");
        run.args(["--user", "--collect", "--quiet"])
            .arg(format!("--unit={unit}"))
            .arg("--property=Nice=19");
        // The hub build budget bounds every cargo unit (#2119), with a CPU cap and a
        // bounded runtime (#2715).
        if !slice.is_empty() {
            run.arg(format!("--slice={slice}"));
        }
        run.arg(format!("--property=CPUQuota={}%", g(cpu_quota)))
            .arg(format!("--property=RuntimeMaxSec={}", g(max_seconds)));
        // A transient service starts from the user manager's environment, not ours: the
        // launch scratch (TMPDIR and friends), the session token and slot markers stay
        // out because only the names passed here cross. systemd-run has no --unsetenv.
        for (name, value) in env.iter().filter(|(name, _)| passes_through(name)) {
            run.arg(format!("--setenv={name}={value}"));
        }
        run.arg("--").args(&command).env_clear().envs(env);
        let output = run.stdin(Stdio::null()).output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let shown: String = stderr.trim().chars().take(200).collect();
            return Ok(format!("failed: {shown}"));
        }
        return Ok(format!("started {unit}"));
    }
    // No systemd: no slice or CPU cap exists here, so the build at least runs nice'd.
    let mut detached = Command::new(&command[0]);
    detached
        .args(&command[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe calls (setsid, nice) between fork and exec.
        unsafe {
            detached.pre_exec(|| {
                libc::setsid();
                libc::nice(19);
                Ok(())
            });
        }
    }
    detached.spawn()?;
    Ok("started".to_string())
}

// ---- the build ----

pub struct DiskGuard {
    pub path: PathBuf,
    pub floor_gib: f64,
    pub interval: Duration,
}
impl DiskGuard {
    /// The free GiB when it is below the floor, else `None`.
    pub fn breach(&self) -> Option<f64> {
        let free = free_gib(&self.path).ok()?;
        (free < self.floor_gib).then_some(free)
    }
}

/// Why a build stopped early.
enum Stop {
    Abort { free: f64, floor: f64 },
    Failed(io::Error),
}
impl From<io::Error> for Stop {
    fn from(error: io::Error) -> Self {
        Stop::Failed(error)
    }
}

fn watch_seconds(env: &Env) -> f64 {
    positive_env(env, "RSI_TARGET_POOL_WATCH_SEC", DEFAULT_WATCH_SECONDS)
}

#[cfg(unix)]
fn signal_group(child: &Child, signal: i32) {
    // SAFETY: the negative pid addresses only the process group this child leads.
    unsafe {
        libc::kill(-(child.id() as i32), signal);
    }
}

/// TERM the build's process group (cargo-slot forwards it to cargo), then KILL it.
fn stop_process_group(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    {
        signal_group(child, libc::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        signal_group(child, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = grace;
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Run a build, logging to `output`. With a `guard`, the build runs in its own process
/// group and is stopped (`Stop::Abort`) as soon as the free space falls below the floor.
fn run_logged(
    command: &[String],
    cwd: &Path,
    env: &Env,
    output: &File,
    guard: Option<&DiskGuard>,
    grace: Duration,
) -> Result<i32, Stop> {
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output.try_clone()?));
    #[cfg(unix)]
    if guard.is_some() {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            writeln!(&*output, "pool build could not start: {error}")?;
            return Ok(1);
        }
    };
    let Some(guard) = guard else {
        return Ok(child.wait()?.code().unwrap_or(-1));
    };
    let mut next_check = Instant::now() + guard.interval;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().unwrap_or(-1));
        }
        if Instant::now() >= next_check {
            if let Some(free) = guard.breach() {
                stop_process_group(&mut child, grace);
                return Err(Stop::Abort {
                    free,
                    floor: guard.floor_gib,
                });
            }
            next_check = Instant::now() + guard.interval;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn log_tail(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let size = file.metadata().map_or(0, |m| m.len());
    let _ = file.seek(SeekFrom::Start(size.saturating_sub(4096)));
    let mut bytes = vec![];
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A tip whose tests do not compile still gets a pool from what compiled: the test
/// targets that build, then the libraries and binaries. `None` when even those fail
/// (a real build break, not a test-compile break).
fn degraded_build(
    wrap: &[String],
    worktree: &Path,
    env: &Env,
    output: &File,
    build_log: &Path,
    test_exit_code: i32,
    guard: Option<&DiskGuard>,
    grace: Duration,
) -> Result<Option<Value>, Stop> {
    let tail = log_tail(build_log);
    writeln!(
        &*output,
        "\ntarget-pool: tests do not compile; building what does"
    )?;
    let with = |rest: &[&str]| -> Vec<String> {
        wrap.iter()
            .cloned()
            .chain(rest.iter().map(|s| (*s).to_string()))
            .collect()
    };
    run_logged(
        &with(&KEEP_GOING_COMMAND),
        worktree,
        env,
        output,
        guard,
        grace,
    )?;
    let library_exit = run_logged(&with(&LIBRARY_COMMAND), worktree, env, output, guard, grace)?;
    if library_exit != 0 {
        return Ok(None);
    }
    Ok(Some(json!({
        "test_exit_code": test_exit_code,
        "test_error_tail": tail,
        "scope": "libraries",
    })))
}

/// Mark the shared in-place outputs read-only, so a build without the unlinking
/// wrapper fails instead of writing into a file other sandboxes share.
pub(crate) fn mark_shared_read_only(target: &Path) {
    let Ok(profiles) = fs::read_dir(target) else {
        return;
    };
    for profile in profiles.flatten() {
        let Ok(entries) = fs::read_dir(profile.path().join("deps")) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !meta.is_file() || name.ends_with(".d") {
                continue;
            }
            if IN_PLACE_SUFFIXES.iter().any(|s| name.ends_with(s)) || !name.contains('.') {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = meta.permissions().mode() & !0o222;
                    let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(mode));
                }
            }
        }
    }
}

/// Reserve space for sccache's temporary-directory suffix and socket name. macOS's
/// `sun_path` is 104 bytes (Linux's is 108); leave a byte for NUL.
fn pool_build_environment(env: &Env, target: &Path, temporary: &Path) -> io::Result<Env> {
    if temporary.as_os_str().len() + 24 >= 104 {
        return Err(io::Error::other(
            "pool scratch path is too long for sccache; set RSI_TARGET_POOL_ROOT to a shorter disk-backed path",
        ));
    }
    let mut built: Env = env
        .iter()
        .filter(|(name, _)| !crate::target_pool::GIT_CONTEXT_ENV.contains(&name.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for name in [
        "TMPDIR",
        "TMP",
        "TEMP",
        "RSI_SESSION_TOKEN",
        "RSI_CARGO_SLOT_HELD",
        "RSI_PROCESS_OWNERSHIP_NAMESPACE",
    ] {
        built.remove(name);
    }
    let temporary = temporary.to_string_lossy().into_owned();
    built.insert(
        "CARGO_TARGET_DIR".into(),
        target.to_string_lossy().into_owned(),
    );
    for name in ["TMPDIR", "TMP", "TEMP"] {
        built.insert(name.into(), temporary.clone());
    }
    built.insert("RSI_CARGO_NO_POOL_RECLAIM".into(), "1".into());
    Ok(built)
}

/// Custom pool roots can also be long. `/var/tmp` is disk-backed on the hub; use a
/// private, automatically cleaned directory there in that case.
fn pool_temporary_directory(directory: &Path) -> io::Result<tempfile::TempDir> {
    let mut parent = directory.to_path_buf();
    if cfg!(unix) && directory.as_os_str().len() + 44 >= 104 {
        parent = PathBuf::from("/var/tmp");
        if !parent.is_dir() {
            return Err(io::Error::other(
                "pool scratch needs a shorter disk-backed RSI_TARGET_POOL_ROOT",
            ));
        }
    }
    tempfile::Builder::new()
        .prefix(".rsi-tmp-")
        .tempdir_in(parent)
}

pub struct BuildOptions {
    pub repo: PathBuf,
    pub sha: Option<String>,
    pub keep: i64,
    pub min_free_gib: f64,
    pub retry_after: f64,
    pub max_distance: u64,
}

fn log_line(err: &mut dyn Write, message: &str) {
    let _ = writeln!(err, "target-pool: {message}");
}

/// Remove a build that crossed the workers' floor, partial output included, and keep a
/// typed `failed` marker: status names the reason, and the retry window applies.
fn abort_pool(
    git: &Git,
    directory: &Path,
    name: &str,
    repo: &Path,
    marker: &Path,
    sha: &str,
    key: &str,
    free: f64,
    floor: f64,
    err: &mut dyn Write,
) -> io::Result<i32> {
    remove_pool(git, directory, name, Some(repo), true);
    let detail = format!("stopped at {free:.1} GiB free; the workers' floor is {floor:.1} GiB");
    write_marker(
        marker,
        &json!({
            "name": name, "sha": sha, "key": key, "state": "failed",
            "reason": ABORT_FLOOR, "detail": detail, "built_at": now(),
            "free_gib": round1(free), "floor_gib": round1(floor),
        }),
    )?;
    log_line(
        err,
        &format!("{ABORT_FLOOR}: {detail}; partial pool removed"),
    );
    Ok(1)
}

/// Time a killed build's process group gets between TERM and KILL.
const STOP_GRACE: Duration = Duration::from_secs(15);

pub fn build(options: &BuildOptions, env: &Env, err: &mut dyn Write) -> io::Result<i32> {
    let git = Git { env };
    let repo = fs::canonicalize(&options.repo)?;
    let sha = git.out(
        &repo,
        &[
            "rev-parse",
            &format!("{}^{{commit}}", options.sha.as_deref().unwrap_or("HEAD")),
        ],
    )?;
    let directory = repo_dir(&git, env, &repo)?;
    let Some(held) = build_lock(&directory)? else {
        log_line(err, "another pool build is running; not starting a second");
        return Ok(0);
    };
    remember_repo(&git, &directory, &repo);
    let key = build_key(&repo, env)?;
    let name = format!("{sha}-{key}");
    let marker = directory.join(format!("{name}.json"));
    if marker.exists() {
        let record: Value = serde_json::from_str(&fs::read_to_string(&marker)?)
            .map_err(|e| io::Error::other(format!("{}: {e}", marker.display())))?;
        let ready = crate::target_pool::is_ready(&record);
        let recently_failed = record["state"] == "failed"
            && now() - record["built_at"].as_f64().unwrap_or(0.0) < options.retry_after;
        if ready || recently_failed {
            log_line(
                err,
                &format!(
                    "pool {name} is {}; nothing to do",
                    record["state"].as_str().unwrap_or("None")
                ),
            );
            return Ok(0);
        }
        remove_pool(&git, &directory, &name, Some(&repo), false);
    }
    if let Some(skip) = admission_skip(&git, &directory, &repo, &sha, &key, options.min_free_gib)? {
        record_event(&directory, "skip", Some(skip.clone()));
        log_line(
            err,
            &format!(
                "{}: {}; not building",
                skip["reason"].as_str().unwrap_or_default(),
                skip["detail"].as_str().unwrap_or_default()
            ),
        );
        return Ok(0);
    }
    let guard = if options.min_free_gib > 0.0 {
        Some(DiskGuard {
            path: directory.clone(),
            floor_gib: worker_floor_gib(env, &directory)?,
            interval: Duration::from_secs_f64(watch_seconds(env)),
        })
    } else {
        None
    };
    let worktree = directory.join(&name);
    git.out(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            &worktree.to_string_lossy(),
            &sha,
        ],
    )?;
    if build_key(&worktree, env)? != key {
        log_line(
            err,
            "the pool commit uses another toolchain than the requesting sandbox",
        );
        remove_pool(&git, &directory, &name, Some(&repo), false);
        return Ok(1);
    }
    let target = worktree.join("target");
    let started = Instant::now();
    let mut ctx = BuildRun::default();
    let outcome = build_in_worktree(
        &git,
        &directory,
        &repo,
        &sha,
        &key,
        &name,
        &worktree,
        &target,
        options,
        guard.as_ref(),
        started,
        &mut ctx,
        err,
    );
    match outcome {
        Err(Stop::Abort { free, floor }) => {
            return abort_pool(
                &git, &directory, &name, &repo, &marker, &sha, &key, free, floor, err,
            );
        }
        Err(Stop::Failed(error)) => return Err(error),
        Ok(()) => {}
    }
    let mut record = Map::new();
    for (field, value) in [
        ("name", json!(name)),
        ("sha", json!(sha)),
        ("key", json!(key)),
        ("target", json!(target)),
        ("worktree", json!(worktree)),
        ("built_at", json!(now())),
        (
            "build_seconds",
            json!(round1(started.elapsed().as_secs_f64())),
        ),
        ("command", json!(ctx.command)),
        ("log_path", json!(ctx.build_log)),
        ("seed", ctx.seed.clone().unwrap_or(Value::Null)),
    ] {
        record.insert(field.into(), value);
    }
    let exit_code = ctx.exit_code;
    if exit_code != 0 && ctx.degraded.is_none() {
        remove_pool(&git, &directory, &name, Some(&repo), true);
        let mut failed = record.clone();
        failed.insert("state".into(), json!("failed"));
        failed.insert("exit_code".into(), json!(exit_code));
        failed.insert("error_tail".into(), json!(log_tail(&ctx.build_log)));
        write_marker(&marker, &Value::Object(failed))?;
        log_line(
            err,
            &format!(
                "pool build failed (exit {exit_code}); log: {}",
                ctx.build_log.display()
            ),
        );
        return Ok(exit_code);
    }
    mark_shared_read_only(&target);
    let size = tree_size(&target);
    record.insert("size_bytes".into(), json!(size));
    if let Some(Value::Object(degraded)) = ctx.degraded.clone() {
        record.insert("degraded".into(), json!(true));
        record.extend(degraded);
        log_line(
            err,
            &format!(
                "pool {name}: tests do not compile (exit {exit_code}); keeping the libraries and binaries that built"
            ),
        );
    }
    record.insert("state".into(), json!("ready"));
    write_marker(&marker, &Value::Object(record.clone()))?;
    record_event(
        &directory,
        "last_pool",
        fields(json!({"size_bytes": size, "sha": sha})),
    );
    record_event(&directory, "skip", None);
    log_line(
        err,
        &format!(
            "pool {name} ready in {}s",
            dumps(&record["build_seconds"], None)
        ),
    );
    let mut skipped = vec![];
    gc(
        &git,
        &directory,
        &GcOptions {
            keep: options.keep.max(1),
            repo: Some(&repo),
            pressure: false,
            reference: None,
            max_distance: DEFAULT_MAX_DISTANCE,
            filesystem: None,
        },
        &mut skipped,
    );
    drop(held);
    Ok(0)
}

#[derive(Default)]
struct BuildRun {
    command: Vec<String>,
    build_log: PathBuf,
    seed: Option<Value>,
    degraded: Option<Value>,
    exit_code: i32,
}

#[allow(clippy::too_many_arguments)]
fn build_in_worktree(
    git: &Git,
    directory: &Path,
    _repo: &Path,
    sha: &str,
    key: &str,
    name: &str,
    worktree: &Path,
    target: &Path,
    options: &BuildOptions,
    guard: Option<&DiskGuard>,
    started: Instant,
    run: &mut BuildRun,
    err: &mut dyn Write,
) -> Result<(), Stop> {
    let env = git.env;
    let claimed = claim_pool(directory, &ready_pools(directory, key), |left| {
        choose_pool(git, worktree, sha, left, options.max_distance)
    });
    if let Some((chosen, in_use)) = claimed {
        let distance = chosen["distance"].as_u64().unwrap_or(0);
        log_line(
            err,
            &format!(
                "seeding pool {name} from {} ({distance} commits behind)",
                chosen["name"].as_str().unwrap_or_default()
            ),
        );
        fs::create_dir(target)?;
        let seeded = (|| -> io::Result<Value> {
            let times =
                normalise_mtimes(git, worktree, chosen["sha"].as_str().unwrap_or_default())?;
            let tree = seed_tree(
                Path::new(chosen["target"].as_str().unwrap_or_default()),
                target,
                wrapper_unlinks_outputs(env),
            )?;
            Ok(json!({"times": times, "tree": tree}))
        })();
        drop(in_use);
        let seeded = seeded?;
        let mut seed = Map::new();
        seed.insert("pool".into(), chosen["name"].clone());
        seed.insert("pool_sha".into(), chosen["sha"].clone());
        seed.insert("distance".into(), chosen["distance"].clone());
        for part in ["tree", "times"] {
            if let Value::Object(map) = &seeded[part] {
                seed.extend(map.clone());
            }
        }
        seed.insert(
            "seconds".into(),
            json!(round1(started.elapsed().as_secs_f64())),
        );
        run.seed = Some(Value::Object(seed));
    }
    if let Some(guard) = guard {
        if let Some(free) = guard.breach() {
            return Err(Stop::Abort {
                free,
                floor: guard.floor_gib,
            }); // the seed's copies alone crossed it
        }
    }
    let slot = env
        .get("RSI_CARGO_SLOT")
        .cloned()
        .or_else(|| {
            env.get("HOME")
                .map(|home| format!("{home}/.rsi/bin/cargo-slot"))
        })
        .unwrap_or_default();
    let held = env
        .get("RSI_CARGO_SLOT_HELD")
        .is_some_and(|v| !v.is_empty());
    let wrap: Vec<String> = if !held && is_executable(Path::new(&slot)) {
        vec![slot]
    } else {
        vec![]
    };
    let mut command = wrap.clone();
    command.extend(BUILD_COMMAND.iter().map(|s| (*s).to_string()));
    log_line(err, &format!("building pool {name}: {}", command.join(" ")));
    run.command = command.clone();
    let build_log = directory.join(format!("{name}.log"));
    run.build_log = build_log.clone();
    // sccache starts its server with a second socket at TMPDIR/sccache*/sock, even when
    // SCCACHE_SERVER_UDS is short. Keep scratch above the SHA-keyed worktree and discard
    // any per-sandbox temporary-directory environment.
    let temporary = pool_temporary_directory(directory)?;
    let environ = pool_build_environment(env, target, temporary.path())?;
    let output = File::create(&build_log)?;
    let grace = STOP_GRACE;
    let exit_code = run_logged(&command, worktree, &environ, &output, guard, grace)?;
    run.exit_code = exit_code;
    if exit_code != 0 {
        run.degraded = degraded_build(
            &wrap, worktree, &environ, &output, &build_log, exit_code, guard, grace,
        )?;
    }
    Ok(())
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

// ---- seed-from: a peer's pool over ssh ----

/// POSIX `shlex.split`.
pub fn shlex_split(text: &str) -> Vec<String> {
    let mut words = vec![];
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    current.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some(&n) if matches!(n, '"' | '\\' | '$' | '`') => {
                                current.push(n);
                                chars.next();
                            }
                            _ => current.push('\\'),
                        },
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    current.push(n);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// POSIX `shlex.quote`.
pub fn shlex_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".into();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if text.chars().all(safe) {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

fn ssh_command(env: &Env) -> Vec<String> {
    shlex_split(
        env.get("RSI_TARGET_POOL_SSH")
            .map_or("ssh -o BatchMode=yes", String::as_str),
    )
}

/// Ready pool records of another host (its `target-pool status`, JSON lines).
fn remote_pools(env: &Env, host: &str, remote_tool: &str) -> io::Result<Vec<Value>> {
    let ssh = ssh_command(env);
    let output = Command::new(&ssh[0])
        .args(&ssh[1..])
        .arg(host)
        .arg(format!("{remote_tool} status"))
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        let tail: String = trimmed
            .chars()
            .rev()
            .take(300)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(io::Error::other(format!(
            "{host}: {remote_tool} status failed: {tail}"
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            record["state"] == "ready" && record["target"].as_str().is_some_and(|t| !t.is_empty())
        })
        .collect())
}

/// Newest ready pool built the way this host builds, of a commit this repo has.
fn choose_remote_pool(
    git: &Git,
    repo: &Path,
    key: &str,
    records: &[Value],
    sha: Option<&str>,
) -> Option<Value> {
    records
        .iter()
        .filter(|record| record["key"] == key)
        .filter(|record| sha.is_none_or(|want| record["sha"] == want))
        .filter(|record| {
            git.succeeds(
                repo,
                &[
                    "cat-file",
                    "-e",
                    &format!("{}^{{commit}}", record["sha"].as_str().unwrap_or_default()),
                ],
            )
        })
        .max_by(|a, b| {
            a["built_at"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&b["built_at"].as_f64().unwrap_or(0.0))
        })
        .cloned()
}

/// The peer pool's disk use in bytes. `du -sk` (1 KiB blocks) is the one size flag GNU and BSD
/// `du` share: the Python tool's `du -sb` failed on a macOS peer (#494).
fn remote_size_bytes(env: &Env, host: &str, target: &str) -> io::Result<u64> {
    let ssh = ssh_command(env);
    let output = Command::new(&ssh[0])
        .args(&ssh[1..])
        .arg(host)
        .arg(format!("du -sk {}", shlex_quote(target)))
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "{host}: du failed: {}",
            stderr.trim()
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
        .ok_or_else(|| io::Error::other(format!("{host}: du printed no size")))
}

/// `tar | zstd` over ssh into `destination`: keeps hard links, modes and mtimes.
fn stream_target(
    env: &Env,
    host: &str,
    target: &str,
    destination: &Path,
    level: i64,
) -> io::Result<()> {
    let target_path = Path::new(target);
    let parent = target_path
        .parent()
        .map_or(String::from("."), |p| p.to_string_lossy().into_owned());
    let name = target_path
        .file_name()
        .map_or(String::new(), |n| n.to_string_lossy().into_owned());
    let remote = format!(
        "tar -C {} -cf - {} | zstd -T0 -{level} -c",
        shlex_quote(&parent),
        shlex_quote(&name)
    );
    let ssh = ssh_command(env);
    let mut sender = Command::new(&ssh[0])
        .args(&ssh[1..])
        .arg(host)
        .arg(remote)
        .env_clear()
        .envs(env)
        .stdout(Stdio::piped())
        .spawn()?;
    let mut decoder = Command::new("zstd")
        .arg("-dc")
        .env_clear()
        .envs(env)
        .stdin(Stdio::from(
            sender
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("no ssh stdout"))?,
        ))
        .stdout(Stdio::piped())
        .spawn()?;
    let mut unpacker = Command::new("tar")
        .args(["-xpf", "-", "-C"])
        .arg(destination)
        .env_clear()
        .envs(env)
        .stdin(Stdio::from(
            decoder
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("no zstd stdout"))?,
        ))
        .spawn()?;
    let codes = [
        unpacker.wait()?.code().unwrap_or(-1),
        decoder.wait()?.code().unwrap_or(-1),
        sender.wait()?.code().unwrap_or(-1),
    ];
    if codes.iter().any(|c| *c != 0) {
        return Err(io::Error::other(format!(
            "transfer failed (tar {}, zstd {}, ssh {})",
            codes[0], codes[1], codes[2]
        )));
    }
    Ok(())
}

/// Whether the marker describes a ready pool that is still on disk.
fn present(marker: &Path) -> bool {
    fs::read_to_string(marker)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|record| crate::target_pool::is_ready(&record))
}

pub struct SeedFromOptions {
    pub host: String,
    pub repo: PathBuf,
    pub sha: Option<String>,
    pub dry_run: bool,
    pub keep: Option<i64>,
    pub level: i64,
    pub remote_tool: String,
    pub min_free_gib: f64,
    pub min_headroom_gib: f64,
}

/// Copy another host's newest matching ready pool in, instead of a cold build.
pub fn seed_from(
    options: &SeedFromOptions,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<i32> {
    let git = Git { env };
    let repo = fs::canonicalize(&options.repo)?;
    let key = build_key(&repo, env)?;
    let directory = repo_dir(&git, env, &repo)?;
    let records = remote_pools(env, &options.host, &options.remote_tool)?;
    let Some(record) = choose_remote_pool(&git, &repo, &key, &records, options.sha.as_deref())
    else {
        log_line(
            err,
            &format!(
                "{} has no ready pool with key {key} for a commit this repo has; build one locally",
                options.host
            ),
        );
        return Ok(2);
    };
    let name = record["name"].as_str().unwrap_or_default().to_string();
    let size = remote_size_bytes(
        env,
        &options.host,
        record["target"].as_str().unwrap_or_default(),
    )?;
    let floor = scaled_floor_gib(&directory, options.min_free_gib)?;
    let free = free_gib(&directory)?;
    let plan = json!({
        "host": options.host, "pool": name, "sha": record["sha"], "key": key,
        "size_gib": round1(size as f64 / GIB), "free_gib": round1(free), "floor_gib": round1(floor),
    });
    let with_result = |extra: Value| -> Value {
        let mut merged = plan.clone();
        if let (Value::Object(m), Value::Object(e)) = (&mut merged, extra) {
            m.extend(e);
        }
        merged
    };
    let marker = directory.join(format!("{name}.json"));
    if present(&marker) {
        writeln!(
            out,
            "{}",
            dumps(&with_result(json!({"result": "already present"})), None)
        )?;
        return Ok(0);
    }
    if options.dry_run {
        writeln!(
            out,
            "{}",
            dumps(&with_result(json!({"result": "dry run"})), None)
        )?;
        return Ok(0);
    }
    // The transfer must leave the workers their cargo-slot floor too (#2773).
    let headroom = options.min_headroom_gib.max(if floor > 0.0 {
        worker_floor_gib(env, &directory)?
    } else {
        0.0
    });
    let size_gib = size as f64 / GIB;
    if free < floor || free - size_gib < headroom {
        log_line(
            err,
            &format!(
                "{SKIP_HEADROOM}: {free:.0} GiB free, pool is {size_gib:.1} GiB, pool floor {floor:.0} GiB, {headroom:.0} GiB must remain; settle sandboxes or reclaim caches first"
            ),
        );
        record_event(
            &directory,
            "skip",
            fields(json!({
                "reason": SKIP_HEADROOM, "free_gib": round1(free), "floor_gib": round1(headroom),
                "expected_gib": round1(size_gib),
                "needed_gib": round1(headroom + size_gib), "expected_source": "remote pool",
                "detail": format!(
                    "only {free:.1} GiB free; the transfer needs {:.1} GiB (floor {headroom:.1} + remote pool {size_gib:.1})",
                    headroom + size_gib
                ),
            })),
        );
        return Ok(3);
    }
    let Some(held) = build_lock(&directory)? else {
        log_line(err, "a pool build is running; not seeding alongside it");
        return Ok(0);
    };
    // The exclusive use lock is the same one gc takes before removing a pool: while we hold
    // it, a gc whose plan names this pool (a stale marker of the same name) skips it instead
    // of deleting the directory we are filling.
    let Some(in_use) = use_lock(&directory, &name, true) else {
        log_line(err, &format!("pool {name} is in use; not seeding over it"));
        drop(held);
        return Ok(0);
    };
    let started = Instant::now();
    let result = (|| -> io::Result<Option<(f64, Value)>> {
        if present(&marker) {
            writeln!(
                out,
                "{}",
                dumps(&with_result(json!({"result": "already present"})), None)
            )?;
            return Ok(None);
        }
        if let Ok(entries) = fs::read_dir(&directory) {
            for leftover in entries.flatten() {
                if leftover.file_name().to_string_lossy().starts_with(".seed-") {
                    let _ = fs::remove_dir_all(leftover.path()); // a killed seed's staging
                }
            }
        }
        let staging = directory.join(format!(".seed-{}", std::process::id()));
        fs::create_dir(&staging)?;
        let moved = (|| -> io::Result<PathBuf> {
            stream_target(
                env,
                &options.host,
                record["target"].as_str().unwrap_or_default(),
                &staging,
                options.level,
            )?;
            if !staging.join("target").is_dir() {
                return Err(io::Error::other("transfer produced no target directory"));
            }
            let worktree = directory.join(&name);
            let _ = fs::remove_file(&marker); // a stale marker must not describe the new tree
            let _ = fs::remove_dir_all(&worktree);
            fs::create_dir(&worktree)?;
            fs::rename(staging.join("target"), worktree.join("target"))?;
            Ok(worktree)
        })();
        let _ = fs::remove_dir_all(&staging);
        let worktree = moved?;
        let seconds = round1(started.elapsed().as_secs_f64());
        // The detached worktree is not copied: a seeded pool serves as a seed source only.
        // `gc` removes the directory like any other pool.
        mark_shared_read_only(&worktree.join("target"));
        let mut ready = Map::new();
        for (field, value) in [
            ("name", json!(name)),
            ("sha", record["sha"].clone()),
            ("key", json!(key)),
            ("target", json!(worktree.join("target"))),
            ("worktree", json!(worktree)),
            (
                "built_at",
                if record["built_at"].is_null() {
                    json!(now())
                } else {
                    record["built_at"].clone()
                },
            ),
            ("build_seconds", json!(seconds)),
            (
                "command",
                if record["command"].is_null() {
                    json!(BUILD_COMMAND)
                } else {
                    record["command"].clone()
                },
            ),
            ("log_path", Value::Null),
            ("seed", Value::Null),
            (
                "degraded",
                json!(record["degraded"].as_bool().unwrap_or(false)),
            ),
            ("size_bytes", json!(tree_size(&worktree.join("target")))),
            (
                "seeded_from",
                json!({"host": options.host, "seconds": seconds, "bytes": size}),
            ),
            ("state", json!("ready")),
        ] {
            ready.insert(field.into(), value);
        }
        let ready = Value::Object(ready);
        write_marker(&marker, &ready)?; // last: nothing sees the pool before it is whole
        record_event(
            &directory,
            "last_pool",
            fields(json!({"size_bytes": ready["size_bytes"], "sha": record["sha"]})),
        );
        // The pool has no worktree to name its repository: record the checkout so gc can
        // count its age and a refresh without --repo can rebuild after it goes (#2717).
        remember_repo(&git, &directory, &repo);
        Ok(Some((seconds, ready)))
    })();
    drop(in_use);
    drop(held);
    let Some((seconds, _ready)) = result? else {
        return Ok(0);
    };
    writeln!(
        out,
        "{}",
        dumps(
            &with_result(json!({
                "result": "seeded", "seconds": seconds,
                "mib_per_second": round1(size as f64 / 1_048_576.0 / seconds.max(0.1)),
            })),
            None
        )
    )?;
    let keep = options.keep.unwrap_or_else(|| default_keep(env)).max(1);
    let mut skipped = vec![];
    gc(
        &git,
        &directory,
        &GcOptions {
            keep,
            repo: Some(&repo),
            pressure: false,
            reference: None,
            max_distance: DEFAULT_MAX_DISTANCE,
            filesystem: None,
        },
        &mut skipped,
    );
    Ok(0)
}

#[cfg(test)]
#[path = "target_pool_build_tests.rs"]
mod tests;
