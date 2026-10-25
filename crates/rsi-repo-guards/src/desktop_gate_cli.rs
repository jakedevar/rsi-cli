//! `rsi-repo-guard desktop-test-gate`: the Rust replacement for
//! `scripts/desktop-test-gate.py` (#363). `--base` selects (a JSON list of argv
//! lists, `[]` when no input changed); `--run` runs both desktop test sets (npm
//! and the locked Cargo workspace) on a private copy and prints the final
//! `DESKTOP_LANE_RESULT {json}` line. Exit codes and result fields match the
//! legacy script; a signal stops the owned step tree, removes the private copy
//! and exits `128 + signal` with a typed `cancelled` result (the legacy script
//! printed none).
use crate::cli::{self, Opt};
use crate::desktop_execution::{self, Environment, ProcessBackend};
use crate::desktop_selection;
use crate::scoped_runner::process;
use crate::wire_json::legacy_json;
use crate::{Ctx, pid_isolation};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const RESULT_MARK: &str = "DESKTOP_LANE_RESULT ";
const USAGE: &str = "[-h] [--repo REPO] [--base BASE] [--head HEAD] [--run]";
const OPTIONS: [Opt; 4] = [
    Opt {
        name: "--repo",
        value: true,
    },
    Opt {
        name: "--base",
        value: true,
    },
    Opt {
        name: "--head",
        value: true,
    },
    Opt {
        name: "--run",
        value: false,
    },
];

