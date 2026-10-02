//! Typed known-failure signature records and classification (#1016).
//!
//! A record pairs an exact test id with a failure matcher (`digest` of the
//! normalized panic payload, and/or stable `contains` substrings) and the
//! owning Issue. Records live in fenced `rsi-failure-signature` blocks inside
//! their owner Issue body; closing the Issue retires the record structurally.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::sync::LazyLock;

pub const SCHEMA_VERSION: u8 = 1;
/// Info string of the fenced block that carries one record.
pub const FENCE_INFO: &str = "rsi-failure-signature";
/// Default snapshot path relative to `$HOME`.
pub const DEFAULT_SNAPSHOT_RELATIVE: &str = ".rsi/qa/known-failures.v1.json";

/// Errors from parsing and validating signature records.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("signature validation failed: {0}")]
    Validation(String),
    #[error("invalid signature JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("signature record declares issue #{actual}, expected #{expected}")]
    IssueMismatch { expected: u64, actual: u64 },
}

const fn default_schema_version() -> u8 {
    SCHEMA_VERSION
}

/// How a failure was matched to a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    Digest,
    Contains,
}

/// Classification of an observed failure against a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Classification {
    Known {
        issue: u64,
        class: SignatureClass,
        by: MatchedBy,
    },
    NameOnly {
        issues: Vec<u64>,
    },
    New,
}

/// Owner-facing class of a known failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureClass {
    Regression,
    Flake,
    Env,
    Seed,
}

impl SignatureClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Regression => "regression",
            Self::Flake => "flake",
            Self::Env => "env",
            Self::Seed => "seed",
        }
    }
}

/// Failure matcher; at least one field must be set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains: Vec<String>,
}

/// One known-failure signature owned by an Issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureRecord {
    #[serde(default = "default_schema_version")]
    pub schema_version: u8,
    pub test_id: String,
    pub matcher: Matcher,
    pub issue: u64,
    pub class: SignatureClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl SignatureRecord {
    /// Reject structurally unusable records.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Validation`] when a field is empty, zero, malformed or
    /// carries an unsupported schema version.
    pub fn validate(&self) -> Result<(), Error> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(Error::Validation(format!(
                "unsupported schema_version {}",
                self.schema_version
            )));
        }
        if self.test_id.trim().is_empty() {
            return Err(Error::Validation("test_id must not be empty".into()));
        }
        if self.issue == 0 {
            return Err(Error::Validation("issue must not be 0".into()));
        }
        if self.matcher.digest.is_none() && self.matcher.contains.is_empty() {
            return Err(Error::Validation(
                "matcher must set a digest or at least one contains substring".into(),
            ));
        }
        if let Some(digest) = &self.matcher.digest
            && !is_lower_hex_64(digest)
        {
            return Err(Error::Validation(format!(
                "digest must be 64 lowercase hex characters, got {digest:?}"
            )));
        }
        Ok(())
    }
}

fn is_lower_hex_64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One snapshot entry: a record plus the status of its owner Issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRecord {
    pub record: SignatureRecord,
    pub issue_status: String,
}

/// Whether an owner Issue status keeps its signatures alive. Closing or
/// cancelling the Issue retires them (acceptance 4).
#[must_use]
pub fn status_is_live(status: &str) -> bool {
    status == "Open" || status == "InProgress"
}

/// A snapshot older than this many hours is flagged stale: an owner Issue may
/// have closed since the export.
pub const SNAPSHOT_STALE_HOURS: i64 = 24;

impl SnapshotRecord {
    /// True while the owner Issue is `Open`/`InProgress`.
    #[must_use]
    pub fn is_live(&self) -> bool {
        status_is_live(&self.issue_status)
    }
}

impl Snapshot {
    /// Records whose owner Issue is still open. Every classifier and query
    /// reads through this, so a stale or hand-edited snapshot file cannot keep
    /// a closed Issue's signature alive.
    pub fn live_records(&self) -> impl Iterator<Item = &SnapshotRecord> {
        self.records.iter().filter(|entry| entry.is_live())
    }

    /// Whole hours since `exported_at`, or `None` when the timestamp does not
    /// parse.
    #[must_use]
    pub fn age_hours(&self, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
        let exported = chrono::DateTime::parse_from_rfc3339(&self.exported_at).ok()?;
        Some((now - exported.with_timezone(&chrono::Utc)).num_hours())
    }

