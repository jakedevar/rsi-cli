//! Shared PID namespace probe/wrapper (#433). Admission remains on the host:
//! wrap only the bounded unit's child command, never the lease holder.
use crate::{Ctx, scoped_runner::process};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
pub const UNAVAILABLE_EXIT: i32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mapping {
    CurrentUser,
    Root,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Namespaced,
    Unavailable,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Status {
    pub mode: Mode,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping: Option<Mapping>,
}
impl Status {
    pub fn describe(&self) -> String {
        let mut text = format!(
            "pid_isolation={}",
            match self.mode {
                Mode::Namespaced => "namespaced",
                Mode::Unavailable => "unavailable",
            }
        );
        if let Some(mapping) = self.mapping {
            text.push_str(match mapping {
                Mapping::CurrentUser => " mapping=current_user",
                Mapping::Root => " mapping=root",
            });
        }
        if !self.reason.is_empty() {
            text.push_str(&format!(" reason={}", self.reason));
        }
        text
    }
    pub fn record(&self, receipt: &mut Value) {
        receipt["pid_isolation"] = serde_json::to_value(self.mode).expect("mode serializes");
        if let Some(object) = receipt.as_object_mut() {
            object.remove("pid_isolation_reason");
            if !self.reason.is_empty() {
                object.insert("pid_isolation_reason".into(), self.reason.clone().into());
            }
        }
    }
}

/// An immutable, once-per-run probe result. Share the same instance with the
/// package, residual and desktop adapters; never re-probe per lane.
#[derive(Clone, Debug)]
pub struct Isolation {
    status: Status,
    prefix: Vec<String>,
    /// Reaping PID 1 that runs the command inside the namespace (`pid-isolate --init --`).
    init: Vec<String>,
}
impl Isolation {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            status: Status {
                mode: Mode::Unavailable,
                reason: reason.into(),
                mapping: None,
            },
            prefix: vec![],
            init: vec![],
        }
    }
    pub fn status(&self) -> &Status {
        &self.status
    }
    pub fn is_namespaced(&self) -> bool {
        self.status.mode == Mode::Namespaced
    }
    pub fn wrap(&self, command: &[String]) -> Vec<String> {
        self.prefix
            .iter()
            .chain(&self.init)
            .chain(command)
            .cloned()
            .collect()
    }
}
fn prefix(unshare: &Path, mapping: Mapping, uid: u32, gid: u32) -> Vec<String> {
    let mut args = vec![unshare.to_string_lossy().into_owned(), "--user".into()];
    match mapping {
        Mapping::CurrentUser => {
            args.extend([format!("--map-user={uid}"), format!("--map-group={gid}")])
        }
        Mapping::Root => args.push("--map-root-user".into()),
    }
    args.extend(["--pid", "--fork", "--kill-child", "--mount-proc", "--"].map(Into::into));
    args
}
/// The canary command is `[helper, pid-isolate, --canary]`; the same helper as the reaping init is
/// `[helper, pid-isolate, --init, --]`. A canary without that shape cannot name an init, so the
/// command would run as PID 1 unreaped; only test fixtures do that.
fn init_command(canary: &[String]) -> Vec<String> {
    match canary.split_last() {
        Some((last, rest)) if last == "--canary" => rest
            .iter()
            .cloned()
            .chain(["--init".into(), "--".into()])
            .collect(),
        _ => vec![],
    }
}
fn executable(ctx: &Ctx, name: &str) -> Option<PathBuf> {
    std::env::split_paths(ctx.var("PATH").unwrap_or_default()).find_map(|dir| {
        let path = ctx.path(dir).join(name);
        let metadata = std::fs::metadata(&path).ok()?;
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
}
struct ProbeResult {
    code: i32,
    stderr: String,
}
struct Host<'a> {
    linux: bool,
    unshare: Option<PathBuf>,
    uid: u32,
    gid: u32,
    pid: u32,
    parent: u32,
    canary: &'a [String],
}
fn probe_with(
    ctx: &Ctx,
    host: Host<'_>,
    mut run: impl FnMut(&[String]) -> io::Result<ProbeResult>,
) -> Isolation {
    if ctx
        .var("RSI_PID_ISOLATION")
        .is_some_and(|s| ["off", "0", "false", "no"].contains(&s.to_ascii_lowercase().as_str()))
    {
        return Isolation::unavailable("disabled");
    }
    if !host.linux {
        return Isolation::unavailable("not_linux");
    }
    let Some(unshare) = host.unshare else {
        return Isolation::unavailable("unshare_missing");
    };
    if host.canary.is_empty() {
        return Isolation::unavailable("probe_failed:empty_canary");
    }
    let mut reason = "unknown".to_string();
    for mapping in [Mapping::CurrentUser, Mapping::Root] {
        let prefix = prefix(&unshare, mapping, host.uid, host.gid);
        let command: Vec<_> = prefix
            .iter()
            .chain(host.canary)
            .cloned()
            .chain([host.pid.to_string(), host.parent.to_string()])
            .collect();
        match run(&command) {
            Ok(done) if done.code == 0 => {
                return Isolation {
                    status: Status {
                        mode: Mode::Namespaced,
                        reason: String::new(),
                        mapping: Some(mapping),
                    },
                    prefix,
                    init: init_command(host.canary),
                };
            }
            Ok(done) if done.code == 12 => return Isolation::unavailable("host_pids_visible"),
            Ok(done) => {
                // Match the old helper's last stderr line/160-character bound.
                let detail: String = done
                    .stderr
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("")
                    .chars()
                    .take(160)
                    .collect();
                reason = format!(
                    "canary_exit_{}{}",
                    done.code,
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                );
            }
            Err(error) => {
                reason = format!(
                    "probe_failed:{}",
                    if error.kind() == io::ErrorKind::TimedOut {
                        "TimeoutExpired"
                    } else {
                        "OSError"
                    }
                )
            }
        }
    }
    Isolation::unavailable(reason)
}

/// `canary` must invoke this library's `canary` entry point in an already-built,
/// content-verified helper, e.g. [rsi-repo-guard, pid-isolate, --canary]. The
/// probe never compiles a client or acquires/releases the caller's admission.
pub fn probe(ctx: &Ctx, canary_command: &[String]) -> Isolation {
    #[cfg(unix)]
    // SAFETY: these identity getters have no preconditions.
    let (uid, gid, parent) = unsafe { (libc::getuid(), libc::getgid(), libc::getppid() as u32) };
    #[cfg(not(unix))]
    let (uid, gid, parent) = (0, 0, 0);
    let env = process::build_env(ctx);
    probe_with(
        ctx,
        Host {
            linux: cfg!(target_os = "linux"),
            unshare: executable(ctx, "unshare"),
            uid,
            gid,
            pid: std::process::id(),
            parent,
            canary: canary_command,
        },
        |command| {
            let mut stderr = String::new();
            let done = process::stream_channels(
                command,
                &ctx.cwd,
                &env,
                Instant::now() + PROBE_TIMEOUT,
                &AtomicUsize::new(0),
                |channel, line| {
                    if channel == 1 {
                        stderr.push_str(line);
                        if stderr.len() > 4096 {
                            let mut start = stderr.len() - 4096;
                            while !stderr.is_char_boundary(start) {
                                start += 1;
                            }
                            stderr.drain(..start);
                        }
                    }
                },
                |_, _| {},
            )?;
            if done.timed_out {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PID canary deadline",
                ));
            }
            if !done.output_complete {
                return Err(io::Error::other("incomplete PID canary output"));
            }
            Ok(ProbeResult {
                code: done.code,
                stderr,
            })
        },
    )
}

