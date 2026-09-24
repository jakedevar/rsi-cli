//! Verification manifest schema and validator.
//!
//! The manifest is a markdown file with YAML frontmatter. It is the single
//! source of truth for per-ticket verification state: automated checks,
//! daemon-level checks, and TUI-only manual checks.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::closure_kernel::ClosureGitShaV1;

pub const VERIFICATION_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const VERIFICATION_MANIFEST_SCHEMA_VERSION_V2: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationManifest {
    pub frontmatter: ManifestFrontmatter,
    pub phases: Vec<PhaseManifest>,
}

impl VerificationManifest {
    /// The set of every plan/research linkage key covered by any item in any
    /// phase (union of each item's `satisfies` and `covers`).
    ///
    /// The cross-stage VERIFY pass in [`crate::agent_contract`] uses this to
    /// decide whether a declared plan/research key is satisfied by the
    /// implementation. A manifest with no linkage annotations returns an
    /// empty set — which is exactly why a VERIFY handoff that *declares*
    /// linkage keys then fails the coverage gate (drift is caught).
    #[must_use]
    pub fn covered_linkage_keys(&self) -> BTreeSet<String> {
        self.phases
            .iter()
            .flat_map(|phase| phase.items.iter())
            .flat_map(VerificationItem::linkage_keys)
            .map(str::to_string)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFrontmatter {
    /// Absent in legacy manifests and normalized to V1.
    #[serde(
        default = "manifest_schema_v1",
        skip_serializing_if = "is_manifest_schema_v1"
    )]
    pub schema_version: u32,
    /// Required only by V2. V1 keeps this absent for byte-compatible serde.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_head: Option<ClosureGitShaV1>,
    pub ticket: String,
    pub plan_doc: String,
    #[serde(default)]
    pub branch: Option<String>,
    pub generated: String,
    #[serde(default)]
    pub phases_sealed: Vec<u32>,
    pub status: ManifestStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManifestStatus {
    PendingVerification,
    TuiOnlyPending,
    Verified,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhaseManifest {
    pub number: u32,
    pub title: String,
    pub buckets_present: Vec<Bucket>,
    pub items: Vec<VerificationItem>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Bucket {
    Automated,
    DaemonLevel,
    TuiManual,
}

impl Bucket {
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            Self::Automated => "Automated",
            Self::DaemonLevel => "Daemon-level",
            Self::TuiManual => "TUI manual",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationItem {
    pub bucket: Bucket,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    /// Cross-stage linkage (S6/D1): the research `Finding.id`s and/or
    /// plan-item IDs this verification item *satisfies*. Consumed by the
    /// cross-stage VERIFY pass in [`crate::agent_contract`] to prove that a
    /// declared plan/research linkage key is actually covered by the
    /// implementation's verification. Strict-superset addition: absent in
    /// every pre-existing manifest, so `#[serde(default,
    /// skip_serializing_if = "Option::is_none")]` keeps their bytes stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satisfies: Option<Vec<String>>,
    /// Cross-stage linkage (S6/D1): the research `Finding.id`s and/or
    /// plan-item IDs this verification item *covers* (a softer alias of
    /// `satisfies`; both are honored by the coverage check). Same
    /// strict-superset serialization contract as `satisfies`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covers: Option<Vec<String>>,
}

impl VerificationItem {
    /// The union of this item's `satisfies` and `covers` linkage keys.
    ///
    /// The cross-stage VERIFY pass treats both fields as coverage — an item
    /// that lists a plan/research key under either heading covers it.
    pub fn linkage_keys(&self) -> impl Iterator<Item = &str> {
        self.satisfies
            .iter()
            .flatten()
            .chain(self.covers.iter().flatten())
            .map(String::as_str)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum ItemStatus {
    Pass,
    Fail,
    Pending,
    Checked,
    Unchecked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Validation {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
    pub schema_version: u32,
}

impl Validation {
    const fn ok(schema_version: u32) -> Self {
        Self {
            valid: true,
            errors: Vec::new(),
            schema_version,
        }
    }

    #[allow(clippy::missing_const_for_fn)]
    fn from_errors(schema_version: u32, errors: Vec<ValidationError>) -> Self {
        Self {
            valid: errors.is_empty(),
            errors,
            schema_version,
        }
    }
}

const fn manifest_schema_v1() -> u32 {
    VERIFICATION_MANIFEST_SCHEMA_VERSION
}

const fn is_manifest_schema_v1(value: &u32) -> bool {
    *value == VERIFICATION_MANIFEST_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationError {
    pub field: String,
    pub rule: String,
    pub message: String,
}

impl ValidationError {
    fn new(field: impl Into<String>, rule: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            rule: rule.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ManifestParseError {
    #[error("manifest is invalid")]
    Invalid(Vec<ValidationError>),
}

static PHASE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^##\s+Phase\s+([0-9]+)(?:\s+[—-]\s*(.*))?\s*$")
        .unwrap_or_else(|err| panic!("valid phase regex: {err}"))
});
static SUBTASK_PHASE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^##\s+Phase\s+[0-9]+.*sub[- ]?task")
        .unwrap_or_else(|err| panic!("valid subtask regex: {err}"))
});
static BUCKET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^###\s+(.+?)\s*$").unwrap_or_else(|err| panic!("valid bucket regex: {err}"))
});

/// Parse a manifest after validating its frontmatter and phase body.
///
/// # Errors
///
/// Returns [`ManifestParseError::Invalid`] when frontmatter, bucket headings,
/// daemon checks, or phase sealing rules fail validation.
pub fn parse(input: &str) -> Result<VerificationManifest, ManifestParseError> {
    let validation = validate(input);
    if !validation.valid {
        return Err(ManifestParseError::Invalid(validation.errors));
    }

    let (yaml, body) = match split_frontmatter(input) {
        Ok(parts) => parts,
        Err(err) => {
            return Err(ManifestParseError::Invalid(vec![ValidationError::new(
                "frontmatter",
                "Frontmatter",
                err,
            )]));
        }
    };
    let frontmatter = serde_yaml_ng::from_str::<ManifestFrontmatter>(yaml).map_err(|err| {
        ManifestParseError::Invalid(vec![ValidationError::new(
            "frontmatter",
            "Schema",
            format!("frontmatter does not match verification manifest schema: {err}"),
        )])
    })?;
    let phases = parse_phases(body);
    Ok(VerificationManifest {
        frontmatter,
        phases,
    })
}

/// Validate a verification manifest markdown document.
#[must_use]
pub fn validate(input: &str) -> Validation {
    let (yaml, body) = match split_frontmatter(input) {
        Ok(parts) => parts,
        Err(err) => {
            return Validation::from_errors(
                VERIFICATION_MANIFEST_SCHEMA_VERSION,
                vec![ValidationError::new("frontmatter", "Frontmatter", err)],
            );
        }
    };

    let mut errors = Vec::new();
    let frontmatter = validate_frontmatter(yaml, &mut errors);
    let schema_version = frontmatter
        .as_ref()
        .map_or(VERIFICATION_MANIFEST_SCHEMA_VERSION, |value| {
            value.schema_version
        });
    validate_body(body, frontmatter.as_ref(), &mut errors);

    if errors.is_empty() {
        Validation::ok(schema_version)
    } else {
        Validation::from_errors(schema_version, errors)
    }
}

fn split_frontmatter(input: &str) -> Result<(&str, &str), &'static str> {
    let mut lines = input.lines();
    let Some(first) = lines.next() else {
        return Err("missing YAML frontmatter fence");
    };
    if first.trim() != "---" {
        return Err("manifest must start with YAML frontmatter fence");
    }

    let mut yaml_end_byte = None;
    let mut offset = first.len() + 1;
    for line in lines {
        if line.trim() == "---" {
            yaml_end_byte = Some(offset);
            break;
        }
        offset += line.len() + 1;
    }

    let Some(end) = yaml_end_byte else {
        return Err("missing closing YAML frontmatter fence");
    };

    let yaml = &input[first.len() + 1..end];
    let body_start = (end + 4).min(input.len());
    Ok((yaml, &input[body_start..]))
}

fn validate_frontmatter(
    yaml: &str,
    errors: &mut Vec<ValidationError>,
) -> Option<ManifestFrontmatter> {
    let value = match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(yaml) {
        Ok(value) => value,
        Err(err) => {
            errors.push(ValidationError::new(
                "frontmatter",
                "Parse",
                format!("malformed YAML frontmatter: {err}"),
            ));
            return None;
        }
    };

    let Some(map) = value.as_mapping() else {
        errors.push(ValidationError::new(
            "frontmatter",
            "Type",
            "frontmatter must be a YAML mapping",
        ));
        return None;
    };

    let schema_version = map
        .get(serde_yaml_ng::Value::String("schema_version".to_string()))
        .and_then(serde_yaml_ng::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(VERIFICATION_MANIFEST_SCHEMA_VERSION);

    if !matches!(
        schema_version,
        VERIFICATION_MANIFEST_SCHEMA_VERSION | VERIFICATION_MANIFEST_SCHEMA_VERSION_V2
    ) {
        errors.push(ValidationError::new(
            "schema_version",
            "SupportedVersion",
            format!("unsupported verification manifest schema version {schema_version}"),
        ));
        return None;
    }

    if schema_version == VERIFICATION_MANIFEST_SCHEMA_VERSION_V2 {
        const V2_KEYS: [&str; 8] = [
            "schema_version",
            "source_head",
            "ticket",
            "plan_doc",
            "branch",
            "generated",
            "phases_sealed",
            "status",
        ];
        for key in map.keys().filter_map(serde_yaml_ng::Value::as_str) {
            if !V2_KEYS.contains(&key) {
                errors.push(ValidationError::new(
                    key,
                    "UnknownField",
                    format!("unknown V2 frontmatter key `{key}`"),
                ));
            }
        }
        if !map.contains_key(serde_yaml_ng::Value::String("source_head".to_string())) {
            errors.push(ValidationError::new(
                "source_head",
                "Presence",
                "V2 requires `source_head`",
            ));
        }
    }

    for required in ["ticket", "plan_doc", "generated", "phases_sealed", "status"] {
        if !map.contains_key(serde_yaml_ng::Value::String(required.to_string())) {
            errors.push(ValidationError::new(
                required,
                "Presence",
                format!("missing required frontmatter key `{required}`"),
            ));
        }
    }

    match serde_yaml_ng::from_str::<ManifestFrontmatter>(yaml) {
        Ok(frontmatter) => {
            if !frontmatter.generated.contains('T') {
                errors.push(ValidationError::new(
                    "generated",
                    "Format",
                    "`generated` must be an ISO-like timestamp",
                ));
            }
            if schema_version == VERIFICATION_MANIFEST_SCHEMA_VERSION_V2
                && frontmatter.source_head.is_none()
            {
                errors.push(ValidationError::new(
                    "source_head",
                    "CanonicalGitSha",
                    "V2 source_head must be a full canonical Git SHA",
                ));
            }
            Some(frontmatter)
        }
        Err(err) => {
            errors.push(ValidationError::new(
                "frontmatter",
                "Schema",
                format!("frontmatter does not match verification manifest schema: {err}"),
            ));
            None
        }
    }
}

/// Parse strict head-bound manifest V2 for Closure evidence admission.
///
/// Generic [`parse`] remains V1-compatible. Closure deliberately refuses V1
/// even when a caller supplies the expected SHA out of band.
pub fn parse_closure_v2_for_source(
    input: &str,
    expected_source_head: &ClosureGitShaV1,
) -> Result<VerificationManifest, ManifestParseError> {
    let manifest = parse(input)?;
    if manifest.frontmatter.schema_version != VERIFICATION_MANIFEST_SCHEMA_VERSION_V2 {
        return Err(ManifestParseError::Invalid(vec![ValidationError::new(
            "schema_version",
            "ManifestV1Unbound",
            "Closure evidence requires verification manifest schema_version: 2",
        )]));
    }
    if manifest.frontmatter.source_head.as_ref() != Some(expected_source_head) {
        return Err(ManifestParseError::Invalid(vec![ValidationError::new(
            "source_head",
            "ExactHeadMatch",
            "V2 source_head does not equal the sealed Closure source head",
        )]));
    }
    Ok(manifest)
}

fn validate_body(
    body: &str,
    frontmatter: Option<&ManifestFrontmatter>,
    errors: &mut Vec<ValidationError>,
) {
    for (idx, line) in body.lines().enumerate() {
        if SUBTASK_PHASE_RE.is_match(line.trim()) {
            errors.push(ValidationError::new(
                format!("body:{}", idx + 1),
                "NoMidPhaseHeadings",
                "phase headings may not encode sub-task or mid-phase manifest entries",
            ));
        }
    }

    let phases = parse_phases(body);
    for phase in &phases {
        for bucket in [Bucket::Automated, Bucket::DaemonLevel, Bucket::TuiManual] {
            if !phase.buckets_present.contains(&bucket) {
                errors.push(ValidationError::new(
                    format!("phase_{}.{}", phase.number, bucket.heading()),
                    "RequiredBucketHeading",
                    format!(
                        "Phase {} is missing `### {}` heading",
                        phase.number,
                        bucket.heading()
                    ),
                ));
            }
        }

        for item in phase
            .items
            .iter()
            .filter(|item| item.bucket == Bucket::DaemonLevel)
        {
            if item.check.as_deref().unwrap_or_default().trim().is_empty() {
                errors.push(ValidationError::new(
                    format!("phase_{}.daemon_level", phase.number),
                    "DaemonCheck",
                    format!("daemon-level item `{}` is missing `check:`", item.title),
                ));
            }
            if item
                .expected
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            {
                errors.push(ValidationError::new(
                    format!("phase_{}.daemon_level", phase.number),
                    "DaemonExpected",
                    format!("daemon-level item `{}` is missing `expected:`", item.title),
                ));
            }
        }
    }

    if let Some(frontmatter) = frontmatter {
        validate_phase_seal_consistency(body, frontmatter, errors);
    }
}

fn validate_phase_seal_consistency(
    body: &str,
    frontmatter: &ManifestFrontmatter,
    errors: &mut Vec<ValidationError>,
) {
    let parsed = parse_phases(body)
        .into_iter()
        .map(|phase| phase.number)
        .collect::<BTreeSet<_>>();
    let sealed = frontmatter
        .phases_sealed
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();

    if parsed != sealed {
        errors.push(ValidationError::new(
            "phases_sealed",
            "SealConsistency",
            format!("`phases_sealed` ({sealed:?}) must match parsed phase blocks ({parsed:?})"),
        ));
    }
}

fn parse_phases(body: &str) -> Vec<PhaseManifest> {
    let mut phases = Vec::new();
    let mut current_phase: Option<PhaseManifest> = None;
    let mut current_bucket: Option<Bucket> = None;
    let mut pending_item: Option<VerificationItem> = None;

    for line in body.lines() {
        let trimmed = line.trim();

        if let Some(caps) = PHASE_RE.captures(trimmed) {
            flush_item(&mut current_phase, &mut pending_item);
            if let Some(phase) = current_phase.take() {
                phases.push(phase);
            }
            let number = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let title = caps
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            current_phase = Some(PhaseManifest {
                number,
                title,
                buckets_present: Vec::new(),
                items: Vec::new(),
            });
            current_bucket = None;
            continue;
        }

        if let Some(caps) = BUCKET_RE.captures(trimmed) {
            flush_item(&mut current_phase, &mut pending_item);
            current_bucket = caps.get(1).and_then(|m| bucket_from_heading(m.as_str()));
            if let (Some(phase), Some(bucket)) = (&mut current_phase, current_bucket)
                && !phase.buckets_present.contains(&bucket)
            {
                phase.buckets_present.push(bucket);
            }
            continue;
        }

        let Some(bucket) = current_bucket else {
            continue;
        };
        if current_phase.is_none() {
            continue;
        }

        if let Some(item_text) = trimmed.strip_prefix("- ") {
            let item_text = item_text.trim();
            if is_empty_bucket_item(item_text) {
                continue;
            }
            if is_field_line(item_text) {
                attach_item_field(&mut pending_item, item_text);
                continue;
            }
            flush_item(&mut current_phase, &mut pending_item);
            pending_item = Some(parse_item_header(bucket, item_text));
        } else if is_field_line(trimmed) {
            attach_item_field(&mut pending_item, trimmed);
        }
    }

    flush_item(&mut current_phase, &mut pending_item);
    if let Some(phase) = current_phase {
        phases.push(phase);
    }
    phases
}

fn bucket_from_heading(heading: &str) -> Option<Bucket> {
    let normalized = heading.trim().to_ascii_lowercase();
    if normalized.starts_with("automated") {
        Some(Bucket::Automated)
    } else if normalized.starts_with("daemon-level") || normalized.starts_with("daemon level") {
        Some(Bucket::DaemonLevel)
    } else if normalized.starts_with("tui manual") {
        Some(Bucket::TuiManual)
    } else {
        None
    }
}

fn parse_item_header(bucket: Bucket, text: &str) -> VerificationItem {
    let (status, title) = parse_status_prefix(text);
    VerificationItem {
        bucket,
        title: title.to_string(),
        status,
        check: None,
        expected: None,
        actual: None,
        satisfies: None,
        covers: None,
    }
}

fn parse_status_prefix(text: &str) -> (Option<ItemStatus>, &str) {
    for (prefix, status) in [
        ("[PASS]", ItemStatus::Pass),
        ("[FAIL]", ItemStatus::Fail),
        ("[PENDING]", ItemStatus::Pending),
        ("[x]", ItemStatus::Checked),
        ("[ ]", ItemStatus::Unchecked),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return (Some(status), rest.trim());
        }
    }
    (None, text.trim())
}

fn is_empty_bucket_item(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    normalized.starts_with("(none") || normalized == "none"
}

fn is_field_line(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("check:")
        || lower.starts_with("expected:")
        || lower.starts_with("actual:")
        || lower.starts_with("satisfies:")
        || lower.starts_with("covers:")
}

/// Split a comma-separated linkage value into trimmed, non-empty keys.
///
/// Tolerates an optional `[...]` wrapper so `satisfies: [F-1, F-2]` and
/// `satisfies: F-1, F-2` both parse.
fn parse_linkage_keys(value: &str) -> Vec<String> {
    let trimmed = value.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed
        .split(',')
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect()
}

fn attach_item_field(item: &mut Option<VerificationItem>, text: &str) {
    let Some(item) = item else {
        return;
    };
    if let Some((key, value)) = text.split_once(':') {
        match key.trim().to_ascii_lowercase().as_str() {
            "check" => item.check = Some(value.trim().to_string()),
            "expected" => item.expected = Some(value.trim().to_string()),
            "actual" => item.actual = Some(value.trim().to_string()),
            "satisfies" => {
                let keys = parse_linkage_keys(value);
                if !keys.is_empty() {
                    item.satisfies = Some(keys);
                }
            }
            "covers" => {
                let keys = parse_linkage_keys(value);
                if !keys.is_empty() {
                    item.covers = Some(keys);
                }
            }
            _ => {}
        }
    }
}

fn flush_item(phase: &mut Option<PhaseManifest>, item: &mut Option<VerificationItem>) {
    if let (Some(phase), Some(item)) = (phase, item.take()) {
        phase.items.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> &'static str {
        r"---
ticket: V1.1
plan_doc: thoughts/shared/plans/example.md
generated: 2026-05-06T14:23:00Z
phases_sealed: [1]
status: pending_verification
---

# Verification Manifest - V1.1

## Phase 1 - foundation

### Automated
- cargo test -p rsi-common manifest_corpus

### Daemon-level
- [PENDING] rsi-rpc ListSessions returns JSON
  - check: RSI_DAEMON_SOCKET_PATH=/tmp/rsi.sock rsi-rpc ListSessions
  - expected: stdout parses as JSON object with result key

### TUI manual
- (none)
"
    }

    fn valid_v2_manifest(source_head: &str) -> String {
        format!(
            r#"---
schema_version: 2
source_head: {source_head}
ticket: K1
plan_doc: thoughts/shared/plans/closure.md
generated: 2026-08-14T12:00:00Z
phases_sealed: [1]
status: verified
---

# Verification Manifest - K1

## Phase 1 - Closure

### Automated
- [PASS] shared parser

### Daemon-level
- [PASS] exact source
  - check: rsi-closure-evidence-validate
  - expected: exact head accepted

### TUI manual
- (none)
"#
        )
    }

    #[test]
    fn parse_valid_manifest() {
        let manifest = match parse(valid_manifest()) {
            Ok(manifest) => manifest,
            Err(err) => panic!("valid manifest should parse: {err}"),
        };
        assert_eq!(manifest.frontmatter.ticket, "V1.1");
        assert_eq!(manifest.frontmatter.schema_version, 1);
        assert!(manifest.frontmatter.source_head.is_none());
        assert_eq!(manifest.phases.len(), 1);
        assert_eq!(manifest.phases[0].items.len(), 2);
    }

    #[test]
    fn closure_v2_requires_exact_head_and_rejects_v1() {
        let expected = ClosureGitShaV1::parse("a".repeat(40)).expect("sha");
        let other = ClosureGitShaV1::parse("b".repeat(40)).expect("sha");
        let manifest = valid_v2_manifest(expected.as_str());
        let parsed = parse_closure_v2_for_source(&manifest, &expected).expect("exact V2");
        assert_eq!(parsed.frontmatter.schema_version, 2);
        assert_eq!(parsed.frontmatter.source_head.as_ref(), Some(&expected));
        assert!(parse_closure_v2_for_source(&manifest, &other).is_err());
        assert!(parse_closure_v2_for_source(valid_manifest(), &expected).is_err());
    }

    #[test]
    fn v2_rejects_unknown_frontmatter_but_v1_behavior_is_unchanged() {
        let expected = ClosureGitShaV1::parse("a".repeat(40)).expect("sha");
        let invalid = valid_v2_manifest(expected.as_str()).replacen(
            "ticket: K1",
            "unknown_key: nope\nticket: K1",
            1,
        );
        assert!(parse(&invalid).is_err());
        assert!(parse(valid_manifest()).is_ok());
    }

    #[test]
    fn serde_round_trip() {
        let manifest = match parse(valid_manifest()) {
            Ok(manifest) => manifest,
            Err(err) => panic!("valid manifest should parse: {err}"),
        };
        let json = match serde_json::to_string(&manifest) {
            Ok(json) => json,
            Err(err) => panic!("manifest should serialize: {err}"),
        };
        let decoded: VerificationManifest = match serde_json::from_str(&json) {
            Ok(decoded) => decoded,
            Err(err) => panic!("manifest should deserialize: {err}"),
        };
        assert_eq!(decoded, manifest);
    }

    fn manifest_with_linkage() -> &'static str {
        r"---
ticket: S6
plan_doc: thoughts/shared/plans/example.md
generated: 2026-06-29T14:23:00Z
phases_sealed: [1]
status: pending_verification
---

# Verification Manifest - S6

## Phase 1 - linkage

### Automated
- cross-stage linkage round-trips
  satisfies: F-001, PLAN-3
  covers: [F-002]

### Daemon-level
- [PENDING] rsi-rpc ListSessions returns JSON
  - check: RSI_DAEMON_SOCKET_PATH=/tmp/rsi.sock rsi-rpc ListSessions
  - expected: stdout parses as JSON object with result key
  - satisfies: F-003

### TUI manual
- (none)
"
    }

    #[test]
    fn manifest_with_satisfies_round_trips() {
        let manifest = match parse(manifest_with_linkage()) {
            Ok(manifest) => manifest,
            Err(err) => panic!("linkage manifest should parse: {err}"),
        };
        // The satisfies/covers keys parsed off the markdown item bodies.
        let keys = manifest.covered_linkage_keys();
        assert!(keys.contains("F-001"), "keys: {keys:?}");
        assert!(keys.contains("F-002"), "keys: {keys:?}");
        assert!(keys.contains("F-003"), "keys: {keys:?}");
        assert!(keys.contains("PLAN-3"), "keys: {keys:?}");

        // And they survive a JSON round-trip unchanged.
        let json = match serde_json::to_string(&manifest) {
            Ok(json) => json,
            Err(err) => panic!("linkage manifest should serialize: {err}"),
        };
        let decoded: VerificationManifest = match serde_json::from_str(&json) {
            Ok(decoded) => decoded,
            Err(err) => panic!("linkage manifest should deserialize: {err}"),
        };
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn legacy_manifest_without_linkage_still_parses() {
        // The pre-S6 manifest carries no satisfies/covers anywhere; it must
        // still parse (backward compat) and expose an empty linkage set.
        let manifest = match parse(valid_manifest()) {
            Ok(manifest) => manifest,
            Err(err) => panic!("legacy manifest should parse: {err}"),
        };
        assert!(
            manifest.covered_linkage_keys().is_empty(),
            "legacy manifest must have no linkage keys"
        );
        for phase in &manifest.phases {
            for item in &phase.items {
                assert!(item.satisfies.is_none());
                assert!(item.covers.is_none());
            }
        }
    }

    #[test]
    fn legacy_manifest_item_json_omits_linkage_fields() {
        // Strict-superset serialization: an item with no linkage must not
        // emit `satisfies`/`covers` keys, keeping pre-S6 output byte-stable.
        let item = VerificationItem {
            bucket: Bucket::Automated,
            title: "legacy".to_string(),
            status: None,
            check: None,
            expected: None,
            actual: None,
            satisfies: None,
            covers: None,
        };
        let json = match serde_json::to_string(&item) {
            Ok(json) => json,
            Err(err) => panic!("item should serialize: {err}"),
        };
        assert!(!json.contains("satisfies"), "json: {json}");
        assert!(!json.contains("covers"), "json: {json}");
    }
}
