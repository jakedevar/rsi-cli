//! V94 retained journal and V95 hardening for source-worktree settlement.

use super::Store;
use super::sandbox_custody::{self, CustodyCause};
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rsi_common::cohort_settlement::{
    SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES, SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS,
    SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION, SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
    SourceWorktreeCohortSummaryV1, SourceWorktreeGitOidV1, SourceWorktreeSettlementCountsV1,
    SourceWorktreeSettlementItemV1, SourceWorktreeSettlementPhaseV1,
    SourceWorktreeSettlementRefusalV1, SourceWorktreeSettlementRunStateV1,
    SourceWorktreeSettlementRunV1, validate_git_oid, validate_idempotency_key, validate_identity,
    validate_sha256_digest, validate_source_ref, validate_target_ref,
};
use rsi_common::types::Sha256Digest;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

// RSI-RELEASED-MIGRATION-BEGIN: v94-v95-settlement-fingerprints
pub(crate) const V94_SETTLEMENT_CATALOG_FINGERPRINT: &str =
    "sha256:4791e4bfaffdacc78f36543ff09f3bd607d630800aa77d1790ab9d629bf01a0a";
const V94_DEPLOYED_SETTLEMENT_CATALOG_FINGERPRINT: &str =
    "sha256:83c87f82ace046bd8fd76d72a8040a1797fdbde51f8992390ced2c5245270e7c";
pub(crate) const V95_SETTLEMENT_CATALOG_FINGERPRINT: &str =
    "sha256:8735eede240ad2f1c0c98232796240ec951e686411297b19a1cfd4ad69c6b7bf";
// RSI-RELEASED-MIGRATION-END: v94-v95-settlement-fingerprints
const SOURCE_WORKTREE_COHORT_LIST_MAX: usize = 4096;
const STARTUP_SETTLEMENT_SCAN_PAGE: usize = 256;
const STARTUP_SETTLEMENT_ROOT_MAX: usize = 8192;
const STARTUP_SETTLEMENT_CANDIDATE_MAX: usize = 16_384;
const STARTUP_SETTLEMENT_PATH_MAX_BYTES: usize = 4096;
const SETTLEMENT_DEPENDENCY_SCAN_PAGE: usize = 256;
pub(crate) const SETTLEMENT_DEPENDENCY_SCAN_MAX_RECORDS: usize = 16_384;
const SETTLEMENT_ITEM_LOAD_LIMIT: usize = SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1;
const SETTLEMENT_REF_MAX_BYTES: usize = SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES;
const SETTLEMENT_ORIGINAL_UPDATED_AT_MAX_BYTES: usize = 64;
const QUARANTINE_REMOVE_AUTHORITY_SCHEMA_VERSION: u32 = 1;
const QUARANTINE_REMOVE_AUTHORITY_KIND: &str = "quarantine_remove_authorized_v1";
const QUARANTINE_REMOVE_AUTHORITY_MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QuarantineRemoveAuthorityDigestDomainV1 {
    OriginalPath,
    QuarantinePath,
    RepositoryIdentity,
    CanonicalRepoDir,
    SourceRef,
    TargetRef,
    AdminDir,
    AdminId,
}

impl QuarantineRemoveAuthorityDigestDomainV1 {
    const fn label(self) -> &'static [u8] {
        match self {
            Self::OriginalPath => b"original-path",
            Self::QuarantinePath => b"quarantine-path",
            Self::RepositoryIdentity => b"repository-identity",
            Self::CanonicalRepoDir => b"canonical-repo-dir",
            Self::SourceRef => b"source-ref",
            Self::TargetRef => b"target-ref",
            Self::AdminDir => b"admin-dir",
            Self::AdminId => b"admin-id",
        }
    }
}

/// Hash one exact retained UTF-8 field under a fixed, non-interchangeable
/// quarantine-authority domain. No path normalization is performed here.
pub(crate) fn quarantine_remove_authority_field_digest(
    domain: QuarantineRemoveAuthorityDigestDomainV1,
    exact_utf8: &str,
) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(b"rsi-quarantine-remove-authority-field-v1\0");
    digest.update(domain.label());
    digest.update([0]);
    digest.update((exact_utf8.len() as u64).to_be_bytes());
    digest.update(exact_utf8.as_bytes());
    Sha256Digest::parse(format!("sha256:{:x}", digest.finalize()))
        .expect("SHA-256 formatter always emits a canonical digest")
}

#[derive(Debug, Clone)]
pub(crate) struct QuarantineRemoveAuthorityFactsV1<'a> {
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub custody_id: Uuid,
    pub custody_generation: u64,
    pub original_path: &'a str,
    pub quarantine_path: &'a str,
    pub repository_identity: &'a str,
    pub canonical_repo_dir: &'a str,
    pub source_ref: &'a str,
    pub target_ref: &'a str,
    pub admin_dir: &'a str,
    pub admin_id: &'a str,
    pub source_oid: SourceWorktreeGitOidV1,
    pub journal_target_oid: SourceWorktreeGitOidV1,
    pub removal_target_oid: SourceWorktreeGitOidV1,
    pub journal_evidence_digest: Sha256Digest,
    pub journal_clean_digest: Sha256Digest,
    pub removal_clean_digest: Sha256Digest,
    pub root_device: u64,
    pub root_inode: u64,
    pub stable_tree_digest: Sha256Digest,
    pub holder_evidence_digest: Sha256Digest,
    pub holder_fixed_point_passes: u32,
    pub trusted_platform_exemption_count: u32,
    pub trusted_platform_exemption_digest: Sha256Digest,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuarantineRemoveAuthorityV1 {
    schema_version: u32,
    kind: String,
    run_id: Uuid,
    session_id: Uuid,
    custody_id: Uuid,
    custody_generation: u64,
    original_path_digest: Sha256Digest,
    quarantine_path_digest: Sha256Digest,
    repository_identity_digest: Sha256Digest,
    canonical_repo_dir_digest: Sha256Digest,
    source_ref_digest: Sha256Digest,
    target_ref_digest: Sha256Digest,
    admin_dir_digest: Sha256Digest,
    admin_id_digest: Sha256Digest,
    source_oid: SourceWorktreeGitOidV1,
    journal_target_oid: SourceWorktreeGitOidV1,
    removal_target_oid: SourceWorktreeGitOidV1,
    journal_evidence_digest: Sha256Digest,
    journal_clean_digest: Sha256Digest,
    removal_clean_digest: Sha256Digest,
    root_device: u64,
    root_inode: u64,
    stable_tree_digest: Sha256Digest,
    holder_evidence_digest: Sha256Digest,
    holder_fixed_point_passes: u32,
    trusted_platform_exemption_count: u32,
    trusted_platform_exemption_digest: Sha256Digest,
}

