//! V120 bounded source-worktree batch and dependency substrate.
//!
//! This module intentionally has no effect route.  It provides the durable
//! cursor key, receipt companions, and bounded read primitives consumed by the
//! later audit/apply slices.  A missing or unverified projection is an
//! incomplete proof, never evidence that a dependency does not exist.

#![allow(
    dead_code,
    reason = "V120 intentionally lands the bounded V2 substrate before Slice 5 activates it"
)]

use super::{
    Store, cohort_settlement::source_worktree_quarantine_path,
    scheduled_jobs::decode_scheduled_job_wake_authority, target_reclaim_sweep,
};
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rand::{RngCore, rngs::OsRng};
use rsi_common::cohort_settlement::{
    SOURCE_WORKTREE_BATCH_MAX_CURSOR_BYTES, SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS,
    SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS, SOURCE_WORKTREE_BATCH_POLICY_VERSION,
    SOURCE_WORKTREE_BATCH_SCHEMA_VERSION, SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS,
    SourceWorktreeBatchCursorV2, validate_git_oid, validate_source_ref, validate_target_ref,
};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, types::ValueRef,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const PAGE_SIZE: usize = SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS;
const RECONCILE_LIMIT: usize = SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS;
const CURSOR_PREFIX: &str = "rsi-swc2:";

// RSI-RELEASED-MIGRATION-BEGIN: v120-source-worktree-batch-catalog
pub(crate) const V120_CATALOG_OBJECTS: [(&str, &str); 49] = [
    ("table", "source_worktree_batch_cursor_keys"),
    ("table", "source_worktree_batch_runs"),
    ("table", "source_worktree_batch_items"),
    ("table", "source_worktree_batch_participants"),
    ("table", "scheduled_job_path_projections"),
    ("table", "source_worktree_session_dependency_health"),
    ("view", "source_worktree_session_dependency_evidence_v120"),
    ("index", "idx_swc_v120_roots_repository_page"),
    ("index", "idx_swc_v120_roots_repository_exact"),
    ("index", "idx_swc_v120_roots_custody_generation"),
    ("index", "idx_swc_v120_custody_participants"),
    ("index", "idx_swc_v120_custody_events"),
    ("index", "idx_swc_v120_execution_custody_health"),
    ("index", "idx_swc_v120_execution_cwd"),
    ("index", "idx_swc_v120_jobs_projection_health"),
    ("index", "idx_swc_v120_jobs_canonical_path"),
    ("index", "idx_swc_v120_jobs_raw_path"),
    ("index", "idx_swc_v120_jobs_wake_session"),
    ("index", "idx_swc_v120_jobs_wake_mode"),
    ("index", "idx_swc_v120_jobs_enabled_id"),
    ("index", "idx_swc_v120_sessions_working_dir"),
    ("index", "idx_swc_v120_sessions_sandbox_root"),
    ("index", "idx_swc_v120_sessions_dependency_health"),
    ("index", "idx_swc_v120_roots_repository_ref"),
    ("index", "idx_swc_v120_batch_participant_session"),
    ("trigger", "scheduled_jobs_v120_projection_after_insert"),
    ("trigger", "scheduled_jobs_v120_projection_after_dependency"),
    ("trigger", "scheduled_jobs_v120_projection_after_delete"),
    (
        "trigger",
        "source_worktree_batch_cursor_keys_v120_no_update",
    ),
    (
        "trigger",
        "source_worktree_batch_cursor_keys_v120_no_delete",
    ),
    ("trigger", "source_worktree_batch_runs_v120_no_delete"),
    (
        "trigger",
        "source_worktree_batch_runs_v120_identity_immutable",
    ),
    ("trigger", "source_worktree_batch_runs_v120_forward"),
    ("trigger", "source_worktree_batch_items_v120_no_update"),
    ("trigger", "source_worktree_batch_items_v120_no_delete"),
    (
        "trigger",
        "source_worktree_batch_participants_v120_no_update",
    ),
    (
        "trigger",
        "source_worktree_batch_participants_v120_no_delete",
    ),
    (
        "trigger",
        "source_worktree_session_health_v120_insert_guard",
    ),
    (
        "trigger",
        "source_worktree_session_health_v120_update_guard",
    ),
    (
        "trigger",
        "source_worktree_session_health_v120_delete_guard",
    ),
    ("trigger", "sessions_v120_session_health_after_insert"),
    ("trigger", "sessions_v120_session_health_after_update"),
    ("trigger", "sessions_v120_session_health_after_delete"),
    (
        "trigger",
        "execution_projections_v120_session_health_after_insert",
    ),
    (
        "trigger",
        "execution_projections_v120_session_health_after_update",
    ),
    (
        "trigger",
        "execution_projections_v120_session_health_after_delete",
    ),
    ("trigger", "custody_roots_v120_session_health_after_insert"),
    ("trigger", "custody_roots_v120_session_health_after_update"),
    ("trigger", "custody_roots_v120_session_health_after_delete"),
];

pub(crate) const V120_CATALOG_FINGERPRINT: &str =
    "sha256:6ce62d2be52da2fc22d47d414d243914577f6287f46c129be6e3664d640ae9bc";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceWorktreeV120MigrationFault {
    AfterPreflight,
    AfterCatalog,
    AfterKey,
    AfterChecks,
    AfterUserVersion,
}

#[cfg(test)]
impl SourceWorktreeV120MigrationFault {
    pub(crate) const ALL: [Self; 5] = [
        Self::AfterPreflight,
        Self::AfterCatalog,
        Self::AfterKey,
        Self::AfterChecks,
        Self::AfterUserVersion,
    ];
}

#[cfg(test)]
thread_local! {
    static MIGRATION_FAULT: std::cell::Cell<Option<SourceWorktreeV120MigrationFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn fail_next_v120_migration(fault: SourceWorktreeV120MigrationFault) {
    MIGRATION_FAULT.with(|slot| slot.set(Some(fault)));
}

fn migration_fault(_fault: SourceWorktreeV120MigrationFault) -> Result<()> {
    #[cfg(test)]
    if MIGRATION_FAULT.with(|slot| {
        if slot.get() == Some(_fault) {
            slot.set(None);
            true
        } else {
            false
        }
    }) {
        return Err(DaemonError::Store(format!(
            "injected V120 source-worktree migration fault: {_fault:?}"
        )));
    }
    Ok(())
}

// RSI-RELEASED-MIGRATION-BEGIN: v120-source-worktree-batch-migration
pub(crate) fn apply_v120_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if active_version != 119 {
        return Err(DaemonError::Store(format!(
            "V120 requires exact V119 source, found V{active_version}"
        )));
    }
    let source_fingerprint = catalog_fingerprint(&tx)?;
    if source_fingerprint != target_reclaim_sweep::V119_FULL_CATALOG_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V120 requires exact V119 catalog, found {source_fingerprint}"
        )));
    }
    migration_fault(SourceWorktreeV120MigrationFault::AfterPreflight)?;

    tx.execute_batch(
        "CREATE TABLE source_worktree_batch_cursor_keys (
            key_id INTEGER PRIMARY KEY CHECK(key_id=1),
            cursor_key BLOB NOT NULL CHECK(typeof(cursor_key)='blob' AND length(cursor_key)=32),
            created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at))
        );
        CREATE TABLE source_worktree_batch_runs (
            run_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(run_id)),
            schema_version INTEGER NOT NULL CHECK(schema_version=2),
            policy_version INTEGER NOT NULL CHECK(policy_version=2),
            repository_identity_digest TEXT NOT NULL CHECK(length(repository_identity_digest)=64),
            cursor_digest TEXT NOT NULL CHECK(length(cursor_digest)=64),
            snapshot_digest TEXT NOT NULL CHECK(length(snapshot_digest)=71 AND substr(snapshot_digest,1,7)='sha256:'),
            snapshot_root_count INTEGER NOT NULL CHECK(snapshot_root_count BETWEEN 0 AND 16384),
            target_ref TEXT NOT NULL CHECK(length(target_ref) BETWEEN 12 AND 4096),
            target_oid TEXT NOT NULL CHECK(length(target_oid) IN (40,64) AND target_oid=lower(target_oid) AND target_oid NOT GLOB '*[^0-9a-f]*'),
            upper_custody_id TEXT CHECK(upper_custody_id IS NULL OR rsi_uuid_is_canonical(upper_custody_id)),
            page_start_custody_id TEXT CHECK(page_start_custody_id IS NULL OR rsi_uuid_is_canonical(page_start_custody_id)),
            page_end_custody_id TEXT CHECK(page_end_custody_id IS NULL OR rsi_uuid_is_canonical(page_end_custody_id)),
            plan_digest TEXT NOT NULL CHECK(length(plan_digest)=71 AND substr(plan_digest,1,7)='sha256:'),
            predecessor_receipt_digest TEXT CHECK(predecessor_receipt_digest IS NULL OR (length(predecessor_receipt_digest)=71 AND substr(predecessor_receipt_digest,1,7)='sha256:')),
            terminal_receipt_digest TEXT CHECK(terminal_receipt_digest IS NULL OR (length(terminal_receipt_digest)=71 AND substr(terminal_receipt_digest,1,7)='sha256:')),
            batch_ordinal INTEGER NOT NULL CHECK(batch_ordinal>=0),
            has_more INTEGER NOT NULL CHECK(has_more IN (0,1)),
            state TEXT NOT NULL CHECK(state IN ('audited','applying','terminal','refused','recovery_required')),
            created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            CHECK((snapshot_root_count=0 AND upper_custody_id IS NULL AND page_start_custody_id IS NULL AND page_end_custody_id IS NULL)
               OR (snapshot_root_count>0 AND upper_custody_id IS NOT NULL
                   AND ((page_start_custody_id IS NULL AND page_end_custody_id IS NULL)
                     OR (page_start_custody_id IS NOT NULL AND page_end_custody_id IS NOT NULL
                         AND page_start_custody_id<=page_end_custody_id AND page_end_custody_id<=upper_custody_id)))),
            CHECK((state='terminal' AND terminal_receipt_digest IS NOT NULL)
               OR (state!='terminal' AND terminal_receipt_digest IS NULL))
        );
        CREATE TABLE source_worktree_batch_items (
            run_id TEXT NOT NULL REFERENCES source_worktree_batch_runs(run_id) ON DELETE RESTRICT,
            sequence INTEGER NOT NULL CHECK(sequence>=0 AND sequence<256),
            custody_id TEXT NOT NULL REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
            custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
            participant_count INTEGER NOT NULL CHECK(participant_count BETWEEN 1 AND 1024),
            participant_digest TEXT NOT NULL CHECK(length(participant_digest)=71 AND substr(participant_digest,1,7)='sha256:'),
            evidence_digest TEXT NOT NULL CHECK(length(evidence_digest)=71 AND substr(evidence_digest,1,7)='sha256:'),
            PRIMARY KEY(run_id,sequence),
            UNIQUE(run_id,custody_id,custody_generation)
        );
        CREATE TABLE source_worktree_batch_participants (
            run_id TEXT NOT NULL,
            item_sequence INTEGER NOT NULL,
            participant_sequence INTEGER NOT NULL CHECK(participant_sequence BETWEEN 0 AND 1023),
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
            status TEXT NOT NULL CHECK(status IN ('Completed','Failed','Interrupted','Archived')),
            continued_from TEXT REFERENCES sessions(id) ON DELETE RESTRICT,
            projection_digest TEXT NOT NULL CHECK(length(projection_digest)=71 AND substr(projection_digest,1,7)='sha256:'),
            PRIMARY KEY(run_id,item_sequence,participant_sequence),
            UNIQUE(run_id,item_sequence,session_id),
            FOREIGN KEY(run_id,item_sequence) REFERENCES source_worktree_batch_items(run_id,sequence) ON DELETE RESTRICT
        );
        CREATE TABLE scheduled_job_path_projections (
            job_id TEXT PRIMARY KEY REFERENCES scheduled_jobs(id) ON DELETE CASCADE,
            raw_working_dir TEXT,
            raw_wake_mode TEXT NOT NULL,
            raw_wake_session_id TEXT,
            raw_enabled INTEGER NOT NULL CHECK(raw_enabled IN (0,1)),
            canonical_working_dir TEXT,
            verification_state TEXT NOT NULL CHECK(verification_state IN ('not_applicable','unverified','verified','invalid')),
            evidence_digest TEXT,
            verified_at TEXT,
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            CHECK((raw_working_dir IS NULL AND canonical_working_dir IS NULL
                    AND verification_state='not_applicable' AND evidence_digest IS NOT NULL AND verified_at IS NOT NULL)
               OR (verification_state='unverified'
                    AND canonical_working_dir IS NULL AND evidence_digest IS NULL AND verified_at IS NULL)
               OR (raw_working_dir IS NOT NULL AND verification_state='verified'
                    AND canonical_working_dir IS NOT NULL AND evidence_digest IS NOT NULL AND verified_at IS NOT NULL)
               OR (verification_state='invalid'
                    AND canonical_working_dir IS NULL AND evidence_digest IS NULL AND verified_at IS NOT NULL))
        );
        CREATE TABLE source_worktree_session_dependency_health (
            session_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(session_id)),
            relevance INTEGER NOT NULL CHECK(relevance IN (0,1,2))
        );
        CREATE VIEW source_worktree_session_dependency_evidence_v120 AS
        SELECT s.id AS session_id,p.custody_id AS projection_custody_id,
               rsi_swc_v120_session_dependency_relevance(
                   s.id,s.status,s.session_kind,s.working_dir,s.sandbox_root,
                   s.sandbox_custody_id,p.session_id IS NOT NULL,p.schema_version,
                   p.projection_version,p.execution_state,p.freshness,
                   p.canonical_repo_dir,p.effective_cwd,p.custody_id,
                   p.custody_generation,p.validated_at,p.error_code,p.updated_at,
                   s.sandbox_kind,s.sandbox_branch,s.sandbox_cleanup_state,
                   r.custody_id,r.state,r.owner_session_id,r.generation,
                   r.canonical_repo_dir,r.sandbox_root,r.sandbox_branch,
                   r.validation_state,r.validated_generation,r.validation_error_code
               ) AS relevance
          FROM sessions s
          LEFT JOIN session_execution_projections p ON p.session_id=s.id
          LEFT JOIN sandbox_custody_roots r ON r.custody_id=p.custody_id;
        INSERT INTO scheduled_job_path_projections
            (job_id,raw_working_dir,raw_wake_mode,raw_wake_session_id,raw_enabled,
             canonical_working_dir,verification_state,evidence_digest,verified_at,updated_at)
        SELECT id,working_dir,wake_mode,wake_session_id,CASE WHEN enabled=1 THEN 1 ELSE 0 END,
               NULL,'unverified',NULL,NULL,updated_at FROM scheduled_jobs;
        INSERT INTO source_worktree_session_dependency_health(session_id,relevance)
        SELECT session_id,relevance FROM source_worktree_session_dependency_evidence_v120;
        CREATE INDEX idx_swc_v120_roots_repository_page
            ON sandbox_custody_roots(repository_identity,state,custody_id);
        CREATE INDEX idx_swc_v120_roots_repository_exact
            ON sandbox_custody_roots(repository_identity,custody_id,generation,state);
        CREATE INDEX idx_swc_v120_roots_custody_generation
            ON sandbox_custody_roots(custody_id,generation,state);
        CREATE INDEX idx_swc_v120_roots_repository_ref
            ON sandbox_custody_roots(repository_identity,sandbox_branch,state,custody_id);
        CREATE INDEX idx_swc_v120_custody_participants
            ON sessions(sandbox_custody_id,id);
        CREATE INDEX idx_swc_v120_custody_events
            ON sandbox_custody_events(custody_id,sequence,event_kind);
        CREATE INDEX idx_swc_v120_execution_custody_health
            ON session_execution_projections(custody_id,freshness,session_id);
        CREATE INDEX idx_swc_v120_execution_cwd
            ON session_execution_projections(effective_cwd,session_id);
        CREATE INDEX idx_swc_v120_jobs_projection_health
            ON scheduled_job_path_projections(raw_enabled,verification_state,job_id);
        CREATE INDEX idx_swc_v120_jobs_canonical_path
            ON scheduled_job_path_projections(canonical_working_dir,job_id);
        CREATE INDEX idx_swc_v120_jobs_raw_path
            ON scheduled_jobs(working_dir,id) WHERE enabled=1 AND working_dir IS NOT NULL;
        CREATE INDEX idx_swc_v120_jobs_wake_session
            ON scheduled_jobs(wake_session_id,id) WHERE enabled=1 AND wake_session_id IS NOT NULL;
        CREATE INDEX idx_swc_v120_jobs_wake_mode
            ON scheduled_jobs(wake_mode,id) WHERE enabled=1;
        CREATE INDEX idx_swc_v120_jobs_enabled_id
            ON scheduled_jobs(enabled,id);
        CREATE INDEX idx_swc_v120_sessions_working_dir ON sessions(working_dir,id);
        CREATE INDEX idx_swc_v120_sessions_sandbox_root ON sessions(sandbox_root,id);
        CREATE INDEX idx_swc_v120_sessions_dependency_health
            ON source_worktree_session_dependency_health(relevance,session_id);
        CREATE INDEX idx_swc_v120_batch_participant_session
            ON source_worktree_batch_participants(session_id,run_id,item_sequence);
        CREATE TRIGGER scheduled_jobs_v120_projection_after_insert AFTER INSERT ON scheduled_jobs
        BEGIN
            INSERT INTO scheduled_job_path_projections
                (job_id,raw_working_dir,raw_wake_mode,raw_wake_session_id,raw_enabled,
                 canonical_working_dir,verification_state,evidence_digest,verified_at,updated_at)
            VALUES(NEW.id,NEW.working_dir,NEW.wake_mode,NEW.wake_session_id,
                   CASE WHEN NEW.enabled=1 THEN 1 ELSE 0 END,NULL,'unverified',NULL,NULL,NEW.updated_at);
        END;
        CREATE TRIGGER scheduled_jobs_v120_projection_after_dependency
        AFTER UPDATE OF working_dir,wake_mode,wake_session_id,enabled ON scheduled_jobs
        WHEN NEW.working_dir IS NOT OLD.working_dir OR NEW.wake_mode IS NOT OLD.wake_mode
          OR NEW.wake_session_id IS NOT OLD.wake_session_id OR NEW.enabled IS NOT OLD.enabled
        BEGIN
            UPDATE scheduled_job_path_projections
               SET raw_working_dir=NEW.working_dir,raw_wake_mode=NEW.wake_mode,
                   raw_wake_session_id=NEW.wake_session_id,
                   raw_enabled=CASE WHEN NEW.enabled=1 THEN 1 ELSE 0 END,
                   canonical_working_dir=NULL,verification_state='unverified',
                   evidence_digest=NULL,verified_at=NULL,updated_at=NEW.updated_at
             WHERE job_id=NEW.id;
        END;
        CREATE TRIGGER scheduled_jobs_v120_projection_after_delete AFTER DELETE ON scheduled_jobs
        BEGIN
            DELETE FROM scheduled_job_path_projections WHERE job_id=OLD.id;
        END;
        CREATE TRIGGER source_worktree_batch_cursor_keys_v120_no_update
        BEFORE UPDATE ON source_worktree_batch_cursor_keys
        BEGIN SELECT RAISE(ABORT,'source-worktree cursor key is immutable'); END;
        CREATE TRIGGER source_worktree_batch_cursor_keys_v120_no_delete
        BEFORE DELETE ON source_worktree_batch_cursor_keys
        BEGIN SELECT RAISE(ABORT,'source-worktree cursor key is immutable'); END;
        CREATE TRIGGER source_worktree_batch_runs_v120_no_delete BEFORE DELETE ON source_worktree_batch_runs
        BEGIN SELECT RAISE(ABORT,'source-worktree batch receipts are retained'); END;
        CREATE TRIGGER source_worktree_batch_runs_v120_identity_immutable BEFORE UPDATE ON source_worktree_batch_runs
        WHEN NEW.run_id!=OLD.run_id OR NEW.schema_version!=OLD.schema_version OR NEW.policy_version!=OLD.policy_version
          OR NEW.repository_identity_digest!=OLD.repository_identity_digest OR NEW.cursor_digest!=OLD.cursor_digest
          OR NEW.snapshot_digest!=OLD.snapshot_digest OR NEW.snapshot_root_count!=OLD.snapshot_root_count
          OR NEW.target_ref!=OLD.target_ref OR NEW.target_oid!=OLD.target_oid
          OR NEW.upper_custody_id IS NOT OLD.upper_custody_id
          OR NEW.page_start_custody_id IS NOT OLD.page_start_custody_id
          OR NEW.page_end_custody_id IS NOT OLD.page_end_custody_id OR NEW.plan_digest!=OLD.plan_digest
          OR NEW.predecessor_receipt_digest IS NOT OLD.predecessor_receipt_digest
          OR NEW.batch_ordinal!=OLD.batch_ordinal OR NEW.has_more!=OLD.has_more OR NEW.created_at!=OLD.created_at
        BEGIN SELECT RAISE(ABORT,'source-worktree batch receipt identity is immutable'); END;
        CREATE TRIGGER source_worktree_batch_runs_v120_forward BEFORE UPDATE ON source_worktree_batch_runs
        WHEN NEW.updated_at<OLD.updated_at
          OR (OLD.state='audited' AND NEW.state NOT IN ('audited','applying','refused','recovery_required'))
          OR (OLD.state='applying' AND NEW.state NOT IN ('applying','terminal','refused','recovery_required'))
          OR (OLD.state IN ('terminal','refused','recovery_required') AND NEW.state!=OLD.state)
        BEGIN SELECT RAISE(ABORT,'source-worktree batch receipt transition is not forward'); END;
        CREATE TRIGGER source_worktree_batch_items_v120_no_update BEFORE UPDATE ON source_worktree_batch_items
        BEGIN SELECT RAISE(ABORT,'source-worktree batch items are immutable'); END;
        CREATE TRIGGER source_worktree_batch_items_v120_no_delete BEFORE DELETE ON source_worktree_batch_items
        BEGIN SELECT RAISE(ABORT,'source-worktree batch items are retained'); END;
        CREATE TRIGGER source_worktree_batch_participants_v120_no_update BEFORE UPDATE ON source_worktree_batch_participants
        BEGIN SELECT RAISE(ABORT,'source-worktree batch participants are immutable'); END;
        CREATE TRIGGER source_worktree_batch_participants_v120_no_delete BEFORE DELETE ON source_worktree_batch_participants
        BEGIN SELECT RAISE(ABORT,'source-worktree batch participants are retained'); END;
        CREATE TRIGGER source_worktree_session_health_v120_insert_guard
        BEFORE INSERT ON source_worktree_session_dependency_health
        WHEN NEW.relevance IS NOT (
            SELECT relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE session_id=NEW.session_id
        )
        BEGIN SELECT RAISE(ABORT,'source-worktree Session health classification is invalid'); END;
        CREATE TRIGGER source_worktree_session_health_v120_update_guard
        BEFORE UPDATE ON source_worktree_session_dependency_health
        WHEN NEW.session_id IS NOT OLD.session_id OR NEW.relevance IS NOT (
            SELECT relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE session_id=NEW.session_id
        )
        BEGIN SELECT RAISE(ABORT,'source-worktree Session health classification is invalid'); END;
        CREATE TRIGGER source_worktree_session_health_v120_delete_guard
        BEFORE DELETE ON source_worktree_session_dependency_health
        WHEN EXISTS(SELECT 1 FROM sessions WHERE id=OLD.session_id)
        BEGIN SELECT RAISE(ABORT,'source-worktree Session health classification is retained'); END;
        CREATE TRIGGER sessions_v120_session_health_after_insert AFTER INSERT ON sessions
        BEGIN
            INSERT INTO source_worktree_session_dependency_health(session_id,relevance)
            SELECT session_id,relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE session_id=NEW.id
            ON CONFLICT(session_id) DO UPDATE SET relevance=excluded.relevance;
        END;
        CREATE TRIGGER sessions_v120_session_health_after_update AFTER UPDATE ON sessions
        BEGIN
            INSERT INTO source_worktree_session_dependency_health(session_id,relevance)
            SELECT session_id,relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE session_id=NEW.id
            ON CONFLICT(session_id) DO UPDATE SET relevance=excluded.relevance;
        END;
        CREATE TRIGGER sessions_v120_session_health_after_delete AFTER DELETE ON sessions
        BEGIN
            DELETE FROM source_worktree_session_dependency_health WHERE session_id=OLD.id;
        END;
        CREATE TRIGGER execution_projections_v120_session_health_after_insert
        AFTER INSERT ON session_execution_projections
        BEGIN
            INSERT INTO source_worktree_session_dependency_health(session_id,relevance)
            SELECT session_id,relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE session_id=NEW.session_id
            ON CONFLICT(session_id) DO UPDATE SET relevance=excluded.relevance;
        END;
        CREATE TRIGGER execution_projections_v120_session_health_after_update
        AFTER UPDATE ON session_execution_projections
        BEGIN
            INSERT INTO source_worktree_session_dependency_health(session_id,relevance)
            SELECT session_id,relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE session_id IN (OLD.session_id,NEW.session_id)
            ON CONFLICT(session_id) DO UPDATE SET relevance=excluded.relevance;
        END;
        CREATE TRIGGER execution_projections_v120_session_health_after_delete
        AFTER DELETE ON session_execution_projections
        BEGIN
            INSERT INTO source_worktree_session_dependency_health(session_id,relevance)
            SELECT session_id,relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE session_id=OLD.session_id
            ON CONFLICT(session_id) DO UPDATE SET relevance=excluded.relevance;
        END;
        CREATE TRIGGER custody_roots_v120_session_health_after_insert
        AFTER INSERT ON sandbox_custody_roots
        BEGIN
            INSERT INTO source_worktree_session_dependency_health(session_id,relevance)
            SELECT session_id,relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE projection_custody_id=NEW.custody_id
            ON CONFLICT(session_id) DO UPDATE SET relevance=excluded.relevance;
        END;
        CREATE TRIGGER custody_roots_v120_session_health_after_update
        AFTER UPDATE ON sandbox_custody_roots
        BEGIN
            INSERT INTO source_worktree_session_dependency_health(session_id,relevance)
            SELECT session_id,relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE projection_custody_id IN (OLD.custody_id,NEW.custody_id)
            ON CONFLICT(session_id) DO UPDATE SET relevance=excluded.relevance;
        END;
        CREATE TRIGGER custody_roots_v120_session_health_after_delete
        AFTER DELETE ON sandbox_custody_roots
        BEGIN
            INSERT INTO source_worktree_session_dependency_health(session_id,relevance)
            SELECT session_id,relevance FROM source_worktree_session_dependency_evidence_v120
             WHERE projection_custody_id=OLD.custody_id
            ON CONFLICT(session_id) DO UPDATE SET relevance=excluded.relevance;
        END;",
    )?;
    migration_fault(SourceWorktreeV120MigrationFault::AfterCatalog)?;

    let mut key = [0_u8; 32];
    OsRng.fill_bytes(&mut key);
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    tx.execute(
        "INSERT INTO source_worktree_batch_cursor_keys(key_id,cursor_key,created_at) VALUES(1,?1,?2)",
        params![key.as_slice(), now],
    )?;
    migration_fault(SourceWorktreeV120MigrationFault::AfterKey)?;
    validate_v120_catalog(&tx)?;
    let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(DaemonError::Store(format!(
            "V120 integrity_check failed: {integrity}"
        )));
    }
    let foreign_keys: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_keys != 0 {
        return Err(DaemonError::Store(format!(
            "V120 found {foreign_keys} foreign-key violation(s)"
        )));
    }
    migration_fault(SourceWorktreeV120MigrationFault::AfterChecks)?;
    tx.execute("PRAGMA user_version=120", [])?;
    migration_fault(SourceWorktreeV120MigrationFault::AfterUserVersion)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v120-source-worktree-batch-migration

