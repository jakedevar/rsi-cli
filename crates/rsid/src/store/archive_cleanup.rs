//! Durable V102 journal and V103 success projection for ordinary archive cleanup.

use super::Store;
use crate::error::{DaemonError, Result};
use crate::store::sandbox_custody::{self, CustodyCause};
use chrono::{SecondsFormat, Utc};
use rsi_common::archive_cleanup::{
    ARCHIVE_CLEANUP_SCHEMA_VERSION, ArchiveCleanupPhaseV1, ArchiveCleanupReceiptV1,
    ArchiveCleanupSafeCodeV1, ArchiveCleanupStatusV1, ArchivePreservationClassV1,
};
use rsi_common::cohort_settlement::SourceWorktreeGitOidV1;
use rsi_common::types::Sha256Digest;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::str::FromStr;
use uuid::Uuid;

pub(crate) const ARCHIVE_CLEANUP_RECOVERY_BATCH: u32 = 32;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_ARCHIVE_CLEANUP_FINAL_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_archive_cleanup_final_commit() {
    FAIL_NEXT_ARCHIVE_CLEANUP_FINAL_COMMIT.with(|flag| flag.set(true));
}

#[cfg(test)]
fn archive_cleanup_final_commit_fault() -> Result<()> {
    if FAIL_NEXT_ARCHIVE_CLEANUP_FINAL_COMMIT.with(|flag| flag.replace(false)) {
        return Err(DaemonError::Store(
            "injected archive cleanup final commit failure".into(),
        ));
    }
    Ok(())
}

