//! Job-admission export annotations. Exact test names are context only: neither
//! a record nor a QA Issue proves a signature match or a failure on the base.
use super::receipt::Failure;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SNAPSHOT: &str = "target/.rsi-tmp/scoped-test/known-failures.v1.json";
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_AGE_SECONDS: i64 = 86400;

struct Snapshot {
    export: Value,
    age: f64,
    path: PathBuf,
}
fn unavailable(path: &Path, reason: impl Into<String>) -> Value {
    json!({"status":"unavailable","source":"snapshot","path":path,"reason":reason.into()})
}
fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Avoid blocking on a FIFO or following a substituted final symlink.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::other("snapshot must be a regular file"));
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("snapshot must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("snapshot exceeds 8 MiB bound"));
    }
    Ok(bytes)
}
impl Snapshot {
    fn load(repo: &Path, now: DateTime<Utc>, deadline: Instant) -> Result<Self, Value> {
        let path = repo.join(SNAPSHOT);
        let bytes = read_regular(&path).map_err(|error| {
            unavailable(
                &path,
                if error.kind() == io::ErrorKind::NotFound {
                    format!("known-failure snapshot missing file: {}", path.display())
                } else {
                    format!("known-failure snapshot read error: {error}")
                },
            )
        })?;
        let export: Value = serde_json::from_slice(&bytes).map_err(|error| {
            unavailable(
                &path,
                format!("known-failure snapshot parse error: {error}"),
            )
        })?;
        let error = |reason: &str| {
            unavailable(
                &path,
                format!("known-failure snapshot parse error: {reason}"),
            )
        };
        if !export.is_object() || export["schema_version"] != 1 {
            return Err(error("unsupported export schema_version (expected 1)"));
        }
        if export["project_id"]
            .as_str()
            .is_none_or(|s| uuid::Uuid::parse_str(s).is_err())
        {
            return Err(error("invalid project_id"));
        }
        let snapshot = &export["snapshot"];
        if !snapshot.is_object() || snapshot["schema_version"] != 1 {
            return Err(error("unsupported snapshot schema_version (expected 1)"));
        }
        let timestamp = snapshot["exported_at"]
            .as_str()
            .ok_or_else(|| error("exported_at must be RFC3339"))?;
        let grammar = crate::re::Re::new(
            r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$",
        );
        if !grammar.is_match(timestamp) {
            return Err(error("exported_at must be RFC3339"));
        }
        let exported = DateTime::parse_from_rfc3339(timestamp)
            .map_err(|_| error("exported_at must be RFC3339"))?
            .with_timezone(&Utc);
        let duration = now.signed_duration_since(exported);
        let age = duration.num_milliseconds() as f64 / 1000.0;
        if now < exported || duration >= chrono::Duration::seconds(MAX_AGE_SECONDS) {
            let mut result = unavailable(
                &path,
                format!("known-failure snapshot stale: age={age:.3}s (limit=86400s)"),
            );
            result["age_seconds"] = json!(age);
            return Err(result);
        }
        let entries = snapshot["records"]
            .as_array()
            .ok_or_else(|| error("records and malformed_issues must be lists"))?;
        if !export["malformed_issues"].is_array() {
            return Err(error("records and malformed_issues must be lists"));
        }
        for entry in entries {
            if Instant::now() >= deadline {
                return Err(unavailable(&path, "known-failure lookup budget exhausted"));
            }
            let record = &entry["record"];
            if !record.is_object()
                || record["test_id"].as_str().is_none_or(str::is_empty)
                || record.get("schema_version").is_some_and(|v| v != 1)
                || record["issue"].as_u64().is_none_or(|n| n == 0)
                || record["class"]
                    .as_str()
                    .is_none_or(|s| !["regression", "flake", "env", "seed"].contains(&s))
                || !record["matcher"].is_object()
                || entry["issue_status"]
                    .as_str()
                    .is_none_or(|s| !["Open", "InProgress", "Closed", "Cancelled"].contains(&s))
            {
                return Err(error("invalid snapshot record"));
            }
        }
        if Instant::now() >= deadline {
            return Err(unavailable(&path, "known-failure lookup budget exhausted"));
        }
        Ok(Self { export, age, path })
    }
    fn query(&self, test: &str) -> Value {
        let records: Vec<_> = self.export["snapshot"]["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| {
                ["Open", "InProgress"]
                    .iter()
                    .any(|s| entry["issue_status"] == *s)
                    && entry["record"]["test_id"] == test
            })
            .map(|entry| entry["record"].clone())
            .collect();
        if records.is_empty()
            && self.export["malformed_issues"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
        {
            let mut result = unavailable(
                &self.path,
                "known-failure snapshot contains malformed Issues",
            );
            result["malformed_issues"] = self.export["malformed_issues"].clone();
            return result;
        }
        let mut result = json!({"status":if records.is_empty() {"unknown"} else {"recorded"},"source":"snapshot","path":self.path,"age_seconds":self.age,"records":records});
        if !records.is_empty() {
            return result;
        }
        result["reason"] = json!("snapshot loaded; no matching active record");
        // Optional context must never change unknown into unavailable: those
        // statuses have deliberately different baseline-only acceptance rules.
        let empty = json!({});
        let advisories = self.export.get("qa_advisories").unwrap_or(&empty);
        let issues = if advisories.is_object() {
            advisories.get(test).cloned().unwrap_or(json!([]))
        } else {
            Value::Null
        };
        let Some(issues) = issues
            .as_array()
            .filter(|items| items.iter().all(|v| v.as_u64().is_some_and(|n| n > 0)))
        else {
            result["qa_advisory_error"] = json!("invalid QA advisory Issue numbers");
            return result;
        };
        let issues: BTreeSet<_> = issues.iter().filter_map(Value::as_u64).collect();
        if !issues.is_empty() {
            result["qa_issue_advisories"] = json!(issues.into_iter().map(|issue| json!({"issue":issue,"message":format!("open qa Issue #{issue} names this test (advisory, not a signature match)")})).collect::<Vec<_>>());
        }
        result
    }
}