impl QuarantineRemoveAuthorityV1 {
    pub(crate) fn new(facts: QuarantineRemoveAuthorityFactsV1<'_>) -> Result<Self> {
        for (value, field) in [
            (facts.original_path, "original path"),
            (facts.quarantine_path, "quarantine path"),
            (facts.canonical_repo_dir, "canonical repository path"),
            (facts.admin_dir, "Git admin directory"),
            (facts.admin_id, "Git admin id"),
        ] {
            validate_settlement_text(
                value,
                field,
                SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES,
                false,
            )?;
        }
        validate_identity(facts.repository_identity).map_err(DaemonError::InvalidParam)?;
        validate_source_ref(facts.source_ref).map_err(DaemonError::InvalidParam)?;
        validate_target_ref(facts.target_ref).map_err(DaemonError::InvalidParam)?;
        let marker = Self {
            schema_version: QUARANTINE_REMOVE_AUTHORITY_SCHEMA_VERSION,
            kind: QUARANTINE_REMOVE_AUTHORITY_KIND.into(),
            run_id: facts.run_id,
            session_id: facts.session_id,
            custody_id: facts.custody_id,
            custody_generation: facts.custody_generation,
            original_path_digest: quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::OriginalPath,
                facts.original_path,
            ),
            quarantine_path_digest: quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::QuarantinePath,
                facts.quarantine_path,
            ),
            repository_identity_digest: quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::RepositoryIdentity,
                facts.repository_identity,
            ),
            canonical_repo_dir_digest: quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::CanonicalRepoDir,
                facts.canonical_repo_dir,
            ),
            source_ref_digest: quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::SourceRef,
                facts.source_ref,
            ),
            target_ref_digest: quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::TargetRef,
                facts.target_ref,
            ),
            admin_dir_digest: quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::AdminDir,
                facts.admin_dir,
            ),
            admin_id_digest: quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::AdminId,
                facts.admin_id,
            ),
            source_oid: facts.source_oid,
            journal_target_oid: facts.journal_target_oid,
            removal_target_oid: facts.removal_target_oid,
            journal_evidence_digest: facts.journal_evidence_digest,
            journal_clean_digest: facts.journal_clean_digest,
            removal_clean_digest: facts.removal_clean_digest,
            root_device: facts.root_device,
            root_inode: facts.root_inode,
            stable_tree_digest: facts.stable_tree_digest,
            holder_evidence_digest: facts.holder_evidence_digest,
            holder_fixed_point_passes: facts.holder_fixed_point_passes,
            trusted_platform_exemption_count: facts.trusted_platform_exemption_count,
            trusted_platform_exemption_digest: facts.trusted_platform_exemption_digest,
        };
        marker.validate()?;
        marker.to_canonical_json()?;
        Ok(marker)
    }

    pub(crate) fn parse_canonical(input: &str) -> Result<Self> {
        if input.len() > QUARANTINE_REMOVE_AUTHORITY_MAX_BYTES {
            return Err(DaemonError::Store(
                "quarantine remove authority exceeds 16384 bytes".into(),
            ));
        }
        let marker: Self = serde_json::from_str(input).map_err(|error| {
            DaemonError::Store(format!("invalid quarantine remove authority: {error}"))
        })?;
        marker.validate()?;
        let canonical = serde_json::to_string(&marker).map_err(|error| {
            DaemonError::Store(format!(
                "cannot serialize quarantine remove authority: {error}"
            ))
        })?;
        if canonical != input {
            return Err(DaemonError::Store(
                "quarantine remove authority is not canonical JSON".into(),
            ));
        }
        Ok(marker)
    }

    pub(crate) fn to_canonical_json(&self) -> Result<String> {
        self.validate()?;
        let value = serde_json::to_string(self).map_err(|error| {
            DaemonError::Store(format!(
                "cannot serialize quarantine remove authority: {error}"
            ))
        })?;
        if value.len() > QUARANTINE_REMOVE_AUTHORITY_MAX_BYTES {
            return Err(DaemonError::Store(
                "quarantine remove authority exceeds 16384 bytes".into(),
            ));
        }
        Ok(value)
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != QUARANTINE_REMOVE_AUTHORITY_SCHEMA_VERSION
            || self.kind != QUARANTINE_REMOVE_AUTHORITY_KIND
        {
            return Err(DaemonError::Store(
                "quarantine remove authority schema or kind is invalid".into(),
            ));
        }
        if self.run_id.is_nil() || self.session_id.is_nil() || self.custody_id.is_nil() {
            return Err(DaemonError::Store(
                "quarantine remove authority ids must be non-nil".into(),
            ));
        }
        if self.custody_generation == 0
            || self.root_device == 0
            || self.root_inode == 0
            || self.holder_fixed_point_passes < 2
        {
            return Err(DaemonError::Store(
                "quarantine remove authority counters or root identity are invalid".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn run_id(&self) -> Uuid {
        self.run_id
    }
    pub(crate) fn session_id(&self) -> Uuid {
        self.session_id
    }
    pub(crate) fn custody_id(&self) -> Uuid {
        self.custody_id
    }
    pub(crate) fn custody_generation(&self) -> u64 {
        self.custody_generation
    }
    pub(crate) fn original_path_digest(&self) -> &Sha256Digest {
        &self.original_path_digest
    }
    pub(crate) fn quarantine_path_digest(&self) -> &Sha256Digest {
        &self.quarantine_path_digest
    }
    pub(crate) fn repository_identity_digest(&self) -> &Sha256Digest {
        &self.repository_identity_digest
    }
    pub(crate) fn canonical_repo_dir_digest(&self) -> &Sha256Digest {
        &self.canonical_repo_dir_digest
    }
    pub(crate) fn source_ref_digest(&self) -> &Sha256Digest {
        &self.source_ref_digest
    }
    pub(crate) fn target_ref_digest(&self) -> &Sha256Digest {
        &self.target_ref_digest
    }
    pub(crate) fn admin_dir_digest(&self) -> &Sha256Digest {
        &self.admin_dir_digest
    }
    pub(crate) fn admin_id_digest(&self) -> &Sha256Digest {
        &self.admin_id_digest
    }
    pub(crate) fn source_oid(&self) -> &SourceWorktreeGitOidV1 {
        &self.source_oid
    }
    pub(crate) fn journal_target_oid(&self) -> &SourceWorktreeGitOidV1 {
        &self.journal_target_oid
    }
    pub(crate) fn removal_target_oid(&self) -> &SourceWorktreeGitOidV1 {
        &self.removal_target_oid
    }
    pub(crate) fn journal_evidence_digest(&self) -> &Sha256Digest {
        &self.journal_evidence_digest
    }
    pub(crate) fn journal_clean_digest(&self) -> &Sha256Digest {
        &self.journal_clean_digest
    }
    pub(crate) fn removal_clean_digest(&self) -> &Sha256Digest {
        &self.removal_clean_digest
    }
    pub(crate) fn root_device(&self) -> u64 {
        self.root_device
    }
    pub(crate) fn root_inode(&self) -> u64 {
        self.root_inode
    }
    pub(crate) fn stable_tree_digest(&self) -> &Sha256Digest {
        &self.stable_tree_digest
    }
    pub(crate) fn holder_evidence_digest(&self) -> &Sha256Digest {
        &self.holder_evidence_digest
    }
    pub(crate) fn holder_fixed_point_passes(&self) -> u32 {
        self.holder_fixed_point_passes
    }
    pub(crate) fn trusted_platform_exemption_count(&self) -> u32 {
        self.trusted_platform_exemption_count
    }
    pub(crate) fn trusted_platform_exemption_digest(&self) -> &Sha256Digest {
        &self.trusted_platform_exemption_digest
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct SourceWorktreeInventoryQuarantineAlias {
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub custody_id: Uuid,
    pub custody_generation: u64,
    pub original_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceWorktreeSettlementV94MigrationFault {
    AfterPreflight,
    AfterSchema,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceWorktreeSettlementV95MigrationFault {
    AfterPreflight,
    AfterTriggerDrop,
    AfterNormalize,
    AfterRunOrder,
    AfterLatestRunPointer,
    AfterIndexes,
    AfterTriggers,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum V94SettlementCatalog {
    Current,
    Deployed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceWorktreeSettlementV94BridgeFault {
    AfterSourceObjectDrop,
    AfterRename,
    AfterSchema,
    AfterCopy,
    AfterShadowDrop,
}

#[cfg(test)]
impl SourceWorktreeSettlementV94BridgeFault {
    pub(crate) const ALL: [Self; 5] = [
        Self::AfterSourceObjectDrop,
        Self::AfterRename,
        Self::AfterSchema,
        Self::AfterCopy,
        Self::AfterShadowDrop,
    ];
}

impl SourceWorktreeSettlementV95MigrationFault {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 10] = [
        Self::AfterPreflight,
        Self::AfterTriggerDrop,
        Self::AfterNormalize,
        Self::AfterRunOrder,
        Self::AfterLatestRunPointer,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

impl SourceWorktreeSettlementV94MigrationFault {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::AfterPreflight,
        Self::AfterSchema,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static V94_MIGRATION_FAULT: RefCell<Option<SourceWorktreeSettlementV94MigrationFault>> = const { RefCell::new(None) };
    static V95_MIGRATION_FAULT: RefCell<Option<SourceWorktreeSettlementV95MigrationFault>> = const { RefCell::new(None) };
    static V94_BRIDGE_FAULT: RefCell<Option<SourceWorktreeSettlementV94BridgeFault>> = const { RefCell::new(None) };
    static SETTLEMENT_FINALIZATION_FAULT: RefCell<Option<SettlementFinalizationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettlementFinalizationFault {
    BeforeTransaction,
    BeforeCommit,
    AfterCommit,
}

#[cfg(test)]
pub(crate) fn fail_next_v94_migration(fault: SourceWorktreeSettlementV94MigrationFault) {
    V94_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
pub(crate) fn fail_next_v95_migration(fault: SourceWorktreeSettlementV95MigrationFault) {
    V95_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
pub(crate) fn fail_next_v94_bridge(fault: SourceWorktreeSettlementV94BridgeFault) {
    V94_BRIDGE_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
pub(crate) fn fail_next_source_worktree_settlement_finalization() {
    fail_source_worktree_settlement_finalization_at(SettlementFinalizationFault::BeforeCommit);
}

#[cfg(test)]
pub(crate) fn fail_source_worktree_settlement_finalization_at(fault: SettlementFinalizationFault) {
    SETTLEMENT_FINALIZATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn settlement_finalization_fault(fault: SettlementFinalizationFault) -> Result<()> {
    let injected = SETTLEMENT_FINALIZATION_FAULT.with(|slot| {
        let injected = *slot.borrow() == Some(fault);
        if injected {
            *slot.borrow_mut() = None;
        }
        injected
    });
    if injected {
        return Err(DaemonError::Store(
            "injected source-worktree settlement finalization fault".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn v94_migration_fault(fault: SourceWorktreeSettlementV94MigrationFault) -> Result<()> {
    let injected = V94_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V94 settlement migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn v95_migration_fault(fault: SourceWorktreeSettlementV95MigrationFault) -> Result<()> {
    let injected = V95_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V95 settlement migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
fn v94_bridge_fault(fault: SourceWorktreeSettlementV94BridgeFault) -> Result<()> {
    let injected = V94_BRIDGE_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V94-to-V95 bridge fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn v94_migration_fault(_: SourceWorktreeSettlementV94MigrationFault) -> Result<()> {
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn v95_migration_fault(_: SourceWorktreeSettlementV95MigrationFault) -> Result<()> {
    Ok(())
}

#[cfg(not(test))]
fn v94_bridge_fault(_: SourceWorktreeSettlementV94BridgeFault) -> Result<()> {
    Ok(())
}

// RSI-RELEASED-MIGRATION-BEGIN: v94-settlement-projections-and-bridge
const V94_DEPLOYED_SETTLEMENT_SCHEMA_SQL: &str = r#"CREATE TABLE source_worktree_settlement_runs (
            run_id TEXT PRIMARY KEY
                CHECK(length(run_id)=36 AND run_id=lower(run_id)),
            schema_version INTEGER NOT NULL CHECK(schema_version=1),
            policy_version INTEGER NOT NULL CHECK(policy_version=1),
            repository_identity TEXT NOT NULL CHECK(length(repository_identity) BETWEEN 1 AND 4096),
            canonical_repo_dir TEXT NOT NULL CHECK(length(canonical_repo_dir) BETWEEN 1 AND 4096),
            target_ref TEXT NOT NULL CHECK(target_ref GLOB 'refs/heads/*'),
            target_oid TEXT NOT NULL CHECK(length(target_oid) IN (40,64) AND target_oid=lower(target_oid) AND target_oid NOT GLOB '*[^0-9a-f]*'),
            plan_digest TEXT NOT NULL CHECK(length(plan_digest)=71 AND plan_digest GLOB 'sha256:*' AND substr(plan_digest,8) NOT GLOB '*[^0-9a-f]*'),
            idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 128),
            authorization_digest TEXT NOT NULL CHECK(length(authorization_digest)=71 AND authorization_digest GLOB 'sha256:*' AND substr(authorization_digest,8) NOT GLOB '*[^0-9a-f]*'),
            request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND request_fingerprint GLOB 'sha256:*' AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
            state TEXT NOT NULL CHECK(state IN ('intent_committed','applying','settled','partial','recovery_required','refused')),
            observed_count INTEGER NOT NULL CHECK(observed_count>=0),
            eligible_count INTEGER NOT NULL CHECK(eligible_count>=0),
            retained_count INTEGER NOT NULL CHECK(retained_count>=0),
            settled_count INTEGER NOT NULL DEFAULT 0 CHECK(settled_count>=0),
            refused_count INTEGER NOT NULL DEFAULT 0 CHECK(refused_count>=0),
            recovery_required_count INTEGER NOT NULL DEFAULT 0 CHECK(recovery_required_count>=0),
            unattempted_count INTEGER NOT NULL DEFAULT 0 CHECK(unattempted_count>=0),
            created_at TEXT NOT NULL CHECK(length(created_at)=30 AND created_at GLOB '????-??-??T??:??:??.?????????Z'),
            updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND updated_at GLOB '????-??-??T??:??:??.?????????Z'),
            finished_at TEXT CHECK(finished_at IS NULL OR (length(finished_at)=30 AND finished_at GLOB '????-??-??T??:??:??.?????????Z')),
            terminal_error TEXT CHECK(terminal_error IS NULL OR length(terminal_error)<=16384),
            UNIQUE(repository_identity,idempotency_key)
        );
        CREATE INDEX idx_source_worktree_settlement_runs_state
            ON source_worktree_settlement_runs(state,updated_at,run_id);
        CREATE TABLE source_worktree_settlement_items (
            run_id TEXT NOT NULL REFERENCES source_worktree_settlement_runs(run_id),
            sequence INTEGER NOT NULL CHECK(sequence>=0),
            session_id TEXT NOT NULL REFERENCES sessions(id),
            original_status TEXT NOT NULL CHECK(original_status IN ('Completed','Failed','Interrupted','Archived')),
            original_updated_at TEXT NOT NULL,
            custody_id TEXT NOT NULL REFERENCES sandbox_custody_roots(custody_id),
            custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
            canonical_repo_dir TEXT NOT NULL,
            sandbox_root TEXT NOT NULL,
            sandbox_branch TEXT NOT NULL,
            repository_identity TEXT NOT NULL,
            source_ref TEXT NOT NULL CHECK(source_ref GLOB 'refs/heads/rsi/*'),
            source_oid TEXT NOT NULL CHECK(length(source_oid) IN (40,64) AND source_oid=lower(source_oid) AND source_oid NOT GLOB '*[^0-9a-f]*'),
            target_oid TEXT NOT NULL CHECK(length(target_oid) IN (40,64) AND target_oid=lower(target_oid) AND target_oid NOT GLOB '*[^0-9a-f]*'),
            evidence_digest TEXT NOT NULL CHECK(length(evidence_digest)=71 AND evidence_digest GLOB 'sha256:*' AND substr(evidence_digest,8) NOT GLOB '*[^0-9a-f]*'),
            clean_state_digest TEXT NOT NULL CHECK(length(clean_state_digest)=71 AND clean_state_digest GLOB 'sha256:*' AND substr(clean_state_digest,8) NOT GLOB '*[^0-9a-f]*'),
            reserved_effects INTEGER NOT NULL CHECK(reserved_effects>=0),
            active_effects INTEGER NOT NULL CHECK(active_effects>=0),
            participant_count INTEGER NOT NULL CHECK(participant_count>0),
            phase TEXT NOT NULL CHECK(phase IN ('planned','intent_committed','worktree_removed','branch_removed','settled','refused','recovery_required','unattempted')),
            before_observation TEXT CHECK(before_observation IS NULL OR length(before_observation)<=16384),
            after_observation TEXT CHECK(after_observation IS NULL OR length(after_observation)<=16384),
            refusal_code TEXT CHECK(refusal_code IS NULL OR length(refusal_code)<=128),
            created_at TEXT NOT NULL CHECK(length(created_at)=30 AND created_at GLOB '????-??-??T??:??:??.?????????Z'),
            updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND updated_at GLOB '????-??-??T??:??:??.?????????Z'),
            PRIMARY KEY(run_id,sequence),
            UNIQUE(run_id,session_id)
        );
        CREATE INDEX idx_source_worktree_settlement_items_phase
            ON source_worktree_settlement_items(run_id,phase,sequence);
        CREATE TRIGGER source_worktree_settlement_runs_no_delete BEFORE DELETE ON source_worktree_settlement_runs
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_no_delete'); END;
        CREATE TRIGGER source_worktree_settlement_runs_identity_immutable BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN NEW.run_id!=OLD.run_id OR NEW.schema_version!=OLD.schema_version OR NEW.policy_version!=OLD.policy_version
              OR NEW.repository_identity!=OLD.repository_identity OR NEW.canonical_repo_dir!=OLD.canonical_repo_dir
              OR NEW.target_ref!=OLD.target_ref OR NEW.target_oid!=OLD.target_oid OR NEW.plan_digest!=OLD.plan_digest
              OR NEW.idempotency_key!=OLD.idempotency_key OR NEW.authorization_digest!=OLD.authorization_digest
              OR NEW.request_fingerprint!=OLD.request_fingerprint OR NEW.created_at!=OLD.created_at
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_identity_immutable'); END;
        CREATE TRIGGER source_worktree_settlement_runs_terminal BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN OLD.state IN ('settled','recovery_required','refused') AND NEW.state!=OLD.state
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_terminal'); END;
        CREATE TRIGGER source_worktree_settlement_items_no_delete BEFORE DELETE ON source_worktree_settlement_items
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_no_delete'); END;
        CREATE TRIGGER source_worktree_settlement_items_identity_immutable BEFORE UPDATE ON source_worktree_settlement_items
            WHEN NEW.run_id!=OLD.run_id OR NEW.sequence!=OLD.sequence OR NEW.session_id!=OLD.session_id
              OR NEW.original_status!=OLD.original_status OR NEW.original_updated_at!=OLD.original_updated_at
              OR NEW.custody_id!=OLD.custody_id OR NEW.custody_generation!=OLD.custody_generation
              OR NEW.canonical_repo_dir!=OLD.canonical_repo_dir OR NEW.sandbox_root!=OLD.sandbox_root
              OR NEW.sandbox_branch!=OLD.sandbox_branch OR NEW.repository_identity!=OLD.repository_identity
              OR NEW.source_ref!=OLD.source_ref OR NEW.source_oid!=OLD.source_oid OR NEW.target_oid!=OLD.target_oid
              OR NEW.evidence_digest!=OLD.evidence_digest OR NEW.clean_state_digest!=OLD.clean_state_digest
              OR NEW.reserved_effects!=OLD.reserved_effects OR NEW.active_effects!=OLD.active_effects
              OR NEW.participant_count!=OLD.participant_count OR NEW.created_at!=OLD.created_at
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_identity_immutable'); END;
        CREATE TRIGGER source_worktree_settlement_items_forward_phase BEFORE UPDATE ON source_worktree_settlement_items
            WHEN NEW.phase!=OLD.phase AND NOT (
                (OLD.phase='planned' AND NEW.phase IN ('intent_committed','refused','recovery_required','unattempted')) OR
                (OLD.phase='intent_committed' AND NEW.phase IN ('worktree_removed','refused','recovery_required')) OR
                (OLD.phase='worktree_removed' AND NEW.phase IN ('branch_removed','recovery_required')) OR
                (OLD.phase='branch_removed' AND NEW.phase IN ('settled','recovery_required'))
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_phase_regression'); END;
        CREATE TRIGGER source_worktree_settlement_items_terminal BEFORE UPDATE ON source_worktree_settlement_items
            WHEN OLD.phase IN ('settled','refused','recovery_required','unattempted') AND NEW.phase!=OLD.phase
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_terminal'); END;"#;

const V94_SETTLEMENT_SCHEMA_SQL: &str =
    "CREATE TABLE source_worktree_settlement_runs (
            run_id TEXT PRIMARY KEY
                CHECK(length(run_id)=36 AND run_id=lower(run_id)
                    AND substr(run_id,9,1)='-' AND substr(run_id,14,1)='-'
                    AND substr(run_id,19,1)='-' AND substr(run_id,24,1)='-'
                    AND replace(run_id,'-','') NOT GLOB '*[^0-9a-f]*'),
            schema_version INTEGER NOT NULL CHECK(schema_version=1),
            policy_version INTEGER NOT NULL CHECK(policy_version=1),
            repository_identity TEXT NOT NULL CHECK(length(repository_identity) BETWEEN 1 AND 4096),
            canonical_repo_dir TEXT NOT NULL CHECK(length(canonical_repo_dir) BETWEEN 1 AND 4096),
            target_ref TEXT NOT NULL CHECK(target_ref GLOB 'refs/heads/*'),
            target_oid TEXT NOT NULL CHECK(length(target_oid) IN (40,64) AND target_oid=lower(target_oid) AND target_oid NOT GLOB '*[^0-9a-f]*'),
            plan_digest TEXT NOT NULL CHECK(length(plan_digest)=71 AND plan_digest GLOB 'sha256:*' AND substr(plan_digest,8) NOT GLOB '*[^0-9a-f]*'),
            idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 128),
            authorization_digest TEXT NOT NULL CHECK(length(authorization_digest)=71 AND authorization_digest GLOB 'sha256:*' AND substr(authorization_digest,8) NOT GLOB '*[^0-9a-f]*'),
            request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND request_fingerprint GLOB 'sha256:*' AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
            state TEXT NOT NULL CHECK(state IN ('intent_committed','applying','settled','partial','recovery_required','refused')),
            observed_count INTEGER NOT NULL CHECK(observed_count>=0),
            eligible_count INTEGER NOT NULL CHECK(eligible_count>=0),
            retained_count INTEGER NOT NULL CHECK(retained_count>=0),
            settled_count INTEGER NOT NULL DEFAULT 0 CHECK(settled_count>=0),
            refused_count INTEGER NOT NULL DEFAULT 0 CHECK(refused_count>=0),
            recovery_required_count INTEGER NOT NULL DEFAULT 0 CHECK(recovery_required_count>=0),
            unattempted_count INTEGER NOT NULL DEFAULT 0 CHECK(unattempted_count>=0),
            created_at TEXT NOT NULL CHECK(length(created_at)=30 AND created_at GLOB '????-??-??T??:??:??.?????????Z'),
            updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND updated_at GLOB '????-??-??T??:??:??.?????????Z'),
            finished_at TEXT CHECK(finished_at IS NULL OR (length(finished_at)=30 AND finished_at GLOB '????-??-??T??:??:??.?????????Z')),
            terminal_error TEXT CHECK(terminal_error IS NULL OR length(terminal_error)<=16384),
            CHECK(eligible_count+retained_count=observed_count),
            CHECK(settled_count+refused_count+recovery_required_count+unattempted_count<=eligible_count),
            UNIQUE(repository_identity,idempotency_key)
        );
        CREATE INDEX idx_source_worktree_settlement_runs_state
            ON source_worktree_settlement_runs(state,updated_at,run_id);
        CREATE TABLE source_worktree_settlement_items (
            run_id TEXT NOT NULL REFERENCES source_worktree_settlement_runs(run_id),
            sequence INTEGER NOT NULL CHECK(sequence>=0),
            session_id TEXT NOT NULL REFERENCES sessions(id),
            original_status TEXT NOT NULL CHECK(original_status IN ('Completed','Failed','Interrupted','Archived')),
            original_updated_at TEXT NOT NULL,
            custody_id TEXT NOT NULL REFERENCES sandbox_custody_roots(custody_id),
            custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
            canonical_repo_dir TEXT NOT NULL,
            sandbox_root TEXT NOT NULL,
            sandbox_branch TEXT NOT NULL,
            repository_identity TEXT NOT NULL,
            source_ref TEXT NOT NULL CHECK(source_ref GLOB 'refs/heads/rsi/*'),
            source_oid TEXT NOT NULL CHECK(length(source_oid) IN (40,64) AND source_oid=lower(source_oid) AND source_oid NOT GLOB '*[^0-9a-f]*'),
            target_oid TEXT NOT NULL CHECK(length(target_oid) IN (40,64) AND target_oid=lower(target_oid) AND target_oid NOT GLOB '*[^0-9a-f]*'),
            evidence_digest TEXT NOT NULL CHECK(length(evidence_digest)=71 AND evidence_digest GLOB 'sha256:*' AND substr(evidence_digest,8) NOT GLOB '*[^0-9a-f]*'),
            clean_state_digest TEXT NOT NULL CHECK(length(clean_state_digest)=71 AND clean_state_digest GLOB 'sha256:*' AND substr(clean_state_digest,8) NOT GLOB '*[^0-9a-f]*'),
            reserved_effects INTEGER NOT NULL CHECK(reserved_effects>=0),
            active_effects INTEGER NOT NULL CHECK(active_effects>=0),
            participant_count INTEGER NOT NULL CHECK(participant_count>0),
            phase TEXT NOT NULL CHECK(phase IN ('planned','intent_committed','worktree_removed','branch_removed','settled','refused','recovery_required','unattempted')),
            before_observation TEXT CHECK(before_observation IS NULL OR length(before_observation)<=16384),
            after_observation TEXT CHECK(after_observation IS NULL OR length(after_observation)<=16384),
            refusal_code TEXT CHECK(refusal_code IS NULL OR refusal_code IN (
                'audit_drift','idempotency_conflict','custody_drift','runtime_owner_active',
                'runtime_state_contended','worktree_remove_failed','worktree_residue',
                'target_drift','source_ref_drift','source_ref_in_use','ref_delete_failed',
                'external_registration_race','database_settlement_failed',
                'recovery_target_unavailable','recovery_proof_failed'
            )),
            created_at TEXT NOT NULL CHECK(length(created_at)=30 AND created_at GLOB '????-??-??T??:??:??.?????????Z'),
            updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND updated_at GLOB '????-??-??T??:??:??.?????????Z'),
            PRIMARY KEY(run_id,sequence),
            UNIQUE(run_id,session_id)
        );
        CREATE INDEX idx_source_worktree_settlement_items_phase
            ON source_worktree_settlement_items(run_id,phase,sequence);
        CREATE TRIGGER source_worktree_settlement_runs_no_delete BEFORE DELETE ON source_worktree_settlement_runs
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_no_delete'); END;
        CREATE TRIGGER source_worktree_settlement_runs_identity_immutable BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN NEW.run_id!=OLD.run_id OR NEW.schema_version!=OLD.schema_version OR NEW.policy_version!=OLD.policy_version
              OR NEW.repository_identity!=OLD.repository_identity OR NEW.canonical_repo_dir!=OLD.canonical_repo_dir
              OR NEW.target_ref!=OLD.target_ref OR NEW.target_oid!=OLD.target_oid OR NEW.plan_digest!=OLD.plan_digest
              OR NEW.idempotency_key!=OLD.idempotency_key OR NEW.authorization_digest!=OLD.authorization_digest
              OR NEW.request_fingerprint!=OLD.request_fingerprint OR NEW.observed_count!=OLD.observed_count
              OR NEW.eligible_count!=OLD.eligible_count OR NEW.retained_count!=OLD.retained_count
              OR NEW.created_at!=OLD.created_at
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_identity_immutable'); END;
        CREATE TRIGGER source_worktree_settlement_runs_forward_state BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN NEW.settled_count<OLD.settled_count OR NEW.refused_count<OLD.refused_count
              OR NEW.recovery_required_count<OLD.recovery_required_count
              OR NEW.unattempted_count<OLD.unattempted_count
              OR (NEW.state!=OLD.state AND NOT (
                  (OLD.state='intent_committed' AND NEW.state IN ('applying','settled','partial','recovery_required','refused')) OR
                  (OLD.state='applying' AND NEW.state IN ('settled','partial','recovery_required','refused')) OR
                  (OLD.state='partial' AND NEW.state IN ('settled','recovery_required','refused'))
              ))
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_state_regression'); END;
        CREATE TRIGGER source_worktree_settlement_runs_terminal BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN OLD.state IN ('settled','recovery_required','refused') AND (
                NEW.state!=OLD.state OR NEW.settled_count!=OLD.settled_count
                OR NEW.refused_count!=OLD.refused_count
                OR NEW.recovery_required_count!=OLD.recovery_required_count
                OR NEW.unattempted_count!=OLD.unattempted_count
                OR NEW.updated_at!=OLD.updated_at OR NEW.finished_at IS NOT OLD.finished_at
                OR NEW.terminal_error IS NOT OLD.terminal_error
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_terminal'); END;
        CREATE TRIGGER source_worktree_settlement_items_no_delete BEFORE DELETE ON source_worktree_settlement_items
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_no_delete'); END;
        CREATE TRIGGER source_worktree_settlement_items_identity_immutable BEFORE UPDATE ON source_worktree_settlement_items
            WHEN NEW.run_id!=OLD.run_id OR NEW.sequence!=OLD.sequence OR NEW.session_id!=OLD.session_id
              OR NEW.original_status!=OLD.original_status OR NEW.original_updated_at!=OLD.original_updated_at
              OR NEW.custody_id!=OLD.custody_id OR NEW.custody_generation!=OLD.custody_generation
              OR NEW.canonical_repo_dir!=OLD.canonical_repo_dir OR NEW.sandbox_root!=OLD.sandbox_root
              OR NEW.sandbox_branch!=OLD.sandbox_branch OR NEW.repository_identity!=OLD.repository_identity
              OR NEW.source_ref!=OLD.source_ref OR NEW.source_oid!=OLD.source_oid OR NEW.target_oid!=OLD.target_oid
              OR NEW.evidence_digest!=OLD.evidence_digest OR NEW.clean_state_digest!=OLD.clean_state_digest
              OR NEW.reserved_effects!=OLD.reserved_effects OR NEW.active_effects!=OLD.active_effects
              OR NEW.participant_count!=OLD.participant_count OR NEW.created_at!=OLD.created_at
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_identity_immutable'); END;
        CREATE TRIGGER source_worktree_settlement_items_forward_phase BEFORE UPDATE ON source_worktree_settlement_items
            WHEN NEW.phase!=OLD.phase AND NOT (
                (OLD.phase='planned' AND NEW.phase IN ('intent_committed','refused','recovery_required','unattempted')) OR
                (OLD.phase='intent_committed' AND NEW.phase IN ('worktree_removed','refused','recovery_required','unattempted')) OR
                (OLD.phase='worktree_removed' AND NEW.phase IN ('branch_removed','recovery_required')) OR
                (OLD.phase='branch_removed' AND NEW.phase IN ('settled','recovery_required'))
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_phase_regression'); END;
        CREATE TRIGGER source_worktree_settlement_items_terminal BEFORE UPDATE ON source_worktree_settlement_items
            WHEN OLD.phase IN ('settled','refused','recovery_required','unattempted') AND NEW.phase!=OLD.phase
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_terminal'); END;";

pub(crate) fn install_v94_schema(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(V94_SETTLEMENT_SCHEMA_SQL)?;
    Ok(())
}

fn normalized_catalog_sql(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn v94_settlement_catalog_snapshot(
    connection: &Connection,
) -> Result<Vec<(String, String, String, String)>> {
    Ok(connection
        .prepare(
            "SELECT type,name,tbl_name,COALESCE(sql,'') FROM sqlite_master
             WHERE tbl_name IN (
                 'source_worktree_settlement_runs',
                 'source_worktree_settlement_items'
             )
             ORDER BY type,name,tbl_name",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                normalized_catalog_sql(&row.get::<_, String>(3)?),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn v94_catalog_matches(tx: &Transaction<'_>, schema_sql: &str, fingerprint: &str) -> Result<bool> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(schema_sql)?;
    Ok(
        v94_settlement_catalog_snapshot(tx)? == v94_settlement_catalog_snapshot(&expected)?
            && v94_catalog_fingerprint(tx)? == fingerprint,
    )
}

pub(crate) fn classify_v94_settlement_catalog(
    tx: &Transaction<'_>,
) -> Result<V94SettlementCatalog> {
    if v94_catalog_matches(
        tx,
        V94_SETTLEMENT_SCHEMA_SQL,
        V94_SETTLEMENT_CATALOG_FINGERPRINT,
    )? {
        return Ok(V94SettlementCatalog::Current);
    }
    if v94_catalog_matches(
        tx,
        V94_DEPLOYED_SETTLEMENT_SCHEMA_SQL,
        V94_DEPLOYED_SETTLEMENT_CATALOG_FINGERPRINT,
    )? {
        return Ok(V94SettlementCatalog::Deployed);
    }
    Err(DaemonError::Store(
        "V95 requires an exact accepted V94 settlement catalog".into(),
    ))
}

pub(crate) fn validate_v94_catalog(tx: &Transaction<'_>) -> Result<()> {
    if classify_v94_settlement_catalog(tx)? != V94SettlementCatalog::Current {
        return Err(DaemonError::Store(
            "V94 settlement catalog does not match the complete static projection".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn install_deployed_v94_schema(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(V94_DEPLOYED_SETTLEMENT_SCHEMA_SQL)?;
    Ok(())
}

pub(crate) fn validate_deployed_v94_rows_for_current_target(tx: &Transaction<'_>) -> Result<()> {
    let unsupported_refusal_code: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM source_worktree_settlement_items
              WHERE refusal_code IS NOT NULL
                AND refusal_code NOT IN (
                    'audit_drift','idempotency_conflict','custody_drift','runtime_owner_active',
                    'runtime_state_contended','worktree_remove_failed','worktree_residue',
                    'target_drift','source_ref_drift','source_ref_in_use','ref_delete_failed',
                    'external_registration_race','database_settlement_failed',
                    'recovery_target_unavailable','recovery_proof_failed'
                )
         )",
        [],
        |row| row.get(0),
    )?;
    if unsupported_refusal_code {
        return Err(DaemonError::Store(
            "V95 bridge rejected a deployed V94 refusal code outside the current target catalog"
                .into(),
        ));
    }
    Ok(())
}

fn verify_deployed_v94_copy(tx: &Transaction<'_>) -> Result<()> {
    let run_source_count: i64 = tx.query_row(
        "SELECT count(*) FROM source_worktree_settlement_runs_v94_deployed",
        [],
        |row| row.get(0),
    )?;
    let run_destination_count: i64 = tx.query_row(
        "SELECT count(*) FROM source_worktree_settlement_runs",
        [],
        |row| row.get(0),
    )?;
    let run_difference: i64 = tx.query_row(
        "SELECT
             (SELECT count(*) FROM (
                 SELECT rowid,* FROM source_worktree_settlement_runs_v94_deployed
                 EXCEPT SELECT rowid,* FROM source_worktree_settlement_runs
             )) +
             (SELECT count(*) FROM (
                 SELECT rowid,* FROM source_worktree_settlement_runs
                 EXCEPT SELECT rowid,* FROM source_worktree_settlement_runs_v94_deployed
             ))",
        [],
        |row| row.get(0),
    )?;
    let item_source_count: i64 = tx.query_row(
        "SELECT count(*) FROM source_worktree_settlement_items_v94_deployed",
        [],
        |row| row.get(0),
    )?;
    let item_destination_count: i64 = tx.query_row(
        "SELECT count(*) FROM source_worktree_settlement_items",
        [],
        |row| row.get(0),
    )?;
    let item_difference: i64 = tx.query_row(
        "SELECT
             (SELECT count(*) FROM (
                 SELECT * FROM source_worktree_settlement_items_v94_deployed
                 EXCEPT SELECT * FROM source_worktree_settlement_items
             )) +
             (SELECT count(*) FROM (
                 SELECT * FROM source_worktree_settlement_items
                 EXCEPT SELECT * FROM source_worktree_settlement_items_v94_deployed
             ))",
        [],
        |row| row.get(0),
    )?;
    if run_source_count != run_destination_count
        || run_difference != 0
        || item_source_count != item_destination_count
        || item_difference != 0
    {
        return Err(DaemonError::Store(format!(
            "V95 deployed V94 bridge copy mismatch: runs={run_source_count}/{run_destination_count} diff={run_difference}, items={item_source_count}/{item_destination_count} diff={item_difference}"
        )));
    }
    Ok(())
}

pub(crate) fn bridge_deployed_v94_catalog(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "PRAGMA defer_foreign_keys=ON;
         DROP TRIGGER source_worktree_settlement_items_terminal;
         DROP TRIGGER source_worktree_settlement_items_forward_phase;
         DROP TRIGGER source_worktree_settlement_items_identity_immutable;
         DROP TRIGGER source_worktree_settlement_items_no_delete;
         DROP TRIGGER source_worktree_settlement_runs_terminal;
         DROP TRIGGER source_worktree_settlement_runs_identity_immutable;
         DROP TRIGGER source_worktree_settlement_runs_no_delete;
         DROP INDEX idx_source_worktree_settlement_items_phase;
         DROP INDEX idx_source_worktree_settlement_runs_state;",
    )?;
    v94_bridge_fault(SourceWorktreeSettlementV94BridgeFault::AfterSourceObjectDrop)?;
    tx.execute_batch(
        "ALTER TABLE source_worktree_settlement_items
             RENAME TO source_worktree_settlement_items_v94_deployed;
         ALTER TABLE source_worktree_settlement_runs
             RENAME TO source_worktree_settlement_runs_v94_deployed;",
    )?;
    v94_bridge_fault(SourceWorktreeSettlementV94BridgeFault::AfterRename)?;
    install_v94_schema(tx)?;
    v94_bridge_fault(SourceWorktreeSettlementV94BridgeFault::AfterSchema)?;
    tx.execute_batch(
        "INSERT INTO source_worktree_settlement_runs (
             rowid,run_id,schema_version,policy_version,repository_identity,canonical_repo_dir,
             target_ref,target_oid,plan_digest,idempotency_key,authorization_digest,
             request_fingerprint,state,observed_count,eligible_count,retained_count,
             settled_count,refused_count,recovery_required_count,unattempted_count,
             created_at,updated_at,finished_at,terminal_error
         )
         SELECT rowid,run_id,schema_version,policy_version,repository_identity,canonical_repo_dir,
                target_ref,target_oid,plan_digest,idempotency_key,authorization_digest,
                request_fingerprint,state,observed_count,eligible_count,retained_count,
                settled_count,refused_count,recovery_required_count,unattempted_count,
                created_at,updated_at,finished_at,terminal_error
           FROM source_worktree_settlement_runs_v94_deployed;
         INSERT INTO source_worktree_settlement_items (
             run_id,sequence,session_id,original_status,original_updated_at,custody_id,
             custody_generation,canonical_repo_dir,sandbox_root,sandbox_branch,
             repository_identity,source_ref,source_oid,target_oid,evidence_digest,
             clean_state_digest,reserved_effects,active_effects,participant_count,
             phase,before_observation,after_observation,refusal_code,created_at,updated_at
         )
         SELECT run_id,sequence,session_id,original_status,original_updated_at,custody_id,
                custody_generation,canonical_repo_dir,sandbox_root,sandbox_branch,
                repository_identity,source_ref,source_oid,target_oid,evidence_digest,
                clean_state_digest,reserved_effects,active_effects,participant_count,
                phase,before_observation,after_observation,refusal_code,created_at,updated_at
           FROM source_worktree_settlement_items_v94_deployed;",
    )?;
    verify_deployed_v94_copy(tx)?;
    v94_bridge_fault(SourceWorktreeSettlementV94BridgeFault::AfterCopy)?;
    tx.execute_batch(
        "DROP TABLE source_worktree_settlement_items_v94_deployed;
         DROP TABLE source_worktree_settlement_runs_v94_deployed;",
    )?;
    v94_bridge_fault(SourceWorktreeSettlementV94BridgeFault::AfterShadowDrop)?;
    validate_v94_catalog(tx)
}
// RSI-RELEASED-MIGRATION-END: v94-settlement-projections-and-bridge

// RSI-RELEASED-MIGRATION-BEGIN: v95-settlement-migration
pub(crate) fn drop_v95_replaceable_triggers(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DROP TRIGGER source_worktree_settlement_runs_forward_state;
         DROP TRIGGER source_worktree_settlement_runs_terminal;
         DROP TRIGGER source_worktree_settlement_items_forward_phase;
         DROP TRIGGER source_worktree_settlement_items_terminal;",
    )?;
    Ok(())
}

fn sqlite_noncanonical_uuid(column: &str) -> String {
    format!(
        "CASE WHEN typeof({column})='text' AND octet_length({column})=36 THEN (
             {column}!=lower({column})
             OR instr({column},char(0))!=0
             OR substr({column},9,1)!='-' OR substr({column},14,1)!='-'
             OR substr({column},19,1)!='-' OR substr({column},24,1)!='-'
             OR octet_length(replace({column},'-',''))!=32
             OR replace({column},'-','') GLOB '*[^0-9a-f]*'
             OR replace({column},'-','')='00000000000000000000000000000000'
         ) ELSE 1 END"
    )
}

/// Authenticate the retained V94 rows before V95 changes either catalog or
/// data. V94 intentionally shipped the journal tables before byte/cardinality
/// hardening, so exact catalog authentication alone cannot make those rows
/// safe to load. All checks operate inside the migration's IMMEDIATE
/// transaction and use SQLite's non-materializing `octet_length()` to measure
/// UTF-8 bytes. Storage classes are authenticated independently so a bounded
/// BLOB or numeric coercion can never reach a Rust `String` decoder.
pub(crate) fn validate_v95_source_rows(tx: &Transaction<'_>) -> Result<()> {
    let run_id_invalid = sqlite_noncanonical_uuid("r.run_id");
    let invalid_runs_sql = format!(
        "SELECT EXISTS(
             SELECT 1 FROM source_worktree_settlement_runs r
              WHERE typeof(r.schema_version)!='integer'
                 OR typeof(r.policy_version)!='integer'
                 OR typeof(r.observed_count)!='integer'
                 OR typeof(r.eligible_count)!='integer'
                 OR typeof(r.retained_count)!='integer'
                 OR typeof(r.settled_count)!='integer'
                 OR typeof(r.refused_count)!='integer'
                 OR typeof(r.recovery_required_count)!='integer'
                 OR typeof(r.unattempted_count)!='integer'
                 OR r.observed_count NOT BETWEEN 1 AND 256
                 OR r.eligible_count NOT BETWEEN 1 AND 256
                 OR r.retained_count NOT BETWEEN 0 AND 256
                 OR r.settled_count NOT BETWEEN 0 AND 256
                 OR r.refused_count NOT BETWEEN 0 AND 256
                 OR r.recovery_required_count NOT BETWEEN 0 AND 256
                 OR r.unattempted_count NOT BETWEEN 0 AND 256
                 OR r.eligible_count+r.retained_count!=r.observed_count
                 OR r.settled_count+r.refused_count+r.recovery_required_count+r.unattempted_count>r.eligible_count
                 OR {run_id_invalid}
                 OR typeof(r.repository_identity)!='text'
                 OR typeof(r.canonical_repo_dir)!='text'
                 OR typeof(r.target_ref)!='text'
                 OR typeof(r.target_oid)!='text'
                 OR typeof(r.plan_digest)!='text'
                 OR typeof(r.idempotency_key)!='text'
                 OR typeof(r.authorization_digest)!='text'
                 OR typeof(r.request_fingerprint)!='text'
                 OR typeof(r.state)!='text'
                 OR typeof(r.created_at)!='text'
                 OR typeof(r.updated_at)!='text'
                 OR octet_length(r.repository_identity) NOT BETWEEN 1 AND 4096
                 OR octet_length(r.canonical_repo_dir) NOT BETWEEN 1 AND 4096
                 OR octet_length(r.target_ref) NOT BETWEEN 1 AND 4096
                 OR octet_length(r.target_oid) NOT IN (40,64)
                 OR octet_length(r.plan_digest)!=71
                 OR octet_length(r.idempotency_key) NOT BETWEEN 1 AND 128
                 OR octet_length(r.authorization_digest)!=71
                 OR octet_length(r.request_fingerprint)!=71
                 OR octet_length(r.state) NOT BETWEEN 1 AND 32
                 OR octet_length(r.created_at)!=30
                 OR octet_length(r.updated_at)!=30
                 OR (r.finished_at IS NOT NULL AND
                     (typeof(r.finished_at)!='text' OR octet_length(r.finished_at)!=30))
                 OR (r.terminal_error IS NOT NULL AND
                     (typeof(r.terminal_error)!='text' OR octet_length(r.terminal_error)>16384))
         )"
    );
    let invalid_runs: bool = tx.query_row(&invalid_runs_sql, [], |row| row.get(0))?;
    if invalid_runs {
        return Err(DaemonError::Store(
            "V95 settlement hardening rejected an out-of-bounds V94 run row".into(),
        ));
    }

    let item_run_id_invalid = sqlite_noncanonical_uuid("i.run_id");
    let item_session_id_invalid = sqlite_noncanonical_uuid("i.session_id");
    let item_custody_id_invalid = sqlite_noncanonical_uuid("i.custody_id");
    let invalid_items_sql = format!(
        "SELECT EXISTS(
             SELECT 1 FROM source_worktree_settlement_items i
              WHERE typeof(i.sequence)!='integer'
                 OR typeof(i.custody_generation)!='integer'
                 OR typeof(i.reserved_effects)!='integer'
                 OR typeof(i.active_effects)!='integer'
                 OR typeof(i.participant_count)!='integer'
                 OR i.sequence NOT BETWEEN 0 AND 255
                 OR typeof(i.run_id)!='text'
                 OR typeof(i.session_id)!='text'
                 OR typeof(i.original_status)!='text'
                 OR typeof(i.original_updated_at)!='text'
                 OR typeof(i.custody_id)!='text'
                 OR typeof(i.canonical_repo_dir)!='text'
                 OR typeof(i.sandbox_root)!='text'
                 OR typeof(i.sandbox_branch)!='text'
                 OR typeof(i.repository_identity)!='text'
                 OR typeof(i.source_ref)!='text'
                 OR typeof(i.source_oid)!='text'
                 OR typeof(i.target_oid)!='text'
                 OR typeof(i.evidence_digest)!='text'
                 OR typeof(i.clean_state_digest)!='text'
                 OR typeof(i.phase)!='text'
                 OR typeof(i.created_at)!='text'
                 OR typeof(i.updated_at)!='text'
                 OR {item_run_id_invalid}
                 OR {item_session_id_invalid}
                 OR octet_length(i.original_status) NOT BETWEEN 1 AND 32
                 OR octet_length(i.original_updated_at) NOT BETWEEN 1 AND 64
                 OR {item_custody_id_invalid}
                 OR i.custody_generation<=0
                 OR octet_length(i.canonical_repo_dir) NOT BETWEEN 1 AND 4096
                 OR octet_length(i.sandbox_root) NOT BETWEEN 1 AND 4096
                 OR octet_length(i.sandbox_branch) NOT BETWEEN 1 AND 4096
                 OR octet_length(i.repository_identity) NOT BETWEEN 1 AND 4096
                 OR octet_length(i.source_ref) NOT BETWEEN 1 AND 4096
                 OR octet_length(i.source_oid) NOT IN (40,64)
                 OR octet_length(i.target_oid) NOT IN (40,64)
                 OR octet_length(i.evidence_digest)!=71
                 OR octet_length(i.clean_state_digest)!=71
                 OR i.reserved_effects<0 OR i.active_effects<0 OR i.participant_count<=0
                 OR octet_length(i.phase) NOT BETWEEN 1 AND 32
                 OR (i.before_observation IS NOT NULL AND
                     (typeof(i.before_observation)!='text' OR octet_length(i.before_observation)>16384))
                 OR (i.after_observation IS NOT NULL AND
                     (typeof(i.after_observation)!='text' OR octet_length(i.after_observation)>16384))
                 OR (i.refusal_code IS NOT NULL AND
                     (typeof(i.refusal_code)!='text' OR octet_length(i.refusal_code)>128))
                 OR octet_length(i.created_at)!=30
                 OR octet_length(i.updated_at)!=30
                 OR NOT EXISTS (
                     SELECT 1 FROM source_worktree_settlement_runs r
                      WHERE r.run_id=i.run_id
                        AND r.repository_identity=i.repository_identity
                        AND r.canonical_repo_dir=i.canonical_repo_dir
                        AND r.target_oid=i.target_oid
                 )
         )"
    );
    let invalid_items: bool = tx.query_row(&invalid_items_sql, [], |row| row.get(0))?;
    if invalid_items {
        return Err(DaemonError::Store(
            "V95 settlement hardening rejected an out-of-bounds V94 item row".into(),
        ));
    }
    let duplicate_custody_authority: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM source_worktree_settlement_items item
              WHERE EXISTS (
                  SELECT 1 FROM source_worktree_settlement_items duplicate
                   WHERE duplicate.run_id=item.run_id
                     AND duplicate.sequence>item.sequence
                     AND duplicate.custody_id=item.custody_id
              )
              LIMIT 1
         )",
        [],
        |row| row.get(0),
    )?;
    if duplicate_custody_authority {
        return Err(DaemonError::Store(
            "V95 settlement hardening rejected duplicate custody authority within a V94 run".into(),
        ));
    }
    let mut timestamps = tx.prepare(
        "SELECT original_updated_at FROM source_worktree_settlement_items
         ORDER BY run_id,sequence",
    )?;
    let mut timestamps = timestamps.query([])?;
    while let Some(row) = timestamps.next()? {
        let value = row.get::<_, String>(0)?;
        super::row_mappers::parse_timestamp(&value).map_err(|error| {
            DaemonError::Store(format!(
                "V95 settlement hardening rejected an invalid original_updated_at: {error}"
            ))
        })?;
    }

    let invalid_cardinality: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM source_worktree_settlement_runs r
              WHERE r.eligible_count!=(
                  SELECT count(*) FROM source_worktree_settlement_items i
                   WHERE i.run_id=r.run_id
              )
         ) OR EXISTS(
             SELECT 1 FROM (
                 SELECT run_id,count(*) AS item_count,min(sequence) AS first_sequence,
                        max(sequence) AS last_sequence
                   FROM source_worktree_settlement_items GROUP BY run_id
             ) grouped
              WHERE item_count NOT BETWEEN 1 AND 256
                 OR first_sequence!=0 OR last_sequence!=item_count-1
         )",
        [],
        |row| row.get(0),
    )?;
    if invalid_cardinality {
        return Err(DaemonError::Store(
            "V95 settlement hardening rejected a noncanonical V94 run/item cardinality".into(),
        ));
    }
    Ok(())
}

pub(crate) fn normalize_v95_runs(tx: &Transaction<'_>) -> Result<()> {
    let invalid_items: i64 = tx.query_row(
        "SELECT count(*) FROM source_worktree_settlement_items
         WHERE (phase IN ('refused','recovery_required') AND refusal_code IS NULL)
            OR (phase NOT IN ('refused','recovery_required','branch_removed') AND refusal_code IS NOT NULL)
            OR (phase='branch_removed' AND refusal_code IS NOT NULL
                AND refusal_code!='database_settlement_failed')",
        [],
        |row| row.get(0),
    )?;
    if invalid_items != 0 {
        return Err(DaemonError::Store(format!(
            "V95 settlement hardening found {invalid_items} item phase/refusal mismatch(es)"
        )));
    }
    let invalid_runs: i64 = tx.query_row(
        "SELECT count(*) FROM source_worktree_settlement_runs r
         WHERE r.eligible_count<=0 OR r.eligible_count!=(
             SELECT count(*) FROM source_worktree_settlement_items i WHERE i.run_id=r.run_id
         )",
        [],
        |row| row.get(0),
    )?;
    if invalid_runs != 0 {
        return Err(DaemonError::Store(format!(
            "V95 settlement hardening found {invalid_runs} run/item cardinality mismatch(es)"
        )));
    }
    // V94 could record a post-effect crash as Refused while its durable phase
    // still read IntentCommitted.  There is no effect-started bit with which
    // to distinguish those rows from a genuine pre-effect refusal.  Convert
    // the closed legacy set conservatively while the V94 terminal/forward
    // triggers are down; every Refused written after V95 is therefore again a
    // proof of no effect and can remain an admissible control state.
    tx.execute(
        "UPDATE source_worktree_settlement_items
         SET phase='recovery_required'
         WHERE phase='refused'",
        [],
    )?;
    tx.execute_batch(
        "WITH item_counts AS (
             SELECT run_id,
                    count(*) AS eligible,
                    sum(CASE WHEN phase='settled' THEN 1 ELSE 0 END) AS settled,
                    sum(CASE WHEN phase='refused' THEN 1 ELSE 0 END) AS refused,
                    sum(CASE WHEN phase='recovery_required' THEN 1 ELSE 0 END) AS recovery,
                    sum(CASE WHEN phase='unattempted' THEN 1 ELSE 0 END) AS unattempted,
                    sum(CASE WHEN phase='intent_committed' THEN 1 ELSE 0 END) AS intent,
                    max(updated_at) AS item_updated_at
             FROM source_worktree_settlement_items GROUP BY run_id
         )
         UPDATE source_worktree_settlement_runs AS r
         SET settled_count=(SELECT settled FROM item_counts WHERE run_id=r.run_id),
             refused_count=(SELECT refused FROM item_counts WHERE run_id=r.run_id),
             recovery_required_count=(SELECT recovery FROM item_counts WHERE run_id=r.run_id),
             unattempted_count=(SELECT unattempted FROM item_counts WHERE run_id=r.run_id),
             state=(SELECT CASE
                 WHEN settled+refused+recovery+unattempted=eligible AND recovery>0
                     THEN 'recovery_required'
                 WHEN settled+refused+recovery+unattempted=eligible AND settled=eligible
                     THEN 'settled'
                 WHEN settled+refused+recovery+unattempted=eligible AND settled=0
                     THEN 'refused'
                 WHEN settled+refused+recovery+unattempted>0 THEN 'partial'
                 WHEN intent=eligible THEN 'intent_committed'
                 ELSE 'applying' END FROM item_counts WHERE run_id=r.run_id),
             updated_at=max(updated_at,(SELECT item_updated_at FROM item_counts WHERE run_id=r.run_id)),
             finished_at=(SELECT CASE
                 WHEN settled+refused+recovery+unattempted=eligible
                 THEN max(coalesce(r.finished_at,r.updated_at),r.updated_at,item_updated_at)
                 ELSE NULL END FROM item_counts WHERE run_id=r.run_id);",
    )?;
    Ok(())
}

/// Assign every retained V94 run one immutable repository-local generation.
/// `rowid` is used only inside this migration transaction to recover the V94
/// insertion order; runtime authority is the persisted generation below.
pub(crate) fn install_v95_run_order(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE source_worktree_settlement_run_order (
            run_id TEXT PRIMARY KEY REFERENCES source_worktree_settlement_runs(run_id),
            repository_identity TEXT NOT NULL,
            canonical_repo_dir TEXT NOT NULL,
            generation INTEGER NOT NULL CHECK(generation>0),
            UNIQUE(repository_identity,generation),
            UNIQUE(repository_identity,canonical_repo_dir,run_id,generation)
        );
        INSERT INTO source_worktree_settlement_run_order (
            run_id,repository_identity,canonical_repo_dir,generation
        )
        SELECT run_id,repository_identity,canonical_repo_dir,
               row_number() OVER (
                   PARTITION BY repository_identity ORDER BY rowid
               )
          FROM source_worktree_settlement_runs;",
    )?;
    Ok(())
}

pub(crate) fn install_v95_latest_run_pointer(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE source_worktree_settlement_latest_runs (
            repository_identity TEXT PRIMARY KEY,
            canonical_repo_dir TEXT NOT NULL,
            run_id TEXT NOT NULL UNIQUE,
            generation INTEGER NOT NULL CHECK(generation>0),
            FOREIGN KEY(repository_identity,canonical_repo_dir,run_id,generation)
                REFERENCES source_worktree_settlement_run_order(
                    repository_identity,canonical_repo_dir,run_id,generation
                )
        );
        INSERT INTO source_worktree_settlement_latest_runs (
            repository_identity,canonical_repo_dir,run_id,generation
        )
        SELECT ordered.repository_identity,ordered.canonical_repo_dir,
               ordered.run_id,ordered.generation
          FROM source_worktree_settlement_run_order ordered
         WHERE NOT EXISTS (
             SELECT 1 FROM source_worktree_settlement_run_order newer
              WHERE newer.repository_identity=ordered.repository_identity
                AND newer.generation>ordered.generation
         );",
    )?;
    Ok(())
}

pub(crate) fn install_v95_indexes(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE INDEX idx_source_worktree_settlement_items_custody_phase
            ON source_worktree_settlement_items(custody_id,phase);
         CREATE INDEX idx_source_worktree_settlement_items_repository_custody_fence
            ON source_worktree_settlement_items(repository_identity,custody_id)
            WHERE phase NOT IN ('refused','unattempted');
         CREATE INDEX idx_source_worktree_settlement_items_session_phase
            ON source_worktree_settlement_items(session_id,phase);
         CREATE INDEX idx_source_worktree_settlement_items_root_phase
            ON source_worktree_settlement_items(sandbox_root,phase);",
    )?;
    Ok(())
}

pub(crate) fn install_v95_triggers(tx: &Transaction<'_>) -> Result<()> {
    let run_id_invalid = sqlite_noncanonical_uuid("NEW.run_id");
    let session_id_invalid = sqlite_noncanonical_uuid("NEW.session_id");
    let custody_id_invalid = sqlite_noncanonical_uuid("NEW.custody_id");
    let item_run_id_invalid = sqlite_noncanonical_uuid("NEW.run_id");
    let run_text_bounds = format!(
        "
              {run_id_invalid}
              OR typeof(NEW.repository_identity)!='text'
              OR typeof(NEW.canonical_repo_dir)!='text'
              OR typeof(NEW.target_ref)!='text'
              OR typeof(NEW.target_oid)!='text'
              OR typeof(NEW.plan_digest)!='text'
              OR typeof(NEW.idempotency_key)!='text'
              OR typeof(NEW.authorization_digest)!='text'
              OR typeof(NEW.request_fingerprint)!='text'
              OR typeof(NEW.state)!='text'
              OR typeof(NEW.created_at)!='text'
              OR typeof(NEW.updated_at)!='text'
              OR octet_length(NEW.repository_identity) NOT BETWEEN 1 AND 4096
              OR octet_length(NEW.canonical_repo_dir) NOT BETWEEN 1 AND 4096
              OR octet_length(NEW.target_ref) NOT BETWEEN 1 AND 4096
              OR octet_length(NEW.target_oid) NOT IN (40,64)
              OR octet_length(NEW.plan_digest)!=71
              OR octet_length(NEW.idempotency_key) NOT BETWEEN 1 AND 128
              OR octet_length(NEW.authorization_digest)!=71
              OR octet_length(NEW.request_fingerprint)!=71
              OR octet_length(NEW.state) NOT BETWEEN 1 AND 32
              OR octet_length(NEW.created_at)!=30
              OR octet_length(NEW.updated_at)!=30
              OR (NEW.finished_at IS NOT NULL AND
                  (typeof(NEW.finished_at)!='text' OR octet_length(NEW.finished_at)!=30))
              OR (NEW.terminal_error IS NOT NULL AND
                  (typeof(NEW.terminal_error)!='text' OR octet_length(NEW.terminal_error)>16384))"
    );
    let item_text_bounds = format!(
        "
              {item_run_id_invalid}
              OR {session_id_invalid}
              OR {custody_id_invalid}
              OR typeof(NEW.original_status)!='text'
              OR typeof(NEW.original_updated_at)!='text'
              OR typeof(NEW.canonical_repo_dir)!='text'
              OR typeof(NEW.sandbox_root)!='text'
              OR typeof(NEW.sandbox_branch)!='text'
              OR typeof(NEW.repository_identity)!='text'
              OR typeof(NEW.source_ref)!='text'
              OR typeof(NEW.source_oid)!='text'
              OR typeof(NEW.target_oid)!='text'
              OR typeof(NEW.evidence_digest)!='text'
              OR typeof(NEW.clean_state_digest)!='text'
              OR typeof(NEW.phase)!='text'
              OR typeof(NEW.created_at)!='text'
              OR typeof(NEW.updated_at)!='text'
              OR octet_length(NEW.original_status) NOT BETWEEN 1 AND 32
              OR octet_length(NEW.original_updated_at) NOT BETWEEN 1 AND 64
              OR octet_length(NEW.canonical_repo_dir) NOT BETWEEN 1 AND 4096
              OR octet_length(NEW.sandbox_root) NOT BETWEEN 1 AND 4096
              OR octet_length(NEW.sandbox_branch) NOT BETWEEN 1 AND 4096
              OR octet_length(NEW.repository_identity) NOT BETWEEN 1 AND 4096
              OR octet_length(NEW.source_ref) NOT BETWEEN 1 AND 4096
              OR octet_length(NEW.source_oid) NOT IN (40,64)
              OR octet_length(NEW.target_oid) NOT IN (40,64)
              OR octet_length(NEW.evidence_digest)!=71
              OR octet_length(NEW.clean_state_digest)!=71
              OR octet_length(NEW.phase) NOT BETWEEN 1 AND 32
              OR (NEW.before_observation IS NOT NULL AND
                  (typeof(NEW.before_observation)!='text' OR octet_length(NEW.before_observation)>16384))
              OR (NEW.after_observation IS NOT NULL AND
                  (typeof(NEW.after_observation)!='text' OR octet_length(NEW.after_observation)>16384))
              OR (NEW.refusal_code IS NOT NULL AND
                  (typeof(NEW.refusal_code)!='text' OR octet_length(NEW.refusal_code)>128))
              OR octet_length(NEW.created_at)!=30
              OR octet_length(NEW.updated_at)!=30"
    );
    let latest_run_id_invalid = sqlite_noncanonical_uuid("NEW.run_id");
    let latest_bounds = format!(
        "typeof(NEW.repository_identity)!='text'
              OR typeof(NEW.canonical_repo_dir)!='text'
              OR {latest_run_id_invalid}
              OR typeof(NEW.generation)!='integer'
              OR octet_length(NEW.repository_identity) NOT BETWEEN 1 AND 4096
              OR octet_length(NEW.canonical_repo_dir) NOT BETWEEN 1 AND 4096
              OR NEW.generation<=0"
    );
    let sql = format!(
        "CREATE TRIGGER source_worktree_settlement_runs_bounds_insert BEFORE INSERT ON source_worktree_settlement_runs
            WHEN typeof(NEW.schema_version)!='integer'
              OR typeof(NEW.policy_version)!='integer'
              OR typeof(NEW.observed_count)!='integer'
              OR typeof(NEW.eligible_count)!='integer'
              OR typeof(NEW.retained_count)!='integer'
              OR typeof(NEW.settled_count)!='integer'
              OR typeof(NEW.refused_count)!='integer'
              OR typeof(NEW.recovery_required_count)!='integer'
              OR typeof(NEW.unattempted_count)!='integer'
              OR NEW.observed_count NOT BETWEEN 1 AND 256
              OR NEW.eligible_count NOT BETWEEN 1 AND 256
              OR NEW.retained_count NOT BETWEEN 0 AND 256
              OR NEW.settled_count NOT BETWEEN 0 AND 256
              OR NEW.refused_count NOT BETWEEN 0 AND 256
              OR NEW.recovery_required_count NOT BETWEEN 0 AND 256
              OR NEW.unattempted_count NOT BETWEEN 0 AND 256
              OR NEW.eligible_count+NEW.retained_count!=NEW.observed_count
              OR NEW.settled_count+NEW.refused_count+NEW.recovery_required_count+NEW.unattempted_count>NEW.eligible_count
              OR {run_text_bounds}
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_bounds'); END;
         CREATE TRIGGER source_worktree_settlement_runs_bounds_update BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN typeof(NEW.schema_version)!='integer'
              OR typeof(NEW.policy_version)!='integer'
              OR typeof(NEW.observed_count)!='integer'
              OR typeof(NEW.eligible_count)!='integer'
              OR typeof(NEW.retained_count)!='integer'
              OR typeof(NEW.settled_count)!='integer'
              OR typeof(NEW.refused_count)!='integer'
              OR typeof(NEW.recovery_required_count)!='integer'
              OR typeof(NEW.unattempted_count)!='integer'
              OR NEW.observed_count NOT BETWEEN 1 AND 256
              OR NEW.eligible_count NOT BETWEEN 1 AND 256
              OR NEW.retained_count NOT BETWEEN 0 AND 256
              OR NEW.settled_count NOT BETWEEN 0 AND 256
              OR NEW.refused_count NOT BETWEEN 0 AND 256
              OR NEW.recovery_required_count NOT BETWEEN 0 AND 256
              OR NEW.unattempted_count NOT BETWEEN 0 AND 256
              OR NEW.eligible_count+NEW.retained_count!=NEW.observed_count
              OR NEW.settled_count+NEW.refused_count+NEW.recovery_required_count+NEW.unattempted_count>NEW.eligible_count
              OR NEW.eligible_count!=(SELECT count(*) FROM source_worktree_settlement_items i WHERE i.run_id=NEW.run_id)
              OR {run_text_bounds}
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_bounds'); END;
         CREATE TRIGGER source_worktree_settlement_items_run_association_insert
            BEFORE INSERT ON source_worktree_settlement_items
            BEGIN
                SELECT RAISE(ABORT,'source_worktree_settlement_item_run_association')
                 WHERE NOT EXISTS (
                    SELECT 1 FROM source_worktree_settlement_runs r
                     WHERE r.run_id=NEW.run_id
                       AND r.repository_identity=NEW.repository_identity
                       AND r.canonical_repo_dir=NEW.canonical_repo_dir
                       AND r.target_oid=NEW.target_oid
                 );
                SELECT RAISE(ABORT,'source_worktree_settlement_item_duplicate_custody')
                 WHERE EXISTS (
                    SELECT 1 FROM source_worktree_settlement_items existing
                     WHERE existing.run_id=NEW.run_id
                       AND existing.custody_id=NEW.custody_id
                 );
            END;
         CREATE TRIGGER source_worktree_settlement_items_bounds_insert BEFORE INSERT ON source_worktree_settlement_items
            WHEN typeof(NEW.sequence)!='integer'
              OR typeof(NEW.custody_generation)!='integer'
              OR typeof(NEW.reserved_effects)!='integer'
              OR typeof(NEW.active_effects)!='integer'
              OR typeof(NEW.participant_count)!='integer'
              OR NEW.sequence NOT BETWEEN 0 AND 255
              OR NEW.sequence!=(SELECT count(*) FROM source_worktree_settlement_items i WHERE i.run_id=NEW.run_id)
              OR NEW.sequence>=coalesce((SELECT eligible_count FROM source_worktree_settlement_runs r WHERE r.run_id=NEW.run_id),0)
              OR NEW.custody_generation<=0
              OR NEW.reserved_effects<0 OR NEW.active_effects<0 OR NEW.participant_count<=0
              OR {item_text_bounds}
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_bounds'); END;
         CREATE TRIGGER source_worktree_settlement_items_bounds_update BEFORE UPDATE ON source_worktree_settlement_items
            WHEN typeof(NEW.sequence)!='integer'
              OR typeof(NEW.custody_generation)!='integer'
              OR typeof(NEW.reserved_effects)!='integer'
              OR typeof(NEW.active_effects)!='integer'
              OR typeof(NEW.participant_count)!='integer'
              OR NEW.sequence NOT BETWEEN 0 AND 255
              OR NEW.sequence>=coalesce((SELECT eligible_count FROM source_worktree_settlement_runs r WHERE r.run_id=NEW.run_id),0)
              OR NEW.custody_generation<=0
              OR NEW.reserved_effects<0 OR NEW.active_effects<0 OR NEW.participant_count<=0
              OR {item_text_bounds}
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_bounds'); END;
         CREATE TRIGGER source_worktree_settlement_items_forward_phase BEFORE UPDATE ON source_worktree_settlement_items
            WHEN NEW.phase!=OLD.phase AND NOT (
                (OLD.phase='planned' AND NEW.phase IN ('intent_committed','refused','recovery_required','unattempted')) OR
                (OLD.phase='intent_committed' AND NEW.phase IN ('worktree_removed','refused','recovery_required','unattempted')) OR
                (OLD.phase='worktree_removed' AND NEW.phase IN ('branch_removed','recovery_required')) OR
                (OLD.phase='branch_removed' AND NEW.phase IN ('settled','recovery_required'))
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_phase_regression'); END;
         CREATE TRIGGER source_worktree_settlement_runs_forward_state BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN NEW.settled_count<OLD.settled_count OR NEW.refused_count<OLD.refused_count
              OR NEW.recovery_required_count<OLD.recovery_required_count
              OR NEW.unattempted_count<OLD.unattempted_count
              OR (NEW.state!=OLD.state AND NOT (
                  (OLD.state='intent_committed' AND NEW.state IN ('applying','settled','partial','recovery_required','refused')) OR
                  (OLD.state='applying' AND NEW.state IN ('settled','partial','recovery_required','refused')) OR
                  (OLD.state='partial' AND NEW.state IN ('settled','recovery_required','refused'))
              ))
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_state_regression'); END;
         CREATE TRIGGER source_worktree_settlement_runs_terminal BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN (OLD.state IN ('settled','recovery_required','refused')
                  OR (OLD.state='partial' AND OLD.finished_at IS NOT NULL)) AND (
                NEW.state!=OLD.state OR NEW.settled_count!=OLD.settled_count
                OR NEW.refused_count!=OLD.refused_count
                OR NEW.recovery_required_count!=OLD.recovery_required_count
                OR NEW.unattempted_count!=OLD.unattempted_count
                OR NEW.updated_at!=OLD.updated_at OR NEW.finished_at IS NOT OLD.finished_at
                OR NEW.terminal_error IS NOT OLD.terminal_error
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_terminal'); END;
         CREATE TRIGGER source_worktree_settlement_items_terminal BEFORE UPDATE ON source_worktree_settlement_items
            WHEN OLD.phase IN ('settled','refused','recovery_required','unattempted') AND (
                NEW.phase!=OLD.phase OR NEW.before_observation IS NOT OLD.before_observation
                OR NEW.after_observation IS NOT OLD.after_observation
                OR NEW.refusal_code IS NOT OLD.refusal_code OR NEW.updated_at!=OLD.updated_at
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_terminal'); END;
         CREATE TRIGGER source_worktree_settlement_run_order_no_delete
            BEFORE DELETE ON source_worktree_settlement_run_order
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_order_no_delete'); END;
         CREATE TRIGGER source_worktree_settlement_run_order_no_update
            BEFORE UPDATE ON source_worktree_settlement_run_order
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_order_no_update'); END;
         CREATE TRIGGER source_worktree_settlement_run_order_no_reuse_insert
            BEFORE INSERT ON source_worktree_settlement_run_order
            WHEN EXISTS (
                SELECT 1 FROM source_worktree_settlement_run_order existing
                 WHERE existing.run_id=NEW.run_id
                    OR (existing.repository_identity=NEW.repository_identity
                        AND existing.generation=NEW.generation)
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_order_no_reuse'); END;
         CREATE TRIGGER source_worktree_settlement_run_order_bounds_insert
            BEFORE INSERT ON source_worktree_settlement_run_order
            WHEN {latest_bounds}
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_order_bounds'); END;
         CREATE TRIGGER source_worktree_settlement_run_order_association_insert
            BEFORE INSERT ON source_worktree_settlement_run_order
            WHEN NOT EXISTS (
                SELECT 1 FROM source_worktree_settlement_runs r
                 WHERE r.run_id=NEW.run_id
                   AND r.repository_identity=NEW.repository_identity
                   AND r.canonical_repo_dir=NEW.canonical_repo_dir
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_order_association'); END;
         CREATE TRIGGER source_worktree_settlement_run_order_append_insert
            BEFORE INSERT ON source_worktree_settlement_run_order
            WHEN NEW.generation!=coalesce((
                SELECT max(existing.generation)
                  FROM source_worktree_settlement_run_order existing
                 WHERE existing.repository_identity=NEW.repository_identity
            ),0)+1
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_order_not_append'); END;
         CREATE TRIGGER source_worktree_settlement_latest_runs_no_delete
            BEFORE DELETE ON source_worktree_settlement_latest_runs
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_latest_run_no_delete'); END;
         CREATE TRIGGER source_worktree_settlement_latest_runs_no_replace_insert
            BEFORE INSERT ON source_worktree_settlement_latest_runs
            WHEN EXISTS (
                SELECT 1 FROM source_worktree_settlement_latest_runs existing
                 WHERE existing.repository_identity=NEW.repository_identity
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_latest_run_no_replace'); END;
         CREATE TRIGGER source_worktree_settlement_latest_runs_bounds_insert
            BEFORE INSERT ON source_worktree_settlement_latest_runs
            WHEN {latest_bounds}
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_latest_run_bounds'); END;
         CREATE TRIGGER source_worktree_settlement_latest_runs_bounds_update
            BEFORE UPDATE ON source_worktree_settlement_latest_runs
            WHEN {latest_bounds}
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_latest_run_bounds'); END;
         CREATE TRIGGER source_worktree_settlement_latest_runs_association_insert
            BEFORE INSERT ON source_worktree_settlement_latest_runs
            WHEN NOT EXISTS (
                SELECT 1 FROM source_worktree_settlement_run_order ordered
                 WHERE ordered.run_id=NEW.run_id
                   AND ordered.repository_identity=NEW.repository_identity
                   AND ordered.canonical_repo_dir=NEW.canonical_repo_dir
                   AND ordered.generation=NEW.generation
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_latest_run_association'); END;
         CREATE TRIGGER source_worktree_settlement_latest_runs_association_update
            BEFORE UPDATE ON source_worktree_settlement_latest_runs
            WHEN NOT EXISTS (
                SELECT 1 FROM source_worktree_settlement_run_order ordered
                 WHERE ordered.run_id=NEW.run_id
                   AND ordered.repository_identity=NEW.repository_identity
                   AND ordered.canonical_repo_dir=NEW.canonical_repo_dir
                   AND ordered.generation=NEW.generation
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_latest_run_association'); END;
         CREATE TRIGGER source_worktree_settlement_latest_runs_forward
            BEFORE UPDATE ON source_worktree_settlement_latest_runs
            WHEN NEW.repository_identity!=OLD.repository_identity
              OR NEW.run_id=OLD.run_id
              OR NEW.generation!=OLD.generation+1
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_latest_run_regression'); END;
         CREATE TRIGGER source_worktree_settlement_latest_runs_advance
            AFTER INSERT ON source_worktree_settlement_runs
            BEGIN
                INSERT INTO source_worktree_settlement_run_order (
                    run_id,repository_identity,canonical_repo_dir,generation
                ) VALUES (
                    NEW.run_id,NEW.repository_identity,NEW.canonical_repo_dir,
                    coalesce((
                        SELECT max(existing.generation)
                          FROM source_worktree_settlement_run_order existing
                         WHERE existing.repository_identity=NEW.repository_identity
                    ),0)+1
                );
                INSERT INTO source_worktree_settlement_latest_runs (
                    repository_identity,canonical_repo_dir,run_id,generation
                )
                SELECT repository_identity,canonical_repo_dir,run_id,generation
                  FROM source_worktree_settlement_run_order
                 WHERE run_id=NEW.run_id
                   AND NOT EXISTS (
                       SELECT 1 FROM source_worktree_settlement_latest_runs latest
                        WHERE latest.repository_identity=NEW.repository_identity
                   );
                UPDATE source_worktree_settlement_latest_runs
                   SET canonical_repo_dir=NEW.canonical_repo_dir,
                       run_id=NEW.run_id,
                       generation=(
                           SELECT ordered.generation
                             FROM source_worktree_settlement_run_order ordered
                            WHERE ordered.run_id=NEW.run_id
                       )
                 WHERE repository_identity=NEW.repository_identity
                   AND run_id!=NEW.run_id;
                SELECT RAISE(ABORT,'source_worktree_settlement_latest_run_not_advanced')
                 WHERE NOT EXISTS (
                    SELECT 1
                      FROM source_worktree_settlement_latest_runs latest
                      JOIN source_worktree_settlement_run_order ordered
                        ON ordered.repository_identity=latest.repository_identity
                       AND ordered.canonical_repo_dir=latest.canonical_repo_dir
                       AND ordered.run_id=latest.run_id
                       AND ordered.generation=latest.generation
                     WHERE latest.repository_identity=NEW.repository_identity
                       AND latest.canonical_repo_dir=NEW.canonical_repo_dir
                       AND latest.run_id=NEW.run_id
                 );
            END;",
    );
    tx.execute_batch(&sql)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn restore_v94_hardening(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DROP TRIGGER source_worktree_settlement_latest_runs_advance;
         DROP TRIGGER source_worktree_settlement_latest_runs_no_delete;
         DROP TRIGGER source_worktree_settlement_latest_runs_no_replace_insert;
         DROP TRIGGER source_worktree_settlement_latest_runs_bounds_insert;
         DROP TRIGGER source_worktree_settlement_latest_runs_bounds_update;
         DROP TRIGGER source_worktree_settlement_latest_runs_association_insert;
         DROP TRIGGER source_worktree_settlement_latest_runs_association_update;
         DROP TRIGGER source_worktree_settlement_latest_runs_forward;
         DROP TABLE source_worktree_settlement_latest_runs;
         DROP TRIGGER source_worktree_settlement_run_order_no_delete;
         DROP TRIGGER source_worktree_settlement_run_order_no_update;
         DROP TRIGGER source_worktree_settlement_run_order_no_reuse_insert;
         DROP TRIGGER source_worktree_settlement_run_order_bounds_insert;
         DROP TRIGGER source_worktree_settlement_run_order_association_insert;
         DROP TRIGGER source_worktree_settlement_run_order_append_insert;
         DROP TABLE source_worktree_settlement_run_order;
         DROP INDEX idx_source_worktree_settlement_items_custody_phase;
         DROP INDEX idx_source_worktree_settlement_items_repository_custody_fence;
         DROP INDEX idx_source_worktree_settlement_items_session_phase;
         DROP INDEX idx_source_worktree_settlement_items_root_phase;
         DROP TRIGGER source_worktree_settlement_runs_bounds_insert;
         DROP TRIGGER source_worktree_settlement_runs_bounds_update;
         DROP TRIGGER source_worktree_settlement_items_bounds_insert;
         DROP TRIGGER source_worktree_settlement_items_bounds_update;
         DROP TRIGGER source_worktree_settlement_items_run_association_insert;
         DROP TRIGGER source_worktree_settlement_runs_forward_state;
         DROP TRIGGER source_worktree_settlement_runs_terminal;
         DROP TRIGGER source_worktree_settlement_items_forward_phase;
         DROP TRIGGER source_worktree_settlement_items_terminal;
         CREATE TRIGGER source_worktree_settlement_runs_forward_state BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN NEW.settled_count<OLD.settled_count OR NEW.refused_count<OLD.refused_count
              OR NEW.recovery_required_count<OLD.recovery_required_count
              OR NEW.unattempted_count<OLD.unattempted_count
              OR (NEW.state!=OLD.state AND NOT (
                  (OLD.state='intent_committed' AND NEW.state IN ('applying','settled','partial','recovery_required','refused')) OR
                  (OLD.state='applying' AND NEW.state IN ('settled','partial','recovery_required','refused')) OR
                  (OLD.state='partial' AND NEW.state IN ('settled','recovery_required','refused'))
              ))
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_state_regression'); END;
         CREATE TRIGGER source_worktree_settlement_runs_terminal BEFORE UPDATE ON source_worktree_settlement_runs
            WHEN OLD.state IN ('settled','recovery_required','refused') AND (
                NEW.state!=OLD.state OR NEW.settled_count!=OLD.settled_count
                OR NEW.refused_count!=OLD.refused_count
                OR NEW.recovery_required_count!=OLD.recovery_required_count
                OR NEW.unattempted_count!=OLD.unattempted_count
                OR NEW.updated_at!=OLD.updated_at OR NEW.finished_at IS NOT OLD.finished_at
                OR NEW.terminal_error IS NOT OLD.terminal_error
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_run_terminal'); END;
         CREATE TRIGGER source_worktree_settlement_items_forward_phase BEFORE UPDATE ON source_worktree_settlement_items
            WHEN NEW.phase!=OLD.phase AND NOT (
                (OLD.phase='planned' AND NEW.phase IN ('intent_committed','refused','recovery_required','unattempted')) OR
                (OLD.phase='intent_committed' AND NEW.phase IN ('worktree_removed','refused','recovery_required','unattempted')) OR
                (OLD.phase='worktree_removed' AND NEW.phase IN ('branch_removed','recovery_required')) OR
                (OLD.phase='branch_removed' AND NEW.phase IN ('settled','recovery_required'))
            )
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_phase_regression'); END;
         CREATE TRIGGER source_worktree_settlement_items_terminal BEFORE UPDATE ON source_worktree_settlement_items
            WHEN OLD.phase IN ('settled','refused','recovery_required','unattempted') AND NEW.phase!=OLD.phase
            BEGIN SELECT RAISE(ABORT,'source_worktree_settlement_item_terminal'); END;",
    )?;
    Ok(())
}

pub(crate) fn validate_v95_catalog(tx: &Transaction<'_>) -> Result<()> {
    for object in [
        "source_worktree_settlement_runs",
        "source_worktree_settlement_items",
        "source_worktree_settlement_run_order",
        "source_worktree_settlement_latest_runs",
        "idx_source_worktree_settlement_items_custody_phase",
        "idx_source_worktree_settlement_items_repository_custody_fence",
        "idx_source_worktree_settlement_items_session_phase",
        "idx_source_worktree_settlement_items_root_phase",
        "source_worktree_settlement_runs_no_delete",
        "source_worktree_settlement_runs_identity_immutable",
        "source_worktree_settlement_runs_bounds_insert",
        "source_worktree_settlement_runs_bounds_update",
        "source_worktree_settlement_runs_forward_state",
        "source_worktree_settlement_runs_terminal",
        "source_worktree_settlement_items_no_delete",
        "source_worktree_settlement_items_identity_immutable",
        "source_worktree_settlement_items_bounds_insert",
        "source_worktree_settlement_items_bounds_update",
        "source_worktree_settlement_items_run_association_insert",
        "source_worktree_settlement_items_forward_phase",
        "source_worktree_settlement_items_terminal",
        "source_worktree_settlement_run_order_no_delete",
        "source_worktree_settlement_run_order_no_update",
        "source_worktree_settlement_run_order_no_reuse_insert",
        "source_worktree_settlement_run_order_bounds_insert",
        "source_worktree_settlement_run_order_association_insert",
        "source_worktree_settlement_run_order_append_insert",
        "source_worktree_settlement_latest_runs_no_delete",
        "source_worktree_settlement_latest_runs_no_replace_insert",
        "source_worktree_settlement_latest_runs_bounds_insert",
        "source_worktree_settlement_latest_runs_bounds_update",
        "source_worktree_settlement_latest_runs_association_insert",
        "source_worktree_settlement_latest_runs_association_update",
        "source_worktree_settlement_latest_runs_forward",
        "source_worktree_settlement_latest_runs_advance",
    ] {
        let present: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
            [object],
            |row| row.get(0),
        )?;
        if !present {
            return Err(DaemonError::Store(format!(
                "V95 settlement catalog missing {object}"
            )));
        }
    }
    let fingerprint = v94_catalog_fingerprint(tx)?;
    if fingerprint != V95_SETTLEMENT_CATALOG_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V95 settlement catalog fingerprint mismatch: expected {}, got {fingerprint}",
            V95_SETTLEMENT_CATALOG_FINGERPRINT
        )));
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v95-settlement-migration

pub(crate) fn v94_catalog_fingerprint(tx: &Transaction<'_>) -> Result<String> {
    let mut statement = tx.prepare(
        "SELECT type,name,tbl_name,coalesce(sql,'') FROM sqlite_master
         WHERE type IN ('table','index','trigger')
           AND tbl_name IN (
               'source_worktree_settlement_runs',
               'source_worktree_settlement_items',
               'source_worktree_settlement_run_order',
               'source_worktree_settlement_latest_runs'
           )
         ORDER BY type,name,tbl_name,coalesce(sql,'')",
    )?;
    let mut rows = statement.query([])?;
    let mut digest = Sha256::new();
    while let Some(row) = rows.next()? {
        for index in 0..4 {
            let field: String = row.get(index)?;
            digest.update(field.as_bytes());
            if index != 3 {
                digest.update(b"\0");
            }
        }
        digest.update(b"\n");
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

#[derive(Debug, Clone)]
pub(crate) struct SourceWorktreeInventoryRow {
    pub custody_id: Uuid,
    pub canonical_repo_dir: String,
    pub sandbox_root: String,
    pub sandbox_branch: String,
    pub repository_identity: String,
    pub source_commit: String,
    pub owner_session_id: Option<Uuid>,
    pub generation: u64,
    pub validation_state: String,
    pub validated_generation: u64,
    pub reserved_effects: u64,
    pub active_effects: u64,
    pub session_id: Option<Uuid>,
    pub status: Option<String>,
    pub session_updated_at: Option<String>,
    pub session_working_dir: Option<String>,
    pub session_sandbox_kind: Option<String>,
    pub session_sandbox_root: Option<String>,
    pub session_sandbox_branch: Option<String>,
    pub session_cleanup_state: Option<String>,
    pub session_kind: Option<String>,
    pub pending_archive: bool,
    pub participant_count: u64,
    pub scheduled_dependency_count: u64,
    pub scheduled_dependency_digest: String,
    pub session_path_dependency_count: u64,
    pub session_path_dependency_digest: String,
}

#[derive(Debug, Clone)]
pub(crate) struct NewSettlementRun {
    pub run_id: Uuid,
    pub repository_identity: String,
    pub canonical_repo_dir: String,
    pub target_ref: String,
    pub target_oid: String,
    pub plan_digest: String,
    pub idempotency_key: String,
    pub authorization_digest: String,
    pub request_fingerprint: String,
    pub observed_count: u32,
    pub retained_count: u32,
    pub items: Vec<NewSettlementItem>,
}

#[derive(Debug, Clone)]
pub(crate) struct NewSettlementItem {
    pub session_id: Uuid,
    pub original_status: String,
    pub original_updated_at: String,
    pub custody_id: Uuid,
    pub custody_generation: u64,
    pub canonical_repo_dir: String,
    pub sandbox_root: String,
    pub sandbox_branch: String,
    pub repository_identity: String,
    pub source_ref: String,
    pub source_oid: String,
    pub target_oid: String,
    pub evidence_digest: String,
    pub clean_state_digest: String,
    pub reserved_effects: u64,
    pub active_effects: u64,
    pub participant_count: u64,
}

pub(crate) enum InsertSettlementRunOutcome {
    Inserted,
    Replay(SourceWorktreeSettlementRunV1),
}

#[derive(Debug, Clone)]
pub(crate) struct SourceWorktreeSettlementJournalItem {
    pub sequence: u32,
    pub session_id: Uuid,
    pub original_status: String,
    pub original_updated_at: String,
    pub custody_id: Uuid,
    pub custody_generation: u64,
    pub canonical_repo_dir: String,
    pub sandbox_root: String,
    pub sandbox_branch: String,
    pub repository_identity: String,
    pub source_ref: String,
    pub source_oid: String,
    pub target_oid: String,
    pub evidence_digest: String,
    pub clean_state_digest: String,
    pub reserved_effects: u64,
    pub active_effects: u64,
    pub participant_count: u64,
    pub phase: SourceWorktreeSettlementPhaseV1,
    pub before_observation: Option<String>,
}

fn validate_settlement_text(
    value: &str,
    field: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<()> {
    if (!allow_empty && value.is_empty()) || value.len() > max_bytes || value.contains('\0') {
        return Err(DaemonError::InvalidParam(format!(
            "settlement {field} is empty, invalid, or exceeds {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn validate_settlement_timestamp(value: &str, field: &str) -> Result<()> {
    validate_settlement_text(
        value,
        field,
        SETTLEMENT_ORIGINAL_UPDATED_AT_MAX_BYTES,
        false,
    )?;
    super::row_mappers::parse_timestamp(value).map_err(|error| {
        DaemonError::InvalidParam(format!(
            "settlement {field} is not a valid timestamp: {error}"
        ))
    })?;
    Ok(())
}

fn validate_new_settlement_run(run: &NewSettlementRun) -> Result<()> {
    if run.run_id.is_nil() {
        return Err(DaemonError::InvalidParam(
            "settlement run id must be non-nil".into(),
        ));
    }
    if run.items.is_empty() || run.items.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS {
        return Err(DaemonError::InvalidParam(format!(
            "settlement run must contain 1..={SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS} items"
        )));
    }
    let eligible_count = u32::try_from(run.items.len())
        .map_err(|_| DaemonError::InvalidParam("settlement item count exceeds u32".into()))?;
    if run.observed_count > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS as u32
        || run.retained_count > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS as u32
        || eligible_count
            .checked_add(run.retained_count)
            .is_none_or(|observed| observed != run.observed_count)
    {
        return Err(DaemonError::InvalidParam(
            "settlement run counts exceed or contradict the bounded inventory".into(),
        ));
    }
    validate_identity(&run.repository_identity).map_err(DaemonError::InvalidParam)?;
    validate_settlement_text(
        &run.canonical_repo_dir,
        "canonical repository path",
        SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES,
        false,
    )?;
    validate_settlement_text(
        &run.target_ref,
        "target ref",
        SETTLEMENT_REF_MAX_BYTES,
        false,
    )?;
    validate_target_ref(&run.target_ref).map_err(DaemonError::InvalidParam)?;
    validate_git_oid(&run.target_oid).map_err(DaemonError::InvalidParam)?;
    validate_sha256_digest(&run.plan_digest).map_err(DaemonError::InvalidParam)?;
    validate_idempotency_key(&run.idempotency_key).map_err(DaemonError::InvalidParam)?;
    validate_sha256_digest(&run.authorization_digest).map_err(DaemonError::InvalidParam)?;
    validate_sha256_digest(&run.request_fingerprint).map_err(DaemonError::InvalidParam)?;

    let mut custody_ids = HashSet::with_capacity(run.items.len());
    for item in &run.items {
        if item.session_id.is_nil() || item.custody_id.is_nil() {
            return Err(DaemonError::InvalidParam(
                "settlement item Session and custody ids must be non-nil".into(),
            ));
        }
        if !custody_ids.insert(item.custody_id) {
            return Err(DaemonError::InvalidParam(
                "settlement run contains a duplicate custody id".into(),
            ));
        }
        if item.repository_identity != run.repository_identity
            || item.canonical_repo_dir != run.canonical_repo_dir
            || item.target_oid != run.target_oid
        {
            return Err(DaemonError::InvalidParam(
                "settlement item repository authority does not match its run".into(),
            ));
        }
        validate_settlement_text(&item.original_status, "original status", 32, false)?;
        validate_settlement_timestamp(&item.original_updated_at, "original updated_at")?;
        if item.custody_generation == 0
            || item.custody_generation > i64::MAX as u64
            || item.reserved_effects > i64::MAX as u64
            || item.active_effects > i64::MAX as u64
            || item.participant_count == 0
            || item.participant_count > i64::MAX as u64
        {
            return Err(DaemonError::InvalidParam(
                "settlement item counters exceed SQLite integer bounds".into(),
            ));
        }
        for (value, field) in [
            (&item.canonical_repo_dir, "item canonical repository path"),
            (&item.sandbox_root, "item sandbox root"),
            (&item.sandbox_branch, "item sandbox branch"),
            (&item.repository_identity, "item repository identity"),
            (&item.source_ref, "item source ref"),
        ] {
            validate_settlement_text(
                value,
                field,
                SOURCE_WORKTREE_SETTLEMENT_MAX_IDENTITY_BYTES,
                false,
            )?;
        }
        validate_source_ref(&item.source_ref).map_err(DaemonError::InvalidParam)?;
        validate_git_oid(&item.source_oid).map_err(DaemonError::InvalidParam)?;
        validate_git_oid(&item.target_oid).map_err(DaemonError::InvalidParam)?;
        validate_sha256_digest(&item.evidence_digest).map_err(DaemonError::InvalidParam)?;
        validate_sha256_digest(&item.clean_state_digest).map_err(DaemonError::InvalidParam)?;
    }
    Ok(())
}

impl Store {
    /// Any source-worktree settlement history beyond a refusal or an untouched
    /// plan fences restoration: the session's worktree is already participating
    /// in source-branch settlement and cannot safely be recreated elsewhere.
    pub(crate) fn source_worktree_settlement_blocks_unarchive(
        &self,
        session_id: Uuid,
    ) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM source_worktree_settlement_items i
                    WHERE i.phase NOT IN ('refused','unattempted')
                      AND (
                          i.session_id=?1 OR i.custody_id IN (
                              SELECT sandbox_custody_id FROM sessions
                              WHERE id=?1 AND sandbox_custody_id IS NOT NULL
                              UNION
                              SELECT custody_id FROM session_execution_projections
                              WHERE session_id=?1 AND custody_id IS NOT NULL
                          )
                      )
                )",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn session_or_custody_has_settlement_fence(&self, session_id: Uuid) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions
                               WHERE id=?1 AND sandbox_cleanup_state='Purged')
                    OR EXISTS(
                    SELECT 1 FROM source_worktree_settlement_items i
                    WHERE i.phase NOT IN ('refused','unattempted')
                      AND (i.session_id=?1 OR i.custody_id IN (
                          SELECT sandbox_custody_id FROM sessions
                              WHERE id=?1 AND sandbox_custody_id IS NOT NULL
                          UNION
                          SELECT custody_id FROM session_execution_projections
                              WHERE session_id=?1 AND custody_id IS NOT NULL
                      ))
                 )",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Resolve every durable Session identity whose provider may still be
    /// using a root protected by retained settlement history.  The result is
    /// consumed by one bounded fixed-point `/proc` scan before startup
    /// recovery performs any Git effect.
    ///
    /// Both queries are keyset-paged and every retained field is byte-bounded.
    /// Any malformed row or bound breach fails closed so startup can skip all
    /// settlement recovery for that boot while leaving the journal/root intact.
    pub(crate) fn source_worktree_startup_orphan_candidates(
        &self,
        sandbox_base: &Path,
    ) -> Result<Vec<Uuid>> {
        let canonical_base = std::fs::canonicalize(sandbox_base).ok();
        let mut raw_roots = HashSet::<PathBuf>::new();
        let mut canonical_roots = HashSet::<PathBuf>::new();
        let mut candidates = HashSet::<Uuid>::new();
        let mut contributing_runs = HashSet::<Uuid>::new();
        let mut after_run_id = String::new();
        let mut after_sequence = -1_i64;
        let mut journal_records_scanned = 0_usize;

        loop {
            let mut statement = self.conn.prepare(
                "SELECT CASE WHEN typeof(run_id)='text' AND octet_length(run_id)<=128
                                   THEN run_id END,
                        CASE WHEN typeof(sequence)='integer' THEN sequence END,
                        CASE WHEN typeof(session_id)='text' AND octet_length(session_id)<=128
                                   THEN session_id END,
                        CASE WHEN typeof(sandbox_root)='text' AND octet_length(sandbox_root)<=?3
                                   THEN sandbox_root END,
                        typeof(run_id)!='text' OR octet_length(run_id)>128
                          OR typeof(sequence)!='integer'
                          OR typeof(session_id)!='text' OR octet_length(session_id)>128
                          OR typeof(sandbox_root)!='text' OR octet_length(sandbox_root)>?3
                   FROM source_worktree_settlement_items
                  WHERE phase IN ('intent_committed','worktree_removed','branch_removed')
                    AND (run_id>?1 OR (run_id=?1 AND sequence>?2))
                  ORDER BY run_id,sequence
                  LIMIT ?4",
            )?;
            let mut rows = statement.query(params![
                after_run_id,
                after_sequence,
                STARTUP_SETTLEMENT_PATH_MAX_BYTES as i64,
                STARTUP_SETTLEMENT_SCAN_PAGE as i64,
            ])?;
            let mut page_count = 0_usize;
            while let Some(row) = rows.next()? {
                page_count = page_count.checked_add(1).ok_or_else(|| {
                    DaemonError::Store("startup settlement page count overflowed".into())
                })?;
                journal_records_scanned =
                    journal_records_scanned.checked_add(1).ok_or_else(|| {
                        DaemonError::Store("startup settlement scan count overflowed".into())
                    })?;
                if journal_records_scanned > SETTLEMENT_DEPENDENCY_SCAN_MAX_RECORDS {
                    return Err(DaemonError::Store(
                        "startup settlement journal scan bound exceeded".into(),
                    ));
                }
                if row.get::<_, bool>(4)? {
                    return Err(DaemonError::Store(
                        "startup settlement item has an invalid storage class or exceeds its byte bound".into(),
                    ));
                }
                let run_id = row.get::<_, Option<String>>(0)?.ok_or_else(|| {
                    DaemonError::Store("startup settlement run_id is not bounded text".into())
                })?;
                let sequence = row.get::<_, Option<i64>>(1)?.ok_or_else(|| {
                    DaemonError::Store("startup settlement sequence is not an integer".into())
                })?;
                if sequence < 0 {
                    return Err(DaemonError::Store(
                        "startup settlement item has a negative sequence".into(),
                    ));
                }
                let parsed_run_id = parse_canonical_uuid(&run_id, "startup settlement run_id")?;
                contributing_runs.insert(parsed_run_id);
                let session_id = row
                    .get::<_, Option<String>>(2)?
                    .ok_or_else(|| {
                        DaemonError::Store(
                            "startup settlement session_id exceeds byte bound".into(),
                        )
                    })
                    .and_then(|value| {
                        parse_canonical_uuid(&value, "startup settlement session_id")
                    })?;
                let sandbox_root = row.get::<_, Option<String>>(3)?.ok_or_else(|| {
                    DaemonError::Store("startup settlement sandbox_root exceeds byte bound".into())
                })?;
                let root = PathBuf::from(sandbox_root);
                validate_startup_settlement_root(
                    &root,
                    sandbox_base,
                    canonical_base.as_deref(),
                    session_id,
                )?;
                let quarantine_root =
                    source_worktree_quarantine_path(&root, parsed_run_id, session_id)?;
                let canonical_quarantine_root = validate_startup_quarantine_root(
                    &quarantine_root,
                    sandbox_base,
                    canonical_base.as_deref(),
                    parsed_run_id,
                    session_id,
                )?;
                raw_roots.insert(root.clone());
                raw_roots.insert(quarantine_root);
                if let Ok(canonical_root) = std::fs::canonicalize(&root) {
                    canonical_roots.insert(canonical_root);
                }
                if let Some(canonical_quarantine_root) = canonical_quarantine_root {
                    canonical_roots.insert(canonical_quarantine_root);
                }
                candidates.insert(session_id);
                if raw_roots.len() > STARTUP_SETTLEMENT_ROOT_MAX
                    || candidates.len() > STARTUP_SETTLEMENT_CANDIDATE_MAX
                {
                    return Err(DaemonError::Store(
                        "startup settlement orphan candidate bound exceeded".into(),
                    ));
                }
                after_run_id = run_id;
                after_sequence = sequence;
            }
            if page_count < STARTUP_SETTLEMENT_SCAN_PAGE {
                break;
            }
        }

        let mut contributing_runs = contributing_runs.into_iter().collect::<Vec<_>>();
        contributing_runs.sort_unstable();
        for run_id in contributing_runs {
            let receipt = load_run_on(&self.conn, run_id)?.ok_or_else(|| {
                DaemonError::Store("startup settlement receipt disappeared".into())
            })?;
            receipt.validate_wire().map_err(DaemonError::Store)?;
        }

        if raw_roots.is_empty() {
            return Ok(Vec::new());
        }

        let mut after_session_id: Option<String> = None;
        let mut session_records_scanned = 0_usize;
        loop {
            let mut statement = self.conn.prepare(
                "SELECT CASE WHEN typeof(s.id)='text' AND octet_length(s.id)<=128 THEN s.id END,
                        CASE WHEN typeof(s.working_dir)='text' AND octet_length(s.working_dir)<=?2
                             THEN s.working_dir END,
                        CASE WHEN s.sandbox_root IS NULL OR
                                       (typeof(s.sandbox_root)='text' AND octet_length(s.sandbox_root)<=?2)
                             THEN s.sandbox_root END,
                        CASE WHEN p.effective_cwd IS NULL OR
                                       (typeof(p.effective_cwd)='text' AND octet_length(p.effective_cwd)<=?2)
                             THEN p.effective_cwd END,
                        typeof(s.id)!='text' OR octet_length(s.id)>128
                          OR typeof(s.working_dir)!='text' OR octet_length(s.working_dir)>?2
                          OR (s.sandbox_root IS NOT NULL AND
                              (typeof(s.sandbox_root)!='text' OR octet_length(s.sandbox_root)>?2))
                          OR (p.effective_cwd IS NOT NULL AND
                              (typeof(p.effective_cwd)!='text' OR octet_length(p.effective_cwd)>?2))
                   FROM sessions s
                   LEFT JOIN session_execution_projections p ON p.session_id=s.id
                  WHERE (?1 IS NULL OR s.id>?1)
                  ORDER BY s.id
                  LIMIT ?3",
            )?;
            let mut rows = statement.query(params![
                after_session_id.as_deref(),
                STARTUP_SETTLEMENT_PATH_MAX_BYTES as i64,
                STARTUP_SETTLEMENT_SCAN_PAGE as i64,
            ])?;
            let mut page_count = 0_usize;
            while let Some(row) = rows.next()? {
                page_count = page_count.checked_add(1).ok_or_else(|| {
                    DaemonError::Store("startup Session page count overflowed".into())
                })?;
                session_records_scanned =
                    session_records_scanned.checked_add(1).ok_or_else(|| {
                        DaemonError::Store("startup Session scan count overflowed".into())
                    })?;
                if session_records_scanned > SETTLEMENT_DEPENDENCY_SCAN_MAX_RECORDS {
                    return Err(DaemonError::Store(
                        "startup Session path scan bound exceeded".into(),
                    ));
                }
                let session_id_text = row.get::<_, Option<String>>(0)?.ok_or_else(|| {
                    DaemonError::Store("startup Session id exceeds byte bound".into())
                })?;
                let session_id = parse_canonical_uuid(&session_id_text, "startup Session id")?;
                if row.get::<_, bool>(4)? {
                    return Err(DaemonError::Store(
                        "startup Session path exceeds byte bound".into(),
                    ));
                }
                let paths = [
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ];
                if paths.iter().flatten().any(|value| {
                    startup_path_matches_settlement_root(
                        Path::new(value),
                        &raw_roots,
                        &canonical_roots,
                    )
                }) {
                    candidates.insert(session_id);
                    if candidates.len() > STARTUP_SETTLEMENT_CANDIDATE_MAX {
                        return Err(DaemonError::Store(
                            "startup settlement orphan candidate bound exceeded".into(),
                        ));
                    }
                }
                after_session_id = Some(session_id_text);
            }
            if page_count < STARTUP_SETTLEMENT_SCAN_PAGE {
                break;
            }
        }

        let mut candidates = candidates.into_iter().collect::<Vec<_>>();
        candidates.sort_unstable();
        Ok(candidates)
    }

    pub(crate) fn list_source_worktree_cohorts(
        &self,
    ) -> Result<Vec<SourceWorktreeCohortSummaryV1>> {
        let mut statement = self.conn.prepare(
            "SELECT CASE WHEN typeof(r.repository_identity)='text' AND octet_length(r.repository_identity)<=4096
                         THEN r.repository_identity END,
                    CASE WHEN typeof(r.canonical_repo_dir)='text' AND octet_length(r.canonical_repo_dir)<=4096
                         THEN r.canonical_repo_dir END,
                    count(*),
                    sum(CASE WHEN s.status IN ('Completed','Failed','Interrupted','Archived') THEN 1 ELSE 0 END),
                    typeof(r.repository_identity)!='text' OR octet_length(r.repository_identity)>4096
                      OR typeof(r.canonical_repo_dir)!='text' OR octet_length(r.canonical_repo_dir)>4096
             FROM sandbox_custody_roots r
             LEFT JOIN sessions s ON s.id=r.owner_session_id AND s.sandbox_custody_id=r.custody_id
             WHERE r.state='live'
             GROUP BY r.repository_identity,r.canonical_repo_dir
             ORDER BY r.repository_identity,r.canonical_repo_dir
             LIMIT ?1",
        )?;
        let rows = statement.query_map(
            [SOURCE_WORKTREE_COHORT_LIST_MAX.saturating_add(1) as i64],
            |row| {
                if row.get::<_, bool>(4)? {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::other(
                            "source-worktree cohort summary field exceeds byte bound",
                        )),
                    ));
                }
                Ok(SourceWorktreeCohortSummaryV1 {
                    schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                    policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                    repository_identity: row.get(0)?,
                    canonical_repo_dir: row.get(1)?,
                    live_roots: row_u32(row, 2)?,
                    terminal_roots: row_u32(row, 3)?,
                })
            },
        )?;
        let mut summaries = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        if summaries.len() > SOURCE_WORKTREE_COHORT_LIST_MAX {
            return Err(DaemonError::Store(format!(
                "source-worktree cohort list exceeds bounded maximum of {SOURCE_WORKTREE_COHORT_LIST_MAX}"
            )));
        }
        let historical_limit = SOURCE_WORKTREE_COHORT_LIST_MAX
            .saturating_sub(summaries.len())
            .saturating_add(1);
        let mut historical_statement = self.conn.prepare(
            "SELECT CASE WHEN typeof(latest.repository_identity)='text'
                                  AND octet_length(latest.repository_identity)<=4096
                                 THEN latest.repository_identity END,
                    CASE WHEN typeof(latest.canonical_repo_dir)='text'
                                  AND octet_length(latest.canonical_repo_dir)<=4096
                                 THEN latest.canonical_repo_dir END,
                    typeof(latest.repository_identity)!='text'
                      OR octet_length(latest.repository_identity) NOT BETWEEN 1 AND 4096
                      OR typeof(latest.canonical_repo_dir)!='text'
                      OR octet_length(latest.canonical_repo_dir) NOT BETWEEN 1 AND 4096
                      OR typeof(latest.run_id)!='text' OR octet_length(latest.run_id)>128
                      OR typeof(latest.generation)!='integer' OR latest.generation<=0
                      OR r.run_id IS NULL
                      OR r.repository_identity IS NOT latest.repository_identity
                      OR r.canonical_repo_dir IS NOT latest.canonical_repo_dir
                      OR ordered.run_id IS NULL
                      OR EXISTS (
                          SELECT 1 FROM source_worktree_settlement_run_order newer
                           WHERE newer.repository_identity=latest.repository_identity
                             AND newer.generation>latest.generation
                      )
               FROM source_worktree_settlement_latest_runs latest
               LEFT JOIN source_worktree_settlement_runs r ON r.run_id=latest.run_id
               LEFT JOIN source_worktree_settlement_run_order ordered
                 ON ordered.repository_identity=latest.repository_identity
                AND ordered.canonical_repo_dir=latest.canonical_repo_dir
                AND ordered.run_id=latest.run_id
                AND ordered.generation=latest.generation
              WHERE NOT EXISTS (
                    SELECT 1 FROM sandbox_custody_roots live
                     WHERE live.state='live'
                       AND live.repository_identity=latest.repository_identity
              )
              ORDER BY latest.repository_identity
              LIMIT ?1",
        )?;
        let historical = historical_statement.query_map([historical_limit as i64], |row| {
            if row.get::<_, bool>(2)? {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(
                        "historical source-worktree cohort summary field exceeds byte bound",
                    )),
                ));
            }
            Ok(SourceWorktreeCohortSummaryV1 {
                schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                repository_identity: row.get(0)?,
                canonical_repo_dir: row.get(1)?,
                live_roots: 0,
                terminal_roots: 0,
            })
        })?;
        summaries.extend(historical.collect::<std::result::Result<Vec<_>, _>>()?);
        if summaries.len() > SOURCE_WORKTREE_COHORT_LIST_MAX {
            return Err(DaemonError::Store(format!(
                "source-worktree cohort list exceeds bounded maximum of {SOURCE_WORKTREE_COHORT_LIST_MAX}"
            )));
        }
        summaries.sort_by(|left, right| {
            left.repository_identity
                .cmp(&right.repository_identity)
                .then_with(|| left.canonical_repo_dir.cmp(&right.canonical_repo_dir))
        });
        Ok(summaries)
    }

    pub(crate) fn source_worktree_inventory(
        &self,
        repository_identity: &str,
        limit: usize,
    ) -> Result<Vec<SourceWorktreeInventoryRow>> {
        self.source_worktree_inventory_with_path_aliases(repository_identity, limit, &[])
    }

    pub(crate) fn source_worktree_inventory_with_path_aliases(
        &self,
        repository_identity: &str,
        limit: usize,
        aliases: &[SourceWorktreeInventoryQuarantineAlias],
    ) -> Result<Vec<SourceWorktreeInventoryRow>> {
        validate_identity(repository_identity).map_err(DaemonError::InvalidParam)?;
        if aliases.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS {
            return Err(DaemonError::Store(format!(
                "source-worktree path aliases exceed {SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS}"
            )));
        }
        let mut statement = self.conn.prepare(
            "SELECT CASE WHEN typeof(r.custody_id)='text' AND octet_length(r.custody_id)<=128 THEN r.custody_id END,
                    CASE WHEN typeof(r.canonical_repo_dir)='text' AND octet_length(r.canonical_repo_dir)<=4096 THEN r.canonical_repo_dir END,
                    CASE WHEN typeof(r.sandbox_root)='text' AND octet_length(r.sandbox_root)<=4096 THEN r.sandbox_root END,
                    CASE WHEN typeof(r.sandbox_branch)='text' AND octet_length(r.sandbox_branch)<=4096 THEN r.sandbox_branch END,
                    CASE WHEN typeof(r.repository_identity)='text' AND octet_length(r.repository_identity)<=4096 THEN r.repository_identity END,
                    CASE WHEN typeof(r.source_commit)='text' AND octet_length(r.source_commit)<=128 THEN r.source_commit END,
                    CASE WHEN r.owner_session_id IS NULL OR
                                   (typeof(r.owner_session_id)='text' AND octet_length(r.owner_session_id)<=128)
                         THEN r.owner_session_id END,
                    CASE WHEN typeof(r.generation)='integer' THEN r.generation END,
                    CASE WHEN typeof(r.validation_state)='text' AND octet_length(r.validation_state)<=64 THEN r.validation_state END,
                    CASE WHEN r.validated_generation IS NULL OR typeof(r.validated_generation)='integer'
                         THEN r.validated_generation END,
                    CASE WHEN typeof(r.reserved_effects)='integer' THEN r.reserved_effects END,
                    CASE WHEN typeof(r.active_effects)='integer' THEN r.active_effects END,
                    CASE WHEN s.id IS NULL OR (typeof(s.id)='text' AND octet_length(s.id)<=128) THEN s.id END,
                    CASE WHEN s.status IS NULL OR (typeof(s.status)='text' AND octet_length(s.status)<=64) THEN s.status END,
                    CASE WHEN s.updated_at IS NULL OR (typeof(s.updated_at)='text' AND octet_length(s.updated_at)<=64) THEN s.updated_at END,
                    CASE WHEN s.working_dir IS NULL OR (typeof(s.working_dir)='text' AND octet_length(s.working_dir)<=4096) THEN s.working_dir END,
                    CASE WHEN s.sandbox_kind IS NULL OR (typeof(s.sandbox_kind)='text' AND octet_length(s.sandbox_kind)<=64) THEN s.sandbox_kind END,
                    CASE WHEN s.sandbox_root IS NULL OR (typeof(s.sandbox_root)='text' AND octet_length(s.sandbox_root)<=4096) THEN s.sandbox_root END,
                    CASE WHEN s.sandbox_branch IS NULL OR (typeof(s.sandbox_branch)='text' AND octet_length(s.sandbox_branch)<=4096) THEN s.sandbox_branch END,
                    CASE WHEN s.sandbox_cleanup_state IS NULL OR (typeof(s.sandbox_cleanup_state)='text' AND octet_length(s.sandbox_cleanup_state)<=64) THEN s.sandbox_cleanup_state END,
                    CASE WHEN s.session_kind IS NULL OR (typeof(s.session_kind)='text' AND octet_length(s.session_kind)<=64) THEN s.session_kind END,
                    CASE WHEN s.pending_archive IS NULL OR typeof(s.pending_archive)='integer' THEN s.pending_archive END,
                    (SELECT count(*) FROM sessions linked WHERE linked.sandbox_custody_id=r.custody_id),
                    typeof(r.custody_id)!='text' OR octet_length(r.custody_id)>128
                      OR typeof(r.canonical_repo_dir)!='text' OR octet_length(r.canonical_repo_dir)>4096
                      OR typeof(r.sandbox_root)!='text' OR octet_length(r.sandbox_root)>4096
                      OR typeof(r.sandbox_branch)!='text' OR octet_length(r.sandbox_branch)>4096
                      OR typeof(r.repository_identity)!='text' OR octet_length(r.repository_identity)>4096
                      OR typeof(r.source_commit)!='text' OR octet_length(r.source_commit)>128
                      OR (r.owner_session_id IS NOT NULL AND
                          (typeof(r.owner_session_id)!='text' OR octet_length(r.owner_session_id)>128))
                      OR typeof(r.validation_state)!='text' OR octet_length(r.validation_state)>64
                      OR (s.id IS NOT NULL AND (typeof(s.id)!='text' OR octet_length(s.id)>128))
                      OR (s.status IS NOT NULL AND (typeof(s.status)!='text' OR octet_length(s.status)>64))
                      OR (s.updated_at IS NOT NULL AND (typeof(s.updated_at)!='text' OR octet_length(s.updated_at)>64))
                      OR (s.working_dir IS NOT NULL AND (typeof(s.working_dir)!='text' OR octet_length(s.working_dir)>4096))
                      OR (s.sandbox_kind IS NOT NULL AND (typeof(s.sandbox_kind)!='text' OR octet_length(s.sandbox_kind)>64))
                      OR (s.sandbox_root IS NOT NULL AND (typeof(s.sandbox_root)!='text' OR octet_length(s.sandbox_root)>4096))
                      OR (s.sandbox_branch IS NOT NULL AND (typeof(s.sandbox_branch)!='text' OR octet_length(s.sandbox_branch)>4096))
                      OR (s.sandbox_cleanup_state IS NOT NULL AND (typeof(s.sandbox_cleanup_state)!='text' OR octet_length(s.sandbox_cleanup_state)>64))
                      OR (s.session_kind IS NOT NULL AND (typeof(s.session_kind)!='text' OR octet_length(s.session_kind)>64))
                      OR (s.pending_archive IS NOT NULL AND typeof(s.pending_archive)!='integer')
                      OR typeof(r.generation)!='integer'
                      OR (r.validated_generation IS NOT NULL AND typeof(r.validated_generation)!='integer')
                      OR typeof(r.reserved_effects)!='integer'
                      OR typeof(r.active_effects)!='integer'
             FROM sandbox_custody_roots r
             LEFT JOIN sessions s ON s.id=r.owner_session_id AND s.sandbox_custody_id=r.custody_id
             WHERE r.state='live' AND r.repository_identity=?1
             ORDER BY COALESCE(s.id,r.custody_id),r.custody_id
             LIMIT ?2",
        )?;
        let bounded_limit = limit.min(SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1));
        let bounded_limit = i64::try_from(bounded_limit)
            .map_err(|_| DaemonError::InvalidParam("inventory limit exceeds i64".into()))?;
        let rows = statement.query_map(
            params![repository_identity, bounded_limit],
            parse_inventory_row,
        )?;
        let mut inventory = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);

        let mut validated_aliases = Vec::with_capacity(aliases.len());
        let mut alias_identities = HashSet::with_capacity(aliases.len());
        for alias in aliases {
            if alias.run_id.is_nil() || alias.session_id.is_nil() || alias.custody_id.is_nil() {
                return Err(DaemonError::Store(
                    "source-worktree path alias ids must be non-nil".into(),
                ));
            }
            if alias.custody_generation == 0 {
                return Err(DaemonError::Store(
                    "source-worktree path alias custody generation must be positive".into(),
                ));
            }
            if !alias_identities.insert((alias.run_id, alias.session_id, alias.custody_id)) {
                return Err(DaemonError::Store(
                    "duplicate source-worktree path alias identity".into(),
                ));
            }
            let original = alias.original_path.to_str().ok_or_else(|| {
                DaemonError::Store("source-worktree path alias is not UTF-8".into())
            })?;
            validate_settlement_text(
                original,
                "path alias original root",
                STARTUP_SETTLEMENT_PATH_MAX_BYTES,
                false,
            )?;
            if !alias.original_path.is_absolute() {
                return Err(DaemonError::Store(
                    "source-worktree path alias original root is not absolute".into(),
                ));
            }
            let quarantine_path = source_worktree_quarantine_path(
                &alias.original_path,
                alias.run_id,
                alias.session_id,
            )?;
            let journal: Option<(
                Option<String>,
                Option<i64>,
                Option<String>,
                Option<String>,
                Option<String>,
            )> =
                self.conn
                    .query_row(
                        "SELECT CASE WHEN typeof(custody_id)='text' AND octet_length(custody_id)<=128
                                          THEN custody_id END,
                                CASE WHEN typeof(custody_generation)='integer' THEN custody_generation END,
                                CASE WHEN typeof(sandbox_root)='text' AND octet_length(sandbox_root)<=4096
                                          THEN sandbox_root END,
                                CASE WHEN typeof(repository_identity)='text' AND octet_length(repository_identity)<=4096
                                          THEN repository_identity END,
                                CASE WHEN typeof(phase)='text' AND octet_length(phase)<=64
                                          THEN phase END,
                                typeof(custody_id)!='text' OR octet_length(custody_id)>128
                                  OR typeof(custody_generation)!='integer'
                                  OR typeof(sandbox_root)!='text' OR octet_length(sandbox_root)>4096
                                  OR typeof(repository_identity)!='text' OR octet_length(repository_identity)>4096
                                  OR typeof(phase)!='text' OR octet_length(phase)>64
                           FROM source_worktree_settlement_items
                          WHERE run_id=?1 AND session_id=?2",
                        params![alias.run_id.to_string(), alias.session_id.to_string()],
                        |row| {
                            if row.get::<_, bool>(5)? {
                                return Err(rusqlite::Error::FromSqlConversionFailure(
                                    5,
                                    rusqlite::types::Type::Text,
                                    Box::new(std::io::Error::other(
                                        "source-worktree path alias journal row exceeds bounds",
                                    )),
                                ));
                            }
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            ))
                        },
                    )
                    .optional()?;
            let Some((custody_id, custody_generation, sandbox_root, journal_repository, phase)) =
                journal
            else {
                return Err(DaemonError::Store(
                    "source-worktree path alias has no journal identity".into(),
                ));
            };
            let custody_id = custody_id.ok_or_else(|| {
                DaemonError::Store("source-worktree path alias custody is malformed".into())
            })?;
            let journal_custody =
                parse_canonical_uuid(&custody_id, "source-worktree path alias custody")?;
            let journal_generation = custody_generation
                .and_then(|value| u64::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    DaemonError::Store(
                        "source-worktree path alias custody generation is malformed".into(),
                    )
                })?;
            if journal_custody != alias.custody_id
                || journal_generation != alias.custody_generation
                || sandbox_root.as_deref() != Some(original)
                || journal_repository.as_deref() != Some(repository_identity)
                || !matches!(
                    phase.as_deref(),
                    Some("intent_committed" | "worktree_removed" | "branch_removed")
                )
            {
                return Err(DaemonError::Store(
                    "source-worktree path alias does not match an effect-capable journal row"
                        .into(),
                ));
            }
            let matching = inventory
                .iter()
                .enumerate()
                .filter(|(_, row)| {
                    row.custody_id == alias.custody_id
                        && row.generation == alias.custody_generation
                        && row.session_id == Some(alias.session_id)
                        && row.sandbox_root == original
                        && row.repository_identity == repository_identity
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if matching.len() != 1 {
                return Err(DaemonError::Store(
                    "source-worktree path alias does not match exactly one live inventory row"
                        .into(),
                ));
            }
            validated_aliases.push((matching[0], alias.original_path.clone(), quarantine_path));
        }

        let mut owner_indexes = HashMap::<String, Vec<usize>>::new();
        let mut custody_indexes = HashMap::<String, Vec<usize>>::new();
        let mut raw_root_indexes = HashMap::<std::path::PathBuf, Vec<usize>>::new();
        let mut canonical_root_indexes = HashMap::<std::path::PathBuf, Vec<usize>>::new();
        for (index, row) in inventory.iter().enumerate() {
            if let Some(owner) = row.owner_session_id {
                owner_indexes
                    .entry(owner.to_string())
                    .or_default()
                    .push(index);
            }
            custody_indexes
                .entry(row.custody_id.to_string())
                .or_default()
                .push(index);
            let raw_root = std::path::PathBuf::from(&row.sandbox_root);
            raw_root_indexes
                .entry(raw_root.clone())
                .or_default()
                .push(index);
            let canonical_root = std::fs::canonicalize(&raw_root).ok();
            if let Some(root) = canonical_root.as_ref() {
                canonical_root_indexes
                    .entry(root.clone())
                    .or_default()
                    .push(index);
            }
        }
        let mut raw_alias_owners = HashMap::<PathBuf, usize>::new();
        let mut canonical_alias_owners = HashMap::<PathBuf, usize>::new();
        for (index, original_path, quarantine_path) in validated_aliases {
            for alias_path in [original_path, quarantine_path] {
                if raw_alias_owners
                    .insert(alias_path.clone(), index)
                    .is_some_and(|previous| previous != index)
                {
                    return Err(DaemonError::Store(
                        "source-worktree raw path alias maps to multiple custody rows".into(),
                    ));
                }
                if raw_root_indexes
                    .get(&alias_path)
                    .is_some_and(|indexes| indexes.iter().any(|candidate| *candidate != index))
                {
                    return Err(DaemonError::Store(
                        "source-worktree raw path alias collides with another custody row".into(),
                    ));
                }
                raw_root_indexes
                    .entry(alias_path.clone())
                    .or_default()
                    .push(index);
                if let Ok(canonical) = std::fs::canonicalize(&alias_path) {
                    if canonical_alias_owners
                        .insert(canonical.clone(), index)
                        .is_some_and(|previous| previous != index)
                    {
                        return Err(DaemonError::Store(
                            "source-worktree canonical path alias maps to multiple custody rows"
                                .into(),
                        ));
                    }
                    if canonical_root_indexes
                        .get(&canonical)
                        .is_some_and(|indexes| indexes.iter().any(|candidate| *candidate != index))
                    {
                        return Err(DaemonError::Store(
                            "source-worktree canonical path alias collides with another custody row"
                                .into(),
                        ));
                    }
                    canonical_root_indexes
                        .entry(canonical)
                        .or_default()
                        .push(index);
                }
            }
        }
        let mut counts = vec![0_u64; inventory.len()];
        let mut digests = (0..inventory.len())
            .map(|_| {
                let mut digest = Sha256::new();
                digest.update(b"rsi-scheduled-dependencies-v1\0");
                digest
            })
            .collect::<Vec<_>>();
        let mut dependency_scan_overflow = false;
        let mut dependency_records_scanned = 0_usize;
        let mut after_job_id: Option<String> = None;
        loop {
            let mut dependencies = self.conn.prepare(
                "SELECT
                    CASE WHEN typeof(id)='text' AND octet_length(id)<=128 THEN id END,
                    CASE WHEN typeof(wake_mode)='text' AND octet_length(wake_mode)<=4096 THEN wake_mode END,
                    CASE WHEN wake_session_id IS NULL OR
                                   (typeof(wake_session_id)='text' AND octet_length(wake_session_id)<=128)
                         THEN wake_session_id END,
                    CASE WHEN working_dir IS NULL OR
                                   (typeof(working_dir)='text' AND octet_length(working_dir)<=4096)
                         THEN working_dir END,
                    typeof(enabled)!='integer'
                      OR typeof(id)!='text' OR octet_length(id)>128
                      OR typeof(wake_mode)!='text' OR octet_length(wake_mode)>4096
                      OR (wake_session_id IS NOT NULL AND
                          (typeof(wake_session_id)!='text' OR octet_length(wake_session_id)>128))
                      OR (working_dir IS NOT NULL AND
                          (typeof(working_dir)!='text' OR octet_length(working_dir)>4096))
                 FROM scheduled_jobs
                 WHERE (enabled=1 OR typeof(enabled)!='integer')
                   AND (?1 IS NULL OR id>?1)
                 ORDER BY id LIMIT ?2",
            )?;
            let mut records = dependencies.query(params![
                after_job_id.as_deref(),
                i64::try_from(SETTLEMENT_DEPENDENCY_SCAN_PAGE + 1).map_err(|_| {
                    DaemonError::Store("scheduled dependency page bound exceeds i64".into())
                })?,
            ])?;
            let mut page_count = 0_usize;
            let mut has_more = false;
            while let Some(dependency) = records.next()? {
                page_count = page_count.checked_add(1).ok_or_else(|| {
                    DaemonError::Store("scheduled dependency page count overflowed".into())
                })?;
                if page_count > SETTLEMENT_DEPENDENCY_SCAN_PAGE {
                    has_more = true;
                    break;
                }
                dependency_records_scanned =
                    dependency_records_scanned.checked_add(1).ok_or_else(|| {
                        DaemonError::Store("scheduled dependency scan count overflowed".into())
                    })?;
                if dependency.get::<_, bool>(4)? {
                    dependency_scan_overflow = true;
                    break;
                }
                let fields = (0..4)
                    .map(|column| dependency.get::<_, Option<String>>(column))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let Some(job_id) = fields[0].as_ref() else {
                    dependency_scan_overflow = true;
                    break;
                };
                if parse_canonical_uuid(job_id, "scheduled dependency job id").is_err() {
                    dependency_scan_overflow = true;
                    break;
                }
                after_job_id = Some(job_id.clone());
                let mut matched: Vec<usize> = Vec::new();
                if let Some(wake_session_id) = fields[2].as_deref() {
                    if parse_canonical_uuid(wake_session_id, "scheduled dependency wake owner")
                        .is_err()
                    {
                        dependency_scan_overflow = true;
                        break;
                    }
                    if let Some(indexes) = owner_indexes.get(wake_session_id) {
                        matched.extend(indexes.iter().copied());
                    }
                }
                if let Some(watched) = fields[1]
                    .as_deref()
                    .and_then(|mode| mode.strip_prefix("on_terminal:"))
                {
                    if parse_canonical_uuid(watched, "scheduled dependency watched owner").is_err()
                    {
                        dependency_scan_overflow = true;
                        break;
                    }
                    if let Some(indexes) = owner_indexes.get(watched) {
                        matched.extend(indexes.iter().copied());
                    }
                }
                if let Some(working_dir) = fields[3].as_deref() {
                    let working_dir = std::path::Path::new(working_dir);
                    let canonical_working_dir = std::fs::canonicalize(working_dir).ok();
                    extend_precomputed_root_matches(
                        working_dir,
                        canonical_working_dir.as_deref(),
                        &raw_root_indexes,
                        &canonical_root_indexes,
                        &mut matched,
                    );
                }
                matched.sort_unstable();
                matched.dedup();
                for index in matched {
                    counts[index] = counts[index].checked_add(1).ok_or_else(|| {
                        DaemonError::Store("scheduled dependency count overflowed u64".into())
                    })?;
                    for field in &fields {
                        match field {
                            Some(field) => {
                                digests[index].update([1]);
                                digests[index].update(
                                    u64::try_from(field.len())
                                        .map_err(|_| {
                                            DaemonError::Store(
                                                "scheduled dependency field length exceeds u64"
                                                    .into(),
                                            )
                                        })?
                                        .to_be_bytes(),
                                );
                                digests[index].update(field.as_bytes());
                            }
                            None => digests[index].update([0]),
                        }
                    }
                }
            }
            if dependency_scan_overflow {
                break;
            }
            if has_more && dependency_records_scanned >= SETTLEMENT_DEPENDENCY_SCAN_MAX_RECORDS {
                dependency_scan_overflow = true;
                break;
            }
            if !has_more {
                break;
            }
        }
        if dependency_scan_overflow {
            for (count, digest) in counts.iter_mut().zip(&mut digests) {
                *count = u64::MAX;
                *digest = Sha256::new();
                digest.update(b"rsi-scheduled-dependencies-scan-bound-exceeded-v1\0");
            }
        }
        for (index, row) in inventory.iter_mut().enumerate() {
            row.scheduled_dependency_count = counts[index];
            row.scheduled_dependency_digest = format!(
                "sha256:{:x}",
                std::mem::take(&mut digests[index]).finalize()
            );
        }

        let mut session_counts = vec![0_u64; inventory.len()];
        let mut session_digests = (0..inventory.len())
            .map(|_| {
                let mut digest = Sha256::new();
                digest.update(b"rsi-session-path-dependencies-v1\0");
                digest
            })
            .collect::<Vec<_>>();
        let mut session_scan_overflow = false;
        let mut session_records_scanned = 0_usize;
        let mut after_session_id: Option<String> = None;
        loop {
            let mut sessions = self.conn.prepare(
                "SELECT
                    CASE WHEN typeof(s.id)='text' AND octet_length(s.id)<=128 THEN s.id END,
                    CASE WHEN typeof(s.status)='text' AND octet_length(s.status)<=64 THEN s.status END,
                    CASE WHEN typeof(s.working_dir)='text' AND octet_length(s.working_dir)<=4096 THEN s.working_dir END,
                    CASE WHEN s.sandbox_root IS NULL OR
                                   (typeof(s.sandbox_root)='text' AND octet_length(s.sandbox_root)<=4096)
                         THEN s.sandbox_root END,
                    CASE WHEN p.effective_cwd IS NULL OR
                                   (typeof(p.effective_cwd)='text' AND octet_length(p.effective_cwd)<=4096)
                         THEN p.effective_cwd END,
                    CASE WHEN s.sandbox_cleanup_state IS NULL OR
                                   (typeof(s.sandbox_cleanup_state)='text' AND octet_length(s.sandbox_cleanup_state)<=64)
                         THEN s.sandbox_cleanup_state END,
                    CASE WHEN p.execution_state IS NULL OR
                                   (typeof(p.execution_state)='text' AND octet_length(p.execution_state)<=64)
                         THEN p.execution_state END,
                    CASE WHEN p.freshness IS NULL OR
                                   (typeof(p.freshness)='text' AND octet_length(p.freshness)<=64)
                         THEN p.freshness END,
                    CASE WHEN p.custody_id IS NULL OR
                                   (typeof(p.custody_id)='text' AND octet_length(p.custody_id)<=128)
                         THEN p.custody_id END,
                    CASE WHEN p.custody_generation IS NULL THEN NULL
                         WHEN typeof(p.custody_generation)='integer' AND p.custody_generation>0
                         THEN printf('%lld',p.custody_generation) END,
                    CASE WHEN r.state IS NULL OR
                                   (typeof(r.state)='text' AND octet_length(r.state)<=64) THEN r.state END,
                    CASE WHEN r.owner_session_id IS NULL OR
                                   (typeof(r.owner_session_id)='text' AND octet_length(r.owner_session_id)<=128)
                         THEN r.owner_session_id END,
                    CASE WHEN r.generation IS NULL THEN NULL
                         WHEN typeof(r.generation)='integer' AND r.generation>0
                         THEN printf('%lld',r.generation) END,
                    CASE WHEN p.error_code IS NULL OR
                                   (typeof(p.error_code)='text' AND octet_length(p.error_code)<=128)
                         THEN p.error_code END,
                    CASE WHEN s.session_kind IS NULL OR
                                   (typeof(s.session_kind)='text' AND octet_length(s.session_kind)<=64)
                         THEN s.session_kind END,
                    CASE WHEN s.sandbox_kind IS NULL OR
                                   (typeof(s.sandbox_kind)='text' AND octet_length(s.sandbox_kind)<=64)
                         THEN s.sandbox_kind END,
                    CASE WHEN s.sandbox_branch IS NULL OR
                                   (typeof(s.sandbox_branch)='text' AND octet_length(s.sandbox_branch)<=4096)
                         THEN s.sandbox_branch END,
                    CASE WHEN s.sandbox_custody_id IS NULL OR
                                   (typeof(s.sandbox_custody_id)='text' AND octet_length(s.sandbox_custody_id)<=128)
                         THEN s.sandbox_custody_id END,
                    CASE WHEN p.canonical_repo_dir IS NULL OR
                                   (typeof(p.canonical_repo_dir)='text' AND octet_length(p.canonical_repo_dir)<=4096)
                         THEN p.canonical_repo_dir END,
                    CASE WHEN r.custody_id IS NULL OR
                                   (typeof(r.custody_id)='text' AND octet_length(r.custody_id)<=128)
                         THEN r.custody_id END,
                    CASE WHEN r.canonical_repo_dir IS NULL OR
                                   (typeof(r.canonical_repo_dir)='text' AND octet_length(r.canonical_repo_dir)<=4096)
                         THEN r.canonical_repo_dir END,
                    CASE WHEN r.sandbox_root IS NULL OR
                                   (typeof(r.sandbox_root)='text' AND octet_length(r.sandbox_root)<=4096)
                         THEN r.sandbox_root END,
                    CASE WHEN r.sandbox_branch IS NULL OR
                                   (typeof(r.sandbox_branch)='text' AND octet_length(r.sandbox_branch)<=4096)
                         THEN r.sandbox_branch END,
                    CASE WHEN r.validation_state IS NULL OR
                                   (typeof(r.validation_state)='text' AND octet_length(r.validation_state)<=64)
                         THEN r.validation_state END,
                    CASE WHEN r.validated_generation IS NULL THEN NULL
                         WHEN typeof(r.validated_generation)='integer' AND r.validated_generation>0
                         THEN printf('%lld',r.validated_generation) END,
                    CASE WHEN r.validation_error_code IS NULL OR
                                   (typeof(r.validation_error_code)='text' AND octet_length(r.validation_error_code)<=128)
                         THEN r.validation_error_code END,
                    CASE WHEN p.validated_at IS NULL OR
                                   (typeof(p.validated_at)='text' AND octet_length(p.validated_at)<=64)
                         THEN p.validated_at END,
                    CASE WHEN typeof(p.schema_version)='integer' THEN printf('%lld',p.schema_version) END,
                    CASE WHEN typeof(p.projection_version)='integer' THEN printf('%lld',p.projection_version) END,
                    CASE WHEN typeof(p.updated_at)='text' AND octet_length(p.updated_at)<=64
                         THEN p.updated_at END,
                    typeof(s.id)!='text' OR octet_length(s.id)>128
                      OR typeof(s.status)!='text' OR octet_length(s.status)>64
                      OR typeof(s.working_dir)!='text' OR octet_length(s.working_dir)>4096
                      OR (s.sandbox_root IS NOT NULL AND
                          (typeof(s.sandbox_root)!='text' OR octet_length(s.sandbox_root)>4096))
                      OR (p.effective_cwd IS NOT NULL AND
                          (typeof(p.effective_cwd)!='text' OR octet_length(p.effective_cwd)>4096))
                      OR (s.sandbox_cleanup_state IS NOT NULL AND
                          (typeof(s.sandbox_cleanup_state)!='text' OR octet_length(s.sandbox_cleanup_state)>64))
                      OR (p.execution_state IS NOT NULL AND
                          (typeof(p.execution_state)!='text' OR octet_length(p.execution_state)>64))
                      OR (p.freshness IS NOT NULL AND
                          (typeof(p.freshness)!='text' OR octet_length(p.freshness)>64))
                      OR (p.custody_id IS NOT NULL AND
                          (typeof(p.custody_id)!='text' OR octet_length(p.custody_id)>128))
                      OR (r.state IS NOT NULL AND
                          (typeof(r.state)!='text' OR octet_length(r.state)>64))
                      OR (r.owner_session_id IS NOT NULL AND
                          (typeof(r.owner_session_id)!='text' OR octet_length(r.owner_session_id)>128))
                      OR (p.error_code IS NOT NULL AND
                          (typeof(p.error_code)!='text' OR octet_length(p.error_code)>128))
                      OR (s.session_kind IS NOT NULL AND
                          (typeof(s.session_kind)!='text' OR octet_length(s.session_kind)>64))
                      OR (s.sandbox_kind IS NOT NULL AND
                          (typeof(s.sandbox_kind)!='text' OR octet_length(s.sandbox_kind)>64))
                      OR (s.sandbox_branch IS NOT NULL AND
                          (typeof(s.sandbox_branch)!='text' OR octet_length(s.sandbox_branch)>4096))
                      OR (s.sandbox_custody_id IS NOT NULL AND
                          (typeof(s.sandbox_custody_id)!='text' OR octet_length(s.sandbox_custody_id)>128))
                      OR (p.canonical_repo_dir IS NOT NULL AND
                          (typeof(p.canonical_repo_dir)!='text' OR octet_length(p.canonical_repo_dir)>4096))
                      OR (r.custody_id IS NOT NULL AND
                          (typeof(r.custody_id)!='text' OR octet_length(r.custody_id)>128))
                      OR (r.canonical_repo_dir IS NOT NULL AND
                          (typeof(r.canonical_repo_dir)!='text' OR octet_length(r.canonical_repo_dir)>4096))
                      OR (r.sandbox_root IS NOT NULL AND
                          (typeof(r.sandbox_root)!='text' OR octet_length(r.sandbox_root)>4096))
                      OR (r.sandbox_branch IS NOT NULL AND
                          (typeof(r.sandbox_branch)!='text' OR octet_length(r.sandbox_branch)>4096))
                      OR (r.validation_state IS NOT NULL AND
                          (typeof(r.validation_state)!='text' OR octet_length(r.validation_state)>64))
                      OR (r.validation_error_code IS NOT NULL AND
                          (typeof(r.validation_error_code)!='text' OR octet_length(r.validation_error_code)>128))
                      OR (p.validated_at IS NOT NULL AND
                          (typeof(p.validated_at)!='text' OR octet_length(p.validated_at)>64))
                      OR p.session_id IS NULL
                      OR typeof(p.schema_version)!='integer'
                      OR typeof(p.projection_version)!='integer'
                      OR typeof(p.updated_at)!='text' OR octet_length(p.updated_at)>64
                      OR (p.custody_generation IS NOT NULL AND
                          (typeof(p.custody_generation)!='integer' OR p.custody_generation<=0))
                      OR (r.generation IS NOT NULL AND
                          (typeof(r.generation)!='integer' OR r.generation<=0))
                      OR (r.validated_generation IS NOT NULL AND
                          (typeof(r.validated_generation)!='integer' OR r.validated_generation<=0))
                 FROM sessions s
                 LEFT JOIN session_execution_projections p ON p.session_id=s.id
                 LEFT JOIN sandbox_custody_roots r ON r.custody_id=p.custody_id
                 WHERE (s.session_kind IS NULL OR s.session_kind NOT IN ('Group','Epic'))
                   AND (?1 IS NULL OR s.id>?1)
                 ORDER BY s.id LIMIT ?2",
            )?;
            let mut session_rows = sessions.query(params![
                after_session_id.as_deref(),
                i64::try_from(SETTLEMENT_DEPENDENCY_SCAN_PAGE + 1).map_err(|_| {
                    DaemonError::Store("Session dependency page bound exceeds i64".into())
                })?,
            ])?;
            let mut page_count = 0_usize;
            let mut has_more = false;
            while let Some(session) = session_rows.next()? {
                page_count = page_count.checked_add(1).ok_or_else(|| {
                    DaemonError::Store("Session dependency page count overflowed".into())
                })?;
                if page_count > SETTLEMENT_DEPENDENCY_SCAN_PAGE {
                    has_more = true;
                    break;
                }
                session_records_scanned =
                    session_records_scanned.checked_add(1).ok_or_else(|| {
                        DaemonError::Store("Session dependency scan count overflowed".into())
                    })?;
                if session.get::<_, bool>(30)? {
                    session_scan_overflow = true;
                    break;
                }
                let fields = (0..30)
                    .map(|column| session.get::<_, Option<String>>(column))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let Some(session_id) = fields[0].as_deref() else {
                    session_scan_overflow = true;
                    break;
                };
                for (column, label) in [
                    (0, "Session dependency session id"),
                    (8, "Session dependency projection custody id"),
                    (11, "Session dependency root owner id"),
                    (17, "Session dependency linked custody id"),
                    (19, "Session dependency root custody id"),
                ] {
                    if fields[column]
                        .as_deref()
                        .is_some_and(|value| parse_canonical_uuid(value, label).is_err())
                    {
                        session_scan_overflow = true;
                        break;
                    }
                }
                if session_scan_overflow {
                    break;
                }
                after_session_id = Some(session_id.to_string());
                if !matches!(
                    fields[14].as_deref(),
                    Some(
                        "Standard"
                            | "TaskRabbit"
                            | "Bug"
                            | "Story"
                            | "Task"
                            | "Feature"
                            | "Refactor"
                            | "Research"
                    )
                ) {
                    session_scan_overflow = true;
                    break;
                }
                let persisted_active_like = matches!(
                    fields[1].as_deref(),
                    Some("Starting" | "Running" | "WaitingApproval")
                );
                let authenticated_projection = fields[26].is_some()
                    && fields[27].as_deref() == Some("1")
                    && fields[28].as_deref() == Some("1")
                    && fields[29].is_some();
                let rootless_projection = fields[10..=12].iter().all(Option::is_none)
                    && fields[19..=25].iter().all(Option::is_none);
                let can_execute_or_restore = persisted_active_like
                    || match fields[6].as_deref() {
                        // Executable states are dependencies whenever any of
                        // their three retained path identities overlaps a
                        // candidate. A malformed near-miss must never become
                        // inert merely because one publisher invariant drifted.
                        Some("ordinary_unsandboxed" | "live_sandboxed") => true,
                        Some("historical_cleanup_failed") => {
                            let common = authenticated_projection
                                && fields[4].is_none()
                                && fields[5].as_deref() == Some("Failed")
                                && fields[7].as_deref() == Some("verified")
                                && fields[13].is_none()
                                && fields[15].as_deref() == Some("GitWorktree")
                                && fields[18] == fields[2];
                            let rootless = common
                                && fields[3].is_none()
                                && fields[16].is_none()
                                && fields[17].is_none()
                                && fields[8].is_none()
                                && fields[9].is_none()
                                && rootless_projection;
                            let linked_failed = common
                                && fields[3] == fields[21]
                                && fields[16] == fields[22]
                                && fields[17] == fields[8]
                                && fields[8] == fields[19]
                                && fields[9] == fields[12]
                                && fields[9] == fields[24]
                                && fields[2] == fields[20]
                                && fields[18] == fields[20]
                                && fields[10].as_deref() == Some("failed")
                                && fields[11].is_none()
                                && fields[23].as_deref() == Some("verified")
                                && fields[25].is_none();
                            if !(rootless || linked_failed) {
                                session_scan_overflow = true;
                                break;
                            }
                            false
                        }
                        Some("quarantined") => {
                            if !authenticated_projection
                                || fields[15].as_deref() != Some("GitWorktree")
                                || fields[4].is_some()
                                || fields[5].as_deref() != Some("Failed")
                                || fields[7].as_deref() != Some("invalid")
                                || fields[3] != fields[21]
                                || fields[16] != fields[22]
                                || fields[17] != fields[8]
                                || fields[8] != fields[19]
                                || fields[9] != fields[12]
                                || fields[9] != fields[24]
                                || fields[2] != fields[20]
                                || fields[18] != fields[20]
                                || fields[10].as_deref() != Some("quarantined")
                                || fields[11].is_some()
                                || fields[23].as_deref() != Some("invalid")
                                || fields[13].is_none()
                                || fields[13] != fields[25]
                            {
                                session_scan_overflow = true;
                                break;
                            }
                            false
                        }
                        Some("historical_transferred") => {
                            let projection_generation = fields[9]
                                .as_deref()
                                .and_then(|value| value.parse::<u64>().ok());
                            let root_generation = fields[12]
                                .as_deref()
                                .and_then(|value| value.parse::<u64>().ok());
                            if !authenticated_projection
                                || fields[7].as_deref() != Some("verified")
                                || fields[4].is_some()
                                || fields[13].is_some()
                                || fields[15].as_deref() != Some("GitWorktree")
                                || fields[5].as_deref() != Some("Live")
                                || fields[3] != fields[21]
                                || fields[16] != fields[22]
                                || fields[17] != fields[8]
                                || fields[8] != fields[19]
                                || fields[2] != fields[20]
                                || fields[18] != fields[20]
                                || fields[10].as_deref() != Some("live")
                                || fields[11].as_deref() == Some(session_id)
                                || fields[11].is_none()
                                || fields[23].as_deref() != Some("verified")
                                || fields[24] != fields[12]
                                || fields[25].is_some()
                                || !matches!((projection_generation, root_generation),
                                    (Some(projection), Some(root)) if projection < root)
                            {
                                session_scan_overflow = true;
                                break;
                            }
                            false
                        }
                        Some("historical_purged") => {
                            let common = authenticated_projection
                                && fields[7].as_deref() == Some("verified")
                                && fields[4].is_none()
                                && fields[13].is_none()
                                && fields[18] == fields[2];
                            let rootless = common
                                && fields[3].is_none()
                                && fields[16].is_none()
                                && fields[17].is_none()
                                && fields[8].is_none()
                                && fields[9].is_none()
                                && matches!(
                                    (fields[15].as_deref(), fields[5].as_deref()),
                                    (None, None) | (Some("GitWorktree"), Some("Purged"))
                                )
                                && rootless_projection;
                            let linked = common
                                && fields[15].as_deref() == Some("GitWorktree")
                                && fields[5].as_deref() == Some("Purged")
                                && fields[3].is_none()
                                && fields[16].is_none()
                                && fields[17] == fields[8]
                                && fields[8] == fields[19]
                                && fields[9] == fields[12]
                                && fields[9] == fields[24]
                                && fields[2] == fields[20]
                                && fields[10].as_deref() == Some("purged")
                                && fields[11].is_none()
                                && fields[23].as_deref() == Some("verified")
                                && fields[25].is_none();
                            if !(rootless || linked) {
                                session_scan_overflow = true;
                                break;
                            }
                            false
                        }
                        Some("invalid") => {
                            let inert = authenticated_projection
                                && fields[7].as_deref() == Some("invalid")
                                && fields[4].is_none()
                                && fields[13].is_some()
                                && fields[3].is_none()
                                && fields[16].is_none()
                                && fields[17].is_none()
                                && fields[8].is_none()
                                && fields[9].is_none()
                                && fields[18] == fields[2]
                                && rootless_projection;
                            !inert
                        }
                        // Missing projection state is malformed/ambiguous. Retain a
                        // matching root rather than guessing that the row is inert.
                        None => true,
                        Some(_) => {
                            session_scan_overflow = true;
                            break;
                        }
                    };
                if !can_execute_or_restore {
                    continue;
                }
                let mut matched = Vec::new();
                for path in [
                    fields[2].as_deref(),
                    fields[3].as_deref(),
                    fields[4].as_deref(),
                ]
                .into_iter()
                .flatten()
                {
                    let path = std::path::Path::new(path);
                    let canonical_path = std::fs::canonicalize(path).ok();
                    extend_precomputed_root_matches(
                        path,
                        canonical_path.as_deref(),
                        &raw_root_indexes,
                        &canonical_root_indexes,
                        &mut matched,
                    );
                }
                for custody_id in [
                    fields[8].as_deref(),
                    fields[17].as_deref(),
                    fields[19].as_deref(),
                ]
                .into_iter()
                .flatten()
                {
                    if let Some(indexes) = custody_indexes.get(custody_id) {
                        matched.extend(indexes.iter().copied());
                    }
                }
                matched.sort_unstable();
                matched.dedup();
                for index in matched {
                    let candidate = &inventory[index];
                    if candidate.session_id.map(|id| id.to_string()).as_deref() == Some(session_id)
                        || candidate
                            .owner_session_id
                            .map(|id| id.to_string())
                            .as_deref()
                            == Some(session_id)
                    {
                        continue;
                    }
                    session_counts[index] =
                        session_counts[index].checked_add(1).ok_or_else(|| {
                            DaemonError::Store(
                                "Session path dependency count overflowed u64".into(),
                            )
                        })?;
                    for field in &fields {
                        match field {
                            Some(field) => {
                                session_digests[index].update([1]);
                                session_digests[index].update(
                                    u64::try_from(field.len())
                                        .map_err(|_| {
                                            DaemonError::Store(
                                                "Session dependency field length exceeds u64"
                                                    .into(),
                                            )
                                        })?
                                        .to_be_bytes(),
                                );
                                session_digests[index].update(field.as_bytes());
                            }
                            None => session_digests[index].update([0]),
                        }
                    }
                }
            }
            if session_scan_overflow {
                break;
            }
            if has_more && session_records_scanned >= SETTLEMENT_DEPENDENCY_SCAN_MAX_RECORDS {
                session_scan_overflow = true;
                break;
            }
            if !has_more {
                break;
            }
        }
        if session_scan_overflow {
            for (count, digest) in session_counts.iter_mut().zip(&mut session_digests) {
                *count = u64::MAX;
                *digest = Sha256::new();
                digest.update(b"rsi-session-path-dependencies-scan-bound-exceeded-v1\0");
            }
        }
        for (index, row) in inventory.iter_mut().enumerate() {
            row.session_path_dependency_count = session_counts[index];
            row.session_path_dependency_digest = format!(
                "sha256:{:x}",
                std::mem::take(&mut session_digests[index]).finalize()
            );
        }
        Ok(inventory)
    }

    pub(crate) fn insert_source_worktree_settlement_run(
        &mut self,
        run: &NewSettlementRun,
    ) -> Result<InsertSettlementRunOutcome> {
        validate_new_settlement_run(run)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT CASE WHEN typeof(run_id)='text' AND octet_length(run_id)<=128 THEN run_id END,
                        CASE WHEN typeof(request_fingerprint)='text' AND octet_length(request_fingerprint)<=128
                             THEN request_fingerprint END,
                        typeof(run_id)!='text' OR octet_length(run_id)>128
                          OR typeof(request_fingerprint)!='text' OR octet_length(request_fingerprint)>128
                   FROM source_worktree_settlement_runs
                 WHERE repository_identity=?1 AND idempotency_key=?2",
                params![run.repository_identity, run.idempotency_key],
                |row| {
                    if row.get::<_, bool>(2)? {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(
                                "settlement replay identity exceeds bounds or has an invalid storage class",
                            )),
                        ));
                    }
                    Ok((row.get(0)?, row.get(1)?))
                },
            )
            .optional()?;
        if let Some((run_id, fingerprint)) = existing {
            if fingerprint != run.request_fingerprint {
                return Err(DaemonError::InvalidParam(
                    "idempotency key already belongs to a different settlement request".into(),
                ));
            }
            let run_id = Uuid::parse_str(&run_id).map_err(|error| {
                DaemonError::Store(format!("invalid settlement run id: {error}"))
            })?;
            let receipt = load_run_on(&tx, run_id)?.ok_or_else(|| {
                DaemonError::Store("settlement replay receipt disappeared".into())
            })?;
            tx.commit()?;
            return Ok(InsertSettlementRunOutcome::Replay(receipt));
        }

        for item in &run.items {
            let fenced: bool = tx.query_row(
                "SELECT EXISTS(
                     SELECT 1
                       FROM source_worktree_settlement_items AS i
                      WHERE i.repository_identity=?1 AND i.custody_id=?2
                        AND i.phase NOT IN ('refused','unattempted')
                      LIMIT 1
                 )",
                params![run.repository_identity, item.custody_id.to_string()],
                |row| row.get(0),
            )?;
            if fenced {
                return Err(DaemonError::InvalidParam(
                    "settlement custody root remains fenced by an earlier durable run".into(),
                ));
            }
        }

        let now = timestamp();
        tx.execute(
            "INSERT INTO source_worktree_settlement_runs (
                run_id,schema_version,policy_version,repository_identity,canonical_repo_dir,
                target_ref,target_oid,plan_digest,idempotency_key,authorization_digest,
                request_fingerprint,state,observed_count,eligible_count,retained_count,
                settled_count,refused_count,recovery_required_count,unattempted_count,
                created_at,updated_at,finished_at,terminal_error
             ) VALUES (?1,1,1,?2,?3,?4,?5,?6,?7,?8,?9,'intent_committed',?10,?11,?12,0,0,0,0,?13,?13,NULL,NULL)",
            params![
                run.run_id.to_string(),
                run.repository_identity,
                run.canonical_repo_dir,
                run.target_ref,
                run.target_oid,
                run.plan_digest,
                run.idempotency_key,
                run.authorization_digest,
                run.request_fingerprint,
                run.observed_count,
                u32::try_from(run.items.len()).map_err(|_| {
                    DaemonError::InvalidParam("settlement item count exceeds u32".into())
                })?,
                run.retained_count,
                now,
            ],
        )?;
        for (sequence, item) in run.items.iter().enumerate() {
            tx.execute(
                "INSERT INTO source_worktree_settlement_items (
                    run_id,sequence,session_id,original_status,original_updated_at,custody_id,
                    custody_generation,canonical_repo_dir,sandbox_root,sandbox_branch,
                    repository_identity,source_ref,source_oid,target_oid,evidence_digest,
                    clean_state_digest,reserved_effects,active_effects,participant_count,
                    phase,before_observation,after_observation,refusal_code,created_at,updated_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,'intent_committed',NULL,NULL,NULL,?20,?20)",
                params![
                    run.run_id.to_string(),
                    u32::try_from(sequence).map_err(|_| {
                        DaemonError::InvalidParam("settlement item sequence exceeds u32".into())
                    })?,
                    item.session_id.to_string(),
                    item.original_status,
                    item.original_updated_at,
                    item.custody_id.to_string(),
                    item.custody_generation,
                    item.canonical_repo_dir,
                    item.sandbox_root,
                    item.sandbox_branch,
                    item.repository_identity,
                    item.source_ref,
                    item.source_oid,
                    item.target_oid,
                    item.evidence_digest,
                    item.clean_state_digest,
                    item.reserved_effects,
                    item.active_effects,
                    item.participant_count,
                    now,
                ],
            )?;
        }
        tx.commit()?;
        Ok(InsertSettlementRunOutcome::Inserted)
    }

    pub(crate) fn get_source_worktree_settlement_run(
        &self,
        run_id: Uuid,
    ) -> Result<Option<SourceWorktreeSettlementRunV1>> {
        load_run_on(&self.conn, run_id)
    }

    pub(crate) fn latest_source_worktree_settlement_run(
        &self,
        repository_identity: &str,
    ) -> Result<Option<SourceWorktreeSettlementRunV1>> {
        validate_identity(repository_identity).map_err(DaemonError::InvalidParam)?;
        let latest: Option<(Option<String>, bool)> = self
            .conn
            .query_row(
                "SELECT CASE WHEN typeof(latest.run_id)='text'
                                      AND octet_length(latest.run_id)<=128
                                  THEN latest.run_id END,
                        typeof(latest.repository_identity)!='text'
                          OR octet_length(latest.repository_identity) NOT BETWEEN 1 AND 4096
                          OR typeof(latest.canonical_repo_dir)!='text'
                          OR octet_length(latest.canonical_repo_dir) NOT BETWEEN 1 AND 4096
                          OR typeof(latest.run_id)!='text' OR octet_length(latest.run_id)>128
                          OR typeof(latest.generation)!='integer' OR latest.generation<=0
                          OR r.run_id IS NULL
                          OR r.repository_identity IS NOT latest.repository_identity
                          OR r.canonical_repo_dir IS NOT latest.canonical_repo_dir
                          OR ordered.run_id IS NULL
                          OR EXISTS (
                              SELECT 1 FROM source_worktree_settlement_run_order newer
                               WHERE newer.repository_identity=latest.repository_identity
                                 AND newer.generation>latest.generation
                          )
                   FROM source_worktree_settlement_latest_runs latest
                   LEFT JOIN source_worktree_settlement_runs r ON r.run_id=latest.run_id
                   LEFT JOIN source_worktree_settlement_run_order ordered
                     ON ordered.repository_identity=latest.repository_identity
                    AND ordered.canonical_repo_dir=latest.canonical_repo_dir
                    AND ordered.run_id=latest.run_id
                    AND ordered.generation=latest.generation
                  WHERE latest.repository_identity=?1",
                [repository_identity],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((run_id, invalid)) = latest else {
            return Ok(None);
        };
        if invalid {
            return Err(DaemonError::Store(
                "latest settlement receipt identity exceeds bounds or has an invalid storage class"
                    .into(),
            ));
        }
        let run_id = parse_canonical_uuid(
            run_id
                .as_deref()
                .ok_or_else(|| DaemonError::Store("latest settlement run id is absent".into()))?,
            "latest settlement run id",
        )?;
        load_run_on(&self.conn, run_id)
    }

    pub(crate) fn replay_source_worktree_settlement_run(
        &self,
        repository_identity: &str,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> Result<Option<SourceWorktreeSettlementRunV1>> {
        let existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT CASE WHEN typeof(run_id)='text' AND octet_length(run_id)<=128 THEN run_id END,
                        CASE WHEN typeof(request_fingerprint)='text' AND octet_length(request_fingerprint)<=128
                             THEN request_fingerprint END,
                        typeof(run_id)!='text' OR octet_length(run_id)>128
                          OR typeof(request_fingerprint)!='text' OR octet_length(request_fingerprint)>128
                   FROM source_worktree_settlement_runs
                 WHERE repository_identity=?1 AND idempotency_key=?2",
                params![repository_identity, idempotency_key],
                |row| {
                    if row.get::<_, bool>(2)? {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(
                                "settlement replay identity exceeds bounds or has an invalid storage class",
                            )),
                        ));
                    }
                    Ok((row.get(0)?, row.get(1)?))
                },
            )
            .optional()?;
        let Some((run_id, existing_fingerprint)) = existing else {
            return Ok(None);
        };
        if existing_fingerprint != request_fingerprint {
            return Err(DaemonError::InvalidParam(
                "idempotency key already belongs to a different settlement request".into(),
            ));
        }
        let run_id = Uuid::parse_str(&run_id)
            .map_err(|error| DaemonError::Store(format!("invalid settlement run id: {error}")))?;
        load_run_on(&self.conn, run_id)
    }

    pub(crate) fn list_source_worktree_settlement_journal_items(
        &self,
        run_id: Uuid,
    ) -> Result<Vec<SourceWorktreeSettlementJournalItem>> {
        let mut statement = self.conn.prepare(
            "SELECT CASE WHEN typeof(sequence)='integer' THEN sequence END,
                    CASE WHEN typeof(session_id)='text' AND octet_length(session_id)<=128 THEN session_id END,
                    CASE WHEN typeof(original_status)='text' AND octet_length(original_status)<=32 THEN original_status END,
                    CASE WHEN typeof(original_updated_at)='text' AND octet_length(original_updated_at)<=64 THEN original_updated_at END,
                    CASE WHEN typeof(custody_id)='text' AND octet_length(custody_id)<=128 THEN custody_id END,
                    CASE WHEN typeof(custody_generation)='integer' THEN custody_generation END,
                    CASE WHEN typeof(canonical_repo_dir)='text' AND octet_length(canonical_repo_dir)<=4096 THEN canonical_repo_dir END,
                    CASE WHEN typeof(sandbox_root)='text' AND octet_length(sandbox_root)<=4096 THEN sandbox_root END,
                    CASE WHEN typeof(sandbox_branch)='text' AND octet_length(sandbox_branch)<=4096 THEN sandbox_branch END,
                    CASE WHEN typeof(repository_identity)='text' AND octet_length(repository_identity)<=4096 THEN repository_identity END,
                    CASE WHEN typeof(source_ref)='text' AND octet_length(source_ref)<=4096 THEN source_ref END,
                    CASE WHEN typeof(source_oid)='text' AND octet_length(source_oid)<=128 THEN source_oid END,
                    CASE WHEN typeof(target_oid)='text' AND octet_length(target_oid)<=128 THEN target_oid END,
                    CASE WHEN typeof(evidence_digest)='text' AND octet_length(evidence_digest)<=128 THEN evidence_digest END,
                    CASE WHEN typeof(clean_state_digest)='text' AND octet_length(clean_state_digest)<=128 THEN clean_state_digest END,
                    CASE WHEN typeof(reserved_effects)='integer' THEN reserved_effects END,
                    CASE WHEN typeof(active_effects)='integer' THEN active_effects END,
                    CASE WHEN typeof(participant_count)='integer' THEN participant_count END,
                    CASE WHEN typeof(phase)='text' AND octet_length(phase)<=64 THEN phase END,
                    CASE WHEN before_observation IS NULL OR
                                   (typeof(before_observation)='text' AND octet_length(before_observation)<=16384)
                         THEN before_observation END,
                    typeof(sequence)!='integer'
                      OR typeof(session_id)!='text' OR octet_length(session_id)>128
                      OR typeof(original_status)!='text' OR octet_length(original_status)>32
                      OR typeof(original_updated_at)!='text' OR octet_length(original_updated_at)>64
                      OR typeof(custody_id)!='text' OR octet_length(custody_id)>128
                      OR typeof(custody_generation)!='integer'
                      OR typeof(canonical_repo_dir)!='text' OR octet_length(canonical_repo_dir)>4096
                      OR typeof(sandbox_root)!='text' OR octet_length(sandbox_root)>4096
                      OR typeof(sandbox_branch)!='text' OR octet_length(sandbox_branch)>4096
                      OR typeof(repository_identity)!='text' OR octet_length(repository_identity)>4096
                      OR typeof(source_ref)!='text' OR octet_length(source_ref)>4096
                      OR typeof(source_oid)!='text' OR octet_length(source_oid)>128
                      OR typeof(target_oid)!='text' OR octet_length(target_oid)>128
                      OR typeof(evidence_digest)!='text' OR octet_length(evidence_digest)>128
                      OR typeof(clean_state_digest)!='text' OR octet_length(clean_state_digest)>128
                      OR typeof(reserved_effects)!='integer'
                      OR typeof(active_effects)!='integer'
                      OR typeof(participant_count)!='integer'
                      OR typeof(phase)!='text' OR octet_length(phase)>64
                      OR (before_observation IS NOT NULL AND
                          (typeof(before_observation)!='text' OR octet_length(before_observation)>16384))
             FROM source_worktree_settlement_items WHERE run_id=?1 ORDER BY sequence
             LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                run_id.to_string(),
                i64::try_from(SETTLEMENT_ITEM_LOAD_LIMIT).map_err(|_| {
                    DaemonError::Store("settlement item load limit exceeds i64".into())
                })?
            ],
            |row| {
                if row.get::<_, bool>(20)? {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        20,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::other(
                            "settlement journal item exceeds byte bound",
                        )),
                    ));
                }
                let session_id: String = row.get(1)?;
                let custody_id: String = row.get(4)?;
                let phase: String = row.get(18)?;
                Ok(SourceWorktreeSettlementJournalItem {
                    sequence: row_u32(row, 0)?,
                    session_id: parse_uuid_column(&session_id, 1)?,
                    original_status: row.get(2)?,
                    original_updated_at: row.get(3)?,
                    custody_id: parse_uuid_column(&custody_id, 4)?,
                    custody_generation: row_u64(row, 5)?,
                    canonical_repo_dir: row.get(6)?,
                    sandbox_root: row.get(7)?,
                    sandbox_branch: row.get(8)?,
                    repository_identity: row.get(9)?,
                    source_ref: row.get(10)?,
                    source_oid: row.get(11)?,
                    target_oid: row.get(12)?,
                    evidence_digest: row.get(13)?,
                    clean_state_digest: row.get(14)?,
                    reserved_effects: row_u64(row, 15)?,
                    active_effects: row_u64(row, 16)?,
                    participant_count: row_u64(row, 17)?,
                    phase: parse_phase(&phase).map_err(|message| {
                        rusqlite::Error::FromSqlConversionFailure(
                            18,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(message)),
                        )
                    })?,
                    before_observation: row.get(19)?,
                })
            },
        )?;
        let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        if items.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS {
            return Err(DaemonError::Store(format!(
                "settlement journal exceeds {SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS} items"
            )));
        }
        Ok(items)
    }

    pub(crate) fn list_nonterminal_source_worktree_settlement_run_ids(
        &self,
        after_run_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT CASE WHEN typeof(r.run_id)='text' AND octet_length(r.run_id)<=128
                         THEN r.run_id END,
                    typeof(r.run_id)!='text' OR octet_length(r.run_id)>128
               FROM source_worktree_settlement_runs r
             WHERE r.run_id>?1 AND EXISTS (
                 SELECT 1 FROM source_worktree_settlement_items i
                 WHERE i.run_id=r.run_id
                   AND i.phase IN ('intent_committed','worktree_removed','branch_removed')
             )
             ORDER BY r.run_id LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                after_run_id.map(|id| id.to_string()).unwrap_or_default(),
                limit.min(256) as i64
            ],
            |row| {
                if row.get::<_, bool>(1)? {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::other(
                            "nonterminal settlement run id exceeds bounds or has an invalid storage class",
                        )),
                    ));
                }
                let value: String = row.get(0)?;
                Uuid::parse_str(&value).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            },
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub(crate) fn source_worktree_settlement_run_has_nonterminal_items(
        &self,
        run_id: Uuid,
    ) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM source_worktree_settlement_items
                    WHERE run_id=?1 AND phase IN ('intent_committed','worktree_removed','branch_removed')
                 )",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn record_source_worktree_quarantine_remove_authority(
        &mut self,
        run_id: Uuid,
        session_id: Uuid,
        marker: &QuarantineRemoveAuthorityV1,
    ) -> Result<()> {
        if marker.run_id() != run_id || marker.session_id() != session_id {
            return Err(DaemonError::Store(
                "quarantine remove authority does not match its journal identity".into(),
            ));
        }
        let canonical = marker.to_canonical_json()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                "SELECT CASE WHEN typeof(i.phase)='text' AND octet_length(i.phase)<=64 THEN i.phase END,
                        CASE WHEN i.before_observation IS NULL OR
                                       (typeof(i.before_observation)='text' AND octet_length(i.before_observation)<=16384)
                             THEN i.before_observation END,
                        CASE WHEN typeof(i.custody_id)='text' AND octet_length(i.custody_id)<=128 THEN i.custody_id END,
                        CASE WHEN typeof(i.custody_generation)='integer' THEN i.custody_generation END,
                        CASE WHEN typeof(i.sandbox_root)='text' AND octet_length(i.sandbox_root)<=4096 THEN i.sandbox_root END,
                        CASE WHEN typeof(i.repository_identity)='text' AND octet_length(i.repository_identity)<=4096 THEN i.repository_identity END,
                        CASE WHEN typeof(i.canonical_repo_dir)='text' AND octet_length(i.canonical_repo_dir)<=4096 THEN i.canonical_repo_dir END,
                        CASE WHEN typeof(i.source_ref)='text' AND octet_length(i.source_ref)<=4096 THEN i.source_ref END,
                        CASE WHEN typeof(i.source_oid)='text' AND octet_length(i.source_oid)<=128 THEN i.source_oid END,
                        CASE WHEN typeof(i.target_oid)='text' AND octet_length(i.target_oid)<=128 THEN i.target_oid END,
                        CASE WHEN typeof(i.evidence_digest)='text' AND octet_length(i.evidence_digest)<=128 THEN i.evidence_digest END,
                        CASE WHEN typeof(i.clean_state_digest)='text' AND octet_length(i.clean_state_digest)<=128 THEN i.clean_state_digest END,
                        CASE WHEN typeof(r.target_ref)='text' AND octet_length(r.target_ref)<=4096 THEN r.target_ref END,
                        typeof(i.phase)!='text' OR octet_length(i.phase)>64
                          OR (i.before_observation IS NOT NULL AND
                              (typeof(i.before_observation)!='text' OR octet_length(i.before_observation)>16384))
                          OR typeof(i.custody_id)!='text' OR octet_length(i.custody_id)>128
                          OR typeof(i.custody_generation)!='integer'
                          OR typeof(i.sandbox_root)!='text' OR octet_length(i.sandbox_root)>4096
                          OR typeof(i.repository_identity)!='text' OR octet_length(i.repository_identity)>4096
                          OR typeof(i.canonical_repo_dir)!='text' OR octet_length(i.canonical_repo_dir)>4096
                          OR typeof(i.source_ref)!='text' OR octet_length(i.source_ref)>4096
                          OR typeof(i.source_oid)!='text' OR octet_length(i.source_oid)>128
                          OR typeof(i.target_oid)!='text' OR octet_length(i.target_oid)>128
                          OR typeof(i.evidence_digest)!='text' OR octet_length(i.evidence_digest)>128
                          OR typeof(i.clean_state_digest)!='text' OR octet_length(i.clean_state_digest)>128
                          OR typeof(r.target_ref)!='text' OR octet_length(r.target_ref)>4096
                   FROM source_worktree_settlement_items i
                   JOIN source_worktree_settlement_runs r ON r.run_id=i.run_id
                  WHERE i.run_id=?1 AND i.session_id=?2",
                params![run_id.to_string(), session_id.to_string()],
                |row| {
                    if row.get::<_, bool>(13)? {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            13,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(
                                "quarantine authority journal row exceeds bounds",
                            )),
                        ));
                    }
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, String>(11)?,
                        row.get::<_, String>(12)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            phase,
            before_observation,
            custody_id,
            custody_generation,
            sandbox_root,
            repository_identity,
            canonical_repo_dir,
            source_ref,
            source_oid,
            journal_target_oid,
            journal_evidence_digest,
            journal_clean_digest,
            target_ref,
        )) = row
        else {
            return Err(DaemonError::Store(
                "quarantine remove authority journal item is missing".into(),
            ));
        };
        if phase != SourceWorktreeSettlementPhaseV1::IntentCommitted.as_str() {
            return Err(DaemonError::Store(
                "quarantine remove authority requires intent_committed phase".into(),
            ));
        }
        let journal_custody = parse_canonical_uuid(&custody_id, "quarantine authority custody id")?;
        let journal_generation = u64::try_from(custody_generation).map_err(|_| {
            DaemonError::Store("quarantine authority custody generation is invalid".into())
        })?;
        let quarantine_path =
            source_worktree_quarantine_path(Path::new(&sandbox_root), run_id, session_id)?;
        let quarantine_path = quarantine_path.to_str().ok_or_else(|| {
            DaemonError::Store("quarantine authority derived path is not UTF-8".into())
        })?;
        let retained_facts_match = marker.custody_id() == journal_custody
            && marker.custody_generation() == journal_generation
            && marker.original_path_digest()
                == &quarantine_remove_authority_field_digest(
                    QuarantineRemoveAuthorityDigestDomainV1::OriginalPath,
                    &sandbox_root,
                )
            && marker.quarantine_path_digest()
                == &quarantine_remove_authority_field_digest(
                    QuarantineRemoveAuthorityDigestDomainV1::QuarantinePath,
                    quarantine_path,
                )
            && marker.repository_identity_digest()
                == &quarantine_remove_authority_field_digest(
                    QuarantineRemoveAuthorityDigestDomainV1::RepositoryIdentity,
                    &repository_identity,
                )
            && marker.canonical_repo_dir_digest()
                == &quarantine_remove_authority_field_digest(
                    QuarantineRemoveAuthorityDigestDomainV1::CanonicalRepoDir,
                    &canonical_repo_dir,
                )
            && marker.source_ref_digest()
                == &quarantine_remove_authority_field_digest(
                    QuarantineRemoveAuthorityDigestDomainV1::SourceRef,
                    &source_ref,
                )
            && marker.target_ref_digest()
                == &quarantine_remove_authority_field_digest(
                    QuarantineRemoveAuthorityDigestDomainV1::TargetRef,
                    &target_ref,
                )
            && marker.source_oid().as_str() == source_oid
            && marker.journal_target_oid().as_str() == journal_target_oid
            && marker.journal_evidence_digest().as_str() == journal_evidence_digest
            && marker.journal_clean_digest().as_str() == journal_clean_digest;
        if !retained_facts_match {
            return Err(DaemonError::Store(
                "quarantine remove authority does not match retained journal facts".into(),
            ));
        }
        if let Some(existing) = before_observation {
            let parsed = QuarantineRemoveAuthorityV1::parse_canonical(&existing).map_err(|_| {
                DaemonError::Store(
                    "quarantine remove authority conflicts with malformed retained evidence".into(),
                )
            })?;
            if existing == canonical && parsed == *marker {
                tx.commit()?;
                return Ok(());
            }
            return Err(DaemonError::Store(
                "quarantine remove authority conflicts with retained evidence".into(),
            ));
        }
        let now = timestamp();
        let changed = tx.execute(
            "UPDATE source_worktree_settlement_items
                SET before_observation=?1,updated_at=?2
              WHERE run_id=?3 AND session_id=?4
                AND phase='intent_committed' AND before_observation IS NULL",
            params![canonical, now, run_id.to_string(), session_id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "quarantine remove authority compare-and-swap lost".into(),
            ));
        }
        refresh_run_on(&tx, run_id, &now)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn advance_source_worktree_settlement_item(
        &self,
        run_id: Uuid,
        session_id: Uuid,
        expected_phase: SourceWorktreeSettlementPhaseV1,
        next_phase: SourceWorktreeSettlementPhaseV1,
        before_observation: Option<&str>,
        after_observation: Option<&str>,
        refusal_code: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let now = timestamp();
        let changed = tx.execute(
            "UPDATE source_worktree_settlement_items SET phase=?1,
                    before_observation=COALESCE(before_observation,?2),
                    after_observation=COALESCE(?3,after_observation),refusal_code=?4,updated_at=?5
             WHERE run_id=?6 AND session_id=?7 AND phase=?8",
            params![
                next_phase.as_str(),
                before_observation,
                after_observation,
                refusal_code,
                now,
                run_id.to_string(),
                session_id.to_string(),
                expected_phase.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "settlement item phase compare-and-swap lost".into(),
            ));
        }
        refresh_run_on(&tx, run_id, &now)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn finalize_source_worktree_settlement_item(
        &mut self,
        run_id: Uuid,
        session_id: Uuid,
    ) -> Result<()> {
        #[cfg(test)]
        settlement_finalization_fault(SettlementFinalizationFault::BeforeTransaction)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = load_item_fence(&tx, run_id, session_id)?;
        if item.phase != SourceWorktreeSettlementPhaseV1::BranchRemoved {
            return Err(DaemonError::Store(
                "settlement finalization requires branch_removed phase".into(),
            ));
        }
        let now = timestamp();
        let changed = tx.execute(
            "UPDATE sessions SET status='Archived',pending_archive=0,
                    retry_attempt=COALESCE(max_retries,retry_attempt),updated_at=?1
             WHERE id=?2 AND status=?3 AND updated_at=?4 AND sandbox_custody_id=?5
               AND sandbox_kind='GitWorktree' AND sandbox_root=?6 AND sandbox_branch=?7
               AND sandbox_cleanup_state='Live'",
            params![
                now,
                session_id.to_string(),
                item.original_status,
                item.original_updated_at,
                item.custody_id.to_string(),
                item.sandbox_root,
                item.sandbox_branch,
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "settlement final Session fence drifted".into(),
            ));
        }
        Store::resolve_c5_autofile_pending_tx(&tx, session_id)?;
        tx.execute(
            "UPDATE sessions SET lead_session_id=NULL,updated_at=?1 WHERE lead_session_id=?2",
            params![now, session_id.to_string()],
        )?;
        sandbox_custody::transition_terminal_root_tx(
            &tx,
            item.custody_id,
            item.custody_generation,
            CustodyCause::Purge,
            "purged",
            "tombstoned",
            "historical_purged",
            None,
        )?;
        let changed = tx.execute(
            "UPDATE source_worktree_settlement_items SET phase='settled',
                    after_observation='database_settled',refusal_code=NULL,updated_at=?1
             WHERE run_id=?2 AND session_id=?3 AND phase='branch_removed'",
            params![now, run_id.to_string(), session_id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "settlement receipt final compare-and-swap lost".into(),
            ));
        }
        refresh_run_on(&tx, run_id, &now)?;
        #[cfg(test)]
        settlement_finalization_fault(SettlementFinalizationFault::BeforeCommit)?;
        tx.commit()?;
        #[cfg(test)]
        settlement_finalization_fault(SettlementFinalizationFault::AfterCommit)?;
        Ok(())
    }

    pub(crate) fn mark_source_worktree_settlement_recovery_required(
        &self,
        run_id: Uuid,
        session_id: Uuid,
        expected_phase: SourceWorktreeSettlementPhaseV1,
        code: &str,
        observation: &str,
    ) -> Result<()> {
        self.advance_source_worktree_settlement_item(
            run_id,
            session_id,
            expected_phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired,
            None,
            Some(observation),
            Some(code),
        )
    }

    pub(crate) fn mark_source_worktree_settlement_refused(
        &self,
        run_id: Uuid,
        session_id: Uuid,
        expected_phase: SourceWorktreeSettlementPhaseV1,
        code: &str,
        observation: &str,
    ) -> Result<()> {
        self.advance_source_worktree_settlement_item(
            run_id,
            session_id,
            expected_phase,
            SourceWorktreeSettlementPhaseV1::Refused,
            None,
            Some(observation),
            Some(code),
        )
    }

    pub(crate) fn mark_source_worktree_settlement_unattempted(
        &self,
        run_id: Uuid,
        session_id: Uuid,
        observation: &str,
    ) -> Result<()> {
        self.advance_source_worktree_settlement_item(
            run_id,
            session_id,
            SourceWorktreeSettlementPhaseV1::IntentCommitted,
            SourceWorktreeSettlementPhaseV1::Unattempted,
            None,
            Some(observation),
            None,
        )
    }

    pub(crate) fn record_source_worktree_settlement_database_retry(
        &self,
        run_id: Uuid,
        session_id: Uuid,
    ) -> Result<()> {
        self.advance_source_worktree_settlement_item(
            run_id,
            session_id,
            SourceWorktreeSettlementPhaseV1::BranchRemoved,
            SourceWorktreeSettlementPhaseV1::BranchRemoved,
            None,
            Some("Git effects completed; atomic database settlement will retry"),
            Some(SourceWorktreeSettlementRefusalV1::DatabaseSettlementFailed.as_str()),
        )
    }
}

