//! Shared Closure Kernel V1 contracts.
//!
//! K1 deliberately contains identity, evidence, persistence and operator RPC
//! contracts only. Destination mutation and cleanup executors are later-slice
//! capabilities; their request/result shapes are predeclared here so V84 can
//! be complete without making either route callable.

#![allow(clippy::missing_errors_doc)]

use chrono::{DateTime, Utc};
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

use crate::program_runs::{canonical_program_run_json, program_run_fingerprint};
use crate::types::{SessionProvider, Sha256Digest};
use crate::verification_manifest::{VerificationManifest, parse_closure_v2_for_source};

pub const CLOSURE_CHILD_OUTPUT_SCHEMA_VERSION: u32 = 1;
pub const CLOSURE_REVIEW_ARTIFACT_SCHEMA_VERSION: u32 = 1;
pub const CLOSURE_FINAL_GATE_MAX_ATTEMPTS_V1: u8 = 3;
pub const CLOSURE_OUTPUT_RECOVERY_PAGE_SIZE_V1: u32 = 64;
pub const CLOSURE_OUTPUT_RECOVERY_MAX_PAGES_V1: u32 = 4;
pub const CLOSURE_OUTPUT_RECOVERY_TIME_BUDGET_MS_V1: u64 = 250;
pub const CLOSURE_OUTPUT_RETRY_DELAY_SECONDS_V1: u64 = 5;
pub const CLOSURE_MAX_TEXT_BYTES_V1: usize = 16 * 1024;
pub const CLOSURE_MAX_ARTIFACT_BYTES_V1: usize = 1024 * 1024;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub const fn new(value: Uuid) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn into_uuid(self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_canonical_uuid(value).map(Self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                parse_canonical_uuid(&value)
                    .map(Self)
                    .map_err(serde::de::Error::custom)
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

/// Parse only the lowercase, hyphenated canonical UUID representation used by
/// Closure wire and durable identities.
pub fn parse_canonical_uuid(value: &str) -> Result<Uuid, String> {
    let parsed = Uuid::parse_str(value).map_err(|error| error.to_string())?;
    if value.len() != 36 || parsed.to_string() != value {
        return Err("UUID must use lowercase hyphenated canonical text".into());
    }
    Ok(parsed)
}

mod canonical_uuid_serde {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        let value = String::deserialize(deserializer)?;
        parse_canonical_uuid(&value).map_err(serde::de::Error::custom)
    }
}

mod optional_canonical_uuid_serde {
    use super::*;

    pub fn serialize<S: Serializer>(
        value: &Option<Uuid>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(|uuid| uuid.to_string()).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Uuid>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| parse_canonical_uuid(&value).map_err(serde::de::Error::custom))
            .transpose()
    }
}

mod canonical_uuid_vec_serde {
    use super::*;

    pub fn serialize<S: Serializer>(value: &[Uuid], serializer: S) -> Result<S::Ok, S::Error> {
        value
            .iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Uuid>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .into_iter()
            .map(|value| parse_canonical_uuid(&value).map_err(serde::de::Error::custom))
            .collect()
    }
}

uuid_id!(ClosureProgramIdV1);
uuid_id!(ClosureSourceIdV1);
uuid_id!(ClosureEvidenceIdV1);
uuid_id!(ClosureReceiptIdV1);
uuid_id!(ClosureAttemptIdV1);
uuid_id!(ClosureCleanupProofIdV1);
uuid_id!(ClosureCleanupActionIdV1);
uuid_id!(ClosureOperatorRequestIdV1);

macro_rules! validated_string {
    ($name:ident, $parser:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                $parser(&value)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

fn validate_git_sha(value: &str) -> Result<(), String> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(
            "git SHA must contain exactly 40 or 64 lowercase hexadecimal characters".into(),
        );
    }
    Ok(())
}

fn validate_local_ref(value: &str) -> Result<(), String> {
    if !value.starts_with("refs/heads/") || value.len() <= "refs/heads/".len() {
        return Err("Closure branch ref must be fully qualified beneath refs/heads/".into());
    }
    validate_ref_tail(value)
}

fn validate_any_closure_ref(value: &str) -> Result<(), String> {
    if !(value.starts_with("refs/heads/") || value.starts_with("refs/rsi/")) {
        return Err("Closure ref must be fully qualified beneath refs/heads/ or refs/rsi/".into());
    }
    validate_ref_tail(value)
}

fn validate_ref_tail(value: &str) -> Result<(), String> {
    let unsafe_ref = value.contains("..")
        || value.contains("@{")
        || value.ends_with('/')
        || value.ends_with('.')
        || value.ends_with(".lock")
        || value.contains("//")
        || value.bytes().any(|byte| {
            byte <= b' '
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        });
    if unsafe_ref {
        return Err("Closure ref is not a canonical safe local ref".into());
    }
    Ok(())
}

fn validate_repository_identity(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 512 || value.contains('\0') {
        return Err("repository identity must contain 1..=512 bytes without NUL".into());
    }
    Ok(())
}

fn validate_relative_evidence_path(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 1024
        || value.starts_with('/')
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        || value.contains('\0')
        || value.contains('\\')
    {
        return Err("evidence path must be a canonical relative slash path".into());
    }
    Ok(())
}