    /// One-line warning when the snapshot is older than
    /// [`SNAPSHOT_STALE_HOURS`] (or its timestamp is unreadable).
    #[must_use]
    pub fn stale_warning(&self, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
        match self.age_hours(now) {
            Some(hours) if hours < SNAPSHOT_STALE_HOURS => None,
            Some(hours) => Some(format!(
                "known-failure snapshot is {hours}h old (limit {SNAPSHOT_STALE_HOURS}h): a closed owner Issue may still be listed; re-run export or use AgentQueryFailureSignatures"
            )),
            None => Some(format!(
                "known-failure snapshot exported_at {:?} is not RFC3339; treat it as stale",
                self.exported_at
            )),
        }
    }
}

/// Exported known-failure snapshot consumed by classifiers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    #[serde(default = "default_schema_version")]
    pub schema_version: u8,
    pub exported_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_rolling: Option<String>,
    pub records: Vec<SnapshotRecord>,
}

/// Result of building a snapshot: the snapshot plus per-Issue parse errors.
#[derive(Clone, Debug)]
pub struct SnapshotBuild {
    pub snapshot: Snapshot,
    pub errors: Vec<String>,
}

#[allow(clippy::expect_used)]
static PANIC_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"panicked at [^\s]+?:\d+:\d+:").expect("panic regex compiles")
});
#[allow(clippy::expect_used)]
static UUID_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
    )
    .expect("uuid regex compiles")
});
#[allow(clippy::expect_used)]
static TIMESTAMP_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?")
        .expect("timestamp regex compiles")
});
#[allow(clippy::expect_used)]
static PATH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?:/tmp|/dev/shm|/home/[^\s/]+)/[^\s]*").expect("path regex compiles")
});
#[allow(clippy::expect_used)]
static HEX_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b[0-9a-fA-F]{8,}\b").expect("hex regex compiles"));
#[allow(clippy::expect_used)]
static DURATION_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\b\d+(?:\.\d+)?\s?(?:ns|us|µs|ms|s)\b").expect("duration regex compiles")
});
#[allow(clippy::expect_used)]
static INTEGER_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[0-9]+").expect("integer regex compiles"));
#[allow(clippy::expect_used)]
static THREAD_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"thread '[^']*'(?: \((?:<N>|[0-9]+)\))?\s*").expect("thread regex compiles")
});

/// Extract the panic payload from raw test output.
///
/// Text after the first `panicked at <path>:<line>:<col>:` line, up to (not
/// including) a `note: run with` line. Without a panic line, the stack-overflow
/// marker wins; otherwise the whole trimmed text.
#[must_use]
pub fn panic_payload(text: &str) -> String {
    if let Some(found) = PANIC_RE.find(text) {
        let rest = &text[found.end()..];
        let end = rest.find("\nnote: run with").unwrap_or(rest.len());
        return rest[..end].trim().to_string();
    }
    if text.contains("has overflowed its stack") {
        return "has overflowed its stack".to_string();
    }
    text.trim().to_string()
}

/// Normalize a panic payload into a stable, host-independent string.
#[must_use]
pub fn normalize(payload: &str) -> String {
    let masked = UUID_RE.replace_all(payload, "<UUID>");
    let masked = TIMESTAMP_RE.replace_all(&masked, "<TS>");
    let masked = PATH_RE.replace_all(&masked, "<PATH>");
    // An all-digit run is a count, not a hex id: mask it like any integer so
    // a value crossing 8 digits does not change the digest.
    let masked = HEX_RE.replace_all(&masked, |caps: &regex::Captures<'_>| {
        if caps[0].bytes().all(|byte| byte.is_ascii_digit()) {
            "<N>"
        } else {
            "<HEX>"
        }
    });
    let masked = DURATION_RE.replace_all(&masked, "<DUR>");
    let masked = INTEGER_RE.replace_all(&masked, "<N>");
    let masked = THREAD_RE.replace_all(&masked, "");
    masked.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Lowercase hex sha256 of `normalize(panic_payload(text))`.
#[must_use]
pub fn digest(text: &str) -> String {
    let normalized = normalize(&panic_payload(text));
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[allow(clippy::expect_used)]
static NEXTTEST_FAILURE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?m)^\s*(?:FAIL|SIG[A-Z0-9]+) \[[^]]+\] \([^)]*\) (.+)$").expect("pattern")
});
#[allow(clippy::expect_used)]
static CARGO_FAILURE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?m)^test (.+) \.\.\. FAILED$").expect("pattern"));