fn extend_precomputed_root_matches(
    path: &std::path::Path,
    canonical_path: Option<&std::path::Path>,
    raw_roots: &HashMap<std::path::PathBuf, Vec<usize>>,
    canonical_roots: &HashMap<std::path::PathBuf, Vec<usize>>,
    matches: &mut Vec<usize>,
) {
    if path.is_absolute() {
        for ancestor in path.ancestors() {
            if let Some(indexes) = raw_roots.get(ancestor) {
                matches.extend(indexes.iter().copied());
            }
        }
    }
    if let Some(canonical_path) = canonical_path {
        for ancestor in canonical_path.ancestors() {
            if let Some(indexes) = canonical_roots.get(ancestor) {
                matches.extend(indexes.iter().copied());
            }
        }
    }
}

fn parse_canonical_uuid(value: &str, field: &str) -> Result<Uuid> {
    let parsed = Uuid::parse_str(value)
        .map_err(|error| DaemonError::Store(format!("invalid {field}: {error}")))?;
    if parsed.is_nil() || parsed.to_string() != value {
        return Err(DaemonError::Store(format!(
            "{field} is not a non-nil lowercase canonical UUID"
        )));
    }
    Ok(parsed)
}

/// Derive the only quarantine path admitted for one retained journal item.
/// Components are canonical UUID text; the original path itself remains exact.
pub(crate) fn source_worktree_quarantine_path(
    original_root: &Path,
    run_id: Uuid,
    session_id: Uuid,
) -> Result<PathBuf> {
    if run_id.is_nil() || session_id.is_nil() {
        return Err(DaemonError::Store(
            "source-worktree quarantine ids must be non-nil".into(),
        ));
    }
    let original = original_root.to_str().ok_or_else(|| {
        DaemonError::Store("source-worktree quarantine original path is not UTF-8".into())
    })?;
    validate_settlement_text(
        original,
        "quarantine original path",
        STARTUP_SETTLEMENT_PATH_MAX_BYTES,
        false,
    )?;
    if !original_root.is_absolute() {
        return Err(DaemonError::Store(
            "source-worktree quarantine original path is not absolute".into(),
        ));
    }
    let session_leaf = session_id.to_string();
    if original_root.file_name().and_then(|leaf| leaf.to_str()) != Some(session_leaf.as_str())
        || original_root.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(DaemonError::Store(
            "source-worktree quarantine original path is not the exact canonical Session leaf"
                .into(),
        ));
    }
    let parent = original_root.parent().ok_or_else(|| {
        DaemonError::Store("source-worktree quarantine original path has no parent".into())
    })?;
    let quarantine = parent
        .join(".settlement-quarantine")
        .join(run_id.to_string())
        .join(session_id.to_string());
    let quarantine_utf8 = quarantine
        .to_str()
        .ok_or_else(|| DaemonError::Store("source-worktree quarantine path is not UTF-8".into()))?;
    validate_settlement_text(
        quarantine_utf8,
        "quarantine path",
        STARTUP_SETTLEMENT_PATH_MAX_BYTES,
        false,
    )?;
    Ok(quarantine)
}

