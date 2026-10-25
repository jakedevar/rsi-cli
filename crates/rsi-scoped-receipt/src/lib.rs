//! The `scoped_test` receipt contract (#3621, Epic #3482 slice A).
//!
//! A worker's verification tool (today the Python `scripts/scoped-test`, later
//! the Rust runner of `crates/rsi-repo-guards`, #423) ends its log with one
//! `SCOPED_TEST_RECEIPT <json>` line. The daemon stores that object as the job's
//! typed receipt, and `AgentCheckLandingCandidate`, topology verification and
//! review read their decisions from it. Replacing the tool must not change any
//! of those decisions, so this crate states what they read:
//!
//! - [`contract::FIELDS`]: every field a daemon decision reads, its JSON shape,
//!   who writes it (the runner, or the daemon after the run) and its readers;
//! - [`contract::check`]: whether a receipt conforms to that table;
//! - [`contract::consumer_projection`] and [`contract::compare`]: the part of a
//!   receipt two runners must agree on for the same inputs, and the differences;
//! - [`parse_line`]: the receipt line the daemon reads from a job log;
//! - [`corpus`]: the conformance corpus, receipts the Python runner printed in
//!   real `scoped_test` jobs, captured by a job (never written by hand).
//!
//! The crate depends on `serde_json` alone and builds on Linux, macOS and
//! Windows. Plan: `thoughts/shared/plans/2026-10-22-scoped-test-rust-port.md`.

pub mod contract;
pub mod corpus;

use serde_json::Value;

/// The receipt schema this contract describes (the receipt's `schema` field).
pub const SCHEMA: u64 = 1;

/// The prefix of the receipt line (#1638): `SCOPED_TEST_RECEIPT <compact JSON>`.
pub const LINE_PREFIX: &str = "SCOPED_TEST_RECEIPT ";

/// The receipt a `scoped_test` run printed: the last `SCOPED_TEST_RECEIPT ` line
/// of `log`, when its JSON is an object.
///
/// The last line wins even when it does not parse, exactly as the daemon reads
/// a job log (`agent_jobs::parse_scoped_receipt_line`): a runner prints progress
/// receipts and the final one last, so an earlier line is never the verdict.
#[must_use]
pub fn parse_line(log: &str) -> Option<Value> {
    log.lines()
        .rev()
        .find_map(|line| line.trim_end().strip_prefix(LINE_PREFIX))
        .and_then(|json| serde_json::from_str::<Value>(json).ok())
        .filter(Value::is_object)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_last_receipt_line_is_the_receipt() {
        let log = "noise\nSCOPED_TEST_RECEIPT {\"ok\":false}\nscoped-test: rsid passed\nSCOPED_TEST_RECEIPT {\"ok\":true}  \ntail\n";
        assert_eq!(parse_line(log).unwrap()["ok"], true);
    }

    #[test]
    fn a_broken_last_line_is_no_receipt_even_after_a_good_one() {
        let log = "SCOPED_TEST_RECEIPT {\"ok\":true}\nSCOPED_TEST_RECEIPT {\"ok\":\n";
        assert_eq!(parse_line(log), None);
        assert_eq!(parse_line("SCOPED_TEST_RECEIPT [1]\n"), None);
        assert_eq!(parse_line("no receipt here\n"), None);
    }
}