/// Every failing test id named in a nextest or libtest log, sorted and deduped.
///
/// Nextest prints `<binary> <test>`; libtest prints just `<test>`. Both end in
/// the exact test id.
#[must_use]
pub fn failing_tests(log: &str) -> Vec<String> {
    let mut found: Vec<String> = NEXTTEST_FAILURE
        .captures_iter(log)
        .filter_map(|caps| caps.get(1).map(|m| test_name(m.as_str())))
        .chain(
            CARGO_FAILURE
                .captures_iter(log)
                .filter_map(|caps| caps.get(1).map(|m| test_name(m.as_str()))),
        )
        .collect();
    found.sort();
    found.dedup();
    found
}

fn test_name(captured: &str) -> String {
    captured
        .split_whitespace()
        .last()
        .unwrap_or(captured)
        .to_string()
}

/// The failure text belonging to one test in a combined nextest/libtest log.
#[must_use]
pub fn failure_text_for(log: &str, test: &str) -> String {
    // Anchor on this test's own panic. The first bare occurrence of the name
    // is usually an early `test <name> ... FAILED` or START line, and the next
    // panic after it can belong to a different test (libtest prints every
    // panic section after the summary lines).
    let anchor = log
        .find(&format!("thread '{test}'"))
        .or_else(|| log.find(&format!("---- {test} stdout ----")))
        .or_else(|| log.find(test));
    let Some(start) = anchor else {
        return log.to_string();
    };
    let slice = &log[start..];
    if let Some(found) = PANIC_RE.find(slice) {
        let rest = &slice[found.start()..];
        let end = rest.find("\nnote: run with").unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    if let Some(offset) = slice.find("has overflowed its stack") {
        let line_start = slice[..offset].rfind('\n').map_or(0, |i| i + 1);
        let line_end = slice[offset..]
            .find('\n')
            .map_or(slice.len(), |i| offset + i);
        return slice[line_start..line_end].to_string();
    }
    slice.to_string()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn fence_info(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("```")?;
    let info = rest.trim();
    if info.is_empty() { None } else { Some(info) }
}

fn is_closing_fence(line: &str) -> bool {
    line.trim()
        .strip_prefix("```")
        .is_some_and(|rest| rest.trim().is_empty())
}

/// Collect every `rsi-failure-signature` block in `body` as a validated record.
///
/// Every record must declare `issue == issue`; any malformed block is an error.
///
/// # Errors
///
/// Returns [`Error::Json`] for malformed JSON, [`Error::Validation`] for an
/// unusable record, and [`Error::IssueMismatch`] when a record names a
/// different Issue than the one hosting the block.
pub fn parse_issue_blocks(body: &str, issue: u64) -> Result<Vec<SignatureRecord>, Error> {
    let mut records = Vec::new();
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        let Some(info) = fence_info(line.trim_start()) else {
            continue;
        };
        let mut block = String::new();
        for inner in lines.by_ref() {
            if is_closing_fence(inner) {
                break;
            }
            block.push_str(inner);
            block.push('\n');
        }
        if info != FENCE_INFO {
            continue;
        }
        let record: SignatureRecord = serde_json::from_str(block.trim())?;
        record.validate()?;
        if record.issue != issue {
            return Err(Error::IssueMismatch {
                expected: issue,
                actual: record.issue,
            });
        }
        records.push(record);
    }
    Ok(records)
}

/// Build a snapshot from `(number, status, body)` Issue triples.
///
/// Only `Open`/`InProgress` Issues are exported, so closing an Issue retires
/// its signatures. Malformed blocks are reported as errors, never panics.
#[must_use]
pub fn build_snapshot(
    issues: &[(u64, String, String)],
    source_rolling: Option<String>,
) -> SnapshotBuild {
    let mut records = Vec::new();
    let mut errors = Vec::new();
    for (number, status, body) in issues {
        if !status_is_live(status) {
            continue;
        }
        match parse_issue_blocks(body, *number) {
            Ok(parsed) => {
                for record in parsed {
                    records.push(SnapshotRecord {
                        record,
                        issue_status: status.clone(),
                    });
                }
            }
            Err(error) => errors.push(format!("issue #{number}: {error}")),
        }
    }
    SnapshotBuild {
        snapshot: Snapshot {
            schema_version: SCHEMA_VERSION,
            exported_at: now_rfc3339(),
            source_rolling,
            records,
        },
        errors,
    }
}

/// Classify an observed failure against a snapshot.
#[must_use]
pub fn classify(
    snapshot: &Snapshot,
    test_id: &str,
    failure_text: &str,
    host: Option<&str>,
) -> Classification {
    let observed_digest = digest(failure_text);
    let mut name_issues: Vec<u64> = Vec::new();
    let mut contains_match: Option<(u64, SignatureClass)> = None;
    for entry in snapshot.live_records() {
        let record = &entry.record;
        if record.test_id != test_id {
            continue;
        }
        if let Some(scope) = &record.host_scope
            && Some(scope.as_str()) != host
        {
            continue;
        }
        if !name_issues.contains(&record.issue) {
            name_issues.push(record.issue);
        }
        if let Some(digest) = &record.matcher.digest
            && digest == &observed_digest
        {
            return Classification::Known {
                issue: record.issue,
                class: record.class,
                by: MatchedBy::Digest,
            };
        }
        if contains_match.is_none()
            && record
                .matcher
                .contains
                .iter()
                .any(|needle| !needle.is_empty() && failure_text.contains(needle.as_str()))
        {
            contains_match = Some((record.issue, record.class));
        }
    }
    if let Some((issue, class)) = contains_match {
        return Classification::Known {
            issue,
            class,
            by: MatchedBy::Contains,
        };
    }
    if name_issues.is_empty() {
        Classification::New
    } else {
        Classification::NameOnly {
            issues: name_issues,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const LAUNCH_TEXT: &str = "thread 'session::launch::tests::build_cache_reclaim_skips_in_memory_active_owner' (718983) panicked at crates/rsid/src/session/launch.rs:12171:9:\nassertion `left == right` failed: an in-memory active owner must fence reclaim\n  left: 8347648\n right: 0\nnote: run with `RUST_BACKTRACE=1`";
    const LAUNCH_TEXT_CHANGED: &str = "thread 'session::launch::tests::build_cache_reclaim_skips_in_memory_active_owner' (1234) panicked at crates/rsid/src/session/launch.rs:12139:9:\nassertion `left == right` failed: an in-memory active owner must fence reclaim\n  left: 4096\n right: 0\nnote: run with `RUST_BACKTRACE=1`";
    const OVERFLOW_TEXT: &str = "thread 'x::y' (1692186) has overflowed its stack\nfatal runtime error: stack overflow, aborting";
    const TMPFS_TEXT: &str = "thread 'a::b' (1825453) panicked at crates/rsid/src/session/launch.rs:24835:40:\nsuccessor crossed injected post-start crash boundary: Err(ExecutionScratchUnavailable(\"sandbox execution scratch rejected: sandbox root is on tmpfs/ramfs\"))\nnote: run with `RUST_BACKTRACE=1`";
    const TMPFS_TEXT_CHANGED: &str = "thread 'a::b' (99999) panicked at crates/rsid/src/session/launch.rs:24001:1:\nsuccessor crossed injected post-start crash boundary: Err(ExecutionScratchUnavailable(\"sandbox execution scratch rejected: sandbox root is on tmpfs/ramfs\"))\nnote: run with `RUST_BACKTRACE=1`";
    const DML_TEXT: &str = "left: \"rsi-process-owner-v1:socket=2f686f6d652f6a616b6564657661722f2e7273692f6461656d6f6e2e736f636b:database=2f686f6d65\"\n right: \"\"";

    fn record(test_id: &str, issue: u64, matcher: Matcher) -> SignatureRecord {
        SignatureRecord {
            schema_version: SCHEMA_VERSION,
            test_id: test_id.to_string(),
            matcher,
            issue,
            class: SignatureClass::Regression,
            host_scope: None,
            note: None,
        }
    }

    fn block(body: &str) -> String {
        format!("```{FENCE_INFO}\n{body}\n```\n")
    }

    fn snapshot(records: Vec<SignatureRecord>) -> Snapshot {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            exported_at: "2026-09-28T00:00:00Z".to_string(),
            source_rolling: None,
            records: records
                .into_iter()
                .map(|record| SnapshotRecord {
                    record,
                    issue_status: "Open".to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn digest_is_stable_across_pid_line_and_counts() {
        assert_eq!(digest(LAUNCH_TEXT), digest(LAUNCH_TEXT_CHANGED));
        assert_eq!(
            normalize(&panic_payload(LAUNCH_TEXT)),
            "assertion `left == right` failed: an in-memory active owner must fence reclaim left: <N> right: <N>"
        );
        assert_eq!(digest(TMPFS_TEXT), digest(TMPFS_TEXT_CHANGED));
        // A count that grows past eight digits is still a count, not a hex id.
        let eight_digit = LAUNCH_TEXT.replace("left: 8347648", "left: 83476480");
        assert_eq!(digest(LAUNCH_TEXT), digest(&eight_digit));
        assert_eq!(
            normalize("owner deadbeef00 took 12345678"),
            "owner <HEX> took <N>"
        );
    }

    #[test]
    fn overflow_payload_and_classification() {
        assert_eq!(panic_payload(OVERFLOW_TEXT), "has overflowed its stack");
        let snap = snapshot(vec![record(
            "x::y",
            985,
            Matcher {
                digest: None,
                contains: vec!["has overflowed its stack".to_string()],
            },
        )]);
        assert_eq!(
            classify(&snap, "x::y", OVERFLOW_TEXT, None),
            Classification::Known {
                issue: 985,
                class: SignatureClass::Regression,
                by: MatchedBy::Contains,
            }
        );
    }

    #[test]
    fn hex_runs_are_masked() {
        let normalized = normalize(DML_TEXT);
        assert_eq!(
            normalized,
            "left: \"rsi-process-owner-v<N>:socket=<HEX>:database=<HEX>\" right: \"\""
        );
    }

    #[test]
    fn parse_accepts_two_blocks_and_rejects_mismatch() {
        let first = serde_json::to_string(&record(
            "a::b",
            7,
            Matcher {
                digest: Some(digest(TMPFS_TEXT)),
                contains: vec![],
            },
        ))
        .unwrap();
        let second = serde_json::to_string(&record(
            "c::d",
            7,
            Matcher {
                digest: None,
                contains: vec!["boom".to_string()],
            },
        ))
        .unwrap();
        let body = format!("{}{}", block(&first), block(&second));
        let parsed = parse_issue_blocks(&body, 7).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].test_id, "a::b");
        assert_eq!(parsed[1].test_id, "c::d");

        let mismatch = block(&second);
        assert!(matches!(
            parse_issue_blocks(&mismatch, 8),
            Err(Error::IssueMismatch {
                expected: 8,
                actual: 7
            })
        ));
    }

    #[test]
    fn build_snapshot_drops_closed_issues() {
        let body = |issue: u64| {
            block(
                &serde_json::to_string(&record(
                    "a::b",
                    issue,
                    Matcher {
                        digest: None,
                        contains: vec!["x".to_string()],
                    },
                ))
                .unwrap(),
            )
        };
        let issues = vec![
            (1u64, "Open".to_string(), body(1)),
            (2u64, "InProgress".to_string(), body(2)),
            (3u64, "Closed".to_string(), body(3)),
            (4u64, "Cancelled".to_string(), body(4)),
        ];
        let built = build_snapshot(&issues, None);
        assert_eq!(built.snapshot.records.len(), 2);
        assert!(built.errors.is_empty());

        let bad = vec![(5u64, "Open".to_string(), block("{ not json }"))];
        let built = build_snapshot(&bad, None);
        assert!(built.snapshot.records.is_empty());
        assert_eq!(built.errors.len(), 1);
    }

    #[test]
    fn validate_rejects_bad_digest() {
        let bad = record(
            "a::b",
            1,
            Matcher {
                digest: Some("NOTHEX".to_string()),
                contains: vec![],
            },
        );
        assert!(bad.validate().is_err());
        let good = record(
            "a::b",
            1,
            Matcher {
                digest: Some(digest(TMPFS_TEXT)),
                contains: vec![],
            },
        );
        assert!(good.validate().is_ok());
        let empty = record("a::b", 1, Matcher::default());
        assert!(empty.validate().is_err());
        let no_issue = record(
            "a::b",
            0,
            Matcher {
                digest: None,
                contains: vec!["x".to_string()],
            },
        );
        assert!(no_issue.validate().is_err());
        let no_test = record(
            "",
            1,
            Matcher {
                digest: None,
                contains: vec!["x".to_string()],
            },
        );
        assert!(no_test.validate().is_err());
    }

    #[test]
    fn classify_name_only_and_new() {
        let snap = snapshot(vec![record(
            "a::b",
            42,
            Matcher {
                digest: Some(digest(TMPFS_TEXT)),
                contains: vec![],
            },
        )]);
        assert_eq!(
            classify(&snap, "a::b", "some unrelated failure", None),
            Classification::NameOnly { issues: vec![42] }
        );
        assert_eq!(
            classify(&snap, "z::z", "some unrelated failure", None),
            Classification::New
        );
    }

    #[test]
    fn host_scope_filters_records() {
        let mut scoped = record(
            "a::b",
            5,
            Matcher {
                digest: None,
                contains: vec!["boom".to_string()],
            },
        );
        scoped.host_scope = Some("laptop".to_string());
        let snap = snapshot(vec![scoped]);
        assert!(matches!(
            classify(&snap, "a::b", "boom", None),
            Classification::New
        ));
        assert!(matches!(
            classify(&snap, "a::b", "boom", Some("laptop")),
            Classification::Known { issue: 5, .. }
        ));
    }

    #[test]
    fn serde_round_trip() {
        let original = record(
            "a::b",
            9,
            Matcher {
                digest: Some(digest(TMPFS_TEXT)),
                contains: vec!["boom".to_string()],
            },
        );
        let json = serde_json::to_string(&original).unwrap();
        let parsed: SignatureRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);

        let snap = snapshot(vec![original]);
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, parsed);
    }

    /// libtest prints every `test <name> ... FAILED` line before any panic
    /// section, so each test's text must come from its own `thread '<name>'`
    /// panic, not from the first panic after its first mention.
    const LIBTEST_LOG: &str = "\
running 2 tests
test a::first ... FAILED
test b::second ... FAILED

failures:

---- a::first stdout ----

thread 'a::first' (11) panicked at src/a.rs:1:1:
first payload
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

---- b::second stdout ----

thread 'b::second' (12) panicked at src/b.rs:2:2:
second payload
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
";

    #[test]
    fn each_libtest_failure_takes_its_own_panic() {
        assert_eq!(failing_tests(LIBTEST_LOG), vec!["a::first", "b::second"]);
        let first = failure_text_for(LIBTEST_LOG, "a::first");
        let second = failure_text_for(LIBTEST_LOG, "b::second");
        assert!(first.contains("first payload"), "{first}");
        assert!(second.contains("second payload"), "{second}");
        assert_ne!(digest(&first), digest(&second));
    }

    #[test]
    fn classify_ignores_records_of_closed_owner_issues() {
        let matcher = Matcher {
            digest: Some(digest(LAUNCH_TEXT)),
            contains: Vec::new(),
        };
        let mut snap = snapshot(vec![
            record("t::a", 10, matcher.clone()),
            record("t::a", 11, matcher),
        ]);
        snap.records[0].issue_status = "Closed".to_string();
        assert_eq!(
            classify(&snap, "t::a", LAUNCH_TEXT, None),
            Classification::Known {
                issue: 11,
                class: SignatureClass::Regression,
                by: MatchedBy::Digest,
            }
        );
        snap.records[1].issue_status = "Cancelled".to_string();
        assert_eq!(
            classify(&snap, "t::a", LAUNCH_TEXT, None),
            Classification::New
        );
        assert_eq!(snap.live_records().count(), 0);
    }

    #[test]
    fn stale_snapshot_warns_after_the_limit_only() {
        let snap = snapshot(Vec::new());
        let exported = chrono::DateTime::parse_from_rfc3339(&snap.exported_at)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let fresh = exported + chrono::Duration::hours(SNAPSHOT_STALE_HOURS - 1);
        let stale = exported + chrono::Duration::hours(SNAPSHOT_STALE_HOURS + 1);
        assert!(snap.stale_warning(fresh).is_none());
        assert!(
            snap.stale_warning(stale)
                .is_some_and(|w| w.contains("25h old"))
        );
        let mut broken = snapshot(Vec::new());
        broken.exported_at = "yesterday".to_string();
        assert!(broken.stale_warning(stale).is_some());
    }
}