fn validate_startup_settlement_root(
    root: &Path,
    sandbox_base: &Path,
    canonical_base: Option<&Path>,
    owner_session_id: Uuid,
) -> Result<()> {
    validate_startup_settlement_root_with_canonicalizer(
        root,
        sandbox_base,
        canonical_base,
        owner_session_id,
        |path| std::fs::canonicalize(path),
    )
}

fn validate_startup_settlement_root_with_canonicalizer<F>(
    root: &Path,
    sandbox_base: &Path,
    canonical_base: Option<&Path>,
    owner_session_id: Uuid,
    canonicalize_root: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<PathBuf>,
{
    let expected_root = sandbox_base.join(owner_session_id.to_string());
    if !root.is_absolute()
        || !sandbox_base.is_absolute()
        || root != expected_root
        || root.parent() != Some(sandbox_base)
    {
        return Err(DaemonError::Store(format!(
            "startup settlement root is not the exact owner sandbox root: {}",
            root.display()
        )));
    }
    let leaf = root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| DaemonError::Store("startup settlement root is not UTF-8".into()))?;
    parse_canonical_uuid(leaf, "startup settlement root leaf")?;
    let root_exists = match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(DaemonError::Store(format!(
                "startup settlement root is a symbolic link: {}",
                root.display()
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(DaemonError::Store(format!(
                "startup settlement root is not a directory: {}",
                root.display()
            )));
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(DaemonError::Store(format!(
                "startup settlement root identity is unreadable for {}: {error}",
                root.display()
            )));
        }
    };
    if root_exists {
        let canonical_root = canonicalize_root(root).map_err(|error| {
            DaemonError::Store(format!(
                "startup settlement root cannot be canonicalized for {}: {error}",
                root.display()
            ))
        })?;
        let Some(canonical_base) = canonical_base else {
            return Err(DaemonError::Store(
                "startup settlement root exists but sandbox base cannot be canonicalized".into(),
            ));
        };
        if canonical_root != canonical_base.join(owner_session_id.to_string()) {
            return Err(DaemonError::Store(format!(
                "startup settlement root canonical identity mismatches its owner: {}",
                root.display()
            )));
        }
    }
    Ok(())
}

fn validate_startup_quarantine_root(
    root: &Path,
    sandbox_base: &Path,
    canonical_base: Option<&Path>,
    run_id: Uuid,
    session_id: Uuid,
) -> Result<Option<PathBuf>> {
    let quarantine_base = sandbox_base.join(".settlement-quarantine");
    let run_base = quarantine_base.join(run_id.to_string());
    let expected_root = run_base.join(session_id.to_string());
    if !root.is_absolute()
        || !sandbox_base.is_absolute()
        || root != expected_root
        || root.parent() != Some(run_base.as_path())
        || run_base.parent() != Some(quarantine_base.as_path())
        || quarantine_base.parent() != Some(sandbox_base)
    {
        return Err(DaemonError::Store(format!(
            "startup settlement quarantine is outside its exact derived shape: {}",
            root.display()
        )));
    }
    let mut root_exists = false;
    for (path, label) in [
        (quarantine_base.as_path(), "quarantine base"),
        (run_base.as_path(), "quarantine run directory"),
        (root, "quarantine root"),
    ] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(DaemonError::Store(format!(
                    "startup settlement {label} is a symbolic link: {}",
                    path.display()
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(DaemonError::Store(format!(
                    "startup settlement {label} is not a directory: {}",
                    path.display()
                )));
            }
            Ok(_) => {
                if path == root {
                    root_exists = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(DaemonError::Store(format!(
                    "startup settlement {label} identity is unreadable for {}: {error}",
                    path.display()
                )));
            }
        }
    }
    if !root_exists {
        return Ok(None);
    }
    let canonical_root = std::fs::canonicalize(root).map_err(|error| {
        DaemonError::Store(format!(
            "startup settlement quarantine cannot be canonicalized for {}: {error}",
            root.display()
        ))
    })?;
    let Some(canonical_base) = canonical_base else {
        return Err(DaemonError::Store(
            "startup settlement quarantine exists but sandbox base cannot be canonicalized".into(),
        ));
    };
    let expected_canonical = canonical_base
        .join(".settlement-quarantine")
        .join(run_id.to_string())
        .join(session_id.to_string());
    if canonical_root != expected_canonical {
        return Err(DaemonError::Store(format!(
            "startup settlement quarantine canonical identity mismatches its journal: {}",
            root.display()
        )));
    }
    Ok(Some(canonical_root))
}

fn startup_path_matches_settlement_root(
    path: &Path,
    raw_roots: &HashSet<PathBuf>,
    canonical_roots: &HashSet<PathBuf>,
) -> bool {
    if path.is_absolute()
        && path
            .ancestors()
            .any(|ancestor| raw_roots.contains(ancestor))
    {
        return true;
    }
    std::fs::canonicalize(path).is_ok_and(|canonical| {
        canonical
            .ancestors()
            .any(|ancestor| canonical_roots.contains(ancestor))
    })
}

fn parse_inventory_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceWorktreeInventoryRow> {
    fn uuid(value: Option<String>, column: usize) -> rusqlite::Result<Option<Uuid>> {
        value
            .map(|value| parse_uuid_column(&value, column))
            .transpose()
    }
    if row.get::<_, bool>(23)? {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            23,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(
                "source-worktree inventory row exceeds byte bound",
            )),
        ));
    }
    Ok(SourceWorktreeInventoryRow {
        custody_id: uuid(row.get(0)?, 0)?.ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(0, "custody_id".into(), rusqlite::types::Type::Null)
        })?,
        canonical_repo_dir: row.get(1)?,
        sandbox_root: row.get(2)?,
        sandbox_branch: row.get(3)?,
        repository_identity: row.get(4)?,
        source_commit: row.get(5)?,
        owner_session_id: uuid(row.get(6)?, 6)?,
        generation: row_u64(row, 7)?,
        validation_state: row.get(8)?,
        validated_generation: row_optional_u64_or_zero(row, 9)?,
        reserved_effects: row_u64(row, 10)?,
        active_effects: row_u64(row, 11)?,
        session_id: uuid(row.get(12)?, 12)?,
        status: row.get(13)?,
        session_updated_at: row.get(14)?,
        session_working_dir: row.get(15)?,
        session_sandbox_kind: row.get(16)?,
        session_sandbox_root: row.get(17)?,
        session_sandbox_branch: row.get(18)?,
        session_cleanup_state: row.get(19)?,
        session_kind: row.get(20)?,
        pending_archive: row.get::<_, i64>(21)? != 0,
        participant_count: row_u64(row, 22)?,
        scheduled_dependency_count: 0,
        scheduled_dependency_digest:
            rsi_common::cohort_settlement::SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST.to_string(),
        session_path_dependency_count: 0,
        session_path_dependency_digest:
            rsi_common::cohort_settlement::SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST
                .to_string(),
    })
}

