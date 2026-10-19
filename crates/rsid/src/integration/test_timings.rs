//! Compact, bounded nextest timing evidence retained with each landing log.

use regex::Regex;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

pub const TEST_TIMINGS_ENV: &str = "RSI_LANDER_TEST_TIMINGS";
pub const TEST_TIMINGS_HEADER: &str = "run\tbinary\ttest\tseconds\tstatus\n";
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ROWS_PER_COMMAND: usize = 20_000;

/// Append completed tests, including failures and timeouts. Nextest repeats
/// failures in its final summary, so identities are deduplicated per command.
/// A process killed before completion contributes only its completed tests.
pub fn append_test_timings(
    path: &Path,
    run: &str,
    stdout: &str,
    stderr: &str,
) -> std::io::Result<()> {
    static LINE: OnceLock<Regex> = OnceLock::new();
    static WRITER: Mutex<()> = Mutex::new(());
    let line = LINE.get_or_init(|| {
        Regex::new(
            r"^\s*([A-Z][A-Z ]*)\s+\[\s*([0-9]+(?:\.[0-9]+)?)s\]\s+\([^)]*\)\s+(\S+)\s+(\S+)\s*$",
        )
        .expect("nextest completed-test timing pattern")
    });
    if run.len() > 256 || run.chars().any(char::is_control) {
        return Ok(());
    }
    let mut rows = BTreeMap::new();
    for text in [stdout, stderr] {
        for text in text.lines() {
            let Some(captures) = line.captures(text) else {
                continue;
            };
            let (binary, test) = (
                captures.get(3).expect("binary capture").as_str(),
                captures.get(4).expect("test capture").as_str(),
            );
            if binary.len() + test.len() > 4096
                || binary.chars().chain(test.chars()).any(char::is_control)
                || !captures[2].parse::<f64>().is_ok_and(f64::is_finite)
            {
                continue;
            }
            if rows.len() < MAX_ROWS_PER_COMMAND || rows.contains_key(&(binary, test)) {
                rows.insert(
                    (binary, test),
                    (captures[2].to_owned(), captures[1].trim().to_owned()),
                );
            }
        }
    }
    let _writer = WRITER.lock().unwrap_or_else(|error| error.into_inner());
    let mut file = OpenOptions::new().append(true).open(path)?;
    let mut size = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
    for ((binary, test), (seconds, status)) in rows {
        let row = format!("{run}\t{binary}\t{test}\t{seconds}\t{status}\n");
        if size.saturating_add(row.len()) > MAX_BYTES {
            break;
        }
        file.write_all(row.as_bytes())?;
        size += row.len();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn completed_tests_are_deduplicated_with_status_and_duration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gate.timings.tsv");
        std::fs::write(&path, TEST_TIMINGS_HEADER).unwrap();
        let pass = "        PASS [   1.234s] (1/3) rsid a::green\n";
        let failure = "        FAIL [   2.500s] (2/3) rsid-store b::red\n";
        let timeout = "     TIMEOUT [ 240.001s] (3/3) rsid c::timeout\n";
        append_test_timings(
            &path,
            "candidate-0",
            &format!("noise\n{pass}{failure}SLOW [>60.000s] rsid unfinished\n"),
            &format!("{failure}{timeout}"),
        )
        .unwrap();
        append_test_timings(&path, "base-1", pass, "").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            concat!(
                "run\tbinary\ttest\tseconds\tstatus\n",
                "candidate-0\trsid\ta::green\t1.234\tPASS\n",
                "candidate-0\trsid\tc::timeout\t240.001\tTIMEOUT\n",
                "candidate-0\trsid-store\tb::red\t2.500\tFAIL\n",
                "base-1\trsid\ta::green\t1.234\tPASS\n",
            )
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn timing_sidecar_stops_at_its_byte_budget_without_a_partial_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gate.timings.tsv");
        let initial = "x".repeat(MAX_BYTES - 10);
        std::fs::write(&path, &initial).unwrap();
        append_test_timings(&path, "run", "PASS [ 1.000s] (1/1) rsid a::test\n", "").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
    }
}
