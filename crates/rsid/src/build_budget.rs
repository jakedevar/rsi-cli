//! #2119: the daemon side of the hub-wide build/test CPU budget.
//!
//! `rsid_core::build_slice` owns the policy (the operator's
//! `hub_build_cpu_percent`, the `rsi-build.slice` properties and each unit's
//! ceiling). This module applies it from async code and puts the merge-queue
//! lander, which runs as a child of the daemon rather than as a job unit, in
//! the same slice.

use rsid_core::build_slice::{self, SliceReport};
use tokio::process::Command;
use uuid::Uuid;

/// Where `systemd-run` lives; never resolved through `PATH`.
const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
/// The unit-name prefix of a lander scope.
pub(crate) const LANDER_SCOPE_PREFIX: &str = "rsi-lander-";

/// #2282: the daemon start. Publishes the setting the store just loaded (the
/// default when the operator never set one) and whether this is the user's own
/// hub daemon, so no write ever carries a value the store did not give, and a
/// throwaway daemon (a TUI end-to-end fixture) leaves the user's shared
/// slices alone. Then applies the budget like any other `apply`.
pub async fn apply_at_start(data_dir: &std::path::Path, hub_build_cpu_percent: u32) -> SliceReport {
    build_slice::set_percent(hub_build_cpu_percent);
    let owns = build_slice::owns_user_slices(data_dir);
    build_slice::bind_owner(owns);
    if !owns {
        tracing::warn!(
            data_dir = %data_dir.display(),
            "not the user's hub daemon: the shared rsi.slice and rsi-build.slice stay as they are"
        );
    }
    apply("start").await
}

/// Configure `rsi-build.slice` for the live setting off the async threads and
/// log the outcome. `reason` names the caller (`start`, `setting`, `lander`).
/// Never fails: an unavailable slice is in the report.
pub async fn apply(reason: &'static str) -> SliceReport {
    #[cfg(test)]
    APPLY_REASONS.lock().unwrap().push(reason);
    let report = tokio::task::spawn_blocking(build_slice::ensure_applied)
        .await
        .unwrap_or_else(|error| SliceReport::failed(format!("slice task failed: {error}")));
    log_report(reason, &report);
    report
}

/// Test seam: the reasons `apply` was called with (never writes real slices).
#[cfg(test)]
pub(crate) static APPLY_REASONS: std::sync::Mutex<Vec<&'static str>> =
    std::sync::Mutex::new(Vec::new());

/// How often the memory budget is re-sampled (#3999).
#[cfg(target_os = "linux")]
const MEMORY_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// #3999: re-apply the build slice on a timer so its MemoryHigh follows the
/// memory the operator's own work holds outside the slice (games, local
/// models), live and without a setting change. Linux only; elsewhere there is
/// no slice and nothing to follow. A process that may not write the user's
/// slices (a throwaway daemon, a test) never starts it, and the watch ends if
/// the slice becomes unavailable (no systemd user manager).
pub fn spawn_memory_watch() {
    #[cfg(target_os = "linux")]
    {
        if build_slice::write_refusal().is_some() {
            return;
        }
        tokio::spawn(async {
            let mut ticker = tokio::time::interval(MEMORY_WATCH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if apply("memory").await.is_unavailable() {
                    tracing::warn!("build slice unavailable; the memory budget watch stops");
                    return;
                }
            }
        });
    }
}

/// #2166: the post-save hook shared by every path that changes a daemon
/// setting (operator `UpdateDaemonConfig`, manager `ProposeDaemonSetting`), so
/// they cannot diverge. Returns the slice report when `field` has one.
pub async fn after_setting_saved(field: &str, reason: &'static str) -> Option<SliceReport> {
    if field != "hub_build_cpu_percent" {
        return None;
    }
    Some(apply(reason).await)
}

fn log_report(reason: &str, report: &SliceReport) {
    // #2126: the parent slice's CPUWeight (the desktop wins contention).
    if report.parent_state == build_slice::SliceState::Unavailable && !report.is_unavailable() {
        tracing::warn!(
            reason,
            slice = build_slice::PARENT_SLICE,
            detail = ?report.detail,
            "the parent slice's CPUWeight and CPUQuota could not be configured"
        );
    }
    if report.parent_state == build_slice::SliceState::Applied {
        tracing::info!(
            reason,
            slice = build_slice::PARENT_SLICE,
            cpu_weight = build_slice::PARENT_CPU_WEIGHT,
            cpu_quota_percent = report.cpu_quota_percent,
            "parent slice CPUWeight below the desktop's and the hub CPUQuota configured"
        );
    }
    if report.is_unavailable() {
        tracing::warn!(
            reason,
            slice = %report.slice,
            detail = ?report.detail,
            "no build CPU budget slice is available; cargo units run without a shared budget"
        );
    } else if report.state == build_slice::SliceState::Applied {
        tracing::info!(
            reason,
            slice = %report.slice,
            hub_build_cpu_percent = report.hub_build_cpu_percent,
            cpu_quota_percent = report.cpu_quota_percent,
            budget_cores = report.budget_cores,
            memory_high_bytes = report.memory.map(|memory| memory.high_bytes),
            memory_max_bytes = report.memory.map(|memory| memory.max_bytes),
            "build CPU and memory budget slice configured"
        );
    } else {
        tracing::debug!(reason, slice = %report.slice, "build CPU budget slice already configured");
    }
}