fn parse_uuid_column(value: &str, column: usize) -> rusqlite::Result<Uuid> {
    let parsed = Uuid::parse_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    if parsed.is_nil() || parsed.to_string() != value {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(
                "UUID is not non-nil lowercase canonical text",
            )),
        ));
    }
    Ok(parsed)
}

fn row_u32(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u32> {
    let value = row.get::<_, i64>(column)?;
    u32::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn row_u64(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(column)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn row_optional_u64_or_zero(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u64> {
    row.get::<_, Option<i64>>(column)?
        .map(|value| {
            u64::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    column,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })
        })
        .transpose()
        .map(|value| value.unwrap_or(0))
}

#[derive(Debug)]
struct ItemFence {
    custody_id: Uuid,
    custody_generation: u64,
    original_status: String,
    original_updated_at: String,
    sandbox_root: String,
    sandbox_branch: String,
    phase: SourceWorktreeSettlementPhaseV1,
}

fn load_item_fence(tx: &Transaction<'_>, run_id: Uuid, session_id: Uuid) -> Result<ItemFence> {
    tx.query_row(
        "SELECT CASE WHEN typeof(custody_id)='text' AND octet_length(custody_id)<=128 THEN custody_id END,
                CASE WHEN typeof(custody_generation)='integer' THEN custody_generation END,
                CASE WHEN typeof(original_status)='text' AND octet_length(original_status)<=32 THEN original_status END,
                CASE WHEN typeof(original_updated_at)='text' AND octet_length(original_updated_at)<=64 THEN original_updated_at END,
                CASE WHEN typeof(sandbox_root)='text' AND octet_length(sandbox_root)<=4096 THEN sandbox_root END,
                CASE WHEN typeof(sandbox_branch)='text' AND octet_length(sandbox_branch)<=4096 THEN sandbox_branch END,
                CASE WHEN typeof(phase)='text' AND octet_length(phase)<=64 THEN phase END,
                typeof(custody_id)!='text' OR octet_length(custody_id)>128
                  OR typeof(custody_generation)!='integer'
                  OR typeof(original_status)!='text' OR octet_length(original_status)>32
                  OR typeof(original_updated_at)!='text' OR octet_length(original_updated_at)>64
                  OR typeof(sandbox_root)!='text' OR octet_length(sandbox_root)>4096
                  OR typeof(sandbox_branch)!='text' OR octet_length(sandbox_branch)>4096
                  OR typeof(phase)!='text' OR octet_length(phase)>64
         FROM source_worktree_settlement_items WHERE run_id=?1 AND session_id=?2",
        params![run_id.to_string(), session_id.to_string()],
        |row| {
            if row.get::<_, bool>(7)? {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(
                        "settlement item fence exceeds byte bound",
                    )),
                ));
            }
            let custody_id: String = row.get(0)?;
            let phase: String = row.get(6)?;
            Ok(ItemFence {
                custody_id: Uuid::parse_str(&custody_id).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                custody_generation: row_u64(row, 1)?,
                original_status: row.get(2)?,
                original_updated_at: row.get(3)?,
                sandbox_root: row.get(4)?,
                sandbox_branch: row.get(5)?,
                phase: parse_phase(&phase).map_err(|message| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::other(message)),
                    )
                })?,
            })
        },
    )
    .map_err(Into::into)
}

