//! `rsi-eval bench` (#3179): reports and paired compares over `BenchRunRecord` v1, and the
//! fingerprint-keyed record store behind baseline reuse.
//!
//! * `rsi-eval bench report <record>`: markdown report (exit 0).
//! * `rsi-eval bench compare <a> <b> [--fail-on-regression]`: paired compare; exit 2 on a
//!   regression when asked. This replaces the percent threshold of `gate.rs` for bench records.
//! * `rsi-eval bench lookup <dir> <fingerprint>`: prints the stored record path, exit 3 on a miss.
//!
//! Exit 1 is a setup error (unreadable or invalid record, mismatched task sets).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rsi_common::bench_record::{
    BenchRunRecord, DEFAULT_BOOTSTRAP_SEED, Verdict, compare, summarize,
};

use crate::errors::EvalError;
use crate::report::{render_bench_markdown, render_compare_markdown};

#[derive(Parser, Debug)]
#[command(name = "rsi-eval bench")]
pub struct BenchCli {
    #[command(subcommand)]
    command: BenchCommand,
}

#[derive(Subcommand, Debug)]
enum BenchCommand {
    /// Markdown report of one record.
    Report {
        record: PathBuf,
        /// Bootstrap seed (default fixed, so reports reproduce).
        #[arg(long, default_value_t = DEFAULT_BOOTSTRAP_SEED)]
        seed: u64,
    },
    /// Paired compare of two records on the same task set.
    Compare {
        /// Baseline record.
        a: PathBuf,
        /// Candidate record.
        b: PathBuf,
        #[arg(long, default_value_t = DEFAULT_BOOTSTRAP_SEED)]
        seed: u64,
        /// Exit 2 when the candidate is a clear regression (interval below zero).
        #[arg(long)]
        fail_on_regression: bool,
    },
    /// Find a stored record whose observation fingerprint is identical.
    Lookup { dir: PathBuf, fingerprint: String },
}

/// Reads and validates a record file.
///
/// # Errors
/// [`EvalError::Io`] or [`EvalError::Bench`].
pub fn load_record(path: &Path) -> Result<BenchRunRecord, EvalError> {
    let text = std::fs::read_to_string(path)?;
    BenchRunRecord::from_json(&text)
        .map_err(|error| EvalError::Bench(format!("{}: {error}", path.display())))
}

fn record_path(dir: &Path, fingerprint: &str) -> Result<PathBuf, EvalError> {
    let hex = fingerprint
        .strip_prefix("sha256:")
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| EvalError::Bench(format!("malformed fingerprint {fingerprint:?}")))?;
    Ok(dir.join(format!("{}.json", hex.to_ascii_lowercase())))
}

