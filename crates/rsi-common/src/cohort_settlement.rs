//! Strict shared contract for operator-authorized source-worktree settlement.
//!
//! Apply carries no repository path, branch, target, Session id, or remote
//! option. The daemon derives authority from custody and a fresh audit digest.

use crate::types::Sha256Digest;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use uuid::Uuid;

pub const SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION: u32 = 1;
pub const SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION: u32 = 1;
pub const SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS: usize = 256;
pub const SOURCE_WORKTREE_SETTLEMENT_MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
pub const SOURCE_WORKTREE_SETTLEMENT_MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
pub const SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES: usize = 4096;
pub const SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST: &str =
    "sha256:b50aed72a3b9d4061f8e64f357fb844a53b5eb2cab0e3d096ccc69df32804464";
pub const SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST: &str =
    "sha256:0e5423a9bb812d9269312036f215ed83396b3c8ce6df34c2c682edcbc8efddcd";

/// V2 is deliberately additive: V1 remains the historical single-cohort
/// protocol while V2 carries a server-authenticated, bounded batch cursor.
pub const SOURCE_WORKTREE_BATCH_SCHEMA_VERSION: u32 = 2;
pub const SOURCE_WORKTREE_BATCH_POLICY_VERSION: u32 = 2;
pub const SOURCE_WORKTREE_BATCH_MAX_CURSOR_BYTES: usize = 4096;
pub const SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS: usize = 16_384;
pub const SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS: usize = 1_024;

/// An opaque daemon-issued batch continuation.  The wire contract validates
/// only its shape and size; the daemon verifies the keyed MAC before decoding
/// any claims from its canonical JSON payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceWorktreeBatchCursorV2(pub String);

