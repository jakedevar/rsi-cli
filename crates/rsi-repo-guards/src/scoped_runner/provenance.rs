//! Exact committed source pin. A matching HEAD alone does not prove the bytes
//! a build consumed: tracked and untracked changes must also remain absent.
use super::process;
use crate::Ctx;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize)]
pub struct SourcePin {
    pub repo: PathBuf,
    pub head: String,
    pub base: String,
    /// A dry run that inspects a candidate other than the checked-out HEAD: nothing
    /// executes, so only the resolved commits are pinned (never acceptance evidence).
    #[serde(skip)]
    pub inspect_only: bool,
}
fn git_until(
    repo: &Path,
    env: &BTreeMap<String, String>,
    args: &[&str],
    deadline: Instant,
    cancel: &AtomicUsize,
) -> io::Result<(i32, String)> {
    let mut command = vec!["git".into(), "-c".into(), "core.fsmonitor=false".into()];
    command.extend(args.iter().map(|s| (*s).into()));
    let mut output = String::new();
    let mut oversized = false;
    let result = process::stream(
        &command,
        repo,
        env,
        deadline,
        cancel,
        |line| {
            if output.len() + line.len() <= 512 * 1024 {
                output.push_str(line);
            } else {
                oversized = true;
            }
        },
        |_| {},
    )?;
    if !result.output_complete || oversized {
        return Err(io::Error::other(
            "source provenance Git output was incomplete or exceeded its bound",
        ));
    }
    Ok((result.code, output))
}
fn git(repo: &Path, env: &BTreeMap<String, String>, args: &[&str]) -> io::Result<(i32, String)> {
    git_until(
        repo,
        env,
        args,
        Instant::now() + Duration::from_secs(10),
        &AtomicUsize::new(0),
    )
}
impl SourcePin {
    /// As `capture`, but a dry run may name a candidate other than the checked-out
    /// HEAD (or sit on a dirty worktree): it pins the resolved commits for inspection only.
    pub fn capture_for(ctx: &Ctx, base: &str, head: &str, dry_run: bool) -> io::Result<Self> {
        let pinned = Self::capture(ctx, base, head);
        if !dry_run {
            return pinned;
        }
        pinned.or_else(|_| Self::inspect(ctx, base, head))
    }
    fn resolve_in(ctx: &Ctx, reference: &str) -> io::Result<String> {
        let env = process::build_env(ctx);
        if reference.starts_with('-') || reference.contains(['\0', '\n', '\r']) {
            return Err(io::Error::other("invalid source ref"));
        }
        let (code, text) = git(
            &ctx.cwd,
            &env,
            &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
        )?;
        if code != 0 {
            return Err(io::Error::other(format!(
                "source ref {reference:?} did not resolve"
            )));
        }
        Ok(text.trim().into())
    }
    fn inspect(ctx: &Ctx, base: &str, head: &str) -> io::Result<Self> {
        Ok(Self {
            repo: ctx.cwd.clone(),
            base: Self::resolve_in(ctx, base)?,
            head: Self::resolve_in(ctx, head)?,
            inspect_only: true,
        })
    }
    pub fn capture(ctx: &Ctx, base: &str, head: &str) -> io::Result<Self> {
        let env = process::build_env(ctx);
        let pin = Self {
            repo: ctx.cwd.clone(),
            base: Self::resolve_in(ctx, base)?,
            head: Self::resolve_in(ctx, head)?,
            inspect_only: false,
        };
        pin.verify(&env)?;
        Ok(pin)
    }
    pub fn verify(&self, env: &BTreeMap<String, String>) -> io::Result<()> {
        self.verify_until(
            env,
            Instant::now() + Duration::from_secs(10),
            &AtomicUsize::new(0),
        )
    }
    fn verify_until(
        &self,
        env: &BTreeMap<String, String>,
        deadline: Instant,
        cancel: &AtomicUsize,
    ) -> io::Result<()> {
        if self.inspect_only {
            let (code, _) = git_until(
                &self.repo,
                env,
                &["cat-file", "-e", &format!("{}^{{commit}}", self.head)],
                deadline,
                cancel,
            )?;
            return if code == 0 {
                Ok(())
            } else {
                Err(io::Error::other(
                    "job_source_moved: the inspected commit vanished",
                ))
            };
        }
        let (code, head) = git_until(
            &self.repo,
            env,
            &["rev-parse", "--verify", "HEAD"],
            deadline,
            cancel,
        )?;
        if code != 0 || head.trim() != self.head {
            return Err(io::Error::other(
                "job_source_moved: HEAD differs from the source pin",
            ));
        }
        let (code, status) = git_until(
            &self.repo,
            env,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
            deadline,
            cancel,
        )?;
        if code != 0 || !status.is_empty() {
            return Err(io::Error::other(
                "job_source_moved: source pin requires a clean committed worktree",
            ));
        }
        Ok(())
    }