/// Runs *inside* unshare. PID 1 plus a fresh /proc are both required. The host
/// IDs are values only, never signalling targets. Low IDs may belong to init.
pub fn canary(args: &[String]) -> i32 {
    if args.len() != 2 {
        return 2;
    }
    let Ok(ids) = args
        .iter()
        .map(|s| s.parse::<u32>())
        .collect::<Result<Vec<_>, _>>()
    else {
        return 2;
    };
    #[cfg(target_os = "linux")]
    {
        if std::process::id() != 1 {
            return 11;
        }
        for pid in ids {
            if pid > 100 && Path::new("/proc").join(pid.to_string()).exists() {
                return 12;
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ids;
        UNAVAILABLE_EXIT
    }
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.first().is_some_and(|s| s == "--canary") {
        return canary(&args[1..]);
    }
    if args.first().is_some_and(|s| s == "--init") {
        return crate::pid_init::run(&args[1..], &ctx.cwd, err);
    }
    let mut command = args;
    let status_only = args == ["--status"];
    let mut require = false;
    while command.first().is_some_and(|s| s == "--require") {
        require = true;
        command = &command[1..];
    }
    if command.first().is_some_and(|s| s == "--") {
        command = &command[1..];
    }
    if !status_only && command.is_empty() {
        let _ = writeln!(
            err,
            "usage: pid-isolate [--require] -- COMMAND [ARG...] | --status"
        );
        return 2;
    }
    let helper = match std::env::current_exe() {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(error) => {
            let _ = writeln!(err, "pid-isolate: cannot locate canary: {error}");
            return 2;
        }
    };
    let isolation = probe(ctx, &[helper, "pid-isolate".into(), "--canary".into()]);
    if status_only {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(isolation.status()).expect("status serializes")
        );
        return if isolation.is_namespaced() {
            0
        } else {
            UNAVAILABLE_EXIT
        };
    }
    let _ = writeln!(err, "pid-isolate: {}", isolation.status().describe());
    let _ = err.flush();
    if require && !isolation.is_namespaced() {
        let _ = writeln!(
            err,
            "pid-isolate: --require: refusing to run without a PID namespace"
        );
        return UNAVAILABLE_EXIT;
    }
    if let Err(error) = process::python_free(command) {
        let _ = writeln!(err, "pid-isolate: {error}");
        return 2;
    }
    let argv = isolation.wrap(command);
    let mut child = std::process::Command::new(&argv[0]);
    child.args(&argv[1..]).current_dir(&ctx.cwd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = child.exec();
        let _ = writeln!(err, "pid-isolate: cannot run {}: {error}", argv[0]);
        127
    }
    #[cfg(not(unix))]
    {
        match child.status() {
            Ok(done) => done.code().unwrap_or(1),
            Err(error) => {
                let _ = writeln!(err, "pid-isolate: cannot run {}: {error}", argv[0]);
                127
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fake(
        ctx: &Ctx,
        linux: bool,
        available: bool,
        codes: &[i32],
        stderr: &str,
    ) -> (Isolation, Vec<Vec<String>>) {
        let mut calls = vec![];
        let mut codes = codes.iter();
        let found = probe_with(
            ctx,
            Host {
                linux,
                unshare: available.then(|| PathBuf::from("/usr/bin/unshare")),
                uid: 1000,
                gid: 1001,
                pid: 234,
                parent: 123,
                canary: &["helper".into(), "--canary".into()],
            },
            |argv| {
                calls.push(argv.to_vec());
                Ok(ProbeResult {
                    code: *codes.next().unwrap(),
                    stderr: stderr.into(),
                })
            },
        );
        (found, calls)
    }
    #[test]
    fn current_user_then_root_mapping_preserve_exact_namespace_flags() {
        let ctx = Ctx::new(".", &[]);
        let (found, calls) = fake(&ctx, true, true, &[0], "");
        assert!(found.is_namespaced());
        assert_eq!(found.status.mapping, Some(Mapping::CurrentUser));
        assert_eq!(
            calls[0],
            [
                "/usr/bin/unshare",
                "--user",
                "--map-user=1000",
                "--map-group=1001",
                "--pid",
                "--fork",
                "--kill-child",
                "--mount-proc",
                "--",
                "helper",
                "--canary",
                "234",
                "123"
            ]
        );
        let command = vec!["cargo".into(), "test".into()];
        let wrapped = found.wrap(&command);
        assert_eq!(wrapped[9..12], ["helper", "--init", "--"]);
        assert_eq!(wrapped[12..], command);
        let (found, calls) = fake(&ctx, true, true, &[1, 0], "unsupported map-user");
        assert_eq!(found.status.mapping, Some(Mapping::Root));
        assert_eq!(
            &calls[1][..3],
            ["/usr/bin/unshare", "--user", "--map-root-user"]
        );
    }
    #[test]
    fn unavailable_reasons_and_host_visibility_stop_without_false_success() {
        for setting in ["off", "0", "FaLsE", "NO"] {
            let (found, calls) = fake(
                &Ctx::new(".", &[("RSI_PID_ISOLATION", setting)]),
                true,
                true,
                &[],
                "",
            );
            assert_eq!(found.status.reason, "disabled");
            assert!(calls.is_empty());
        }
        let ctx = Ctx::new(".", &[]);
        assert_eq!(
            fake(&ctx, false, true, &[], "").0.status.reason,
            "not_linux"
        );
        assert_eq!(
            fake(&ctx, true, false, &[], "").0.status.reason,
            "unshare_missing"
        );
        let (found, calls) = fake(&ctx, true, true, &[12], "");
        assert_eq!(found.status.reason, "host_pids_visible");
        assert_eq!(calls.len(), 1);
        let (found, _) = fake(
            &ctx,
            true,
            true,
            &[1, 1],
            "first\nOperation not permitted\n",
        );
        assert_eq!(
            found.status.reason,
            "canary_exit_1: Operation not permitted"
        );
        let command = vec!["cargo".into(), "test".into()];
        assert_eq!(found.wrap(&command), command);
        let (found, _) = fake(&ctx, true, true, &[11, 11], &"é".repeat(200));
        assert_eq!(
            found.status.reason.chars().count(),
            "canary_exit_11: ".len() + 160
        );
    }
    #[test]
    fn probe_errors_remain_unavailable_and_canary_arguments_are_strict() {
        for (kind, reason) in [
            (io::ErrorKind::TimedOut, "probe_failed:TimeoutExpired"),
            (io::ErrorKind::NotFound, "probe_failed:OSError"),
        ] {
            let mut calls = 0;
            let found = probe_with(
                &Ctx::new(".", &[]),
                Host {
                    linux: true,
                    unshare: Some("unshare".into()),
                    uid: 1,
                    gid: 1,
                    pid: 234,
                    parent: 123,
                    canary: &["helper".into()],
                },
                |_| {
                    calls += 1;
                    Err(io::Error::new(kind, "fixture"))
                },
            );
            assert_eq!(found.status.reason, reason);
            assert_eq!(calls, 2);
        }
        assert_eq!(canary(&[]), 2);
        assert_eq!(canary(&["../123".into(), "2".into()]), 2);
    }
    #[test]
    fn receipt_and_cli_preserve_mode_reason_and_unavailable_exit() {
        let ctx = Ctx::new(".", &[("RSI_PID_ISOLATION", "off")]);
        let (mut out, mut err) = (vec![], vec![]);
        assert_eq!(main(&["--status".into()], &ctx, &mut out, &mut err), 3);
        assert_eq!(
            serde_json::from_slice::<Value>(&out).unwrap(),
            serde_json::json!({"mode":"unavailable","reason":"disabled"})
        );
        assert_eq!(main(&[], &ctx, &mut out, &mut err), 2);
        assert_eq!(
            main(
                &["--require".into(), "--".into(), "never-start".into()],
                &ctx,
                &mut out,
                &mut err
            ),
            3
        );
        let mut receipt = serde_json::json!({});
        Isolation::unavailable("disabled")
            .status
            .record(&mut receipt);
        assert_eq!(receipt["pid_isolation_reason"], "disabled");
        let (found, _) = fake(&Ctx::new(".", &[]), true, true, &[0], "");
        found.status.record(&mut receipt);
        assert_eq!(receipt, serde_json::json!({"pid_isolation":"namespaced"}));
        assert_eq!(
            found.status.describe(),
            "pid_isolation=namespaced mapping=current_user"
        );
    }
}