validated_string!(ClosureGitShaV1, validate_git_sha);
validated_string!(ClosureLocalBranchRefV1, validate_local_ref);
validated_string!(ClosureGitRefV1, validate_any_closure_ref);
validated_string!(ClosureRepositoryIdentityV1, validate_repository_identity);
validated_string!(ClosureEvidencePathV1, validate_relative_evidence_path);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureSourceIdentityV1 {
    pub program_id: ClosureProgramIdV1,
    pub source_id: ClosureSourceIdV1,
    #[serde(with = "canonical_uuid_serde")]
    pub custody_id: Uuid,
    pub custody_generation: u64,
    #[serde(with = "canonical_uuid_serde")]
    pub lineage_root_session_id: Uuid,
    pub repository_identity: ClosureRepositoryIdentityV1,
    pub source_ref: ClosureLocalBranchRefV1,
    pub source_base_sha: ClosureGitShaV1,
    pub destination_ref: ClosureLocalBranchRefV1,
    pub destination_pre_head: ClosureGitShaV1,
    pub staging_ref: ClosureLocalBranchRefV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureChildCorrelationV1 {
    pub program_id: ClosureProgramIdV1,
    pub source_id: ClosureSourceIdV1,
    #[serde(with = "canonical_uuid_serde")]
    pub custody_id: Uuid,
    pub custody_generation: u64,
    #[serde(with = "canonical_uuid_serde")]
    pub lineage_root_session_id: Uuid,
    #[serde(with = "canonical_uuid_serde")]
    pub tip_session_id: Uuid,
    pub rotation_depth: u32,
    #[serde(with = "canonical_uuid_serde")]
    pub model_invocation_id: Uuid,
    pub source_base_sha: ClosureGitShaV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClosureChildOutcomeV1 {
    Committed {
        reported_source_head: ClosureGitShaV1,
    },
    NoChange {
        observed_source_head: ClosureGitShaV1,
        reason: String,
    },
    Blocker {
        code: String,
        reason: String,
        requested_operator_action: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureChildOutputEnvelopeV1 {
    pub schema_version: u32,
    pub correlation: ClosureChildCorrelationV1,
    pub summary: String,
    pub outcome: ClosureChildOutcomeV1,
}

impl ClosureChildOutputEnvelopeV1 {
    pub fn validate(&self) -> Result<(), ClosureEvidenceValidationErrorV1> {
        if self.schema_version != CLOSURE_CHILD_OUTPUT_SCHEMA_VERSION {
            return Err(ClosureEvidenceValidationErrorV1::UnsupportedOutcomeSchema {
                found: self.schema_version,
            });
        }
        validate_bounded_text("summary", &self.summary, CLOSURE_MAX_TEXT_BYTES_V1)?;
        if self.correlation.custody_generation == 0 {
            return Err(ClosureEvidenceValidationErrorV1::InvalidField {
                field: "correlation.custody_generation".into(),
                message: "must be positive".into(),
            });
        }
        match &self.outcome {
            ClosureChildOutcomeV1::Committed { .. } => {}
            ClosureChildOutcomeV1::NoChange { reason, .. } => {
                validate_bounded_text("outcome.reason", reason, CLOSURE_MAX_TEXT_BYTES_V1)?;
            }
            ClosureChildOutcomeV1::Blocker {
                code,
                reason,
                requested_operator_action,
            } => {
                validate_ascii_token("outcome.code", code, 64)?;
                validate_bounded_text("outcome.reason", reason, CLOSURE_MAX_TEXT_BYTES_V1)?;
                validate_bounded_text(
                    "outcome.requested_operator_action",
                    requested_operator_action,
                    CLOSURE_MAX_TEXT_BYTES_V1,
                )?;
            }
        }
        Ok(())
    }
}

pub fn parse_closure_child_output_envelope_v1(
    input: &str,
) -> Result<ClosureChildOutputEnvelopeV1, ClosureEvidenceValidationErrorV1> {
    let envelope: ClosureChildOutputEnvelopeV1 = strict_json_from_str(input)?;
    envelope.validate()?;
    Ok(envelope)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureReviewVerdictV1 {
    Accepted,
    ChangesRequired,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureFindingSeverityV1 {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureFindingDispositionV1 {
    Unresolved,
    Resolved,
    Withdrawn,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureAuditResultV1 {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureReviewerIdentityV1 {
    #[serde(with = "canonical_uuid_serde")]
    pub session_id: Uuid,
    #[serde(with = "canonical_uuid_serde")]
    pub model_invocation_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureReviewFindingV1 {
    pub id: String,
    pub severity: ClosureFindingSeverityV1,
    pub summary: String,
    pub disposition: ClosureFindingDispositionV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureScopePolicyAuditV1 {
    pub scope_result: ClosureAuditResultV1,
    pub policy_result: ClosureAuditResultV1,
    pub reviewed_scope: Vec<String>,
    pub review_policy_digest: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureReviewArtifactV1 {
    pub schema_version: u32,
    pub reviewed_source_head: ClosureGitShaV1,
    pub reviewer: ClosureReviewerIdentityV1,
    pub verdict: ClosureReviewVerdictV1,
    pub findings: Vec<ClosureReviewFindingV1>,
    pub unresolved_finding_ids: Vec<String>,
    pub scope_policy_audit: ClosureScopePolicyAuditV1,
}

impl ClosureReviewArtifactV1 {
    pub fn validate(&self) -> Result<(), ClosureEvidenceValidationErrorV1> {
        if self.schema_version != CLOSURE_REVIEW_ARTIFACT_SCHEMA_VERSION {
            return Err(ClosureEvidenceValidationErrorV1::UnsupportedReviewSchema {
                found: self.schema_version,
            });
        }
        let mut ids = BTreeSet::new();
        let mut unresolved = BTreeSet::new();
        for finding in &self.findings {
            validate_finding_id(&finding.id)?;
            if !ids.insert(finding.id.clone()) {
                return Err(ClosureEvidenceValidationErrorV1::DuplicateFindingId {
                    id: finding.id.clone(),
                });
            }
            validate_bounded_text(
                "findings.summary",
                &finding.summary,
                CLOSURE_MAX_TEXT_BYTES_V1,
            )?;
            match finding.disposition {
                ClosureFindingDispositionV1::Unresolved => {
                    if finding.resolution.is_some() {
                        return Err(ClosureEvidenceValidationErrorV1::InvalidFindingResolution {
                            id: finding.id.clone(),
                        });
                    }
                    unresolved.insert(finding.id.clone());
                }
                _ => {
                    let Some(resolution) = finding.resolution.as_deref() else {
                        return Err(ClosureEvidenceValidationErrorV1::InvalidFindingResolution {
                            id: finding.id.clone(),
                        });
                    };
                    validate_bounded_text(
                        "findings.resolution",
                        resolution,
                        CLOSURE_MAX_TEXT_BYTES_V1,
                    )?;
                }
            }
        }
        let declared = self
            .unresolved_finding_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if declared.len() != self.unresolved_finding_ids.len() || declared != unresolved {
            return Err(ClosureEvidenceValidationErrorV1::InconsistentUnresolvedFindingSet);
        }
        for scope in &self.scope_policy_audit.reviewed_scope {
            validate_bounded_text("scope_policy_audit.reviewed_scope", scope, 1024)?;
        }
        if self.verdict == ClosureReviewVerdictV1::Accepted {
            if self.scope_policy_audit.scope_result != ClosureAuditResultV1::Pass
                || self.scope_policy_audit.policy_result != ClosureAuditResultV1::Pass
            {
                return Err(ClosureEvidenceValidationErrorV1::AcceptedReviewAuditFailed);
            }
            if self.findings.iter().any(|finding| {
                finding.disposition == ClosureFindingDispositionV1::Unresolved
                    && matches!(
                        finding.severity,
                        ClosureFindingSeverityV1::Critical | ClosureFindingSeverityV1::High
                    )
            }) {
                return Err(ClosureEvidenceValidationErrorV1::AcceptedReviewHasBlockingFindings);
            }
        }
        Ok(())
    }

    pub fn canonical_finding_set_json(&self) -> Result<String, String> {
        let mut findings = self.findings.clone();
        findings.sort_by(|left, right| left.id.cmp(&right.id));
        let mut unresolved = self.unresolved_finding_ids.clone();
        unresolved.sort();
        canonical_program_run_json(&serde_json::json!({
            "findings": findings,
            "unresolved_finding_ids": unresolved,
        }))
    }

    pub fn finding_set_digest(&self) -> Result<String, String> {
        let json = self.canonical_finding_set_json()?;
        Ok(program_run_fingerprint(
            "closure-review-findings:v1",
            json.as_bytes(),
        ))
    }
}

pub fn parse_closure_review_artifact_v1(
    input: &str,
) -> Result<ClosureReviewArtifactV1, ClosureEvidenceValidationErrorV1> {
    if input.len() > CLOSURE_MAX_ARTIFACT_BYTES_V1 {
        return Err(ClosureEvidenceValidationErrorV1::ArtifactTooLarge);
    }
    let artifact: ClosureReviewArtifactV1 = strict_json_from_str(input)?;
    artifact.validate()?;
    Ok(artifact)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureEvidenceExpectationV1 {
    pub source_head: ClosureGitShaV1,
    #[serde(with = "canonical_uuid_serde")]
    pub reviewer_session_id: Uuid,
    #[serde(with = "canonical_uuid_serde")]
    pub model_invocation_id: Uuid,
    pub review_policy_digest: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatedClosureEvidenceBundleV1 {
    pub review: ClosureReviewArtifactV1,
    pub manifest: VerificationManifest,
    pub review_digest: String,
    pub finding_set_digest: String,
    pub manifest_digest: String,
}

pub fn validate_closure_evidence_bundle_v1(
    review_json: &str,
    manifest_markdown: &str,
    expected: &ClosureEvidenceExpectationV1,
) -> Result<ValidatedClosureEvidenceBundleV1, ClosureEvidenceValidationErrorV1> {
    let review = parse_closure_review_artifact_v1(review_json)?;
    if review.reviewed_source_head != expected.source_head {
        return Err(ClosureEvidenceValidationErrorV1::StaleReviewedSourceHead);
    }
    if review.reviewer.session_id != expected.reviewer_session_id
        || review.reviewer.model_invocation_id != expected.model_invocation_id
    {
        return Err(ClosureEvidenceValidationErrorV1::ReviewerCorrelationMismatch);
    }
    if review.scope_policy_audit.review_policy_digest != expected.review_policy_digest {
        return Err(ClosureEvidenceValidationErrorV1::ReviewPolicyDigestMismatch);
    }
    let manifest =
        parse_closure_v2_for_source(manifest_markdown, &expected.source_head).map_err(|error| {
            ClosureEvidenceValidationErrorV1::Manifest {
                message: format!("{error:?}"),
            }
        })?;
    let finding_set_digest = review.finding_set_digest().map_err(|message| {
        ClosureEvidenceValidationErrorV1::InvalidField {
            field: "findings".into(),
            message,
        }
    })?;
    Ok(ValidatedClosureEvidenceBundleV1 {
        review,
        manifest,
        review_digest: program_run_fingerprint(
            "closure-review-artifact:v1",
            review_json.as_bytes(),
        ),
        finding_set_digest,
        manifest_digest: program_run_fingerprint(
            "closure-verification-manifest:v2",
            manifest_markdown.as_bytes(),
        ),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ClosureEvidenceValidationErrorV1 {
    #[error("malformed strict JSON: {message}")]
    MalformedJson { message: String },
    #[error("duplicate JSON key `{key}`")]
    DuplicateJsonKey { key: String },
    #[error("unsupported Closure outcome schema {found}")]
    UnsupportedOutcomeSchema { found: u32 },
    #[error("unsupported review schema {found}")]
    UnsupportedReviewSchema { found: u32 },
    #[error("artifact exceeds the Closure evidence size bound")]
    ArtifactTooLarge,
    #[error("invalid field `{field}`: {message}")]
    InvalidField { field: String, message: String },
    #[error("duplicate review finding id `{id}`")]
    DuplicateFindingId { id: String },
    #[error("finding `{id}` has an invalid resolution/disposition combination")]
    InvalidFindingResolution { id: String },
    #[error("unresolved finding ids do not exactly match unresolved findings")]
    InconsistentUnresolvedFindingSet,
    #[error("accepted review has a failed scope or policy audit")]
    AcceptedReviewAuditFailed,
    #[error("accepted review has unresolved critical/high findings")]
    AcceptedReviewHasBlockingFindings,
    #[error("reviewed source head does not equal the sealed source head")]
    StaleReviewedSourceHead,
    #[error("reviewer session/model invocation correlation mismatch")]
    ReviewerCorrelationMismatch,
    #[error("review policy digest mismatch")]
    ReviewPolicyDigestMismatch,
    #[error("Closure manifest validation failed: {message}")]
    Manifest { message: String },
}

fn strict_json_from_str<T>(input: &str) -> Result<T, ClosureEvidenceValidationErrorV1>
where
    T: serde::de::DeserializeOwned,
{
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = NoDuplicateValueSeed
        .deserialize(&mut deserializer)
        .map_err(map_json_error)?;
    deserializer.end().map_err(map_json_error)?;
    serde_json::from_value(value).map_err(map_json_error)
}

fn map_json_error(error: serde_json::Error) -> ClosureEvidenceValidationErrorV1 {
    let message = error.to_string();
    if let Some(key) = message
        .strip_prefix("duplicate JSON key `")
        .and_then(|tail| tail.split_once('`').map(|(key, _)| key.to_string()))
    {
        ClosureEvidenceValidationErrorV1::DuplicateJsonKey { key }
    } else {
        ClosureEvidenceValidationErrorV1::MalformedJson { message }
    }
}

struct NoDuplicateValueSeed;

impl<'de> DeserializeSeed<'de> for NoDuplicateValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(NoDuplicateValueVisitor)
    }
}

struct NoDuplicateValueVisitor;

impl<'de> Visitor<'de> for NoDuplicateValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        NoDuplicateValueSeed.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(NoDuplicateValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON key `{key}`"
                )));
            }
            let value = map.next_value_seed(NoDuplicateValueSeed)?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

fn validate_bounded_text(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ClosureEvidenceValidationErrorV1> {
    if value.trim().is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(ClosureEvidenceValidationErrorV1::InvalidField {
            field: field.into(),
            message: format!("must contain 1..={max_bytes} bytes without NUL"),
        });
    }
    Ok(())
}

fn validate_ascii_token(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ClosureEvidenceValidationErrorV1> {
    if value.is_empty()
        || value.len() > max_bytes
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ClosureEvidenceValidationErrorV1::InvalidField {
            field: field.into(),
            message: format!("must be a 1..={max_bytes} byte ASCII token"),
        });
    }
    Ok(())
}

fn validate_finding_id(value: &str) -> Result<(), ClosureEvidenceValidationErrorV1> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_uppercase()
        && value.bytes().all(|byte| {
            byte.is_ascii_uppercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(ClosureEvidenceValidationErrorV1::InvalidField {
            field: "findings.id".into(),
            message: "must match ^[A-Z][A-Z0-9._-]{0,63}$".into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClosureReviewPolicyV1 {
    RequiredIndependent,
    NotRequired {
        basis: ClosureReviewNotRequiredBasisV1,
        rationale: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureReviewNotRequiredBasisV1 {
    Tier0Deterministic,
    ProvenNoChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureVerificationPolicyV1 {
    pub required_buckets: Vec<String>,
    pub require_all_items_pass: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureVerifierCommandV1 {
    pub argv: Vec<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureVerifierPolicyV1 {
    pub commands: Vec<ClosureVerifierCommandV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureCheckoutRemediationPolicyV1 {
    Refuse,
    ManagedDetachReattach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureDestinationClaimStateV1 {
    Held,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureProgramStateV1 {
    Draft,
    Configured,
    Working,
    OutcomeBlocked,
    AwaitingEvidence,
    Eligible,
    AwaitingIntegration,
    Integrating,
    Conflict,
    GateFailed,
    Integrated,
    QuarantinedCleanupPending,
    CleanupPending,
    CleanupInProgress,
    CleanupFailed,
    Closed,
    QuarantinedClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureSourceStateV1 {
    Working,
    OutcomeCommitted,
    OutcomeNoChange,
    OutcomeBlocker,
    OutcomeBlocked,
    AwaitingEvidence,
    Eligible,
    IntegrationQueued,
    Integrated,
    Retained,
    CleanupComplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureOutputValidationDispositionV1 {
    AcceptedCommitted,
    AcceptedNoChange,
    AcceptedBlocker,
    MissingProviderOutput,
    MalformedOutput,
    CorrelationMismatch,
    BlockedAmbiguousLineage,
    BlockedOutcomeGitMismatch,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationEventProducerKindV1 {
    ProviderAssistantOutput,
    DaemonProviderDiagnostic,
    ProviderOther,
}

impl ConversationEventProducerKindV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderAssistantOutput => "provider_assistant_output",
            Self::DaemonProviderDiagnostic => "daemon_provider_diagnostic",
            Self::ProviderOther => "provider_other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationEventProvenanceV1 {
    pub producer_kind: ConversationEventProducerKindV1,
    #[serde(with = "canonical_uuid_serde")]
    pub model_invocation_id: Uuid,
    pub provider_event_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureOutputIngestionKeyV1 {
    pub source_id: ClosureSourceIdV1,
    #[serde(with = "canonical_uuid_serde")]
    pub tip_session_id: Uuid,
    #[serde(with = "canonical_uuid_serde")]
    pub model_invocation_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureOutputRecoveryCursorV1 {
    pub source_created_at: DateTime<Utc>,
    pub source_id: ClosureSourceIdV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureOutputRecoveryBudgetV1 {
    pub page_size: u32,
    pub max_pages: u32,
    pub time_budget_ms: u64,
}

impl Default for ClosureOutputRecoveryBudgetV1 {
    fn default() -> Self {
        Self {
            page_size: CLOSURE_OUTPUT_RECOVERY_PAGE_SIZE_V1,
            max_pages: CLOSURE_OUTPUT_RECOVERY_MAX_PAGES_V1,
            time_budget_ms: CLOSURE_OUTPUT_RECOVERY_TIME_BUDGET_MS_V1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureOutputRecoveryResultV1 {
    pub examined: u32,
    pub committed: u32,
    pub replayed: u32,
    pub deferred: u32,
    pub next_cursor: Option<ClosureOutputRecoveryCursorV1>,
    pub high_water: Option<ClosureOutputRecoveryCursorV1>,
    pub deadline_reached: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureRefusalCodeV1 {
    InvalidRequest,
    InvalidRepository,
    InvalidLocalRef,
    ProtectedDestinationRequiresPromotion,
    DestinationClaimed,
    ProgramVersionConflict,
    SourceAlreadyLaunched,
    IdempotencyMismatch,
    MissingProviderOutput,
    MalformedOutput,
    StaleModelInvocation,
    BlockedAmbiguousLineage,
    BlockedOutcomeGitMismatch,
    UnsupportedReviewSchema,
    StaleReviewedSha,
    ManifestV1Unbound,
    ManifestSourceHeadMismatch,
    InvalidEvidenceCustody,
    ReviewPolicyMismatch,
    VerificationPolicyMismatch,
    EvidenceNotEligible,
    K2Unavailable,
    K3Unavailable,
    DestinationCheckedOut,
    DestinationDrift,
    GateAttemptLimitReached,
    DiscardAfterDestinationApplyForbidden,
    DestinationRewritten,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureProgramConfigV1 {
    pub repository_root: PathBuf,
    pub base_ref: ClosureLocalBranchRefV1,
    pub destination_ref: ClosureLocalBranchRefV1,
    pub review_policy: ClosureReviewPolicyV1,
    pub verification_policy: ClosureVerificationPolicyV1,
    pub verifier_policy: ClosureVerifierPolicyV1,
    pub checkout_remediation_policy: ClosureCheckoutRemediationPolicyV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateClosureProgramRequestV1 {
    pub config: ClosureProgramConfigV1,
    #[serde(with = "canonical_uuid_serde")]
    pub idempotency_key: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateClosureProgramResultV1 {
    pub program_id: ClosureProgramIdV1,
    pub version: u64,
    pub repository_identity: ClosureRepositoryIdentityV1,
    pub base_sha: ClosureGitShaV1,
    pub destination_pre_head: ClosureGitShaV1,
    pub state: ClosureProgramStateV1,
    pub replayed: bool,
    pub refusal: Option<ClosureRefusalCodeV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateClosureProgramRequestV1 {
    pub program_id: ClosureProgramIdV1,
    pub expected_program_version: u64,
    pub config: ClosureProgramConfigV1,
    #[serde(with = "canonical_uuid_serde")]
    pub idempotency_key: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureProgramMutationResultV1 {
    pub program_id: ClosureProgramIdV1,
    pub version: u64,
    pub review_policy_digest: Sha256Digest,
    pub verification_policy_digest: Sha256Digest,
    pub verifier_policy_digest: Sha256Digest,
    pub replayed: bool,
    pub refusal: Option<ClosureRefusalCodeV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchClosureSourceRequestV1 {
    pub program_id: ClosureProgramIdV1,
    pub title: String,
    pub query: String,
    pub provider: SessionProvider,
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(with = "canonical_uuid_serde")]
    pub idempotency_key: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchClosureSourceResultV1 {
    pub source: ClosureSourceIdentityV1,
    #[serde(with = "canonical_uuid_serde")]
    pub root_session_id: Uuid,
    pub state: ClosureSourceStateV1,
    pub replayed: bool,
    pub refusal: Option<ClosureRefusalCodeV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListClosureProgramsRequestV1 {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub state: Option<ClosureProgramStateV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureProgramSummaryV1 {
    pub program_id: ClosureProgramIdV1,
    pub version: u64,
    pub repository_identity: ClosureRepositoryIdentityV1,
    pub base_ref: ClosureLocalBranchRefV1,
    pub base_sha: ClosureGitShaV1,
    pub destination_ref: ClosureLocalBranchRefV1,
    pub destination_pre_head: ClosureGitShaV1,
    pub destination_claim_state: ClosureDestinationClaimStateV1,
    pub state: ClosureProgramStateV1,
    pub source_id: Option<ClosureSourceIdV1>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureProgramListV1 {
    pub programs: Vec<ClosureProgramSummaryV1>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetClosureProgramRequestV1 {
    pub program_id: Option<ClosureProgramIdV1>,
    pub source_id: Option<ClosureSourceIdV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureProgramDetailV1 {
    pub program: ClosureProgramSummaryV1,
    pub source: Option<ClosureSourceIdentityV1>,
    pub source_state: Option<ClosureSourceStateV1>,
    pub output_validation: Option<ClosureOutputValidationResultV1>,
    pub evidence: Option<AcceptedReviewEvidenceV1>,
    #[serde(default, with = "optional_canonical_uuid_serde")]
    pub integration_queue_item_id: Option<Uuid>,
    pub next_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureOutputValidationResultV1 {
    #[serde(with = "canonical_uuid_serde")]
    pub validation_id: Uuid,
    pub key: ClosureOutputIngestionKeyV1,
    pub conversation_event_id: Option<i64>,
    pub conversation_sequence: Option<i32>,
    pub disposition: ClosureOutputValidationDispositionV1,
    pub source_state: ClosureSourceStateV1,
    pub normalized_envelope: Option<ClosureChildOutputEnvelopeV1>,
    pub normalized_digest: Option<String>,
    pub issues: Vec<String>,
    pub replayed: bool,
    #[serde(default, with = "optional_canonical_uuid_serde")]
    pub integration_queue_item_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClosureEvidenceDispositionV1 {
    RequiredIndependent {
        #[serde(with = "canonical_uuid_serde")]
        reviewer_session_id: Uuid,
        #[serde(with = "canonical_uuid_serde")]
        reviewer_model_invocation_id: Uuid,
        expected_evidence_commit: ClosureGitShaV1,
        review_json_path: ClosureEvidencePathV1,
        manifest_v2_path: ClosureEvidencePathV1,
    },
    NotRequired {
        basis: ClosureReviewNotRequiredBasisV1,
        rationale: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordClosureEvidenceRequestV1 {
    pub source_id: ClosureSourceIdV1,
    pub expected_sealed_source_head: ClosureGitShaV1,
    pub disposition: ClosureEvidenceDispositionV1,
    #[serde(with = "canonical_uuid_serde")]
    pub idempotency_key: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedReviewEvidenceV1 {
    pub evidence_id: ClosureEvidenceIdV1,
    pub source_id: ClosureSourceIdV1,
    pub reviewed_source_head: ClosureGitShaV1,
    pub review_schema_version: Option<u32>,
    pub review_digest: Option<String>,
    pub finding_set_digest: Option<String>,
    pub manifest_schema_version: Option<u32>,
    pub manifest_digest: Option<String>,
    pub manifest_source_head: Option<ClosureGitShaV1>,
    #[serde(default, with = "optional_canonical_uuid_serde")]
    pub reviewer_session_id: Option<Uuid>,
    #[serde(default, with = "optional_canonical_uuid_serde")]
    pub reviewer_model_invocation_id: Option<Uuid>,
    pub reviewer_provider: Option<SessionProvider>,
    pub reviewer_model: Option<String>,
    #[serde(default, with = "optional_canonical_uuid_serde")]
    pub reviewer_custody_id: Option<Uuid>,
    pub reviewer_custody_generation: Option<u64>,
    pub evidence_commit: Option<ClosureGitShaV1>,
    pub review_handoff_event_id: Option<i64>,
    pub review_handoff_digest: Option<String>,
    pub source_ref_before: ClosureGitShaV1,
    pub source_ref_after: ClosureGitShaV1,
    pub source_worktree_head_before: ClosureGitShaV1,
    pub source_worktree_head_after: ClosureGitShaV1,
    pub review_policy_digest: Sha256Digest,
    pub verification_policy_digest: Sha256Digest,
    pub verifier_policy_digest: Sha256Digest,
    pub accepted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureEvidenceResultV1 {
    pub evidence: Option<AcceptedReviewEvidenceV1>,
    pub eligible: bool,
    #[serde(default, with = "optional_canonical_uuid_serde")]
    pub integration_queue_item_id: Option<Uuid>,
    pub replayed: bool,
    pub refusal: Option<ClosureRefusalCodeV1>,
}

macro_rules! deferred_write_contract {
    ($request:ident { $($(#[$field_meta:meta])* $field:ident : $ty:ty),* $(,)? }, $result:ident { $($(#[$result_meta:meta])* $rfield:ident : $rty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $request {
            $($(#[$field_meta])* pub $field: $ty,)*
            #[serde(with = "canonical_uuid_serde")]
            pub idempotency_key: Uuid
        }
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $result {
            $($(#[$result_meta])* pub $rfield: $rty,)*
            pub replayed: bool,
            pub refusal: Option<ClosureRefusalCodeV1>
        }
    };
}

deferred_write_contract!(ResumeClosureFinalizationRequestV1 {
    source_id: ClosureSourceIdV1,
    candidate_receipt_id: ClosureReceiptIdV1,
    detach_target_confirmation: Option<String>
}, ClosureFinalizationResultV1 {
    attempt_id: Option<ClosureAttemptIdV1>,
    #[serde(default, with = "optional_canonical_uuid_serde")]
    consumption_id: Option<Uuid>,
    phase: String,
    disposition: String,
    destination_pre_head: Option<ClosureGitShaV1>,
    destination_post_head: Option<ClosureGitShaV1>,
    receipt_id: Option<ClosureReceiptIdV1>
});

deferred_write_contract!(ApproveClosurePromotionRequestV1 {
    source_id: ClosureSourceIdV1,
    candidate_receipt_id: ClosureReceiptIdV1,
    confirmation: String,
    detach_target_confirmation: Option<String>
}, ClosurePromotionResultV1 {
    finalization: ClosureFinalizationResultV1
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureGateRecheckReasonV1 {
    Environmental,
    Flaky,
}

deferred_write_contract!(RecheckClosureFinalGateRequestV1 {
    source_id: ClosureSourceIdV1,
    failed_destination_receipt_id: ClosureReceiptIdV1,
    failed_post_head: ClosureGitShaV1,
    reason: ClosureGateRecheckReasonV1,
    rationale: String
}, ClosureGateAttemptResultV1 {
    ordinal: Option<u8>,
    limit: u8,
    #[serde(default, with = "optional_canonical_uuid_serde")]
    started_receipt_id: Option<Uuid>,
    #[serde(default, with = "optional_canonical_uuid_serde")]
    result_receipt_id: Option<Uuid>,
    gated_head: Option<ClosureGitShaV1>,
    disposition: String,
    remaining_attempts: u8
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureQuarantineReasonV1 {
    CandidateBad,
    PolicyIncompatible,
    ForwardRepairRequired,
}

deferred_write_contract!(SupersedeClosureGateFailureRequestV1 {
    source_id: ClosureSourceIdV1,
    failed_destination_receipt_id: ClosureReceiptIdV1,
    expected_current_destination_head: ClosureGitShaV1,
    reason: ClosureQuarantineReasonV1,
    rationale: String,
    confirmation: String
}, ClosureGateFailureSettlementResultV1 {
    settlement_attempt_id: Option<ClosureAttemptIdV1>,
    receipt_id: Option<ClosureReceiptIdV1>,
    quarantine_ref: Option<ClosureGitRefV1>,
    quarantine_head: Option<ClosureGitShaV1>,
    destination_claim_state: ClosureDestinationClaimStateV1,
    custody_state: String,
    cleanup_state: String
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureDiscardReasonV1 {
    Obsolete,
    Superseded,
    Rejected,
    Abandoned,
}

deferred_write_contract!(RecordClosureDiscardRequestV1 {
    source_id: ClosureSourceIdV1,
    expected_source_head: ClosureGitShaV1,
    reason: ClosureDiscardReasonV1,
    rationale: String,
    confirmation: String
}, ClosureDiscardResultV1 {
    discard_proof_id: Option<ClosureCleanupProofIdV1>,
    state: String
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewClosureCleanupRequestV1 {
    pub source_id: ClosureSourceIdV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureCleanupPreviewV1 {
    #[serde(with = "canonical_uuid_serde")]
    pub preview_id: Uuid,
    pub source_id: ClosureSourceIdV1,
    pub inventory_digest: String,
    pub expires_at: DateTime<Utc>,
    pub worktree_root: PathBuf,
    pub source_ref: ClosureLocalBranchRefV1,
    pub staging_ref: ClosureLocalBranchRefV1,
    pub retained_quarantine_ref: Option<ClosureGitRefV1>,
    #[serde(default, with = "canonical_uuid_vec_serde")]
    pub session_ids: Vec<Uuid>,
    pub eligible: bool,
    pub refusal: Option<ClosureRefusalCodeV1>,
}

deferred_write_contract!(ExecuteClosureCleanupRequestV1 {
    source_id: ClosureSourceIdV1,
    #[serde(with = "canonical_uuid_serde")]
    preview_id: Uuid,
    preview_digest: String,
    confirmation: String
}, ClosureCleanupResultV1 {
    action_id: Option<ClosureCleanupActionIdV1>,
    phase: String,
    completed_targets: Vec<String>,
    remaining_targets: Vec<String>,
    error: Option<String>,
    custody_state: String,
    program_state: ClosureProgramStateV1,
    destination_claim_state: ClosureDestinationClaimStateV1
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureIntegrationReceiptV1 {
    pub receipt_id: ClosureReceiptIdV1,
    pub parent_receipt_id: Option<ClosureReceiptIdV1>,
    pub program_id: ClosureProgramIdV1,
    pub source_id: ClosureSourceIdV1,
    pub kind: String,
    pub method: ClosureIntegrationMethodV1,
    pub source_base_sha: ClosureGitShaV1,
    pub source_head: ClosureGitShaV1,
    pub staging_pre_head: Option<ClosureGitShaV1>,
    pub staging_post_head: Option<ClosureGitShaV1>,
    pub destination_pre_head: Option<ClosureGitShaV1>,
    pub expected_destination_pre_head: Option<ClosureGitShaV1>,
    pub destination_post_head: Option<ClosureGitShaV1>,
    pub evidence_id: Option<ClosureEvidenceIdV1>,
    pub review_digest: Option<String>,
    pub manifest_digest: Option<String>,
    pub review_policy_digest: Sha256Digest,
    pub verification_policy_digest: Sha256Digest,
    pub verifier_policy_digest: Sha256Digest,
    pub disposition: String,
    pub actor: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureIntegrationMethodV1 {
    FastForward,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureGateAttemptReceiptV1 {
    #[serde(with = "canonical_uuid_serde")]
    pub receipt_id: Uuid,
    pub integration_attempt_id: ClosureAttemptIdV1,
    pub ordinal: u8,
    pub phase: String,
    pub gated_head: ClosureGitShaV1,
    pub verifier_policy_digest: Sha256Digest,
    pub disposition: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureQuarantineReceiptV1 {
    pub receipt_id: ClosureReceiptIdV1,
    pub failed_destination_receipt_id: ClosureReceiptIdV1,
    pub quarantine_ref: ClosureGitRefV1,
    pub quarantine_head: ClosureGitShaV1,
    pub reason: ClosureQuarantineReasonV1,
    pub actor: String,
    pub created_at: DateTime<Utc>,
}

#[must_use]
pub fn closure_request_fingerprint<T: Serialize>(
    method: &str,
    request: &T,
) -> Result<String, String> {
    let canonical = canonical_program_run_json(request)?;
    Ok(program_run_fingerprint(
        &format!("closure-operator:{method}:v1"),
        canonical.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(character: char) -> ClosureGitShaV1 {
        ClosureGitShaV1::parse(character.to_string().repeat(40)).expect("test sha")
    }

    fn digest(character: char) -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", character.to_string().repeat(64)))
            .expect("test digest")
    }

    #[test]
    fn refs_and_shas_are_canonical() {
        assert!(ClosureGitShaV1::parse("A".repeat(40)).is_err());
        assert!(ClosureGitShaV1::parse("a".repeat(39)).is_err());
        assert!(ClosureLocalBranchRefV1::parse("rolling").is_err());
        assert!(ClosureLocalBranchRefV1::parse("refs/heads/rolling").is_ok());
        assert!(ClosureLocalBranchRefV1::parse("refs/heads/a..b").is_err());
    }

    #[test]
    fn closure_uuid_ids_accept_only_lowercase_hyphenated_canonical_text() {
        let uuid = Uuid::new_v4();
        let canonical = uuid.to_string();
        assert_eq!(
            canonical.parse::<ClosureProgramIdV1>().unwrap(),
            ClosureProgramIdV1::new(uuid)
        );
        assert!(
            canonical
                .to_uppercase()
                .parse::<ClosureProgramIdV1>()
                .is_err()
        );
        assert!(
            format!("{{{canonical}}}")
                .parse::<ClosureProgramIdV1>()
                .is_err()
        );
        assert!(
            format!("urn:uuid:{canonical}")
                .parse::<ClosureProgramIdV1>()
                .is_err()
        );
        assert!(
            uuid.simple()
                .to_string()
                .parse::<ClosureProgramIdV1>()
                .is_err()
        );

        for alternate in [
            canonical.to_uppercase(),
            format!("{{{canonical}}}"),
            format!("urn:uuid:{canonical}"),
            uuid.simple().to_string(),
        ] {
            assert!(
                serde_json::from_str::<ClosureProgramIdV1>(&format!("\"{alternate}\"")).is_err(),
                "alternate UUID spelling must fail serde: {alternate}"
            );
        }

        let reviewer = ClosureReviewerIdentityV1 {
            session_id: uuid,
            model_invocation_id: Uuid::new_v4(),
        };
        let mut wire = serde_json::to_value(reviewer).unwrap();
        wire["session_id"] = serde_json::Value::String(canonical.to_uppercase());
        assert!(
            serde_json::from_value::<ClosureReviewerIdentityV1>(wire).is_err(),
            "plain session/invocation UUID fields share the canonical boundary"
        );
    }

    #[test]
    fn closure_policy_digests_keep_the_canonical_sha256_prefix() {
        let raw = "a".repeat(64);
        assert!(Sha256Digest::parse(&raw).is_err());
        assert!(Sha256Digest::parse(format!("sha256:{raw}")).is_ok());
    }

    #[test]
    fn closure_envelope_rejects_duplicate_keys_and_unknown_fields() {
        let invalid = r#"{"schema_version":1,"schema_version":1,"correlation":{},"summary":"x","outcome":{"kind":"committed","reported_source_head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#;
        assert!(matches!(
            parse_closure_child_output_envelope_v1(invalid),
            Err(ClosureEvidenceValidationErrorV1::DuplicateJsonKey { .. })
        ));

        let program = Uuid::new_v4();
        let source = Uuid::new_v4();
        let custody = Uuid::new_v4();
        let root = Uuid::new_v4();
        let tip = Uuid::new_v4();
        let invocation = Uuid::new_v4();
        let unknown = format!(
            r#"{{"schema_version":1,"correlation":{{"program_id":"{program}","source_id":"{source}","custody_id":"{custody}","custody_generation":1,"lineage_root_session_id":"{root}","tip_session_id":"{tip}","rotation_depth":0,"model_invocation_id":"{invocation}","source_base_sha":"{}","extra":true}},"summary":"done","outcome":{{"kind":"committed","reported_source_head":"{}"}}}}"#,
            sha('a'),
            sha('b')
        );
        assert!(parse_closure_child_output_envelope_v1(&unknown).is_err());
    }

    #[test]
    fn review_artifact_enforces_unresolved_set_and_policy() {
        let artifact = ClosureReviewArtifactV1 {
            schema_version: 1,
            reviewed_source_head: sha('a'),
            reviewer: ClosureReviewerIdentityV1 {
                session_id: Uuid::new_v4(),
                model_invocation_id: Uuid::new_v4(),
            },
            verdict: ClosureReviewVerdictV1::Accepted,
            findings: vec![ClosureReviewFindingV1 {
                id: "F-001".into(),
                severity: ClosureFindingSeverityV1::High,
                summary: "unresolved issue".into(),
                disposition: ClosureFindingDispositionV1::Unresolved,
                resolution: None,
            }],
            unresolved_finding_ids: vec!["F-001".into()],
            scope_policy_audit: ClosureScopePolicyAuditV1 {
                scope_result: ClosureAuditResultV1::Pass,
                policy_result: ClosureAuditResultV1::Pass,
                reviewed_scope: vec!["crates/rsi-common".into()],
                review_policy_digest: digest('a'),
            },
        };
        assert_eq!(
            artifact.validate(),
            Err(ClosureEvidenceValidationErrorV1::AcceptedReviewHasBlockingFindings)
        );
    }

    #[test]
    fn closure_review_rejects_unknown_schema_and_stale_reviewed_sha() {
        let reviewer_session_id = Uuid::new_v4();
        let model_invocation_id = Uuid::new_v4();
        let review_policy_digest = digest('c');
        let artifact = ClosureReviewArtifactV1 {
            schema_version: CLOSURE_REVIEW_ARTIFACT_SCHEMA_VERSION,
            reviewed_source_head: sha('a'),
            reviewer: ClosureReviewerIdentityV1 {
                session_id: reviewer_session_id,
                model_invocation_id,
            },
            verdict: ClosureReviewVerdictV1::Accepted,
            findings: Vec::new(),
            unresolved_finding_ids: Vec::new(),
            scope_policy_audit: ClosureScopePolicyAuditV1 {
                scope_result: ClosureAuditResultV1::Pass,
                policy_result: ClosureAuditResultV1::Pass,
                reviewed_scope: vec!["K1".into()],
                review_policy_digest: review_policy_digest.clone(),
            },
        };
        let review_json = serde_json::to_string(&artifact).expect("review JSON");
        let unknown = review_json.replacen("\"schema_version\":1", "\"schema_version\":99", 1);
        assert_eq!(
            parse_closure_review_artifact_v1(&unknown),
            Err(ClosureEvidenceValidationErrorV1::UnsupportedReviewSchema { found: 99 })
        );

        let manifest = format!(
            "---\nschema_version: 2\nsource_head: {}\nticket: K1\nplan_doc: thoughts/shared/plans/closure.md\ngenerated: 2026-08-14T12:00:00Z\nphases_sealed: [1]\nstatus: verified\n---\n\n# Verification Manifest - K1\n\n## Phase 1 - Closure\n\n### Automated\n- [PASS] exact source\n  satisfies: F-003\n\n### Daemon-level\n- (none)\n\n### TUI manual\n- (none)\n",
            sha('a')
        );
        let expectation = ClosureEvidenceExpectationV1 {
            source_head: sha('b'),
            reviewer_session_id,
            model_invocation_id,
            review_policy_digest,
        };
        assert_eq!(
            validate_closure_evidence_bundle_v1(&review_json, &manifest, &expectation),
            Err(ClosureEvidenceValidationErrorV1::StaleReviewedSourceHead)
        );
    }
}
