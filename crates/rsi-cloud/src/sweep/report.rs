use super::{FAILURE, files, read_lossy, summary};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::Path;
use std::sync::LazyLock;

static CARGO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored").unwrap()
});
static CARGO_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^test (.+?) \.\.\. FAILED$").unwrap());
static SIGNAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(signal: \d+, SIG[A-Z0-9]+").unwrap());
static FLAKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bFLAKE\b|\bflaky\b").unwrap());

/// Human-readable report, without its final print newline.
pub fn report(
    sha: &str,
    started: &str,
    finished: &str,
    result: &Path,
    seed: &Path,
) -> Result<String, String> {
    let seeds: BTreeSet<String> = if seed.exists() {
        crate::util::splitlines(&read_lossy(seed)?)
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        BTreeSet::new()
    };
    let mut failures = BTreeSet::new();
    let mut classes = BTreeMap::new();
    let mut flakes = BTreeSet::new();
    let mut rows = Vec::new();
    // Sort by status file path (not by the label stored inside that file).
    for file in files(&result.join("status"), ".tsv")? {
        let text = read_lossy(&file)?;
        let fields: Vec<_> = text.trim().split('\t').collect();
        if fields.len() != 2 {
            return Err("invalid status row".into());
        }
        let (label, code) = (fields[0], fields[1]);
        let exit = code.parse::<i64>().map_err(|e| e.to_string())?;
        let log = read_lossy(&result.join("logs").join(format!("{label}.log")))?;
        let (_, passed, failed, skipped) = summary(&log).unwrap_or_else(|| {
            let (mut passed, mut failed, mut skipped) = (0, 0, 0);
            for m in CARGO.captures_iter(&log) {
                passed += m[1].parse::<i64>().unwrap();
                failed += m[2].parse::<i64>().unwrap();
                skipped += m[3].parse::<i64>().unwrap();
            }
            (0, passed, failed, skipped)
        });
        let mut names: BTreeSet<String> = CARGO_NAME
            .captures_iter(&log)
            .map(|m| m[1].to_owned())
            .collect();
        for m in FAILURE.captures_iter(&log) {
            names.insert(m[2].to_owned());
            if &m[1] != "FAIL" {
                classes.insert(
                    m[2].to_owned(),
                    if &m[1] == "TIMEOUT" {
                        "timeout"
                    } else {
                        "crash"
                    },
                );
            }
        }
        if exit != 0 && names.is_empty() {
            if SIGNAL.is_match(&log) {
                let name = format!("{label}:crash");
                failures.insert(name.clone());
                classes.insert(name, "crash");
            } else {
                failures.insert(format!("{label}:build_or_harness_failure"));
            }
        }
        failures.extend(names);
        flakes.extend(
            crate::util::splitlines(&log)
                .into_iter()
                .filter(|line| FLAKE.is_match(line))
                .map(|s| s.trim().to_owned()),
        );
        rows.push(format!(
            "| {label} | {passed} | {failed} | {skipped} | {code} |"
        ));
    }
    let start = crate::util::parse_time(started).ok_or("invalid started timestamp")?;
    let end = crate::util::parse_time(finished).ok_or("invalid finished timestamp")?;
    let elapsed = (end - start)
        .num_microseconds()
        .ok_or("timestamp interval too large")? as f64
        / 60_000_000.0;
    let mut text = format!(
        "# Cloud QA sweep — {sha}\n\nTip SHA: `{sha}`  \nStarted: {started}  \nFinished: {finished}  \nWall: {elapsed:.1} min  \nLanes: {}  \nFailing names needing signature review: {}\n\n| Lane | Pass | Fail | Skip | Exit |\n| --- | ---: | ---: | ---: | ---: |\n",
        rows.len(),
        failures.len()
    );
    for row in rows {
        writeln!(text, "{row}").unwrap();
    }
    writeln!(
        text,
        "\nSeed set: {}",
        if seed.exists() {
            seed.display().to_string()
        } else {
            "absent; classification unavailable".into()
        }
    )
    .unwrap();
    if seed.exists() {
        writeln!(
            text,
            "Names absent from seed set: {}; signatures still require review",
            failures.difference(&seeds).count()
        )
        .unwrap();
    }
    writeln!(
        text,
        "Unclassified failing names (seed names alone do not verify failure signatures):"
    )
    .unwrap();
    for name in &failures {
        writeln!(text, "- `{name}`").unwrap();
    }
    if failures.is_empty() {
        writeln!(text, "- none").unwrap();
    }
    writeln!(text, "FLAKE lines:").unwrap();
    for line in &flakes {
        writeln!(
            text,
            "- `{}`",
            line.replace('`', "").chars().take(300).collect::<String>()
        )
        .unwrap();
    }
    if flakes.is_empty() {
        writeln!(text, "- none observed").unwrap();
    }
    writeln!(text, "Crash and timeout classes:").unwrap();
    for (name, kind) in &classes {
        writeln!(text, "- `{name}` [{kind}]").unwrap();
    }
    if classes.is_empty() {
        writeln!(text, "- none").unwrap();
    }
    text.pop();
    Ok(text)
}