    /// Observe the pinned checkout once per second while work runs, including
    /// silent builds/tests. A probe has one ten-second deadline across both Git
    /// calls. Failure cancels only work owned by the supplied closure; the
    /// source error takes precedence over its resulting termination status.
    pub fn monitored<T>(
        &self,
        env: &BTreeMap<String, String>,
        cancel: &AtomicUsize,
        work: impl FnOnce(&AtomicUsize) -> io::Result<T>,
    ) -> io::Result<T> {
        self.monitor_with_interval(env, cancel, Duration::from_secs(1), work)
    }
    fn monitor_with_interval<T>(
        &self,
        env: &BTreeMap<String, String>,
        external: &AtomicUsize,
        interval: Duration,
        work: impl FnOnce(&AtomicUsize) -> io::Result<T>,
    ) -> io::Result<T> {
        self.verify(env)?;
        let stop = AtomicUsize::new(0);
        let cancel = AtomicUsize::new(external.load(Ordering::Relaxed));
        let (send, receive) = mpsc::channel();
        // Unwind must stop the watcher too: scope joins children on panic.
        struct Stop<'a>(mpsc::Sender<()>, &'a AtomicUsize);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.1.store(1, Ordering::Relaxed);
                let _ = self.0.send(());
            }
        }
        std::thread::scope(|scope| {
            let stop = &stop;
            let cancel = &cancel;
            let watcher = scope.spawn(move || -> io::Result<()> {
                let mut next = Instant::now() + interval;
                loop {
                    match receive.recv_timeout(Duration::from_millis(25)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                    }
                    let signal = external.load(Ordering::Relaxed);
                    if signal != 0 {
                        cancel.store(signal, Ordering::Relaxed);
                    }
                    if Instant::now() >= next {
                        let checked =
                            self.verify_until(env, Instant::now() + Duration::from_secs(10), &stop);
                        if stop.load(Ordering::Relaxed) != 0 {
                            return Ok(());
                        }
                        if let Err(error) = checked {
                            cancel.store(15, Ordering::Relaxed);
                            return Err(io::Error::other(format!(
                                "job_source_moved: source monitor: {error}"
                            )));
                        }
                        next = Instant::now() + interval;
                    }
                }
            });
            let guard = Stop(send, &stop);
            let outcome = work(&cancel);
            drop(guard);
            watcher
                .join()
                .map_err(|_| io::Error::other("source monitor panicked"))??;
            self.verify(env)?;
            outcome
        })
    }
}