/// `inner` (the lander) as a transient scope in `rsi-build.slice`:
/// `systemd-run --user --scope` registers the process with systemd and then
/// execs the program in place, so the pid, process group and pipes stay the
/// lander's own. `PartOf=rsid.service` stops the scope with the daemon, as the
/// service's control-group kill used to; an unmanaged daemon has no such unit
/// and the property is inert. Program, arguments, directory and environment
/// edits carry over; stdio and process-group settings are the caller's.
///
/// Returns the command and its unit name.
pub(crate) fn lander_scope_command(inner: &Command) -> (Command, String) {
    let (scoped, unit) = scope_std_command_with(inner.as_std(), true);
    (Command::from(scoped), unit)
}

/// [`lander_scope_command`] for a run that must outlive the daemon (#3435): no
/// `PartOf=rsid.service`, so stopping or restarting the daemon leaves the
/// scope, and the gate inside it, running for the next daemon to adopt.
pub(crate) fn detached_lander_scope_command(inner: &Command) -> (Command, String) {
    let (scoped, unit) = scope_std_command_with(inner.as_std(), false);
    (Command::from(scoped), unit)
}

/// [`lander_scope_command`] for a blocking `std` command (the merge queue's
/// compile probe, #3394). The caller's `env_clear` is not carried over (`std`
/// does not expose it): give the command only variables it should add.
pub(crate) fn scope_std_command(source: &std::process::Command) -> (std::process::Command, String) {
    scope_std_command_with(source, true)
}

fn scope_std_command_with(
    source: &std::process::Command,
    part_of_daemon: bool,
) -> (std::process::Command, String) {
    let unit = format!("{LANDER_SCOPE_PREFIX}{}.scope", Uuid::new_v4());
    let mut scoped = std::process::Command::new(SYSTEMD_RUN);
    scoped
        .arg("--user")
        .arg("--scope")
        .arg("--collect")
        .arg("--quiet")
        // Lander arguments cross byte-for-byte; systemd-run would otherwise
        // expand `${NAME}` in them.
        .arg("--expand-environment=no")
        .arg(format!("--slice={}", build_slice::SLICE))
        .arg(format!("--unit={unit}"));
    if part_of_daemon {
        scoped.arg("--property=PartOf=rsid.service");
    }
    scoped
        .arg("--")
        .arg(source.get_program())
        .args(source.get_args());
    if let Some(dir) = source.get_current_dir() {
        scoped.current_dir(dir);
    }
    // #3391: `systemd-run` hands its environment to the scoped lander, so the
    // allowlist applies here too (it keeps the user-bus variables
    // `systemd-run --user` needs); then the lander's own variables on top.
    crate::lander_env_policy::apply(&mut scoped);
    copy_envs(source, &mut scoped);
    (scoped, unit)
}

fn copy_envs(source: &std::process::Command, target: &mut std::process::Command) {
    for (key, value) in source.get_envs() {
        match value {
            Some(value) => {
                target.env(key, value);
            }
            None => {
                target.env_remove(key);
            }
        }
    }
}

/// `inner` run through `/bin/sh` so its exit status survives the daemon that
/// started it (#3435): the shell records the status in `exit_file` (written
/// aside and renamed, so a reader never sees half of it) when `inner` ends,
/// whoever is still watching. Directory and environment edits carry over;
/// stdio and process-group settings are the caller's.
pub(crate) fn exit_recording_command(inner: &Command, exit_file: &std::path::Path) -> Command {
    let source = inner.as_std();
    let mut wrapped = Command::new("/bin/sh");
    wrapped
        .arg("-c")
        .arg(EXIT_RECORDING_SCRIPT)
        .arg(exit_file)
        .arg(source.get_program())
        .args(source.get_args());
    if let Some(dir) = source.get_current_dir() {
        wrapped.current_dir(dir);
    }
    crate::lander_env_policy::apply_tokio(&mut wrapped);
    copy_envs(source, wrapped.as_std_mut());
    wrapped
}