#[cfg(not(test))]
fn archive_cleanup_final_commit_fault() -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct NewArchiveCleanupIntent {
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub custody_id: Uuid,
    pub custody_generation: u64,
    pub session_kind: String,
    pub session_status: String,
    pub session_updated_at: String,
    pub parent_id: Option<Uuid>,
    pub continued_from: Option<Uuid>,
    pub topology_digest: String,
    pub repository_identity: String,
    pub canonical_repo_dir: String,
    pub original_root: String,
    pub quarantine_root: String,
    pub root_device: u64,
    pub root_inode: u64,
    pub git_common_dir: String,
    pub git_admin_dir: String,
    pub git_admin_id: String,
    pub source_ref: String,
    pub source_oid: String,
    pub preservation_class: ArchivePreservationClassV1,
    pub target_ref: Option<String>,
    pub target_oid: Option<String>,
    pub clean_state_digest: String,
    pub tree_digest: String,
    pub dependency_digest: String,
    pub holder_digest: String,
    pub evidence_digest: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ArchiveCleanupRun {
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub custody_id: Uuid,
    pub custody_generation: u64,
    pub session_kind: String,
    pub session_status: String,
    pub session_updated_at: String,
    pub parent_id: Option<Uuid>,
    pub continued_from: Option<Uuid>,
    pub topology_digest: String,
    pub repository_identity: String,
    pub canonical_repo_dir: String,
    pub original_root: String,
    pub quarantine_root: String,
    pub root_device: u64,
    pub root_inode: u64,
    pub git_common_dir_digest: String,
    pub git_admin_dir_digest: String,
    pub git_admin_id: String,
    pub source_ref: String,
    pub source_oid: String,
    pub preservation_class: ArchivePreservationClassV1,
    pub target_ref: Option<String>,
    pub target_oid: Option<String>,
    pub clean_state_digest: String,
    pub tree_digest: String,
    pub dependency_digest: String,
    pub holder_digest: String,
    pub evidence_digest: String,
    pub phase: ArchiveCleanupPhaseV1,
    pub row_version: u64,
    pub safe_code: Option<ArchiveCleanupSafeCodeV1>,
    pub removal_authority_json: Option<String>,
    pub removal_authority_digest: Option<String>,
    pub receipt: Option<ArchiveCleanupReceiptV1>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveCleanupRecoveryCursor {
    pub updated_at: String,
    pub run_id: Uuid,
}

#[derive(Debug, Clone)]
pub(crate) struct ArchiveCleanupRecoveryPage {
    pub runs: Vec<ArchiveCleanupRun>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveProjectionConsumer {
    Bus,
    Watch,
    Memory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveProjectionConsumerState {
    Pending,
    Delivering,
    Delivered,
}

impl ArchiveProjectionConsumer {
    pub(crate) const ALL: [Self; 3] = [Self::Bus, Self::Watch, Self::Memory];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Bus => "bus",
            Self::Watch => "watch",
            Self::Memory => "memory",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveCleanupProjectionDelivery {
    pub projection_id: Uuid,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub pending_consumers: Vec<ArchiveProjectionConsumer>,
}

#[derive(Serialize)]
struct ArchiveProjectionContent<'a> {
    version: u32,
    projection_id: Uuid,
    run_id: Uuid,
    session_id: Uuid,
    custody_id: Uuid,
    custody_generation: u64,
    projection_kind: &'static str,
    receipt_digest: &'a str,
}

pub(crate) fn archive_projection_id(run_id: Uuid) -> Uuid {
    Uuid::new_v5(
        &run_id,
        b"rsi.archive-cleanup.success-projection/session-archived/v1",
    )
}

#[derive(Serialize)]
struct ReceiptContent<'a> {
    version: u32,
    run_id: Uuid,
    session_id: Uuid,
    custody_id: Uuid,
    custody_generation: u64,
    preservation_class: ArchivePreservationClassV1,
    source_branch: &'a str,
    source_oid: &'a str,
    target_ref: Option<&'a str>,
    target_oid: Option<&'a str>,
    phase: ArchiveCleanupPhaseV1,
    branch_preserved: bool,
    session_status: &'static str,
    custody_state: &'static str,
    cleanup_state: &'static str,
    created_at: &'a str,
    settled_at: &'a str,
}

pub(crate) fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(crate) fn digest_field(domain: &str, value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(domain.as_bytes());
    digest.update(b"\0");
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
    format!("sha256:{:x}", digest.finalize())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn archive_topology_digest(
    session_id: Uuid,
    session_kind: &str,
    session_status: &str,
    session_updated_at: &str,
    parent_id: Option<Uuid>,
    continued_from: Option<Uuid>,
    title: Option<&str>,
    agent_role: Option<&str>,
    epic_spawn_ordinal: Option<u32>,
) -> Result<String> {
    #[derive(Serialize)]
    struct Topology<'a> {
        session_id: Uuid,
        session_kind: &'a str,
        session_status: &'a str,
        session_updated_at: &'a str,
        parent_id: Option<Uuid>,
        continued_from: Option<Uuid>,
        title_digest: String,
        agent_role_digest: String,
        epic_spawn_ordinal: Option<u32>,
    }
    let canonical = serde_json::to_string(&Topology {
        session_id,
        session_kind,
        session_status,
        session_updated_at,
        parent_id,
        continued_from,
        title_digest: digest_field("archive-session-title-v1", title.unwrap_or("")),
        agent_role_digest: digest_field("archive-session-role-v1", agent_role.unwrap_or("")),
        epic_spawn_ordinal,
    })
    .map_err(|error| DaemonError::Store(error.to_string()))?;
    Ok(digest_field("archive-session-topology-v1", &canonical))
}

pub(crate) fn validate_v102_catalog(tx: &Transaction<'_>) -> Result<()> {
    let mut statement = tx.prepare(
        "SELECT type,name FROM sqlite_master
         WHERE name LIKE 'archive_cleanup_%' AND name NOT LIKE 'sqlite_autoindex_%'
         ORDER BY type,name",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected = super::V112_ARCHIVE_CATALOG_OBJECTS
        .into_iter()
        .map(|(kind, name)| (kind.to_string(), name.to_string()))
        .collect::<Vec<_>>();
    if actual != expected {
        return Err(DaemonError::Store(format!(
            "V102 archive cleanup catalog mismatch: {actual:?}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_v103_catalog(tx: &Transaction<'_>) -> Result<()> {
    let mut statement = tx.prepare(
        "SELECT type,name FROM sqlite_master
         WHERE (name LIKE 'archive_cleanup_success_projection%'
                OR name LIKE 'archive_cleanup_projection_consumer%')
           AND name NOT LIKE 'sqlite_autoindex_%'
         ORDER BY type,name",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected = super::V113_ARCHIVE_PROJECTION_CATALOG_OBJECTS
        .into_iter()
        .map(|(kind, name)| (kind.to_string(), name.to_string()))
        .collect::<Vec<_>>();
    if actual != expected {
        return Err(DaemonError::Store(format!(
            "V103 archive projection catalog mismatch: {actual:?}"
        )));
    }
    Ok(())
}

fn insert_v103_projection_on(
    tx: &Transaction<'_>,
    run_id: Uuid,
    session_id: Uuid,
    custody_id: Uuid,
    custody_generation: u64,
    receipt_digest: &str,
    delivered: bool,
    now: &str,
) -> Result<Uuid> {
    let projection_id = archive_projection_id(run_id);
    let canonical_json = serde_json::to_string(&ArchiveProjectionContent {
        version: 1,
        projection_id,
        run_id,
        session_id,
        custody_id,
        custody_generation,
        projection_kind: "session_archived",
        receipt_digest,
    })
    .map_err(|error| DaemonError::Store(error.to_string()))?;
    let projection_digest = digest_field("archive-cleanup-success-projection-v1", &canonical_json);
    let delivery_state = if delivered { "delivered" } else { "pending" };
    let delivered_at = delivered.then_some(now);
    tx.execute(
        "INSERT INTO archive_cleanup_success_projections(
             projection_id,run_id,session_id,custody_id,custody_generation,
             projection_kind,schema_version,canonical_json,projection_digest,receipt_digest,
             delivery_state,row_version,attempt_count,first_attempt_at,last_attempt_at,
             delivered_at,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,'session_archived',1,?6,?7,?8,?9,1,0,NULL,NULL,?10,?11,?11)",
        params![
            projection_id.to_string(),
            run_id.to_string(),
            session_id.to_string(),
            custody_id.to_string(),
            custody_generation as i64,
            canonical_json,
            projection_digest,
            receipt_digest,
            delivery_state,
            delivered_at,
            now,
        ],
    )?;
    for consumer in ArchiveProjectionConsumer::ALL {
        tx.execute(
            "INSERT INTO archive_cleanup_projection_consumers(
                 projection_id,consumer_kind,delivery_state,row_version,attempt_count,
                 first_attempt_at,last_attempt_at,delivered_at,created_at,updated_at)
             VALUES(?1,?2,?3,1,0,NULL,NULL,?4,?5,?5)",
            params![
                projection_id.to_string(),
                consumer.as_str(),
                delivery_state,
                delivered_at,
                now,
            ],
        )?;
    }
    Ok(projection_id)
}

pub(crate) fn backfill_v103_settled_projections(tx: &Transaction<'_>) -> Result<()> {
    let rows = {
        let mut statement = tx.prepare(
            "SELECT run_id,session_id,custody_id,custody_generation,receipt_digest,settled_at
             FROM archive_cleanup_runs
             WHERE phase='settled'
             ORDER BY run_id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (run_id, session_id, custody_id, generation, receipt_digest, settled_at) in rows {
        insert_v103_projection_on(
            tx,
            Uuid::parse_str(&run_id).map_err(|error| DaemonError::Store(error.to_string()))?,
            Uuid::parse_str(&session_id).map_err(|error| DaemonError::Store(error.to_string()))?,
            Uuid::parse_str(&custody_id).map_err(|error| DaemonError::Store(error.to_string()))?,
            u64::try_from(generation).map_err(|_| {
                DaemonError::Store("invalid archive projection custody generation".into())
            })?,
            &receipt_digest,
            true,
            &settled_at,
        )?;
    }
    Ok(())
}

pub(crate) fn validate_v103_projection_rows(tx: &Transaction<'_>) -> Result<()> {
    let invalid: i64 = tx.query_row(
        "SELECT count(*) FROM archive_cleanup_success_projections projection
         LEFT JOIN archive_cleanup_runs run ON run.run_id=projection.run_id
         WHERE run.run_id IS NULL OR run.phase!='settled'
            OR run.session_id!=projection.session_id OR run.custody_id!=projection.custody_id
            OR run.custody_generation!=projection.custody_generation
            OR run.receipt_digest!=projection.receipt_digest
            OR (SELECT count(*) FROM archive_cleanup_projection_consumers consumer
                WHERE consumer.projection_id=projection.projection_id)!=3",
        [],
        |row| row.get(0),
    )?;
    if invalid != 0 {
        return Err(DaemonError::Store(format!(
            "V103 archive projection validation found {invalid} invalid row(s)"
        )));
    }
    let settled: i64 = tx.query_row(
        "SELECT count(*) FROM archive_cleanup_runs WHERE phase='settled'",
        [],
        |row| row.get(0),
    )?;
    let projections: i64 = tx.query_row(
        "SELECT count(*) FROM archive_cleanup_success_projections",
        [],
        |row| row.get(0),
    )?;
    if settled != projections {
        return Err(DaemonError::Store(format!(
            "V103 archive projection cardinality mismatch: settled={settled}, projections={projections}"
        )));
    }
    Ok(())
}

impl Store {
    pub(crate) fn insert_archive_cleanup_intent(
        &mut self,
        intent: &NewArchiveCleanupIntent,
    ) -> Result<ArchiveCleanupRun> {
        let now = timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_latest_for_generation_on(
            &tx,
            intent.session_id,
            intent.custody_id,
            intent.custody_generation,
        )? {
            if existing.phase != ArchiveCleanupPhaseV1::Refused {
                return Ok(existing);
            }
        }
        let values = [
            intent.topology_digest.as_str(),
            intent.clean_state_digest.as_str(),
            intent.tree_digest.as_str(),
            intent.dependency_digest.as_str(),
            intent.holder_digest.as_str(),
            intent.evidence_digest.as_str(),
        ];
        if values
            .iter()
            .any(|value| Sha256Digest::parse(*value).is_err())
        {
            return Err(DaemonError::Store(
                "archive cleanup intent contains a noncanonical digest".into(),
            ));
        }
        let repository_identity_digest = digest_field(
            "archive-repository-identity-v1",
            &intent.repository_identity,
        );
        let canonical_repo_dir_digest = digest_field(
            "archive-repository-directory-v1",
            &intent.canonical_repo_dir,
        );
        let original_root_digest = digest_field("archive-original-root-v1", &intent.original_root);
        let quarantine_root_digest =
            digest_field("archive-quarantine-root-v1", &intent.quarantine_root);
        let git_common_dir_digest =
            digest_field("archive-git-common-directory-v1", &intent.git_common_dir);
        let git_admin_dir_digest =
            digest_field("archive-git-admin-directory-v1", &intent.git_admin_dir);
        tx.execute(
            "INSERT INTO archive_cleanup_runs(
                 run_id,session_id,custody_id,custody_generation,schema_version,marker_version,
                 session_kind,session_status,session_updated_at,parent_id,continued_from,topology_digest,
                 repository_identity,repository_identity_digest,canonical_repo_dir,canonical_repo_dir_digest,
                 original_root,original_root_digest,quarantine_root,quarantine_root_digest,
                 root_device,root_inode,git_common_dir_digest,git_admin_dir_digest,git_admin_id,
                 source_ref,source_oid,preservation_class,target_ref,target_oid,clean_state_digest,
                 tree_digest,dependency_digest,holder_digest,evidence_digest,phase,phase_ordinal,row_version,
                 branch_preserved,intent_at,last_attempt_at,created_at,updated_at)
             VALUES(?1,?2,?3,?4,1,1,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,
                    ?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,
                    ?33,'intent_committed',1,1,0,?34,?34,?34,?34)",
            params![
                intent.run_id.to_string(),
                intent.session_id.to_string(),
                intent.custody_id.to_string(),
                intent.custody_generation as i64,
                intent.session_kind,
                intent.session_status,
                intent.session_updated_at,
                intent.parent_id.map(|value| value.to_string()),
                intent.continued_from.map(|value| value.to_string()),
                intent.topology_digest,
                intent.repository_identity,
                repository_identity_digest,
                intent.canonical_repo_dir,
                canonical_repo_dir_digest,
                intent.original_root,
                original_root_digest,
                intent.quarantine_root,
                quarantine_root_digest,
                intent.root_device as i64,
                intent.root_inode as i64,
                git_common_dir_digest,
                git_admin_dir_digest,
                intent.git_admin_id,
                intent.source_ref,
                intent.source_oid,
                preservation_str(intent.preservation_class),
                intent.target_ref,
                intent.target_oid,
                intent.clean_state_digest,
                intent.tree_digest,
                intent.dependency_digest,
                intent.holder_digest,
                intent.evidence_digest,
                now,
            ],
        )?;
        insert_event_on(
            &tx,
            intent.run_id,
            1,
            None,
            ArchiveCleanupPhaseV1::IntentCommitted,
            "intent_committed",
            Some(&intent.evidence_digest),
            None,
            None,
            &now,
        )?;
        tx.commit()?;
        self.archive_cleanup_run(intent.run_id)?.ok_or_else(|| {
            DaemonError::Store("archive cleanup intent disappeared after commit".into())
        })
    }

    pub(crate) fn archive_cleanup_run(&self, run_id: Uuid) -> Result<Option<ArchiveCleanupRun>> {
        self.conn
            .query_row(
                &run_select_sql("WHERE run_id=?1"),
                [run_id.to_string()],
                map_run,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn latest_archive_cleanup_for_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<ArchiveCleanupRun>> {
        self.conn
            .query_row(
                &run_select_sql("WHERE session_id=?1 ORDER BY created_at DESC,run_id DESC LIMIT 1"),
                [session_id.to_string()],
                map_run,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn archive_cleanup_status(
        &self,
        session_id: Uuid,
    ) -> Result<ArchiveCleanupStatusV1> {
        let run = self.latest_archive_cleanup_for_session(session_id)?;
        let Some(run) = run else {
            return Ok(ArchiveCleanupStatusV1 {
                version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
                session_id,
                run_id: None,
                phase: None,
                safe_code: ArchiveCleanupSafeCodeV1::NotApplicable,
                retryable: false,
                next_action: "No archive cleanup run exists for this Session.".into(),
                receipt: None,
                updated_at: None,
            });
        };
        let (retryable, next_action) = match run.phase {
            ArchiveCleanupPhaseV1::Settled => (
                false,
                "Cleanup is settled; the preserved source branch remains available.".into(),
            ),
            ArchiveCleanupPhaseV1::Refused => (
                true,
                "Stop competing maintenance, resolve the reported condition, then retry archive.".into(),
            ),
            ArchiveCleanupPhaseV1::RecoveryRequired => (
                false,
                "Preserve the repository and quarantine evidence; use a compatible recovery binary.".into(),
            ),
            _ => (
                true,
                "The daemon will resume this journaled run after restart or an exact archive retry.".into(),
            ),
        };
        let safe_code = run.safe_code.unwrap_or(match run.phase {
            ArchiveCleanupPhaseV1::Settled => ArchiveCleanupSafeCodeV1::Settled,
            ArchiveCleanupPhaseV1::RecoveryRequired => {
                ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous
            }
            _ => ArchiveCleanupSafeCodeV1::ProofUnavailable,
        });
        Ok(ArchiveCleanupStatusV1 {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            session_id,
            run_id: Some(run.run_id),
            phase: Some(run.phase),
            safe_code,
            retryable,
            next_action,
            receipt: run.receipt,
            updated_at: Some(run.updated_at),
        })
    }

    /// A Session with ordinary archive-cleanup history can enter the existing
    /// fresh-custody unarchive path only from the exact settled projection.
    /// Sessions with no such history retain their pre-V101 behavior.
    pub(crate) fn verify_archive_cleanup_unarchive_gate(&self, session_id: Uuid) -> Result<()> {
        let Some(run) = self.latest_archive_cleanup_for_session(session_id)? else {
            return Ok(());
        };
        if !self.archive_cleanup_settled_projection_exact(&run)? {
            return Err(DaemonError::InvalidParam(
                "archive cleanup is not settled for unarchive".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn current_settled_archive_cleanup_receipt(
        &self,
        session_id: Uuid,
    ) -> Result<Option<ArchiveCleanupReceiptV1>> {
        let Some(run) = self.latest_archive_cleanup_for_session(session_id)? else {
            return Ok(None);
        };
        if !self.archive_cleanup_settled_projection_exact(&run)? {
            return Ok(None);
        }
        Ok(run.receipt)
    }

    pub(crate) fn pending_archive_cleanup_projections(
        &self,
        session_id: Option<Uuid>,
        limit: u32,
    ) -> Result<Vec<ArchiveCleanupProjectionDelivery>> {
        self.pending_archive_cleanup_projections_with_memory(session_id, true, limit)
    }

    pub(crate) fn archive_cleanup_projection_recovery_page(
        &self,
        memory_available: bool,
        limit: u32,
    ) -> Result<Vec<ArchiveCleanupProjectionDelivery>> {
        self.pending_archive_cleanup_projections_with_memory(None, memory_available, limit)
    }

    pub(crate) fn archive_cleanup_projection_session_page(
        &self,
        session_id: Uuid,
        memory_available: bool,
        limit: u32,
    ) -> Result<Vec<ArchiveCleanupProjectionDelivery>> {
        self.pending_archive_cleanup_projections_with_memory(
            Some(session_id),
            memory_available,
            limit,
        )
    }

    fn pending_archive_cleanup_projections_with_memory(
        &self,
        session_id: Option<Uuid>,
        include_memory_only: bool,
        limit: u32,
    ) -> Result<Vec<ArchiveCleanupProjectionDelivery>> {
        let limit = limit.min(ARCHIVE_CLEANUP_RECOVERY_BATCH);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = if let Some(session_id) = session_id {
            let mut statement = self.conn.prepare(
                "SELECT projection.projection_id,projection.run_id,projection.session_id
                 FROM archive_cleanup_success_projections projection
                 WHERE projection.delivery_state!='delivered'
                   AND projection.session_id=?1
                   AND EXISTS (
                     SELECT 1 FROM archive_cleanup_projection_consumers consumer
                     WHERE consumer.projection_id=projection.projection_id
                       AND consumer.delivery_state!='delivered'
                       AND (?2=1 OR consumer.consumer_kind!='memory')
                   )
                 ORDER BY projection.updated_at,projection.projection_id
                 LIMIT ?3",
            )?;
            statement
                .query_map(
                    params![
                        session_id.to_string(),
                        i64::from(include_memory_only),
                        i64::from(limit)
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let mut statement = self.conn.prepare(
                "SELECT projection.projection_id,projection.run_id,projection.session_id
                 FROM archive_cleanup_success_projections projection
                 WHERE projection.delivery_state!='delivered'
                   AND EXISTS (
                     SELECT 1 FROM archive_cleanup_projection_consumers consumer
                     WHERE consumer.projection_id=projection.projection_id
                       AND consumer.delivery_state!='delivered'
                       AND (?1=1 OR consumer.consumer_kind!='memory')
                   )
                 ORDER BY projection.updated_at,projection.projection_id
                 LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![i64::from(include_memory_only), i64::from(limit)],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut deliveries = Vec::with_capacity(rows.len());
        for (projection_id, run_id, session_id) in rows {
            let projection_id = Uuid::parse_str(&projection_id)
                .map_err(|error| DaemonError::Store(error.to_string()))?;
            let mut statement = self.conn.prepare(
                "SELECT consumer_kind FROM archive_cleanup_projection_consumers
                 WHERE projection_id=?1 AND delivery_state!='delivered'
                 ORDER BY CASE consumer_kind WHEN 'watch' THEN 1 WHEN 'bus' THEN 2 ELSE 3 END",
            )?;
            let pending_consumers = statement
                .query_map([projection_id.to_string()], |row| {
                    let kind: String = row.get(0)?;
                    match kind.as_str() {
                        "bus" => Ok(ArchiveProjectionConsumer::Bus),
                        "watch" => Ok(ArchiveProjectionConsumer::Watch),
                        "memory" => Ok(ArchiveProjectionConsumer::Memory),
                        _ => Err(rusqlite::Error::InvalidQuery),
                    }
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            deliveries.push(ArchiveCleanupProjectionDelivery {
                projection_id,
                run_id: Uuid::parse_str(&run_id)
                    .map_err(|error| DaemonError::Store(error.to_string()))?,
                session_id: Uuid::parse_str(&session_id)
                    .map_err(|error| DaemonError::Store(error.to_string()))?,
                pending_consumers,
            });
        }
        Ok(deliveries)
    }

    pub(crate) fn begin_archive_cleanup_projection_consumer(
        &mut self,
        projection_id: Uuid,
        consumer: ArchiveProjectionConsumer,
    ) -> Result<bool> {
        let now = timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: Option<String> = tx
            .query_row(
                "SELECT delivery_state FROM archive_cleanup_projection_consumers
                 WHERE projection_id=?1 AND consumer_kind=?2",
                params![projection_id.to_string(), consumer.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            return Err(DaemonError::Store(
                "archive projection consumer receipt is missing".into(),
            ));
        };
        if state == "delivered" {
            return Ok(false);
        }
        let changed = tx.execute(
            "UPDATE archive_cleanup_projection_consumers
             SET delivery_state='delivering',row_version=row_version+1,
                 attempt_count=attempt_count+1,first_attempt_at=COALESCE(first_attempt_at,?1),
                 last_attempt_at=?1,updated_at=?1
             WHERE projection_id=?2 AND consumer_kind=?3
               AND delivery_state IN ('pending','delivering')",
            params![now, projection_id.to_string(), consumer.as_str()],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "archive projection consumer claim lost".into(),
            ));
        }
        tx.execute(
            "UPDATE archive_cleanup_success_projections
             SET delivery_state='delivering',row_version=row_version+1,
                 attempt_count=attempt_count+1,first_attempt_at=COALESCE(first_attempt_at,?1),
                 last_attempt_at=?1,updated_at=?1
             WHERE projection_id=?2 AND delivery_state IN ('pending','delivering')",
            params![now, projection_id.to_string()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn archive_cleanup_projection_consumer_state(
        &self,
        projection_id: Uuid,
        consumer: ArchiveProjectionConsumer,
    ) -> Result<Option<ArchiveProjectionConsumerState>> {
        let state = self
            .conn
            .query_row(
                "SELECT delivery_state FROM archive_cleanup_projection_consumers
                 WHERE projection_id=?1 AND consumer_kind=?2",
                params![projection_id.to_string(), consumer.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        state
            .map(|state| match state.as_str() {
                "pending" => Ok(ArchiveProjectionConsumerState::Pending),
                "delivering" => Ok(ArchiveProjectionConsumerState::Delivering),
                "delivered" => Ok(ArchiveProjectionConsumerState::Delivered),
                _ => Err(DaemonError::Store(
                    "archive projection consumer state is invalid".into(),
                )),
            })
            .transpose()
    }

    pub(crate) fn complete_archive_cleanup_projection_consumer(
        &mut self,
        projection_id: Uuid,
        consumer: ArchiveProjectionConsumer,
    ) -> Result<()> {
        let now = timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String = tx.query_row(
            "SELECT delivery_state FROM archive_cleanup_projection_consumers
             WHERE projection_id=?1 AND consumer_kind=?2",
            params![projection_id.to_string(), consumer.as_str()],
            |row| row.get(0),
        )?;
        if state == "delivered" {
            tx.commit()?;
            return Ok(());
        }
        if state != "delivering" {
            return Err(DaemonError::Store(
                "archive projection consumer was not claimed".into(),
            ));
        }
        tx.execute(
            "UPDATE archive_cleanup_projection_consumers
             SET delivery_state='delivered',row_version=row_version+1,
                 delivered_at=?1,updated_at=?1
             WHERE projection_id=?2 AND consumer_kind=?3 AND delivery_state='delivering'",
            params![now, projection_id.to_string(), consumer.as_str()],
        )?;
        let pending: i64 = tx.query_row(
            "SELECT count(*) FROM archive_cleanup_projection_consumers
             WHERE projection_id=?1 AND delivery_state!='delivered'",
            [projection_id.to_string()],
            |row| row.get(0),
        )?;
        if pending == 0 {
            tx.execute(
                "UPDATE archive_cleanup_success_projections
                 SET delivery_state='delivered',row_version=row_version+1,
                     delivered_at=?1,updated_at=?1
                 WHERE projection_id=?2 AND delivery_state='delivering'",
                params![now, projection_id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn archive_cleanup_settled_projection_exact(&self, run: &ArchiveCleanupRun) -> Result<bool> {
        let Some(receipt) = run.receipt.as_ref() else {
            return Ok(false);
        };
        receipt.validate_wire().map_err(DaemonError::Store)?;
        if run.phase != ArchiveCleanupPhaseV1::Settled
            || receipt.run_id != run.run_id
            || receipt.custody_id != run.custody_id
            || receipt.custody_generation != run.custody_generation
        {
            return Ok(false);
        }
        let next_generation = run.custody_generation.checked_add(1).ok_or_else(|| {
            DaemonError::Store("archive cleanup custody generation overflow".into())
        })?;
        self.conn
            .query_row(
                "SELECT EXISTS(
                SELECT 1 FROM sessions s
                JOIN sandbox_custody_roots r ON r.custody_id=s.sandbox_custody_id
                JOIN session_execution_projections p ON p.session_id=s.id
                WHERE s.id=?1 AND s.status='Archived'
                  AND s.sandbox_kind='GitWorktree'
                  AND s.sandbox_cleanup_state='Purged'
                  AND s.sandbox_root IS NULL AND s.sandbox_branch IS NULL
                  AND s.sandbox_custody_id=?2
                  AND r.state='purged' AND r.owner_session_id IS NULL
                  AND r.generation=?3 AND r.validation_state='verified'
                  AND r.validated_generation=r.generation
                  AND p.execution_state='historical_purged'
                  AND p.freshness='verified' AND p.effective_cwd IS NULL
                  AND p.custody_id=r.custody_id AND p.custody_generation=r.generation
                  AND p.error_code IS NULL
            )",
                params![
                    run.session_id.to_string(),
                    run.custody_id.to_string(),
                    next_generation as i64,
                ],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn advance_archive_cleanup_phase(
        &mut self,
        run_id: Uuid,
        expected_phase: ArchiveCleanupPhaseV1,
        expected_row_version: u64,
        next_phase: ArchiveCleanupPhaseV1,
        event_code: &str,
        marker: Option<(&str, &str)>,
    ) -> Result<ArchiveCleanupRun> {
        if !legal_transition(expected_phase, next_phase) {
            return Err(DaemonError::Store(
                "illegal archive cleanup phase transition".into(),
            ));
        }
        let now = timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_run_on(&tx, run_id)?
            .ok_or_else(|| DaemonError::Store("archive cleanup run is unavailable".into()))?;
        if run.phase != expected_phase || run.row_version != expected_row_version {
            return Err(DaemonError::Store(
                "archive cleanup phase compare-and-swap lost".into(),
            ));
        }
        let (marker_json, marker_digest) = marker
            .map(|(json, digest)| (Some(json), Some(digest)))
            .unwrap_or((None, None));
        let timestamp_column = match next_phase {
            ArchiveCleanupPhaseV1::Quarantined => "quarantined_at",
            ArchiveCleanupPhaseV1::RemovalAuthorized => "authorized_at",
            ArchiveCleanupPhaseV1::WorktreeRemoved => "removed_at",
            _ => {
                return Err(DaemonError::Store(
                    "ordinary phase advancement requires a nonterminal forward phase".into(),
                ));
            }
        };
        let sql = format!(
            "UPDATE archive_cleanup_runs SET phase=?1,phase_ordinal=?2,row_version=row_version+1,
                    branch_preserved=?3,removal_authority_json=COALESCE(?4,removal_authority_json),
                    removal_authority_digest=COALESCE(?5,removal_authority_digest),
                    {timestamp_column}=?6,last_attempt_at=?6,updated_at=?6
             WHERE run_id=?7 AND phase=?8 AND row_version=?9"
        );
        let changed = tx.execute(
            &sql,
            params![
                next_phase.as_str(),
                next_phase.ordinal() as i64,
                i64::from(matches!(
                    next_phase,
                    ArchiveCleanupPhaseV1::RemovalAuthorized
                        | ArchiveCleanupPhaseV1::WorktreeRemoved
                )),
                marker_json,
                marker_digest,
                now,
                run_id.to_string(),
                expected_phase.as_str(),
                expected_row_version as i64,
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "archive cleanup phase compare-and-swap lost".into(),
            ));
        }
        insert_event_on(
            &tx,
            run_id,
            next_event_sequence(&tx, run_id)?,
            Some(expected_phase),
            next_phase,
            event_code,
            Some(&run.evidence_digest),
            marker_digest,
            None,
            &now,
        )?;
        tx.commit()?;
        self.archive_cleanup_run(run_id)?.ok_or_else(|| {
            DaemonError::Store("archive cleanup run disappeared after phase commit".into())
        })
    }

    pub(crate) fn terminate_archive_cleanup_run(
        &mut self,
        run_id: Uuid,
        expected_phase: ArchiveCleanupPhaseV1,
        expected_row_version: u64,
        next_phase: ArchiveCleanupPhaseV1,
        safe_code: ArchiveCleanupSafeCodeV1,
        detail_code: Option<&str>,
    ) -> Result<()> {
        if !matches!(
            next_phase,
            ArchiveCleanupPhaseV1::Refused | ArchiveCleanupPhaseV1::RecoveryRequired
        ) || !legal_transition(expected_phase, next_phase)
        {
            return Err(DaemonError::Store(
                "illegal archive cleanup terminal transition".into(),
            ));
        }
        let now = timestamp();
        let timestamp_column = if next_phase == ArchiveCleanupPhaseV1::Refused {
            "refused_at"
        } else {
            "recovery_at"
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let evidence_digest: String = tx.query_row(
            "SELECT evidence_digest FROM archive_cleanup_runs
             WHERE run_id=?1 AND phase=?2 AND row_version=?3",
            params![
                run_id.to_string(),
                expected_phase.as_str(),
                expected_row_version as i64
            ],
            |row| row.get(0),
        )?;
        let sql = format!(
            "UPDATE archive_cleanup_runs SET phase=?1,phase_ordinal=100,row_version=row_version+1,
                    safe_code=?2,recovery_detail_code=?3,{timestamp_column}=?4,last_attempt_at=?4,updated_at=?4
             WHERE run_id=?5 AND phase=?6 AND row_version=?7"
        );
        if tx.execute(
            &sql,
            params![
                next_phase.as_str(),
                safe_code.as_str(),
                detail_code,
                now,
                run_id.to_string(),
                expected_phase.as_str(),
                expected_row_version as i64,
            ],
        )? != 1
        {
            return Err(DaemonError::Store(
                "archive cleanup terminal compare-and-swap lost".into(),
            ));
        }
        insert_event_on(
            &tx,
            run_id,
            next_event_sequence(&tx, run_id)?,
            Some(expected_phase),
            next_phase,
            safe_code.as_str(),
            Some(&evidence_digest),
            None,
            None,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn finalize_archive_cleanup(
        &mut self,
        run_id: Uuid,
        expected_row_version: u64,
    ) -> Result<ArchiveCleanupReceiptV1> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_run_on(&tx, run_id)?.ok_or_else(|| {
            DaemonError::Store("archive cleanup finalization run is unavailable".into())
        })?;
        if run.phase != ArchiveCleanupPhaseV1::WorktreeRemoved
            || run.row_version != expected_row_version
            || run.removal_authority_json.is_none()
            || run.removal_authority_digest.is_none()
        {
            return Err(DaemonError::Store(
                "archive cleanup finalization fence is invalid".into(),
            ));
        }
        let children: i64 = tx.query_row(
            "SELECT count(*) FROM sessions WHERE parent_id=?1",
            [run.session_id.to_string()],
            |row| row.get(0),
        )?;
        let participants: i64 = tx.query_row(
            "SELECT count(*) FROM sessions WHERE sandbox_custody_id=?1",
            [run.custody_id.to_string()],
            |row| row.get(0),
        )?;
        let lineage_successors: i64 = tx.query_row(
            "SELECT count(*) FROM sessions WHERE continued_from=?1",
            [run.session_id.to_string()],
            |row| row.get(0),
        )?;
        if children != 0 || participants != 1 || lineage_successors != 0 {
            return Err(DaemonError::Store(
                "archive cleanup final Session topology changed".into(),
            ));
        }
        super::successor_reservations::reject_nonterminal_agent_successor_lead_clear_on(
            &tx,
            run.session_id,
        )?;
        let current_topology: (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
        ) = tx.query_row(
            "SELECT session_kind,status,updated_at,parent_id,continued_from,title,
                    agent_role,epic_spawn_ordinal FROM sessions WHERE id=?1",
            [run.session_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        let current_parent = current_topology
            .3
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| DaemonError::Store("archive parent identity is malformed".into()))?;
        let current_continued = current_topology
            .4
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| DaemonError::Store("archive lineage identity is malformed".into()))?;
        let current_ordinal = current_topology
            .7
            .map(u32::try_from)
            .transpose()
            .map_err(|_| DaemonError::Store("archive ordinal identity is malformed".into()))?;
        let current_topology_digest = archive_topology_digest(
            run.session_id,
            &current_topology.0,
            &current_topology.1,
            &current_topology.2,
            current_parent,
            current_continued,
            current_topology.5.as_deref(),
            current_topology.6.as_deref(),
            current_ordinal,
        )?;
        if current_topology_digest != run.topology_digest {
            return Err(DaemonError::Store(
                "archive cleanup final topology fence drifted".into(),
            ));
        }
        let now = timestamp();
        let changed = tx.execute(
            "UPDATE sessions SET status='Archived',pending_archive=0,
                    retry_attempt=COALESCE(max_retries,retry_attempt),updated_at=?1
             WHERE id=?2 AND status=?3 AND updated_at=?4 AND session_kind=?5
               AND sandbox_custody_id=?6 AND sandbox_kind='GitWorktree'
               AND sandbox_root=?7 AND sandbox_branch=?8 AND sandbox_cleanup_state='Live'",
            params![
                now,
                run.session_id.to_string(),
                run.session_status,
                run.session_updated_at,
                run.session_kind,
                run.custody_id.to_string(),
                run.original_root,
                run.source_ref.strip_prefix("refs/heads/").unwrap_or(""),
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "archive cleanup final Session fence drifted".into(),
            ));
        }
        Store::resolve_c5_autofile_pending_tx(&tx, run.session_id)?;
        tx.execute(
            "UPDATE sessions SET lead_session_id=NULL,updated_at=?1
             WHERE lead_session_id=?2 AND session_kind IN ('Group','Epic')",
            params![now, run.session_id.to_string()],
        )?;
        sandbox_custody::transition_terminal_root_tx(
            &tx,
            run.custody_id,
            run.custody_generation,
            CustodyCause::Purge,
            "purged",
            "tombstoned",
            "historical_purged",
            None,
        )?;

        let content = ReceiptContent {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            run_id,
            session_id: run.session_id,
            custody_id: run.custody_id,
            custody_generation: run.custody_generation,
            preservation_class: run.preservation_class,
            source_branch: &run.source_ref,
            source_oid: &run.source_oid,
            target_ref: run.target_ref.as_deref(),
            target_oid: run.target_oid.as_deref(),
            phase: ArchiveCleanupPhaseV1::Settled,
            branch_preserved: true,
            session_status: "Archived",
            custody_state: "purged",
            cleanup_state: "Purged",
            created_at: &run.created_at,
            settled_at: &now,
        };
        let canonical = serde_json::to_string(&content)
            .map_err(|error| DaemonError::Store(error.to_string()))?;
        let digest = digest_field("archive-cleanup-receipt-v1", &canonical);
        let receipt = ArchiveCleanupReceiptV1 {
            version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
            run_id,
            session_id: run.session_id,
            custody_id: run.custody_id,
            custody_generation: run.custody_generation,
            preservation_class: run.preservation_class,
            source_branch: run.source_ref,
            source_oid: SourceWorktreeGitOidV1::parse(run.source_oid)
                .map_err(DaemonError::Store)?,
            target_ref: run.target_ref,
            target_oid: run
                .target_oid
                .map(SourceWorktreeGitOidV1::parse)
                .transpose()
                .map_err(DaemonError::Store)?,
            phase: ArchiveCleanupPhaseV1::Settled,
            branch_preserved: true,
            session_status: "Archived".into(),
            custody_state: "purged".into(),
            cleanup_state: "Purged".into(),
            created_at: run.created_at,
            settled_at: now.clone(),
            receipt_digest: Sha256Digest::parse(digest.clone()).map_err(DaemonError::Store)?,
        };
        receipt.validate_wire().map_err(DaemonError::Store)?;
        let receipt_json = serde_json::to_string(&receipt)
            .map_err(|error| DaemonError::Store(error.to_string()))?;
        let changed = tx.execute(
            "UPDATE archive_cleanup_runs SET phase='settled',phase_ordinal=5,
                    row_version=row_version+1,safe_code='settled',branch_preserved=1,
                    receipt_json=?1,receipt_digest=?2,settled_at=?3,last_attempt_at=?3,updated_at=?3
             WHERE run_id=?4 AND phase='worktree_removed' AND row_version=?5
               AND receipt_json IS NULL AND receipt_digest IS NULL",
            params![
                receipt_json,
                digest,
                now,
                run_id.to_string(),
                expected_row_version as i64,
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "archive cleanup receipt compare-and-swap lost".into(),
            ));
        }
        insert_event_on(
            &tx,
            run_id,
            next_event_sequence(&tx, run_id)?,
            Some(ArchiveCleanupPhaseV1::WorktreeRemoved),
            ArchiveCleanupPhaseV1::Settled,
            "settled",
            Some(&run.evidence_digest),
            run.removal_authority_digest.as_deref(),
            Some(receipt.receipt_digest.as_str()),
            &now,
        )?;
        insert_v103_projection_on(
            &tx,
            run_id,
            run.session_id,
            run.custody_id,
            run.custody_generation,
            receipt.receipt_digest.as_str(),
            false,
            &now,
        )?;
        archive_cleanup_final_commit_fault()?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn archive_cleanup_recovery_page(
        &self,
        cursor: Option<&ArchiveCleanupRecoveryCursor>,
        limit: u32,
    ) -> Result<ArchiveCleanupRecoveryPage> {
        let limit = limit.min(ARCHIVE_CLEANUP_RECOVERY_BATCH);
        if limit == 0 {
            return Ok(ArchiveCleanupRecoveryPage {
                runs: Vec::new(),
                has_more: false,
            });
        }
        let mut statement = self.conn.prepare(&run_select_sql(
            "WHERE phase NOT IN ('settled','refused','recovery_required')
             AND (?1 IS NULL OR updated_at>?1 OR (updated_at=?1 AND run_id>?2))
             ORDER BY updated_at,run_id LIMIT ?3",
        ))?;
        let cursor_time = cursor.map(|value| value.updated_at.as_str());
        let cursor_id = cursor.map(|value| value.run_id.to_string());
        let mut runs = statement
            .query_map(
                params![cursor_time, cursor_id, i64::from(limit) + 1],
                map_run,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let has_more = runs.len() > limit as usize;
        runs.truncate(limit as usize);
        Ok(ArchiveCleanupRecoveryPage { runs, has_more })
    }

    pub(crate) fn archive_cleanup_lineage_successor_count(&self, session_id: Uuid) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM sessions WHERE continued_from=?1",
            [session_id.to_string()],
            |row| row.get(0),
        )?;
        u64::try_from(count)
            .map_err(|_| DaemonError::Store("archive cleanup lineage count is invalid".into()))
    }
}

fn preservation_str(value: ArchivePreservationClassV1) -> &'static str {
    match value {
        ArchivePreservationClassV1::NoOutput => "no_output",
        ArchivePreservationClassV1::IntegratedAncestor => "integrated_ancestor",
    }
}

fn parse_preservation(value: &str) -> Result<ArchivePreservationClassV1> {
    match value {
        "no_output" => Ok(ArchivePreservationClassV1::NoOutput),
        "integrated_ancestor" => Ok(ArchivePreservationClassV1::IntegratedAncestor),
        _ => Err(DaemonError::Store(
            "invalid archive cleanup preservation class".into(),
        )),
    }
}

fn legal_transition(from: ArchiveCleanupPhaseV1, to: ArchiveCleanupPhaseV1) -> bool {
    matches!(
        (from, to),
        (
            ArchiveCleanupPhaseV1::IntentCommitted,
            ArchiveCleanupPhaseV1::Quarantined
                | ArchiveCleanupPhaseV1::Refused
                | ArchiveCleanupPhaseV1::RecoveryRequired
        ) | (
            ArchiveCleanupPhaseV1::Quarantined,
            ArchiveCleanupPhaseV1::RemovalAuthorized | ArchiveCleanupPhaseV1::RecoveryRequired
        ) | (
            ArchiveCleanupPhaseV1::RemovalAuthorized,
            ArchiveCleanupPhaseV1::WorktreeRemoved | ArchiveCleanupPhaseV1::RecoveryRequired
        ) | (
            ArchiveCleanupPhaseV1::WorktreeRemoved,
            ArchiveCleanupPhaseV1::Settled | ArchiveCleanupPhaseV1::RecoveryRequired
        )
    )
}

fn run_select_sql(suffix: &str) -> String {
    format!(
        "SELECT run_id,session_id,custody_id,custody_generation,session_kind,session_status,
                session_updated_at,parent_id,continued_from,topology_digest,repository_identity,
                canonical_repo_dir,original_root,quarantine_root,root_device,root_inode,
                git_common_dir_digest,git_admin_dir_digest,git_admin_id,source_ref,source_oid,
                preservation_class,target_ref,target_oid,clean_state_digest,tree_digest,
                dependency_digest,holder_digest,evidence_digest,phase,row_version,safe_code,
                removal_authority_json,removal_authority_digest,receipt_json,receipt_digest,
                created_at,updated_at
         FROM archive_cleanup_runs {suffix}"
    )
}

fn map_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArchiveCleanupRun> {
    fn uuid_at(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
        let value: String = row.get(index)?;
        Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }
    fn optional_uuid_at(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<Uuid>> {
        let value: Option<String> = row.get(index)?;
        value
            .map(|value| {
                Uuid::parse_str(&value).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        index,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()
    }
    let preservation: String = row.get(21)?;
    let phase: String = row.get(29)?;
    let safe_code: Option<String> = row.get(31)?;
    let receipt_json: Option<String> = row.get(34)?;
    let receipt_digest: Option<String> = row.get(35)?;
    let receipt: Option<ArchiveCleanupReceiptV1> = receipt_json
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                34,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    if let Some(receipt) = receipt.as_ref() {
        receipt.validate_wire().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                34,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error)),
            )
        })?;
        let computed = archive_cleanup_receipt_digest(receipt).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                34,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error.to_string())),
            )
        })?;
        if receipt.receipt_digest.as_str() != computed
            || receipt_digest.as_deref() != Some(computed.as_str())
        {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                34,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(
                    "archive cleanup receipt digest mismatch",
                )),
            ));
        }
    } else if receipt_digest.is_some() {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            35,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(
                "archive cleanup receipt digest has no receipt",
            )),
        ));
    }
    let parsed_phase = ArchiveCleanupPhaseV1::from_str(&phase).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            29,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(error)),
        )
    })?;
    if (parsed_phase == ArchiveCleanupPhaseV1::Settled) != receipt.is_some() {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            34,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(
                "archive cleanup phase and receipt disagree",
            )),
        ));
    }
    Ok(ArchiveCleanupRun {
        run_id: uuid_at(row, 0)?,
        session_id: uuid_at(row, 1)?,
        custody_id: uuid_at(row, 2)?,
        custody_generation: row.get::<_, i64>(3)? as u64,
        session_kind: row.get(4)?,
        session_status: row.get(5)?,
        session_updated_at: row.get(6)?,
        parent_id: optional_uuid_at(row, 7)?,
        continued_from: optional_uuid_at(row, 8)?,
        topology_digest: row.get(9)?,
        repository_identity: row.get(10)?,
        canonical_repo_dir: row.get(11)?,
        original_root: row.get(12)?,
        quarantine_root: row.get(13)?,
        root_device: row.get::<_, i64>(14)? as u64,
        root_inode: row.get::<_, i64>(15)? as u64,
        git_common_dir_digest: row.get(16)?,
        git_admin_dir_digest: row.get(17)?,
        git_admin_id: row.get(18)?,
        source_ref: row.get(19)?,
        source_oid: row.get(20)?,
        preservation_class: parse_preservation(&preservation).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                21,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        target_ref: row.get(22)?,
        target_oid: row.get(23)?,
        clean_state_digest: row.get(24)?,
        tree_digest: row.get(25)?,
        dependency_digest: row.get(26)?,
        holder_digest: row.get(27)?,
        evidence_digest: row.get(28)?,
        phase: parsed_phase,
        row_version: row.get::<_, i64>(30)? as u64,
        safe_code: safe_code
            .map(|value| ArchiveCleanupSafeCodeV1::from_str(&value))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    31,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(error)),
                )
            })?,
        removal_authority_json: row.get(32)?,
        removal_authority_digest: row.get(33)?,
        receipt,
        created_at: row.get(36)?,
        updated_at: row.get(37)?,
    })
}

fn archive_cleanup_receipt_digest(receipt: &ArchiveCleanupReceiptV1) -> Result<String> {
    let content = ReceiptContent {
        version: receipt.version,
        run_id: receipt.run_id,
        session_id: receipt.session_id,
        custody_id: receipt.custody_id,
        custody_generation: receipt.custody_generation,
        preservation_class: receipt.preservation_class,
        source_branch: &receipt.source_branch,
        source_oid: receipt.source_oid.as_str(),
        target_ref: receipt.target_ref.as_deref(),
        target_oid: receipt
            .target_oid
            .as_ref()
            .map(SourceWorktreeGitOidV1::as_str),
        phase: receipt.phase,
        branch_preserved: receipt.branch_preserved,
        session_status: "Archived",
        custody_state: "purged",
        cleanup_state: "Purged",
        created_at: &receipt.created_at,
        settled_at: &receipt.settled_at,
    };
    let canonical =
        serde_json::to_string(&content).map_err(|error| DaemonError::Store(error.to_string()))?;
    Ok(digest_field("archive-cleanup-receipt-v1", &canonical))
}

fn load_run_on(tx: &Transaction<'_>, run_id: Uuid) -> Result<Option<ArchiveCleanupRun>> {
    tx.query_row(
        &run_select_sql("WHERE run_id=?1"),
        [run_id.to_string()],
        map_run,
    )
    .optional()
    .map_err(Into::into)
}

fn load_latest_for_generation_on(
    tx: &Transaction<'_>,
    session_id: Uuid,
    custody_id: Uuid,
    generation: u64,
) -> Result<Option<ArchiveCleanupRun>> {
    tx.query_row(
        &run_select_sql(
            "WHERE session_id=?1 AND custody_id=?2 AND custody_generation=?3
             ORDER BY created_at DESC,run_id DESC LIMIT 1",
        ),
        params![
            session_id.to_string(),
            custody_id.to_string(),
            generation as i64
        ],
        map_run,
    )
    .optional()
    .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
fn insert_event_on(
    tx: &Transaction<'_>,
    run_id: Uuid,
    sequence: u64,
    from: Option<ArchiveCleanupPhaseV1>,
    to: ArchiveCleanupPhaseV1,
    code: &str,
    evidence_digest: Option<&str>,
    marker_digest: Option<&str>,
    receipt_digest: Option<&str>,
    occurred_at: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO archive_cleanup_events(
             run_id,sequence,from_phase,to_phase,safe_event_code,evidence_digest,
             marker_digest,receipt_digest,occurred_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            run_id.to_string(),
            sequence as i64,
            from.map(ArchiveCleanupPhaseV1::as_str),
            to.as_str(),
            code,
            evidence_digest,
            marker_digest,
            receipt_digest,
            occurred_at,
        ],
    )?;
    Ok(())
}

fn next_event_sequence(tx: &Transaction<'_>, run_id: Uuid) -> Result<u64> {
    let sequence: i64 = tx.query_row(
        "SELECT COALESCE(max(sequence),0)+1 FROM archive_cleanup_events WHERE run_id=?1",
        [run_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(sequence as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_phase_edges_are_closed_and_forward_only() {
        assert!(legal_transition(
            ArchiveCleanupPhaseV1::IntentCommitted,
            ArchiveCleanupPhaseV1::Quarantined
        ));
        assert!(legal_transition(
            ArchiveCleanupPhaseV1::WorktreeRemoved,
            ArchiveCleanupPhaseV1::Settled
        ));
        assert!(!legal_transition(
            ArchiveCleanupPhaseV1::Quarantined,
            ArchiveCleanupPhaseV1::IntentCommitted
        ));
        assert!(!legal_transition(
            ArchiveCleanupPhaseV1::Settled,
            ArchiveCleanupPhaseV1::RecoveryRequired
        ));
    }

    #[test]
    fn topology_digest_binds_identity_without_exposing_title_authority() {
        let session_id = Uuid::new_v4();
        let baseline = archive_topology_digest(
            session_id,
            "Task",
            "Completed",
            "2026-09-06T12:00:00.000000000Z",
            Some(Uuid::new_v4()),
            None,
            Some("raw manual title"),
            Some("implementer"),
            Some(4),
        )
        .expect("topology digest");
        let changed = archive_topology_digest(
            session_id,
            "Task",
            "Completed",
            "2026-09-06T12:00:00.000000000Z",
            None,
            None,
            Some("raw manual title"),
            Some("implementer"),
            Some(4),
        )
        .expect("changed topology digest");
        assert_ne!(baseline, changed);
        Sha256Digest::parse(baseline).expect("title and role are represented by a digest");
    }
}
