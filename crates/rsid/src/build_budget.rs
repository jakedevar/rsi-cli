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
    let source = inner.as_std();
    let unit = format!("{LANDER_SCOPE_PREFIX}{}.scope", Uuid::new_v4());
    let mut scoped = Command::new(SYSTEMD_RUN);
    scoped
        .arg("--user")
        .arg("--scope")
        .arg("--collect")
        .arg("--quiet")
        // Lander arguments cross byte-for-byte; systemd-run would otherwise
        // expand `${NAME}` in them.
        .arg("--expand-environment=no")
        .arg(format!("--slice={}", build_slice::SLICE))
        .arg(format!("--unit={unit}"))
        .arg("--property=PartOf=rsid.service")
        .arg("--")
        .arg(source.get_program())
        .args(source.get_args());
    if let Some(dir) = source.get_current_dir() {
        scoped.current_dir(dir);
    }
    for (key, value) in source.get_envs() {
        match value {
            Some(value) => {
                scoped.env(key, value);
            }
            None => {
                scoped.env_remove(key);
            }
        }
    }
    (scoped, unit)
}

/// Kill whatever is left in a lander scope (a descendant that left the
/// lander's process group). Best effort: a scope that already ended is fine.
pub(crate) async fn kill_scope(unit: String) {
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::process::Command::new(build_slice::SYSTEMCTL)
            .args(["--user", "kill", "--kill-whom=all", "--signal=SIGKILL"])
            .arg(&unit)
            .env_remove("RSI_SESSION_TOKEN")
            .output();
    })
    .await;
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
        assert!(envs.contains(&(OsStr::new("RSI_SESSION_TOKEN"), None)));
    }
}