pub struct Annotations {
    pub records: Vec<Value>,
    pub without_record: Vec<Failure>,
}
/// Read only the export beneath the admitted repository, independently of the
/// helper copy, HOME, target override or agent credentials. A single immutable
/// load and exact-name cache serve this annotation batch's ten-second budget.
pub fn annotate(repo: &Path, reds: &[Failure], emit: impl FnMut(&str)) -> Annotations {
    annotate_at(
        repo,
        reds,
        Utc::now(),
        Instant::now() + Duration::from_secs(10),
        emit,
    )
}
fn annotate_at(
    repo: &Path,
    reds: &[Failure],
    now: DateTime<Utc>,
    deadline: Instant,
    mut emit: impl FnMut(&str),
) -> Annotations {
    let loaded = Snapshot::load(repo, now, deadline);
    let mut cache = BTreeMap::new();
    let mut result = Annotations {
        records: vec![],
        without_record: vec![],
    };
    for failure in reds {
        let lookup = cache.entry(failure.test.clone()).or_insert_with(|| {
            if Instant::now() >= deadline {
                return unavailable(
                    &repo.join(SNAPSHOT),
                    "known-failure lookup budget exhausted",
                );
            }
            match &loaded {
                Ok(snapshot) => snapshot.query(&failure.test),
                Err(error) => error.clone(),
            }
        });
        let mut record = serde_json::to_value(failure).expect("failure serializes");
        record
            .as_object_mut()
            .expect("failure object")
            .extend(lookup.as_object().expect("lookup object").clone());
        result.records.push(record);
        if lookup["status"] == "unknown" {
            result.without_record.push(failure.clone());
            emit(&format!(
                "scoped-test: baseline red without known-failure record: {} {}",
                failure.package, failure.test
            ));
            for advisory in lookup["qa_issue_advisories"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(message) = advisory["message"].as_str() {
                    emit(&format!(
                        "scoped-test: {} {}: {message}",
                        failure.package, failure.test
                    ));
                }
            }
        } else if lookup["status"] == "unavailable" {
            emit(&format!(
                "scoped-test: baseline red known-failure lookup unavailable: {} {}",
                failure.package, failure.test
            ));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn export() -> Value {
        json!({"schema_version":1,"project_id":"00000000-0000-4000-8000-000000000430","snapshot":{"schema_version":1,"exported_at":"2026-10-22T07:00:00Z","records":[]},"malformed_issues":[]})
    }
    fn failure() -> Failure {
        Failure {
            package: "pkg".into(),
            kind: "lib".into(),
            target: "pkg".into(),
            test: "suite::test".into(),
        }
    }
    fn record() -> Value {
        json!({"schema_version":1,"issue":433,"test_id":"suite::test","class":"regression","matcher":{"contains":["boom"]}})
    }
    fn check(value: &Value) -> (Value, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        crate::testutil::write(dir.path(), SNAPSHOT, &value.to_string());
        let mut messages = vec![];
        let result = annotate_at(
            dir.path(),
            &[failure()],
            DateTime::parse_from_rfc3339("2026-10-22T07:00:01Z")
                .unwrap()
                .with_timezone(&Utc),
            Instant::now() + Duration::from_secs(10),
            |s| messages.push(s.into()),
        );
        (result.records[0].clone(), messages)
    }
    #[test]
    fn active_exact_records_closed_owners_and_malformed_issues_preserve_statuses() {
        for (state, name, malformed, expected) in [
            ("Open", "suite::test", false, "recorded"),
            ("InProgress", "suite::test", true, "recorded"),
            ("Closed", "suite::test", false, "unknown"),
            ("Cancelled", "suite::test", false, "unknown"),
            ("Open", "suite::test_extra", false, "unknown"),
            ("Closed", "suite::test", true, "unavailable"),
        ] {
            let mut export = export();
            let mut record = record();
            record["test_id"] = json!(name);
            export["snapshot"]["records"] = json!([{"record":record,"issue_status":state}]);
            if malformed {
                export["malformed_issues"] = json!(["invalid Issue"]);
            }
            let result = check(&export).0;
            assert_eq!(result["status"], expected, "{state}/{name}/{malformed}");
            if expected == "recorded" {
                assert_eq!(result["records"], json!([record]));
            }
        }
    }
    #[test]
    fn qa_advisories_are_exact_sorted_deduplicated_and_signature_records_win() {
        let mut export = export();
        export["qa_advisories"] = json!({"suite::test":[431,430,431],"suite::test_extra":[432]});
        let (result, messages) = check(&export);
        assert_eq!(result["status"], "unknown");
        assert_eq!(result["records"], json!([]));
        assert_eq!(
            result["qa_issue_advisories"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["issue"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![430, 431]
        );
        assert_eq!(messages.len(), 3);
        assert!(messages[1].contains("advisory, not a signature match"));
        export["snapshot"]["records"] = json!([{"record":record(),"issue_status":"Open"}]);
        let (result, messages) = check(&export);
        assert_eq!(result["status"], "recorded");
        assert_eq!(messages, Vec::<String>::new());
        assert_eq!(result["qa_issue_advisories"], Value::Null);
        export["snapshot"]["records"] = json!([]);
        export["qa_advisories"] = json!({"suite::test_extra":[432]});
        assert_eq!(check(&export).0["qa_issue_advisories"], Value::Null);
    }
    #[test]
    fn malformed_optional_advisories_stay_unknown_and_old_exports_still_load() {
        for advisory in [
            json!([430]),
            json!({"suite::test":[true]}),
            json!({"suite::test":[0]}),
            json!({"suite::test":[-1]}),
            json!({"suite::test":"430"}),
        ] {
            let mut export = export();
            export["qa_advisories"] = advisory;
            let result = check(&export).0;
            assert_eq!(result["status"], "unknown");
            assert_eq!(
                result["qa_advisory_error"],
                "invalid QA advisory Issue numbers"
            );
        }
        let result = check(&export()).0;
        assert_eq!(result["status"], "unknown");
        assert_eq!(result["qa_advisory_error"], Value::Null);
    }
    #[test]
    fn stale_future_bad_schema_timestamp_or_record_never_attest_absence() {
        for timestamp in [
            "2026-10-21T07:00:01Z",
            "2026-10-22T07:00:02Z",
            "2026-10-22 07:00:00Z",
            "2026-10-22T07:00:00",
            "bad",
        ] {
            let mut export = export();
            export["snapshot"]["exported_at"] = json!(timestamp);
            assert_eq!(check(&export).0["status"], "unavailable", "{timestamp}");
        }
        for (field, value) in [
            ("schema_version", json!(2)),
            ("project_id", json!("invalid")),
            ("malformed_issues", json!({})),
            ("snapshot", json!(null)),
        ] {
            let mut export = export();
            export[field] = value;
            assert_eq!(check(&export).0["status"], "unavailable", "{field}");
        }
        for (field, value) in [
            ("test_id", json!("")),
            ("issue", json!(true)),
            ("class", json!("unknown")),
            ("matcher", json!([])),
            ("schema_version", json!(2)),
        ] {
            let mut export = export();
            let mut record = record();
            record[field] = value;
            export["snapshot"]["records"] = json!([{"record":record,"issue_status":"Open"}]);
            assert_eq!(check(&export).0["status"], "unavailable", "{field}");
        }
    }
    #[test]
    fn missing_invalid_oversized_and_nonregular_snapshot_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let reds = [failure()];
        let now = Utc::now();
        let deadline = Instant::now() + Duration::from_secs(10);
        assert_eq!(
            annotate_at(dir.path(), &reds, now, deadline, |_| {}).records[0]["status"],
            "unavailable"
        );
        let path = crate::testutil::write(dir.path(), SNAPSHOT, "not json");
        assert_eq!(
            annotate_at(dir.path(), &reds, now, deadline, |_| {}).records[0]["status"],
            "unavailable"
        );
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert_eq!(
            annotate_at(dir.path(), &reds, now, deadline, |_| {}).records[0]["status"],
            "unavailable"
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            annotate_at(dir.path(), &reds, now, deadline, |_| {}).records[0]["status"],
            "unavailable"
        );
    }
    #[test]
    fn annotation_keeps_per_package_identity_and_budget_exhaustion_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-22T07:00:01Z")
            .unwrap()
            .with_timezone(&Utc);
        crate::testutil::write(dir.path(), SNAPSHOT, &export().to_string());
        let reds = [
            failure(),
            Failure {
                package: "other".into(),
                ..failure()
            },
        ];
        let result = annotate_at(
            dir.path(),
            &reds,
            now,
            Instant::now() + Duration::from_secs(10),
            |_| {},
        );
        assert_eq!(result.without_record, reds);
        assert_eq!(result.records[0]["package"], "pkg");
        assert_eq!(result.records[1]["package"], "other");
        let result = annotate_at(dir.path(), &reds, now, Instant::now(), |_| {});
        assert_eq!(result.records[0]["status"], "unavailable");
        assert_eq!(
            result.records[0]["reason"],
            "known-failure lookup budget exhausted"
        );
    }
}
