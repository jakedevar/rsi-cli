//! Closed wire contract for ordinary operator archive cleanup.
//!
//! Callers select only a Session. Every repository, custody, proof, run, and
//! receipt identity is derived by the daemon and is therefore response-only.

use crate::cohort_settlement::SourceWorktreeGitOidV1;
use crate::types::Sha256Digest;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ARCHIVE_CLEANUP_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveSessionParamsV1 {
    pub session_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetArchiveCleanupStatusParamsV1 {
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchivePreservationClassV1 {
    NoOutput,
    IntegratedAncestor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveCleanupPhaseV1 {
    IntentCommitted,
    Quarantined,
    RemovalAuthorized,
    WorktreeRemoved,
    Settled,
    Refused,
    RecoveryRequired,
}

impl ArchiveCleanupPhaseV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntentCommitted => "intent_committed",
            Self::Quarantined => "quarantined",
            Self::RemovalAuthorized => "removal_authorized",
            Self::WorktreeRemoved => "worktree_removed",
            Self::Settled => "settled",
            Self::Refused => "refused",
            Self::RecoveryRequired => "recovery_required",
        }
    }

    #[must_use]
    pub const fn ordinal(self) -> u32 {
        match self {
            Self::IntentCommitted => 1,
            Self::Quarantined => 2,
            Self::RemovalAuthorized => 3,
            Self::WorktreeRemoved => 4,
            Self::Settled => 5,
            Self::Refused | Self::RecoveryRequired => 100,
        }
    }
}

impl std::str::FromStr for ArchiveCleanupPhaseV1 {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "intent_committed" => Ok(Self::IntentCommitted),
            "quarantined" => Ok(Self::Quarantined),
            "removal_authorized" => Ok(Self::RemovalAuthorized),
            "worktree_removed" => Ok(Self::WorktreeRemoved),
            "settled" => Ok(Self::Settled),
            "refused" => Ok(Self::Refused),
            "recovery_required" => Ok(Self::RecoveryRequired),
            _ => Err(format!("unknown archive cleanup phase {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveCleanupDispositionV1 {
    NoCleanupRequired,
    CleanupSettled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveCleanupSafeCodeV1 {
    Settled,
    NotApplicable,
    PolicyRetained,
    SessionActive,
    SessionTopologyChanged,
    CustodyChanged,
    DependencyPresent,
    ProcessHolderPresent,
    RepositoryUnavailable,
    TargetUnavailable,
    TargetChanged,
    SourceRefChanged,
    WorktreeChanged,
    WorktreeDirty,
    OutputNotIntegrated,
    QuarantineCollision,
    ProofUnavailable,
    RemovalRefused,
    DatabaseSettlementFailed,
    RecoveryEvidenceAmbiguous,
}

impl ArchiveCleanupSafeCodeV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::NotApplicable => "not_applicable",
            Self::PolicyRetained => "policy_retained",
            Self::SessionActive => "session_active",
            Self::SessionTopologyChanged => "session_topology_changed",
            Self::CustodyChanged => "custody_changed",
            Self::DependencyPresent => "dependency_present",
            Self::ProcessHolderPresent => "process_holder_present",
            Self::RepositoryUnavailable => "repository_unavailable",
            Self::TargetUnavailable => "target_unavailable",
            Self::TargetChanged => "target_changed",
            Self::SourceRefChanged => "source_ref_changed",
            Self::WorktreeChanged => "worktree_changed",
            Self::WorktreeDirty => "worktree_dirty",
            Self::OutputNotIntegrated => "output_not_integrated",
            Self::QuarantineCollision => "quarantine_collision",
            Self::ProofUnavailable => "proof_unavailable",
            Self::RemovalRefused => "removal_refused",
            Self::DatabaseSettlementFailed => "database_settlement_failed",
            Self::RecoveryEvidenceAmbiguous => "recovery_evidence_ambiguous",
        }
    }
}

