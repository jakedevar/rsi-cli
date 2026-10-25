//! The conformance corpus: receipts the Python runner printed in real
//! `scoped_test` jobs, one per shape class (#3621).
//!
//! Goldens are never written by hand. The recipe job `scoped-receipt-corpus`
//! (`.rsi/jobs.toml`, `make scoped-receipt-corpus`) reads the stored receipts
//! from the daemon's job table, read-only, and pipes them to
//! `rsi-scoped-receipt corpus`, which keeps the newest receipt of each
//! [`shape_class`] that fits [`MAX_RECEIPT_BYTES`], replaces the operator's home
//! directory with `~`, drops a receipt that still names the account, and writes
//! `tests/golden/receipts.jsonl`. Every golden must conform to the contract
//! (`contract::check`); a Rust runner's receipt must conform the same way.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The largest receipt the corpus keeps (bytes of its compact JSON, after
/// sanitizing): large plans inline their whole selection, which says nothing
/// more about the contract.
pub const MAX_RECEIPT_BYTES: usize = 16_000;

/// The values daemon decisions branch on, as one key.
///
/// They are the verdict, the exit code (0, 1 or other), the plan,
/// `new_failures` (absent, null, empty, some), baseline reds, the trial merge,
/// a derivation failure and the warning gate.
#[must_use]
pub fn shape_class(receipt: &Value) -> String {
    let flag = |key: &str| match receipt.get(key) {
        Some(Value::Bool(value)) => value.to_string(),
        Some(Value::Null) => "null".into(),
        Some(_) => "other".into(),
        None => "absent".into(),
    };
    let exit = match receipt.get("exit_code").and_then(Value::as_i64) {
        Some(0) => "0",
        Some(1) => "1",
        Some(_) => "other",
        None => "absent",
    };
    let new_failures = match receipt.get("new_failures") {
        None => "absent",
        Some(Value::Null) => "null",
        Some(Value::Array(list)) if list.is_empty() => "empty",
        Some(Value::Array(_)) => "some",
        Some(_) => "other",
    };
    let reds = receipt
        .get("baseline_reds")
        .and_then(Value::as_array)
        .is_some_and(|list| !list.is_empty());
    let derive = receipt
        .get("derive_failure")
        .is_some_and(|value| !value.is_null());
    format!(
        "ok={};exit={exit};plan={};new_failures={new_failures};baseline_reds={};merge={};derive_failure={};warnings={}",
        flag("ok"),
        receipt.get("plan").and_then(Value::as_str).unwrap_or("-"),
        if reds { "some" } else { "none" },
        flag("merges_cleanly_onto_rolling"),
        if derive { "yes" } else { "no" },
        receipt
            .pointer("/warnings/status")
            .and_then(Value::as_str)
            .unwrap_or("-"),
    )
}

/// `text` with the home directory `home` replaced by `~`, or `None` when a
/// `forbidden` word (the account name, at least three characters, compared
/// without case) is still in it afterwards.
#[must_use]
pub fn sanitize(text: &str, home: Option<&str>, forbidden: &[String]) -> Option<String> {
    let home = home.map(|home| home.trim_end_matches('/'));
    let text = match home {
        Some(home) if home.len() > 1 => text.replace(home, "~"),
        _ => text.to_owned(),
    };
    let lower = text.to_lowercase();
    forbidden
        .iter()
        .filter(|word| word.chars().count() >= 3)
        .all(|word| !lower.contains(&word.to_lowercase()))
        .then_some(text)
}

/// One golden: a stored receipt and the job that printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Golden {
    pub class: String,
    pub job_id: String,
    pub created_at: String,
    pub receipt: Value,
}

impl Golden {
    /// The corpus line: `{"class", "created_at", "job_id", "receipt"}`.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut line = Map::new();
        line.insert("class".into(), Value::String(self.class.clone()));
        line.insert("created_at".into(), Value::String(self.created_at.clone()));
        line.insert("job_id".into(), Value::String(self.job_id.clone()));
        line.insert("receipt".into(), self.receipt.clone());
        Value::Object(line).to_string()
    }

    /// A corpus line read back.
    ///
    /// # Errors
    /// A line that is not a corpus object, naming what is missing.
    pub fn from_line(line: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("corpus line without a `{key}` string"))
        };
        Ok(Self {
            class: text("class")?,
            job_id: text("job_id")?,
            created_at: text("created_at")?,
            receipt: value
                .get("receipt")
                .filter(|receipt| receipt.is_object())
                .cloned()
                .ok_or("corpus line without a `receipt` object")?,
        })
    }
}

