//! #3923: installed-binary boundary for the lander's additive shell lane.
//! Selection is from committed trees; only ported bash TAP suites run here.
//! The transitional Python lane keeps its own contract.
use crate::cli::{self, Opt};
use crate::scoped_runner::{process, residual, residual_run};
use crate::{Ctx, pid_isolation};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

const USAGE: &str = "--base SHA --head SHA --repo DIR --receipt PATH [--timeout-secs N]";
const OPTIONS: &[Opt] = &[
    Opt {
        name: "--base",
        value: true,
    },
    Opt {
        name: "--head",
        value: true,
    },
    Opt {
        name: "--repo",
        value: true,
    },
    Opt {
        name: "--receipt",
        value: true,
    },
    Opt {
        name: "--timeout-secs",
        value: true,
    },
];

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match cli::parse("residual-lane", USAGE, args, OPTIONS, &[], Some(0), err) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.code(),
    };
    let (Some(base), Some(head), Some(repo), Some(receipt)) = (
        parsed.value("--base"),
        parsed.value("--head"),
        parsed.value("--repo"),
        parsed.value("--receipt"),
    ) else {
        let _ = writeln!(err, "usage: residual-lane {USAGE}");
        return 2;
    };
    let seconds = match parsed
        .value("--timeout-secs")
        .unwrap_or("1200")
        .parse::<u64>()
    {
        Ok(seconds) if (1..=14400).contains(&seconds) => seconds,
        _ => {
            let _ = writeln!(err, "residual-lane: --timeout-secs must be in 1..=14400");
            return 2;
        }
    };
    let result = (|| -> io::Result<bool> {
        let cancel = process::install_signals()?;
        run(
            ctx,
            &ctx.path(repo),
            base,
            head,
            &ctx.path(receipt),
            Duration::from_secs(seconds),
            &cancel,
            out,
        )
    })();
    match result {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(error) => {
            let _ = writeln!(err, "residual-lane: {error}");
            1
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    ctx: &Ctx,
    repo: &Path,
    base: &str,
    head: &str,
    path: &Path,
    total: Duration,
    cancel: &AtomicUsize,
    out: &mut dyn Write,
) -> io::Result<bool> {
    let logs = path
        .parent()
        .ok_or_else(|| io::Error::other("receipt has no parent"))?;
    std::fs::create_dir_all(logs)?;
    let mut selected = Vec::new();
    let mut isolation = pid_isolation::Isolation::unavailable("no shell suites selected");
    let result = (|| -> io::Result<Vec<Value>> {
        let entries = residual::lane(repo, base, head)?
            .into_iter()
            .filter(|entry| entry["runner"] == "bash")
            .collect::<Vec<_>>();
        selected = entries
            .iter()
            .map(|entry| entry["test_file"].clone())
            .collect();
        if entries.is_empty() {
            return Ok(entries);
        }
        let helper = std::env::current_exe()?;
        let vars = ctx.vars().collect::<Vec<_>>();
        let lane_ctx = Ctx::new(repo, &vars);
        isolation = pid_isolation::probe(
            &lane_ctx,
            &[
                helper.to_string_lossy().into_owned(),
                "pid-isolate".into(),
                "--canary".into(),
            ],
        );
        writeln!(out, "shell_gate {}", isolation.status().describe())?;
        let env = ctx
            .vars()
            .filter(|(key, _)| *key != "TAP_SELECT")
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>();
        let mut output_error = None;
        let result = residual_run::execute(
            entries,
            false,
            &residual_run::Run {
                work: repo,
                env: &env,
                isolation: &isolation,
                cancel,
                per_suite: Duration::from_secs(300),
                total_left: Some(total),
                log_dir: logs,
            },
            &mut |line| {
                if let Err(error) = writeln!(out, "{line}").and_then(|()| out.flush()) {
                    output_error = Some(error);
                }
            },
        )?;
        if let Some(error) = output_error {
            return Err(error);
        }
        Ok(result)
    })();
    let (entries, error) = match result {
        Ok(entries) => (entries, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let ok = error.is_none()
        && entries.len() == selected.len()
        && entries.iter().all(|entry| entry["status"] == "passed");
    let mut receipt = json!({
        "schema_version": 1, "base": base, "receipt_head": head,
        "selected": selected, "ok": ok, "residual": entries, "error": error,
    });
    isolation.status().record(&mut receipt);
    std::fs::write(path, serde_json::to_vec_pretty(&receipt)?)?;
    Ok(ok)
}

#[cfg(all(test, unix))]
#[path = "residual_lane_cli_tests.rs"]
mod tests;
