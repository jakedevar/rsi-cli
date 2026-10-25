//! `rsi-eval bench tb2 preflight --plan <plan.json>` and `run --plan <plan.json>` (#3679 R2).
//!
//! Both commands read a [`RunnerPlanV1`] and use only Rust plus the `docker` and `ssh`
//! executables. Exit status: 0 ok (for `run`, every trial passed and was cleaned up), 2 `run`
//! finished but a trial did not pass or was not cleaned, and a failed operation exits with
//! [`OpKind::exit_code`] (20 preflight, 21 pin check, 22 trial operation, 23 ssh), the family
//! `scripts/bench-transport` used. stdout carries one JSON document; errors go to stderr as
//! `tb2_runner error [<kind>]: <detail>`.

use std::{io, path::Path};

use rsi_common::bench_runner::RunnerPlanV1;

use super::{
    runner,
    transport::{CommandExecutor, OpError, OpKind, SystemExecutor},
};

/// Read and parse a plan file.
///
/// # Errors
/// [`OpKind::InvalidPlan`] for an unreadable or malformed plan.
pub fn load_plan(path: &Path) -> Result<RunnerPlanV1, OpError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| OpError::new(OpKind::InvalidPlan, format!("{}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| OpError::new(OpKind::InvalidPlan, format!("{}: {e}", path.display())))
}

fn report_error(error: &OpError, err: &mut dyn io::Write) -> u8 {
    let _ = writeln!(
        err,
        "tb2_runner error [{}]: {}",
        error.kind.as_str(),
        error.detail
    );
    error.kind.exit_code()
}

fn print_json(value: &impl serde::Serialize, out: &mut dyn io::Write) -> io::Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    writeln!(out, "{text}")
}

/// `preflight` over an injected executor (tests use a fake one).
pub fn preflight_with(
    plan: &RunnerPlanV1,
    exec: &dyn CommandExecutor,
    out: &mut dyn io::Write,
    err: &mut dyn io::Write,
) -> u8 {
    match runner::preflight(plan, exec) {
        Ok(ready) => {
            if print_json(&ready.report, out).is_err() {
                return OpKind::Io.exit_code();
            }
            0
        }
        Err(error) => report_error(&error, err),
    }
}

/// `run` over an injected executor.
pub fn run_with(
    plan: &RunnerPlanV1,
    exec: &dyn CommandExecutor,
    out: &mut dyn io::Write,
    err: &mut dyn io::Write,
) -> u8 {
    match runner::run(plan, exec) {
        Ok(summary) => {
            if print_json(&summary, out).is_err() {
                return OpKind::Io.exit_code();
            }
            if summary.all_passed_and_clean() {
                0
            } else {
                let _ = writeln!(
                    err,
                    "tb2_runner: {} of {} trials passed with a clean teardown",
                    summary
                        .trials
                        .iter()
                        .filter(|t| t.passed && t.cleaned)
                        .count(),
                    summary.total
                );
                2
            }
        }
        Err(error) => report_error(&error, err),
    }
}

/// `rsi-eval bench tb2 preflight --plan <path>`.
pub fn preflight_command(plan: &Path, out: &mut dyn io::Write, err: &mut dyn io::Write) -> u8 {
    match load_plan(plan) {
        Ok(plan) => preflight_with(&plan, &SystemExecutor, out, err),
        Err(error) => report_error(&error, err),
    }
}

/// `rsi-eval bench tb2 run --plan <path>`.
pub fn run_command(plan: &Path, out: &mut dyn io::Write, err: &mut dyn io::Write) -> u8 {
    match load_plan(plan) {
        Ok(plan) => run_with(&plan, &SystemExecutor, out, err),
        Err(error) => report_error(&error, err),
    }
}