impl SourceWorktreeBatchCursorV2 {
    pub fn validate(&self) -> Result<(), String> {
        if self.0.len() > SOURCE_WORKTREE_BATCH_MAX_CURSOR_BYTES {
            return Err("source-worktree batch cursor is malformed or exceeds its bound".into());
        }
        let Some(rest) = self.0.strip_prefix("rsi-swc2:") else {
            return Err("source-worktree batch cursor is malformed or exceeds its bound".into());
        };
        let Some((payload, mac)) = rest.split_once(':') else {
            return Err("source-worktree batch cursor is malformed or exceeds its bound".into());
        };
        let is_lower_hex = |value: &str| {
            !value.is_empty()
                && value.len().is_multiple_of(2)
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if !is_lower_hex(payload) || mac.len() != 64 || !is_lower_hex(mac) {
            return Err("source-worktree batch cursor is malformed or exceeds its bound".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSourceWorktreeBatchParamsV2 {
    pub repository_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<SourceWorktreeBatchCursorV2>,
}

impl AuditSourceWorktreeBatchParamsV2 {
    pub fn validate(&self) -> Result<(), String> {
        validate_identity(&self.repository_identity)?;
        if let Some(cursor) = &self.cursor {
            cursor.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplySourceWorktreeBatchParamsV2 {
    pub repository_identity: String,
    pub cursor: SourceWorktreeBatchCursorV2,
    pub plan_digest: Sha256Digest,
    pub authorization: String,
    pub idempotency_key: String,
}

impl ApplySourceWorktreeBatchParamsV2 {
    /// The ordinal is derived only after the daemon has authenticated the
    /// cursor.  Request JSON never chooses an ordinal or a source root.
    pub fn expected_authorization(&self, batch_ordinal: u64) -> String {
        format!(
            "APPLY {} BATCH {} {}",
            self.repository_identity, batch_ordinal, self.plan_digest
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_identity(&self.repository_identity)?;
        self.cursor.validate()?;
        validate_idempotency_key(&self.idempotency_key)?;
        if self.authorization.is_empty()
            || self.authorization.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_DIAGNOSTIC_BYTES
        {
            return Err("authorization is empty or exceeds its bound".into());
        }
        Ok(())
    }

    /// Authorization is meaningful only after the daemon authenticates the
    /// cursor and obtains the ordinal from its claims.
    pub fn validate_authenticated(&self, batch_ordinal: u64) -> Result<(), String> {
        self.validate()?;
        if self.authorization != self.expected_authorization(batch_ordinal) {
            return Err("authorization must exactly match the authenticated batch phrase".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWorktreeBatchItemV2 {
    pub sequence: u32,
    #[serde(with = "canonical_uuid_serde")]
    pub custody_id: Uuid,
    pub custody_generation: u64,
    pub participant_count: u32,
    pub participant_digest: Sha256Digest,
    pub evidence_digest: Sha256Digest,
}

impl SourceWorktreeBatchItemV2 {
    fn validate_wire(&self, expected_sequence: usize) -> Result<(), String> {
        if self.sequence != expected_sequence as u32
            || self.custody_id.is_nil()
            || self.custody_generation == 0
            || self.participant_count == 0
            || self.participant_count as usize > SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS
        {
            return Err("batch item identity or participant evidence is invalid".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceWorktreeBatchRunStateV2 {
    Audited,
    Applying,
    Terminal,
    Refused,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWorktreeBatchRunV2 {
    #[serde(with = "canonical_uuid_serde")]
    pub run_id: Uuid,
    pub repository_identity: String,
    pub cursor: SourceWorktreeBatchCursorV2,
    pub snapshot_digest: Sha256Digest,
    pub snapshot_root_count: u32,
    pub target_ref: String,
    pub target_oid: SourceWorktreeGitOidV1,
    pub plan_digest: Sha256Digest,
    pub batch_ordinal: u64,
    pub has_more: bool,
    pub predecessor_terminal_receipt_digest: Option<Sha256Digest>,
    pub terminal_receipt_digest: Option<Sha256Digest>,
    pub state: SourceWorktreeBatchRunStateV2,
    pub items: Vec<SourceWorktreeBatchItemV2>,
    pub created_at: String,
    pub updated_at: String,
}

impl SourceWorktreeBatchRunV2 {
    pub fn validate_wire(self) -> Result<Self, String> {
        validate_uuid(self.run_id)?;
        validate_identity(&self.repository_identity)?;
        self.cursor.validate()?;
        validate_target_ref(&self.target_ref)?;
        validate_timestamp(&self.created_at)?;
        validate_timestamp(&self.updated_at)?;
        if self.items.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS
            || self.snapshot_root_count as usize > SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS
            || (self.terminal_receipt_digest.is_some()
                != matches!(self.state, SourceWorktreeBatchRunStateV2::Terminal))
        {
            return Err("batch run state or item count is invalid".into());
        }
        let mut participants = 0_usize;
        for (expected, item) in self.items.iter().enumerate() {
            item.validate_wire(expected)?;
            participants = participants
                .checked_add(item.participant_count as usize)
                .ok_or_else(|| "batch participant count overflowed".to_string())?;
        }
        if participants > SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS {
            return Err("batch participant count exceeds its bound".into());
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWorktreeBatchAuditV2 {
    pub schema_version: u32,
    pub policy_version: u32,
    pub repository_identity: String,
    pub cursor: SourceWorktreeBatchCursorV2,
    pub snapshot_digest: Sha256Digest,
    pub snapshot_root_count: u32,
    pub target_ref: String,
    pub target_oid: SourceWorktreeGitOidV1,
    pub plan_digest: Sha256Digest,
    pub batch_ordinal: u64,
    pub has_more: bool,
    pub applyable: bool,
    pub authorization_phrase: Option<String>,
    pub items: Vec<SourceWorktreeBatchItemV2>,
    pub writes: u64,
}

impl SourceWorktreeBatchAuditV2 {
    pub fn validate_wire(self) -> Result<Self, String> {
        if self.schema_version != SOURCE_WORKTREE_BATCH_SCHEMA_VERSION
            || self.policy_version != SOURCE_WORKTREE_BATCH_POLICY_VERSION
        {
            return Err("unsupported source-worktree batch schema or policy version".into());
        }
        validate_identity(&self.repository_identity)?;
        self.cursor.validate()?;
        validate_target_ref(&self.target_ref)?;
        if self.items.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS
            || self.snapshot_root_count as usize > SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS
            || self.writes != 0
        {
            return Err("batch audit exceeds its root bound or wrote state".into());
        }
        let expected_phrase = format!(
            "APPLY {} BATCH {} {}",
            self.repository_identity, self.batch_ordinal, self.plan_digest
        );
        if self.applyable != self.authorization_phrase.is_some()
            || self
                .authorization_phrase
                .as_deref()
                .is_some_and(|phrase| phrase != expected_phrase)
        {
            return Err("batch audit authorization phrase is inconsistent".into());
        }
        let mut participants = 0_usize;
        for (expected, item) in self.items.iter().enumerate() {
            item.validate_wire(expected)?;
            participants = participants
                .checked_add(item.participant_count as usize)
                .ok_or_else(|| "batch participant count overflowed".to_string())?;
        }
        if participants > SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS {
            return Err("batch participant count exceeds its bound".into());
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListSourceWorktreeCohortsParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSourceWorktreeCohortParams {
    pub repository_identity: String,
}

impl AuditSourceWorktreeCohortParams {
    pub fn validate(&self) -> Result<(), String> {
        validate_identity(&self.repository_identity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplySourceWorktreeCohortParams {
    pub repository_identity: String,
    pub plan_digest: Sha256Digest,
    pub authorization: String,
    pub idempotency_key: String,
}

impl ApplySourceWorktreeCohortParams {
    pub fn expected_authorization(&self) -> String {
        format!("APPLY {} {}", self.repository_identity, self.plan_digest)
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_identity(&self.repository_identity)?;
        validate_idempotency_key(&self.idempotency_key)?;
        if self.authorization != self.expected_authorization() {
            return Err("authorization must exactly match the displayed APPLY phrase".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetSourceWorktreeSettlementRunParams {
    #[serde(with = "canonical_uuid_serde")]
    pub run_id: Uuid,
}

impl GetSourceWorktreeSettlementRunParams {
    pub fn validate(&self) -> Result<(), String> {
        validate_uuid(self.run_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWorktreeCohortSummaryV1 {
    pub schema_version: u32,
    pub policy_version: u32,
    pub repository_identity: String,
    pub canonical_repo_dir: String,
    pub live_roots: u32,
    pub terminal_roots: u32,
}

impl SourceWorktreeCohortSummaryV1 {
    pub fn validate_wire(self) -> Result<Self, String> {
        validate_version(self.schema_version, self.policy_version)?;
        validate_identity(&self.repository_identity)?;
        validate_bounded_text(&self.canonical_repo_dir, "canonical repository path", false)?;
        if self.terminal_roots > self.live_roots {
            return Err("cohort summary terminal count exceeds its live inventory".into());
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceWorktreeProofV1 {
    IntegratedAncestor,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceWorktreeDispositionV1 {
    EligibleIntegratedAncestor,
    ActiveOwner,
    NonterminalStatus,
    DirtyWorktree,
    MissingRoot,
    OutsideSandboxBase,
    SymlinkIdentity,
    WorktreeUnregistered,
    HeadMismatch,
    MissingSourceRef,
    SourceRefMismatch,
    CustodyUnverified,
    CustodyGenerationDrift,
    SharedParticipant,
    ReservedEffect,
    ActiveEffect,
    ScheduledDependency,
    SessionPathDependency,
    ProtectedRef,
    TargetUnavailable,
    TargetAmbiguous,
    RetainedNonAncestor,
    InvalidSourceRef,
    IncompleteEnumeration,
    BoundExceeded,
    NoAction,
}

impl SourceWorktreeDispositionV1 {
    pub fn is_eligible(&self) -> bool {
        matches!(self, Self::EligibleIntegratedAncestor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceWorktreeSettlementPhaseV1 {
    Planned,
    IntentCommitted,
    WorktreeRemoved,
    BranchRemoved,
    Settled,
    Refused,
    RecoveryRequired,
    Unattempted,
}

impl SourceWorktreeSettlementPhaseV1 {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Settled | Self::Refused | Self::RecoveryRequired | Self::Unattempted
        )
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::IntentCommitted => "intent_committed",
            Self::WorktreeRemoved => "worktree_removed",
            Self::BranchRemoved => "branch_removed",
            Self::Settled => "settled",
            Self::Refused => "refused",
            Self::RecoveryRequired => "recovery_required",
            Self::Unattempted => "unattempted",
        }
    }
}

impl std::str::FromStr for SourceWorktreeSettlementPhaseV1 {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "planned" => Self::Planned,
            "intent_committed" => Self::IntentCommitted,
            "worktree_removed" => Self::WorktreeRemoved,
            "branch_removed" => Self::BranchRemoved,
            "settled" => Self::Settled,
            "refused" => Self::Refused,
            "recovery_required" => Self::RecoveryRequired,
            "unattempted" => Self::Unattempted,
            _ => return Err(format!("unknown settlement phase {value}")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceWorktreeSettlementRunStateV1 {
    IntentCommitted,
    Applying,
    Settled,
    Partial,
    RecoveryRequired,
    Refused,
}

impl SourceWorktreeSettlementRunStateV1 {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::IntentCommitted => "intent_committed",
            Self::Applying => "applying",
            Self::Settled => "settled",
            Self::Partial => "partial",
            Self::RecoveryRequired => "recovery_required",
            Self::Refused => "refused",
        }
    }
}

impl std::str::FromStr for SourceWorktreeSettlementRunStateV1 {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "intent_committed" => Self::IntentCommitted,
            "applying" => Self::Applying,
            "settled" => Self::Settled,
            "partial" => Self::Partial,
            "recovery_required" => Self::RecoveryRequired,
            "refused" => Self::Refused,
            _ => return Err(format!("unknown settlement run state {value}")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceWorktreeSettlementRefusalV1 {
    AuditDrift,
    IdempotencyConflict,
    CustodyDrift,
    RuntimeOwnerActive,
    RuntimeStateContended,
    WorktreeRemoveFailed,
    WorktreeResidue,
    TargetDrift,
    SourceRefDrift,
    SourceRefInUse,
    RefDeleteFailed,
    ExternalRegistrationRace,
    DatabaseSettlementFailed,
    RecoveryTargetUnavailable,
    RecoveryProofFailed,
}

impl SourceWorktreeSettlementRefusalV1 {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::AuditDrift => "audit_drift",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::CustodyDrift => "custody_drift",
            Self::RuntimeOwnerActive => "runtime_owner_active",
            Self::RuntimeStateContended => "runtime_state_contended",
            Self::WorktreeRemoveFailed => "worktree_remove_failed",
            Self::WorktreeResidue => "worktree_residue",
            Self::TargetDrift => "target_drift",
            Self::SourceRefDrift => "source_ref_drift",
            Self::SourceRefInUse => "source_ref_in_use",
            Self::RefDeleteFailed => "ref_delete_failed",
            Self::ExternalRegistrationRace => "external_registration_race",
            Self::DatabaseSettlementFailed => "database_settlement_failed",
            Self::RecoveryTargetUnavailable => "recovery_target_unavailable",
            Self::RecoveryProofFailed => "recovery_proof_failed",
        }
    }
}

impl std::str::FromStr for SourceWorktreeSettlementRefusalV1 {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "audit_drift" => Self::AuditDrift,
            "idempotency_conflict" => Self::IdempotencyConflict,
            "custody_drift" => Self::CustodyDrift,
            "runtime_owner_active" => Self::RuntimeOwnerActive,
            "runtime_state_contended" => Self::RuntimeStateContended,
            "worktree_remove_failed" => Self::WorktreeRemoveFailed,
            "worktree_residue" => Self::WorktreeResidue,
            "target_drift" => Self::TargetDrift,
            "source_ref_drift" => Self::SourceRefDrift,
            "source_ref_in_use" => Self::SourceRefInUse,
            "ref_delete_failed" => Self::RefDeleteFailed,
            "external_registration_race" => Self::ExternalRegistrationRace,
            "database_settlement_failed" => Self::DatabaseSettlementFailed,
            "recovery_target_unavailable" => Self::RecoveryTargetUnavailable,
            "recovery_proof_failed" => Self::RecoveryProofFailed,
            _ => return Err(format!("unknown settlement refusal {value}")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SourceWorktreeGitOidV1(String);

impl SourceWorktreeGitOidV1 {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_git_oid(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceWorktreeGitOidV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SourceWorktreeGitOidV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWorktreeSettlementCountsV1 {
    pub observed: u32,
    pub eligible: u32,
    pub retained: u32,
    pub settled: u32,
    pub refused: u32,
    pub recovery_required: u32,
    pub unattempted: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWorktreeAuditItemV1 {
    #[serde(with = "canonical_uuid_serde")]
    pub session_id: Uuid,
    pub status: String,
    pub updated_at: String,
    #[serde(with = "canonical_uuid_serde")]
    pub custody_id: Uuid,
    pub custody_generation: u64,
    pub scheduled_dependency_count: u32,
    pub scheduled_dependency_digest: Sha256Digest,
    pub session_path_dependency_count: u32,
    pub session_path_dependency_digest: Sha256Digest,
    pub sandbox_root: String,
    pub source_ref: String,
    pub source_oid: Option<SourceWorktreeGitOidV1>,
    pub clean_state_digest: Option<Sha256Digest>,
    pub proof: SourceWorktreeProofV1,
    pub evidence_digest: Sha256Digest,
    pub disposition: SourceWorktreeDispositionV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWorktreeCohortAuditV1 {
    pub schema_version: u32,
    pub policy_version: u32,
    pub repository_identity: String,
    pub canonical_repo_dir: String,
    pub target_ref: Option<String>,
    pub target_oid: Option<SourceWorktreeGitOidV1>,
    pub plan_digest: Sha256Digest,
    pub authorization_phrase: Option<String>,
    pub writes: u64,
    pub applyable: bool,
    pub counts: SourceWorktreeSettlementCountsV1,
    pub items: Vec<SourceWorktreeAuditItemV1>,
    #[serde(
        default,
        with = "optional_canonical_uuid_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

impl SourceWorktreeCohortAuditV1 {
    pub fn validate_wire(self) -> Result<Self, String> {
        validate_version(self.schema_version, self.policy_version)?;
        validate_identity(&self.repository_identity)?;
        validate_bounded_text(&self.canonical_repo_dir, "canonical repository path", false)?;
        if let Some(run_id) = self.run_id {
            validate_uuid(run_id)?;
        }
        if self.writes != 0 || self.items.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS {
            return Err("audit report wrote state or exceeds the bounded root count".into());
        }
        validate_audit_counts(&self.counts, self.items.len())?;
        validate_bounded_optional(&self.refusal, "audit refusal")?;
        if self.target_ref.is_some() != self.target_oid.is_some() {
            return Err("audit target ref/OID presence is inconsistent".into());
        }
        if let Some(target_ref) = self.target_ref.as_deref() {
            validate_target_ref(target_ref)?;
        }
        if self.applyable != (self.refusal.is_none() && self.counts.eligible > 0) {
            return Err("audit applyable flag is inconsistent with counts/refusal".into());
        }
        let expected_authorization = self
            .applyable
            .then(|| format!("APPLY {} {}", self.repository_identity, self.plan_digest));
        if self.authorization_phrase != expected_authorization {
            return Err("audit authorization phrase is inconsistent with its exact plan".into());
        }
        if self.applyable && self.target_ref.is_none() {
            return Err("applyable audit lacks a complete target observation".into());
        }
        let actual_eligible = self
            .items
            .iter()
            .filter(|item| item.disposition.is_eligible())
            .count() as u32;
        if actual_eligible != self.counts.eligible {
            return Err("audit eligible count does not match item dispositions".into());
        }
        for item in &self.items {
            validate_uuid(item.session_id)?;
            validate_uuid(item.custody_id)?;
            validate_timestamp(&item.updated_at)?;
            validate_bounded_text(&item.sandbox_root, "sandbox root", false)?;
            if item.disposition.is_eligible() {
                validate_source_ref(&item.source_ref)?;
            } else {
                validate_raw_source_ref(&item.source_ref)?;
            }
            validate_bounded_optional(&item.diagnostic, "audit diagnostic")?;
            if item.scheduled_dependency_count > 0
                && !matches!(
                    item.disposition,
                    SourceWorktreeDispositionV1::ScheduledDependency
                        | SourceWorktreeDispositionV1::SessionPathDependency
                )
            {
                return Err(
                    "audit item with an enabled scheduled dependency is not retained".into(),
                );
            }
            if item.disposition == SourceWorktreeDispositionV1::ScheduledDependency
                && item.scheduled_dependency_count == 0
            {
                return Err("scheduled-dependency disposition lacks dependency evidence".into());
            }
            if item.session_path_dependency_count > 0
                && !matches!(
                    item.disposition,
                    SourceWorktreeDispositionV1::ScheduledDependency
                        | SourceWorktreeDispositionV1::SessionPathDependency
                )
            {
                return Err("audit item with a Session path dependency is not retained".into());
            }
            if item.disposition == SourceWorktreeDispositionV1::SessionPathDependency
                && item.session_path_dependency_count == 0
                && item.scheduled_dependency_count == 0
            {
                return Err("Session-path disposition lacks dependency evidence".into());
            }
            if (item.session_path_dependency_count == 0)
                != (item.session_path_dependency_digest.as_str()
                    == SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST)
            {
                return Err("Session-path dependency count and digest are inconsistent".into());
            }
            if (item.scheduled_dependency_count == 0)
                != (item.scheduled_dependency_digest.as_str()
                    == SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST)
            {
                return Err(
                    "scheduled-dependency count and identity digest are inconsistent".into(),
                );
            }
            if item.disposition.is_eligible()
                && (!matches!(item.proof, SourceWorktreeProofV1::IntegratedAncestor)
                    || item.source_oid.is_none()
                    || item.clean_state_digest.is_none()
                    || item.scheduled_dependency_count != 0
                    || item.session_path_dependency_count != 0)
            {
                return Err("eligible audit item lacks its exact proof tuple".into());
            }
            if !item.disposition.is_eligible()
                && matches!(item.proof, SourceWorktreeProofV1::IntegratedAncestor)
            {
                return Err("retained audit item carries an eligible preservation proof".into());
            }
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWorktreeSettlementItemV1 {
    pub sequence: u32,
    #[serde(with = "canonical_uuid_serde")]
    pub session_id: Uuid,
    #[serde(with = "canonical_uuid_serde")]
    pub custody_id: Uuid,
    pub source_ref: String,
    pub expected_source_oid: SourceWorktreeGitOidV1,
    pub phase: SourceWorktreeSettlementPhaseV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_code: Option<SourceWorktreeSettlementRefusalV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_observation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_observation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWorktreeSettlementRunV1 {
    pub schema_version: u32,
    pub policy_version: u32,
    #[serde(with = "canonical_uuid_serde")]
    pub run_id: Uuid,
    pub repository_identity: String,
    pub canonical_repo_dir: String,
    pub target_ref: String,
    pub target_oid: SourceWorktreeGitOidV1,
    pub plan_digest: Sha256Digest,
    pub idempotency_key: String,
    pub state: SourceWorktreeSettlementRunStateV1,
    pub counts: SourceWorktreeSettlementCountsV1,
    pub items: Vec<SourceWorktreeSettlementItemV1>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_error: Option<String>,
}

impl SourceWorktreeSettlementRunV1 {
    pub fn validate_wire(self) -> Result<Self, String> {
        validate_version(self.schema_version, self.policy_version)?;
        validate_uuid(self.run_id)?;
        validate_identity(&self.repository_identity)?;
        validate_bounded_text(&self.canonical_repo_dir, "canonical repository path", false)?;
        validate_target_ref(&self.target_ref)?;
        validate_idempotency_key(&self.idempotency_key)?;
        validate_timestamp(&self.created_at)?;
        validate_timestamp(&self.updated_at)?;
        if let Some(value) = &self.finished_at {
            validate_timestamp(value)?;
        }
        validate_bounded_optional(&self.terminal_error, "terminal error")?;
        let max_roots = u32::try_from(SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS)
            .map_err(|_| "settlement root bound exceeds the wire count type")?;
        let item_count = u32::try_from(self.items.len())
            .map_err(|_| "settlement receipt item count exceeds the wire count type")?;
        let terminal_count = self
            .counts
            .settled
            .checked_add(self.counts.refused)
            .and_then(|count| count.checked_add(self.counts.recovery_required))
            .and_then(|count| count.checked_add(self.counts.unattempted));
        if self.items.is_empty()
            || self.items.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS
            || [
                self.counts.observed,
                self.counts.eligible,
                self.counts.retained,
                self.counts.settled,
                self.counts.refused,
                self.counts.recovery_required,
                self.counts.unattempted,
            ]
            .into_iter()
            .any(|count| count > max_roots)
            || self.counts.eligible != item_count
            || self.counts.eligible.checked_add(self.counts.retained) != Some(self.counts.observed)
            || [
                self.counts.settled,
                self.counts.refused,
                self.counts.recovery_required,
                self.counts.unattempted,
            ]
            .into_iter()
            .any(|count| count > item_count)
            || terminal_count.is_none_or(|count| count > item_count)
        {
            return Err("settlement receipt counts exceed or contradict its inventory".into());
        }
        let mut actual = SourceWorktreeSettlementCountsV1::default();
        actual.observed = self.counts.observed;
        actual.eligible = self.items.len() as u32;
        actual.retained = self.counts.retained;
        for (sequence, item) in self.items.iter().enumerate() {
            if item.sequence as usize != sequence {
                return Err("settlement item sequence is not canonical".into());
            }
            validate_uuid(item.session_id)?;
            validate_uuid(item.custody_id)?;
            validate_source_ref(&item.source_ref)?;
            validate_bounded_optional(&item.before_observation, "before observation")?;
            validate_bounded_optional(&item.after_observation, "after observation")?;
            match item.phase {
                SourceWorktreeSettlementPhaseV1::Settled => actual.settled += 1,
                SourceWorktreeSettlementPhaseV1::Refused => actual.refused += 1,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired => actual.recovery_required += 1,
                SourceWorktreeSettlementPhaseV1::Unattempted => actual.unattempted += 1,
                _ => {}
            }
            let refusal_required = matches!(
                item.phase,
                SourceWorktreeSettlementPhaseV1::Refused
                    | SourceWorktreeSettlementPhaseV1::RecoveryRequired
            );
            let retryable_database_failure = item.phase
                == SourceWorktreeSettlementPhaseV1::BranchRemoved
                && item.refusal_code
                    == Some(SourceWorktreeSettlementRefusalV1::DatabaseSettlementFailed);
            if refusal_required != item.refusal_code.is_some() && !retryable_database_failure {
                return Err("settlement item phase/refusal evidence is inconsistent".into());
            }
        }
        if actual != self.counts {
            return Err("settlement receipt counts do not match item phases".into());
        }
        let terminal = actual
            .settled
            .saturating_add(actual.refused)
            .saturating_add(actual.recovery_required)
            .saturating_add(actual.unattempted);
        let all_terminal = terminal == actual.eligible;
        let expected_state = if all_terminal && actual.recovery_required > 0 {
            SourceWorktreeSettlementRunStateV1::RecoveryRequired
        } else if all_terminal && actual.settled == actual.eligible {
            SourceWorktreeSettlementRunStateV1::Settled
        } else if all_terminal && actual.settled == 0 {
            SourceWorktreeSettlementRunStateV1::Refused
        } else if terminal > 0 {
            SourceWorktreeSettlementRunStateV1::Partial
        } else if self
            .items
            .iter()
            .all(|item| item.phase == SourceWorktreeSettlementPhaseV1::IntentCommitted)
        {
            SourceWorktreeSettlementRunStateV1::IntentCommitted
        } else {
            SourceWorktreeSettlementRunStateV1::Applying
        };
        if self.state != expected_state || self.finished_at.is_some() != all_terminal {
            return Err("settlement run state/timestamps do not match item phases".into());
        }
        Ok(self)
    }
}

pub fn validate_identity(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES {
        return Err(format!(
            "repository_identity must contain 1..={} bytes",
            SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES
        ));
    }
    if value.contains('\0') || value.trim() != value {
        return Err("repository_identity contains invalid bytes or surrounding whitespace".into());
    }
    Ok(())
}

pub fn validate_sha256_digest(value: &str) -> Result<(), String> {
    Sha256Digest::parse(value).map(|_| ())
}

pub fn validate_git_oid(value: &str) -> Result<(), String> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("Git OID must be 40 or 64 lowercase hexadecimal characters".into());
    }
    Ok(())
}

pub fn validate_idempotency_key(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_IDEMPOTENCY_KEY_BYTES {
        return Err(format!(
            "idempotency_key must contain 1..={} bytes",
            SOURCE_WORKTREE_SETTLEMENT_MAX_IDEMPOTENCY_KEY_BYTES
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err("idempotency_key contains unsupported characters".into());
    }
    Ok(())
}

fn validate_uuid(value: Uuid) -> Result<(), String> {
    if value.is_nil() {
        return Err("UUID must be non-nil lowercase hyphenated canonical text".into());
    }
    Ok(())
}

fn validate_version(schema: u32, policy: u32) -> Result<(), String> {
    if schema != SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION
        || policy != SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION
    {
        return Err("unsupported source-worktree settlement version".into());
    }
    Ok(())
}

fn validate_audit_counts(
    counts: &SourceWorktreeSettlementCountsV1,
    items: usize,
) -> Result<(), String> {
    if counts.observed as usize != items
        || counts.eligible.saturating_add(counts.retained) != counts.observed
        || counts
            .settled
            .saturating_add(counts.refused)
            .saturating_add(counts.recovery_required)
            .saturating_add(counts.unattempted)
            != 0
    {
        return Err("audit counts are inconsistent".into());
    }
    Ok(())
}

fn validate_bounded_text(value: &str, label: &str, allow_empty: bool) -> Result<(), String> {
    if (!allow_empty && value.is_empty())
        || value.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_DIAGNOSTIC_BYTES
        || value.contains('\0')
    {
        return Err(format!("{label} is empty, invalid, or exceeds its bound"));
    }
    Ok(())
}

fn validate_bounded_optional(value: &Option<String>, label: &str) -> Result<(), String> {
    if let Some(value) = value {
        validate_bounded_text(value, label, true)?;
    }
    Ok(())
}

pub fn validate_target_ref(value: &str) -> Result<(), String> {
    if !value.starts_with("refs/heads/")
        || value.len() <= "refs/heads/".len()
        || value.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES
    {
        return Err("target ref must be a fully qualified local branch".into());
    }
    validate_ref_tail(value)
}

pub fn validate_source_ref(value: &str) -> Result<(), String> {
    if !value.starts_with("refs/heads/rsi/")
        || value.len() <= "refs/heads/rsi/".len()
        || value.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES
    {
        return Err("source ref must be a fully qualified rsi local branch".into());
    }
    validate_ref_tail(value)
}

fn validate_raw_source_ref(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES
        || value.contains('\0')
    {
        return Err("invalid source-ref evidence is empty, invalid, or exceeds its bound".into());
    }
    Ok(())
}

fn validate_ref_tail(value: &str) -> Result<(), String> {
    if value.contains("..")
        || value.contains("@{")
        || value.ends_with('.')
        || value.ends_with('/')
        || value.contains("//")
        || value.split('/').any(|component| {
            component.is_empty() || component.starts_with('.') || component.ends_with(".lock")
        })
        || value.bytes().any(|byte| {
            byte <= b' '
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        return Err("Git ref is not canonical".into());
    }
    Ok(())
}

fn validate_timestamp(value: &str) -> Result<(), String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(value).map_err(|error| error.to_string())?;
    if parsed
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Nanos, true)
        != value
    {
        return Err("timestamp must be canonical RFC3339 nanoseconds UTC".into());
    }
    Ok(())
}

mod canonical_uuid_serde {
    use super::*;
    pub fn serialize<S: Serializer>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        let value = String::deserialize(deserializer)?;
        let parsed = Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
        if value.len() != 36 || parsed.to_string() != value || parsed.is_nil() {
            return Err(serde::de::Error::custom(
                "UUID must be non-nil lowercase hyphenated canonical text",
            ));
        }
        Ok(parsed)
    }
}

mod optional_canonical_uuid_serde {
    use super::*;
    pub fn serialize<S: Serializer>(
        value: &Option<Uuid>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_some(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Uuid>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| {
                let parsed = Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
                if value.len() != 36 || parsed.to_string() != value || parsed.is_nil() {
                    return Err(serde::de::Error::custom(
                        "UUID must be non-nil lowercase hyphenated canonical text",
                    ));
                }
                Ok(parsed)
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest() -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap()
    }

    fn settlement_run() -> SourceWorktreeSettlementRunV1 {
        let timestamp = "2026-08-24T00:00:00.000000000Z".to_string();
        SourceWorktreeSettlementRunV1 {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            run_id: Uuid::new_v4(),
            repository_identity: "repo:test".into(),
            canonical_repo_dir: "/tmp/repository".into(),
            target_ref: "refs/heads/rolling".into(),
            target_oid: SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap(),
            plan_digest: digest(),
            idempotency_key: "settlement-test".into(),
            state: SourceWorktreeSettlementRunStateV1::IntentCommitted,
            counts: SourceWorktreeSettlementCountsV1 {
                observed: 1,
                eligible: 1,
                retained: 0,
                settled: 0,
                refused: 0,
                recovery_required: 0,
                unattempted: 0,
            },
            items: vec![SourceWorktreeSettlementItemV1 {
                sequence: 0,
                session_id: Uuid::new_v4(),
                custody_id: Uuid::new_v4(),
                source_ref: "refs/heads/rsi/source".into(),
                expected_source_oid: SourceWorktreeGitOidV1::parse("b".repeat(40)).unwrap(),
                phase: SourceWorktreeSettlementPhaseV1::IntentCommitted,
                refusal_code: None,
                before_observation: None,
                after_observation: None,
            }],
            created_at: timestamp.clone(),
            updated_at: timestamp,
            finished_at: None,
            terminal_error: None,
        }
    }

    fn eligible_audit() -> SourceWorktreeCohortAuditV1 {
        let repository_identity = "repo:test".to_string();
        let plan_digest = digest();
        SourceWorktreeCohortAuditV1 {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: repository_identity.clone(),
            canonical_repo_dir: "/tmp/repository".into(),
            target_ref: Some("refs/heads/rolling".into()),
            target_oid: Some(SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap()),
            plan_digest: plan_digest.clone(),
            authorization_phrase: Some(format!("APPLY {repository_identity} {plan_digest}")),
            writes: 0,
            applyable: true,
            counts: SourceWorktreeSettlementCountsV1 {
                observed: 1,
                eligible: 1,
                retained: 0,
                settled: 0,
                refused: 0,
                recovery_required: 0,
                unattempted: 0,
            },
            items: vec![SourceWorktreeAuditItemV1 {
                session_id: Uuid::new_v4(),
                status: "Completed".into(),
                updated_at: "2026-08-24T00:00:00.000000000Z".into(),
                custody_id: Uuid::new_v4(),
                custody_generation: 1,
                scheduled_dependency_count: 0,
                scheduled_dependency_digest: Sha256Digest::parse(
                    SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST,
                )
                .unwrap(),
                session_path_dependency_count: 0,
                session_path_dependency_digest: Sha256Digest::parse(
                    SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST,
                )
                .unwrap(),
                sandbox_root: "/tmp/sandbox".into(),
                source_ref: "refs/heads/rsi/source".into(),
                source_oid: Some(SourceWorktreeGitOidV1::parse("b".repeat(40)).unwrap()),
                clean_state_digest: Some(digest()),
                proof: SourceWorktreeProofV1::IntegratedAncestor,
                evidence_digest: digest(),
                disposition: SourceWorktreeDispositionV1::EligibleIntegratedAncestor,
                diagnostic: None,
            }],
            run_id: None,
            refusal: None,
        }
    }

    #[test]
    fn apply_contract_is_exact_and_cannot_select_targets() {
        let identity = "/tmp/repository/.git".to_string();
        let plan_digest = digest();
        let params = ApplySourceWorktreeCohortParams {
            repository_identity: identity.clone(),
            plan_digest: plan_digest.clone(),
            authorization: format!("APPLY {identity} {plan_digest}"),
            idempotency_key: "operator-2026-08-24".into(),
        };
        params.validate().unwrap();
        for forbidden in [
            "session_id",
            "sandbox_root",
            "branch",
            "target_ref",
            "remote",
        ] {
            let mut value = serde_json::to_value(&params).unwrap();
            value[forbidden] = json!("forbidden");
            assert!(serde_json::from_value::<ApplySourceWorktreeCohortParams>(value).is_err());
        }
    }

    #[test]
    fn apply_contract_rejects_digest_phrase_and_key_drift() {
        let identity = "/tmp/repository/.git".to_string();
        let mut params = ApplySourceWorktreeCohortParams {
            repository_identity: identity.clone(),
            plan_digest: digest(),
            authorization: format!("APPLY {identity} {}", digest()),
            idempotency_key: "stable:key".into(),
        };
        params.validate().unwrap();
        params.authorization.push(' ');
        assert!(params.validate().is_err());
        assert!(serde_json::from_value::<ApplySourceWorktreeCohortParams>(json!({"repository_identity": identity,"plan_digest": format!("sha256:{}", "A".repeat(64)),"authorization":"bad","idempotency_key":"stable:key"})).is_err());
        params.idempotency_key = "bad key".into();
        assert!(params.validate().is_err());
    }

    #[test]
    fn strict_decode_rejects_unknown_fields_and_noncanonical_identity() {
        assert!(serde_json::from_value::<ListSourceWorktreeCohortsParams>(json!({})).is_ok());
        assert!(
            serde_json::from_value::<ListSourceWorktreeCohortsParams>(json!({"limit":1})).is_err()
        );
        assert!(
            serde_json::from_value::<GetSourceWorktreeSettlementRunParams>(
                json!({"run_id":Uuid::new_v4().to_string().to_uppercase()})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<SourceWorktreeGitOidV1>(json!("A".repeat(40))).is_err());
        assert!(serde_json::from_value::<SourceWorktreeGitOidV1>(json!("a".repeat(64))).is_ok());
    }

    #[test]
    fn audit_run_id_is_optional_but_must_be_canonical_and_non_nil() {
        let mut audit = eligible_audit();
        let run_id = Uuid::new_v4();
        audit.run_id = Some(run_id);
        let encoded = serde_json::to_value(&audit).expect("encode audit with durable run");
        assert_eq!(encoded["run_id"], run_id.to_string());
        serde_json::from_value::<SourceWorktreeCohortAuditV1>(encoded)
            .expect("decode canonical audit run id")
            .validate_wire()
            .expect("validate canonical audit run id");

        audit.run_id = Some(Uuid::nil());
        assert!(audit.validate_wire().is_err());
        let mut invalid = serde_json::to_value(eligible_audit()).expect("audit JSON");
        invalid["run_id"] = json!(run_id.to_string().to_uppercase());
        assert!(serde_json::from_value::<SourceWorktreeCohortAuditV1>(invalid).is_err());
    }

    #[test]
    fn settlement_receipt_requires_a_bounded_nonempty_exact_inventory() {
        settlement_run().validate_wire().unwrap();

        let mut coherent_oversize = settlement_run();
        coherent_oversize.counts.observed =
            u32::try_from(SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1).unwrap();
        coherent_oversize.counts.retained =
            u32::try_from(SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS).unwrap();
        assert!(coherent_oversize.validate_wire().is_err());

        let mut saturating_overflow = settlement_run();
        saturating_overflow.counts.observed = u32::MAX;
        saturating_overflow.counts.retained = u32::MAX;
        assert!(saturating_overflow.validate_wire().is_err());

        let mut empty = settlement_run();
        empty.items.clear();
        empty.counts = SourceWorktreeSettlementCountsV1::default();
        empty.state = SourceWorktreeSettlementRunStateV1::Settled;
        empty.finished_at = Some(empty.updated_at.clone());
        assert!(empty.validate_wire().is_err());
    }

    #[test]
    fn settlement_receipt_bounds_every_count_and_uuid_identity() {
        macro_rules! assert_count_rejected {
            ($field:ident) => {{
                let mut run = settlement_run();
                run.counts.$field =
                    u32::try_from(SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1).unwrap();
                assert!(
                    run.validate_wire().is_err(),
                    "oversized {} count was accepted",
                    stringify!($field)
                );
            }};
        }
        assert_count_rejected!(observed);
        assert_count_rejected!(eligible);
        assert_count_rejected!(retained);
        assert_count_rejected!(settled);
        assert_count_rejected!(refused);
        assert_count_rejected!(recovery_required);
        assert_count_rejected!(unattempted);

        let mut nil_run = settlement_run();
        nil_run.run_id = Uuid::nil();
        assert!(nil_run.validate_wire().is_err());

        let mut nil_session = settlement_run();
        nil_session.items[0].session_id = Uuid::nil();
        assert!(nil_session.validate_wire().is_err());

        let mut nil_custody = settlement_run();
        nil_custody.items[0].custody_id = Uuid::nil();
        assert!(nil_custody.validate_wire().is_err());
    }

    #[test]
    fn strict_source_refs_reject_git_noncanonical_components() {
        settlement_run().validate_wire().unwrap();
        eligible_audit().validate_wire().unwrap();

        for source_ref in [
            "refs/heads/rsi/topic.lock",
            "refs/heads/rsi//topic",
            "refs/heads/rsi/.topic",
        ] {
            let mut receipt = settlement_run();
            receipt.items[0].source_ref = source_ref.into();
            assert!(
                receipt.validate_wire().is_err(),
                "receipt accepted {source_ref}"
            );

            let mut audit = eligible_audit();
            audit.items[0].source_ref = source_ref.into();
            assert!(
                audit.validate_wire().is_err(),
                "eligible audit accepted {source_ref}"
            );
        }

        let oversized_source = format!(
            "refs/heads/rsi/{}",
            "x".repeat(SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES)
        );
        let mut receipt = settlement_run();
        receipt.items[0].source_ref = oversized_source.clone();
        assert!(receipt.validate_wire().is_err());
        let mut audit = eligible_audit();
        audit.items[0].source_ref = oversized_source;
        assert!(audit.validate_wire().is_err());

        let oversized_target = format!(
            "refs/heads/{}",
            "x".repeat(SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES)
        );
        let mut receipt = settlement_run();
        receipt.target_ref = oversized_target.clone();
        assert!(receipt.validate_wire().is_err());
        let mut audit = eligible_audit();
        audit.target_ref = Some(oversized_target);
        assert!(audit.validate_wire().is_err());
    }

    fn batch_cursor() -> SourceWorktreeBatchCursorV2 {
        SourceWorktreeBatchCursorV2(format!("rsi-swc2:7b7d:{}", "a".repeat(64)))
    }

    fn batch_item(sequence: u32) -> SourceWorktreeBatchItemV2 {
        SourceWorktreeBatchItemV2 {
            sequence,
            custody_id: Uuid::new_v4(),
            custody_generation: 1,
            participant_count: 1,
            participant_digest: digest(),
            evidence_digest: digest(),
        }
    }

    #[test]
    fn batch_v2_requests_are_strict_path_free_and_apply_requires_cursor() {
        let initial: AuditSourceWorktreeBatchParamsV2 = serde_json::from_value(json!({
            "repository_identity": "repo:v2"
        }))
        .expect("initial audit without cursor");
        initial.validate().expect("valid initial audit");

        let continuation: AuditSourceWorktreeBatchParamsV2 = serde_json::from_value(json!({
            "repository_identity": "repo:v2",
            "cursor": batch_cursor(),
        }))
        .expect("continuation audit with cursor");
        continuation.validate().expect("valid continuation audit");

        for forbidden in [
            "path",
            "sandbox_root",
            "branch",
            "target_ref",
            "session_id",
            "batch_ordinal",
        ] {
            let mut value = serde_json::to_value(&continuation).unwrap();
            value[forbidden] = json!("caller-authority-forbidden");
            assert!(
                serde_json::from_value::<AuditSourceWorktreeBatchParamsV2>(value).is_err(),
                "audit accepted {forbidden}"
            );
        }

        let missing_cursor = json!({
            "repository_identity": "repo:v2",
            "plan_digest": digest(),
            "authorization": format!("APPLY repo:v2 BATCH 3 {}", digest()),
            "idempotency_key": "batch-v2",
        });
        assert!(
            serde_json::from_value::<ApplySourceWorktreeBatchParamsV2>(missing_cursor).is_err()
        );
    }

    #[test]
    fn batch_v2_authorization_uses_only_authenticated_ordinal() {
        let mut apply = ApplySourceWorktreeBatchParamsV2 {
            repository_identity: "repo:v2".into(),
            cursor: batch_cursor(),
            plan_digest: digest(),
            authorization: format!("APPLY repo:v2 BATCH 7 {}", digest()),
            idempotency_key: "batch-v2".into(),
        };
        apply
            .validate_authenticated(7)
            .expect("exact authenticated phrase");
        assert!(apply.validate_authenticated(8).is_err());
        apply.authorization.push(' ');
        assert!(apply.validate_authenticated(7).is_err());
    }

    #[test]
    fn batch_v2_cursor_shape_is_bounded_lower_hex_and_unambiguous() {
        batch_cursor().validate().expect("valid cursor shape");
        for malformed in [
            format!("rsi-swc2:7B7D:{}", "a".repeat(64)),
            format!("rsi-swc2:7b7d:{}:00", "a".repeat(64)),
            format!("rsi-swc2:7b7:{}", "a".repeat(64)),
            format!("rsi-swc2:7b7d:{}", "A".repeat(64)),
        ] {
            assert!(SourceWorktreeBatchCursorV2(malformed).validate().is_err());
        }
        assert!(
            SourceWorktreeBatchCursorV2(format!(
                "rsi-swc2:{}:{}",
                "00".repeat(SOURCE_WORKTREE_BATCH_MAX_CURSOR_BYTES),
                "a".repeat(64)
            ))
            .validate()
            .is_err()
        );
    }

    #[test]
    fn batch_v2_audit_and_run_enforce_page_and_participant_bounds() {
        let mut audit = SourceWorktreeBatchAuditV2 {
            schema_version: SOURCE_WORKTREE_BATCH_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_BATCH_POLICY_VERSION,
            repository_identity: "repo:v2".into(),
            cursor: batch_cursor(),
            snapshot_digest: digest(),
            snapshot_root_count: 1,
            target_ref: "refs/heads/rolling".into(),
            target_oid: SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap(),
            plan_digest: digest(),
            batch_ordinal: 7,
            has_more: false,
            applyable: true,
            authorization_phrase: Some(format!("APPLY repo:v2 BATCH 7 {}", digest())),
            items: vec![batch_item(0)],
            writes: 0,
        };
        audit.clone().validate_wire().expect("valid V2 audit");
        audit.authorization_phrase = Some(format!("APPLY repo:v2 BATCH 8 {}", digest()));
        assert!(audit.validate_wire().is_err());

        let timestamp = "2026-09-19T00:00:00.000000000Z".to_string();
        let mut run = SourceWorktreeBatchRunV2 {
            run_id: Uuid::new_v4(),
            repository_identity: "repo:v2".into(),
            cursor: batch_cursor(),
            snapshot_digest: digest(),
            snapshot_root_count: 1,
            target_ref: "refs/heads/rolling".into(),
            target_oid: SourceWorktreeGitOidV1::parse("b".repeat(40)).unwrap(),
            plan_digest: digest(),
            batch_ordinal: 7,
            has_more: false,
            predecessor_terminal_receipt_digest: None,
            terminal_receipt_digest: Some(digest()),
            state: SourceWorktreeBatchRunStateV2::Terminal,
            items: vec![batch_item(0)],
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        run.clone().validate_wire().expect("valid V2 run");
        run.items[0].participant_count =
            u32::try_from(SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS + 1).unwrap();
        assert!(run.validate_wire().is_err());
    }
}
