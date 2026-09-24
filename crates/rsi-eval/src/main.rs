//! `rsi-eval` — RSI-006 evaluation/replay harness CLI.
//!
//! Phase 5 wires the metrics collector, baseline I/O, and regression gate
//! on top of the Phase 4 corpus loader + driver. Captures a baseline JSON
//! at `eval/baselines/<harness>.json` (or `--capture-baseline`); when
//! `--baseline` is provided, gates the candidate against it and exits 2 on
//! regression.
//!
//! Exit codes (locked — RSI-007 Dreamer treats these as the gate oracle):
//! - 0  — gate passed (or no gate run)
//! - 1  — setup error (corpus invalid, daemon unreachable, default-socket refused)
//! - 2  — gate failed (one or more aggregate metrics exceeded the threshold)

use clap::Parser;
use rsi_eval::baseline::{default_baseline_path, read_baseline, write_baseline};
use rsi_eval::corpus::load_corpus;
use rsi_eval::driver::{self, ReplayOutcome};
use rsi_eval::errors::EvalError;
use rsi_eval::gate::{GateDecision, evaluate};
use rsi_eval::metrics::{ReplayResult, aggregate};
use rsi_eval::report::{render_json, render_markdown};
use rsi_eval::socket_guard;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(name = "rsi-eval", version)]
struct Cli {
    /// Identifier of the harness under test (typically a SHA256 of the
    /// resolved worker_preamble or a git sha). Used to label baseline output.
    #[arg(long)]
    harness: String,

    /// Corpus name (subdirectory under `eval/corpus/`). `default` resolves
    /// to `eval/corpus/` itself.
    #[arg(long, default_value = "default")]
    corpus: String,

    /// Path to a baseline JSON to gate against. If omitted, the run is
    /// captured but no gate is applied (exit 0 always on successful run).
    #[arg(long)]
    baseline: Option<PathBuf>,

    /// Per-metric regression threshold (percent). Default 10.
    #[arg(long, default_value_t = 10.0)]
    threshold_pct: f64,

    /// Where to write the markdown report. Default: stdout.
    #[arg(long)]
    report_md: Option<PathBuf>,

    /// Where to write the machine-readable JSON report. Default: stdout.
    #[arg(long)]
    report_json: Option<PathBuf>,

    /// Where to write the captured baseline JSON for this run.
    /// Default: `eval/baselines/<harness>.json`.
    #[arg(long)]
    capture_baseline: Option<PathBuf>,

    /// Refuse to run against the user's default daemon socket. The eval
    /// driver always wants to run against an isolated daemon. To override
    /// (e.g., debugging against a live daemon), pass `--allow-default-socket`.
    #[arg(long)]
    allow_default_socket: bool,

    /// Skip the launch step; load corpus, validate, and exit. For CI corpus
    /// validation.
    #[arg(long)]
    dry_run: bool,

    /// Per-replay timeout in seconds. Default 300 (5 minutes).
    #[arg(long, default_value_t = 300u64)]
    replay_timeout_sec: u64,

    /// Total run timeout in seconds. Default 1800 (30 minutes).
    #[arg(long, default_value_t = 1800u64)]
    total_timeout_sec: u64,

    /// Eval corpus root. Default: `eval/corpus/` relative to current dir.
    #[arg(long, default_value = "eval/corpus")]
    corpus_root: PathBuf,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rsi-eval: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, EvalError> {
    // 1. Load corpus first. Corpus errors are setup errors (exit 1).
    let corpus = load_corpus(&cli.corpus_root, &cli.corpus)?;
    eprintln!("rsi-eval: loaded {} corpus tickets", corpus.len());

    if cli.dry_run {
        eprintln!("rsi-eval: dry-run complete (corpus valid)");
        return Ok(ExitCode::from(0));
    }

    // 2. Guard against the user-default socket.
    let socket = socket_guard::check(cli.allow_default_socket)?;
    eprintln!("rsi-eval: using daemon socket {}", socket.display());

    // 3. Resolve a working_dir for replays. Default to current_dir.
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));

    // 4. Drive each ticket serially with the configured budgets.
    let total_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(cli.total_timeout_sec);
    let replay_timeout = std::time::Duration::from_secs(cli.replay_timeout_sec);
    let run_started = Instant::now();

    let mut outcomes: Vec<ReplayOutcome> = Vec::with_capacity(corpus.len());
    let mut expected_for_outcomes: Vec<rsi_eval::corpus::CorpusExpected> =
        Vec::with_capacity(corpus.len());
    for ticket in &corpus {
        if tokio::time::Instant::now() >= total_deadline {
            tracing::warn!(
                ticket = %ticket.id,
                "total run budget exceeded; skipping remaining tickets"
            );
            break;
        }
        eprintln!("rsi-eval: replaying ticket {}", ticket.id);
        let outcome =
            driver::replay_ticket(&socket, ticket, working_dir.clone(), replay_timeout).await?;
        outcomes.push(outcome);
        expected_for_outcomes.push(ticket.expected.clone());
    }

    // 5. Aggregate into a baseline snapshot. Tickets without a final session
    // (launch failed) are excluded from the per-ticket map.
    let mut replay_results: Vec<ReplayResult> = Vec::with_capacity(outcomes.len());
    for (outcome, expected) in outcomes.iter().zip(expected_for_outcomes.iter()) {
        if let Some(session) = outcome.session.clone() {
            replay_results.push(ReplayResult {
                ticket_id: outcome.ticket_id.clone(),
                session,
                turn_metrics: Vec::new(),
                expected: expected.clone(),
            });
        }
    }
    let wall_time_seconds = run_started.elapsed().as_secs_f64();
    // Use the harness label (or first hash captured) for traceability.
    let observed_hash = replay_results
        .iter()
        .find_map(|r| r.session.harness_version_hash.clone())
        .unwrap_or_else(|| cli.harness.clone());
    let git_commit = current_git_sha().unwrap_or_else(|| "unknown".to_string());
    let snapshot = aggregate(
        &replay_results,
        &observed_hash,
        &cli.corpus,
        &git_commit,
        wall_time_seconds,
    );

    // 6. Compare against baseline if provided.
    let gate = match cli.baseline.as_deref() {
        Some(path) => {
            let baseline_snapshot = read_baseline(path)?;
            evaluate(&baseline_snapshot, &snapshot, cli.threshold_pct)
        }
        None => GateDecision {
            passed: true,
            regressions: Vec::new(),
        },
    };

    // 7. Write the captured baseline (default path or explicit override).
    let capture_path = cli
        .capture_baseline
        .unwrap_or_else(|| default_baseline_path(&cli.harness));
    write_baseline(&snapshot, &capture_path)?;
    eprintln!("rsi-eval: baseline written to {}", capture_path.display());

    // 8. Render reports.
    let baseline_for_report = match cli.baseline.as_deref() {
        Some(path) => Some(read_baseline(path)?),
        None => None,
    };
    let md = render_markdown(baseline_for_report.as_ref(), &snapshot, &gate);
    match cli.report_md.as_deref() {
        Some(path) => std::fs::write(path, md)?,
        None => println!("{md}"),
    }
    let json = render_json(&snapshot, &gate);
    match cli.report_json.as_deref() {
        Some(path) => std::fs::write(path, serde_json::to_string_pretty(&json)?)?,
        None => println!("{}", serde_json::to_string_pretty(&json)?),
    }

    Ok(if gate.passed {
        ExitCode::from(0)
    } else {
        ExitCode::from(2)
    })
}

fn current_git_sha() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
