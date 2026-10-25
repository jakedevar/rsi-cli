//! hub #3852: the compact `AgentGetJob` view.
//!
//! A finished `scoped_test` job stores a receipt with per-package warning
//! tables, filter selections and tool blobs; `AgentGetJob` returned all of it
//! (about 130,000 characters), which the agent harness refuses as over its
//! token limit. `fields:"compact"` returns what a worker reads after a run
//! (verdict, head, merge state, failures, per-package status, the warnings
//! verdict) in a bounded shape. Every list is capped and reports how many
//! entries it dropped; `result.dropped` names the result keys left out, and
//! `fields:"full"` (the default) restores the whole job.

use serde_json::{Map, Value, json};

use crate::agent_jobs::{AgentJobResultV1, AgentJobV1};

/// Entries kept in each failure list (`failures`, `new_failures`,
/// `baseline_reds`, `failing_tests`).
pub const COMPACT_MAX_FAILURES: usize = 25;
/// Packages kept in the per-package table.
pub const COMPACT_MAX_PACKAGES: usize = 64;
/// Conflicting paths kept.
pub const COMPACT_MAX_CONFLICT_PATHS: usize = 25;
/// Characters kept from the end of the job log tail (`result.detail`).
pub const COMPACT_MAX_DETAIL_CHARS: usize = 2_000;
/// Characters kept from one package's compile diagnostic.
pub const COMPACT_MAX_DIAGNOSTIC_CHARS: usize = 300;
/// Characters kept from one failure entry given as plain text.
const MAX_TEXT_CHARS: usize = 300;

/// Failure-entry keys kept (the keys the receipt contract names).
const FAILURE_KEYS: &[&str] = &["package", "kind", "target", "test"];
/// Package-entry keys kept.
const PACKAGE_KEYS: &[&str] = &["package", "status", "completed", "phase"];

impl AgentJobV1 {
    /// The bounded view `AgentGetJob` returns for `fields:"compact"`.
    #[must_use]
    pub fn compact_view(&self) -> Value {
        let mut view = self.compact_header();
        view.insert("view".into(), json!("compact"));
        if let Some(result) = &self.result {
            view.insert("result".into(), compact_result(result));
        }
        Value::Object(view)
    }

    /// hub #3889: the one-line row `AgentListJobs` returns by default. It
    /// shares [`Self::compact_header`] with [`Self::compact_view`] and carries
    /// no `result` payload: the verdict fields a lister needs (`refusal`,
    /// `receipt_head`) are lifted out of it instead. `AgentGetJob` reads the
    /// whole result.
    #[must_use]
    pub fn compact_row(&self) -> Value {
        let mut row = self.compact_header();
        row.insert("wake".into(), json!(self.wake));
        if let Some(result) = &self.result {
            insert_some(&mut row, "refusal", &result.refusal);
        }
        if let Some(head) = crate::agent_landing_candidate::receipt_head(self) {
            row.insert("receipt_head".into(), json!(head));
        }
        Value::Object(row)
    }

    /// The fields every compact form of a job carries.
    fn compact_header(&self) -> Map<String, Value> {
        let mut view = Map::new();
        view.insert("id".into(), json!(self.id));
        view.insert("kind".into(), json!(self.kind));
        view.insert("name".into(), json!(self.name));
        view.insert("state".into(), json!(self.state));
        view.insert("exit_code".into(), json!(self.exit_code));
        view.insert("log_path".into(), json!(self.log_path));
        view.insert("created_at".into(), json!(self.created_at));
        view.insert("finished_at".into(), json!(self.finished_at));
        if let Some(started_at) = &self.started_at {
            view.insert("started_at".into(), json!(started_at));
        }
        if let Some(held) = &self.held {
            view.insert("held".into(), json!(held));
        }
        view
    }
}