/// `$0` is the exit file, `"$@"` the lander command line.
const EXIT_RECORDING_SCRIPT: &str =
    r#""$@"; code=$?; printf '%s' "$code" > "$0.tmp" && mv -f "$0.tmp" "$0"; exit "$code""#;

/// Whether the transient scope `unit` is still running. A scope that systemd
/// already collected reads as not running.
pub(crate) async fn scope_active(unit: String) -> bool {
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(build_slice::SYSTEMCTL)
            .args(["--user", "is-active", "--quiet"])
            .arg(&unit)
            .env_remove("RSI_SESSION_TOKEN")
            .status()
            .is_ok_and(|status| status.success())
    })
    .await
    .unwrap_or(false)
}

/// Stop a lander scope: SIGTERM to everything in it, a short grace, then
/// SIGKILL (the order a timed-out attached run gets, #1251).
pub(crate) async fn stop_scope(unit: String) {
    let term = unit.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::process::Command::new(build_slice::SYSTEMCTL)
            .args(["--user", "kill", "--kill-whom=all", "--signal=SIGTERM"])
            .arg(&term)
            .env_remove("RSI_SESSION_TOKEN")
            .output();
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    kill_scope(unit).await;
}

/// Kill whatever is left in a lander scope (a descendant that left the
/// lander's process group). Best effort: a scope that already ended is fine.
pub(crate) async fn kill_scope(unit: String) {
    let _ = tokio::task::spawn_blocking(move || kill_scope_blocking(&unit)).await;
}

/// [`kill_scope`] for blocking code.
pub(crate) fn kill_scope_blocking(unit: &str) {
    let _ = std::process::Command::new(build_slice::SYSTEMCTL)
        .args(["--user", "kill", "--kill-whom=all", "--signal=SIGKILL"])
        .arg(unit)
        .env_remove("RSI_SESSION_TOKEN")
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// Tests (and the real-daemon fixtures they start) share the user's systemd
    /// manager with the running daemon: they must never write the operator's
    /// real `rsi-build.slice`, which an earlier version did from a quota-limited
    /// test unit, throttling the very run that wrote it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn tests_never_write_the_real_build_slice() {
        let report = build_slice::ensure_applied();
        assert!(report.is_unavailable(), "{report:?}");
        assert!(
            report
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("test seam")),
            "{report:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_lander_runs_in_a_scope_of_the_build_slice_with_its_own_arguments() {
        let mut lander = Command::new("/opt/rsi/rsi-rolling-land");
        lander
            .current_dir("/q/repo")
            .args(["--repo", "/q/repo", "--accepted", "${abc}"])
            .env("CARGO_TARGET_DIR", "/q/target")
            .env_remove("RSI_SESSION_TOKEN");
        let (scoped, unit) = lander_scope_command(&lander);
        let std = scoped.as_std();
        assert_eq!(std.get_program(), "/usr/bin/systemd-run");
        let args: Vec<&OsStr> = std.get_args().collect();
        let before: Vec<String> = args
            .iter()
            .take_while(|arg| **arg != "--")
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        for expected in [
            "--user",
            "--scope",
            "--collect",
            "--expand-environment=no",
            "--slice=rsi-build.slice",
            "--property=PartOf=rsid.service",
        ] {
            assert!(
                before.contains(&expected.to_string()),
                "{expected}: {before:?}"
            );
        }
        assert!(unit.starts_with("rsi-lander-") && unit.ends_with(".scope"));
        assert!(before.contains(&format!("--unit={unit}")));
        let after: Vec<&OsStr> = args
            .iter()
            .skip_while(|arg| **arg != "--")
            .skip(1)
            .copied()
            .collect();
        assert_eq!(
            after,
            [
                "/opt/rsi/rsi-rolling-land",
                "--repo",
                "/q/repo",
                "--accepted",
                "${abc}"
            ]
        );
        assert_eq!(std.get_current_dir(), Some(std::path::Path::new("/q/repo")));
        let envs: Vec<(&OsStr, Option<&OsStr>)> = std.get_envs().collect();
        assert!(envs.contains(&(
            OsStr::new("CARGO_TARGET_DIR"),
            Some(OsStr::new("/q/target"))
        )));
        // #3391: the scope starts from the allowlisted environment, so the
        // session token is cleared (not merely removed) and never set.
        assert!(
            envs.iter()
                .all(|(name, value)| *name != "RSI_SESSION_TOKEN" || value.is_none()),
            "{envs:?}"
        );
        assert!(
            envs.iter()
                .all(|(name, value)| crate::lander_env_policy::is_allowed(name) || value.is_none()),
            "{envs:?}"
        );
    }
}