/// What one capture read and kept.
#[derive(Debug, Default)]
pub struct Capture {
    /// The newest eligible receipt of each class, ordered by class.
    pub goldens: Vec<Golden>,
    pub rows: usize,
    /// Rows without three tab-separated fields or a JSON object receipt.
    pub unparsed: usize,
    /// Rows dropped because a forbidden word survived sanitizing.
    pub identity: usize,
    /// Rows over [`MAX_RECEIPT_BYTES`].
    pub oversize: usize,
    /// Classes seen, including those with no eligible receipt.
    pub classes: usize,
}

/// Select the corpus from rows `job id<TAB>created_at<TAB>receipt JSON`.
///
/// One row per line: the stored receipts of `scoped_test` jobs. The newest
/// eligible receipt of each class wins (a later `created_at`, then a greater
/// job id).
#[must_use]
pub fn capture(rows: &str, home: Option<&str>, forbidden: &[String]) -> Capture {
    let mut capture = Capture::default();
    let mut classes = BTreeMap::<String, Option<Golden>>::new();
    for row in rows.lines().filter(|row| !row.trim().is_empty()) {
        capture.rows += 1;
        let mut fields = row.splitn(3, '\t');
        let (Some(job_id), Some(created_at), Some(json)) =
            (fields.next(), fields.next(), fields.next())
        else {
            capture.unparsed += 1;
            continue;
        };
        let Some(receipt) = serde_json::from_str::<Value>(json)
            .ok()
            .filter(Value::is_object)
        else {
            capture.unparsed += 1;
            continue;
        };
        let class = shape_class(&receipt);
        let slot = classes.entry(class.clone()).or_default();
        let Some(clean) = sanitize(&receipt.to_string(), home, forbidden) else {
            capture.identity += 1;
            continue;
        };
        if clean.len() > MAX_RECEIPT_BYTES {
            capture.oversize += 1;
            continue;
        }
        let Ok(receipt) = serde_json::from_str::<Value>(&clean) else {
            capture.unparsed += 1;
            continue;
        };
        let newer = slot.as_ref().is_none_or(|kept| {
            (created_at, job_id) > (kept.created_at.as_str(), kept.job_id.as_str())
        });
        if newer {
            *slot = Some(Golden {
                class,
                job_id: job_id.trim().to_owned(),
                created_at: created_at.trim().to_owned(),
                receipt,
            });
        }
    }
    capture.classes = classes.len();
    capture.goldens = classes.into_values().flatten().collect();
    capture
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::contract::{Origin, check, compare, consumer_projection};
    use serde_json::json;

    /// The corpus the `scoped-receipt-corpus` recipe job captured.
    const GOLDEN: &str = include_str!("../tests/golden/receipts.jsonl");

    fn goldens() -> Vec<Golden> {
        GOLDEN
            .lines()
            .map(|line| Golden::from_line(line).unwrap())
            .collect()
    }

    #[test]
    fn every_captured_python_receipt_conforms_to_the_contract() {
        let goldens = goldens();
        assert!(goldens.len() >= 20, "{} goldens", goldens.len());
        for golden in &goldens {
            let problems: Vec<String> = check(&golden.receipt, Origin::Stored)
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(problems, Vec::<String>::new(), "job {}", golden.job_id);
            // The projection is a conforming runner receipt and a fixed point.
            let projection = consumer_projection(&golden.receipt);
            assert_eq!(
                check(&projection, Origin::Runner),
                Vec::new(),
                "job {}",
                golden.job_id
            );
            assert_eq!(
                compare(&golden.receipt, &projection),
                Vec::new(),
                "job {}",
                golden.job_id
            );
        }
    }

    #[test]
    fn the_corpus_holds_one_receipt_per_class_in_class_order() {
        let goldens = goldens();
        for golden in &goldens {
            assert_eq!(
                shape_class(&golden.receipt),
                golden.class,
                "job {}",
                golden.job_id
            );
            assert_eq!(Golden::from_line(&golden.to_line()).unwrap(), *golden);
        }
        let classes: Vec<&str> = goldens.iter().map(|golden| golden.class.as_str()).collect();
        let mut sorted = classes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(classes, sorted);
    }

    #[test]
    fn the_corpus_covers_the_verdicts_the_daemon_branches_on() {
        let classes: Vec<String> = goldens().into_iter().map(|golden| golden.class).collect();
        for wanted in [
            "ok=true;exit=0;",
            "ok=false;exit=1;",
            "new_failures=null;",
            "new_failures=some;",
            "baseline_reds=some;",
            "merge=false;",
            "plan=targeted_subset;",
            "derive_failure=yes;",
        ] {
            assert!(
                classes.iter().any(|class| class.contains(wanted)),
                "no golden of class {wanted}"
            );
        }
    }

    fn row(job: &str, at: &str, receipt: &Value) -> String {
        format!("{job}\t{at}\t{receipt}\n")
    }

    #[test]
    fn capture_keeps_the_newest_eligible_receipt_of_each_class_and_counts_the_rest() {
        let green = json!({"schema": 1, "ok": true, "exit_code": 0, "log_dir": "/home/op/x"});
        let red = json!({"schema": 1, "ok": false, "exit_code": 1, "new_failures": []});
        let big =
            json!({"schema": 1, "ok": true, "exit_code": 0, "pad": "x".repeat(MAX_RECEIPT_BYTES)});
        let named = json!({"schema": 1, "ok": false, "exit_code": 1, "new_failures": [], "who": "OPerator"});
        let rows = [
            row("a", "2026-10-01T00:00:00Z", &green),
            row("b", "2026-10-03T00:00:00Z", &green),
            row("c", "2026-10-02T00:00:00Z", &green),
            row("d", "2026-10-09T00:00:00Z", &big),
            row("e", "2026-10-01T00:00:00Z", &red),
            row("f", "2026-10-05T00:00:00Z", &named),
            "g\tno tabs left".to_owned(),
            "\n".to_owned(),
        ]
        .concat();
        let capture = capture(&rows, Some("/home/op/"), &["operator".to_owned()]);
        assert_eq!(
            (
                capture.rows,
                capture.unparsed,
                capture.identity,
                capture.oversize,
                capture.classes
            ),
            (7, 1, 1, 1, 2)
        );
        let kept: Vec<(&str, &str)> = capture
            .goldens
            .iter()
            .map(|golden| {
                (
                    golden.job_id.as_str(),
                    golden.receipt["log_dir"].as_str().unwrap_or("-"),
                )
            })
            .collect();
        assert_eq!(kept, [("e", "-"), ("b", "~/x")]);
        assert_eq!(capture.goldens[1].class, shape_class(&green));
    }

    #[test]
    fn sanitize_replaces_the_home_directory_and_refuses_a_surviving_account_name() {
        let words = ["operator".to_owned(), "x".to_owned()];
        assert_eq!(
            sanitize("/home/op/a /home/op", Some("/home/op/"), &words).as_deref(),
            Some("~/a ~")
        );
        assert_eq!(sanitize("/srv/Operator/a", Some("/home/op"), &words), None);
        assert_eq!(sanitize("plain", None, &[]).as_deref(), Some("plain"));
    }

    #[test]
    fn the_shape_class_names_each_decision_input() {
        let receipt = json!({
            "ok": false, "exit_code": 124, "plan": "targeted_subset", "new_failures": null,
            "baseline_reds": [{"test": "a"}], "merges_cleanly_onto_rolling": false,
            "derive_failure": ["no merge"], "warnings": {"status": "new_warnings"}
        });
        assert_eq!(
            shape_class(&receipt),
            "ok=false;exit=other;plan=targeted_subset;new_failures=null;baseline_reds=some;merge=false;derive_failure=yes;warnings=new_warnings"
        );
        assert_eq!(
            shape_class(&json!({})),
            "ok=absent;exit=absent;plan=-;new_failures=absent;baseline_reds=none;merge=absent;derive_failure=no;warnings=-"
        );
    }
}