fn compact_result(result: &AgentJobResultV1) -> Value {
    let mut out = Map::new();
    out.insert("exit_code".into(), json!(result.exit_code));
    insert_some(&mut out, "failure_class", &result.failure_class);
    insert_some(&mut out, "landed_sha", &result.landed_sha);
    insert_some(&mut out, "refusal", &result.refusal);
    insert_some(&mut out, "slot_wait_secs", &result.slot_wait_secs);
    if !result.failing_tests.is_empty() || result.failing_tests_omitted > 0 {
        let (kept, omitted) = bounded_texts(&result.failing_tests);
        out.insert("failing_tests".into(), json!(kept));
        out.insert(
            "failing_tests_omitted".into(),
            json!(omitted + u64::from(result.failing_tests_omitted)),
        );
    }
    if let Some(detail) = &result.detail {
        out.insert(
            "detail".into(),
            json!(tail_chars(detail, COMPACT_MAX_DETAIL_CHARS)),
        );
    }
    if let Some(receipt) = &result.receipt {
        out.insert("receipt".into(), compact_receipt(receipt));
    }
    if let Some(bench) = &result.bench {
        out.insert("bench".into(), json!(bench));
    }
    let dropped = dropped_result_keys(result);
    if !dropped.is_empty() {
        out.insert("dropped".into(), json!(dropped));
    }
    Value::Object(out)
}

/// The result keys present on `result` that the compact view leaves out.
fn dropped_result_keys(result: &AgentJobResultV1) -> Vec<&'static str> {
    [
        ("sweep", result.sweep.is_some()),
        ("scoped_tool", result.scoped_tool.is_some()),
        ("deploy_id", result.deploy_id.is_some()),
        ("source_head", result.source_head.is_some()),
        ("source_dirty", result.source_dirty.is_some()),
        ("target_dir", result.target_dir.is_some()),
    ]
    .into_iter()
    .filter_map(|(key, present)| present.then_some(key))
    .collect()
}

fn insert_some<T: serde::Serialize>(out: &mut Map<String, Value>, key: &str, value: &Option<T>) {
    if let Some(value) = value {
        out.insert(key.into(), json!(value));
    }
}

