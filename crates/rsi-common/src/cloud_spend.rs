//! Remote-gate spend report and caps (Issue #1036, follow-up to #1010 S1).
//!
//! The operator owns two dollar caps as daemon settings
//! (`cloud_spend_stop_line_usd`, `cloud_spend_daily_cap_usd`). rsid mirrors
//! them into `spend-caps.json` next to the ledger so `scripts/cloud-spend.py`
//! (called by `scripts/cloud-gate.sh`) reads the operator's value instead of a
//! hard-coded one. This module parses the human-written ledger
//! (`~/.rsi/cloud/spend.md`) into the per-run and per-day view the operator
//! RPC (`GetCloudSpend`) and the Settings row show. `scripts/cloud-spend.py`
//! parses the same ledger with the same rules.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Cumulative estimate (whole USD) at which a new remote run is refused.
pub const DEFAULT_STOP_LINE_USD: u64 = 90;
/// Estimate (whole USD) per UTC day at which a new remote run is refused.
/// Matches the `rsi-cloud-us-west-1-daily` AWS Budget backstop.
pub const DEFAULT_DAILY_CAP_USD: u64 = 15;
/// Upper bound accepted for either cap.
pub const MAX_CAP_USD: u64 = 100_000;
/// File the daemon writes beside the ledger; the scripts read it.
pub const CAPS_FILE_NAME: &str = "spend-caps.json";
/// The ledger file name inside the cloud directory.
pub const LEDGER_FILE_NAME: &str = "spend.md";
/// Most recent runs returned by the report (the totals cover every run).
pub const REPORT_RUN_LIMIT: usize = 20;

