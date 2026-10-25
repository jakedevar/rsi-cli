//! Native Terminal-Bench 2.0 pin, split, refresh, and leak-audit tooling (#3620).
pub mod audit;
pub mod dataset;
pub mod pin;
pub mod split;
#[cfg(test)]
mod tests;
use clap::{Parser, Subcommand};
use std::{
    ffi::OsString,
    fs,
    io::{self, Read},
    path::PathBuf,
    process::ExitCode,
};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const DATASET: &str = "eval/bench/tb2/dataset.toml";
pub const SPLIT: &str = "eval/bench/tb2/split.json";
#[derive(Debug, Parser)]
#[command(
    name = "rsi-eval bench tb2",
    about = "Pinned Terminal-Bench 2.0 tooling (native Rust)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    /// Audit added diff lines or files. Findings contain file, line and kind, never the term.
    LeakAudit {
        #[arg(long)]
        checkout: Option<PathBuf>,
        /// Unified diff file, or - for stdin.
        #[arg(long, conflicts_with = "files", required_unless_present = "files")]
        diff: Option<PathBuf>,
        /// Files or candidate directories to audit in full.
        #[arg(long,num_args=1..,conflicts_with="diff",required_unless_present="diff")]
        files: Vec<PathBuf>,
        /// Repeatable glob. Defaults exclude only the two pin files; candidates are always audited.
        #[arg(long)]
        exclude: Option<Vec<String>>,
        #[arg(long, value_enum, default_value = "held_out")]
        terms: audit::Terms,
        #[arg(long, default_value = ".")]
        repo_root: PathBuf,
        #[arg(long,default_value=DATASET)]
        dataset: PathBuf,
        #[arg(long,default_value=SPLIT)]
        split: PathBuf,
    },
    /// Detect checkout or pulled-image drift (exit 1); input/environment errors exit 2.
    PinCheck {
        checkout: PathBuf,
        #[arg(long,default_value=DATASET)]
        dataset: PathBuf,
        #[arg(long)]
        images: bool,
        #[arg(long)]
        require_pulled: bool,
        /// Limit image checks (repeatable); checkout checks always cover every task.
        #[arg(long = "task")]
        tasks: Vec<String>,
    },
    /// Regenerate split.json; check the committed bytes by default (exit 1 on drift).
    Split {
        #[arg(long,default_value=DATASET)]
        dataset: PathBuf,
        #[arg(long,default_value=SPLIT)]
        split: PathBuf,
        #[arg(long)]
        write: bool,
        /// Explicit spelling of the default check operation.
        #[arg(long, conflicts_with = "write")]
        check: bool,
    },
    /// Re-pin dataset.toml from a checkout and linux/amd64 registry manifests.
    RefreshDataset {
        checkout: PathBuf,
        /// Captured JSON mapping image tag to {digest, bytes}; skips Docker/network.
        #[arg(long)]
        manifests: Option<PathBuf>,
        #[arg(long,default_value=DATASET)]
        out: PathBuf,
    },
    /// Read-only readiness checks of a remote oracle run: pin, hosts, Docker, Compose, platform and
    /// the laptop's disk and memory. Exit 20 preflight, 21 pin check, 23 ssh.
    Preflight {
        /// `RunnerPlanV1` JSON (no credentials).
        #[arg(long)]
        plan: PathBuf,
    },
    /// Run an oracle plan on the remote Docker host: pull the pinned images, run bounded trials,
    /// collect evidence and tear down. Needs no Harbor, uv or Python. Exit 0 when every trial
    /// passed cleanly, 2 when one did not.
    Run {
        #[arg(long)]
        plan: PathBuf,
    },
    /// Print this binary's runner build stamp (source manifest and evaluator digest) as JSON, for
    /// the daemon's admission check. Exit 1 when a stamped source was missing at build time.
    Stamp,
}
fn execute(command: Command, out: &mut dyn io::Write, err: &mut dyn io::Write) -> Result<u8> {
    match command {
        Command::LeakAudit {
            checkout,
            diff,
            files,
            exclude,
            terms,
            repo_root,
            dataset,
            split,
        } => {
            let items = if let Some(diff) = diff {
                let mut text = String::new();
                if diff.as_os_str() == "-" {
                    io::stdin().read_to_string(&mut text)?;
                } else {
                    text = fs::read_to_string(diff)?;
                }
                audit::added_lines(&text)
            } else {
                audit::file_lines(&files)?
            };
            let excludes =
                exclude.unwrap_or_else(|| audit::DEFAULT_EXCLUDES.map(str::to_owned).to_vec());
            let findings = audit::audit_items(
                &repo_root,
                &dataset,
                &split,
                checkout.as_deref(),
                terms,
                &items,
                &excludes,
            )?;
            for finding in &findings {
                writeln!(out, "{finding}")?;
            }
            for (label, what) in [("HIT", "held-out"), ("GENERALITY", "generality")] {
                let count = findings.iter().filter(|f| f.label == label).count();
                if count > 0 {
                    writeln!(err, "leak_audit: {count} {what} hit(s)")?;
                }
            }
            if findings.is_empty() {
                writeln!(out, "leak_audit: clean")?;
                Ok(0)
            } else {
                Ok(1)
            }
        }
        Command::PinCheck {
            checkout,
            dataset,
            images,
            require_pulled,
            tasks,
        } => {
            let doc = dataset::load(&dataset)?;
            let mut drift = pin::checkout(&checkout, &doc.dataset.commit, &doc.task)?;
            if images {
                drift.extend(pin::images(
                    &doc.task,
                    &tasks,
                    require_pulled,
                    pin::image_repo_digests,
                )?);
            }
            for line in &drift {
                writeln!(out, "DRIFT {line}")?;
            }
            if drift.is_empty() {
                writeln!(
                    out,
                    "pin_check: ok ({} tasks at {})",
                    doc.task.len(),
                    doc.dataset.commit.chars().take(8).collect::<String>()
                )?;
                Ok(0)
            } else {
                writeln!(
                    err,
                    "pin_check: {} drift(s) from {}",
                    drift.len(),
                    dataset.display()
                )?;
                Ok(1)
            }
        }
        Command::Split {
            dataset,
            split,
            write,
            check: _,
        } => {
            let doc = dataset::load(&dataset)?;
            let text = split::make_split(&doc.task, 3135)?.text()?;
            if write {
                fs::write(split, text)?;
                Ok(0)
            } else if fs::read_to_string(&split)? == text {
                Ok(0)
            } else {
                writeln!(
                    err,
                    "drift: {} differs from the split regenerated from {}",
                    split.display(),
                    dataset.display()
                )?;
                Ok(1)
            }
        }
        Command::RefreshDataset {
            checkout,
            manifests,
            out,
        } => {
            let manifests = manifests
                .map(|p| -> Result<_> { Ok(serde_json::from_str(&fs::read_to_string(p)?)?) })
                .transpose()?;
            let rows = dataset::read_tasks(&checkout, manifests.as_ref())?;
            let split = split::make_split(&rows, 3135)?;
            fs::write(
                out,
                dataset::render(&rows, &split::plan_shards(&rows, &split)),
            )?;
            Ok(0)
        }
        Command::Preflight { plan } => {
            Ok(crate::tb2_runner::cli::preflight_command(&plan, out, err))
        }
        Command::Run { plan } => Ok(crate::tb2_runner::cli::run_command(&plan, out, err)),
        Command::Stamp => Ok(crate::tb2_runner::stamp::stamp_command(out, err)),
    }
}
/// CLI arguments after `rsi-eval bench tb2`. Audit/pin input errors exit 2;
/// split/refresh failures exit 1, preserving the previous tools' exit codes.
pub fn run_cli(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let cli = Cli::parse_from(std::iter::once(OsString::from("rsi-eval bench tb2")).chain(args));
    let (error_code, error_label) = match &cli.command {
        Command::LeakAudit { .. } => (2, "leak_audit error"),
        Command::PinCheck { .. } => (2, "pin_check error"),
        Command::Split { .. } | Command::RefreshDataset { .. } => (1, "tb2 error"),
        Command::Preflight { .. } | Command::Run { .. } | Command::Stamp => {
            (20, "tb2_runner error")
        }
    };
    match execute(
        cli.command,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    ) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("{error_label}: {e}");
            ExitCode::from(error_code)
        }
    }
}
