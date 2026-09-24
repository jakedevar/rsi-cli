//! Shared v1 contract for durable rolling test-health observations.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub os_image: String,
    pub image_version: String,
    pub architecture: String,
    pub rust_toolchain: String,
    pub test_command: String,
    pub features: Vec<String>,
    pub tmpdir_policy: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunIdentity {
    pub workflow_ref: String,
    pub run_id: String,
    pub attempt: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Shard {
    #[serde(rename = "crate")]
    pub crate_name: String,
    pub state: String,
    pub artifact_digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResult {
    pub id: String,
    pub attempts: u32,
    pub passes: u32,
    pub failures: u32,
    pub failure_signatures: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Exception {
    pub profile_id: String,
    pub test_id: String,
    pub failure_signature: String,
    pub kind: String,
    pub reason: String,
    pub owner_issue: String,
    pub approver: String,
    pub review_reference: String,
    pub max_retries: u32,
    pub min_attempts: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestDisposition {
    Passing,
    AllowedPreexisting,
    Blocking,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Comparison {
    pub test_id: String,
    pub failure_signatures: Vec<String>,
    pub disposition: TestDisposition,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComparisonRequest {
    pub target_commit: String,
    pub profile: Profile,
    pub baseline: Record,
    pub candidate: Vec<TestResult>,
    pub exceptions: Vec<Exception>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema_version: u8,
    pub kind: String,
    pub observed_commit: String,
    pub profile_id: String,
    pub run: RunIdentity,
    pub profile: Profile,
    pub shards: Vec<Shard>,
    pub tests: Vec<TestResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractError(pub String);

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ContractError {}

pub fn jcs<T: Serialize>(value: &T) -> Result<Vec<u8>, ContractError> {
    serde_jcs::to_vec(value).map_err(|error| ContractError(format!("JCS encoding: {error}")))
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn profile_id(profile: &Profile) -> Result<String, ContractError> {
    Ok(sha256(&jcs(profile)?))
}

pub fn record_digest(record: &Record) -> Result<String, ContractError> {
    let mut unsigned = record.clone();
    unsigned.record_digest = None;
    Ok(sha256(&jcs(&unsigned)?))
}

/// Canonical filename used for immutable shard sidecars. Crate names use Cargo's
/// ASCII identifier alphabet, so rejecting other bytes also prevents path traversal.
pub fn shard_artifact_name(crate_name: &str) -> Result<String, ContractError> {
    if crate_name.is_empty()
        || !crate_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ContractError("invalid Cargo crate name".into()));
    }
    Ok(format!("shards/{crate_name}.json"))
}

pub fn validate(record: &Record) -> Result<(), ContractError> {
    let fail = |message: &str| Err(ContractError(message.to_string()));
    if record.schema_version != SCHEMA_VERSION {
        return fail("unsupported_schema");
    }
    if !is_hex40(&record.observed_commit) {
        return fail("observed_commit must be 40 lowercase hexadecimal characters");
    }
    if profile_id(&record.profile)? != record.profile_id {
        return fail("profile_id does not match canonical profile JSON");
    }
    if record.run.attempt == 0
        || uuid::Uuid::parse_str(&record.run.run_id)
            .ok()
            .is_none_or(|id| id.to_string() != record.run.run_id)
    {
        return fail("run identity is incomplete");
    }
    if record.profile.tmpdir_policy != "short-deterministic" {
        return fail("unsupported tmpdir_policy");
    }

    let mut shard_names = BTreeSet::new();
    for shard in &record.shards {
        shard_artifact_name(&shard.crate_name)?;
        if !shard_names.insert(&shard.crate_name) {
            return fail("duplicate shard crate");
        }
        if !matches!(shard.state.as_str(), "pending" | "complete" | "incomplete") {
            return fail("invalid shard state");
        }
        if !is_sha256(&shard.artifact_digest) {
            return fail("invalid shard artifact digest");
        }
    }
    let mut test_ids = BTreeSet::new();
    let comparable_record = matches!(record.kind.as_str(), "complete" | "targeted");
    for test in &record.tests {
        if test.id.split("::").count() < 3 || !test_ids.insert(&test.id) {
            return fail("invalid or duplicate test id");
        }
        if test.attempts == 0
            || test.passes.checked_add(test.failures) != Some(test.attempts)
            || (comparable_record && test.failures > 0 && test.failure_signatures.is_empty())
        {
            return fail("test attempt totals are inconsistent");
        }
        if test
            .failure_signatures
            .iter()
            .any(|signature| !is_sha256(signature))
        {
            return fail("invalid failure signature digest");
        }
    }

    match record.kind.as_str() {
        "pending" => {
            if record.pending_digest.is_some() {
                return fail("pending records cannot carry pending_digest");
            }
            let Some(digest) = record.record_digest.as_deref() else {
                return fail("pending record requires record_digest");
            };
            if !is_sha256(digest) || record_digest(record)? != digest {
                return fail("record_digest does not match canonical record");
            }
        }
        "complete" | "incomplete" | "targeted" => {
            if !record.pending_digest.as_deref().is_some_and(is_sha256) {
                return fail("terminal record requires pending_digest");
            }
            let Some(digest) = record.record_digest.as_deref() else {
                return fail("terminal record requires record_digest");
            };
            if !is_sha256(digest) || record_digest(record)? != digest {
                return fail("record_digest does not match canonical record");
            }
            if record.kind == "complete"
                && record.shards.iter().any(|shard| shard.state != "complete")
            {
                return fail("complete record contains a non-complete shard");
            }
        }
        _ => return fail("invalid record kind"),
    }
    Ok(())
}

/// Compare candidate failures with an exact-target baseline and reviewed exceptions.
/// Missing, stale, incomplete, or profile-mismatched baselines fail closed.
pub fn compare(request: &ComparisonRequest) -> Result<Vec<Comparison>, ContractError> {
    validate(&request.baseline)?;
    if !matches!(request.baseline.kind.as_str(), "complete" | "targeted")
        || request.baseline.observed_commit != request.target_commit
    {
        return Err(ContractError(
            "baseline_unavailable: missing_or_stale".into(),
        ));
    }
    let profile_id = profile_id(&request.profile)?;
    if request.baseline.profile_id != profile_id {
        return Err(ContractError(
            "baseline_unavailable: profile_mismatch".into(),
        ));
    }
    validate_exception_policy(&request.exceptions)?;

    let mut candidate_ids = BTreeSet::new();
    for test in &request.candidate {
        if !candidate_ids.insert(test.id.as_str())
            || test.attempts == 0
            || test.passes.checked_add(test.failures) != Some(test.attempts)
            || test
                .failure_signatures
                .iter()
                .any(|signature| !is_sha256(signature))
        {
            return Err(ContractError(
                "invalid or duplicate candidate test result".into(),
            ));
        }
    }

    let mut baseline = std::collections::HashMap::new();
    for test in &request.baseline.tests {
        baseline.insert(test.id.as_str(), test);
    }
    let mut outcomes = Vec::with_capacity(request.candidate.len());
    for candidate in &request.candidate {
        if candidate.failures == 0 {
            outcomes.push(Comparison {
                test_id: candidate.id.clone(),
                failure_signatures: Vec::new(),
                disposition: TestDisposition::Passing,
            });
            continue;
        }
        let existing = baseline.get(candidate.id.as_str());
        let permitted = !candidate.failure_signatures.is_empty()
            && candidate.failure_signatures.iter().all(|signature| {
                let existed = existing.is_some_and(|test| {
                    test.failures > 0 && test.failure_signatures.contains(signature)
                });
                existed
                    && request.exceptions.iter().any(|exception| {
                        exception.profile_id == profile_id
                            && exception.test_id == candidate.id
                            && exception.failure_signature == *signature
                            && exception.owner_issue.trim().starts_with('#')
                            && !exception.approver.trim().is_empty()
                            && !exception.review_reference.trim().is_empty()
                            && match exception.kind.as_str() {
                                "failure" => exception.max_retries == 0,
                                "flake" => {
                                    exception.max_retries <= 2
                                        && exception.min_attempts >= 3
                                        && existing.is_some_and(|test| {
                                            test.attempts >= exception.min_attempts
                                                && u64::from(test.passes) * 3
                                                    >= u64::from(test.attempts)
                                        })
                                        && candidate.attempts >= 3
                                        && candidate.passes >= 1
                                }
                                _ => false,
                            }
                    })
            });
        outcomes.push(Comparison {
            test_id: candidate.id.clone(),
            failure_signatures: candidate.failure_signatures.clone(),
            disposition: if permitted {
                TestDisposition::AllowedPreexisting
            } else {
                TestDisposition::Blocking
            },
        });
    }
    Ok(outcomes)
}

pub fn validate_exception_policy(exceptions: &[Exception]) -> Result<(), ContractError> {
    let mut keys = BTreeSet::new();
    for exception in exceptions {
        if !is_sha256(&exception.profile_id)
            || !is_sha256(&exception.failure_signature)
            || exception.test_id.split("::").count() < 3
            || !keys.insert((
                exception.profile_id.as_str(),
                exception.test_id.as_str(),
                exception.failure_signature.as_str(),
            ))
            || exception.reason.trim().is_empty()
            || !exception.owner_issue.trim().starts_with('#')
            || !exception.approver.trim().starts_with("manager ")
            || exception.review_reference.trim().is_empty()
            || match exception.kind.as_str() {
                "failure" => exception.max_retries != 0,
                "flake" => exception.max_retries > 2 || exception.min_attempts < 3,
                _ => true,
            }
        {
            return Err(ContractError(
                "invalid or duplicate exception policy entry".into(),
            ));
        }
    }
    Ok(())
}

/// Validate shard sidecar bytes against the digest declared in the terminal record.
pub fn validate_shard_artifacts(
    record: &Record,
    artifacts: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<(), ContractError> {
    validate(record)?;
    if record.kind == "pending" {
        return Ok(());
    }
    for shard in &record.shards {
        let name = shard_artifact_name(&shard.crate_name)?;
        let Some(bytes) = artifacts.get(&name) else {
            return Err(ContractError(format!("missing shard artifact {name}")));
        };
        if sha256(bytes) != shard.artifact_digest {
            return Err(ContractError(format!(
                "shard artifact digest mismatch: {name}"
            )));
        }
    }
    Ok(())
}

fn is_hex40(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> Record {
        let profile = Profile {
            os_image: "linux".into(),
            image_version: "stable".into(),
            architecture: "x86_64".into(),
            rust_toolchain: "1.94.1".into(),
            test_command: "cargo nextest run".into(),
            features: vec![],
            tmpdir_policy: "short-deterministic".into(),
        };
        let shard_bytes = br#"{"tests":[]}"#.to_vec();
        let mut record = Record {
            schema_version: SCHEMA_VERSION,
            kind: "complete".into(),
            observed_commit: "a".repeat(40),
            profile_id: profile_id(&profile).unwrap(),
            run: RunIdentity {
                workflow_ref: "host-observer".into(),
                run_id: "00000000-0000-4000-8000-000000000001".into(),
                attempt: 1,
            },
            profile,
            shards: vec![Shard {
                crate_name: "rsi-common".into(),
                state: "complete".into(),
                artifact_digest: sha256(&shard_bytes),
            }],
            tests: vec![TestResult {
                id: "rsi-common::lib::rolling_health::tests::roundtrip".into(),
                attempts: 1,
                passes: 1,
                failures: 0,
                failure_signatures: vec![],
            }],
            pending_digest: Some(sha256(b"pending")),
            record_digest: None,
        };
        record.record_digest = Some(record_digest(&record).unwrap());
        record
    }

    #[test]
    fn jcs_digests_validate_and_bind_shard_sidecar_bytes() {
        let record = terminal();
        validate(&record).unwrap();
        let path = shard_artifact_name("rsi-common").unwrap();
        let artifacts = [(path.clone(), br#"{"tests":[]}"#.to_vec())]
            .into_iter()
            .collect();
        validate_shard_artifacts(&record, &artifacts).unwrap();

        let wrong = [(path, b"different".to_vec())].into_iter().collect();
        assert!(validate_shard_artifacts(&record, &wrong).is_err());
    }

    #[test]
    fn rejects_unsupported_schema_and_path_like_crate_names() {
        let mut record = terminal();
        record.schema_version = 2;
        assert_eq!(validate(&record).unwrap_err().0, "unsupported_schema");
        assert!(shard_artifact_name("../escape").is_err());
    }

    #[test]
    fn comparator_requires_exact_target_and_reviewed_matching_exception() {
        let mut baseline = terminal();
        let signature = sha256(b"stable failure");
        baseline.tests[0].attempts = 3;
        baseline.tests[0].passes = 1;
        baseline.tests[0].failures = 2;
        baseline.tests[0].failure_signatures = vec![signature.clone()];
        baseline.record_digest = Some(record_digest(&baseline).unwrap());
        let exception = Exception {
            profile_id: baseline.profile_id.clone(),
            test_id: baseline.tests[0].id.clone(),
            failure_signature: signature.clone(),
            kind: "flake".into(),
            reason: "Intermittent failure observed on the accepted baseline".into(),
            owner_issue: "#629".into(),
            approver: "manager c3ddb5b9".into(),
            review_reference: "assignment-1".into(),
            max_retries: 2,
            min_attempts: 3,
        };
        let mut request = ComparisonRequest {
            target_commit: baseline.observed_commit.clone(),
            profile: baseline.profile.clone(),
            baseline,
            candidate: vec![TestResult {
                id: "rsi-common::lib::rolling_health::tests::roundtrip".into(),
                attempts: 3,
                passes: 1,
                failures: 2,
                failure_signatures: vec![signature],
            }],
            exceptions: vec![exception],
        };
        assert_eq!(
            compare(&request).unwrap()[0].disposition,
            TestDisposition::AllowedPreexisting
        );

        request.target_commit = "b".repeat(40);
        assert_eq!(
            compare(&request).unwrap_err().0,
            "baseline_unavailable: missing_or_stale"
        );
    }

    #[test]
    fn exception_policy_requires_a_nonempty_reason_and_roundtrips() {
        let exception = Exception {
            profile_id: sha256(b"profile"),
            test_id: "rsi-common::lib::rolling_health::tests::roundtrip".into(),
            failure_signature: sha256(b"stable failure"),
            kind: "failure".into(),
            reason: "Required audit context for this exception".into(),
            owner_issue: "#629".into(),
            approver: "manager c3ddb5b9".into(),
            review_reference: "assignment-1".into(),
            max_retries: 0,
            min_attempts: 1,
        };

        let json = serde_json::to_vec(&exception).unwrap();
        let decoded: Exception = serde_json::from_slice(&json).unwrap();
        validate_exception_policy(std::slice::from_ref(&decoded)).unwrap();

        let mut blank_reason = decoded;
        blank_reason.reason = " \t ".into();
        assert!(validate_exception_policy(&[blank_reason]).is_err());
    }
}