fn load_run_on(
    conn: &rusqlite::Connection,
    run_id: Uuid,
) -> Result<Option<SourceWorktreeSettlementRunV1>> {
    let row = conn
        .query_row(
            "SELECT CASE WHEN typeof(schema_version)='integer' THEN schema_version END,
                    CASE WHEN typeof(policy_version)='integer' THEN policy_version END,
                    CASE WHEN typeof(run_id)='text' AND octet_length(run_id)<=128 THEN run_id END,
                    CASE WHEN typeof(repository_identity)='text' AND octet_length(repository_identity)<=4096 THEN repository_identity END,
                    CASE WHEN typeof(canonical_repo_dir)='text' AND octet_length(canonical_repo_dir)<=4096 THEN canonical_repo_dir END,
                    CASE WHEN typeof(target_ref)='text' AND octet_length(target_ref)<=4096 THEN target_ref END,
                    CASE WHEN typeof(target_oid)='text' AND octet_length(target_oid)<=128 THEN target_oid END,
                    CASE WHEN typeof(plan_digest)='text' AND octet_length(plan_digest)<=128 THEN plan_digest END,
                    CASE WHEN typeof(idempotency_key)='text' AND octet_length(idempotency_key)<=128 THEN idempotency_key END,
                    CASE WHEN typeof(state)='text' AND octet_length(state)<=64 THEN state END,
                    CASE WHEN typeof(observed_count)='integer' THEN observed_count END,
                    CASE WHEN typeof(eligible_count)='integer' THEN eligible_count END,
                    CASE WHEN typeof(retained_count)='integer' THEN retained_count END,
                    CASE WHEN typeof(settled_count)='integer' THEN settled_count END,
                    CASE WHEN typeof(refused_count)='integer' THEN refused_count END,
                    CASE WHEN typeof(recovery_required_count)='integer' THEN recovery_required_count END,
                    CASE WHEN typeof(unattempted_count)='integer' THEN unattempted_count END,
                    CASE WHEN typeof(created_at)='text' AND octet_length(created_at)<=30 THEN created_at END,
                    CASE WHEN typeof(updated_at)='text' AND octet_length(updated_at)<=30 THEN updated_at END,
                    CASE WHEN finished_at IS NULL OR (typeof(finished_at)='text' AND octet_length(finished_at)<=30) THEN finished_at END,
                    CASE WHEN terminal_error IS NULL OR (typeof(terminal_error)='text' AND octet_length(terminal_error)<=16384) THEN terminal_error END,
                    typeof(schema_version)!='integer'
                      OR typeof(policy_version)!='integer'
                      OR typeof(run_id)!='text' OR octet_length(run_id)>128
                      OR typeof(repository_identity)!='text' OR octet_length(repository_identity)>4096
                      OR typeof(canonical_repo_dir)!='text' OR octet_length(canonical_repo_dir)>4096
                      OR typeof(target_ref)!='text' OR octet_length(target_ref)>4096
                      OR typeof(target_oid)!='text' OR octet_length(target_oid)>128
                      OR typeof(plan_digest)!='text' OR octet_length(plan_digest)>128
                      OR typeof(idempotency_key)!='text' OR octet_length(idempotency_key)>128
                      OR typeof(state)!='text' OR octet_length(state)>64
                      OR typeof(observed_count)!='integer'
                      OR typeof(eligible_count)!='integer'
                      OR typeof(retained_count)!='integer'
                      OR typeof(settled_count)!='integer'
                      OR typeof(refused_count)!='integer'
                      OR typeof(recovery_required_count)!='integer'
                      OR typeof(unattempted_count)!='integer'
                      OR typeof(created_at)!='text' OR octet_length(created_at)>30
                      OR typeof(updated_at)!='text' OR octet_length(updated_at)>30
                      OR (finished_at IS NOT NULL AND (typeof(finished_at)!='text' OR octet_length(finished_at)>30))
                      OR (terminal_error IS NOT NULL AND (typeof(terminal_error)!='text' OR octet_length(terminal_error)>16384))
             FROM source_worktree_settlement_runs WHERE run_id=?1",
            [run_id.to_string()],
            |row| {
                if row.get::<_, bool>(21)? {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        21,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::other(
                            "settlement run exceeds byte bound",
                        )),
                    ));
                }
                let run_id: String = row.get(2)?;
                let state: String = row.get(9)?;
                Ok((
                    row_u32(row, 0)?,
                    row_u32(row, 1)?,
                    Uuid::parse_str(&run_id).map_err(|error| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error)))?,
                    row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?, row.get::<_, String>(7)?, row.get::<_, String>(8)?,
                    parse_run_state(&state).map_err(|message| rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(std::io::Error::other(message))))?,
                    SourceWorktreeSettlementCountsV1 {
                        observed: row_u32(row, 10)?,
                        eligible: row_u32(row, 11)?,
                        retained: row_u32(row, 12)?,
                        settled: row_u32(row, 13)?,
                        refused: row_u32(row, 14)?,
                        recovery_required: row_u32(row, 15)?,
                        unattempted: row_u32(row, 16)?,
                    },
                    row.get::<_, String>(17)?, row.get::<_, String>(18)?,
                    row.get::<_, Option<String>>(19)?, row.get::<_, Option<String>>(20)?,
                ))
            },
        )
        .optional()?;
    let Some((
        schema_version,
        policy_version,
        run_id,
        repository_identity,
        canonical_repo_dir,
        target_ref,
        target_oid,
        plan_digest,
        idempotency_key,
        state,
        counts,
        created_at,
        updated_at,
        finished_at,
        terminal_error,
    )) = row
    else {
        return Ok(None);
    };
    let mut statement = conn.prepare(
        "SELECT CASE WHEN typeof(sequence)='integer' THEN sequence END,
                CASE WHEN typeof(session_id)='text' AND octet_length(session_id)<=128 THEN session_id END,
                CASE WHEN typeof(custody_id)='text' AND octet_length(custody_id)<=128 THEN custody_id END,
                CASE WHEN typeof(source_ref)='text' AND octet_length(source_ref)<=4096 THEN source_ref END,
                CASE WHEN typeof(source_oid)='text' AND octet_length(source_oid)<=128 THEN source_oid END,
                CASE WHEN typeof(phase)='text' AND octet_length(phase)<=64 THEN phase END,
                CASE WHEN refusal_code IS NULL OR (typeof(refusal_code)='text' AND octet_length(refusal_code)<=128) THEN refusal_code END,
                CASE WHEN before_observation IS NULL OR (typeof(before_observation)='text' AND octet_length(before_observation)<=16384) THEN before_observation END,
                CASE WHEN after_observation IS NULL OR (typeof(after_observation)='text' AND octet_length(after_observation)<=16384) THEN after_observation END,
                typeof(sequence)!='integer'
                  OR typeof(session_id)!='text' OR octet_length(session_id)>128
                  OR typeof(custody_id)!='text' OR octet_length(custody_id)>128
                  OR typeof(source_ref)!='text' OR octet_length(source_ref)>4096
                  OR typeof(source_oid)!='text' OR octet_length(source_oid)>128
                  OR typeof(phase)!='text' OR octet_length(phase)>64
                  OR (refusal_code IS NOT NULL AND (typeof(refusal_code)!='text' OR octet_length(refusal_code)>128))
                  OR (before_observation IS NOT NULL AND (typeof(before_observation)!='text' OR octet_length(before_observation)>16384))
                  OR (after_observation IS NOT NULL AND (typeof(after_observation)!='text' OR octet_length(after_observation)>16384))
         FROM source_worktree_settlement_items WHERE run_id=?1 ORDER BY sequence
         LIMIT ?2",
    )?;
    let items = statement
        .query_map(
            params![
                run_id.to_string(),
                i64::try_from(SETTLEMENT_ITEM_LOAD_LIMIT).map_err(|_| {
                    DaemonError::Store("settlement item load limit exceeds i64".into())
                })?
            ],
            |row| {
                if row.get::<_, bool>(9)? {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        9,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::other(
                            "settlement receipt item exceeds byte bound",
                        )),
                    ));
                }
                let session_id: String = row.get(1)?;
                let custody_id: String = row.get(2)?;
                let phase: String = row.get(5)?;
                Ok(SourceWorktreeSettlementItemV1 {
                    sequence: row_u32(row, 0)?,
                    session_id: Uuid::parse_str(&session_id).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    custody_id: Uuid::parse_str(&custody_id).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    source_ref: row.get(3)?,
                    expected_source_oid: SourceWorktreeGitOidV1::parse(row.get::<_, String>(4)?)
                        .map_err(|message| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::other(message)),
                            )
                        })?,
                    phase: parse_phase(&phase).map_err(|message| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(message)),
                        )
                    })?,
                    refusal_code: row
                        .get::<_, Option<String>>(6)?
                        .map(|value| value.parse::<SourceWorktreeSettlementRefusalV1>())
                        .transpose()
                        .map_err(|message| {
                            rusqlite::Error::FromSqlConversionFailure(
                                6,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::other(message)),
                            )
                        })?,
                    before_observation: row.get(7)?,
                    after_observation: row.get(8)?,
                })
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if items.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS {
        return Err(DaemonError::Store(format!(
            "settlement receipt exceeds {SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS} items"
        )));
    }
    Ok(Some(SourceWorktreeSettlementRunV1 {
        schema_version,
        policy_version,
        run_id,
        repository_identity,
        canonical_repo_dir,
        target_ref,
        target_oid: SourceWorktreeGitOidV1::parse(target_oid).map_err(DaemonError::Store)?,
        plan_digest: Sha256Digest::parse(plan_digest).map_err(DaemonError::Store)?,
        idempotency_key,
        state,
        counts,
        items,
        created_at,
        updated_at,
        finished_at,
        terminal_error,
    }))
}