/// One ledger line that carries an `est compute $X` figure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudSpendRun {
    /// Text before the first colon, e.g. `Gate window i-0abc`.
    pub label: String,
    /// UTC day the run is charged to (`YYYY-MM-DD`), when the line has a timestamp.
    pub date: Option<String>,
    pub est_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudSpendDay {
    pub date: String,
    pub est_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudSpendReport {
    pub ledger_found: bool,
    pub grant_usd: Option<f64>,
    /// The stop line in the ledger header, kept for information.
    pub header_stop_line_usd: Option<f64>,
    /// The operator's effective caps (daemon settings).
    pub stop_line_usd: u64,
    pub daily_cap_usd: u64,
    pub spent_usd: f64,
    /// UTC day used for `today_usd`.
    pub today: String,
    pub today_usd: f64,
    pub runs_total: usize,
    /// Most recent runs, oldest first, at most `REPORT_RUN_LIMIT`.
    pub runs: Vec<CloudSpendRun>,
    pub days: Vec<CloudSpendDay>,
    pub stop_line_reached: bool,
    pub daily_cap_reached: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudSpendCaps {
    pub stop_line_usd: u64,
    pub daily_cap_usd: u64,
}

/// The JSON the daemon writes to `spend-caps.json`.
pub fn caps_file_json(caps: &CloudSpendCaps) -> String {
    let mut text = serde_json::to_string(caps).expect("caps serialize");
    text.push('\n');
    text
}

fn dollars_after(text: &str, marker: &str) -> Option<f64> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let rest = rest.trim_start().strip_prefix('$')?;
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// First `YYYY-MM-DD` date that follows `stop ` on the line, else the first
/// date on the line.
fn line_date(line: &str) -> Option<NaiveDate> {
    fn date_at(text: &str) -> Option<NaiveDate> {
        let bytes = text.as_bytes();
        (0..bytes.len().saturating_sub(9)).find_map(|start| {
            let candidate = text.get(start..start + 10)?;
            let time = text.get(start + 10..start + 11)?;
            if time != "T" && time != " " {
                return None;
            }
            NaiveDate::parse_from_str(candidate, "%Y-%m-%d").ok()
        })
    }
    line.find("stop ")
        .and_then(|at| date_at(&line[at..]))
        .or_else(|| date_at(line))
}

fn run_of(line: &str) -> Option<CloudSpendRun> {
    let mut est = 0.0;
    let mut cursor = line;
    let mut found = false;
    while let Some(at) = cursor.find("est compute") {
        cursor = &cursor[at..];
        if let Some(usd) = dollars_after(cursor, "est compute") {
            est += usd;
            found = true;
        }
        cursor = &cursor["est compute".len()..];
    }
    found.then(|| CloudSpendRun {
        label: line.split(':').next().unwrap_or(line).trim().to_string(),
        date: line_date(line).map(|d| d.to_string()),
        est_usd: est,
    })
}

/// Build the report from the ledger text (`None` when the file is missing).
pub fn build_report(
    ledger: Option<&str>,
    caps: &CloudSpendCaps,
    today: NaiveDate,
) -> CloudSpendReport {
    let text = ledger.unwrap_or("");
    let runs: Vec<CloudSpendRun> = text.lines().filter_map(run_of).collect();
    let spent_usd: f64 = runs.iter().map(|run| run.est_usd).sum();
    let mut by_day: BTreeMap<String, f64> = BTreeMap::new();
    for run in &runs {
        if let Some(date) = &run.date {
            *by_day.entry(date.clone()).or_default() += run.est_usd;
        }
    }
    let today_key = today.to_string();
    let today_usd = by_day.get(&today_key).copied().unwrap_or(0.0);
    let skip = runs.len().saturating_sub(REPORT_RUN_LIMIT);
    CloudSpendReport {
        ledger_found: ledger.is_some(),
        grant_usd: dollars_after(text, "Operator grant:"),
        header_stop_line_usd: dollars_after(text, "Stop and report by"),
        stop_line_usd: caps.stop_line_usd,
        daily_cap_usd: caps.daily_cap_usd,
        spent_usd,
        today: today_key,
        today_usd,
        runs_total: runs.len(),
        runs: runs.into_iter().skip(skip).collect(),
        days: by_day
            .into_iter()
            .map(|(date, est_usd)| CloudSpendDay { date, est_usd })
            .collect(),
        stop_line_reached: spent_usd >= caps.stop_line_usd as f64,
        daily_cap_reached: today_usd >= caps.daily_cap_usd as f64,
    }
}

/// One-line summary for the Settings row.
pub fn summary_line(report: &CloudSpendReport) -> String {
    if !report.ledger_found {
        return "no spend ledger".to_string();
    }
    let last = report
        .runs
        .last()
        .map(|run| format!(" · last run ${:.2}", run.est_usd))
        .unwrap_or_default();
    let flag = if report.stop_line_reached {
        " · STOP LINE REACHED"
    } else if report.daily_cap_reached {
        " · DAILY CAP REACHED"
    } else {
        ""
    };
    format!(
        "today ${:.2} of ${} · total ${:.2} of ${}{last}{flag}",
        report.today_usd, report.daily_cap_usd, report.spent_usd, report.stop_line_usd
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEDGER: &str = "# AWS satellite spend ledger\n\n\
Operator grant: $100 starting 2026-09-27.\n\
Configured compute estimate: $3.56/hour. Stop and report by $90 cumulative under this grant.\n\
Window 1 stop confirmed: AWS StateTransitionReason User initiated 2026-09-28 23:59:16 UTC; launch 23:50:23 UTC; est compute $0.53 at $3.56/h.\n\
Gate window i-a: start 2026-09-29T18:21:41Z, est $1.78/h\n\
Gate window i-a: stop 2026-09-29T19:23:23Z, elapsed 120 s, est compute $2.00 at $1.78/h; ok.\n\
Gate window i-b: stop 2026-09-29T21:30:00Z, est compute $1.50 at $1.78/h; ok.\n\
Gate window i-c: stop 2026-09-30T00:09:47Z, est compute $1.25 at $1.78/h; ok.\n";

    fn caps(stop: u64, daily: u64) -> CloudSpendCaps {
        CloudSpendCaps {
            stop_line_usd: stop,
            daily_cap_usd: daily,
        }
    }

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn report_shows_per_run_and_per_day_spend() {
        let report = build_report(Some(LEDGER), &caps(90, 15), day("2026-09-29"));
        assert_eq!(report.runs_total, 4);
        assert!((report.spent_usd - 5.28).abs() < 1e-9);
        assert!((report.today_usd - 3.5).abs() < 1e-9);
        assert_eq!(report.grant_usd, Some(100.0));
        assert_eq!(report.header_stop_line_usd, Some(90.0));
        assert_eq!(
            report.runs.last().map(|run| (run.label.as_str(), run.est_usd)),
            Some(("Gate window i-c", 1.25))
        );
        assert_eq!(
            report
                .days
                .iter()
                .map(|d| (d.date.as_str(), d.est_usd))
                .collect::<Vec<_>>(),
            vec![("2026-09-28", 0.53), ("2026-09-29", 3.5), ("2026-09-30", 1.25)]
        );
        assert!(!report.stop_line_reached && !report.daily_cap_reached);
    }

    #[test]
    fn operator_caps_decide_reached_flags_not_the_ledger_header() {
        let daily = build_report(Some(LEDGER), &caps(90, 3), day("2026-09-29"));
        assert!(daily.daily_cap_reached && !daily.stop_line_reached);
        let stop = build_report(Some(LEDGER), &caps(5, 15), day("2026-09-29"));
        assert!(stop.stop_line_reached && !stop.daily_cap_reached);
        assert_eq!(stop.header_stop_line_usd, Some(90.0));
    }

    #[test]
    fn summary_line_names_today_total_and_last_run() {
        let report = build_report(Some(LEDGER), &caps(90, 3), day("2026-09-29"));
        assert_eq!(
            summary_line(&report),
            "today $3.50 of $3 · total $5.28 of $90 · last run $1.25 · DAILY CAP REACHED"
        );
        assert_eq!(
            summary_line(&build_report(None, &caps(90, 15), day("2026-09-29"))),
            "no spend ledger"
        );
    }

    #[test]
    fn caps_file_round_trips() {
        let caps = caps(75, 12);
        let parsed: CloudSpendCaps = serde_json::from_str(&caps_file_json(&caps)).unwrap();
        assert_eq!(parsed, caps);
    }
}