/// Stores `record` under its observation fingerprint; refuses a record whose stored fingerprint
/// does not match its inputs.
///
/// # Errors
/// [`EvalError::Bench`] or [`EvalError::Io`].
pub fn store_record(dir: &Path, record: &BenchRunRecord) -> Result<PathBuf, EvalError> {
    let computed = record.computed_fingerprint();
    if computed != record.run.observation_fingerprint {
        return Err(EvalError::Bench(format!(
            "stored fingerprint {} does not match inputs ({computed})",
            record.run.observation_fingerprint
        )));
    }
    let path = record_path(dir, &computed)?;
    std::fs::create_dir_all(dir)?;
    let json = record
        .to_json()
        .map_err(|error| EvalError::Bench(error.to_string()))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// The stored record for `fingerprint`, only when its fingerprint is identical.
///
/// # Errors
/// [`EvalError::Bench`] for a malformed fingerprint or an invalid stored file;
/// [`EvalError::Io`] for read failures other than a missing file.
pub fn lookup_reusable(
    dir: &Path,
    fingerprint: &str,
) -> Result<Option<(PathBuf, BenchRunRecord)>, EvalError> {
    let path = record_path(dir, fingerprint)?;
    let record = match load_record(&path) {
        Ok(record) => record,
        Err(EvalError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    Ok(record
        .reusable_for(fingerprint)
        .map(|_| (path, record.clone())))
}

fn run_command(command: BenchCommand) -> Result<ExitCode, EvalError> {
    match command {
        BenchCommand::Report { record, seed } => {
            let record = load_record(&record)?;
            let summary = summarize(&record, seed);
            println!("{}", render_bench_markdown(&record, &summary));
            Ok(ExitCode::SUCCESS)
        }
        BenchCommand::Compare {
            a,
            b,
            seed,
            fail_on_regression,
        } => {
            let (a, b) = (load_record(&a)?, load_record(&b)?);
            let comparison =
                compare(&a, &b, seed).map_err(|error| EvalError::Bench(error.to_string()))?;
            println!("{}", render_compare_markdown(&a, &b, &comparison));
            Ok(
                if fail_on_regression && comparison.verdict == Verdict::Regression {
                    ExitCode::from(2)
                } else {
                    ExitCode::SUCCESS
                },
            )
        }
        BenchCommand::Lookup { dir, fingerprint } => match lookup_reusable(&dir, &fingerprint)? {
            Some((path, _)) => {
                println!("{}", path.display());
                Ok(ExitCode::SUCCESS)
            }
            None => {
                eprintln!("rsi-eval bench: no reusable record for {fingerprint}");
                Ok(ExitCode::from(3))
            }
        },
    }
}

/// Entry point for `rsi-eval bench <args>`; `args` excludes the program name and `bench`.
pub fn run_cli(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let argv = std::iter::once(OsString::from("rsi-eval bench")).chain(args);
    let cli = BenchCli::parse_from(argv);
    match run_command(cli.command) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("rsi-eval: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use rsi_common::bench_record::{RunInfo, SCHEMA_VERSION, Timeouts, Totals, TrialRow};
    use std::collections::BTreeMap;

    fn trial(task: &str, passed: bool) -> TrialRow {
        TrialRow {
            attempt: 0,
            cached_tokens: 10,
            cost_usd: 0.5,
            exception_type: None,
            failure_cause: None,
            image_digest: "sha256:aa".to_string(),
            infra_interrupted: false,
            invalid_route: false,
            output_tokens: 5,
            passed,
            prompt_tokens: 20,
            reasoning_tokens: 0,
            reward: f64::from(u8::from(passed)),
            reward_valid: true,
            session_id: None,
            split: "held_out".to_string(),
            stop_reason: None,
            suspected_solution_lookup: false,
            task: task.to_string(),
            trace_sha256: None,
            turns: 3,
            wall_ms: 1000,
        }
    }

    pub(super) fn record(results: &[(&str, bool)], model: &str) -> BenchRunRecord {
        let mut record = BenchRunRecord {
            run: RunInfo {
                adapter_tree_hash: "tree".to_string(),
                concurrency: 1,
                config_digest: "sha256:cfg".to_string(),
                dataset_commit: "2fd12b88".to_string(),
                dataset_repo: "harbor-framework/terminal-bench-2".to_string(),
                effort: "high".to_string(),
                finished_at: "2026-10-19T13:00:00.000000000Z".to_string(),
                harbor_version: "0.24.0".to_string(),
                harness_build_digest: "sha256:build".to_string(),
                host: "h".to_string(),
                image_digests: BTreeMap::new(),
                k: 1,
                model: model.to_string(),
                observation_fingerprint: String::new(),
                route: "direct".to_string(),
                sampling: BTreeMap::new(),
                split: "held_out".to_string(),
                started_at: "2026-10-19T12:00:00.000000000Z".to_string(),
                tasks: results.iter().map(|(t, _)| (*t).to_string()).collect(),
                timeouts: Timeouts {
                    agent_sec: 900,
                    multiplier: 1.0,
                    verifier_sec: 600,
                },
                totals: Totals::default(),
            },
            schema_version: SCHEMA_VERSION,
            trials: results.iter().map(|(t, p)| trial(t, *p)).collect(),
        };
        record.seal();
        record
    }

    #[test]
    fn store_then_lookup_returns_only_an_identical_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let a = record(&[("t1", true), ("t2", false)], "model-a");
        let b = record(&[("t1", true), ("t2", false)], "model-b");
        assert_ne!(a.run.observation_fingerprint, b.run.observation_fingerprint);
        let path = store_record(dir.path(), &a).unwrap();
        assert!(path.exists());
        let (found_path, found) = lookup_reusable(dir.path(), &a.run.observation_fingerprint)
            .unwrap()
            .unwrap();
        assert_eq!((found_path, found), (path, a.clone()));
        assert!(
            lookup_reusable(dir.path(), &b.run.observation_fingerprint)
                .unwrap()
                .is_none()
        );
        assert!(lookup_reusable(dir.path(), "../escape").is_err());
    }

    #[test]
    fn store_refuses_a_stale_fingerprint_and_lookup_rejects_a_tampered_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut stale = record(&[("t1", true)], "m");
        stale.run.k = 3;
        assert!(store_record(dir.path(), &stale).is_err());

        let good = record(&[("t1", true)], "m");
        let path = store_record(dir.path(), &good).unwrap();
        let mut tampered = good.clone();
        tampered.run.model = "other".to_string();
        std::fs::write(&path, tampered.to_json().unwrap()).unwrap();
        assert!(lookup_reusable(dir.path(), &good.run.observation_fingerprint).is_err());
    }

    #[test]
    fn report_and_compare_render_markdown_from_stored_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = record(&[("t1", false), ("t2", true)], "m");
        let b = record(&[("t1", true), ("t2", true)], "m");
        let (pa, pb) = (dir.path().join("a.json"), dir.path().join("b.json"));
        std::fs::write(&pa, a.to_json().unwrap()).unwrap();
        std::fs::write(&pb, b.to_json().unwrap()).unwrap();
        let (la, lb) = (load_record(&pa).unwrap(), load_record(&pb).unwrap());
        assert_eq!(la, a);

        let md = render_bench_markdown(&lb, &summarize(&lb, DEFAULT_BOOTSTRAP_SEED));
        assert!(md.contains("| pass rate | 100.0% |"), "{md}");
        assert!(md.contains("95% Wilson"));
        assert!(md.contains(&b.run.observation_fingerprint));

        let cmp = compare(&la, &lb, DEFAULT_BOOTSTRAP_SEED).unwrap();
        let md = render_compare_markdown(&la, &lb, &cmp);
        assert!(
            md.contains("| improved / regressed / unchanged | 1 / 0 / 1 |"),
            "{md}"
        );
        assert!(md.contains("| t1 | 0.00 | 1.00 | +1.00 |"));
    }
}