/// The one argv the selected lane runs as (the Python-free `python3 <script> --run`).
pub fn lane_command(repo: &str) -> Vec<String> {
    [
        "scripts/repo-guard",
        "desktop-test-gate",
        "--run",
        "--repo",
        repo,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match cli::parse(
        "desktop-test-gate",
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
    let requested = ctx.path(parsed.value("--repo").unwrap_or("."));
    let repo = requested.canonicalize().unwrap_or(requested);
    if parsed.flag("--run") {
        let runner = std::env::current_exe().map_or_else(
            |_| "rsi-repo-guard".to_owned(),
            |path| path.to_string_lossy().into_owned(),
        );
        return match process::install_signals() {
            Ok(cancel) => run(ctx, &repo, &cancel, &runner, out, err),
            Err(error) => {
                let _ = writeln!(err, "desktop lane: {error}");
                1
            }
        };
    }
    let Some(base) = parsed.value("--base") else {
        let _ = writeln!(err, "usage: desktop-test-gate {USAGE}");
        let _ = writeln!(
            err,
            "desktop-test-gate: error: --base is required unless --run is given"
        );
        return 2;
    };
    select(
        &repo,
        base,
        parsed.value("--head").unwrap_or("HEAD"),
        out,
        err,
    )
}

pub fn select(
    repo: &std::path::Path,
    base: &str,
    head: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match desktop_selection::lane_selected(repo, base, head) {
        Ok(selected) => {
            let lanes = if selected {
                serde_json::json!([lane_command(".")])
            } else {
                serde_json::json!([])
            };
            let _ = writeln!(out, "{}", legacy_json(&lanes, false));
            0
        }
        Err(reason) => {
            let _ = writeln!(err, "{reason}");
            1
        }
    }
}

pub fn lane_environment(ctx: &Ctx) -> Environment {
    ctx.vars()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn run(
    ctx: &Ctx,
    repo: &std::path::Path,
    cancel: &AtomicUsize,
    runner: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let environment = lane_environment(ctx);
    let isolation = pid_isolation::probe(
        ctx,
        &[runner.to_owned(), "pid-isolate".into(), "--canary".into()],
    );
    let scratch_parent: PathBuf = ctx
        .var("TMPDIR")
        .filter(|value| !value.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from);
    let mut write_failed = false;
    let outcome = {
        let mut emit = |line: &str| {
            write_failed |= out.write_all(line.as_bytes()).is_err() || out.flush().is_err();
        };
        let mut backend = ProcessBackend {
            environment: &environment,
            repo,
            isolation: &isolation,
            cancel,
            emit: &mut emit,
            deadline: None,
        };
        desktop_execution::run_lane_with_cancel(
            repo,
            &environment,
            &scratch_parent,
            cancel,
            &mut backend,
        )
    };
    let signal = cancel.load(Ordering::Relaxed);
    match outcome {
        Ok((code, result)) => {
            let code = if result["status"] == "cancelled" && signal != 0 {
                128 + i32::try_from(signal).unwrap_or(0)
            } else {
                code
            };
            if write_failed {
                let _ = writeln!(err, "desktop lane: standard output closed");
                return 1;
            }
            match result["status"].as_str() {
                Some("desktop_lane_timed_out") => {
                    let _ = writeln!(
                        out,
                        "desktop_lane_timed_out: {} exceeded {} seconds",
                        result["failed_step"].as_str().unwrap_or("?"),
                        result["timeout_seconds"]
                            .as_f64()
                            .map_or_else(String::new, |seconds| seconds.to_string())
                    );
                }
                Some("desktop_lock_stale") => {
                    let _ = writeln!(
                        out,
                        "desktop lane: desktop_lock_stale: {}",
                        desktop_execution::LOCK_FIX
                    );
                }
                _ => {}
            }
            let _ = writeln!(out, "{RESULT_MARK}{}", legacy_json(&result, false));
            let _ = out.flush();
            code
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted && signal != 0 => {
            let result = serde_json::json!({"status":"cancelled","lane":"desktop","signal":signal,"executed_tests":0});
            let _ = writeln!(out, "{RESULT_MARK}{}", legacy_json(&result, false));
            let _ = out.flush();
            128 + i32::try_from(signal).unwrap_or(0)
        }
        Err(error) => {
            let _ = writeln!(err, "desktop lane: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git, scratch_repository, write};
    use serde_json::Value;

    fn ctx(dir: &std::path::Path) -> Ctx {
        Ctx::new(
            dir,
            &[
                ("PATH", &std::env::var("PATH").unwrap()),
                ("HOME", &std::env::var("HOME").unwrap_or_default()),
            ],
        )
    }
    fn commit(dir: &std::path::Path, message: &str) -> String {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
        git(dir, &["rev-parse", "HEAD"]).trim().to_owned()
    }
    fn invoke(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = main(&args, &ctx(dir), &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }
    fn marker(out: &str) -> Value {
        let line = out
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix(RESULT_MARK))
            .expect("result marker");
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn selection_prints_the_lane_command_or_an_empty_list_and_fails_closed_without_the_selector() {
        let temp = scratch_repository();
        let dir = temp.path();
        git(dir, &["init", "-q", "-b", "trunk"]);
        write(dir, "scripts/desktop-test-gate.py", "selector marker\n");
        write(dir, "desktop/package.json", "{}\n");
        write(dir, "crates/other/src/lib.rs", "one\n");
        let base = commit(dir, "base");
        write(dir, "crates/other/src/lib.rs", "two\n");
        let unrelated = commit(dir, "unrelated change");
        write(dir, "desktop/package.json", "{\"name\":\"x\"}\n");
        let touched = commit(dir, "desktop change");
        let repo = dir.to_string_lossy().into_owned();

        let (code, out, err) = invoke(
            dir,
            &["--repo", &repo, "--base", &base, "--head", &unrelated],
        );
        assert_eq!((code, out.as_str(), err.as_str()), (0, "[]\n", ""));
        let (code, out, err) = invoke(dir, &["--repo", &repo, "--base", &base, "--head", &touched]);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (
                0,
                "[[\"scripts/repo-guard\", \"desktop-test-gate\", \"--run\", \"--repo\", \".\"]]\n",
                ""
            )
        );
        let (code, _, err) = invoke(dir, &["--repo", &repo]);
        assert_eq!(code, 2);
        assert!(
            err.ends_with("error: --base is required unless --run is given\n"),
            "{err}"
        );

        git(dir, &["rm", "-q", "scripts/desktop-test-gate.py"]);
        write(dir, "desktop/package.json", "{\"name\":\"y\"}\n");
        let missing = commit(dir, "selector removed with a desktop change");
        let (code, out, err) = invoke(
            dir,
            &["--repo", &repo, "--base", &touched, "--head", &missing],
        );
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(err.contains("is missing from the candidate"), "{err}");
        assert_eq!(lane_command(".")[1], "desktop-test-gate");
    }

    #[test]
    fn a_missing_runner_is_typed_unavailable_never_green() {
        let temp = scratch_repository();
        let empty = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(temp.path(), &[("PATH", &empty.path().to_string_lossy())]);
        let cancel = AtomicUsize::new(0);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(
            &ctx,
            temp.path(),
            &cancel,
            "/nonexistent-runner",
            &mut out,
            &mut err,
        );
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 1, "{out}");
        let result = marker(&out);
        assert_eq!(result["status"], "runner_unavailable");
        assert_eq!(result["runner"], "node");
        assert_eq!(result["executed_tests"], 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_before_staging_exits_128_plus_signal_with_a_typed_cancelled_result() {
        let temp = scratch_repository();
        let cancel = AtomicUsize::new(15);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(
            &ctx(temp.path()),
            temp.path(),
            &cancel,
            "/nonexistent-runner",
            &mut out,
            &mut err,
        );
        assert_eq!(code, 143);
        let result = marker(&String::from_utf8(out).unwrap());
        assert_eq!(result["status"], "cancelled");
        assert_eq!(result["signal"], 15);
        assert!(temp.path().read_dir().unwrap().all(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".git")
        }));
    }
}