pub(crate) fn validate_v120_catalog(connection: &Connection) -> Result<()> {
    for (kind, name) in V120_CATALOG_OBJECTS {
        let found: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type=?1 AND name=?2",
            params![kind, name],
            |row| row.get(0),
        )?;
        if found != 1 {
            return Err(DaemonError::Store(format!(
                "V120 catalog missing {kind} {name}"
            )));
        }
    }
    let fingerprint = v120_catalog_fingerprint(connection)?;
    if fingerprint != V120_CATALOG_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V120 catalog fingerprint mismatch: expected {V120_CATALOG_FINGERPRINT}, got {fingerprint}"
        )));
    }
    let key_count: i64 = connection.query_row(
        "SELECT count(*) FROM source_worktree_batch_cursor_keys WHERE key_id=1 AND length(cursor_key)=32",
        [],
        |row| row.get(0),
    )?;
    if key_count != 1 {
        return Err(DaemonError::Store(
            "V120 cursor key singleton is malformed".into(),
        ));
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v120-source-worktree-batch-catalog

pub(crate) fn v120_catalog_fingerprint(connection: &Connection) -> Result<String> {
    let mut objects = V120_CATALOG_OBJECTS.to_vec();
    objects.sort_unstable();
    let mut hash = Sha256::new();
    for (kind, name) in objects {
        let row: (String, String, String, String) = connection.query_row(
            "SELECT type,name,tbl_name,coalesce(sql,'') FROM sqlite_master WHERE type=?1 AND name=?2",
            params![kind, name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        for (index, field) in [row.0, row.1, row.2, row.3].into_iter().enumerate() {
            hash.update(field.as_bytes());
            if index != 3 {
                hash.update(b"\0");
            }
        }
        hash.update(b"\n");
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

fn catalog_fingerprint(connection: &Connection) -> Result<String> {
    let mut statement = connection.prepare(
        "SELECT type,name,tbl_name,coalesce(sql,'') FROM sqlite_master
         ORDER BY type,name,tbl_name,coalesce(sql,'')",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut hash = Sha256::new();
    for row in rows {
        let (kind, name, table, sql) = row?;
        for (index, field) in [kind, name, table, sql].into_iter().enumerate() {
            hash.update(field.as_bytes());
            if index != 3 {
                hash.update(b"\0");
            }
        }
        hash.update(b"\n");
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

#[cfg(test)]
thread_local! {
    static PROJECTION_REFRESH_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_projection_refresh() {
    PROJECTION_REFRESH_FAULT.with(|slot| slot.set(true));
}

fn projection_refresh_fault() -> Result<()> {
    #[cfg(test)]
    if PROJECTION_REFRESH_FAULT.with(|slot| slot.replace(false)) {
        return Err(DaemonError::Store(
            "injected scheduled-job projection refresh fault".into(),
        ));
    }
    Ok(())
}

/// Bounded startup reconciliation revalidates at most the repository safety
/// ceiling. It repairs missing rows and catches symlink repoints, but performs
/// no write when the current evidence is unchanged.
pub(crate) fn reconcile_scheduled_job_path_projections(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT CAST(id AS TEXT) FROM scheduled_jobs
          ORDER BY CASE WHEN enabled=1 THEN 0 ELSE 1 END,
                   CASE WHEN id IN (SELECT job_id FROM scheduled_job_path_projections
                                     WHERE verification_state IN ('unverified','invalid'))
                        THEN 0 ELSE 1 END,
                   id LIMIT ?1",
    )?;
    let rows = statement.query_map([RECONCILE_LIMIT as i64], |row| row.get::<_, String>(0))?;
    for row in rows {
        refresh_scheduled_job_path_projection(connection, &row?)?;
    }
    Ok(())
}

/// Refresh one projection in the same Store transaction as its scheduled-job
/// write.  Historical migration fixtures legitimately run before V120, so an
/// absent projection table is the only no-op case.
pub(crate) fn refresh_scheduled_job_path_projection(
    connection: &Connection,
    job_id: &str,
) -> Result<()> {
    let installed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='scheduled_job_path_projections')",
        [],
        |row| row.get(0),
    )?;
    if !installed {
        return Ok(());
    }
    projection_refresh_fault()?;
    let row: Option<(Option<String>, Option<String>, Option<String>, Option<i64>, bool)> = connection
        .query_row(
            "SELECT
                CASE WHEN working_dir IS NULL OR (typeof(working_dir)='text' AND octet_length(working_dir)<=4096)
                     THEN working_dir END,
                CASE WHEN typeof(wake_mode)='text' AND octet_length(wake_mode) BETWEEN 1 AND 4096
                     THEN wake_mode END,
                CASE WHEN wake_session_id IS NULL OR (typeof(wake_session_id)='text' AND octet_length(wake_session_id)<=128)
                     THEN wake_session_id END,
                CASE WHEN typeof(enabled)='integer' AND enabled IN (0,1) THEN enabled END,
                (working_dir IS NOT NULL AND (typeof(working_dir)!='text' OR octet_length(working_dir)>4096))
                  OR typeof(wake_mode)!='text' OR octet_length(wake_mode) NOT BETWEEN 1 AND 4096
                  OR (wake_session_id IS NOT NULL AND (typeof(wake_session_id)!='text' OR octet_length(wake_session_id)>128))
                  OR typeof(enabled)!='integer' OR enabled NOT IN (0,1)
             FROM scheduled_jobs WHERE id=?1",
            [job_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let Some((raw, wake_mode, wake_session_id, enabled, malformed)) = row else {
        return Err(DaemonError::Store(
            "scheduled job disappeared before path projection refresh".into(),
        ));
    };
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let enabled = enabled.unwrap_or(0);
    let wake_mode = wake_mode.unwrap_or_else(|| "invalid".into());
    let wake_authority_valid =
        scheduled_job_wake_authority_is_canonical(&wake_mode, wake_session_id.as_deref());
    let (canonical, state) = if malformed || !wake_authority_valid {
        (None, "invalid")
    } else if let Some(raw) = raw.as_deref() {
        match std::fs::canonicalize(Path::new(raw)) {
            Ok(path) if path.is_dir() => (Some(path.to_string_lossy().into_owned()), "verified"),
            _ => (None, "invalid"),
        }
    } else {
        (None, "not_applicable")
    };
    let evidence = matches!(state, "verified" | "not_applicable").then(|| {
        projection_evidence_digest(
            raw.as_deref(),
            canonical.as_deref(),
            &wake_mode,
            wake_session_id.as_deref(),
            enabled,
        )
    });
    let existing: Option<(
        Option<String>,
        String,
        Option<String>,
        i64,
        Option<String>,
        String,
        Option<String>,
    )> = connection
        .query_row(
            "SELECT raw_working_dir,raw_wake_mode,raw_wake_session_id,raw_enabled,
                    canonical_working_dir,verification_state,evidence_digest
               FROM scheduled_job_path_projections WHERE job_id=?1",
            [job_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let desired = (
        raw.clone(),
        wake_mode.clone(),
        wake_session_id.clone(),
        enabled,
        canonical.clone(),
        state.to_string(),
        evidence.clone(),
    );
    if existing.as_ref() == Some(&desired) {
        return Ok(());
    }
    connection.execute(
        "INSERT INTO scheduled_job_path_projections
            (job_id,raw_working_dir,raw_wake_mode,raw_wake_session_id,raw_enabled,
             canonical_working_dir,verification_state,evidence_digest,verified_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)
         ON CONFLICT(job_id) DO UPDATE SET
            raw_working_dir=excluded.raw_working_dir,raw_wake_mode=excluded.raw_wake_mode,
            raw_wake_session_id=excluded.raw_wake_session_id,raw_enabled=excluded.raw_enabled,
            canonical_working_dir=excluded.canonical_working_dir,
            verification_state=excluded.verification_state,evidence_digest=excluded.evidence_digest,
            verified_at=excluded.verified_at,updated_at=excluded.updated_at",
        params![
            job_id,
            raw,
            wake_mode,
            wake_session_id,
            enabled,
            canonical,
            state,
            evidence,
            now,
        ],
    )?;
    Ok(())
}

fn projection_evidence_digest(
    raw: Option<&str>,
    canonical: Option<&str>,
    wake_mode: &str,
    wake_session_id: Option<&str>,
    enabled: i64,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"rsi-source-worktree-job-projection-v120\0");
    for field in [raw, canonical, Some(wake_mode), wake_session_id] {
        hash_optional_field(&mut hash, field);
    }
    hash.update(enabled.to_be_bytes());
    format!("sha256:{:x}", hash.finalize())
}

fn hash_optional_field(hash: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        None => hash.update([0]),
    }
}

fn canonical_uuid(value: &str) -> std::result::Result<Uuid, ()> {
    let parsed = Uuid::parse_str(value).map_err(|_| ())?;
    if parsed.is_nil() || parsed.to_string() != value {
        return Err(());
    }
    Ok(parsed)
}

fn scheduled_job_wake_authority_is_canonical(
    wake_mode: &str,
    wake_session_id: Option<&str>,
) -> bool {
    decode_scheduled_job_wake_authority(Some(wake_mode), wake_session_id).is_ok()
        && wake_session_id.is_none_or(|value| canonical_uuid(value).is_ok())
        && wake_mode
            .strip_prefix("on_terminal:")
            .is_none_or(|value| canonical_uuid(value).is_ok())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeSnapshotKeyV2 {
    pub custody_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeSnapshotItemV2 {
    pub key: SourceWorktreeSnapshotKeyV2,
    pub generation: u64,
    pub sandbox_root: String,
    pub source_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeSnapshotPageV2 {
    pub items: Vec<SourceWorktreeSnapshotItemV2>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeSnapshotCaptureV2 {
    repository_snapshot_digest: String,
    upper: Option<SourceWorktreeSnapshotKeyV2>,
    root_count: u32,
    page: SourceWorktreeSnapshotPageV2,
}

impl SourceWorktreeSnapshotCaptureV2 {
    pub(crate) fn repository_snapshot_digest(&self) -> &str {
        &self.repository_snapshot_digest
    }

    pub(crate) fn upper(&self) -> Option<&SourceWorktreeSnapshotKeyV2> {
        self.upper.as_ref()
    }

    pub(crate) const fn root_count(&self) -> u32 {
        self.root_count
    }

    pub(crate) const fn page(&self) -> &SourceWorktreeSnapshotPageV2 {
        &self.page
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeCandidateInventoryV2 {
    pub requested_session_id: Uuid,
    pub custody_id: Uuid,
    pub generation: u64,
    pub state: String,
    pub repository_identity: String,
    pub canonical_repo_dir: String,
    pub sandbox_root: String,
    pub source_ref: String,
    pub source_commit: String,
    pub owner_session_id: Uuid,
    pub owner_status: String,
    pub requested_status: String,
    pub requested_continued_from: Option<Uuid>,
    pub validation_state: String,
    pub validated_generation: Option<u64>,
    pub reserved_effects: u64,
    pub active_effects: u64,
    pub owner_projection_state: String,
    pub owner_projection_freshness: String,
    pub owner_projection_effective_cwd: Option<String>,
    pub owner_projection_custody_id: Option<Uuid>,
    pub owner_projection_generation: Option<u64>,
    pub participant_count: u32,
    pub participant_overflow: bool,
    pub custody_event_count: u32,
    pub custody_event_overflow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeParticipantV2 {
    pub session_id: Uuid,
    pub status: String,
    pub session_kind: String,
    pub continued_from: Option<Uuid>,
    pub sandbox_root: Option<String>,
    pub sandbox_branch: Option<String>,
    pub projection_state: Option<String>,
    pub projection_freshness: Option<String>,
    pub projection_effective_cwd: Option<String>,
    pub projection_custody_id: Option<Uuid>,
    pub projection_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeParticipantInventoryV2 {
    pub participants: Vec<SourceWorktreeParticipantV2>,
    pub overflow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeCustodyEventV2 {
    pub sequence: u64,
    pub event_kind: String,
    pub cause: String,
    pub from_generation: Option<u64>,
    pub to_generation: u64,
    pub from_owner_session_id: Option<Uuid>,
    pub to_owner_session_id: Option<Uuid>,
    pub origin_session_id: Option<Uuid>,
    pub prior_state: Option<String>,
    pub next_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeCustodyEventInventoryV2 {
    pub events: Vec<SourceWorktreeCustodyEventV2>,
    pub overflow: bool,
}

impl Store {
    /// Capture the complete bounded root identity and one detail page from one
    /// SQLite read snapshot. The UUID upper bound describes key space only;
    /// insertion-time exclusion comes from the read transaction and the
    /// internally computed count/digest. A later request must recapture and
    /// compare all three values before trusting a cursor.
    pub(crate) fn source_worktree_snapshot_capture(
        &self,
        repository_identity: &str,
        after: Option<&SourceWorktreeSnapshotKeyV2>,
    ) -> Result<SourceWorktreeSnapshotCaptureV2> {
        self.source_worktree_snapshot_capture_inner(repository_identity, after, || {})
    }

    #[cfg(test)]
    fn source_worktree_snapshot_capture_with_hook(
        &self,
        repository_identity: &str,
        after: Option<&SourceWorktreeSnapshotKeyV2>,
        after_snapshot_started: impl FnOnce(),
    ) -> Result<SourceWorktreeSnapshotCaptureV2> {
        self.source_worktree_snapshot_capture_inner(
            repository_identity,
            after,
            after_snapshot_started,
        )
    }

    fn source_worktree_snapshot_capture_inner(
        &self,
        repository_identity: &str,
        after: Option<&SourceWorktreeSnapshotKeyV2>,
        after_snapshot_started: impl FnOnce(),
    ) -> Result<SourceWorktreeSnapshotCaptureV2> {
        validate_repository_identity(repository_identity)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let mut digest = Sha256::new();
        digest.update(b"rsi-source-worktree-repository-snapshot-v2\0");
        hash_optional_field(&mut digest, Some(repository_identity));
        let mut scan_after: Option<String> = None;
        let mut root_count = 0_usize;
        let mut upper = None;
        let mut items = Vec::with_capacity(PAGE_SIZE);
        let mut has_more = false;
        let mut hook = Some(after_snapshot_started);
        loop {
            let page = {
                let mut statement = tx.prepare(
                    "SELECT custody_id,generation,sandbox_root,sandbox_branch
                       FROM sandbox_custody_roots INDEXED BY idx_swc_v120_roots_repository_page
                      WHERE repository_identity=?1 AND state='live'
                        AND (?2 IS NULL OR custody_id>?2)
                      ORDER BY custody_id LIMIT ?3",
                )?;
                let rows = statement.query_map(
                    params![repository_identity, scan_after.as_deref(), PAGE_SIZE as i64],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            if let Some(hook) = hook.take() {
                // The first SELECT has fixed the SQLite read snapshot. Tests
                // insert through a second WAL connection at this boundary.
                hook();
            }
            if page.is_empty() {
                break;
            }
            for (custody, generation, sandbox_root, source_ref) in &page {
                root_count += 1;
                if root_count > SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS {
                    return Err(DaemonError::Store(format!(
                        "source-worktree snapshot exceeds {SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS} roots"
                    )));
                }
                let custody_id = parse_uuid(custody, "snapshot custody id")?;
                let generation = u64::try_from(*generation).map_err(|_| {
                    DaemonError::Store("invalid custody generation in snapshot page".into())
                })?;
                if generation == 0 || sandbox_root.len() > 4096 || source_ref.len() > 4096 {
                    return Err(DaemonError::Store(
                        "malformed source-worktree snapshot row".into(),
                    ));
                }
                let source_ref = canonical_source_ref(source_ref)?;
                hash_optional_field(&mut digest, Some(custody));
                digest.update(generation.to_be_bytes());
                hash_optional_field(&mut digest, Some(sandbox_root));
                hash_optional_field(&mut digest, Some(&source_ref));
                let key = SourceWorktreeSnapshotKeyV2 { custody_id };
                upper = Some(key.clone());
                if after.is_none_or(|after| custody_id > after.custody_id) {
                    if items.len() < PAGE_SIZE {
                        items.push(SourceWorktreeSnapshotItemV2 {
                            key,
                            generation,
                            sandbox_root: sandbox_root.clone(),
                            source_ref,
                        });
                    } else {
                        has_more = true;
                    }
                }
            }
            scan_after = page.last().map(|row| row.0.clone());
            if page.len() < PAGE_SIZE {
                break;
            }
        }
        digest.update((root_count as u64).to_be_bytes());
        hash_optional_field(
            &mut digest,
            upper
                .as_ref()
                .map(|key| key.custody_id.to_string())
                .as_deref(),
        );
        tx.commit()?;
        Ok(SourceWorktreeSnapshotCaptureV2 {
            repository_snapshot_digest: format!("sha256:{:x}", digest.finalize()),
            upper,
            root_count: root_count as u32,
            page: SourceWorktreeSnapshotPageV2 { items, has_more },
        })
    }

    /// Indexed root-centric dependency proof. SQL selects only raw/canonical
    /// prefix supersets and exact custody/session links; Rust repeats current
    /// containment and hashes the full evidence rows.
    pub(crate) fn source_worktree_targeted_dependencies(
        &self,
        custody_id: Uuid,
        generation: u64,
    ) -> Result<SourceWorktreeTargetedDependenciesV2> {
        self.source_worktree_targeted_dependencies_impl(custody_id, generation, false)
    }

    /// Dependency proof for the absent-root adoption path. The caller must
    /// already have proved that the canonical sandbox path and Git worktree
    /// registration are both absent. Custody participants are omitted from
    /// external dependency counts, while every nonparticipant still receives
    /// the full fresh projection and path validation used by the normal proof.
    pub(crate) fn source_worktree_targeted_dependencies_for_absent_root(
        &self,
        custody_id: Uuid,
        generation: u64,
    ) -> Result<SourceWorktreeTargetedDependenciesV2> {
        self.source_worktree_targeted_dependencies_impl(custody_id, generation, true)
    }

    fn source_worktree_targeted_dependencies_impl(
        &self,
        custody_id: Uuid,
        generation: u64,
        absent_root_proven: bool,
    ) -> Result<SourceWorktreeTargetedDependenciesV2> {
        let expected: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT generation,sandbox_root FROM sandbox_custody_roots WHERE custody_id=?1",
                [custody_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((actual_generation, stored_root)) = expected else {
            return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(
                "missing_custody_root",
            ));
        };
        if actual_generation != generation as i64 || generation == 0 {
            return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(
                "custody_generation_or_root_drift",
            ));
        }
        let roots = match dependency_roots(&self.conn, custody_id, generation, &stored_root)? {
            Some(roots) => roots,
            None => {
                return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(
                    "dependency_alias_evidence_invalid",
                ));
            }
        };
        let participants = participant_ids(&self.conn, custody_id)?;
        if participants.len() > SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS {
            return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(
                "participant_dependency_bound",
            ));
        }
        let mut job_ids = match fresh_enabled_job_path_dependencies(&self.conn, &roots)? {
            Ok(ids) => ids,
            Err(reason) => {
                return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(reason));
            }
        };
        for root in &roots.raw {
            collect_path_ids(
                &self.conn,
                "scheduled_jobs",
                "idx_swc_v120_jobs_raw_path",
                "working_dir",
                "enabled=1 AND working_dir IS NOT NULL",
                root,
                &mut job_ids,
            )?;
        }
        for root in &roots.canonical {
            collect_path_ids(
                &self.conn,
                "scheduled_job_path_projections",
                "idx_swc_v120_jobs_canonical_path",
                "canonical_working_dir",
                "verification_state='verified' AND raw_enabled=1",
                root,
                &mut job_ids,
            )?;
        }
        for participant in &participants {
            collect_exact_ids(
                &self.conn,
                "scheduled_jobs",
                "idx_swc_v120_jobs_wake_session",
                "wake_session_id",
                "enabled=1 AND wake_session_id IS NOT NULL",
                participant,
                &mut job_ids,
            )?;
            collect_exact_ids(
                &self.conn,
                "scheduled_jobs",
                "idx_swc_v120_jobs_wake_mode",
                "wake_mode",
                "enabled=1",
                &format!("on_terminal:{participant}"),
                &mut job_ids,
            )?;
        }
        if job_ids.len() > SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS {
            return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(
                "scheduled_job_dependency_bound",
            ));
        }
        let mut scheduled_dependencies = 0_u32;
        let mut scheduled_digest = Sha256::new();
        scheduled_digest.update(b"rsi-source-worktree-targeted-scheduled-v2\0");
        for job_id in job_ids {
            let job = match load_job_dependency(&self.conn, &job_id)? {
                Some(job) => job,
                None => continue,
            };
            if !job.projection_is_current() {
                return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(
                    "scheduled_job_projection_stale",
                ));
            }
            let path_related = job
                .raw_working_dir
                .as_deref()
                .is_some_and(|path| path_matches_roots(Path::new(path), &roots))
                || job
                    .canonical_working_dir
                    .as_deref()
                    .is_some_and(|path| path_matches_roots(Path::new(path), &roots));
            let link_related = job
                .wake_session_id
                .as_deref()
                .is_some_and(|id| participants.contains(id))
                || job
                    .watched_session_id()
                    .is_some_and(|id| participants.contains(id));
            if path_related || link_related {
                scheduled_dependencies =
                    scheduled_dependencies.checked_add(1).ok_or_else(|| {
                        DaemonError::Store("scheduled dependency count overflowed".into())
                    })?;
                job.hash_into(&mut scheduled_digest);
            }
        }

        // An absent root can make the participant's projection stale solely
        // because the proved sandbox directory no longer exists. Participants
        // are excluded from the external dependency count below; all unrelated
        // session paths still receive fresh projection validation.
        let stale_missing_root_participants = absent_root_proven.then_some(&participants);
        let mut session_ids = match fresh_session_path_dependencies(
            &self.conn,
            &roots,
            stale_missing_root_participants,
        )? {
            Ok(ids) => ids,
            Err(reason) => {
                return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(reason));
            }
        };
        for root in &roots.raw {
            collect_relevant_session_path_ids(
                &self.conn,
                "sessions",
                "working_dir",
                root,
                &mut session_ids,
            )?;
            collect_relevant_session_path_ids(
                &self.conn,
                "sessions",
                "sandbox_root",
                root,
                &mut session_ids,
            )?;
        }
        for root in &roots.canonical {
            collect_relevant_session_path_ids(
                &self.conn,
                "session_execution_projections",
                "effective_cwd",
                root,
                &mut session_ids,
            )?;
        }
        collect_relevant_session_exact_ids(
            &self.conn,
            "session_execution_projections",
            "custody_id",
            &custody_id.to_string(),
            &mut session_ids,
        )?;
        if session_ids.len() > SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS {
            return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(
                "session_dependency_bound",
            ));
        }
        let mut session_dependencies = 0_u32;
        let mut session_digest = Sha256::new();
        session_digest.update(b"rsi-source-worktree-targeted-session-v2\0");
        for session_id in session_ids {
            if participants.contains(&session_id) {
                continue;
            }
            let Some(session) = load_session_dependency(&self.conn, &session_id)? else {
                continue;
            };
            match session.relevance() {
                SessionDependencyRelevance::ProvenIrrelevant => continue,
                SessionDependencyRelevance::Invalid => {
                    return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(
                        "session_projection_unhealthy",
                    ));
                }
                SessionDependencyRelevance::Relevant => {}
            }
            let path_related = [
                Some(session.working_dir.as_str()),
                session.sandbox_root.as_deref(),
                session.effective_cwd.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|path| path_matches_roots(Path::new(path), &roots));
            let custody_related = session.projection_custody_id.as_deref()
                == Some(custody_id.to_string().as_str())
                || session.sandbox_custody_id.as_deref() == Some(custody_id.to_string().as_str());
            if path_related || custody_related {
                if !session.projection_is_current() {
                    return Ok(SourceWorktreeTargetedDependenciesV2::incomplete(
                        "session_projection_unhealthy",
                    ));
                }
                session_dependencies = session_dependencies.checked_add(1).ok_or_else(|| {
                    DaemonError::Store("Session dependency count overflowed".into())
                })?;
                session.hash_into(&mut session_digest);
            }
        }
        Ok(SourceWorktreeTargetedDependenciesV2 {
            complete: true,
            scheduled_dependency_count: scheduled_dependencies,
            scheduled_dependency_digest: format!("sha256:{:x}", scheduled_digest.finalize()),
            session_path_dependency_count: session_dependencies,
            session_path_dependency_digest: format!("sha256:{:x}", session_digest.finalize()),
            reason: None,
        })
    }

    /// Exact one-root inventory.  The caller receives the identity it must
    /// reauthenticate before an effect and never gets a truncated set of
    /// participants or custody history.
    pub(crate) fn source_worktree_candidate_inventory(
        &self,
        session_id: Uuid,
        custody_id: Uuid,
        generation: u64,
    ) -> Result<Option<SourceWorktreeCandidateInventoryV2>> {
        let row: Option<(
            i64,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            Option<i64>,
            i64,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            i64,
            i64,
        )> = self
            .conn
            .query_row(
                "SELECT r.generation,r.state,r.repository_identity,r.canonical_repo_dir,r.sandbox_root,
                        r.sandbox_branch,r.source_commit,r.owner_session_id,owner.status,requested.status,
                        requested.continued_from,r.validation_state,r.validated_generation,
                        r.reserved_effects,r.active_effects,p.execution_state,p.freshness,p.effective_cwd,
                        p.custody_id,p.custody_generation,
                        (SELECT count(*) FROM (
                            SELECT linked.id FROM sessions linked
                             WHERE linked.sandbox_custody_id=r.custody_id
                             LIMIT 1025
                        )),
                        (SELECT count(*) FROM (
                            SELECT e.sequence FROM sandbox_custody_events e
                             WHERE e.custody_id=r.custody_id
                             LIMIT 16385
                        ))
                   FROM sandbox_custody_roots r
                   JOIN sessions requested ON requested.id=?1 AND requested.sandbox_custody_id=r.custody_id
                   JOIN sessions owner ON owner.id=r.owner_session_id
                   JOIN session_execution_projections p ON p.session_id=owner.id
                  WHERE r.custody_id=?2 AND r.generation=?3",
                params![session_id.to_string(), custody_id.to_string(), generation as i64],
                |row| {
                    Ok((
                        row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
                        row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?,
                        row.get(10)?, row.get(11)?, row.get(12)?, row.get(13)?, row.get(14)?,
                        row.get(15)?, row.get(16)?, row.get(17)?, row.get(18)?, row.get(19)?,
                        row.get(20)?, row.get(21)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            actual_generation,
            state,
            repository_identity,
            canonical_repo_dir,
            sandbox_root,
            branch,
            source_commit,
            owner,
            owner_status,
            requested_status,
            continued_from,
            validation_state,
            validated_generation,
            reserved_effects,
            active_effects,
            projection_state,
            projection_freshness,
            projection_cwd,
            projection_custody,
            projection_generation,
            participants,
            events,
        )) = row
        else {
            return Ok(None);
        };
        let participant_overflow = participants > SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS as i64;
        let custody_event_overflow = events > SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS as i64;
        Ok(Some(SourceWorktreeCandidateInventoryV2 {
            requested_session_id: session_id,
            custody_id,
            generation: positive_u64(actual_generation, "candidate generation")?,
            state,
            repository_identity,
            canonical_repo_dir,
            sandbox_root,
            source_ref: canonical_source_ref(&branch)?,
            source_commit,
            owner_session_id: parse_uuid(&owner, "candidate owner")?,
            owner_status,
            requested_status,
            requested_continued_from: continued_from
                .map(|value| parse_uuid(&value, "candidate predecessor"))
                .transpose()?,
            validation_state,
            validated_generation: validated_generation
                .map(|value| positive_u64(value, "validated generation"))
                .transpose()?,
            reserved_effects: nonnegative_u64(reserved_effects, "reserved effects")?,
            active_effects: nonnegative_u64(active_effects, "active effects")?,
            owner_projection_state: projection_state,
            owner_projection_freshness: projection_freshness,
            owner_projection_effective_cwd: projection_cwd,
            owner_projection_custody_id: projection_custody
                .map(|value| parse_uuid(&value, "owner projection custody"))
                .transpose()?,
            owner_projection_generation: projection_generation
                .map(|value| positive_u64(value, "owner projection generation"))
                .transpose()?,
            participant_count: u32::try_from(
                participants.min(SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS as i64),
            )
            .map_err(|_| DaemonError::Store("candidate participant count is malformed".into()))?,
            participant_overflow,
            custody_event_count: u32::try_from(
                events.min(SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS as i64),
            )
            .map_err(|_| DaemonError::Store("candidate event count is malformed".into()))?,
            custody_event_overflow,
        }))
    }

    pub(crate) fn source_worktree_participant_inventory(
        &self,
        custody_id: Uuid,
        remaining_limit: usize,
    ) -> Result<SourceWorktreeParticipantInventoryV2> {
        if remaining_limit == 0 || remaining_limit > SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS {
            return Err(DaemonError::InvalidParam(
                "participant inventory remaining limit is invalid".into(),
            ));
        }
        let mut statement = self.conn.prepare(
            "SELECT s.id,s.status,s.session_kind,s.continued_from,s.sandbox_root,s.sandbox_branch,
                    p.execution_state,p.freshness,p.effective_cwd,p.custody_id,p.custody_generation
               FROM sessions s LEFT JOIN session_execution_projections p ON p.session_id=s.id
              WHERE s.sandbox_custody_id=?1 ORDER BY s.id LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![custody_id.to_string(), (remaining_limit + 1) as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            },
        )?;
        let mut result = Vec::new();
        for row in rows {
            let (
                id,
                status,
                session_kind,
                continued_from,
                sandbox_root,
                sandbox_branch,
                projection_state,
                freshness,
                effective_cwd,
                projection_custody,
                projection_generation,
            ) = row?;
            if result.len() == remaining_limit {
                return Ok(SourceWorktreeParticipantInventoryV2 {
                    participants: Vec::new(),
                    overflow: true,
                });
            }
            let id = parse_uuid(&id, "participant id")?;
            let continued_from = continued_from
                .map(|value| parse_uuid(&value, "participant predecessor"))
                .transpose()?;
            let projection_generation = projection_generation
                .map(|value| positive_u64(value, "participant projection generation"))
                .transpose()?;
            result.push(SourceWorktreeParticipantV2 {
                session_id: id,
                status,
                session_kind,
                continued_from,
                sandbox_root,
                sandbox_branch,
                projection_state,
                projection_freshness: freshness,
                projection_effective_cwd: effective_cwd,
                projection_custody_id: projection_custody
                    .map(|value| parse_uuid(&value, "participant projection custody"))
                    .transpose()?,
                projection_generation,
            });
        }
        Ok(SourceWorktreeParticipantInventoryV2 {
            participants: result,
            overflow: false,
        })
    }

    pub(crate) fn source_worktree_custody_event_inventory(
        &self,
        custody_id: Uuid,
    ) -> Result<SourceWorktreeCustodyEventInventoryV2> {
        let mut statement = self.conn.prepare(
            "SELECT sequence,event_kind,cause,from_generation,to_generation,
                    from_owner_session_id,to_owner_session_id,origin_session_id,prior_state,next_state
               FROM sandbox_custody_events WHERE custody_id=?1 ORDER BY sequence LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                custody_id.to_string(),
                (SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS + 1) as i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                ))
            },
        )?;
        let mut result = Vec::new();
        for row in rows {
            let (
                sequence,
                event_kind,
                cause,
                from_generation,
                to_generation,
                from,
                to,
                origin,
                prior_state,
                next_state,
            ) = row?;
            if result.len() == SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS {
                return Ok(SourceWorktreeCustodyEventInventoryV2 {
                    events: Vec::new(),
                    overflow: true,
                });
            }
            let parse = |value: Option<String>| {
                value
                    .map(|value| parse_uuid(&value, "custody event participant"))
                    .transpose()
            };
            result.push(SourceWorktreeCustodyEventV2 {
                sequence: positive_u64(sequence, "custody event sequence")?,
                event_kind,
                cause,
                from_generation: from_generation
                    .map(|value| positive_u64(value, "custody event from generation"))
                    .transpose()?,
                to_generation: positive_u64(to_generation, "custody event to generation")?,
                from_owner_session_id: parse(from)?,
                to_owner_session_id: parse(to)?,
                origin_session_id: parse(origin)?,
                prior_state,
                next_state,
            });
        }
        Ok(SourceWorktreeCustodyEventInventoryV2 {
            events: result,
            overflow: false,
        })
    }

    /// Repository-wide collision proof is deliberately independent of the
    /// current page, so a same-ref root after page 256 remains visible.
    pub(crate) fn source_worktree_global_ref_collision(
        &self,
        repository_identity: &str,
        source_ref: &str,
        except_custody_id: Uuid,
    ) -> Result<bool> {
        validate_repository_identity(repository_identity)?;
        validate_source_ref(source_ref).map_err(DaemonError::InvalidParam)?;
        let branch = source_ref
            .strip_prefix("refs/heads/")
            .ok_or_else(|| DaemonError::InvalidParam("source ref is not local".into()))?;
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sandbox_custody_roots
              WHERE repository_identity=?1 AND sandbox_branch=?2 AND custody_id!=?3 AND state='live')",
            params![repository_identity, branch, except_custody_id.to_string()], |row| row.get(0),
        ).map_err(Into::into)
    }
}

#[derive(Debug)]
struct DependencyRoots {
    raw: Vec<PathBuf>,
    canonical: Vec<PathBuf>,
}

fn dependency_roots(
    connection: &Connection,
    custody_id: Uuid,
    generation: u64,
    stored_root: &str,
) -> Result<Option<DependencyRoots>> {
    let original = PathBuf::from(stored_root);
    if !original.is_absolute() || stored_root.len() > 4096 {
        return Ok(None);
    }
    let mut raw = BTreeSet::from([original.clone()]);
    let mut aliases = connection.prepare(
        "SELECT run_id,session_id,sandbox_root,phase
           FROM source_worktree_settlement_items
          WHERE custody_id=?1 AND custody_generation=?2
            AND phase IN ('intent_committed','worktree_removed','branch_removed')
          ORDER BY run_id,session_id LIMIT ?3",
    )?;
    let rows = aliases.query_map(
        params![
            custody_id.to_string(),
            generation as i64,
            (PAGE_SIZE + 1) as i64
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    )?;
    let mut alias_count = 0_usize;
    for row in rows {
        alias_count += 1;
        if alias_count > PAGE_SIZE {
            return Ok(None);
        }
        let (run_id, session_id, journal_root, _) = row?;
        if journal_root != stored_root {
            return Ok(None);
        }
        let quarantine = source_worktree_quarantine_path(
            &original,
            parse_uuid(&run_id, "dependency alias run")?,
            parse_uuid(&session_id, "dependency alias session")?,
        )?;
        raw.insert(quarantine);
    }
    let mut canonical = BTreeSet::new();
    for path in &raw {
        if let Ok(value) = std::fs::canonicalize(path) {
            if value.is_dir() {
                canonical.insert(value);
            }
        }
    }
    Ok(Some(DependencyRoots {
        raw: raw.into_iter().collect(),
        canonical: canonical.into_iter().collect(),
    }))
}

fn prefix_bounds(path: &Path) -> Result<(String, String, String)> {
    let exact = path
        .to_str()
        .ok_or_else(|| DaemonError::Store("dependency path is not UTF-8".into()))?;
    if !path.is_absolute() || exact.len() > 4096 {
        return Err(DaemonError::Store(
            "dependency path is not a bounded absolute path".into(),
        ));
    }
    let stem = if exact == "/" {
        ""
    } else {
        exact.trim_end_matches('/')
    };
    Ok((exact.to_string(), format!("{stem}/"), format!("{stem}0")))
}

fn collect_path_ids(
    connection: &Connection,
    table: &str,
    index: &str,
    column: &str,
    predicate: &str,
    root: &Path,
    ids: &mut BTreeSet<String>,
) -> Result<()> {
    let id_column = match table {
        "scheduled_job_path_projections" => "job_id",
        "session_execution_projections" => "session_id",
        _ => "id",
    };
    let (exact, lower, upper) = prefix_bounds(root)?;
    let sql = format!(
        "SELECT {id_column} FROM {table} INDEXED BY {index}
          WHERE {predicate} AND ({column}=?1 OR ({column}>=?2 AND {column}<?3))
          ORDER BY {column},{id_column} LIMIT ?4"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        params![
            exact,
            lower,
            upper,
            (SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS + 1) as i64
        ],
        |row| row.get::<_, String>(0),
    )?;
    for row in rows {
        ids.insert(row?);
    }
    Ok(())
}

fn collect_exact_ids(
    connection: &Connection,
    table: &str,
    index: &str,
    column: &str,
    predicate: &str,
    value: &str,
    ids: &mut BTreeSet<String>,
) -> Result<()> {
    let id_column = match table {
        "scheduled_job_path_projections" => "job_id",
        "session_execution_projections" => "session_id",
        _ => "id",
    };
    let sql = format!(
        "SELECT {id_column} FROM {table} INDEXED BY {index}
          WHERE {predicate} AND {column}=?1 ORDER BY {id_column} LIMIT ?2"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        params![value, (SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS + 1) as i64],
        |row| row.get::<_, String>(0),
    )?;
    for row in rows {
        ids.insert(row?);
    }
    Ok(())
}

/// Intersect Session path/custody candidates with the authenticated V120
/// relevance projection before applying the candidate ceiling. The health
/// index has already bounded this side of the join to relevant or invalid
/// rows, so arbitrarily large authenticated inert history cannot create an
/// unbounded path-index scan or a false capacity refusal.
fn collect_relevant_session_path_ids(
    connection: &Connection,
    table: &str,
    column: &str,
    root: &Path,
    ids: &mut BTreeSet<String>,
) -> Result<()> {
    let id_column = match table {
        "sessions" => "id",
        "session_execution_projections" => "session_id",
        _ => {
            return Err(DaemonError::Store(
                "invalid V120 Session path candidate table".into(),
            ));
        }
    };
    if !matches!(column, "working_dir" | "sandbox_root" | "effective_cwd") {
        return Err(DaemonError::Store(
            "invalid V120 Session path candidate column".into(),
        ));
    }
    let (exact, lower, upper) = prefix_bounds(root)?;
    let sql = format!(
        "SELECT h.session_id
           FROM source_worktree_session_dependency_health h
                INDEXED BY idx_swc_v120_sessions_dependency_health
           JOIN {table} d ON d.{id_column}=h.session_id
          WHERE h.relevance IN (1,2)
            AND (d.{column}=?1 OR (d.{column}>=?2 AND d.{column}<?3))
          ORDER BY h.relevance,h.session_id LIMIT ?4"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        params![
            exact,
            lower,
            upper,
            (SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS + 1) as i64
        ],
        |row| row.get::<_, String>(0),
    )?;
    for row in rows {
        ids.insert(row?);
    }
    Ok(())
}

fn collect_relevant_session_exact_ids(
    connection: &Connection,
    table: &str,
    column: &str,
    exact: &str,
    ids: &mut BTreeSet<String>,
) -> Result<()> {
    if table != "session_execution_projections" || column != "custody_id" {
        return Err(DaemonError::Store(
            "invalid V120 Session exact candidate projection".into(),
        ));
    }
    let mut statement = connection.prepare(
        "SELECT h.session_id
           FROM source_worktree_session_dependency_health h
                INDEXED BY idx_swc_v120_sessions_dependency_health
           JOIN session_execution_projections p ON p.session_id=h.session_id
          WHERE h.relevance IN (1,2) AND p.custody_id=?1
          ORDER BY h.relevance,h.session_id LIMIT ?2",
    )?;
    let rows = statement.query_map(
        params![exact, (SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS + 1) as i64],
        |row| row.get::<_, String>(0),
    )?;
    for row in rows {
        ids.insert(row?);
    }
    Ok(())
}

/// Cached path indexes identify a small candidate superset, but cannot prove a
/// negative after an arbitrary directory or alias is replaced outside SQLite.
/// Revalidate every enabled job path within the independent dependency-health
/// ceiling before trusting that indexed superset.
fn fresh_enabled_job_path_dependencies(
    connection: &Connection,
    roots: &DependencyRoots,
) -> Result<std::result::Result<BTreeSet<String>, &'static str>> {
    let mut dependencies = BTreeSet::new();
    let mut after: Option<String> = None;
    let mut total = 0_usize;
    loop {
        let mut statement = connection.prepare(
            "SELECT CAST(id AS TEXT) FROM scheduled_jobs INDEXED BY idx_swc_v120_jobs_enabled_id
              WHERE enabled=1 AND (?1 IS NULL OR id>?1)
              ORDER BY enabled,id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after.as_deref(), PAGE_SIZE as i64], |row| {
            row.get::<_, String>(0)
        })?;
        let page = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if page.is_empty() {
            break;
        }
        if total == SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS {
            return Ok(Err("scheduled_job_dependency_health_bound"));
        }
        total += page.len();
        for id in &page {
            let Some(job) = load_job_dependency(connection, id)? else {
                return Ok(Err("scheduled_job_projection_missing"));
            };
            if !job.projection_is_current() {
                return Ok(Err("scheduled_job_projection_unhealthy"));
            }
            if job
                .raw_working_dir
                .as_deref()
                .is_some_and(|path| path_matches_roots(Path::new(path), roots))
                || job
                    .canonical_working_dir
                    .as_deref()
                    .is_some_and(|path| path_matches_roots(Path::new(path), roots))
            {
                dependencies.insert(id.clone());
            }
        }
        after = page.last().cloned();
    }
    Ok(Ok(dependencies))
}

/// Session working directories and sandbox paths are mutable filesystem
/// names. Validate every executable or restorable leaf projection afresh so a
/// cached outside path cannot hide a newly inward-pointing symlink.
fn fresh_session_path_dependencies(
    connection: &Connection,
    roots: &DependencyRoots,
    skip_projection_health_for: Option<&BTreeSet<String>>,
) -> Result<std::result::Result<BTreeSet<String>, &'static str>> {
    let mut dependencies = BTreeSet::new();
    let mut after: Option<(i64, String)> = None;
    let mut total = 0_usize;
    loop {
        let mut statement = connection.prepare(
            "SELECT relevance,session_id
               FROM source_worktree_session_dependency_health
                    INDEXED BY idx_swc_v120_sessions_dependency_health
              WHERE relevance IN (1,2)
                AND (?1 IS NULL OR relevance>?1 OR (relevance=?1 AND session_id>?2))
              ORDER BY relevance,session_id LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                after.as_ref().map(|key| key.0),
                after.as_ref().map(|key| key.1.as_str()),
                PAGE_SIZE as i64,
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?;
        let page = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if page.is_empty() {
            break;
        }
        if total == SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS {
            return Ok(Err("session_dependency_health_bound"));
        }
        total += page.len();
        for (stored_relevance, id) in &page {
            if skip_projection_health_for.is_some_and(|ids| ids.contains(id)) {
                continue;
            }
            let Some(session) = load_session_dependency(connection, id)? else {
                return Ok(Err("session_projection_missing"));
            };
            let relevance = session.relevance();
            if relevance.database_code() != *stored_relevance {
                return Ok(Err("session_projection_unhealthy"));
            }
            match relevance {
                SessionDependencyRelevance::ProvenIrrelevant => {
                    return Ok(Err("session_projection_unhealthy"));
                }
                SessionDependencyRelevance::Invalid => {
                    return Ok(Err("session_projection_unhealthy"));
                }
                SessionDependencyRelevance::Relevant => {}
            }
            if !session.projection_is_current() {
                return Ok(Err("session_projection_unhealthy"));
            }
            if [
                Some(session.working_dir.as_str()),
                session.sandbox_root.as_deref(),
                session.effective_cwd.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|path| path_matches_roots(Path::new(path), roots))
            {
                dependencies.insert(id.clone());
            }
        }
        after = page.last().cloned();
    }
    Ok(Ok(dependencies))
}

fn participant_ids(connection: &Connection, custody_id: Uuid) -> Result<BTreeSet<String>> {
    let mut statement = connection.prepare(
        "SELECT id FROM sessions INDEXED BY idx_swc_v120_custody_participants
          WHERE sandbox_custody_id=?1 ORDER BY id LIMIT ?2",
    )?;
    let rows = statement.query_map(
        params![
            custody_id.to_string(),
            (SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS + 1) as i64
        ],
        |row| row.get::<_, String>(0),
    )?;
    rows.collect::<std::result::Result<BTreeSet<_>, _>>()
        .map_err(Into::into)
}

fn path_matches_roots(path: &Path, roots: &DependencyRoots) -> bool {
    let raw_match = roots
        .raw
        .iter()
        .any(|root| path == root || path.starts_with(root));
    let canonical_match = std::fs::canonicalize(path).ok().is_some_and(|canonical| {
        roots
            .canonical
            .iter()
            .any(|root| canonical == *root || canonical.starts_with(root))
    });
    raw_match || canonical_match
}

#[derive(Debug)]
struct JobDependency {
    id: String,
    enabled: i64,
    raw_working_dir: Option<String>,
    wake_mode: String,
    wake_session_id: Option<String>,
    projection_exists: bool,
    projection_raw_working_dir: Option<String>,
    projection_wake_mode: Option<String>,
    projection_wake_session_id: Option<String>,
    projection_enabled: Option<i64>,
    canonical_working_dir: Option<String>,
    verification_state: Option<String>,
    evidence_digest: Option<String>,
    malformed: bool,
}

impl JobDependency {
    fn watched_session_id(&self) -> Option<&str> {
        self.wake_mode.strip_prefix("on_terminal:")
    }

    fn projection_is_current(&self) -> bool {
        if self.malformed
            || !self.projection_exists
            || self.enabled != 1
            || self.projection_raw_working_dir != self.raw_working_dir
            || self.projection_wake_mode.as_deref() != Some(self.wake_mode.as_str())
            || self.projection_wake_session_id != self.wake_session_id
            || self.projection_enabled != Some(self.enabled)
            || !scheduled_job_wake_authority_is_canonical(
                &self.wake_mode,
                self.wake_session_id.as_deref(),
            )
        {
            return false;
        }
        let current_canonical = self.raw_working_dir.as_deref().and_then(|raw| {
            std::fs::canonicalize(raw)
                .ok()
                .filter(|path| path.is_dir())
                .map(|path| path.to_string_lossy().into_owned())
        });
        let expected_state = if self.raw_working_dir.is_none() {
            "not_applicable"
        } else if current_canonical.is_some() {
            "verified"
        } else {
            return false;
        };
        if self.verification_state.as_deref() != Some(expected_state)
            || self.canonical_working_dir != current_canonical
        {
            return false;
        }
        self.evidence_digest.as_deref()
            == Some(
                projection_evidence_digest(
                    self.raw_working_dir.as_deref(),
                    self.canonical_working_dir.as_deref(),
                    &self.wake_mode,
                    self.wake_session_id.as_deref(),
                    self.enabled,
                )
                .as_str(),
            )
    }

    fn hash_into(&self, hash: &mut Sha256) {
        for field in [
            Some(self.id.as_str()),
            self.raw_working_dir.as_deref(),
            Some(self.wake_mode.as_str()),
            self.wake_session_id.as_deref(),
            self.canonical_working_dir.as_deref(),
            self.verification_state.as_deref(),
            self.evidence_digest.as_deref(),
        ] {
            hash_optional_field(hash, field);
        }
        hash.update(self.enabled.to_be_bytes());
    }
}

fn load_job_dependency(connection: &Connection, job_id: &str) -> Result<Option<JobDependency>> {
    connection
        .query_row(
            "SELECT CAST(j.id AS TEXT),
                    CASE WHEN typeof(j.enabled)='integer' THEN j.enabled END,
                    CASE WHEN j.working_dir IS NULL OR (typeof(j.working_dir)='text' AND octet_length(j.working_dir)<=4096) THEN j.working_dir END,
                    CASE WHEN typeof(j.wake_mode)='text' AND octet_length(j.wake_mode) BETWEEN 1 AND 4096 THEN j.wake_mode END,
                    CASE WHEN j.wake_session_id IS NULL OR (typeof(j.wake_session_id)='text' AND octet_length(j.wake_session_id)<=128) THEN j.wake_session_id END,
                    p.job_id IS NOT NULL,p.raw_working_dir,p.raw_wake_mode,p.raw_wake_session_id,p.raw_enabled,
                    p.canonical_working_dir,p.verification_state,p.evidence_digest,
                    typeof(j.enabled)!='integer' OR j.enabled NOT IN (0,1)
                      OR (j.working_dir IS NOT NULL AND (typeof(j.working_dir)!='text' OR octet_length(j.working_dir)>4096))
                      OR typeof(j.wake_mode)!='text' OR octet_length(j.wake_mode) NOT BETWEEN 1 AND 4096
                      OR (j.wake_session_id IS NOT NULL AND (typeof(j.wake_session_id)!='text' OR octet_length(j.wake_session_id)>128))
               FROM scheduled_jobs j LEFT JOIN scheduled_job_path_projections p ON p.job_id=j.id
              WHERE j.id=?1",
            [job_id],
            |row| {
                Ok(JobDependency {
                    id: row.get(0)?,
                    enabled: row.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                    raw_working_dir: row.get(2)?,
                    wake_mode: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    wake_session_id: row.get(4)?,
                    projection_exists: row.get(5)?,
                    projection_raw_working_dir: row.get(6)?,
                    projection_wake_mode: row.get(7)?,
                    projection_wake_session_id: row.get(8)?,
                    projection_enabled: row.get(9)?,
                    canonical_working_dir: row.get(10)?,
                    verification_state: row.get(11)?,
                    evidence_digest: row.get(12)?,
                    malformed: row.get(13)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

#[derive(Debug)]
struct SessionDependency {
    id: String,
    status: String,
    session_kind: String,
    working_dir: String,
    sandbox_root: Option<String>,
    sandbox_custody_id: Option<String>,
    projection_exists: bool,
    schema_version: Option<i64>,
    projection_version: Option<i64>,
    execution_state: Option<String>,
    freshness: Option<String>,
    canonical_repo_dir: Option<String>,
    effective_cwd: Option<String>,
    projection_custody_id: Option<String>,
    projection_generation: Option<i64>,
    projection_validated_at: Option<String>,
    projection_error_code: Option<String>,
    projection_updated_at: Option<String>,
    sandbox_kind: Option<String>,
    sandbox_branch: Option<String>,
    sandbox_cleanup_state: Option<String>,
    root_custody_id: Option<String>,
    root_state: Option<String>,
    root_owner_session_id: Option<String>,
    root_generation: Option<i64>,
    root_canonical_repo_dir: Option<String>,
    root_sandbox_root: Option<String>,
    root_sandbox_branch: Option<String>,
    root_validation_state: Option<String>,
    root_validated_generation: Option<i64>,
    root_validation_error_code: Option<String>,
    malformed: bool,
}

// RSI-RELEASED-MIGRATION-BEGIN: v120-source-worktree-session-relevance-helper
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionDependencyRelevance {
    Relevant,
    ProvenIrrelevant,
    Invalid,
}

impl SessionDependencyRelevance {
    const fn database_code(self) -> i64 {
        match self {
            Self::ProvenIrrelevant => 0,
            Self::Relevant => 1,
            Self::Invalid => 2,
        }
    }
}

impl SessionDependency {
    fn relevance(&self) -> SessionDependencyRelevance {
        use SessionDependencyRelevance::{Invalid, ProvenIrrelevant, Relevant};

        if matches!(self.session_kind.as_str(), "Group" | "Epic") {
            return ProvenIrrelevant;
        }
        if self.malformed
            || !matches!(
                self.session_kind.as_str(),
                "Standard"
                    | "TaskRabbit"
                    | "Bug"
                    | "Story"
                    | "Task"
                    | "Feature"
                    | "Refactor"
                    | "Research"
            )
            || !matches!(
                self.status.as_str(),
                "Starting"
                    | "Running"
                    | "WaitingApproval"
                    | "Completed"
                    | "Failed"
                    | "Interrupted"
                    | "Archived"
                    | "Deleted"
            )
        {
            return Invalid;
        }
        if matches!(
            self.status.as_str(),
            "Starting" | "Running" | "WaitingApproval"
        ) {
            return Relevant;
        }
        if !self.projection_is_authenticated() {
            return Invalid;
        }
        match self.execution_state.as_deref() {
            Some("ordinary_unsandboxed" | "live_sandboxed") => {
                if self.status == "Deleted" {
                    Invalid
                } else {
                    Relevant
                }
            }
            Some("historical_cleanup_failed") => {
                if self.historical_cleanup_failed_is_authenticated() {
                    ProvenIrrelevant
                } else {
                    Invalid
                }
            }
            Some("quarantined") => {
                if self.quarantined_is_authenticated() {
                    ProvenIrrelevant
                } else {
                    Invalid
                }
            }
            Some("historical_transferred") => {
                if self.historical_transferred_is_authenticated() {
                    ProvenIrrelevant
                } else {
                    Invalid
                }
            }
            Some("historical_purged") => {
                if self.historical_purged_is_authenticated() {
                    ProvenIrrelevant
                } else {
                    Invalid
                }
            }
            Some("invalid") => {
                if self.invalid_projection_is_authenticated() {
                    ProvenIrrelevant
                } else {
                    Invalid
                }
            }
            None | Some(_) => Invalid,
        }
    }

    fn projection_is_authenticated(&self) -> bool {
        self.projection_exists
            && self.schema_version == Some(1)
            && self.projection_version == Some(1)
            && self.projection_validated_at.is_some()
            && self.projection_updated_at.is_some()
    }

    fn rootless_projection(&self) -> bool {
        self.root_custody_id.is_none()
            && self.root_state.is_none()
            && self.root_owner_session_id.is_none()
            && self.root_generation.is_none()
            && self.root_canonical_repo_dir.is_none()
            && self.root_sandbox_root.is_none()
            && self.root_sandbox_branch.is_none()
            && self.root_validation_state.is_none()
            && self.root_validated_generation.is_none()
            && self.root_validation_error_code.is_none()
    }

    fn root_identity_matches(&self) -> bool {
        self.sandbox_custody_id == self.projection_custody_id
            && self.projection_custody_id == self.root_custody_id
            && self.sandbox_root == self.root_sandbox_root
            && self.sandbox_branch == self.root_sandbox_branch
            && self.canonical_repo_dir == self.root_canonical_repo_dir
            && self.working_dir == self.root_canonical_repo_dir.as_deref().unwrap_or_default()
            && self.projection_generation == self.root_generation
            && self.root_validated_generation == self.root_generation
            && self.root_validation_state.as_deref() == Some("verified")
            && self.root_validation_error_code.is_none()
    }

    fn historical_cleanup_failed_is_authenticated(&self) -> bool {
        let common = self.freshness.as_deref() == Some("verified")
            && self.effective_cwd.is_none()
            && self.projection_error_code.is_none()
            && self.sandbox_kind.as_deref() == Some("GitWorktree")
            && self.sandbox_cleanup_state.as_deref() == Some("Failed")
            && self.canonical_repo_dir.as_deref() == Some(self.working_dir.as_str());
        common
            && ((self.sandbox_root.is_none()
                && self.sandbox_branch.is_none()
                && self.sandbox_custody_id.is_none()
                && self.projection_custody_id.is_none()
                && self.projection_generation.is_none()
                && self.rootless_projection())
                || (self.root_identity_matches()
                    && self.root_state.as_deref() == Some("failed")
                    && self.root_owner_session_id.is_none()))
    }

    fn quarantined_is_authenticated(&self) -> bool {
        self.freshness.as_deref() == Some("invalid")
            && self.effective_cwd.is_none()
            && self.projection_error_code.is_some()
            && self.sandbox_kind.as_deref() == Some("GitWorktree")
            && self.sandbox_cleanup_state.as_deref() == Some("Failed")
            && self.canonical_repo_dir.as_deref() == Some(self.working_dir.as_str())
            && self.root_identity_matches()
            && self.root_state.as_deref() == Some("quarantined")
            && self.root_owner_session_id.is_none()
    }

    fn historical_transferred_is_authenticated(&self) -> bool {
        self.freshness.as_deref() == Some("verified")
            && self.effective_cwd.is_none()
            && self.projection_error_code.is_none()
            && self.sandbox_kind.as_deref() == Some("GitWorktree")
            && self.sandbox_cleanup_state.as_deref() == Some("Live")
            && self.canonical_repo_dir.as_deref() == Some(self.working_dir.as_str())
            && self.root_identity_matches_except_generation()
            && self.root_state.as_deref() == Some("live")
            && self.root_owner_session_id.as_deref() != Some(self.id.as_str())
            && matches!(
                (self.projection_generation, self.root_generation),
                (Some(projection), Some(root)) if projection > 0 && projection < root
            )
    }

    fn root_identity_matches_except_generation(&self) -> bool {
        self.sandbox_custody_id == self.projection_custody_id
            && self.projection_custody_id == self.root_custody_id
            && self.sandbox_root == self.root_sandbox_root
            && self.sandbox_branch == self.root_sandbox_branch
            && self.canonical_repo_dir == self.root_canonical_repo_dir
            && self.working_dir == self.root_canonical_repo_dir.as_deref().unwrap_or_default()
            && self.root_validated_generation == self.root_generation
            && self.root_validation_state.as_deref() == Some("verified")
            && self.root_validation_error_code.is_none()
    }

    fn historical_purged_is_authenticated(&self) -> bool {
        let common = self.freshness.as_deref() == Some("verified")
            && self.effective_cwd.is_none()
            && self.projection_error_code.is_none()
            && self.canonical_repo_dir.as_deref() == Some(self.working_dir.as_str());
        common
            && ((self.sandbox_root.is_none()
                && self.sandbox_branch.is_none()
                && self.sandbox_custody_id.is_none()
                && self.projection_custody_id.is_none()
                && self.projection_generation.is_none()
                && self.rootless_projection()
                && matches!(
                    (
                        self.sandbox_kind.as_deref(),
                        self.sandbox_cleanup_state.as_deref()
                    ),
                    (None, None) | (Some("GitWorktree"), Some("Purged"))
                ))
                || (self.sandbox_root.is_none()
                    && self.sandbox_branch.is_none()
                    && self.sandbox_kind.as_deref() == Some("GitWorktree")
                    && self.sandbox_cleanup_state.as_deref() == Some("Purged")
                    && self.sandbox_custody_id == self.projection_custody_id
                    && self.projection_custody_id == self.root_custody_id
                    && self.projection_generation == self.root_generation
                    && self.root_validated_generation == self.root_generation
                    && self.root_canonical_repo_dir.as_deref() == Some(self.working_dir.as_str())
                    && self.root_state.as_deref() == Some("purged")
                    && self.root_owner_session_id.is_none()
                    && self.root_validation_state.as_deref() == Some("verified")
                    && self.root_validation_error_code.is_none()))
    }

    fn invalid_projection_is_authenticated(&self) -> bool {
        self.freshness.as_deref() == Some("invalid")
            && self.effective_cwd.is_none()
            && self.projection_error_code.is_some()
            && self.canonical_repo_dir.as_deref() == Some(self.working_dir.as_str())
            && self.sandbox_root.is_none()
            && self.sandbox_branch.is_none()
            && self.sandbox_custody_id.is_none()
            && self.projection_custody_id.is_none()
            && self.projection_generation.is_none()
            && self.rootless_projection()
    }

    fn projection_is_current(&self) -> bool {
        if self.malformed
            || !self.projection_exists
            || self.schema_version != Some(1)
            || self.projection_version != Some(1)
            || self.freshness.as_deref() != Some("verified")
            || self.relevance() != SessionDependencyRelevance::Relevant
            || self
                .projection_custody_id
                .as_deref()
                .map(canonical_uuid)
                .transpose()
                .is_err()
            || self.projection_generation.is_some_and(|value| value <= 0)
        {
            return false;
        }
        let Some(effective_cwd) = self.effective_cwd.as_deref() else {
            return false;
        };
        let Some(canonical_repo_dir) = self.canonical_repo_dir.as_deref() else {
            return false;
        };
        let working_dir_is_current = std::fs::canonicalize(&self.working_dir)
            .ok()
            .filter(|path| path.is_dir())
            .is_some_and(|path| path.to_string_lossy() == canonical_repo_dir);
        let repository_is_canonical = std::fs::canonicalize(canonical_repo_dir)
            .ok()
            .filter(|path| path.is_dir())
            .is_some_and(|path| path.to_string_lossy() == canonical_repo_dir);
        if !working_dir_is_current || !repository_is_canonical {
            return false;
        }
        let effective_is_current = std::fs::canonicalize(effective_cwd)
            .ok()
            .filter(|path| path.is_dir())
            .is_some_and(|path| path.to_string_lossy() == effective_cwd);
        if !effective_is_current {
            return false;
        }
        match self.execution_state.as_deref() {
            Some("ordinary_unsandboxed") => true,
            Some("live_sandboxed") => self
                .sandbox_root
                .as_deref()
                .and_then(|raw| std::fs::canonicalize(raw).ok())
                .filter(|path| path.is_dir())
                .is_some_and(|path| path.to_string_lossy() == effective_cwd),
            _ => false,
        }
    }

    fn hash_into(&self, hash: &mut Sha256) {
        for field in [
            Some(self.id.as_str()),
            Some(self.status.as_str()),
            Some(self.session_kind.as_str()),
            Some(self.working_dir.as_str()),
            self.sandbox_root.as_deref(),
            self.sandbox_custody_id.as_deref(),
            self.execution_state.as_deref(),
            self.freshness.as_deref(),
            self.canonical_repo_dir.as_deref(),
            self.effective_cwd.as_deref(),
            self.projection_custody_id.as_deref(),
        ] {
            hash_optional_field(hash, field);
        }
        hash.update(self.projection_generation.unwrap_or_default().to_be_bytes());
    }
}

fn required_sql_text(value: ValueRef<'_>, maximum_bytes: usize, malformed: &mut bool) -> String {
    match value {
        ValueRef::Text(bytes) if bytes.len() <= maximum_bytes => match std::str::from_utf8(bytes) {
            Ok(value) => value.to_owned(),
            Err(_) => {
                *malformed = true;
                String::new()
            }
        },
        _ => {
            *malformed = true;
            String::new()
        }
    }
}

fn optional_sql_text(
    value: ValueRef<'_>,
    maximum_bytes: usize,
    malformed: &mut bool,
) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Text(bytes) if bytes.len() <= maximum_bytes => match std::str::from_utf8(bytes) {
            Ok(value) => Some(value.to_owned()),
            Err(_) => {
                *malformed = true;
                None
            }
        },
        _ => {
            *malformed = true;
            None
        }
    }
}

fn optional_sql_integer(value: ValueRef<'_>, malformed: &mut bool) -> Option<i64> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(value) => Some(value),
        _ => {
            *malformed = true;
            None
        }
    }
}

fn session_dependency_from_value_refs<'a>(
    mut value: impl FnMut(usize) -> rusqlite::Result<ValueRef<'a>>,
) -> rusqlite::Result<SessionDependency> {
    let mut malformed = false;
    let id = required_sql_text(value(0)?, 128, &mut malformed);
    if canonical_uuid(&id).is_err() {
        malformed = true;
    }
    let status = required_sql_text(value(1)?, 64, &mut malformed);
    let session_kind = required_sql_text(value(2)?, 64, &mut malformed);
    let working_dir = required_sql_text(value(3)?, 4096, &mut malformed);
    let sandbox_root = optional_sql_text(value(4)?, 4096, &mut malformed);
    let sandbox_custody_id = optional_sql_text(value(5)?, 128, &mut malformed);
    let projection_exists = match value(6)? {
        ValueRef::Integer(value @ (0 | 1)) => value == 1,
        _ => {
            malformed = true;
            false
        }
    };
    let schema_version = optional_sql_integer(value(7)?, &mut malformed);
    let projection_version = optional_sql_integer(value(8)?, &mut malformed);
    let execution_state = optional_sql_text(value(9)?, 64, &mut malformed);
    let freshness = optional_sql_text(value(10)?, 64, &mut malformed);
    let canonical_repo_dir = optional_sql_text(value(11)?, 4096, &mut malformed);
    let effective_cwd = optional_sql_text(value(12)?, 4096, &mut malformed);
    let projection_custody_id = optional_sql_text(value(13)?, 128, &mut malformed);
    let projection_generation = optional_sql_integer(value(14)?, &mut malformed);
    let projection_validated_at = optional_sql_text(value(15)?, 64, &mut malformed);
    let projection_error_code = optional_sql_text(value(16)?, 128, &mut malformed);
    let projection_updated_at = optional_sql_text(value(17)?, 64, &mut malformed);
    let sandbox_kind = optional_sql_text(value(18)?, 64, &mut malformed);
    let sandbox_branch = optional_sql_text(value(19)?, 4096, &mut malformed);
    let sandbox_cleanup_state = optional_sql_text(value(20)?, 64, &mut malformed);
    let root_custody_id = optional_sql_text(value(21)?, 128, &mut malformed);
    let root_state = optional_sql_text(value(22)?, 64, &mut malformed);
    let root_owner_session_id = optional_sql_text(value(23)?, 128, &mut malformed);
    let root_generation = optional_sql_integer(value(24)?, &mut malformed);
    let root_canonical_repo_dir = optional_sql_text(value(25)?, 4096, &mut malformed);
    let root_sandbox_root = optional_sql_text(value(26)?, 4096, &mut malformed);
    let root_sandbox_branch = optional_sql_text(value(27)?, 4096, &mut malformed);
    let root_validation_state = optional_sql_text(value(28)?, 64, &mut malformed);
    let root_validated_generation = optional_sql_integer(value(29)?, &mut malformed);
    let root_validation_error_code = optional_sql_text(value(30)?, 128, &mut malformed);

    for candidate in [
        sandbox_custody_id.as_deref(),
        projection_custody_id.as_deref(),
        root_custody_id.as_deref(),
        root_owner_session_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if canonical_uuid(candidate).is_err() {
            malformed = true;
        }
    }
    if projection_generation.is_some_and(|value| value <= 0)
        || root_generation.is_some_and(|value| value <= 0)
        || root_validated_generation.is_some_and(|value| value <= 0)
    {
        malformed = true;
    }

    Ok(SessionDependency {
        id,
        status,
        session_kind,
        working_dir,
        sandbox_root,
        sandbox_custody_id,
        projection_exists,
        schema_version,
        projection_version,
        execution_state,
        freshness,
        canonical_repo_dir,
        effective_cwd,
        projection_custody_id,
        projection_generation,
        projection_validated_at,
        projection_error_code,
        projection_updated_at,
        sandbox_kind,
        sandbox_branch,
        sandbox_cleanup_state,
        root_custody_id,
        root_state,
        root_owner_session_id,
        root_generation,
        root_canonical_repo_dir,
        root_sandbox_root,
        root_sandbox_branch,
        root_validation_state,
        root_validated_generation,
        root_validation_error_code,
        malformed,
    })
}

pub(super) fn session_dependency_relevance_sql(
    context: &rusqlite::functions::Context<'_>,
) -> rusqlite::Result<i64> {
    let dependency = session_dependency_from_value_refs(|index| Ok(context.get_raw(index)))?;
    Ok(dependency.relevance().database_code())
}
// RSI-RELEASED-MIGRATION-END: v120-source-worktree-session-relevance-helper

fn load_session_dependency(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<SessionDependency>> {
    connection
        .query_row(
            "SELECT s.id,s.status,s.session_kind,s.working_dir,s.sandbox_root,s.sandbox_custody_id,
                    p.session_id IS NOT NULL,p.schema_version,p.projection_version,p.execution_state,p.freshness,
                    p.canonical_repo_dir,p.effective_cwd,p.custody_id,p.custody_generation,
                    p.validated_at,p.error_code,p.updated_at,
                    s.sandbox_kind,s.sandbox_branch,s.sandbox_cleanup_state,
                    r.custody_id,r.state,r.owner_session_id,r.generation,r.canonical_repo_dir,
                    r.sandbox_root,r.sandbox_branch,r.validation_state,r.validated_generation,
                    r.validation_error_code
               FROM sessions s LEFT JOIN session_execution_projections p ON p.session_id=s.id
               LEFT JOIN sandbox_custody_roots r ON r.custody_id=p.custody_id
              WHERE s.id=?1",
            [session_id],
            |row| session_dependency_from_value_refs(|index| row.get_ref(index)),
        )
        .optional()
        .map_err(Into::into)
}

fn validate_repository_identity(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 4096 || value.contains('\0') {
        return Err(DaemonError::InvalidParam(
            "invalid repository identity".into(),
        ));
    }
    Ok(())
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid> {
    canonical_uuid(value)
        .map_err(|_| DaemonError::Store(format!("{field} is not a canonical UUID")))
}

fn positive_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| DaemonError::Store(format!("{field} is not positive")))
}

fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| DaemonError::Store(format!("{field} is negative")))
}

fn canonical_source_ref(branch: &str) -> Result<String> {
    if branch.starts_with("refs/") {
        return Err(DaemonError::Store(
            "sandbox branch must be stored without refs/heads prefix".into(),
        ));
    }
    let source_ref = format!("refs/heads/{branch}");
    validate_source_ref(&source_ref).map_err(DaemonError::Store)?;
    Ok(source_ref)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceWorktreeTargetedDependenciesV2 {
    pub complete: bool,
    pub scheduled_dependency_count: u32,
    pub scheduled_dependency_digest: String,
    pub session_path_dependency_count: u32,
    pub session_path_dependency_digest: String,
    pub reason: Option<&'static str>,
}

impl SourceWorktreeTargetedDependenciesV2 {
    fn incomplete(reason: &'static str) -> Self {
        Self {
            complete: false,
            scheduled_dependency_count: 0,
            scheduled_dependency_digest: "sha256:incomplete".into(),
            session_path_dependency_count: 0,
            session_path_dependency_digest: "sha256:incomplete".into(),
            reason: Some(reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CursorClaimsV2 {
    pub schema_version: u32,
    pub policy_version: u32,
    pub repository_identity_digest: String,
    pub snapshot_digest: String,
    pub snapshot_count: u32,
    pub target_ref: String,
    pub target_oid: String,
    pub upper_custody_id: Option<String>,
    pub batch_ordinal: u64,
    pub page_start_custody_id: Option<String>,
    pub page_end_custody_id: Option<String>,
    pub has_more: bool,
    pub previous_receipt_digest: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct CursorFenceV2<'a> {
    pub target_ref: &'a str,
    pub target_oid: &'a str,
    pub batch_ordinal: u64,
    pub previous_receipt_digest: Option<&'a str>,
}

pub(crate) fn issue_cursor_v2(
    connection: &Connection,
    repository_identity: &str,
    snapshot: &SourceWorktreeSnapshotCaptureV2,
    fence: &CursorFenceV2<'_>,
) -> Result<SourceWorktreeBatchCursorV2> {
    validate_repository_identity(repository_identity)?;
    validate_sha256_digest(snapshot.repository_snapshot_digest(), "snapshot digest")?;
    validate_target_ref(fence.target_ref).map_err(DaemonError::InvalidParam)?;
    validate_git_oid(fence.target_oid).map_err(DaemonError::InvalidParam)?;
    let page_start_custody_id = snapshot
        .page()
        .items
        .first()
        .map(|item| item.key.custody_id);
    let page_end_custody_id = snapshot.page().items.last().map(|item| item.key.custody_id);
    validate_cursor_fences(
        snapshot.root_count(),
        snapshot.upper().map(|key| key.custody_id),
        page_start_custody_id,
        page_end_custody_id,
        snapshot.page().has_more,
    )?;
    if let Some(digest) = fence.previous_receipt_digest {
        validate_sha256_digest(digest, "predecessor terminal receipt digest")?;
    }
    let claims = CursorClaimsV2 {
        schema_version: SOURCE_WORKTREE_BATCH_SCHEMA_VERSION,
        policy_version: SOURCE_WORKTREE_BATCH_POLICY_VERSION,
        repository_identity_digest: hex::encode(
            blake3::hash(repository_identity.as_bytes()).as_bytes(),
        ),
        snapshot_digest: snapshot.repository_snapshot_digest().to_string(),
        snapshot_count: snapshot.root_count(),
        target_ref: fence.target_ref.to_string(),
        target_oid: fence.target_oid.to_string(),
        upper_custody_id: snapshot.upper().map(|key| key.custody_id.to_string()),
        batch_ordinal: fence.batch_ordinal,
        page_start_custody_id: page_start_custody_id.map(|id| id.to_string()),
        page_end_custody_id: page_end_custody_id.map(|id| id.to_string()),
        has_more: snapshot.page().has_more,
        previous_receipt_digest: fence.previous_receipt_digest.map(str::to_string),
    };
    let payload =
        serde_json::to_vec(&claims).map_err(|error| DaemonError::Store(error.to_string()))?;
    let key = cursor_key(connection)?;
    let mac = blake3::keyed_hash(&key, &payload);
    let cursor = format!(
        "{CURSOR_PREFIX}{}:{}",
        hex::encode(payload),
        hex::encode(mac.as_bytes())
    );
    if cursor.len() > SOURCE_WORKTREE_BATCH_MAX_CURSOR_BYTES {
        return Err(DaemonError::Store(
            "issued source-worktree cursor exceeds its bound".into(),
        ));
    }
    Ok(SourceWorktreeBatchCursorV2(cursor))
}

pub(crate) fn verify_cursor_v2(
    connection: &Connection,
    cursor: &SourceWorktreeBatchCursorV2,
    repository_identity: &str,
) -> Result<CursorClaimsV2> {
    validate_repository_identity(repository_identity)?;
    cursor.validate().map_err(DaemonError::InvalidParam)?;
    let Some(rest) = cursor.0.strip_prefix(CURSOR_PREFIX) else {
        return Err(DaemonError::InvalidParam(
            "malformed source-worktree cursor".into(),
        ));
    };
    let Some((payload_hex, mac_hex)) = rest.split_once(':') else {
        return Err(DaemonError::InvalidParam(
            "malformed source-worktree cursor".into(),
        ));
    };
    let payload = hex::decode(payload_hex)
        .map_err(|_| DaemonError::InvalidParam("malformed source-worktree cursor".into()))?;
    let supplied_mac = hex::decode(mac_hex)
        .map_err(|_| DaemonError::InvalidParam("malformed source-worktree cursor".into()))?;
    if supplied_mac.len() != 32 {
        return Err(DaemonError::InvalidParam(
            "malformed source-worktree cursor".into(),
        ));
    }
    let key = cursor_key(connection)?;
    let expected_mac = blake3::keyed_hash(&key, &payload);
    let different = expected_mac
        .as_bytes()
        .iter()
        .zip(&supplied_mac)
        .fold(0_u8, |difference, (expected, supplied)| {
            difference | (expected ^ supplied)
        });
    if different != 0 {
        return Err(DaemonError::InvalidParam(
            "source-worktree cursor MAC is invalid".into(),
        ));
    }
    let claims: CursorClaimsV2 = serde_json::from_slice(&payload).map_err(|_| {
        DaemonError::InvalidParam("source-worktree cursor claims are invalid".into())
    })?;
    let canonical = serde_json::to_vec(&claims).map_err(|_| {
        DaemonError::InvalidParam("source-worktree cursor claims are invalid".into())
    })?;
    if canonical != payload {
        return Err(DaemonError::InvalidParam(
            "source-worktree cursor payload is not canonical".into(),
        ));
    }
    if claims.schema_version != SOURCE_WORKTREE_BATCH_SCHEMA_VERSION
        || claims.policy_version != SOURCE_WORKTREE_BATCH_POLICY_VERSION
        || claims.repository_identity_digest
            != hex::encode(blake3::hash(repository_identity.as_bytes()).as_bytes())
    {
        return Err(DaemonError::InvalidParam(
            "source-worktree cursor does not bind this repository".into(),
        ));
    }
    validate_sha256_digest(&claims.snapshot_digest, "snapshot digest")?;
    validate_target_ref(&claims.target_ref).map_err(DaemonError::InvalidParam)?;
    validate_git_oid(&claims.target_oid).map_err(DaemonError::InvalidParam)?;
    let upper = claims
        .upper_custody_id
        .as_deref()
        .map(|value| parse_cursor_uuid(value, "upper custody id"))
        .transpose()?;
    let start = claims
        .page_start_custody_id
        .as_deref()
        .map(|value| parse_cursor_uuid(value, "page start custody id"))
        .transpose()?;
    let end = claims
        .page_end_custody_id
        .as_deref()
        .map(|value| parse_cursor_uuid(value, "page end custody id"))
        .transpose()?;
    validate_cursor_fences(claims.snapshot_count, upper, start, end, claims.has_more)?;
    if let Some(digest) = claims.previous_receipt_digest.as_deref() {
        validate_sha256_digest(digest, "predecessor terminal receipt digest")?;
    }
    Ok(claims)
}

fn parse_cursor_uuid(value: &str, field: &str) -> Result<Uuid> {
    canonical_uuid(value).map_err(|_| {
        DaemonError::InvalidParam(format!("source-worktree cursor {field} is invalid"))
    })
}

fn validate_cursor_fences(
    snapshot_count: u32,
    upper: Option<Uuid>,
    start: Option<Uuid>,
    end: Option<Uuid>,
    has_more: bool,
) -> Result<()> {
    let page_valid = match (snapshot_count, upper, start, end) {
        (0, None, None, None) => !has_more,
        (count, Some(_), None, None) if count > 0 => !has_more,
        (count, Some(upper), Some(start), Some(end)) if count > 0 => {
            start <= end && end <= upper && (!has_more || end < upper)
        }
        _ => false,
    };
    if snapshot_count as usize > SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS || !page_valid {
        return Err(DaemonError::InvalidParam(
            "source-worktree cursor fences are invalid".into(),
        ));
    }
    Ok(())
}

fn validate_sha256_digest(value: &str, field: &str) -> Result<()> {
    let valid = value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid {
        return Err(DaemonError::InvalidParam(format!(
            "source-worktree cursor {field} is invalid"
        )));
    }
    Ok(())
}

fn cursor_key(connection: &Connection) -> Result<[u8; 32]> {
    let key: Vec<u8> = connection.query_row(
        "SELECT cursor_key FROM source_worktree_batch_cursor_keys WHERE key_id=1",
        [],
        |row| row.get(0),
    )?;
    key.try_into()
        .map_err(|_| DaemonError::Store("V120 cursor key is malformed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::rewind_store_to_schema_version;
    use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
    use rusqlite::types::ToSql;
    use std::fs;

    const TEST_NOW: &str = "2099-01-01T00:00:00.000000000Z";

    fn insert_test_session(
        connection: &Connection,
        session_id: Uuid,
        working_dir: &Path,
        status: &str,
    ) {
        connection
            .execute(
                "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at)
                 VALUES(?1,'V120 source-worktree fixture',?2,?3,?4,?4)",
                params![
                    session_id.to_string(),
                    working_dir.to_string_lossy().into_owned(),
                    status,
                    TEST_NOW,
                ],
            )
            .expect("insert V120 fixture Session");
    }

    fn verify_test_session_projection(
        connection: &Connection,
        session_id: Uuid,
        cwd: &Path,
        custody: Option<(Uuid, u64)>,
    ) {
        let cwd = fs::canonicalize(cwd).expect("canonicalize V120 fixture cwd");
        let repository_dir: String = connection
            .query_row(
                "SELECT working_dir FROM sessions WHERE id=?1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .expect("read V120 fixture Session working directory");
        let repository_dir = fs::canonicalize(repository_dir)
            .expect("canonicalize V120 fixture Session working directory");
        let (execution_state, custody_id, generation) = match custody {
            Some((custody_id, generation)) => (
                "live_sandboxed",
                Some(custody_id.to_string()),
                Some(generation as i64),
            ),
            None => ("ordinary_unsandboxed", None, None),
        };
        connection
            .execute(
                "UPDATE session_execution_projections
                    SET execution_state=?1,freshness='verified',canonical_repo_dir=?2,
                        effective_cwd=?3,custody_id=?4,custody_generation=?5,
                        validated_at=?6,error_code=NULL,updated_at=?6
                  WHERE session_id=?7",
                params![
                    execution_state,
                    repository_dir.to_string_lossy().into_owned(),
                    cwd.to_string_lossy().into_owned(),
                    custody_id,
                    generation,
                    TEST_NOW,
                    session_id.to_string(),
                ],
            )
            .expect("verify V120 fixture Session projection");
    }

    fn insert_test_root(
        connection: &Connection,
        owner_session_id: Uuid,
        custody_id: Uuid,
        generation: u64,
        repository_identity: &str,
        repository_dir: &Path,
        sandbox_root: &Path,
        branch: &str,
    ) {
        connection
            .execute(
                "INSERT INTO sandbox_custody_roots(
                    custody_id,canonical_repo_dir,sandbox_root,sandbox_branch,
                    repository_identity,source_commit,state,owner_session_id,generation,
                    event_sequence,validation_state,validated_generation,validated_at,
                    validation_error_code,effect_boot_id,reserved_effects,active_effects,
                    created_at,updated_at,tombstoned_at)
                 VALUES(?1,?2,?3,?4,?5,?6,'live',?7,?8,?8,'verified',?8,?9,
                        NULL,NULL,0,0,?9,?9,NULL)",
                params![
                    custody_id.to_string(),
                    repository_dir.to_string_lossy().into_owned(),
                    sandbox_root.to_string_lossy().into_owned(),
                    branch,
                    repository_identity,
                    "a".repeat(40),
                    owner_session_id.to_string(),
                    generation as i64,
                    TEST_NOW,
                ],
            )
            .expect("insert V120 fixture custody root");
    }

    fn link_test_session_to_root(
        connection: &Connection,
        session_id: Uuid,
        custody_id: Uuid,
        sandbox_root: &Path,
        branch: &str,
    ) {
        connection
            .execute(
                "UPDATE sessions
                    SET sandbox_kind='GitWorktree',sandbox_root=?1,sandbox_branch=?2,
                        sandbox_cleanup_state='Live',sandbox_custody_id=?3
                  WHERE id=?4",
                params![
                    sandbox_root.to_string_lossy().into_owned(),
                    branch,
                    custody_id.to_string(),
                    session_id.to_string(),
                ],
            )
            .expect("link V120 fixture Session to custody root");
    }

    fn test_job(
        name: &str,
        working_dir: Option<PathBuf>,
        wake_mode: WakeMode,
        wake_session_id: Option<Uuid>,
        enabled: bool,
    ) -> ScheduledJob {
        let now = Utc::now();
        ScheduledJob {
            id: Uuid::new_v4(),
            name: name.into(),
            message: format!("V120 fixture {name}"),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled,
            working_dir,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode,
            wake_session_id,
        }
    }

    fn explain(connection: &Connection, sql: &str, values: &[&dyn ToSql]) -> String {
        connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare V120 EXPLAIN")
            .query_map(values, |row| row.get::<_, String>(3))
            .expect("execute V120 EXPLAIN")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect V120 EXPLAIN")
            .join("\n")
    }

    #[test]
    fn v120_cursor_rejects_tamper_cross_repository_and_oversize() {
        let store = Store::open_in_memory().expect("open V120 store");
        validate_v120_catalog(&store.conn).expect("V120 catalog");
        let snapshot = store
            .source_worktree_snapshot_capture("repo:v120", None)
            .expect("capture empty cursor snapshot");
        let cursor = issue_cursor_v2(
            &store.conn,
            "repo:v120",
            &snapshot,
            &CursorFenceV2 {
                target_ref: "refs/heads/rolling",
                target_oid: &"b".repeat(40),
                batch_ordinal: 7,
                previous_receipt_digest: None,
            },
        )
        .expect("issue cursor");
        verify_cursor_v2(&store.conn, &cursor, "repo:v120").expect("verify cursor");
        assert!(verify_cursor_v2(&store.conn, &cursor, "repo:other").is_err());
        let mut tampered = cursor.clone();
        let last = tampered.0.pop().unwrap();
        tampered.0.push(if last == '0' { '1' } else { '0' });
        assert!(verify_cursor_v2(&store.conn, &tampered, "repo:v120").is_err());
        assert!(
            SourceWorktreeBatchCursorV2(format!("rsi-swc2:{}", "a".repeat(4096)))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn v120_cursor_authenticates_before_decode_and_rejects_noncanonical_claims_and_fences() {
        let store = Store::open_in_memory().expect("open V120 cursor store");
        for ordinal in 1..=2_u128 {
            let owner = Uuid::from_u128(0x500 + ordinal);
            insert_test_session(&store.conn, owner, Path::new("/repository"), "Completed");
            insert_test_root(
                &store.conn,
                owner,
                Uuid::from_u128(ordinal),
                1,
                "repo:v120:cursor",
                Path::new("/repository"),
                Path::new(&format!("/sandbox/{owner}")),
                &format!("rsi/v120-cursor-{ordinal}"),
            );
        }
        let snapshot = store
            .source_worktree_snapshot_capture("repo:v120:cursor", None)
            .expect("capture cursor root snapshot");
        let cursor = issue_cursor_v2(
            &store.conn,
            "repo:v120:cursor",
            &snapshot,
            &CursorFenceV2 {
                target_ref: "refs/heads/rolling",
                target_oid: &"2".repeat(40),
                batch_ordinal: 9,
                previous_receipt_digest: Some(&format!("sha256:{}", "3".repeat(64))),
            },
        )
        .expect("issue fully bound V120 cursor");
        let claims = verify_cursor_v2(&store.conn, &cursor, "repo:v120:cursor")
            .expect("authenticate fully bound V120 cursor");
        assert_eq!(claims.batch_ordinal, 9);
        assert_eq!(claims.snapshot_count, 2);
        assert_eq!(claims.target_ref, "refs/heads/rolling");
        assert_eq!(
            claims.page_end_custody_id,
            Some(Uuid::from_u128(2).to_string())
        );

        let key = cursor_key(&store.conn).expect("load internal test key");
        let pretty_payload = serde_json::to_vec_pretty(&claims).expect("encode noncanonical JSON");
        let pretty_mac = blake3::keyed_hash(&key, &pretty_payload);
        let noncanonical = SourceWorktreeBatchCursorV2(format!(
            "{CURSOR_PREFIX}{}:{}",
            hex::encode(pretty_payload),
            hex::encode(pretty_mac.as_bytes())
        ));
        assert!(noncanonical.validate().is_ok());
        let error = verify_cursor_v2(&store.conn, &noncanonical, "repo:v120:cursor")
            .expect_err("valid MAC must not bless noncanonical JSON");
        assert!(error.to_string().contains("not canonical"), "{error}");

        let mut unknown = serde_json::to_value(&claims).expect("encode V120 claims");
        unknown["caller_path"] = serde_json::json!("/forbidden");
        let unknown_payload = serde_json::to_vec(&unknown).expect("encode unknown claim");
        let unknown_mac = blake3::keyed_hash(&key, &unknown_payload);
        let unknown_cursor = SourceWorktreeBatchCursorV2(format!(
            "{CURSOR_PREFIX}{}:{}",
            hex::encode(unknown_payload),
            hex::encode(unknown_mac.as_bytes())
        ));
        assert!(
            verify_cursor_v2(&store.conn, &unknown_cursor, "repo:v120:cursor").is_err(),
            "deny_unknown_fields must survive a valid MAC"
        );

        let mut mac_tamper = cursor.clone();
        let separator = mac_tamper.0.rfind(':').expect("cursor MAC separator") + 1;
        let original = mac_tamper.0.as_bytes()[separator] as char;
        mac_tamper.0.replace_range(
            separator..separator + 1,
            if original == '0' { "1" } else { "0" },
        );
        let error = verify_cursor_v2(&store.conn, &mac_tamper, "repo:v120:cursor")
            .expect_err("deterministic MAC byte tamper");
        assert!(error.to_string().contains("MAC is invalid"), "{error}");

        let mut reversed = claims.clone();
        reversed.page_start_custody_id = Some(Uuid::from_u128(30).to_string());
        reversed.page_end_custody_id = Some(Uuid::from_u128(20).to_string());
        reversed.has_more = true;
        let reversed_payload = serde_json::to_vec(&reversed).expect("encode reversed claims");
        let reversed_mac = blake3::keyed_hash(&key, &reversed_payload);
        let reversed_cursor = SourceWorktreeBatchCursorV2(format!(
            "{CURSOR_PREFIX}{}:{}",
            hex::encode(reversed_payload),
            hex::encode(reversed_mac.as_bytes())
        ));
        assert!(
            verify_cursor_v2(&store.conn, &reversed_cursor, "repo:v120:cursor").is_err(),
            "a valid MAC must not bless reversed keyset fences"
        );
    }

    #[test]
    fn v120_snapshot_page_uses_repository_keyset_index() {
        let directory = tempfile::tempdir().expect("create V120 test directory");
        let store =
            Store::open(&directory.path().join("snapshot.sqlite")).expect("open V120 store");
        let plan: String = store
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT custody_id,generation,sandbox_root,sandbox_branch
               FROM sandbox_custody_roots INDEXED BY idx_swc_v120_roots_repository_page
              WHERE repository_identity=?1 AND state='live'
                AND (?2 IS NULL OR custody_id>?2)
              ORDER BY custody_id LIMIT ?3",
                params!["repo:v120", Option::<String>::None, 256_i64],
                |row| row.get(3),
            )
            .expect("query plan");
        assert!(
            plan.contains("idx_swc_v120_roots_repository_page"),
            "{plan}"
        );
        assert!(
            store
                .source_worktree_snapshot_capture("repo:v120", None)
                .unwrap()
                .page
                .items
                .is_empty()
        );
    }

    #[test]
    fn source_worktree_snapshot_pages_513_roots_with_finite_upper_and_late_page_evidence() {
        let directory = tempfile::tempdir().expect("create V120 snapshot fixture");
        let repository_dir = directory.path().join("repository");
        let roots_dir = directory.path().join("roots");
        fs::create_dir_all(&repository_dir).expect("create repository fixture");
        fs::create_dir_all(&roots_dir).expect("create root fixture base");
        let store = Store::open(&directory.path().join("snapshot-513.sqlite"))
            .expect("open V120 snapshot fixture");
        let repository_identity = "repo:v120:snapshot-513";
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open V120 snapshot seed transaction");
        for ordinal in 1..=513_u128 {
            let custody_id = Uuid::from_u128(ordinal);
            let owner_id = Uuid::from_u128(0x10000 + ordinal);
            insert_test_session(&tx, owner_id, &repository_dir, "Completed");
            verify_test_session_projection(&tx, owner_id, &repository_dir, None);
            let branch = if matches!(ordinal, 1 | 513) {
                "rsi/v120-collision".to_string()
            } else {
                format!("rsi/v120-{ordinal:04}")
            };
            insert_test_root(
                &tx,
                owner_id,
                custody_id,
                1,
                repository_identity,
                &repository_dir,
                &roots_dir.join(owner_id.to_string()),
                &branch,
            );
        }
        tx.commit().expect("commit V120 snapshot seed");
        let last_owner = Uuid::from_u128(0x10000 + 513);
        let last_root = roots_dir.join(last_owner.to_string());
        let last_nested = last_root.join("nested");
        fs::create_dir_all(&last_nested).expect("create late-page dependency root");

        let first = store
            .source_worktree_snapshot_capture(repository_identity, None)
            .expect("capture first V120 snapshot page");
        assert_eq!(first.root_count, 513);
        assert_eq!(
            first.upper.as_ref().unwrap().custody_id,
            Uuid::from_u128(513)
        );
        assert_eq!(first.page.items.len(), 256);
        assert!(first.page.has_more);
        assert_eq!(first.page.items[0].key.custody_id, Uuid::from_u128(1));
        let second = store
            .source_worktree_snapshot_capture(
                repository_identity,
                Some(&first.page.items.last().unwrap().key),
            )
            .expect("read second V120 snapshot page");
        assert_eq!(
            second.repository_snapshot_digest,
            first.repository_snapshot_digest
        );
        assert_eq!(second.root_count, first.root_count);
        assert_eq!(second.upper, first.upper);
        assert_eq!(second.page.items.len(), 256);
        assert!(second.page.has_more);
        let third = store
            .source_worktree_snapshot_capture(
                repository_identity,
                Some(&second.page.items.last().unwrap().key),
            )
            .expect("read terminal V120 snapshot page");
        assert_eq!(
            third.repository_snapshot_digest,
            first.repository_snapshot_digest
        );
        assert_eq!(third.page.items.len(), 1);
        assert!(!third.page.has_more);
        assert_eq!(third.page.items[0].key.custody_id, Uuid::from_u128(513));

        assert!(
            store
                .source_worktree_global_ref_collision(
                    repository_identity,
                    "refs/heads/rsi/v120-collision",
                    Uuid::from_u128(1),
                )
                .expect("check repository-global ref collision"),
            "the same ref on row 513 must be visible outside page one"
        );
        let dependency = test_job(
            "late-page-path",
            Some(last_nested),
            WakeMode::Fresh,
            None,
            true,
        );
        store
            .insert_scheduled_job(&dependency)
            .expect("insert late-page dependency");
        let evidence = store
            .source_worktree_targeted_dependencies(Uuid::from_u128(513), 1)
            .expect("read late-page dependency evidence");
        assert!(evidence.complete, "{:?}", evidence.reason);
        assert_eq!(evidence.scheduled_dependency_count, 1);
    }

    #[test]
    fn source_worktree_snapshot_capture_excludes_concurrent_lower_keys_consistently() {
        let directory = tempfile::tempdir().expect("create concurrent snapshot fixture");
        let database = directory.path().join("snapshot-concurrent.sqlite");
        let repository_dir = directory.path().join("repository");
        let roots_dir = directory.path().join("roots");
        fs::create_dir_all(&repository_dir).expect("create concurrent repository");
        fs::create_dir_all(&roots_dir).expect("create concurrent roots base");
        let store = Store::open(&database).expect("open concurrent snapshot Store");
        let repository_identity = "repo:v120:snapshot-concurrent";
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open concurrent snapshot seed transaction");
        for ordinal in 1..=300_u128 {
            let custody_id = Uuid::from_u128(ordinal * 2);
            let owner_id = Uuid::from_u128(0x20000 + ordinal);
            insert_test_session(&tx, owner_id, &repository_dir, "Completed");
            insert_test_root(
                &tx,
                owner_id,
                custody_id,
                1,
                repository_identity,
                &repository_dir,
                &roots_dir.join(owner_id.to_string()),
                &format!("rsi/v120-concurrent-{ordinal}"),
            );
        }
        tx.commit().expect("commit concurrent snapshot seed");
        let baseline = store
            .source_worktree_snapshot_capture(repository_identity, None)
            .expect("capture baseline snapshot");
        let after = baseline.page.items.last().unwrap().key.clone();
        assert_eq!(after.custody_id, Uuid::from_u128(512));

        let writer = Connection::open(&database).expect("open concurrent snapshot writer");
        Store::register_sql_functions(&writer).expect("register concurrent writer functions");
        writer
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")
            .expect("configure concurrent snapshot writer");
        let concurrent = store
            .source_worktree_snapshot_capture_with_hook(repository_identity, Some(&after), || {
                let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)
                    .expect("open concurrent insertion transaction");
                for (custody, owner, label) in [
                    (1_u128, 0x30001_u128, "below-after"),
                    (513_u128, 0x30002_u128, "remaining-range"),
                ] {
                    let owner = Uuid::from_u128(owner);
                    insert_test_session(&tx, owner, &repository_dir, "Completed");
                    insert_test_root(
                        &tx,
                        owner,
                        Uuid::from_u128(custody),
                        1,
                        repository_identity,
                        &repository_dir,
                        &roots_dir.join(owner.to_string()),
                        &format!("rsi/v120-{label}"),
                    );
                }
                tx.commit().expect("commit concurrent lower-key insertions");
            })
            .expect("capture one consistent SQLite snapshot");
        assert_eq!(concurrent.root_count, baseline.root_count);
        assert_eq!(
            concurrent.repository_snapshot_digest,
            baseline.repository_snapshot_digest
        );
        assert_eq!(concurrent.upper, baseline.upper);
        assert_eq!(
            concurrent.page.items.first().unwrap().key.custody_id,
            Uuid::from_u128(514),
            "the insertion within the remaining key range must not enter the established read snapshot"
        );

        let recaptured = store
            .source_worktree_snapshot_capture(repository_identity, Some(&after))
            .expect("recapture after concurrent insertions");
        assert_eq!(recaptured.root_count, 302);
        assert_ne!(
            recaptured.repository_snapshot_digest,
            baseline.repository_snapshot_digest
        );
        assert_eq!(
            recaptured.page.items.first().unwrap().key.custody_id,
            Uuid::from_u128(513)
        );
    }

    #[test]
    fn v120_explain_uses_targeted_prefix_exact_cwd_and_ref_indexes() {
        let store = Store::open_in_memory().expect("open V120 EXPLAIN store");
        let exact = "/v120/root";
        let lower = "/v120/root/";
        let upper = "/v120/root0";
        let plan = explain(
            &store.conn,
            "SELECT id FROM scheduled_jobs INDEXED BY idx_swc_v120_jobs_raw_path
              WHERE enabled=1 AND working_dir IS NOT NULL
                AND (working_dir=?1 OR (working_dir>=?2 AND working_dir<?3))",
            &[&exact, &lower, &upper],
        );
        assert!(plan.contains("idx_swc_v120_jobs_raw_path"), "{plan}");
        let plan = explain(
            &store.conn,
            "SELECT job_id FROM scheduled_job_path_projections INDEXED BY idx_swc_v120_jobs_canonical_path
              WHERE verification_state='verified' AND raw_enabled=1
                AND (canonical_working_dir=?1 OR (canonical_working_dir>=?2 AND canonical_working_dir<?3))",
            &[&exact, &lower, &upper],
        );
        assert!(plan.contains("idx_swc_v120_jobs_canonical_path"), "{plan}");
        let wake_id = Uuid::new_v4().to_string();
        let plan = explain(
            &store.conn,
            "SELECT id FROM scheduled_jobs INDEXED BY idx_swc_v120_jobs_wake_session
              WHERE enabled=1 AND wake_session_id IS NOT NULL AND wake_session_id=?1",
            &[&wake_id],
        );
        assert!(plan.contains("idx_swc_v120_jobs_wake_session"), "{plan}");
        let plan = explain(
            &store.conn,
            "SELECT session_id FROM session_execution_projections INDEXED BY idx_swc_v120_execution_cwd
              WHERE effective_cwd=?1 OR (effective_cwd>=?2 AND effective_cwd<?3)",
            &[&exact, &lower, &upper],
        );
        assert!(plan.contains("idx_swc_v120_execution_cwd"), "{plan}");
        let plan = explain(
            &store.conn,
            "SELECT custody_id FROM sandbox_custody_roots INDEXED BY idx_swc_v120_roots_repository_ref
              WHERE repository_identity=?1 AND sandbox_branch=?2 AND state='live'",
            &[&"repo:v120", &"rsi/topic"],
        );
        assert!(plan.contains("idx_swc_v120_roots_repository_ref"), "{plan}");
        let plan = explain(
            &store.conn,
            "SELECT id FROM scheduled_jobs INDEXED BY idx_swc_v120_jobs_enabled_id
              WHERE enabled=1 ORDER BY enabled,id LIMIT 16385",
            &[],
        );
        assert!(plan.contains("idx_swc_v120_jobs_enabled_id"), "{plan}");
        let plan = explain(
            &store.conn,
            "SELECT session_id FROM source_worktree_session_dependency_health
                    INDEXED BY idx_swc_v120_sessions_dependency_health
              WHERE relevance IN (1,2)
              ORDER BY relevance,session_id LIMIT 16385",
            &[],
        );
        assert!(
            plan.contains("idx_swc_v120_sessions_dependency_health"),
            "{plan}"
        );
    }

    #[test]
    fn v120_session_health_projection_is_guarded_and_tracks_source_evidence() {
        let directory = tempfile::tempdir().expect("create Session health fixture");
        let working = directory.path().join("working");
        fs::create_dir_all(&working).expect("create Session health working directory");
        let store = Store::open(&directory.path().join("session-health.sqlite"))
            .expect("open Session health Store");
        let session_id = Uuid::from_u128(0x7001);
        insert_test_session(&store.conn, session_id, &working, "Completed");

        let health = |connection: &Connection| {
            connection
                .query_row(
                    "SELECT relevance FROM source_worktree_session_dependency_health
                      WHERE session_id=?1",
                    [session_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read Session dependency health")
        };
        assert_eq!(health(&store.conn), 2, "unverified projection is invalid");

        verify_test_session_projection(&store.conn, session_id, &working, None);
        assert_eq!(
            health(&store.conn),
            1,
            "verified executable leaf is relevant"
        );
        assert!(
            store
                .conn
                .execute(
                    "UPDATE source_worktree_session_dependency_health SET relevance=0
                      WHERE session_id=?1",
                    [session_id.to_string()],
                )
                .is_err(),
            "a forged irrelevant classification must be rejected"
        );
        assert!(
            store
                .conn
                .execute(
                    "DELETE FROM source_worktree_session_dependency_health WHERE session_id=?1",
                    [session_id.to_string()],
                )
                .is_err(),
            "a live Session classification cannot be removed"
        );

        store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='Group' WHERE id=?1",
                [session_id.to_string()],
            )
            .expect("turn fixture into a proven inert container");
        assert_eq!(health(&store.conn), 0);
        store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='Task',status='Running' WHERE id=?1",
                [session_id.to_string()],
            )
            .expect("restore executable leaf fixture");
        assert_eq!(health(&store.conn), 1);
        let reopened = Store::open(&directory.path().join("session-health.sqlite"))
            .expect("reopen with V120 relevance scalar registered");
        assert_eq!(health(&reopened.conn), 1);
        drop(reopened);
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [session_id.to_string()],
            )
            .expect("make projection absence require historical evidence");

        store
            .conn
            .execute_batch("DROP TRIGGER session_execution_projections_no_delete")
            .expect("permit isolated projection deletion fixture");
        store
            .conn
            .execute(
                "DELETE FROM session_execution_projections WHERE session_id=?1",
                [session_id.to_string()],
            )
            .expect("delete projection through V120 invalidation trigger");
        assert_eq!(
            health(&store.conn),
            2,
            "missing projection stays indexed invalid"
        );
    }

    #[test]
    fn source_worktree_candidate_is_exact_and_participant_overflow_retains_event_chain() {
        let directory = tempfile::tempdir().expect("create V120 candidate fixture");
        let repository_dir = directory.path().join("repository");
        let sandbox_base = directory.path().join("sandboxes");
        fs::create_dir_all(&repository_dir).expect("create candidate repository");
        fs::create_dir_all(&sandbox_base).expect("create candidate sandbox base");
        let store = Store::open(&directory.path().join("candidate.sqlite"))
            .expect("open V120 candidate fixture");
        let first_owner = Uuid::from_u128(0x201);
        let current_owner = Uuid::from_u128(0x202);
        let custody_id = Uuid::from_u128(0x301);
        let sandbox_root = sandbox_base.join(current_owner.to_string());
        fs::create_dir_all(&sandbox_root).expect("create candidate root");
        insert_test_session(&store.conn, first_owner, &repository_dir, "Archived");
        insert_test_session(&store.conn, current_owner, &repository_dir, "Completed");
        store
            .conn
            .execute(
                "UPDATE sessions SET continued_from=?1 WHERE id=?2",
                params![first_owner.to_string(), current_owner.to_string()],
            )
            .expect("link candidate lineage");
        insert_test_root(
            &store.conn,
            current_owner,
            custody_id,
            2,
            "repo:v120:candidate",
            &repository_dir,
            &sandbox_root,
            "rsi/v120-candidate",
        );
        link_test_session_to_root(
            &store.conn,
            first_owner,
            custody_id,
            &sandbox_root,
            "rsi/v120-candidate",
        );
        link_test_session_to_root(
            &store.conn,
            current_owner,
            custody_id,
            &sandbox_root,
            "rsi/v120-candidate",
        );
        verify_test_session_projection(
            &store.conn,
            current_owner,
            &sandbox_root,
            Some((custody_id, 2)),
        );
        store
            .conn
            .execute(
                "INSERT INTO sandbox_custody_events(
                    event_id,custody_id,sequence,event_kind,cause,from_generation,to_generation,
                    from_owner_session_id,to_owner_session_id,origin_session_id,scheduled_job_id,
                    prior_state,next_state,error_code,occurred_at)
                 VALUES(?1,?2,1,'allocated','fresh_launch',NULL,1,NULL,?3,NULL,NULL,NULL,'live',NULL,?4)",
                params![
                    Uuid::from_u128(0x401).to_string(),
                    custody_id.to_string(),
                    first_owner.to_string(),
                    TEST_NOW,
                ],
            )
            .expect("insert allocation event");
        store
            .conn
            .execute(
                "INSERT INTO sandbox_custody_events(
                    event_id,custody_id,sequence,event_kind,cause,from_generation,to_generation,
                    from_owner_session_id,to_owner_session_id,origin_session_id,scheduled_job_id,
                    prior_state,next_state,error_code,occurred_at)
                 VALUES(?1,?2,2,'transferred','rotation',1,2,?3,?4,?3,NULL,'live','live',NULL,?5)",
                params![
                    Uuid::from_u128(0x402).to_string(),
                    custody_id.to_string(),
                    first_owner.to_string(),
                    current_owner.to_string(),
                    TEST_NOW,
                ],
            )
            .expect("insert transfer event");

        let inventory = store
            .source_worktree_candidate_inventory(first_owner, custody_id, 2)
            .expect("load exact candidate")
            .expect("candidate exists");
        assert_eq!(inventory.requested_session_id, first_owner);
        assert_eq!(inventory.owner_session_id, current_owner);
        assert_eq!(inventory.owner_status, "Completed");
        assert_eq!(inventory.requested_status, "Archived");
        assert_eq!(inventory.participant_count, 2);
        assert!(!inventory.participant_overflow);
        assert_eq!(inventory.custody_event_count, 2);
        assert!(!inventory.custody_event_overflow);
        assert_eq!(inventory.owner_projection_custody_id, Some(custody_id));
        assert_eq!(inventory.owner_projection_generation, Some(2));
        assert!(
            store
                .source_worktree_candidate_inventory(Uuid::new_v4(), custody_id, 2)
                .expect("query wrong Session")
                .is_none()
        );
        assert!(
            store
                .source_worktree_candidate_inventory(first_owner, custody_id, 1)
                .expect("query wrong generation")
                .is_none()
        );
        let participants = store
            .source_worktree_participant_inventory(custody_id, 2)
            .expect("load exact participants");
        assert!(!participants.overflow);
        assert_eq!(participants.participants.len(), 2);
        let events = store
            .source_worktree_custody_event_inventory(custody_id)
            .expect("load custody event chain");
        assert!(!events.overflow);
        assert_eq!(events.events.len(), 2);
        assert_eq!(events.events[0].to_generation, 1);
        assert_eq!(events.events[1].from_generation, Some(1));
        assert_eq!(events.events[1].to_generation, 2);
        assert_eq!(events.events[1].from_owner_session_id, Some(first_owner));
        assert_eq!(events.events[1].to_owner_session_id, Some(current_owner));

        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open participant overflow transaction");
        for ordinal in 0..1023_u128 {
            let session_id = Uuid::from_u128(0x1000 + ordinal);
            insert_test_session(&tx, session_id, &repository_dir, "Archived");
            tx.execute(
                "UPDATE sessions SET sandbox_custody_id=?1 WHERE id=?2",
                params![custody_id.to_string(), session_id.to_string()],
            )
            .expect("link overflow participant");
        }
        tx.commit().expect("commit participant overflow fixture");
        let overflow = store
            .source_worktree_participant_inventory(
                custody_id,
                SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS,
            )
            .expect("read participant overflow");
        assert!(overflow.overflow);
        assert!(
            overflow.participants.is_empty(),
            "overflow must retain the candidate instead of truncating evidence"
        );
        let saturated = store
            .source_worktree_candidate_inventory(first_owner, custody_id, 2)
            .expect("load participant-saturated candidate")
            .expect("participant-saturated candidate exists");
        assert_eq!(
            saturated.participant_count,
            SOURCE_WORKTREE_BATCH_MAX_PARTICIPANTS as u32
        );
        assert!(saturated.participant_overflow);
        let dependency = store
            .source_worktree_targeted_dependencies(custody_id, 2)
            .expect("participant bound is typed dependency evidence");
        assert!(!dependency.complete);
        assert_eq!(dependency.reason, Some("participant_dependency_bound"));

        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open custody-event overflow transaction");
        for sequence in 3..=(SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS + 1) as u64 {
            tx.execute(
                "INSERT INTO sandbox_custody_events(
                    event_id,custody_id,sequence,event_kind,cause,from_generation,to_generation,
                    from_owner_session_id,to_owner_session_id,origin_session_id,scheduled_job_id,
                    prior_state,next_state,error_code,occurred_at)
                 VALUES(?1,?2,?3,'validation_failed','startup_reconciliation',2,2,
                        ?4,?4,NULL,NULL,'live','live','test_validation',?5)",
                params![
                    Uuid::from_u128(0x1000000 + sequence as u128).to_string(),
                    custody_id.to_string(),
                    sequence as i64,
                    current_owner.to_string(),
                    TEST_NOW,
                ],
            )
            .expect("insert bounded custody-event history");
        }
        tx.commit().expect("commit custody-event overflow fixture");
        let event_saturated = store
            .source_worktree_candidate_inventory(first_owner, custody_id, 2)
            .expect("load event-saturated candidate")
            .expect("event-saturated candidate exists");
        assert_eq!(
            event_saturated.custody_event_count,
            SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS as u32
        );
        assert!(event_saturated.custody_event_overflow);
        let events = store
            .source_worktree_custody_event_inventory(custody_id)
            .expect("read bounded custody-event overflow");
        assert!(events.overflow);
        assert!(
            events.events.is_empty(),
            "overflow must retain the candidate rather than expose a truncated event chain"
        );
    }

    #[test]
    fn source_worktree_targeted_dependencies_cover_paths_aliases_links_and_health() {
        let directory = tempfile::tempdir().expect("create V120 dependency fixture");
        let repository_dir = directory.path().join("repository");
        let sandbox_base = directory.path().join("sandboxes");
        let outside = directory.path().join("outside");
        fs::create_dir_all(&repository_dir).expect("create dependency repository");
        fs::create_dir_all(outside.join("nested")).expect("create unrelated directory");
        fs::create_dir_all(&sandbox_base).expect("create dependency sandbox base");
        let store = Store::open(&directory.path().join("dependencies.sqlite"))
            .expect("open V120 dependency fixture");
        let owner = Uuid::from_u128(0x5001);
        let wake_target = Uuid::from_u128(0x5002);
        let custody_id = Uuid::from_u128(0x6001);
        let branch = "rsi/v120-dependencies";
        let sandbox_root = sandbox_base.join(owner.to_string());
        let nested = sandbox_root.join("nested");
        fs::create_dir_all(&nested).expect("create nested candidate directory");
        insert_test_session(&store.conn, owner, &repository_dir, "Archived");
        insert_test_session(&store.conn, wake_target, &outside, "Completed");
        verify_test_session_projection(&store.conn, wake_target, &outside, None);
        insert_test_root(
            &store.conn,
            owner,
            custody_id,
            1,
            "repo:v120:dependencies",
            &repository_dir,
            &sandbox_root,
            branch,
        );
        link_test_session_to_root(&store.conn, owner, custody_id, &sandbox_root, branch);
        verify_test_session_projection(&store.conn, owner, &sandbox_root, Some((custody_id, 1)));

        let run_id = Uuid::from_u128(0x7001);
        store
            .conn
            .execute(
                "INSERT INTO source_worktree_settlement_runs(
                    run_id,schema_version,policy_version,repository_identity,canonical_repo_dir,
                    target_ref,target_oid,plan_digest,idempotency_key,authorization_digest,
                    request_fingerprint,state,observed_count,eligible_count,retained_count,
                    settled_count,refused_count,recovery_required_count,unattempted_count,
                    created_at,updated_at,finished_at,terminal_error)
                 VALUES(?1,1,1,'repo:v120:dependencies',?2,'refs/heads/rolling',?3,?4,
                        'v120-alias',?4,?4,'intent_committed',1,1,0,0,0,0,0,?5,?5,NULL,NULL)",
                params![
                    run_id.to_string(),
                    repository_dir.to_string_lossy().into_owned(),
                    "b".repeat(40),
                    format!("sha256:{}", "c".repeat(64)),
                    TEST_NOW,
                ],
            )
            .expect("insert retained alias run");
        store
            .conn
            .execute(
                "INSERT INTO source_worktree_settlement_items(
                    run_id,sequence,session_id,original_status,original_updated_at,custody_id,
                    custody_generation,canonical_repo_dir,sandbox_root,sandbox_branch,
                    repository_identity,source_ref,source_oid,target_oid,evidence_digest,
                    clean_state_digest,reserved_effects,active_effects,participant_count,
                    phase,before_observation,after_observation,refusal_code,created_at,updated_at)
                 VALUES(?1,0,?2,'Archived',?3,?4,1,?5,?6,?7,'repo:v120:dependencies',
                        ?8,?9,?10,?11,?11,0,0,1,'intent_committed',NULL,NULL,NULL,?3,?3)",
                params![
                    run_id.to_string(),
                    owner.to_string(),
                    TEST_NOW,
                    custody_id.to_string(),
                    repository_dir.to_string_lossy().into_owned(),
                    sandbox_root.to_string_lossy().into_owned(),
                    branch,
                    format!("refs/heads/{branch}"),
                    "a".repeat(40),
                    "b".repeat(40),
                    format!("sha256:{}", "d".repeat(64)),
                ],
            )
            .expect("insert retained alias item");
        let quarantine = source_worktree_quarantine_path(&sandbox_root, run_id, owner)
            .expect("derive authenticated quarantine alias");
        let quarantine_nested = quarantine.join("nested");
        fs::create_dir_all(&quarantine_nested).expect("create quarantine alias directory");

        let alias = directory.path().join("symlink-alias");
        std::os::unix::fs::symlink(&sandbox_root, &alias).expect("create source-worktree alias");
        let nested_job = test_job("nested", Some(nested.clone()), WakeMode::Fresh, None, true);
        let alias_job = test_job(
            "symlink",
            Some(alias.join("nested")),
            WakeMode::Fresh,
            None,
            true,
        );
        let quarantine_job = test_job(
            "quarantine",
            Some(quarantine_nested),
            WakeMode::Fresh,
            None,
            true,
        );
        let resume_job = test_job("resume-link", None, WakeMode::Resume, Some(owner), true);
        let terminal_job = test_job(
            "terminal-link",
            None,
            WakeMode::OnTerminal(owner),
            Some(wake_target),
            true,
        );
        let disabled_job = test_job(
            "disabled-nested",
            Some(nested.clone()),
            WakeMode::Fresh,
            None,
            false,
        );
        let unrelated_job = test_job(
            "unrelated-path",
            Some(outside.join("nested")),
            WakeMode::Fresh,
            None,
            true,
        );
        let unrelated_no_path = test_job("unrelated-no-path", None, WakeMode::Fresh, None, true);
        for job in [
            &nested_job,
            &alias_job,
            &quarantine_job,
            &resume_job,
            &terminal_job,
            &disabled_job,
            &unrelated_job,
            &unrelated_no_path,
        ] {
            store
                .insert_scheduled_job(job)
                .unwrap_or_else(|error| panic!("insert {}: {error}", job.name));
        }

        let related_raw_session = Uuid::from_u128(0x8001);
        insert_test_session(&store.conn, related_raw_session, &nested, "Completed");
        verify_test_session_projection(&store.conn, related_raw_session, &nested, None);
        let related_cwd_session = Uuid::from_u128(0x8002);
        insert_test_session(&store.conn, related_cwd_session, &outside, "Failed");
        verify_test_session_projection(&store.conn, related_cwd_session, &nested, None);
        let unrelated_session = Uuid::from_u128(0x8003);
        insert_test_session(&store.conn, unrelated_session, &outside, "Completed");
        verify_test_session_projection(&store.conn, unrelated_session, &outside, None);
        let container_session = Uuid::from_u128(0x8004);
        insert_test_session(&store.conn, container_session, &nested, "Completed");
        store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='Group' WHERE id=?1",
                [container_session.to_string()],
            )
            .expect("mark nested Session as non-executable container");
        verify_test_session_projection(&store.conn, container_session, &nested, None);

        let evidence = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("read targeted dependency evidence");
        assert!(evidence.complete, "{:?}", evidence.reason);
        assert_eq!(evidence.scheduled_dependency_count, 5);
        assert_eq!(evidence.session_path_dependency_count, 2);
        assert!(evidence.scheduled_dependency_digest.starts_with("sha256:"));
        assert!(
            evidence
                .session_path_dependency_digest
                .starts_with("sha256:")
        );

        fs::remove_file(&alias).expect("remove initial symlink target");
        std::os::unix::fs::symlink(&outside, &alias).expect("repoint source-worktree alias");
        let repointed = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("read repointed alias evidence");
        assert!(!repointed.complete);
        assert_eq!(repointed.reason, Some("scheduled_job_projection_unhealthy"));
        fs::remove_file(&alias).expect("remove repointed alias");
        std::os::unix::fs::symlink(&sandbox_root, &alias).expect("restore source-worktree alias");
        refresh_scheduled_job_path_projection(&store.conn, &alias_job.id.to_string())
            .expect("refresh restored alias evidence");

        store
            .conn
            .execute(
                "UPDATE scheduled_job_path_projections SET evidence_digest=?1 WHERE job_id=?2",
                params![
                    format!("sha256:{}", "0".repeat(64)),
                    nested_job.id.to_string()
                ],
            )
            .expect("poison scheduled projection evidence digest");
        let stale = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("read stale digest evidence");
        assert!(!stale.complete);
        assert_eq!(stale.reason, Some("scheduled_job_projection_unhealthy"));
        refresh_scheduled_job_path_projection(&store.conn, &nested_job.id.to_string())
            .expect("repair stale digest evidence");

        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_session_id='not-a-uuid',updated_at=?1 WHERE id=?2",
                params![TEST_NOW, unrelated_no_path.id.to_string()],
            )
            .expect("install malformed legacy wake evidence");
        refresh_scheduled_job_path_projection(&store.conn, &unrelated_no_path.id.to_string())
            .expect("retain malformed legacy evidence as invalid");
        let malformed = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("read malformed projection health");
        assert!(!malformed.complete);
        assert_eq!(malformed.reason, Some("scheduled_job_projection_unhealthy"));
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_session_id=NULL,updated_at=?1 WHERE id=?2",
                params![TEST_NOW, unrelated_no_path.id.to_string()],
            )
            .expect("repair malformed legacy wake evidence");
        refresh_scheduled_job_path_projection(&store.conn, &unrelated_no_path.id.to_string())
            .expect("verify repaired no-path evidence");

        for (wake_mode, wake_origin, label) in [
            ("agent_fresh".to_string(), None, "missing AgentFresh origin"),
            (
                "agent_fresh".to_string(),
                Some(format!("{{{owner}}}")),
                "noncanonical AgentFresh origin",
            ),
            (
                format!("on_terminal:{{{owner}}}"),
                Some(wake_target.to_string()),
                "noncanonical watched Session",
            ),
        ] {
            store
                .conn
                .execute(
                    "UPDATE scheduled_jobs
                        SET wake_mode=?1,wake_session_id=?2,updated_at=?3 WHERE id=?4",
                    params![
                        wake_mode,
                        wake_origin,
                        TEST_NOW,
                        unrelated_no_path.id.to_string()
                    ],
                )
                .unwrap_or_else(|error| panic!("install {label}: {error}"));
            refresh_scheduled_job_path_projection(&store.conn, &unrelated_no_path.id.to_string())
                .unwrap_or_else(|error| panic!("refresh {label}: {error}"));
            reconcile_scheduled_job_path_projections(&store.conn)
                .unwrap_or_else(|error| panic!("reconcile {label}: {error}"));
            let unhealthy = store
                .source_worktree_targeted_dependencies(custody_id, 1)
                .unwrap_or_else(|error| panic!("read {label}: {error}"));
            assert!(!unhealthy.complete, "{label} must fail closed");
            assert_eq!(
                unhealthy.reason,
                Some("scheduled_job_projection_unhealthy"),
                "{label}"
            );
        }
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs
                    SET wake_mode='fresh',wake_session_id=NULL,updated_at=?1 WHERE id=?2",
                params![TEST_NOW, unrelated_no_path.id.to_string()],
            )
            .expect("repair recognized wake authority evidence");
        refresh_scheduled_job_path_projection(&store.conn, &unrelated_no_path.id.to_string())
            .expect("verify repaired recognized wake authority evidence");

        store
            .conn
            .execute(
                "DELETE FROM scheduled_job_path_projections WHERE job_id=?1",
                [nested_job.id.to_string()],
            )
            .expect("remove related projection row");
        let missing = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("read missing related projection evidence");
        assert!(!missing.complete);
        assert_eq!(missing.reason, Some("scheduled_job_projection_unhealthy"));
        refresh_scheduled_job_path_projection(&store.conn, &nested_job.id.to_string())
            .expect("repair missing related projection");

        store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET freshness='unverified',effective_cwd=NULL,validated_at=NULL,error_code=NULL
                  WHERE session_id=?1",
                [related_raw_session.to_string()],
            )
            .expect("invalidate related Session projection");
        let unhealthy_session = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("read unhealthy Session evidence");
        assert!(!unhealthy_session.complete);
        assert_eq!(
            unhealthy_session.reason,
            Some("session_projection_unhealthy")
        );
    }

    #[test]
    fn source_worktree_fresh_path_health_rejects_inward_repoints_and_missing_projection() {
        let directory = tempfile::tempdir().expect("create inward-repoint fixture");
        let repository_dir = directory.path().join("repository");
        let outside = directory.path().join("outside");
        let sandbox_root = directory.path().join("sandbox");
        for path in [&repository_dir, &outside, &sandbox_root] {
            fs::create_dir_all(path).expect("create inward-repoint fixture directory");
        }
        let store = Store::open(&directory.path().join("inward-repoint.sqlite"))
            .expect("open inward-repoint Store");
        let owner = Uuid::from_u128(0x8a01);
        let custody_id = Uuid::from_u128(0x8a02);
        insert_test_session(&store.conn, owner, &repository_dir, "Archived");
        insert_test_root(
            &store.conn,
            owner,
            custody_id,
            1,
            "repo:v120:inward-repoint",
            &repository_dir,
            &sandbox_root,
            "rsi/v120-inward-repoint",
        );
        link_test_session_to_root(
            &store.conn,
            owner,
            custody_id,
            &sandbox_root,
            "rsi/v120-inward-repoint",
        );
        verify_test_session_projection(&store.conn, owner, &sandbox_root, Some((custody_id, 1)));

        let job_alias = directory.path().join("job-alias");
        std::os::unix::fs::symlink(&outside, &job_alias).expect("create outside job alias");
        let alias_job = test_job(
            "inward-job-alias",
            Some(job_alias.clone()),
            WakeMode::Fresh,
            None,
            true,
        );
        store
            .insert_scheduled_job(&alias_job)
            .expect("insert outside-alias job");
        fs::remove_file(&job_alias).expect("remove outside job alias");
        std::os::unix::fs::symlink(&sandbox_root, &job_alias)
            .expect("repoint job alias into candidate");
        let job_alias_result = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("validate inward job alias");
        assert!(!job_alias_result.complete);
        assert_eq!(
            job_alias_result.reason,
            Some("scheduled_job_projection_unhealthy")
        );
        store
            .conn
            .execute(
                "DELETE FROM scheduled_jobs WHERE id=?1",
                [alias_job.id.to_string()],
            )
            .expect("remove inward-alias job");

        let ordinary_job_dir = directory.path().join("ordinary-job-dir");
        fs::create_dir(&ordinary_job_dir).expect("create ordinary job directory");
        let ordinary_job = test_job(
            "ordinary-to-inward-symlink",
            Some(ordinary_job_dir.clone()),
            WakeMode::Fresh,
            None,
            true,
        );
        store
            .insert_scheduled_job(&ordinary_job)
            .expect("insert ordinary-path job");
        fs::remove_dir(&ordinary_job_dir).expect("remove ordinary job directory");
        std::os::unix::fs::symlink(&sandbox_root, &ordinary_job_dir)
            .expect("replace ordinary job directory with inward symlink");
        let ordinary_job_result = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("validate ordinary job path replacement");
        assert!(!ordinary_job_result.complete);
        assert_eq!(
            ordinary_job_result.reason,
            Some("scheduled_job_projection_unhealthy")
        );
        store
            .conn
            .execute(
                "DELETE FROM scheduled_jobs WHERE id=?1",
                [ordinary_job.id.to_string()],
            )
            .expect("remove ordinary-path job");

        let missing_alias = directory.path().join("missing-projection-alias");
        std::os::unix::fs::symlink(&outside, &missing_alias)
            .expect("create missing-projection outside alias");
        let missing_job = test_job(
            "missing-inward-projection",
            Some(missing_alias.clone()),
            WakeMode::Fresh,
            None,
            true,
        );
        store
            .insert_scheduled_job(&missing_job)
            .expect("insert missing-projection job");
        store
            .conn
            .execute(
                "DELETE FROM scheduled_job_path_projections WHERE job_id=?1",
                [missing_job.id.to_string()],
            )
            .expect("remove outside cached projection");
        fs::remove_file(&missing_alias).expect("remove missing-projection outside alias");
        std::os::unix::fs::symlink(&sandbox_root, &missing_alias)
            .expect("repoint missing-projection alias into candidate");
        let missing_result = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("validate missing inward projection");
        assert!(!missing_result.complete);
        assert_eq!(
            missing_result.reason,
            Some("scheduled_job_projection_unhealthy")
        );
        store
            .conn
            .execute(
                "DELETE FROM scheduled_jobs WHERE id=?1",
                [missing_job.id.to_string()],
            )
            .expect("remove missing-projection job");

        let session_alias = directory.path().join("session-alias");
        std::os::unix::fs::symlink(&outside, &session_alias).expect("create outside Session alias");
        let alias_session = Uuid::from_u128(0x8a03);
        insert_test_session(&store.conn, alias_session, &session_alias, "Completed");
        verify_test_session_projection(&store.conn, alias_session, &outside, None);
        fs::remove_file(&session_alias).expect("remove outside Session alias");
        std::os::unix::fs::symlink(&sandbox_root, &session_alias)
            .expect("repoint Session alias into candidate");
        let alias_session_result = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("validate inward Session alias");
        assert!(!alias_session_result.complete);
        assert_eq!(
            alias_session_result.reason,
            Some("session_projection_unhealthy")
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='Group' WHERE id=?1",
                [alias_session.to_string()],
            )
            .expect("retire inward-alias Session as a proven container");

        let malformed_alias = directory.path().join("malformed-active-alias");
        std::os::unix::fs::symlink(&outside, &malformed_alias)
            .expect("create malformed active outside alias");
        let malformed_session = Uuid::from_u128(0x8a05);
        insert_test_session(&store.conn, malformed_session, &malformed_alias, "Running");
        store
            .conn
            .execute(
                "UPDATE sessions
                    SET sandbox_kind='GitWorktree',sandbox_root=NULL,
                        sandbox_branch='rsi/malformed',sandbox_cleanup_state='Live'
                  WHERE id=?1",
                [malformed_session.to_string()],
            )
            .expect("install malformed active sandbox tuple");
        store
            .conn
            .execute_batch("DROP TRIGGER session_execution_projections_no_delete;")
            .expect("allow missing-projection corruption fixture");
        store
            .conn
            .execute(
                "DELETE FROM session_execution_projections WHERE session_id=?1",
                [malformed_session.to_string()],
            )
            .expect("remove malformed active Session projection");
        fs::remove_file(&malformed_alias).expect("remove malformed active outside alias");
        std::os::unix::fs::symlink(&sandbox_root, &malformed_alias)
            .expect("repoint malformed active alias into candidate");
        let malformed_active = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("validate malformed active inward alias");
        assert!(!malformed_active.complete);
        assert_eq!(
            malformed_active.reason,
            Some("session_projection_unhealthy")
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='Group' WHERE id=?1",
                [malformed_session.to_string()],
            )
            .expect("retire malformed active Session fixture");

        let ordinary_session_dir = directory.path().join("ordinary-session-dir");
        fs::create_dir(&ordinary_session_dir).expect("create ordinary Session directory");
        let ordinary_session = Uuid::from_u128(0x8a04);
        insert_test_session(
            &store.conn,
            ordinary_session,
            &ordinary_session_dir,
            "Interrupted",
        );
        verify_test_session_projection(&store.conn, ordinary_session, &ordinary_session_dir, None);
        fs::remove_dir(&ordinary_session_dir).expect("remove ordinary Session directory");
        std::os::unix::fs::symlink(&sandbox_root, &ordinary_session_dir)
            .expect("replace ordinary Session directory with inward symlink");
        let ordinary_session_result = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("validate ordinary Session path replacement");
        assert!(!ordinary_session_result.complete);
        assert_eq!(
            ordinary_session_result.reason,
            Some("session_projection_unhealthy")
        );
    }

    #[test]
    fn source_worktree_fresh_path_health_pages_and_refuses_each_class_over_ceiling() {
        let directory = tempfile::tempdir().expect("create dependency-health bound fixture");
        let repository_dir = directory.path().join("repository");
        let sandbox_root = directory.path().join("sandbox");
        fs::create_dir_all(&repository_dir).expect("create bound repository");
        fs::create_dir_all(&sandbox_root).expect("create bound sandbox root");
        let store = Store::open(&directory.path().join("dependency-health-bound.sqlite"))
            .expect("open dependency-health bound Store");
        let owner = Uuid::from_u128(0x8b01);
        let custody_id = Uuid::from_u128(0x8b02);
        insert_test_session(&store.conn, owner, &repository_dir, "Archived");
        insert_test_root(
            &store.conn,
            owner,
            custody_id,
            1,
            "repo:v120:dependency-health-bound",
            &repository_dir,
            &sandbox_root,
            "rsi/v120-dependency-health-bound",
        );
        link_test_session_to_root(
            &store.conn,
            owner,
            custody_id,
            &sandbox_root,
            "rsi/v120-dependency-health-bound",
        );
        verify_test_session_projection(&store.conn, owner, &sandbox_root, Some((custody_id, 1)));

        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open enabled-job bound transaction");
        for ordinal in 1..=(SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS + 1) as u128 {
            tx.execute(
                "INSERT INTO scheduled_jobs(
                    id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,working_dir,
                    provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id)
                 VALUES(?1,'health-bound','health-bound','{}',NULL,?2,1,NULL,
                        NULL,NULL,NULL,?2,?2,'fresh',NULL)",
                params![Uuid::from_u128(0x10000 + ordinal).to_string(), TEST_NOW],
            )
            .expect("insert enabled dependency-health job");
        }
        tx.commit().expect("commit enabled-job bound fixture");
        reconcile_scheduled_job_path_projections(&store.conn)
            .expect("verify first dependency-health ceiling jobs");
        let job_bound = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("read enabled-job dependency-health bound");
        assert!(!job_bound.complete);
        assert_eq!(
            job_bound.reason,
            Some("scheduled_job_dependency_health_bound")
        );
        store
            .conn
            .execute("DELETE FROM scheduled_jobs", [])
            .expect("clear dependency-health jobs");

        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open irrelevant Session history transaction");
        for ordinal in 1..=(SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS + 1) as u128 {
            let session_id = Uuid::from_u128(0x100000 + ordinal);
            insert_test_session(&tx, session_id, &sandbox_root, "Deleted");
            tx.execute(
                "UPDATE session_execution_projections
                    SET execution_state='historical_purged',freshness='verified',
                        canonical_repo_dir=?1,effective_cwd=NULL,custody_id=NULL,
                        custody_generation=NULL,validated_at=?2,error_code=NULL,updated_at=?2
                  WHERE session_id=?3",
                params![
                    sandbox_root.to_string_lossy().into_owned(),
                    TEST_NOW,
                    session_id.to_string()
                ],
            )
            .expect("authenticate irrelevant removed Session history");
        }
        tx.commit().expect("commit irrelevant Session history");
        let irrelevant_history = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("scan more than the relevant ceiling of removed history");
        assert!(
            irrelevant_history.complete,
            "{:?}",
            irrelevant_history.reason
        );
        assert_eq!(irrelevant_history.session_path_dependency_count, 0);

        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open relevant Session bound transaction");
        for ordinal in 1..=SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS as u128 {
            insert_test_session(
                &tx,
                Uuid::from_u128(0x200000 + ordinal),
                &repository_dir,
                "Completed",
            );
        }
        tx.execute(
            "UPDATE session_execution_projections
                SET execution_state='ordinary_unsandboxed',freshness='verified',
                    canonical_repo_dir=?1,effective_cwd=?1,validated_at=?2,
                    error_code=NULL,updated_at=?2
              WHERE custody_id IS NULL AND execution_state='ordinary_unsandboxed'",
            params![repository_dir.to_string_lossy().into_owned(), TEST_NOW,],
        )
        .expect("verify bounded Session projection rows");
        tx.commit().expect("commit Session bound fixture");
        let session_bound = store
            .source_worktree_targeted_dependencies(custody_id, 1)
            .expect("read Session dependency-health bound");
        assert!(!session_bound.complete);
        assert_eq!(
            session_bound.reason,
            Some("session_dependency_health_bound")
        );
    }

    #[test]
    fn v120_projection_refresh_fault_rolls_back_every_transactional_writer() {
        let directory = tempfile::tempdir().expect("create V120 writer fixture");
        let working = directory.path().join("working");
        fs::create_dir_all(&working).expect("create V120 writer directory");
        let store = Store::open(&directory.path().join("writers.sqlite"))
            .expect("open V120 writer fixture");

        let ordinary = test_job(
            "ordinary-fault",
            Some(working.clone()),
            WakeMode::Fresh,
            None,
            true,
        );
        fail_next_projection_refresh();
        assert!(store.insert_scheduled_job(&ordinary).is_err());
        let raw_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM scheduled_jobs WHERE id=?1",
                [ordinary.id.to_string()],
                |row| row.get(0),
            )
            .expect("count rolled-back ordinary job");
        let projection_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM scheduled_job_path_projections WHERE job_id=?1",
                [ordinary.id.to_string()],
                |row| row.get(0),
            )
            .expect("count rolled-back ordinary projection");
        assert_eq!((raw_count, projection_count), (0, 0));

        let owner = Uuid::new_v4();
        let watched = Uuid::new_v4();
        insert_test_session(&store.conn, owner, &working, "Running");
        insert_test_session(&store.conn, watched, &working, "Running");
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open agent-message watch transaction");
        fail_next_projection_refresh();
        assert!(
            super::super::agent_coordination::arm_agent_message_watch_in_tx(
                &tx,
                owner,
                watched,
                Utc::now(),
            )
            .is_err()
        );
        tx.rollback().expect("roll back failed agent-message watch");
        let watch_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM scheduled_jobs WHERE wake_session_id=?1 AND wake_mode=?2",
                params![owner.to_string(), format!("on_terminal:{watched}")],
                |row| row.get(0),
            )
            .expect("count rolled-back agent-message watch");
        assert_eq!(watch_count, 0);

        let mut guard = test_job(
            "guard-before",
            Some(working.clone()),
            WakeMode::Resume,
            Some(owner),
            false,
        );
        store
            .insert_scheduled_job(&guard)
            .expect("insert guard restoration fixture");
        let before_guard: (String, i64) = store
            .conn
            .query_row(
                "SELECT name,enabled FROM scheduled_jobs WHERE id=?1",
                [guard.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read guard before failed restore");
        guard.name = "guard-after".into();
        guard.enabled = true;
        fail_next_projection_refresh();
        assert!(store.restore_program_guard_scheduled_job(&guard).is_err());
        let after_guard: (String, i64) = store
            .conn
            .query_row(
                "SELECT name,enabled FROM scheduled_jobs WHERE id=?1",
                [guard.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read guard after failed restore");
        assert_eq!(after_guard, before_guard);

        let mut recovery = test_job(
            "recovery-before",
            Some(working),
            WakeMode::Resume,
            Some(owner),
            false,
        );
        store
            .insert_scheduled_job(&recovery)
            .expect("insert recovery upsert fixture");
        let before_recovery: (String, i64) = store
            .conn
            .query_row(
                "SELECT name,enabled FROM scheduled_jobs WHERE id=?1",
                [recovery.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read recovery before failed upsert");
        recovery.name = "recovery-after".into();
        recovery.enabled = true;
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open recovery upsert transaction");
        fail_next_projection_refresh();
        assert!(
            super::super::scheduled_jobs::upsert_master_no_idle_recovery_wake_tx(&tx, &recovery)
                .is_err()
        );
        tx.rollback().expect("roll back failed recovery upsert");
        let after_recovery: (String, i64) = store
            .conn
            .query_row(
                "SELECT name,enabled FROM scheduled_jobs WHERE id=?1",
                [recovery.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read recovery after failed upsert");
        assert_eq!(after_recovery, before_recovery);
    }

    #[test]
    fn v120_startup_reconcile_prioritizes_enabled_jobs_over_disabled_history() {
        let store = Store::open_in_memory().expect("open V120 reconcile fixture");
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
            .expect("open reconcile history transaction");
        for ordinal in 1..=SOURCE_WORKTREE_BATCH_MAX_SCAN_ROOTS as u128 {
            tx.execute(
                "INSERT INTO scheduled_jobs(
                    id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,working_dir,
                    provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id)
                 VALUES(?1,'disabled-history','disabled','{}',NULL,?2,0,NULL,NULL,NULL,NULL,?2,?2,'fresh',NULL)",
                params![Uuid::from_u128(ordinal).to_string(), TEST_NOW],
            )
            .expect("insert disabled reconciliation history");
        }
        let enabled_id = Uuid::from_u128(u128::MAX);
        tx.execute(
            "INSERT INTO scheduled_jobs(
                id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,working_dir,
                provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id)
             VALUES(?1,'enabled-late','enabled','{}',NULL,?2,1,NULL,NULL,NULL,NULL,?2,?2,'fresh',NULL)",
            params![enabled_id.to_string(), TEST_NOW],
        )
        .expect("insert enabled job after disabled history");
        tx.commit().expect("commit reconciliation history");

        reconcile_scheduled_job_path_projections(&store.conn)
            .expect("bounded reconciliation must reach enabled late job");
        let state: String = store
            .conn
            .query_row(
                "SELECT verification_state FROM scheduled_job_path_projections WHERE job_id=?1",
                [enabled_id.to_string()],
                |row| row.get(0),
            )
            .expect("read enabled late projection");
        assert_eq!(state, "not_applicable");
        let disabled_unverified: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM scheduled_job_path_projections
                  WHERE raw_enabled=0 AND verification_state='unverified'",
                [],
                |row| row.get(0),
            )
            .expect("count unreconciled disabled history");
        assert_eq!(disabled_unverified, 1);
    }

    #[test]
    fn v120_migration_failpoints_rollback_exact_v119_and_allow_fresh_cycles() {
        let store = Store::open_in_memory().expect("open V120 migration fixture");
        let legacy_session = Uuid::from_u128(0x8f01);
        let legacy_working = Path::new("/v120/legacy-session-health");
        insert_test_session(&store.conn, legacy_session, legacy_working, "Deleted");
        store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET execution_state='historical_purged',freshness='verified',
                        canonical_repo_dir=?1,effective_cwd=NULL,custody_id=NULL,
                        custody_generation=NULL,validated_at=?2,error_code=NULL,updated_at=?2
                  WHERE session_id=?3",
                params![
                    legacy_working.to_string_lossy().into_owned(),
                    TEST_NOW,
                    legacy_session.to_string()
                ],
            )
            .expect("authenticate legacy inert Session before rewind");
        for fault in SourceWorktreeV120MigrationFault::ALL {
            rewind_store_to_schema_version(&store.conn, 119);
            assert_eq!(
                catalog_fingerprint(&store.conn).expect("fingerprint rewound V119"),
                target_reclaim_sweep::V119_FULL_CATALOG_FINGERPRINT
            );
            fail_next_v120_migration(fault);
            let error = store
                .apply_source_worktree_v120_migration()
                .expect_err("injected V120 migration fault must abort");
            assert!(error.to_string().contains("injected V120"), "{error}");
            assert_eq!(
                store
                    .conn
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                    .expect("read version after V120 fault"),
                119
            );
            assert_eq!(
                catalog_fingerprint(&store.conn).expect("fingerprint after V120 fault"),
                target_reclaim_sweep::V119_FULL_CATALOG_FINGERPRINT
            );
            let leaked: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name LIKE 'source_worktree_batch_%'
                       OR name LIKE '%_v120_%' OR name LIKE 'idx_swc_v120_%'",
                    [],
                    |row| row.get(0),
                )
                .expect("count leaked V120 objects");
            assert_eq!(leaked, 0, "fault {fault:?} leaked V120 catalog state");
            store
                .apply_source_worktree_v120_migration()
                .expect("V120 converges after fault rollback");
            validate_v120_catalog(&store.conn).expect("validate converged V120 catalog");
            assert_eq!(
                store
                    .conn
                    .query_row(
                        "SELECT relevance FROM source_worktree_session_dependency_health
                          WHERE session_id=?1",
                        [legacy_session.to_string()],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("read V119-to-V120 seeded Session classification"),
                0
            );
        }

        let now = TEST_NOW;
        let insert_run = |run_id: Uuid, snapshot: char| {
            store
                .conn
                .execute(
                    "INSERT INTO source_worktree_batch_runs(
                        run_id,schema_version,policy_version,repository_identity_digest,
                        cursor_digest,snapshot_digest,snapshot_root_count,target_ref,target_oid,
                        upper_custody_id,page_start_custody_id,page_end_custody_id,plan_digest,
                        predecessor_receipt_digest,terminal_receipt_digest,batch_ordinal,has_more,
                        state,created_at,updated_at)
                     VALUES(?1,2,2,?2,?3,?4,0,'refs/heads/rolling',?5,NULL,NULL,NULL,?6,
                            NULL,NULL,0,0,'audited',?7,?7)",
                    params![
                        run_id.to_string(),
                        "1".repeat(64),
                        "2".repeat(64),
                        format!("sha256:{}", snapshot.to_string().repeat(64)),
                        "4".repeat(40),
                        format!("sha256:{}", "5".repeat(64)),
                        now,
                    ],
                )
                .expect("a fresh retained audit cycle may restart ordinal zero");
        };
        insert_run(Uuid::from_u128(0x9001), 'a');
        insert_run(Uuid::from_u128(0x9002), 'b');
        let cycles: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM source_worktree_batch_runs
                  WHERE repository_identity_digest=?1 AND batch_ordinal=0",
                ["1".repeat(64)],
                |row| row.get(0),
            )
            .expect("count fresh audit cycles");
        assert_eq!(cycles, 2);
    }

    #[test]
    fn v120_disk_reopen_preserves_secret_and_cursor_and_refuses_catalog_poison() {
        let directory = tempfile::tempdir().expect("create disk-backed V120 fixture");
        let database = directory.path().join("reopen.sqlite");
        let (key_before, cursor) = {
            let store = Store::open(&database).expect("create disk-backed V120 store");
            let key = cursor_key(&store.conn).expect("read generated V120 cursor key");
            let snapshot = store
                .source_worktree_snapshot_capture("repo:v120:reopen", None)
                .expect("capture disk-backed cursor snapshot");
            let cursor = issue_cursor_v2(
                &store.conn,
                "repo:v120:reopen",
                &snapshot,
                &CursorFenceV2 {
                    target_ref: "refs/heads/rolling",
                    target_oid: &"7".repeat(40),
                    batch_ordinal: 0,
                    previous_receipt_digest: None,
                },
            )
            .expect("issue disk-backed V120 cursor");
            assert!(
                store
                    .conn
                    .execute(
                        "UPDATE source_worktree_batch_cursor_keys SET cursor_key=?1 WHERE key_id=1",
                        [vec![8_u8; 32]],
                    )
                    .is_err(),
                "cursor key update must be rejected"
            );
            assert!(
                store
                    .conn
                    .execute(
                        "DELETE FROM source_worktree_batch_cursor_keys WHERE key_id=1",
                        []
                    )
                    .is_err(),
                "cursor key delete must be rejected"
            );
            (key, cursor)
        };
        {
            let reopened = Store::open(&database).expect("reopen disk-backed V120 store");
            assert_eq!(cursor_key(&reopened.conn).unwrap(), key_before);
            let claims = verify_cursor_v2(&reopened.conn, &cursor, "repo:v120:reopen")
                .expect("cursor remains valid after disk-backed reopen");
            assert_eq!(claims.upper_custody_id, None);
        }

        let poison = Connection::open(&database).expect("open raw poison connection");
        poison
            .execute_batch(
                "DROP INDEX idx_swc_v120_execution_cwd;
                 CREATE INDEX idx_swc_v120_execution_cwd
                    ON session_execution_projections(session_id,effective_cwd);",
            )
            .expect("poison a named V120 catalog definition");
        drop(poison);
        let error = match Store::open(&database) {
            Ok(_) => panic!("reopen must reject V120 catalog poison"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("V120 catalog fingerprint mismatch"),
            "{error}"
        );
    }
}