fn refresh_run_on(conn: &rusqlite::Connection, run_id: Uuid, now: &str) -> Result<()> {
    let (eligible, settled, refused, recovery, unattempted, intent): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = conn.query_row(
        "SELECT count(*),
                    sum(CASE WHEN phase='settled' THEN 1 ELSE 0 END),
                    sum(CASE WHEN phase='refused' THEN 1 ELSE 0 END),
                    sum(CASE WHEN phase='recovery_required' THEN 1 ELSE 0 END),
                    sum(CASE WHEN phase='unattempted' THEN 1 ELSE 0 END),
                    sum(CASE WHEN phase='intent_committed' THEN 1 ELSE 0 END)
             FROM source_worktree_settlement_items WHERE run_id=?1",
        [run_id.to_string()],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    for (label, value) in [
        ("eligible", eligible),
        ("settled", settled),
        ("refused", refused),
        ("recovery_required", recovery),
        ("unattempted", unattempted),
        ("intent_committed", intent),
    ] {
        let value = u32::try_from(value).map_err(|_| {
            DaemonError::Store(format!(
                "settlement {label} count is negative or exceeds u32"
            ))
        })?;
        if value > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS as u32 {
            return Err(DaemonError::Store(format!(
                "settlement {label} count exceeds {SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS}"
            )));
        }
    }
    let (state, terminal) = source_worktree_settlement_run_projection(
        eligible,
        settled,
        refused,
        recovery,
        unattempted,
        intent,
    );
    conn.execute(
        "UPDATE source_worktree_settlement_runs SET state=?1,settled_count=?2,
                refused_count=?3,recovery_required_count=?4,unattempted_count=?5,
                updated_at=?6,finished_at=CASE WHEN ?7 THEN ?6 ELSE NULL END
         WHERE run_id=?8",
        params![
            state,
            settled,
            refused,
            recovery,
            unattempted,
            now,
            terminal,
            run_id.to_string()
        ],
    )?;
    Ok(())
}

fn source_worktree_settlement_run_projection(
    eligible: i64,
    settled: i64,
    refused: i64,
    recovery: i64,
    unattempted: i64,
    intent: i64,
) -> (&'static str, bool) {
    let terminal_count = settled + refused + recovery + unattempted;
    let terminal = terminal_count == eligible;
    let state = if terminal && recovery > 0 {
        "recovery_required"
    } else if terminal && settled == eligible {
        "settled"
    } else if terminal && settled == 0 {
        "refused"
    } else if terminal_count > 0 {
        "partial"
    } else if intent == eligible {
        "intent_committed"
    } else {
        "applying"
    };
    (state, terminal)
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_phase(value: &str) -> std::result::Result<SourceWorktreeSettlementPhaseV1, String> {
    value.parse()
}

fn parse_run_state(value: &str) -> std::result::Result<SourceWorktreeSettlementRunStateV1, String> {
    value.parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LATEST_SCHEMA_VERSION;
    use rsi_common::types::SessionStatus;

    #[test]
    fn v94_catalog_is_retained_below_the_current_schema_head() {
        let store = Store::open_in_memory_via_migrations_for_test().expect("blank to schema head");
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, LATEST_SCHEMA_VERSION);
        let tx = store.conn.unchecked_transaction().expect("catalog tx");
        validate_v95_catalog(&tx).expect("complete V95 catalog");
        assert_eq!(
            v94_catalog_fingerprint(&tx).expect("fingerprint V95 settlement catalog"),
            V95_SETTLEMENT_CATALOG_FINGERPRINT
        );
        tx.rollback().expect("rollback read tx");
    }

    #[test]
    fn v94_journal_rejects_delete_and_phase_regression() {
        let store = Store::open_in_memory().expect("store");
        assert!(
            store
                .conn
                .execute("DELETE FROM source_worktree_settlement_runs", [])
                .is_ok(),
            "empty delete fires no row trigger"
        );
        let trigger_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'source_worktree_settlement_%'",
                [],
                |row| row.get(0),
            )
            .expect("triggers");
        assert_eq!(trigger_count, 27);
    }

    #[test]
    fn ordinary_archived_session_still_unarchives_without_a_settlement_receipt() {
        let store = Store::open_in_memory().expect("store");
        let mut session = crate::store::tests::make_test_session();
        session.id = Uuid::new_v4();
        session.status = SessionStatus::Archived;
        session.pending_archive = true;
        store
            .insert_session(&session)
            .expect("insert archived Session");

        let unarchived = store
            .unarchive_session(session.id)
            .expect("ordinary unarchive succeeds")
            .expect("ordinary Session remains");
        assert_eq!(unarchived.status, SessionStatus::Completed);
        assert!(!unarchived.pending_archive);
    }

    struct QuarantineFixture {
        store: Store,
        run_id: Uuid,
        session_id: Uuid,
        custody_id: Uuid,
        repository_identity: String,
        canonical_repo_dir: String,
        sandbox_root: PathBuf,
    }

    fn quarantine_fixture(sandbox_base: &Path, key: &str) -> QuarantineFixture {
        use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
        use rsi_common::types::{SandboxCleanupState, SandboxKind};

        let mut store = Store::open_in_memory().expect("quarantine Store fixture");
        let run_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let custody_id = Uuid::new_v4();
        let repository_identity = format!("repo:quarantine-store:{key}");
        let canonical_repo_dir = format!("/tmp/quarantine-store-repo-{key}");
        let sandbox_root = sandbox_base.join(session_id.to_string());
        let sandbox_branch = format!("rsi/quarantine/{key}/{session_id}");
        let mut session = crate::store::tests::make_test_session();
        session.id = session_id;
        session.status = SessionStatus::Completed;
        session.working_dir = PathBuf::from(&canonical_repo_dir);
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(sandbox_root.clone());
        session.sandbox_branch = Some(sandbox_branch.clone());
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &session,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id,
                    canonical_repo_dir: canonical_repo_dir.clone(),
                    sandbox_root: sandbox_root
                        .to_str()
                        .expect("UTF-8 quarantine sandbox")
                        .into(),
                    sandbox_branch: sandbox_branch.clone(),
                    repository_identity: repository_identity.clone(),
                    source_commit: "a".repeat(40),
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .expect("insert quarantine custody");
        let original_updated_at: String = store
            .conn
            .query_row(
                "SELECT updated_at FROM sessions WHERE id=?1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .expect("read exact persisted Session timestamp");
        store
            .insert_source_worktree_settlement_run(&NewSettlementRun {
                run_id,
                repository_identity: repository_identity.clone(),
                canonical_repo_dir: canonical_repo_dir.clone(),
                target_ref: "refs/heads/main".into(),
                target_oid: "b".repeat(40),
                plan_digest: format!("sha256:{}", "e".repeat(64)),
                idempotency_key: key.into(),
                authorization_digest: format!("sha256:{}", "f".repeat(64)),
                request_fingerprint: format!("sha256:{}", "1".repeat(64)),
                observed_count: 1,
                retained_count: 0,
                items: vec![NewSettlementItem {
                    session_id,
                    original_status: "Completed".into(),
                    original_updated_at,
                    custody_id,
                    custody_generation: 1,
                    canonical_repo_dir: canonical_repo_dir.clone(),
                    sandbox_root: sandbox_root
                        .to_str()
                        .expect("UTF-8 quarantine sandbox")
                        .into(),
                    sandbox_branch,
                    repository_identity: repository_identity.clone(),
                    source_ref: format!("refs/heads/rsi/quarantine/{key}/{session_id}"),
                    source_oid: "a".repeat(40),
                    target_oid: "b".repeat(40),
                    evidence_digest: format!("sha256:{}", "c".repeat(64)),
                    clean_state_digest: format!("sha256:{}", "d".repeat(64)),
                    reserved_effects: 0,
                    active_effects: 0,
                    participant_count: 1,
                }],
            })
            .expect("insert quarantine journal");
        QuarantineFixture {
            store,
            run_id,
            session_id,
            custody_id,
            repository_identity,
            canonical_repo_dir,
            sandbox_root,
        }
    }

    fn synthetic_run_template(fixture: &QuarantineFixture) -> NewSettlementRun {
        let receipt = fixture
            .store
            .get_source_worktree_settlement_run(fixture.run_id)
            .expect("read template receipt")
            .expect("template receipt");
        let item = fixture
            .store
            .list_source_worktree_settlement_journal_items(fixture.run_id)
            .expect("read template journal")
            .pop()
            .expect("template journal item");
        NewSettlementRun {
            run_id: Uuid::new_v4(),
            repository_identity: receipt.repository_identity,
            canonical_repo_dir: receipt.canonical_repo_dir,
            target_ref: receipt.target_ref,
            target_oid: receipt.target_oid.as_str().to_string(),
            plan_digest: receipt.plan_digest.as_str().to_string(),
            idempotency_key: "synthetic-template".into(),
            authorization_digest: format!("sha256:{}", "2".repeat(64)),
            request_fingerprint: format!("sha256:{}", "3".repeat(64)),
            observed_count: receipt.counts.observed,
            retained_count: receipt.counts.retained,
            items: vec![NewSettlementItem {
                session_id: item.session_id,
                original_status: item.original_status,
                original_updated_at: item.original_updated_at,
                custody_id: item.custody_id,
                custody_generation: item.custody_generation,
                canonical_repo_dir: item.canonical_repo_dir,
                sandbox_root: item.sandbox_root,
                sandbox_branch: item.sandbox_branch,
                repository_identity: item.repository_identity,
                source_ref: item.source_ref,
                source_oid: item.source_oid,
                target_oid: item.target_oid,
                evidence_digest: item.evidence_digest,
                clean_state_digest: item.clean_state_digest,
                reserved_effects: item.reserved_effects,
                active_effects: item.active_effects,
                participant_count: item.participant_count,
            }],
        }
    }

    fn insert_two_item_run_from_template(
        store: &Store,
        template_run_id: Uuid,
        run_id: Uuid,
        idempotency_key: &str,
    ) {
        let inserted = store
            .conn
            .execute(
                "INSERT INTO source_worktree_settlement_runs (
                    run_id,schema_version,policy_version,repository_identity,canonical_repo_dir,
                    target_ref,target_oid,plan_digest,idempotency_key,authorization_digest,
                    request_fingerprint,state,observed_count,eligible_count,retained_count,
                    settled_count,refused_count,recovery_required_count,unattempted_count,
                    created_at,updated_at,finished_at,terminal_error
                 )
                 SELECT ?1,schema_version,policy_version,repository_identity,canonical_repo_dir,
                        target_ref,target_oid,plan_digest,?2,authorization_digest,?3,
                        'intent_committed',2,2,0,0,0,0,0,created_at,updated_at,NULL,NULL
                   FROM source_worktree_settlement_runs WHERE run_id=?4",
                params![
                    run_id.to_string(),
                    idempotency_key,
                    format!("sha256:{}", "7".repeat(64)),
                    template_run_id.to_string()
                ],
            )
            .expect("insert direct-SQL two-item settlement run");
        assert_eq!(inserted, 1);
    }

    fn insert_item_from_template(
        store: &Store,
        template_run_id: Uuid,
        run_id: Uuid,
        sequence: u32,
        session_id: Uuid,
    ) -> rusqlite::Result<usize> {
        store.conn.execute(
            "INSERT INTO source_worktree_settlement_items (
                run_id,sequence,session_id,original_status,original_updated_at,custody_id,
                custody_generation,canonical_repo_dir,sandbox_root,sandbox_branch,
                repository_identity,source_ref,source_oid,target_oid,evidence_digest,
                clean_state_digest,reserved_effects,active_effects,participant_count,
                phase,before_observation,after_observation,refusal_code,created_at,updated_at
             )
             SELECT ?1,?2,?3,original_status,original_updated_at,custody_id,
                    custody_generation,canonical_repo_dir,sandbox_root,sandbox_branch,
                    repository_identity,source_ref,source_oid,target_oid,evidence_digest,
                    clean_state_digest,reserved_effects,active_effects,participant_count,
                    phase,before_observation,after_observation,refusal_code,created_at,updated_at
               FROM source_worktree_settlement_items
              WHERE run_id=?4 AND sequence=0",
            params![
                run_id.to_string(),
                sequence,
                session_id.to_string(),
                template_run_id.to_string()
            ],
        )
    }

    #[test]
    fn latest_settlement_run_is_durable_and_repository_scoped() {
        let directory = tempfile::tempdir().expect("latest-run fixture directory");
        let mut fixture = quarantine_fixture(directory.path(), "latest-run");
        assert!(
            fixture
                .store
                .latest_source_worktree_settlement_run("repo:unrelated")
                .expect("query unrelated repository")
                .is_none()
        );
        let mut other_repository = synthetic_run_template(&fixture);
        other_repository.run_id = Uuid::new_v4();
        other_repository.repository_identity = "repo:independent-latest-run".into();
        other_repository.canonical_repo_dir = "/tmp/independent-latest-run".into();
        other_repository.idempotency_key = "independent-latest-run".into();
        other_repository.request_fingerprint = format!("sha256:{}", "8".repeat(64));
        other_repository.items[0].repository_identity =
            other_repository.repository_identity.clone();
        other_repository.items[0].canonical_repo_dir = other_repository.canonical_repo_dir.clone();
        fixture
            .store
            .insert_source_worktree_settlement_run(&other_repository)
            .expect("insert independent repository receipt");
        assert_eq!(
            fixture
                .store
                .latest_source_worktree_settlement_run(&fixture.repository_identity)
                .expect("load original repository pointer")
                .expect("original pointer")
                .run_id,
            fixture.run_id
        );

        let first = fixture
            .store
            .get_source_worktree_settlement_run(fixture.run_id)
            .expect("read first receipt")
            .expect("first receipt");
        let item = fixture
            .store
            .list_source_worktree_settlement_journal_items(fixture.run_id)
            .expect("read first journal")
            .pop()
            .expect("first journal item");
        fixture
            .store
            .mark_source_worktree_settlement_refused(
                fixture.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "custody_drift",
                "closed without effects",
            )
            .expect("close first receipt without a durable custody fence");
        let second_run_id = Uuid::from_u128(u128::MAX);
        let second = NewSettlementRun {
            run_id: second_run_id,
            repository_identity: first.repository_identity.clone(),
            canonical_repo_dir: first.canonical_repo_dir.clone(),
            target_ref: first.target_ref.clone(),
            target_oid: first.target_oid.as_str().to_string(),
            plan_digest: first.plan_digest.as_str().to_string(),
            idempotency_key: "latest-run:newer".into(),
            authorization_digest: format!("sha256:{}", "2".repeat(64)),
            request_fingerprint: format!("sha256:{}", "3".repeat(64)),
            observed_count: first.counts.observed,
            retained_count: first.counts.retained,
            items: vec![NewSettlementItem {
                session_id: item.session_id,
                original_status: item.original_status,
                original_updated_at: item.original_updated_at,
                custody_id: item.custody_id,
                custody_generation: item.custody_generation,
                canonical_repo_dir: item.canonical_repo_dir,
                sandbox_root: item.sandbox_root,
                sandbox_branch: item.sandbox_branch,
                repository_identity: item.repository_identity,
                source_ref: item.source_ref,
                source_oid: item.source_oid,
                target_oid: item.target_oid,
                evidence_digest: item.evidence_digest,
                clean_state_digest: item.clean_state_digest,
                reserved_effects: item.reserved_effects,
                active_effects: item.active_effects,
                participant_count: item.participant_count,
            }],
        };
        assert!(matches!(
            fixture
                .store
                .insert_source_worktree_settlement_run(&second)
                .expect("insert newer durable receipt"),
            InsertSettlementRunOutcome::Inserted
        ));

        let latest = fixture
            .store
            .latest_source_worktree_settlement_run(&fixture.repository_identity)
            .expect("read latest durable receipt")
            .expect("latest receipt");
        assert_eq!(latest.run_id, second_run_id);
        assert_eq!(latest.repository_identity, fixture.repository_identity);
        assert_eq!(
            fixture
                .store
                .latest_source_worktree_settlement_run(&other_repository.repository_identity)
                .expect("load independent repository pointer")
                .expect("independent receipt")
                .run_id,
            other_repository.run_id
        );
    }

    #[test]
    fn latest_run_pointer_rejects_delete_regression_bounds_and_bad_association() {
        let directory = tempfile::tempdir().expect("latest-run pointer guards directory");
        let fixture = quarantine_fixture(directory.path(), "pointer-guards");
        let (run_id, generation): (String, i64) = fixture
            .store
            .conn
            .query_row(
                "SELECT run_id,generation FROM source_worktree_settlement_latest_runs
                  WHERE repository_identity=?1",
                [&fixture.repository_identity],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("load guarded latest-run pointer");
        assert_eq!(run_id, fixture.run_id.to_string());
        assert_eq!(generation, 1);
        assert!(
            fixture
                .store
                .conn
                .execute(
                    "DELETE FROM source_worktree_settlement_latest_runs
                      WHERE repository_identity=?1",
                    [&fixture.repository_identity],
                )
                .is_err()
        );
        assert!(
            fixture
                .store
                .conn
                .execute(
                    "UPDATE source_worktree_settlement_latest_runs
                        SET generation=generation+2
                      WHERE repository_identity=?1",
                    [&fixture.repository_identity],
                )
                .is_err()
        );
        assert!(
            fixture
                .store
                .conn
                .execute(
                    "UPDATE source_worktree_settlement_latest_runs SET generation=generation
                      WHERE repository_identity=?1",
                    [&fixture.repository_identity],
                )
                .is_err()
        );
        assert!(
            fixture
                .store
                .conn
                .execute(
                    "UPDATE source_worktree_settlement_latest_runs
                        SET canonical_repo_dir=?1,generation=generation+1
                      WHERE repository_identity=?2",
                    params![
                        rusqlite::types::Value::Blob(vec![0xff]),
                        fixture.repository_identity
                    ],
                )
                .is_err()
        );
        assert!(
            fixture
                .store
                .conn
                .execute(
                    "INSERT INTO source_worktree_settlement_latest_runs (
                        repository_identity,canonical_repo_dir,run_id,generation
                     ) VALUES ('repo:wrong-association',?1,?2,1)",
                    params![fixture.canonical_repo_dir, fixture.run_id.to_string()],
                )
                .is_err()
        );
    }

    #[test]
    fn latest_run_pointer_cannot_oscillate_or_rewrite_immutable_run_order() {
        let directory = tempfile::tempdir().expect("latest-run order guard directory");
        let mut fixture = quarantine_fixture(directory.path(), "pointer-oscillation");
        let first_run_id = fixture.run_id;
        let mut second = synthetic_run_template(&fixture);
        fixture
            .store
            .mark_source_worktree_settlement_refused(
                first_run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "custody_drift",
                "closed without effects",
            )
            .expect("close first run without effects");
        second.run_id = Uuid::new_v4();
        second.idempotency_key = "pointer-oscillation:second".into();
        second.request_fingerprint = format!("sha256:{}", "9".repeat(64));
        fixture
            .store
            .insert_source_worktree_settlement_run(&second)
            .expect("append second repository run");

        let pointer = || {
            fixture
                .store
                .conn
                .query_row(
                    "SELECT run_id,generation FROM source_worktree_settlement_latest_runs
                      WHERE repository_identity=?1",
                    [&fixture.repository_identity],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("read latest-run pointer")
        };
        assert_eq!(pointer(), (second.run_id.to_string(), 2));

        for (run_id, generation, label) in [
            (first_run_id, 1_i64, "regress"),
            (first_run_id, 3_i64, "reuse an old run at a new generation"),
            (second.run_id, 4_i64, "skip a generation"),
        ] {
            let error = fixture
                .store
                .conn
                .execute(
                    "UPDATE source_worktree_settlement_latest_runs
                        SET run_id=?1,generation=?2
                      WHERE repository_identity=?3",
                    params![run_id.to_string(), generation, fixture.repository_identity],
                )
                .expect_err(label);
            assert!(
                error.to_string().contains("settlement_latest_run"),
                "unexpected {label} error: {error}"
            );
            assert_eq!(pointer(), (second.run_id.to_string(), 2));
        }
        assert!(
            fixture
                .store
                .conn
                .execute(
                    "INSERT OR REPLACE INTO source_worktree_settlement_latest_runs (
                        repository_identity,canonical_repo_dir,run_id,generation
                     ) VALUES (?1,?2,?3,1)",
                    params![
                        fixture.repository_identity,
                        fixture.canonical_repo_dir,
                        first_run_id.to_string()
                    ],
                )
                .is_err(),
            "replacement insert regressed the latest-run pointer"
        );
        assert_eq!(pointer(), (second.run_id.to_string(), 2));

        assert!(
            fixture
                .store
                .conn
                .execute(
                    "UPDATE source_worktree_settlement_run_order
                        SET generation=3 WHERE run_id=?1",
                    [first_run_id.to_string()],
                )
                .is_err(),
            "immutable per-run generation accepted an update"
        );
        assert!(
            fixture
                .store
                .conn
                .execute(
                    "DELETE FROM source_worktree_settlement_run_order WHERE run_id=?1",
                    [first_run_id.to_string()],
                )
                .is_err(),
            "immutable per-run generation accepted a delete"
        );
        assert!(
            fixture
                .store
                .conn
                .execute(
                    "INSERT OR REPLACE INTO source_worktree_settlement_run_order (
                        run_id,repository_identity,canonical_repo_dir,generation
                     ) VALUES (?1,?2,?3,3)",
                    params![
                        first_run_id.to_string(),
                        fixture.repository_identity,
                        fixture.canonical_repo_dir
                    ],
                )
                .is_err(),
            "replacement insert rewrote an immutable per-run generation"
        );
        assert_eq!(pointer(), (second.run_id.to_string(), 2));
    }

    #[test]
    fn custody_fence_query_uses_the_partial_repository_index() {
        let directory = tempfile::tempdir().expect("custody fence query-plan directory");
        let fixture = quarantine_fixture(directory.path(), "custody-plan");
        let mut statement = fixture
            .store
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT EXISTS(
                     SELECT 1
                       FROM source_worktree_settlement_items AS i
                      WHERE i.repository_identity=?1 AND i.custody_id=?2
                        AND i.phase NOT IN ('refused','unattempted')
                      LIMIT 1
                 )",
            )
            .expect("prepare custody fence query plan");
        let details = statement
            .query_map(
                params![fixture.repository_identity, fixture.custody_id.to_string()],
                |row| row.get::<_, String>(3),
            )
            .expect("explain custody fence lookup")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect custody fence query plan");
        assert!(
            details.iter().any(|detail| detail
                .contains("idx_source_worktree_settlement_items_repository_custody_fence")),
            "partial custody fence index absent from query plan: {details:?}"
        );
    }

    #[test]
    fn duplicate_custody_ids_in_one_run_are_rejected_before_persistence() {
        let directory = tempfile::tempdir().expect("duplicate custody directory");
        let mut fixture = quarantine_fixture(directory.path(), "duplicate-custody");
        let mut duplicate = synthetic_run_template(&fixture);
        duplicate.run_id = Uuid::new_v4();
        duplicate.idempotency_key = "duplicate-custody:second".into();
        duplicate.request_fingerprint = format!("sha256:{}", "8".repeat(64));
        duplicate.observed_count = 2;
        duplicate.items.push(duplicate.items[0].clone());

        let error = match fixture
            .store
            .insert_source_worktree_settlement_run(&duplicate)
        {
            Err(error) => error,
            Ok(_) => panic!("duplicate custody authority must be rejected"),
        };
        assert!(error.to_string().contains("duplicate custody id"));
        assert!(
            fixture
                .store
                .get_source_worktree_settlement_run(duplicate.run_id)
                .expect("probe rejected duplicate run")
                .is_none()
        );
    }

    #[test]
    fn v95_catalog_trigger_rejects_direct_sql_duplicate_custody_authority() {
        let directory = tempfile::tempdir().expect("direct duplicate custody directory");
        let fixture = quarantine_fixture(directory.path(), "direct-duplicate-custody");
        let duplicate_run_id = Uuid::new_v4();
        let mut second_session = crate::store::tests::make_test_session();
        second_session.id = Uuid::new_v4();
        second_session.status = SessionStatus::Completed;
        fixture
            .store
            .insert_session(&second_session)
            .expect("insert second direct-SQL settlement Session");
        insert_two_item_run_from_template(
            &fixture.store,
            fixture.run_id,
            duplicate_run_id,
            "direct-duplicate-custody:second",
        );
        insert_item_from_template(
            &fixture.store,
            fixture.run_id,
            duplicate_run_id,
            0,
            fixture.session_id,
        )
        .expect("insert first direct-SQL settlement item");

        let error = insert_item_from_template(
            &fixture.store,
            fixture.run_id,
            duplicate_run_id,
            1,
            second_session.id,
        )
        .expect_err("catalog trigger must reject duplicate custody authority");
        assert!(
            error
                .to_string()
                .contains("source_worktree_settlement_item_duplicate_custody"),
            "unexpected direct-SQL duplicate error: {error}"
        );
        assert_eq!(
            fixture
                .store
                .conn
                .query_row(
                    "SELECT count(*) FROM source_worktree_settlement_items WHERE run_id=?1",
                    [duplicate_run_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count retained direct-SQL settlement items"),
            1
        );
    }

    #[test]
    fn v95_migration_rejects_authentic_v94_duplicate_custody_authority() {
        let directory = tempfile::tempdir().expect("V94 duplicate custody directory");
        let fixture = quarantine_fixture(directory.path(), "v94-duplicate-custody");
        let tx = fixture
            .store
            .conn
            .unchecked_transaction()
            .expect("open V95-to-V94 fixture transaction");
        restore_v94_hardening(&tx).expect("restore exact V94 settlement hardening");
        tx.execute("PRAGMA user_version=94", [])
            .expect("restore V94 user version");
        tx.commit().expect("commit exact V94 fixture");

        let duplicate_run_id = Uuid::new_v4();
        let mut second_session = crate::store::tests::make_test_session();
        second_session.id = Uuid::new_v4();
        second_session.status = SessionStatus::Completed;
        fixture
            .store
            .insert_session(&second_session)
            .expect("insert second V94 settlement Session");
        insert_two_item_run_from_template(
            &fixture.store,
            fixture.run_id,
            duplicate_run_id,
            "v94-duplicate-custody:second",
        );
        insert_item_from_template(
            &fixture.store,
            fixture.run_id,
            duplicate_run_id,
            0,
            fixture.session_id,
        )
        .expect("insert first V94 settlement item");
        insert_item_from_template(
            &fixture.store,
            fixture.run_id,
            duplicate_run_id,
            1,
            second_session.id,
        )
        .expect("V94 admits the duplicate used to test migration preflight");

        let error = fixture
            .store
            .init_schema()
            .expect_err("V95 migration must reject duplicate V94 custody authority");
        assert!(
            error
                .to_string()
                .contains("duplicate custody authority within a V94 run"),
            "unexpected V95 duplicate-custody migration error: {error}"
        );
        assert_eq!(
            fixture
                .store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .expect("read rolled-back V94 user version"),
            94
        );
        let tx = fixture
            .store
            .conn
            .unchecked_transaction()
            .expect("authenticate rolled-back V94 settlement catalog");
        validate_v94_catalog(&tx).expect("failed V95 migration retained exact V94 catalog");
        tx.rollback().expect("close V94 authentication transaction");
    }

    #[test]
    fn fenced_or_settled_root_cannot_advance_pointer_until_a_new_root_exists() {
        use crate::store::sandbox_custody::{NewCustodyRoot, SessionCustodyBinding};
        use rsi_common::types::{SandboxCleanupState, SandboxKind};

        let directory = tempfile::tempdir().expect("settlement fence fixture directory");
        let mut fixture = quarantine_fixture(directory.path(), "later-run-fence");
        let mut later = synthetic_run_template(&fixture);
        later.run_id = Uuid::new_v4();
        later.idempotency_key = "later-run:fenced".into();
        later.request_fingerprint = format!("sha256:{}", "6".repeat(64));
        let fenced = match fixture.store.insert_source_worktree_settlement_run(&later) {
            Err(error) => error,
            Ok(_) => panic!("intent-committed custody must fence a later run"),
        };
        assert!(fenced.to_string().contains("earlier durable run"));

        fixture
            .store
            .advance_source_worktree_settlement_item(
                fixture.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementPhaseV1::WorktreeRemoved,
                None,
                Some("worktree removed"),
                None,
            )
            .expect("advance original run to worktree removed");
        fixture
            .store
            .advance_source_worktree_settlement_item(
                fixture.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::WorktreeRemoved,
                SourceWorktreeSettlementPhaseV1::BranchRemoved,
                None,
                Some("branch removed"),
                None,
            )
            .expect("advance original run to branch removed");
        fixture
            .store
            .finalize_source_worktree_settlement_item(fixture.run_id, fixture.session_id)
            .expect("settle original custody root");
        let settled = match fixture.store.insert_source_worktree_settlement_run(&later) {
            Err(error) => error,
            Ok(_) => panic!("settled custody must remain permanently fenced"),
        };
        assert!(settled.to_string().contains("earlier durable run"));

        let new_session_id = Uuid::new_v4();
        let new_custody_id = Uuid::new_v4();
        let new_root = directory.path().join("new-root");
        let new_branch = format!("rsi/new-root/{new_session_id}");
        let mut session = crate::store::tests::make_test_session();
        session.id = new_session_id;
        session.status = SessionStatus::Completed;
        session.working_dir = PathBuf::from(&fixture.canonical_repo_dir);
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(new_root.clone());
        session.sandbox_branch = Some(new_branch.clone());
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        fixture
            .store
            .insert_session_with_custody(
                &session,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id: new_custody_id,
                    canonical_repo_dir: fixture.canonical_repo_dir.clone(),
                    sandbox_root: new_root.to_string_lossy().into_owned(),
                    sandbox_branch: new_branch.clone(),
                    repository_identity: fixture.repository_identity.clone(),
                    source_commit: "a".repeat(40),
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .expect("insert newly created custody root");
        let original_updated_at: String = fixture
            .store
            .conn
            .query_row(
                "SELECT updated_at FROM sessions WHERE id=?1",
                [new_session_id.to_string()],
                |row| row.get(0),
            )
            .expect("load new Session timestamp");
        later.run_id = Uuid::new_v4();
        later.idempotency_key = "later-run:new-root".into();
        later.request_fingerprint = format!("sha256:{}", "7".repeat(64));
        let item = &mut later.items[0];
        item.session_id = new_session_id;
        item.original_updated_at = original_updated_at;
        item.custody_id = new_custody_id;
        item.canonical_repo_dir = fixture.canonical_repo_dir.clone();
        item.sandbox_root = new_root.to_string_lossy().into_owned();
        item.sandbox_branch = new_branch.clone();
        item.repository_identity = fixture.repository_identity.clone();
        item.source_ref = format!("refs/heads/{new_branch}");
        assert!(matches!(
            fixture
                .store
                .insert_source_worktree_settlement_run(&later)
                .expect("new custody root permits a later repository run"),
            InsertSettlementRunOutcome::Inserted
        ));
        assert_eq!(
            fixture
                .store
                .latest_source_worktree_settlement_run(&fixture.repository_identity)
                .expect("load advanced repository pointer")
                .expect("advanced receipt")
                .run_id,
            later.run_id
        );
    }

    #[test]
    fn run_backed_cohort_remains_listed_after_its_live_custody_is_gone() {
        let directory = tempfile::tempdir().expect("run-backed cohort fixture directory");
        let mut fixture = quarantine_fixture(directory.path(), "run-backed-list");
        fixture
            .store
            .tombstone_custody_root(fixture.custody_id, 1, CustodyCause::Purge)
            .expect("tombstone fixture custody root");

        let summaries = fixture
            .store
            .list_source_worktree_cohorts()
            .expect("list receipt-backed cohorts");
        let summary = summaries
            .iter()
            .find(|summary| summary.repository_identity == fixture.repository_identity)
            .expect("receipt-backed zero-Live cohort");
        assert_eq!(summary.canonical_repo_dir, fixture.canonical_repo_dir);
        assert_eq!(summary.live_roots, 0);
        assert_eq!(summary.terminal_roots, 0);
    }

    #[test]
    fn live_and_historical_cohort_union_is_strictly_bounded() {
        let directory = tempfile::tempdir().expect("bounded cohort fixture directory");
        let mut fixture = quarantine_fixture(directory.path(), "bounded-list");
        let template = synthetic_run_template(&fixture);
        for index in 0..SOURCE_WORKTREE_COHORT_LIST_MAX {
            let mut run = template.clone();
            run.run_id = Uuid::from_u128(index as u128 + 1);
            run.repository_identity = format!("repo:historical:{index:04}");
            run.canonical_repo_dir = format!("/tmp/historical-{index:04}");
            run.idempotency_key = format!("historical:{index}");
            run.request_fingerprint = format!("sha256:{index:064x}");
            run.items[0].repository_identity = run.repository_identity.clone();
            run.items[0].canonical_repo_dir = run.canonical_repo_dir.clone();
            fixture
                .store
                .insert_source_worktree_settlement_run(&run)
                .expect("insert bounded historical run");

            if index + 2 == SOURCE_WORKTREE_COHORT_LIST_MAX {
                let summaries = fixture
                    .store
                    .list_source_worktree_cohorts()
                    .expect("list one Live plus bounded historical cohorts");
                assert_eq!(summaries.len(), SOURCE_WORKTREE_COHORT_LIST_MAX);
                assert!(summaries.iter().any(|summary| summary.live_roots == 1));
                assert_eq!(
                    summaries
                        .iter()
                        .filter(|summary| summary.live_roots == 0)
                        .count(),
                    SOURCE_WORKTREE_COHORT_LIST_MAX - 1
                );
            }
        }
        let error = fixture
            .store
            .list_source_worktree_cohorts()
            .expect_err("one Live plus too many historical cohorts must fail closed");
        assert!(error.to_string().contains("exceeds bounded maximum"));
    }

    #[test]
    fn malformed_historical_receipt_fails_closed_instead_of_returning_live_only() {
        let directory = tempfile::tempdir().expect("malformed cohort fixture directory");
        let mut fixture = quarantine_fixture(directory.path(), "malformed-list");
        let mut historical = synthetic_run_template(&fixture);
        historical.run_id = Uuid::from_u128(u128::MAX - 1);
        historical.repository_identity = "repo:historical:malformed".into();
        historical.canonical_repo_dir = "/tmp/historical-malformed".into();
        historical.idempotency_key = "historical:malformed".into();
        historical.request_fingerprint = format!("sha256:{}", "4".repeat(64));
        historical.items[0].repository_identity = historical.repository_identity.clone();
        historical.items[0].canonical_repo_dir = historical.canonical_repo_dir.clone();
        fixture
            .store
            .insert_source_worktree_settlement_run(&historical)
            .expect("insert historical receipt");
        fixture
            .store
            .mark_source_worktree_settlement_refused(
                historical.run_id,
                historical.items[0].session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "custody_drift",
                "closed without effects",
            )
            .expect("close older historical receipt without a fence");
        let mut newer = historical.clone();
        newer.run_id = Uuid::from_u128(u128::MAX);
        newer.idempotency_key = "historical:newer-valid".into();
        newer.request_fingerprint = format!("sha256:{}", "5".repeat(64));
        fixture
            .store
            .insert_source_worktree_settlement_run(&newer)
            .expect("insert newer valid receipt for the same repository");
        assert_eq!(
            fixture
                .store
                .latest_source_worktree_settlement_run(&historical.repository_identity)
                .expect("query newest historical receipt")
                .expect("newest historical receipt")
                .run_id,
            newer.run_id
        );
        fixture
            .store
            .conn
            .execute_batch(
                "PRAGMA foreign_keys=OFF;
                 DROP TRIGGER source_worktree_settlement_latest_runs_bounds_update;
                 DROP TRIGGER source_worktree_settlement_latest_runs_association_update;
                 DROP TRIGGER source_worktree_settlement_latest_runs_forward;",
            )
            .expect("disable only test-database corruption guards");
        fixture
            .store
            .conn
            .execute(
                "UPDATE source_worktree_settlement_latest_runs
                    SET canonical_repo_dir=?1 WHERE repository_identity=?2",
                params![
                    rusqlite::types::Value::Blob(vec![0xff]),
                    historical.repository_identity
                ],
            )
            .expect("inject malformed latest-run pointer storage");

        let error = fixture
            .store
            .list_source_worktree_cohorts()
            .expect_err("malformed historical row must fail the whole listing");
        assert!(
            error
                .to_string()
                .contains("historical source-worktree cohort summary field")
        );
    }

    fn marker(fixture: &QuarantineFixture, removal_oid_digit: char) -> QuarantineRemoveAuthorityV1 {
        let item = fixture
            .store
            .list_source_worktree_settlement_journal_items(fixture.run_id)
            .expect("load quarantine item")
            .pop()
            .expect("quarantine item");
        let run = fixture
            .store
            .get_source_worktree_settlement_run(fixture.run_id)
            .expect("load quarantine run")
            .expect("quarantine run");
        let quarantine_path = source_worktree_quarantine_path(
            &fixture.sandbox_root,
            fixture.run_id,
            fixture.session_id,
        )
        .expect("derive quarantine path");
        QuarantineRemoveAuthorityV1::new(QuarantineRemoveAuthorityFactsV1 {
            run_id: fixture.run_id,
            session_id: fixture.session_id,
            custody_id: fixture.custody_id,
            custody_generation: 1,
            original_path: fixture.sandbox_root.to_str().expect("UTF-8 original"),
            quarantine_path: quarantine_path.to_str().expect("UTF-8 quarantine"),
            repository_identity: &fixture.repository_identity,
            canonical_repo_dir: &fixture.canonical_repo_dir,
            source_ref: &item.source_ref,
            target_ref: &run.target_ref,
            admin_dir: "/tmp/quarantine-admin/worktrees/id",
            admin_id: "worktrees/id",
            source_oid: SourceWorktreeGitOidV1::parse(item.source_oid).expect("source oid"),
            journal_target_oid: SourceWorktreeGitOidV1::parse(item.target_oid)
                .expect("journal target oid"),
            removal_target_oid: SourceWorktreeGitOidV1::parse(
                removal_oid_digit.to_string().repeat(40),
            )
            .expect("removal target oid"),
            journal_evidence_digest: Sha256Digest::parse(item.evidence_digest)
                .expect("journal evidence"),
            journal_clean_digest: Sha256Digest::parse(item.clean_state_digest)
                .expect("journal clean"),
            removal_clean_digest: Sha256Digest::parse(format!("sha256:{}", "2".repeat(64)))
                .expect("removal clean"),
            root_device: 7,
            root_inode: 11,
            stable_tree_digest: Sha256Digest::parse(format!("sha256:{}", "3".repeat(64)))
                .expect("tree digest"),
            holder_evidence_digest: Sha256Digest::parse(format!("sha256:{}", "4".repeat(64)))
                .expect("holder digest"),
            holder_fixed_point_passes: 2,
            trusted_platform_exemption_count: 0,
            trusted_platform_exemption_digest: Sha256Digest::parse(format!(
                "sha256:{}",
                "5".repeat(64)
            ))
            .expect("platform digest"),
        })
        .expect("construct quarantine marker")
    }

    #[test]
    fn quarantine_marker_is_canonical_typed_bounded_and_domain_separated() {
        let directory = tempfile::tempdir().expect("marker directory");
        let fixture = quarantine_fixture(directory.path(), "marker-canonical");
        let authority_marker = marker(&fixture, 'b');
        let canonical = authority_marker
            .to_canonical_json()
            .expect("canonical marker");
        assert!(canonical.len() <= QUARANTINE_REMOVE_AUTHORITY_MAX_BYTES);
        assert_eq!(
            QuarantineRemoveAuthorityV1::parse_canonical(&canonical).expect("parse marker"),
            authority_marker
        );
        assert_ne!(
            quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::OriginalPath,
                "/same",
            ),
            quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::QuarantinePath,
                "/same",
            )
        );
        assert!(QuarantineRemoveAuthorityV1::parse_canonical(&format!(" {canonical}")).is_err());
        let unknown = canonical
            .strip_suffix('}')
            .map(|prefix| format!("{prefix},\"unknown\":1}}"))
            .expect("JSON object");
        assert!(QuarantineRemoveAuthorityV1::parse_canonical(&unknown).is_err());
        let duplicate = canonical.replacen(
            "\"schema_version\":1",
            "\"schema_version\":1,\"schema_version\":1",
            1,
        );
        assert!(QuarantineRemoveAuthorityV1::parse_canonical(&duplicate).is_err());

        let mut weak = authority_marker.clone();
        weak.holder_fixed_point_passes = 1;
        assert!(weak.to_canonical_json().is_err());
        let mut nil = authority_marker.clone();
        nil.run_id = Uuid::nil();
        assert!(nil.to_canonical_json().is_err());
        let mut zero_root = authority_marker;
        zero_root.root_device = 0;
        assert!(zero_root.to_canonical_json().is_err());
    }

    #[test]
    fn quarantine_authority_cas_replays_once_and_rejects_tamper_or_phase_drift() {
        let directory = tempfile::tempdir().expect("CAS directory");
        let mut fixture = quarantine_fixture(directory.path(), "marker-cas");
        let authority_marker = marker(&fixture, 'b');
        let canonical = authority_marker
            .to_canonical_json()
            .expect("canonical marker");
        let mut retained_fact_tamper = authority_marker.clone();
        retained_fact_tamper.original_path_digest = quarantine_remove_authority_field_digest(
            QuarantineRemoveAuthorityDigestDomainV1::OriginalPath,
            "/wrong-original",
        );
        assert!(
            fixture
                .store
                .record_source_worktree_quarantine_remove_authority(
                    fixture.run_id,
                    fixture.session_id,
                    &retained_fact_tamper,
                )
                .expect_err("retained fact tamper must fail")
                .to_string()
                .contains("retained journal facts")
        );
        fixture
            .store
            .record_source_worktree_quarantine_remove_authority(
                fixture.run_id,
                fixture.session_id,
                &authority_marker,
            )
            .expect("record marker");
        let persisted_receipt = fixture
            .store
            .get_source_worktree_settlement_run(fixture.run_id)
            .expect("load marker receipt")
            .expect("marker receipt")
            .validate_wire()
            .expect("marker receipt remains wire-valid");
        assert_eq!(
            persisted_receipt.state,
            SourceWorktreeSettlementRunStateV1::IntentCommitted
        );
        assert!(persisted_receipt.finished_at.is_none());
        let first_updated_at: String = fixture
            .store
            .conn
            .query_row(
                "SELECT updated_at FROM source_worktree_settlement_runs WHERE run_id=?1",
                [fixture.run_id.to_string()],
                |row| row.get(0),
            )
            .expect("first aggregate timestamp");
        fixture
            .store
            .record_source_worktree_quarantine_remove_authority(
                fixture.run_id,
                fixture.session_id,
                &authority_marker,
            )
            .expect("exact replay");
        let replay_updated_at: String = fixture
            .store
            .conn
            .query_row(
                "SELECT updated_at FROM source_worktree_settlement_runs WHERE run_id=?1",
                [fixture.run_id.to_string()],
                |row| row.get(0),
            )
            .expect("replay aggregate timestamp");
        assert_eq!(first_updated_at, replay_updated_at);
        assert_eq!(
            fixture
                .store
                .list_source_worktree_settlement_journal_items(fixture.run_id)
                .expect("load marker journal")[0]
                .before_observation
                .as_deref(),
            Some(canonical.as_str())
        );

        let changed = marker(&fixture, '9');
        assert!(
            fixture
                .store
                .record_source_worktree_quarantine_remove_authority(
                    fixture.run_id,
                    fixture.session_id,
                    &changed,
                )
                .expect_err("changed marker cannot overwrite")
                .to_string()
                .contains("conflicts")
        );
        fixture
            .store
            .advance_source_worktree_settlement_item(
                fixture.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired,
                Some("attempted overwrite"),
                Some("recovery"),
                Some("recovery_proof_failed"),
            )
            .expect("advance to recovery");
        let retained: String = fixture
            .store
            .conn
            .query_row(
                "SELECT before_observation FROM source_worktree_settlement_items
                  WHERE run_id=?1 AND session_id=?2",
                params![fixture.run_id.to_string(), fixture.session_id.to_string()],
                |row| row.get(0),
            )
            .expect("retained marker");
        assert_eq!(retained, canonical);
        assert!(
            fixture
                .store
                .record_source_worktree_quarantine_remove_authority(
                    fixture.run_id,
                    fixture.session_id,
                    &authority_marker,
                )
                .expect_err("later phase cannot gain authority")
                .to_string()
                .contains("intent_committed")
        );
    }

    #[test]
    fn quarantine_authority_cas_rejects_malformed_noncanonical_and_unknown_evidence() {
        let directory = tempfile::tempdir().expect("malformed CAS directory");
        for (index, retained) in ["{".to_string(), " {}".to_string()].into_iter().enumerate() {
            let mut fixture =
                quarantine_fixture(directory.path(), &format!("marker-malformed-{index}"));
            let marker = marker(&fixture, 'b');
            let retained = if index == 1 {
                format!(
                    " {}",
                    marker
                        .to_canonical_json()
                        .expect("canonical whitespace case")
                )
            } else {
                retained
            };
            fixture
                .store
                .conn
                .execute(
                    "UPDATE source_worktree_settlement_items SET before_observation=?1
                      WHERE run_id=?2 AND session_id=?3",
                    params![
                        retained,
                        fixture.run_id.to_string(),
                        fixture.session_id.to_string()
                    ],
                )
                .expect("install malformed retained evidence");
            assert!(
                fixture
                    .store
                    .record_source_worktree_quarantine_remove_authority(
                        fixture.run_id,
                        fixture.session_id,
                        &marker,
                    )
                    .expect_err("malformed evidence must conflict")
                    .to_string()
                    .contains("malformed")
            );
        }

        let mut fixture = quarantine_fixture(directory.path(), "marker-unknown");
        let marker = marker(&fixture, 'b');
        let canonical = marker.to_canonical_json().expect("canonical marker");
        let unknown = canonical
            .strip_suffix('}')
            .map(|prefix| format!("{prefix},\"unknown\":1}}"))
            .expect("JSON object");
        fixture
            .store
            .conn
            .execute(
                "UPDATE source_worktree_settlement_items SET before_observation=?1
                  WHERE run_id=?2 AND session_id=?3",
                params![
                    unknown,
                    fixture.run_id.to_string(),
                    fixture.session_id.to_string()
                ],
            )
            .expect("install unknown marker field");
        assert!(
            fixture
                .store
                .record_source_worktree_quarantine_remove_authority(
                    fixture.run_id,
                    fixture.session_id,
                    &marker,
                )
                .is_err()
        );
    }

    #[test]
    fn quarantine_marker_survives_all_later_transitions_and_finalization() {
        let directory = tempfile::tempdir().expect("transition directory");
        let mut fixture = quarantine_fixture(directory.path(), "marker-transitions");
        let marker = marker(&fixture, 'b');
        let canonical = marker.to_canonical_json().expect("canonical marker");
        fixture
            .store
            .record_source_worktree_quarantine_remove_authority(
                fixture.run_id,
                fixture.session_id,
                &marker,
            )
            .expect("record transition marker");
        for (expected, next) in [
            (
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementPhaseV1::WorktreeRemoved,
            ),
            (
                SourceWorktreeSettlementPhaseV1::WorktreeRemoved,
                SourceWorktreeSettlementPhaseV1::BranchRemoved,
            ),
        ] {
            fixture
                .store
                .advance_source_worktree_settlement_item(
                    fixture.run_id,
                    fixture.session_id,
                    expected,
                    next,
                    Some("prose overwrite attempt"),
                    Some("transition observation"),
                    None,
                )
                .expect("advance marker transition");
            let receipt = fixture
                .store
                .get_source_worktree_settlement_run(fixture.run_id)
                .expect("load effect-phase receipt")
                .expect("effect-phase receipt")
                .validate_wire()
                .expect("effect-phase receipt remains wire-valid");
            assert_eq!(receipt.state, SourceWorktreeSettlementRunStateV1::Applying);
            assert!(receipt.finished_at.is_none());
        }
        fixture
            .store
            .finalize_source_worktree_settlement_item(fixture.run_id, fixture.session_id)
            .expect("finalize marker journal");
        let (phase, retained): (String, String) = fixture
            .store
            .conn
            .query_row(
                "SELECT phase,before_observation FROM source_worktree_settlement_items
                  WHERE run_id=?1 AND session_id=?2",
                params![fixture.run_id.to_string(), fixture.session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("load finalized marker");
        assert_eq!(phase, "settled");
        assert_eq!(retained, canonical);
        let receipt = fixture
            .store
            .get_source_worktree_settlement_run(fixture.run_id)
            .expect("load terminal marker receipt")
            .expect("terminal marker receipt")
            .validate_wire()
            .expect("terminal marker receipt remains wire-valid");
        assert_eq!(receipt.state, SourceWorktreeSettlementRunStateV1::Settled);
        assert!(receipt.finished_at.is_some());
    }

    #[test]
    fn refresh_run_projection_covers_all_intent_applying_mixed_and_terminal_states() {
        for (counts, expected) in [
            ((2, 0, 0, 0, 0, 2), ("intent_committed", false)),
            ((2, 0, 0, 0, 0, 1), ("applying", false)),
            ((2, 1, 0, 0, 0, 1), ("partial", false)),
            ((2, 1, 1, 0, 0, 0), ("partial", true)),
            ((2, 0, 0, 1, 1, 0), ("recovery_required", true)),
            ((2, 0, 1, 0, 1, 0), ("refused", true)),
            ((2, 2, 0, 0, 0, 0), ("settled", true)),
        ] {
            let (eligible, settled, refused, recovery, unattempted, intent) = counts;
            assert_eq!(
                source_worktree_settlement_run_projection(
                    eligible,
                    settled,
                    refused,
                    recovery,
                    unattempted,
                    intent,
                ),
                expected,
                "counts={counts:?}"
            );
        }
    }

    fn inventory_alias(fixture: &QuarantineFixture) -> SourceWorktreeInventoryQuarantineAlias {
        SourceWorktreeInventoryQuarantineAlias {
            run_id: fixture.run_id,
            session_id: fixture.session_id,
            custody_id: fixture.custody_id,
            custody_generation: 1,
            original_path: fixture.sandbox_root.clone(),
        }
    }

    fn insert_scheduled_path_dependency(store: &Store, working_dir: &Path, name: &str) {
        let now = timestamp();
        store
            .conn
            .execute(
                "INSERT INTO scheduled_jobs (
                    id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,
                    working_dir,provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id
                 ) VALUES (?1,?2,'dependency','{}',NULL,?3,1,?4,NULL,NULL,NULL,?3,?3,'fresh',NULL)",
                params![
                    Uuid::new_v4().to_string(),
                    name,
                    now,
                    working_dir.to_str().expect("UTF-8 scheduled path"),
                ],
            )
            .expect("insert scheduled q dependency");
    }

    #[test]
    fn quarantine_inventory_alias_blocks_scheduled_and_executable_session_paths() {
        let directory = tempfile::tempdir().expect("alias directory");
        let fixture = quarantine_fixture(directory.path(), "alias-paths");
        let quarantine = source_worktree_quarantine_path(
            &fixture.sandbox_root,
            fixture.run_id,
            fixture.session_id,
        )
        .expect("derive alias quarantine");
        insert_scheduled_path_dependency(
            &fixture.store,
            &quarantine.join("scheduled/provider"),
            "q-scheduled",
        );
        let without_alias = fixture
            .store
            .source_worktree_inventory(
                &fixture.repository_identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
            )
            .expect("load audit inventory without aliases");
        assert_eq!(without_alias[0].scheduled_dependency_count, 0);

        let alias_session_id = Uuid::new_v4();
        let mut alias_session = crate::store::tests::make_test_session();
        alias_session.id = alias_session_id;
        alias_session.status = SessionStatus::Starting;
        alias_session.working_dir = quarantine.join("session/provider");
        fixture
            .store
            .insert_session(&alias_session)
            .expect("insert q-path executable Session");
        let inventory = fixture
            .store
            .source_worktree_inventory_with_path_aliases(
                &fixture.repository_identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
                &[inventory_alias(&fixture)],
            )
            .expect("load inventory with q alias");
        assert_eq!(inventory[0].scheduled_dependency_count, 1);
        assert_eq!(inventory[0].session_path_dependency_count, 1);
    }

    #[test]
    fn quarantine_inventory_alias_keeps_custody_only_dependency_blocking() {
        let directory = tempfile::tempdir().expect("custody alias directory");
        let fixture = quarantine_fixture(directory.path(), "alias-custody");
        let alias_session_id = Uuid::new_v4();
        let mut alias_session = crate::store::tests::make_test_session();
        alias_session.id = alias_session_id;
        alias_session.status = SessionStatus::Starting;
        alias_session.working_dir = directory.path().join("unrelated");
        fixture
            .store
            .insert_session(&alias_session)
            .expect("insert custody-only Session");
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET custody_id=?1,custody_generation=1
                  WHERE session_id=?2",
                params![fixture.custody_id.to_string(), alias_session_id.to_string()],
            )
            .expect("bind custody-only dependency");
        let inventory = fixture
            .store
            .source_worktree_inventory_with_path_aliases(
                &fixture.repository_identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
                &[inventory_alias(&fixture)],
            )
            .expect("load custody-only inventory");
        assert_eq!(inventory[0].session_path_dependency_count, 1);
    }

    #[test]
    fn quarantine_inventory_alias_mismatch_and_bounds_fail_closed() {
        let directory = tempfile::tempdir().expect("invalid alias directory");
        let fixture = quarantine_fixture(directory.path(), "alias-invalid");
        let alias = inventory_alias(&fixture);
        let mut generation_drift = alias.clone();
        generation_drift.custody_generation = 2;
        assert!(
            fixture
                .store
                .source_worktree_inventory_with_path_aliases(
                    &fixture.repository_identity,
                    SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
                    &[generation_drift],
                )
                .is_err()
        );
        let mut identity_drift = alias.clone();
        identity_drift.session_id = Uuid::new_v4();
        assert!(
            fixture
                .store
                .source_worktree_inventory_with_path_aliases(
                    &fixture.repository_identity,
                    SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
                    &[identity_drift],
                )
                .is_err()
        );
        assert!(
            fixture
                .store
                .source_worktree_inventory_with_path_aliases(
                    &fixture.repository_identity,
                    SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
                    &vec![alias; SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1],
                )
                .expect_err("alias count bound")
                .to_string()
                .contains("exceed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_inventory_alias_matches_canonical_q_path() {
        let directory = tempfile::tempdir().expect("canonical alias directory");
        let fixture = quarantine_fixture(directory.path(), "alias-canonical");
        let quarantine = source_worktree_quarantine_path(
            &fixture.sandbox_root,
            fixture.run_id,
            fixture.session_id,
        )
        .expect("derive canonical q");
        std::fs::create_dir_all(quarantine.join("provider")).expect("create q provider path");
        let symlink = directory.path().join("q-alias");
        std::os::unix::fs::symlink(&quarantine, &symlink).expect("create q symlink alias");
        insert_scheduled_path_dependency(&fixture.store, &symlink.join("provider"), "q-canonical");
        let inventory = fixture
            .store
            .source_worktree_inventory_with_path_aliases(
                &fixture.repository_identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
                &[inventory_alias(&fixture)],
            )
            .expect("load canonical q inventory");
        assert_eq!(inventory[0].scheduled_dependency_count, 1);
    }

    #[test]
    fn startup_orphans_match_original_q_or_both_and_q_session_aliases() {
        for (case, original_exists, quarantine_exists) in [
            ("original", true, false),
            ("quarantine", false, true),
            ("both", true, true),
        ] {
            let directory = tempfile::tempdir().expect("startup candidate directory");
            let sandbox_base = directory.path().join("sandboxes");
            std::fs::create_dir_all(&sandbox_base).expect("create sandbox base");
            let fixture = quarantine_fixture(&sandbox_base, &format!("startup-{case}"));
            let quarantine = source_worktree_quarantine_path(
                &fixture.sandbox_root,
                fixture.run_id,
                fixture.session_id,
            )
            .expect("derive startup q");
            if original_exists {
                std::fs::create_dir_all(&fixture.sandbox_root).expect("create original root");
            }
            if quarantine_exists {
                std::fs::create_dir_all(&quarantine).expect("create quarantine root");
            }
            let q_alias_id = Uuid::new_v4();
            let mut q_alias = crate::store::tests::make_test_session();
            q_alias.id = q_alias_id;
            q_alias.status = SessionStatus::Starting;
            q_alias.working_dir = quarantine.join("provider/future");
            fixture
                .store
                .insert_session(&q_alias)
                .expect("insert startup q Session alias");
            let candidates = fixture
                .store
                .source_worktree_startup_orphan_candidates(&sandbox_base)
                .expect("load startup quarantine candidates");
            assert!(candidates.contains(&fixture.session_id), "{case}");
            assert!(candidates.contains(&q_alias_id), "{case}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn startup_orphans_reject_q_symlink_and_non_directory_collision() {
        for collision in ["symlink", "file"] {
            let directory = tempfile::tempdir().expect("startup collision directory");
            let sandbox_base = directory.path().join("sandboxes");
            std::fs::create_dir_all(&sandbox_base).expect("create collision sandbox base");
            let fixture = quarantine_fixture(&sandbox_base, &format!("startup-q-{collision}"));
            let quarantine = source_worktree_quarantine_path(
                &fixture.sandbox_root,
                fixture.run_id,
                fixture.session_id,
            )
            .expect("derive collision q");
            std::fs::create_dir_all(quarantine.parent().expect("q parent"))
                .expect("create q parent");
            if collision == "symlink" {
                let outside = directory.path().join("outside");
                std::fs::create_dir_all(&outside).expect("create outside q target");
                std::os::unix::fs::symlink(outside, &quarantine).expect("create q symlink");
            } else {
                std::fs::write(&quarantine, b"collision").expect("create q file collision");
            }
            let error = fixture
                .store
                .source_worktree_startup_orphan_candidates(&sandbox_base)
                .expect_err("q collision must fail startup closed");
            assert!(
                error.to_string().contains("symbolic link")
                    || error.to_string().contains("not a directory"),
                "{collision}: {error}"
            );
        }
    }

    #[test]
    fn startup_orphans_reject_original_non_directory_collision() {
        let directory = tempfile::tempdir().expect("startup original collision directory");
        let sandbox_base = directory.path().join("sandboxes");
        std::fs::create_dir_all(&sandbox_base).expect("create original collision sandbox base");
        let fixture = quarantine_fixture(&sandbox_base, "startup-original-file");
        std::fs::write(&fixture.sandbox_root, b"collision")
            .expect("create original file collision");

        let error = fixture
            .store
            .source_worktree_startup_orphan_candidates(&sandbox_base)
            .expect_err("original file collision must fail startup closed");
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    #[test]
    fn startup_existing_original_canonicalize_error_fails_closed() {
        let directory = tempfile::tempdir().expect("startup canonicalize error directory");
        let sandbox_base = directory.path().join("sandboxes");
        let session_id = Uuid::new_v4();
        let root = sandbox_base.join(session_id.to_string());
        std::fs::create_dir_all(&root).expect("create original root");
        let canonical_base = std::fs::canonicalize(&sandbox_base).expect("canonical sandbox base");

        let error = validate_startup_settlement_root_with_canonicalizer(
            &root,
            &sandbox_base,
            Some(&canonical_base),
            session_id,
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected unreadable root",
                ))
            },
        )
        .expect_err("existing uncanonicalizable root must fail startup closed");
        assert!(
            error.to_string().contains("cannot be canonicalized"),
            "{error}"
        );
    }

    #[test]
    fn quarantine_path_growth_is_bounded() {
        let run_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let mut base = PathBuf::from("/tmp");
        while base.join(session_id.to_string()).as_os_str().len() < 4050 {
            base.push("x".repeat(20));
        }
        let original = base.join(session_id.to_string());
        assert!(original.as_os_str().len() <= STARTUP_SETTLEMENT_PATH_MAX_BYTES);
        assert!(
            source_worktree_quarantine_path(&original, run_id, session_id)
                .expect_err("derived q must respect the path bound")
                .to_string()
                .contains("exceeds")
        );
    }

    #[test]
    fn quarantine_path_rejects_wrong_or_noncanonical_session_leaf() {
        let directory = tempfile::tempdir().expect("path directory");
        let run_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        assert!(
            source_worktree_quarantine_path(
                &directory.path().join(Uuid::new_v4().to_string()),
                run_id,
                session_id,
            )
            .is_err()
        );
        assert!(
            source_worktree_quarantine_path(
                &directory
                    .path()
                    .join("component")
                    .join("..")
                    .join(session_id.to_string()),
                run_id,
                session_id,
            )
            .is_err()
        );
    }
}
