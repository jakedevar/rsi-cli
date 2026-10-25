//! #3191: resolve a bench run config and its run overlay without calling a model.
//!
//! `rsi-bench-resolve <config> [--repo <dir>]` reads the committed run config
//! (`eval/bench/tb2/runs/*.toml` or `eval/bench/tb2/candidates/<id>/run.toml`) and
//! the overlay it names from the worktree (the current directory by default) and
//! prints, as JSON, the effective prompt digest, the tool list and the loop knobs a
//! run would launch with. It opens no run, starts no session and spends nothing.

use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut config = None;
    let mut repo = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => {
                repo = Some(PathBuf::from(
                    args.next().ok_or("--repo needs a directory")?,
                ))
            }
            _ if config.is_none() && !argument.starts_with('-') => config = Some(argument),
            _ => {
                return Err(format!(
                    "unexpected argument `{argument}`; usage: rsi-bench-resolve <config> [--repo <dir>]"
                ));
            }
        }
    }
    let config = config.ok_or("usage: rsi-bench-resolve <config> [--repo <dir>]")?;
    let repo = match repo {
        Some(repo) => repo,
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    };
    let report = rsid::agent_jobs::bench_dry_run(&repo, &config)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
    );
    Ok(())
}