/// Linux package workers belong to the daemon's shared build slice and carry
/// only build knobs. The admission holder remains outside the execution clock.
pub fn bounded_command(
    runner: &Path,
    plan: &Path,
    repo: &Path,
    seconds: f64,
    quota: f64,
    unit: &str,
    env: &BTreeMap<String, String>,
    slice: Option<&str>,
    isolation: &crate::pid_isolation::Isolation,
) -> io::Result<Vec<String>> {
    if !seconds.is_finite()
        || seconds <= 0.0
        || !quota.is_finite()
        || quota <= 0.0
        || unit.is_empty()
        || !unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(io::Error::other("invalid bounded worker budget/unit"));
    }
    // systemd owns the unshare wrapper and descendants inside its bounded
    // unit. Admission stays in the calling host process, outside this argv.
    let worker = isolation.wrap(&vec![
        runner.to_string_lossy().into_owned(),
        "--execute-plan".into(),
        plan.to_string_lossy().into_owned(),
    ]);
    if !cfg!(target_os = "linux") {
        return Ok(worker);
    }
    let mut command = vec![
        "systemd-run".into(),
        "--user".into(),
        "--collect".into(),
        "--wait".into(),
        "--pipe".into(),
        "--service-type=exec".into(),
        format!("--unit={unit}"),
        format!("--working-directory={}", repo.display()),
        "-p".into(),
        format!("RuntimeMaxSec={seconds}"),
        "-p".into(),
        format!("CPUQuota={quota}%"),
        "-p".into(),
        "TimeoutStopSec=2".into(),
        "-p".into(),
        "KillMode=control-group".into(),
    ];
    if let Some(slice) = slice.filter(|s| !s.is_empty()) {
        command.push(format!("--slice={slice}"));
    }
    command.extend(
        env.iter()
            .filter(|(key, _)| process::BUILD_ENV_NAMES.contains(&key.as_str()))
            .map(|(key, value)| format!("--setenv={key}={value}")),
    );
    command.extend(worker);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_worker_rejects_invalid_limits_and_never_copies_authority() {
        let env = BTreeMap::from([
            ("CARGO_TARGET_DIR".into(), "target".into()),
            ("UNRELATED_AUTHORITY".into(), "credential".into()),
        ]);
        assert!(
            bounded_command(
                Path::new("runner"),
                Path::new("plan"),
                Path::new("repo"),
                f64::NAN,
                200.0,
                "unit",
                &env,
                Some("rsi-build.slice"),
                &crate::pid_isolation::Isolation::unavailable("not_linux")
            )
            .is_err()
        );
        let command = bounded_command(
            Path::new("runner"),
            Path::new("plan"),
            Path::new("repo"),
            30.0,
            200.0,
            "rsi-scoped-test-test",
            &env,
            Some("rsi-build.slice"),
            &crate::pid_isolation::Isolation::unavailable("not_linux"),
        )
        .unwrap();
        assert!(command.iter().all(|arg| !arg.contains("credential")));
        if cfg!(target_os = "linux") {
            assert!(command.contains(&"--slice=rsi-build.slice".into()));
            assert!(command.contains(&"--setenv=CARGO_TARGET_DIR=target".into()));
        } else {
            assert_eq!(command, vec!["runner", "--execute-plan", "plan"]);
        }
    }

    #[cfg(unix)]
    fn fixture() -> (tempfile::TempDir, SourcePin, BTreeMap<String, String>) {
        let dir = tempfile::Builder::new()
            .prefix("rsi-source-watch-")
            .tempdir_in("/tmp")
            .unwrap();
        crate::testutil::git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("tracked"), "committed\n").unwrap();
        crate::testutil::git(dir.path(), &["add", "tracked"]);
        crate::testutil::git(dir.path(), &["commit", "-qm", "source fixture"]);
        let path = std::env::var("PATH").unwrap();
        let ctx = Ctx::new(dir.path(), &[("PATH", &path)]);
        let source = SourcePin::capture(&ctx, "HEAD", "HEAD").unwrap();
        (dir, source, process::build_env(&ctx))
    }
    #[cfg(unix)]
    #[test]
    fn source_watch_stops_silent_owned_work_on_tracked_untracked_or_head_movement() {
        for change in ["tracked", "untracked", "HEAD"] {
            let (dir, source, env) = fixture();
            let started = Instant::now();
            let result = source.monitor_with_interval(
                &env,
                &AtomicUsize::new(0),
                Duration::from_millis(25),
                |cancel| {
                    if change == "HEAD" {
                        crate::testutil::git(
                            dir.path(),
                            &["commit", "--allow-empty", "-qm", "advance fixture"],
                        );
                    } else {
                        std::fs::write(dir.path().join(change), "changed\n")?;
                    }
                    let done = process::stream(
                        &["/bin/sh".into(), "-c".into(), "sleep 30 & wait".into()],
                        dir.path(),
                        &env,
                        Instant::now() + Duration::from_secs(4),
                        cancel,
                        |_| {},
                        |_| {},
                    )?;
                    assert_eq!(done.cancelled, Some(15));
                    assert!(!done.timed_out);
                    Ok(done)
                },
            );
            assert!(
                result.unwrap_err().to_string().contains("job_source_moved"),
                "{change}"
            );
            assert!(started.elapsed() < Duration::from_secs(3), "{change}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn a_dry_run_may_inspect_a_candidate_other_than_the_checked_out_head() {
        let (dir, _, env) = fixture();
        let first = crate::testutil::git(dir.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        crate::testutil::git(
            dir.path(),
            &["commit", "--allow-empty", "-qm", "advance fixture"],
        );
        let path = std::env::var("PATH").unwrap();
        let ctx = Ctx::new(dir.path(), &[("PATH", &path)]);
        // A real run refuses a head the worktree is not on; a dry run inspects it.
        let refused = SourcePin::capture_for(&ctx, "HEAD", &first, false).unwrap_err();
        assert!(
            refused.to_string().contains("job_source_moved"),
            "{refused}"
        );
        let pin = SourcePin::capture_for(&ctx, "HEAD", &first, true).unwrap();
        assert!(pin.inspect_only);
        assert_eq!(pin.head, first);
        pin.verify(&env).unwrap();
        // The checked-out head is still pinned as an executing source.
        let current = SourcePin::capture_for(&ctx, "HEAD", "HEAD", true).unwrap();
        assert!(!current.inspect_only);
        // An unresolvable candidate is still an error, never an empty inspection.
        assert!(SourcePin::capture_for(&ctx, "HEAD", "no-such-ref", true).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn source_watch_preserves_success_external_cancellation_and_unwind_cleanup() {
        let (dir, source, env) = fixture();
        let mut output = String::new();
        let done = source
            .monitor_with_interval(
                &env,
                &AtomicUsize::new(0),
                Duration::from_millis(25),
                |cancel| {
                    process::stream(
                        &[
                            "/bin/sh".into(),
                            "-c".into(),
                            "sleep 0.15; printf completed".into(),
                        ],
                        dir.path(),
                        &env,
                        Instant::now() + Duration::from_secs(3),
                        cancel,
                        |line| output.push_str(line),
                        |_| {},
                    )
                },
            )
            .unwrap();
        assert_eq!(done.code, 0);
        assert_eq!(output, "completed");
        let done = source
            .monitored(&env, &AtomicUsize::new(2), |cancel| {
                process::stream(
                    &["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                    dir.path(),
                    &env,
                    Instant::now() + Duration::from_secs(3),
                    cancel,
                    |_| {},
                    |_| {},
                )
            })
            .unwrap();
        assert_eq!(done.cancelled, Some(2));
        let started = Instant::now();
        let panic = std::panic::catch_unwind(|| {
            let _: io::Result<()> =
                source.monitored(&env, &AtomicUsize::new(0), |_| panic!("fixture unwinds"));
        });
        assert!(panic.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