/// The receipt fields a worker reads after a run. Keys the receipt does not
/// carry are left out; a bounded list reports `<key>_omitted` when it was cut.
#[must_use]
pub fn compact_receipt(receipt: &Value) -> Value {
    let Some(object) = receipt.as_object() else {
        return receipt.clone();
    };
    let mut out = Map::new();
    for key in [
        "schema",
        "ok",
        "exit_code",
        "base",
        "head",
        "failure_class",
        "merges_cleanly_onto_rolling",
        "partial",
    ] {
        if let Some(value) = object.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    for key in ["failures", "new_failures", "baseline_reds"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        insert_bounded(&mut out, key, value, COMPACT_MAX_FAILURES, FAILURE_KEYS);
    }
    if let Some(value) = object.get("conflict_paths") {
        insert_bounded(
            &mut out,
            "conflict_paths",
            value,
            COMPACT_MAX_CONFLICT_PATHS,
            &[],
        );
    }
    if let Some(value) = object.get("packages") {
        insert_packages(&mut out, value);
    }
    if let Some(status) = object.get("warnings").and_then(|w| w.get("status")) {
        out.insert("warnings".into(), json!({ "status": status }));
    }
    Value::Object(out)
}

fn insert_packages(out: &mut Map<String, Value>, value: &Value) {
    let Some(items) = value.as_array() else {
        out.insert("packages".into(), value.clone());
        return;
    };
    let kept: Vec<Value> = items
        .iter()
        .take(COMPACT_MAX_PACKAGES)
        .map(|item| {
            let mut entry = keep_keys(item, PACKAGE_KEYS);
            let diagnostic = item.get("diagnostic").and_then(Value::as_str);
            if let (Some(diagnostic), Some(map)) = (diagnostic, entry.as_object_mut()) {
                let head: String = diagnostic
                    .chars()
                    .take(COMPACT_MAX_DIAGNOSTIC_CHARS)
                    .collect();
                map.insert("diagnostic".into(), json!(head));
            }
            entry
        })
        .collect();
    out.insert("packages".into(), Value::Array(kept));
    let omitted = items.len().saturating_sub(COMPACT_MAX_PACKAGES);
    if omitted > 0 {
        out.insert("packages_omitted".into(), json!(omitted));
    }
}

/// `value` as a list capped at `max` entries; objects keep only `keys` (all of
/// their fields when `keys` is empty), text is cut to a few hundred characters.
fn insert_bounded(
    out: &mut Map<String, Value>,
    key: &str,
    value: &Value,
    max: usize,
    keys: &[&str],
) {
    let Some(items) = value.as_array() else {
        // `new_failures: null` means "not measured"; keep that distinction.
        out.insert(key.into(), value.clone());
        return;
    };
    let kept: Vec<Value> = items
        .iter()
        .take(max)
        .map(|item| match item {
            Value::String(text) => json!(head_chars(text, MAX_TEXT_CHARS)),
            _ if keys.is_empty() => item.clone(),
            _ => keep_keys(item, keys),
        })
        .collect();
    out.insert(key.into(), Value::Array(kept));
    let omitted = items.len().saturating_sub(max);
    if omitted > 0 {
        out.insert(format!("{key}_omitted"), json!(omitted));
    }
}

fn keep_keys(item: &Value, keys: &[&str]) -> Value {
    let Some(entry) = item.as_object() else {
        return item.clone();
    };
    Value::Object(
        keys.iter()
            .filter_map(|key| Some(((*key).to_owned(), entry.get(*key)?.clone())))
            .collect(),
    )
}

/// The first [`COMPACT_MAX_FAILURES`] texts and how many were left out.
fn bounded_texts(items: &[String]) -> (Vec<&str>, u64) {
    let kept = items
        .iter()
        .take(COMPACT_MAX_FAILURES)
        .map(String::as_str)
        .collect();
    let omitted = items.len().saturating_sub(COMPACT_MAX_FAILURES);
    (kept, u64::try_from(omitted).unwrap_or(u64::MAX))
}

fn head_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The last `max` characters of `text`.
fn tail_chars(text: &str, max: usize) -> &str {
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    let skip = count - max;
    let start = text.char_indices().nth(skip).map_or(0, |(index, _)| index);
    &text[start..]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent_jobs::{JobKind, JobState};
    use uuid::Uuid;

    /// A finished `scoped_test` receipt the size of a real one: sixty
    /// packages with warning tables, a long filter selection and tool blobs.
    fn big_receipt() -> Value {
        let packages: Vec<Value> = (0..60)
            .map(|n| {
                json!({
                    "package": format!("pkg-{n}"), "status": "passed", "completed": 100 + n,
                    "failed_tests": [], "new_failures": [], "baseline_reds": [],
                    "log": format!("/tmp/rsi-scoped-test-xyz/pkg-{n}.log"),
                    "run_sec": 12.5, "slot_wait_sec": 0.7,
                })
            })
            .collect();
        let warning_packages: Value = (0..60)
            .map(|n| {
                (
                    format!("pkg-{n}"),
                    json!({
                        "baseline": 0, "complete": true, "configs": ["bin", "lib", "lib test"],
                        "fixed": [], "fixed_total": 0, "new": [], "new_total": 0,
                        "observed": 0, "status": "ok",
                        "places": (0..40).map(|p| format!("crates/pkg-{n}/src/file_{p}.rs:{p}")).collect::<Vec<_>>(),
                    }),
                )
            })
            .collect::<Map<String, Value>>()
            .into();
        let filters: Vec<String> = (0..600)
            .map(|n| format!("rsid::session::module_{n}::tests::a_long_test_name_{n}"))
            .collect();
        json!({
            "schema": 1, "ok": true, "exit_code": 0, "dry_run": false,
            "base": "origin/rolling", "head": "abc123def4567890",
            "merges_cleanly_onto_rolling": true, "conflict_paths": [],
            "new_failures": [], "baseline_reds": [], "failures": [],
            "packages": packages,
            "warnings": {"base_baseline": "present", "status": "ok", "packages": warning_packages},
            "filters": filters, "selected_filters": filters, "run_filters": filters,
            "tool": {"blob": "x".repeat(20_000)},
            "baseline_comparison": {"blob": "y".repeat(10_000)},
        })
    }

    fn job(result: AgentJobResultV1) -> AgentJobV1 {
        let mut job: AgentJobV1 = serde_json::from_value(json!({
            "id": Uuid::nil(), "kind": "test", "name": null, "state": "succeeded",
            "owner_session_id": Uuid::nil(), "unit_name": "u", "cwd": "/tmp/x",
            "log_path": "/tmp/x.log",
            "params": {"kind": "test", "scoped_test": {"base": "origin/rolling"}},
            "exit_code": 0, "result": null, "created_at": "2026-10-23T00:00:00Z",
            "finished_at": "2026-10-23T01:00:00Z", "wake": "owner",
        }))
        .expect("job shape");
        job.kind = JobKind::Test;
        job.state = JobState::Succeeded;
        job.result = Some(result);
        job
    }

    #[test]
    fn a_real_sized_finished_scoped_test_receipt_stays_under_the_bound() {
        let receipt = big_receipt();
        let full = serde_json::to_string(&job(AgentJobResultV1 {
            exit_code: Some(0),
            receipt: Some(receipt.clone()),
            detail: Some("z".repeat(60_000)),
            ..AgentJobResultV1::default()
        }))
        .unwrap();
        assert!(
            full.len() > 100_000,
            "fixture is receipt-sized: {}",
            full.len()
        );
        let compact = job(AgentJobResultV1 {
            exit_code: Some(0),
            receipt: Some(receipt),
            detail: Some("z".repeat(60_000)),
            ..AgentJobResultV1::default()
        })
        .compact_view();
        let text = compact.to_string();
        assert!(text.len() < 20_000, "compact view is {} chars", text.len());
        assert!(text.len() < full.len() / 10);
    }

    #[test]
    fn the_compact_receipt_keeps_the_verdict_fields_a_worker_reads() {
        let compact = compact_receipt(&big_receipt());
        assert_eq!(compact["ok"], true);
        assert_eq!(compact["exit_code"], 0);
        assert_eq!(compact["base"], "origin/rolling");
        assert_eq!(compact["head"], "abc123def4567890");
        assert_eq!(compact["merges_cleanly_onto_rolling"], true);
        assert_eq!(compact["new_failures"], json!([]));
        assert_eq!(compact["baseline_reds"], json!([]));
        assert_eq!(compact["warnings"], json!({"status": "ok"}));
        let packages = compact["packages"].as_array().unwrap();
        assert_eq!(packages.len(), 60);
        assert_eq!(
            packages[3],
            json!({"package": "pkg-3", "status": "passed", "completed": 103})
        );
        assert!(compact.get("filters").is_none());
        assert!(compact.get("tool").is_none());
    }

    #[test]
    fn red_lists_are_bounded_and_say_how_many_were_cut() {
        let failures: Vec<Value> = (0..COMPACT_MAX_FAILURES + 7)
            .map(|n| json!({"package": "rsid", "kind": "lib", "target": "rsid", "test": format!("t{n}"), "extra": "x"}))
            .collect();
        let compact = compact_receipt(&json!({
            "ok": false, "new_failures": failures, "failures": failures, "baseline_reds": null,
        }));
        let kept = compact["new_failures"].as_array().unwrap();
        assert_eq!(kept.len(), COMPACT_MAX_FAILURES);
        assert_eq!(
            kept[0],
            json!({"package": "rsid", "kind": "lib", "target": "rsid", "test": "t0"})
        );
        assert_eq!(compact["new_failures_omitted"], 7);
        assert_eq!(compact["failures_omitted"], 7);
        // null means "not measured" and stays null.
        assert_eq!(compact["baseline_reds"], Value::Null);
    }

    #[test]
    fn a_failed_package_keeps_its_phase_and_a_cut_diagnostic() {
        let compact = compact_receipt(&json!({"packages": [{
            "package": "rsi", "status": "failed", "completed": 0, "phase": "build_failed",
            "diagnostic": "e".repeat(5_000), "log": "/tmp/x.log",
        }]}));
        let entry = &compact["packages"][0];
        assert_eq!(entry["phase"], "build_failed");
        assert_eq!(
            entry["diagnostic"].as_str().unwrap().chars().count(),
            COMPACT_MAX_DIAGNOSTIC_CHARS
        );
        assert!(entry.get("log").is_none());
    }

    #[test]
    fn the_request_defaults_to_full_and_accepts_only_the_two_views() {
        use crate::agent_control_schema::AgentControlVerbV1;
        use crate::agent_jobs::{AgentGetJobRequestV1, AgentJobFieldsV1};
        let id = Uuid::nil().to_string();
        let plain: AgentGetJobRequestV1 = serde_json::from_value(json!({"job_id": id})).unwrap();
        assert_eq!(plain.fields, AgentJobFieldsV1::Full);
        let compact: AgentGetJobRequestV1 =
            serde_json::from_value(json!({"job_id": id, "fields": "compact"})).unwrap();
        assert_eq!(compact.fields, AgentJobFieldsV1::Compact);
        let bad = json!({"job_id": id, "fields": "tiny"});
        assert!(serde_json::from_value::<AgentGetJobRequestV1>(bad.clone()).is_err());
        assert!(AgentControlVerbV1::GetJob.validate_params(&bad).is_err());
        assert!(
            AgentControlVerbV1::GetJob
                .validate_params(&json!({"job_id": id, "fields": "compact"}))
                .is_ok()
        );
    }

    #[test]
    fn the_job_envelope_keeps_state_and_names_what_it_dropped() {
        let mut view = job(AgentJobResultV1 {
            exit_code: Some(1),
            failing_tests: (0..COMPACT_MAX_FAILURES + 3)
                .map(|n| format!("t{n}"))
                .collect(),
            failing_tests_omitted: 2,
            detail: Some("é".repeat(COMPACT_MAX_DETAIL_CHARS + 50)),
            source_head: Some("abc".into()),
            ..AgentJobResultV1::default()
        })
        .compact_view();
        assert_eq!(view["view"], "compact");
        assert_eq!(view["state"], "succeeded");
        assert_eq!(view["log_path"], "/tmp/x.log");
        assert!(view.get("params").is_none());
        assert!(view.get("cwd").is_none());
        let result = view["result"].as_object_mut().unwrap();
        assert_eq!(
            result["failing_tests"].as_array().unwrap().len(),
            COMPACT_MAX_FAILURES
        );
        assert_eq!(result["failing_tests_omitted"], 5);
        assert_eq!(
            result["detail"].as_str().unwrap().chars().count(),
            COMPACT_MAX_DETAIL_CHARS
        );
        assert_eq!(result["dropped"], json!(["source_head"]));
    }

    /// hub #3889: the default listing row carries no `result` payload but the
    /// state, exit code and receipt head; `fields:"full"` is unchanged.
    #[test]
    fn the_listing_row_drops_the_result_and_keeps_the_verdict_fields() {
        use crate::agent_jobs::{
            AgentJobListFieldsV1, AgentListJobsRequestV1, AgentListJobsResultV1,
        };
        let result = AgentJobResultV1 {
            exit_code: Some(0),
            receipt: Some(big_receipt()),
            detail: Some("z".repeat(60_000)),
            refusal: Some("job_cancelled".into()),
            ..AgentJobResultV1::default()
        };
        let listed = AgentListJobsResultV1 {
            jobs: vec![job(result)],
        };
        let full_job = serde_json::to_value(&listed.jobs[0]).unwrap();

        assert_eq!(
            serde_json::from_value::<AgentListJobsRequestV1>(json!({}))
                .unwrap()
                .fields,
            AgentJobListFieldsV1::Compact
        );
        let compact = listed.render(AgentJobListFieldsV1::Compact);
        let text = compact.to_string();
        assert!(
            text.len() < 1_000,
            "compact listing is {} chars",
            text.len()
        );
        let row = &compact["jobs"][0];
        assert!(row.get("result").is_none());
        assert_eq!(row["state"], "succeeded");
        assert_eq!(row["exit_code"], 0);
        assert_eq!(row["receipt_head"], "abc123def4567890");
        assert_eq!(row["refusal"], "job_cancelled");
        assert_eq!(row["wake"], "owner");
        assert_eq!(row["log_path"], "/tmp/x.log");

        let full = listed.render(AgentJobListFieldsV1::Full);
        assert_eq!(full["jobs"][0], full_job);
        assert!(full["jobs"][0]["result"]["receipt"].is_object());
    }
}