impl std::str::FromStr for ArchiveCleanupSafeCodeV1 {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "settled" => Ok(Self::Settled),
            "not_applicable" => Ok(Self::NotApplicable),
            "policy_retained" => Ok(Self::PolicyRetained),
            "session_active" => Ok(Self::SessionActive),
            "session_topology_changed" => Ok(Self::SessionTopologyChanged),
            "custody_changed" => Ok(Self::CustodyChanged),
            "dependency_present" => Ok(Self::DependencyPresent),
            "process_holder_present" => Ok(Self::ProcessHolderPresent),
            "repository_unavailable" => Ok(Self::RepositoryUnavailable),
            "target_unavailable" => Ok(Self::TargetUnavailable),
            "target_changed" => Ok(Self::TargetChanged),
            "source_ref_changed" => Ok(Self::SourceRefChanged),
            "worktree_changed" => Ok(Self::WorktreeChanged),
            "worktree_dirty" => Ok(Self::WorktreeDirty),
            "output_not_integrated" => Ok(Self::OutputNotIntegrated),
            "quarantine_collision" => Ok(Self::QuarantineCollision),
            "proof_unavailable" => Ok(Self::ProofUnavailable),
            "removal_refused" => Ok(Self::RemovalRefused),
            "database_settlement_failed" => Ok(Self::DatabaseSettlementFailed),
            "recovery_evidence_ambiguous" => Ok(Self::RecoveryEvidenceAmbiguous),
            _ => Err(format!("unknown archive cleanup safe code {value}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveCleanupReceiptV1 {
    pub version: u32,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub custody_id: Uuid,
    pub custody_generation: u64,
    pub preservation_class: ArchivePreservationClassV1,
    pub source_branch: String,
    pub source_oid: SourceWorktreeGitOidV1,
    pub target_ref: Option<String>,
    pub target_oid: Option<SourceWorktreeGitOidV1>,
    pub phase: ArchiveCleanupPhaseV1,
    pub branch_preserved: bool,
    pub session_status: String,
    pub custody_state: String,
    pub cleanup_state: String,
    pub created_at: String,
    pub settled_at: String,
    pub receipt_digest: Sha256Digest,
}

impl ArchiveCleanupReceiptV1 {
    /// Validates the closed, daemon-authored receipt wire contract.
    ///
    /// # Errors
    ///
    /// Returns a stable validation error when any receipt identity,
    /// preservation proof, terminal projection, or timestamp is invalid.
    pub fn validate_wire(&self) -> Result<(), String> {
        if self.version != ARCHIVE_CLEANUP_SCHEMA_VERSION
            || self.run_id.is_nil()
            || self.session_id.is_nil()
            || self.custody_id.is_nil()
            || self.custody_generation == 0
            || self.phase != ArchiveCleanupPhaseV1::Settled
            || !self.branch_preserved
            || self.session_status != "Archived"
            || self.custody_state != "purged"
            || self.cleanup_state != "Purged"
            || !self.source_branch.starts_with("refs/heads/")
        {
            return Err("invalid settled archive cleanup receipt".into());
        }
        match self.preservation_class {
            ArchivePreservationClassV1::NoOutput
                if self.target_ref.is_some() || self.target_oid.is_some() =>
            {
                return Err("no-output receipt cannot carry a target".into());
            }
            ArchivePreservationClassV1::IntegratedAncestor
                if self
                    .target_ref
                    .as_deref()
                    .is_none_or(|value| !value.starts_with("refs/heads/"))
                    || self.target_oid.is_none() =>
            {
                return Err("integrated-ancestor receipt requires an exact target".into());
            }
            _ => {}
        }
        validate_timestamp(&self.created_at)?;
        validate_timestamp(&self.settled_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveSessionResultV1 {
    pub version: u32,
    pub disposition: ArchiveCleanupDispositionV1,
    pub receipt: Option<ArchiveCleanupReceiptV1>,
}

impl ArchiveSessionResultV1 {
    #[must_use]
    pub const fn no_cleanup_required() -> Self {
        Self {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            disposition: ArchiveCleanupDispositionV1::NoCleanupRequired,
            receipt: None,
        }
    }

    /// Builds the only result shape that can claim cleanup success.
    ///
    /// # Errors
    ///
    /// Returns the receipt validation error when the daemon-authored receipt
    /// is not a valid settled receipt.
    pub fn cleanup_settled(receipt: ArchiveCleanupReceiptV1) -> Result<Self, String> {
        receipt.validate_wire()?;
        Ok(Self {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            disposition: ArchiveCleanupDispositionV1::CleanupSettled,
            receipt: Some(receipt),
        })
    }

    /// Validates the archive result's version and receipt/disposition pairing.
    ///
    /// # Errors
    ///
    /// Returns a stable validation error for an unknown version, an invalid
    /// receipt, or a disposition that disagrees with receipt presence.
    pub fn validate_wire(&self) -> Result<(), String> {
        if self.version != ARCHIVE_CLEANUP_SCHEMA_VERSION {
            return Err("invalid archive result version".into());
        }
        match (&self.disposition, &self.receipt) {
            (ArchiveCleanupDispositionV1::NoCleanupRequired, None) => Ok(()),
            (ArchiveCleanupDispositionV1::CleanupSettled, Some(receipt)) => receipt.validate_wire(),
            _ => Err("archive disposition and receipt disagree".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveCleanupStatusV1 {
    pub version: u32,
    pub session_id: Uuid,
    pub run_id: Option<Uuid>,
    pub phase: Option<ArchiveCleanupPhaseV1>,
    pub safe_code: ArchiveCleanupSafeCodeV1,
    pub retryable: bool,
    pub next_action: String,
    pub receipt: Option<ArchiveCleanupReceiptV1>,
    pub updated_at: Option<String>,
}

impl ArchiveCleanupStatusV1 {
    /// Validates status identity, timestamp, phase, and optional receipt truth.
    ///
    /// # Errors
    ///
    /// Returns a stable validation error when the status or its receipt is
    /// malformed or internally inconsistent.
    pub fn validate_wire(&self) -> Result<(), String> {
        if self.version != ARCHIVE_CLEANUP_SCHEMA_VERSION || self.session_id.is_nil() {
            return Err("invalid archive cleanup status identity".into());
        }
        if let Some(value) = &self.updated_at {
            validate_timestamp(value)?;
        }
        if let Some(receipt) = &self.receipt {
            receipt.validate_wire()?;
            if receipt.session_id != self.session_id
                || Some(receipt.run_id) != self.run_id
                || self.phase != Some(ArchiveCleanupPhaseV1::Settled)
            {
                return Err("archive status receipt identity mismatch".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveCleanupErrorV1 {
    pub version: u32,
    pub safe_code: ArchiveCleanupSafeCodeV1,
    pub run_id: Option<Uuid>,
    pub phase: Option<ArchiveCleanupPhaseV1>,
    pub retryable: bool,
    pub next_action: String,
}

impl ArchiveCleanupErrorV1 {
    /// Validates the bounded, path-free operator error payload.
    ///
    /// # Errors
    ///
    /// Returns a stable validation error when the version, run/phase pairing,
    /// retry guidance, or bounded next action is invalid.
    pub fn validate_wire(&self) -> Result<(), String> {
        if self.version != ARCHIVE_CLEANUP_SCHEMA_VERSION
            || self.run_id.is_some() != self.phase.is_some()
            || self.run_id.is_some_and(|run_id| run_id.is_nil())
            || self.next_action.is_empty()
            || self.next_action.len() > 512
        {
            return Err("invalid archive cleanup error data".into());
        }
        Ok(())
    }
}

fn validate_timestamp(value: &str) -> Result<(), String> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| "invalid archive cleanup timestamp".to_string())?;
    if parsed
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Nanos, true)
        != value
    {
        return Err("archive cleanup timestamp must be canonical RFC3339 nanoseconds UTC".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settled_receipt(session_id: Uuid) -> ArchiveCleanupReceiptV1 {
        ArchiveCleanupReceiptV1 {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            run_id: Uuid::new_v4(),
            session_id,
            custody_id: Uuid::new_v4(),
            custody_generation: 1,
            preservation_class: ArchivePreservationClassV1::NoOutput,
            source_branch: "refs/heads/rsi/archive-contract".into(),
            source_oid: SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap(),
            target_ref: None,
            target_oid: None,
            phase: ArchiveCleanupPhaseV1::Settled,
            branch_preserved: true,
            session_status: "Archived".into(),
            custody_state: "purged".into(),
            cleanup_state: "Purged".into(),
            created_at: "2026-09-08T12:34:56.123456789Z".into(),
            settled_at: "2026-09-08T12:35:56.123456789Z".into(),
            receipt_digest: Sha256Digest::parse(format!("sha256:{}", "b".repeat(64))).unwrap(),
        }
    }

    #[test]
    fn archive_requests_accept_only_the_session_selector() {
        let session_id = Uuid::new_v4();
        assert!(
            serde_json::from_value::<ArchiveSessionParamsV1>(json!({
                "session_id": session_id,
            }))
            .is_ok()
        );
        for forbidden in [
            "run_id",
            "path",
            "branch",
            "source_ref",
            "repository",
            "custody_id",
            "custody_generation",
            "proof",
            "receipt",
            "authority",
        ] {
            let mut value = json!({"session_id": session_id});
            value
                .as_object_mut()
                .unwrap()
                .insert(forbidden.into(), json!("caller"));
            assert!(serde_json::from_value::<ArchiveSessionParamsV1>(value.clone()).is_err());
            assert!(serde_json::from_value::<GetArchiveCleanupStatusParamsV1>(value).is_err());
        }
    }

    #[test]
    fn result_shape_cannot_claim_cleanup_without_a_receipt() {
        let invalid = ArchiveSessionResultV1 {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            disposition: ArchiveCleanupDispositionV1::CleanupSettled,
            receipt: None,
        };
        assert!(invalid.validate_wire().is_err());
    }

    #[test]
    fn safe_error_requires_matching_daemon_run_and_phase_identity() {
        let invalid = ArchiveCleanupErrorV1 {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            safe_code: ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
            run_id: Some(Uuid::new_v4()),
            phase: None,
            retryable: false,
            next_action: "Preserve evidence.".into(),
        };
        assert!(invalid.validate_wire().is_err());
    }

    #[test]
    fn status_rejects_a_settled_receipt_for_another_session() {
        let status_session_id = Uuid::new_v4();
        let receipt = settled_receipt(Uuid::new_v4());
        let status = ArchiveCleanupStatusV1 {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            session_id: status_session_id,
            run_id: Some(receipt.run_id),
            phase: Some(ArchiveCleanupPhaseV1::Settled),
            safe_code: ArchiveCleanupSafeCodeV1::Settled,
            retryable: false,
            next_action: "Archive cleanup is settled.".into(),
            receipt: Some(receipt),
            updated_at: Some("2026-09-08T12:35:56.123456789Z".into()),
        };

        assert_eq!(
            status.validate_wire(),
            Err("archive status receipt identity mismatch".into())
        );
    }

    #[test]
    fn archive_contract_rejects_noncanonical_fractional_timestamp_precision() {
        let mut receipt = settled_receipt(Uuid::new_v4());
        receipt.created_at = "2026-09-08T12:34:56.1Z".into();
        assert_eq!(
            receipt.validate_wire(),
            Err("archive cleanup timestamp must be canonical RFC3339 nanoseconds UTC".into())
        );

        let mut status = ArchiveCleanupStatusV1 {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            session_id: receipt.session_id,
            run_id: None,
            phase: None,
            safe_code: ArchiveCleanupSafeCodeV1::NotApplicable,
            retryable: false,
            next_action: "No cleanup is required.".into(),
            receipt: None,
            updated_at: Some("2026-09-08T12:34:56.1Z".into()),
        };
        assert_eq!(
            status.validate_wire(),
            Err("archive cleanup timestamp must be canonical RFC3339 nanoseconds UTC".into())
        );

        status.updated_at = Some("2026-09-08T12:34:56.100000000Z".into());
        status.validate_wire().expect("canonical nanoseconds pass");
    }
}
