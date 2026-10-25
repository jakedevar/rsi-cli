//! Cloud sweep v1 artifacts. The JSON bytes, QA.md and final VERDICT line are
//! shared contracts with the rolling lander and the daemon's cloud_sweep job.
mod artifact;
mod report;
mod verdict;
mod wire;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

pub use artifact::{JsonSource, json_report, pretty_json};
pub use report::report;
pub use verdict::{Classification, build_verdict, verdict};
pub use wire::{legacy_json, sha256};

static SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Summary \[[^\n]*\] (\d+) tests run: (\d+) passed[^\n]*?, (\d+) skipped").unwrap()
});
static FAILURE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(FAIL|TIMEOUT|SIG[A-Z0-9]+) \[[^\]]*\] \([^)]*\) \S+ (.+)$").unwrap()
});

fn summary(log: &str) -> Option<(i64, i64, i64, i64)> {
    SUMMARY.captures_iter(log).last().map(|m| {
        let run = m[1].parse::<i64>().unwrap();
        let passed = m[2].parse::<i64>().unwrap();
        (run, passed, run - passed, m[3].parse().unwrap())
    })
}

pub fn failures(log: &str) -> Vec<String> {
    FAILURE
        .captures_iter(log)
        .map(|m| m[2].to_owned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn read_lossy(path: &Path) -> Result<String, String> {
    std::fs::read(path)
        .map(|b| {
            String::from_utf8_lossy(&b)
                .replace("\r\n", "\n")
                .replace('\r', "\n")
        })
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn status_rows(result: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut rows = BTreeMap::new();
    for path in files(&result.join("status"), ".tsv")? {
        let text = read_lossy(&path)?;
        let fields: Vec<_> = text.trim().split('\t').collect();
        if fields.len() != 2 || fields[1].parse::<i64>().is_err() {
            return Err(format!("{}: invalid status row", path.display()));
        }
        rows.insert(fields[0].to_owned(), fields[1].to_owned());
    }
    Ok(rows)
}

fn files(dir: &Path, suffix: &str) -> Result<Vec<std::path::PathBuf>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path
            .file_name()
            .is_some_and(|s| s.to_string_lossy().ends_with(suffix))
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn run(command: &str, args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match command {
        "sweep-report" if args.len() == 5 => report(
            &args[0],
            &args[1],
            &args[2],
            Path::new(&args[3]),
            Path::new(&args[4]),
        ),
        "sweep-json" if args.len() == 5 => artifact::from_process(args),
        "sweep-verdict" => {
            return verdict::run(args, out, err);
        }
        _ => {
            let _ = writeln!(
                err,
                "usage: rsi-cloud {command} SHA STARTED FINISHED RESULT_DIR {}",
                if command == "sweep-json" {
                    "RUNNER"
                } else {
                    "SEED_FILE"
                }
            );
            return 1;
        }
    };
    match result {
        Ok(text) => match writeln!(out, "{text}") {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(err, "{e}");
                1
            }
        },
        Err(e) => {
            let _ = writeln!(err, "{e}");
            1
        }
    }
}
