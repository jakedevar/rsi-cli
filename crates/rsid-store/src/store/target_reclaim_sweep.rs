//! Durable finite sweep state for terminal sandbox target reclamation.

use super::{Store, sandbox_custody::TerminalCustodyReclaimCandidate};
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

const MAX_PAGE_SIZE: u32 =
    rsi_common::sandbox_storage::SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MAX;
const SWEEP_BUSY_TIMEOUT: Duration = Duration::from_millis(50);

// RSI-RELEASED-MIGRATION-BEGIN: v119-target-reclaim-sweep-fingerprints
pub(crate) const V118_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:391bd93dad3ced9c8663eedc948c39f9fd01f07a81fd34cbafc7e5cb21040e26";
pub(crate) const V119_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:0682d2612479d70e501a1c500694cb80b7c7add11aa62de7533ee17792809e33";
// RSI-RELEASED-MIGRATION-END: v119-target-reclaim-sweep-fingerprints

// RSI-RELEASED-MIGRATION-BEGIN: v119-target-reclaim-sweep-catalog
pub(crate) const V119_CATALOG_OBJECTS: [(&str, &str); 19] = [
    ("index", "idx_sandbox_custody_roots_reclaim_owner_v119"),
    ("index", "idx_sandbox_custody_roots_repository_state_v119"),
    (
        "index",
        "idx_sandbox_target_reclaim_intent_events_identity_v119",
    ),
    ("index", "idx_sandbox_target_reclaim_intents_state_v119"),
    ("index", "idx_sessions_terminal_reclaim_page_v119"),
    ("table", "sandbox_reclaim_sweeps"),
    ("table", "sandbox_target_reclaim_intent_events"),
    ("table", "sandbox_target_reclaim_intent_sweeps"),
    ("table", "sandbox_target_reclaim_intents"),
    (
        "trigger",
        "sandbox_target_reclaim_intent_events_v119_exact_insert",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intent_events_v119_no_delete",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intent_events_v119_no_update",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intent_sweeps_v119_forward_update",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intent_sweeps_v119_no_delete",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intents_v119_forward_update",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intents_v119_identity_immutable",
    ),
    (
        "trigger",
        "sandbox_target_reclaim_intents_v119_terminal_delete",
    ),
    ("trigger", "sandbox_reclaim_sweeps_v119_forward_update"),
    ("trigger", "sandbox_reclaim_sweeps_v119_no_delete"),
];

pub(crate) fn install_v119_table_and_row(tx: &Transaction<'_>, now: &str) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE sandbox_reclaim_sweeps (
            sweep_id INTEGER PRIMARY KEY NOT NULL CHECK(sweep_id=1),
            schema_version INTEGER NOT NULL CHECK(schema_version=1),
            cycle INTEGER NOT NULL CHECK(cycle>=0),
            after_updated_at TEXT
                CHECK(after_updated_at IS NULL OR rsi_rfc3339_is_valid(after_updated_at)),
            after_session_id TEXT
                CHECK(after_session_id IS NULL OR rsi_uuid_is_canonical(after_session_id)),
            upper_updated_at TEXT
                CHECK(upper_updated_at IS NULL OR rsi_rfc3339_is_valid(upper_updated_at)),
            upper_session_id TEXT
                CHECK(upper_session_id IS NULL OR rsi_uuid_is_canonical(upper_session_id)),
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            CHECK((after_updated_at IS NULL)=(after_session_id IS NULL)),
            CHECK((upper_updated_at IS NULL)=(upper_session_id IS NULL)),
            CHECK(upper_updated_at IS NOT NULL OR after_updated_at IS NULL),
            CHECK(after_updated_at IS NULL OR after_updated_at<upper_updated_at
                OR (after_updated_at=upper_updated_at AND after_session_id<=upper_session_id))
        );
        CREATE TABLE sandbox_target_reclaim_intents (
            schedule_id INTEGER PRIMARY KEY AUTOINCREMENT,
            custody_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(custody_id)),
            generation INTEGER NOT NULL CHECK(generation>0),
            allocation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(allocation_id)),
            bucket INTEGER NOT NULL CHECK(bucket BETWEEN 0 AND 255),
            slot_name TEXT NOT NULL
                CHECK(slot_name='v3_' || custody_id || '_' || CAST(generation AS TEXT)),
            payload_name TEXT NOT NULL CHECK(payload_name='payload'),
            expected_device INTEGER NOT NULL CHECK(expected_device>=0),
            expected_inode INTEGER NOT NULL CHECK(expected_inode>0),
            state TEXT NOT NULL CHECK(state IN ('Prepared','Staged','Deleting')),
            row_version INTEGER NOT NULL CHECK(row_version>0),
            prepared_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(prepared_at)),
            staged_at TEXT CHECK(staged_at IS NULL OR rsi_rfc3339_nanos_is_canonical(staged_at)),
            deleting_at TEXT
                CHECK(deleting_at IS NULL OR rsi_rfc3339_nanos_is_canonical(deleting_at)),
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            UNIQUE(custody_id,generation),
            UNIQUE(bucket,slot_name),
            FOREIGN KEY(custody_id)
                REFERENCES sandbox_custody_roots(custody_id)
                ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            CHECK((state='Prepared' AND row_version=1 AND staged_at IS NULL AND deleting_at IS NULL)
               OR (state='Staged' AND row_version>=2 AND staged_at IS NOT NULL AND deleting_at IS NULL)
               OR (state='Deleting' AND row_version>=3 AND staged_at IS NOT NULL
                   AND deleting_at IS NOT NULL)),
            CHECK(updated_at>=prepared_at
              AND (staged_at IS NULL OR staged_at>=prepared_at)
              AND (deleting_at IS NULL OR deleting_at>=staged_at))
        );
        CREATE TABLE sandbox_target_reclaim_intent_sweeps (
            sweep_id INTEGER PRIMARY KEY NOT NULL CHECK(sweep_id=1),
            schema_version INTEGER NOT NULL CHECK(schema_version=1),
            cycle INTEGER NOT NULL CHECK(cycle>=0),
            after_schedule_id INTEGER CHECK(after_schedule_id>0),
            upper_schedule_id INTEGER CHECK(upper_schedule_id>0),
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            CHECK(upper_schedule_id IS NOT NULL OR after_schedule_id IS NULL),
            CHECK(after_schedule_id IS NULL OR after_schedule_id<=upper_schedule_id)
        );
        CREATE TABLE sandbox_target_reclaim_intent_events (
            event_id INTEGER PRIMARY KEY AUTOINCREMENT,
            schedule_id INTEGER NOT NULL CHECK(schedule_id>0),
            custody_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(custody_id)),
            generation INTEGER NOT NULL CHECK(generation>0),
            allocation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(allocation_id)),
            bucket INTEGER NOT NULL CHECK(bucket BETWEEN 0 AND 255),
            slot_name TEXT NOT NULL
                CHECK(slot_name='v3_' || custody_id || '_' || CAST(generation AS TEXT)),
            payload_name TEXT NOT NULL CHECK(payload_name='payload'),
            expected_device INTEGER NOT NULL CHECK(expected_device>=0),
            expected_inode INTEGER NOT NULL CHECK(expected_inode>0),
            terminal_state TEXT NOT NULL CHECK(terminal_state IN ('Completed','Abandoned')),
            reason TEXT NOT NULL CHECK(length(reason) BETWEEN 1 AND 96),
            final_row_version INTEGER NOT NULL CHECK(final_row_version>0),
            recorded_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(recorded_at)),
            UNIQUE(schedule_id),
            UNIQUE(custody_id,generation)
        );",
    )?;
    tx.execute(
        "INSERT INTO sandbox_reclaim_sweeps(
            sweep_id,schema_version,cycle,after_updated_at,after_session_id,
            upper_updated_at,upper_session_id,updated_at)
         VALUES(1,1,0,NULL,NULL,NULL,NULL,?1)",
        [now],
    )?;
    tx.execute(
        "INSERT INTO sandbox_target_reclaim_intent_sweeps(
            sweep_id,schema_version,cycle,after_schedule_id,upper_schedule_id,updated_at)
         VALUES(1,1,0,NULL,NULL,?1)",
        [now],
    )?;
    Ok(())
}

pub(crate) fn install_v119_indexes(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE INDEX idx_sessions_terminal_reclaim_page_v119
            ON sessions(updated_at,id)
            WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted');
        CREATE INDEX idx_sandbox_custody_roots_reclaim_owner_v119
            ON sandbox_custody_roots(owner_session_id,generation,custody_id)
            WHERE state='live' AND validation_state='verified'
              AND reserved_effects=0 AND active_effects=0;
        CREATE INDEX idx_sandbox_custody_roots_repository_state_v119
            ON sandbox_custody_roots(repository_identity,state,custody_id,generation);
        CREATE INDEX idx_sandbox_target_reclaim_intents_state_v119
            ON sandbox_target_reclaim_intents(state,schedule_id);
        CREATE INDEX idx_sandbox_target_reclaim_intent_events_identity_v119
            ON sandbox_target_reclaim_intent_events(custody_id,generation,event_id);",
    )?;
    Ok(())
}

pub(crate) fn install_v119_triggers(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TRIGGER sandbox_reclaim_sweeps_v119_forward_update
        BEFORE UPDATE ON sandbox_reclaim_sweeps
        WHEN NEW.sweep_id IS NOT OLD.sweep_id
          OR NEW.schema_version IS NOT OLD.schema_version
          OR NEW.cycle<OLD.cycle OR NEW.cycle>OLD.cycle+1
          OR NEW.updated_at<OLD.updated_at
          OR CASE
             WHEN NEW.cycle=OLD.cycle THEN CASE WHEN
                OLD.upper_updated_at IS NOT NULL
                AND (
                    (NEW.upper_updated_at IS OLD.upper_updated_at
                     AND NEW.upper_session_id IS OLD.upper_session_id
                     AND NEW.after_updated_at IS NOT NULL
                     AND (OLD.after_updated_at IS NULL
                          OR NEW.after_updated_at>OLD.after_updated_at
                          OR (NEW.after_updated_at=OLD.after_updated_at
                              AND NEW.after_session_id>OLD.after_session_id)))
                    OR
                    (NEW.upper_updated_at IS NULL AND NEW.upper_session_id IS NULL
                     AND NEW.after_updated_at IS NULL AND NEW.after_session_id IS NULL)
                ) THEN 0 ELSE 1 END
             WHEN NEW.cycle=OLD.cycle+1 THEN CASE WHEN
                OLD.upper_updated_at IS NULL AND OLD.upper_session_id IS NULL
                AND OLD.after_updated_at IS NULL AND OLD.after_session_id IS NULL
                THEN 0 ELSE 1 END
             ELSE 1 END
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim sweep is forward-only'); END;
        CREATE TRIGGER sandbox_reclaim_sweeps_v119_no_delete
        BEFORE DELETE ON sandbox_reclaim_sweeps
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim sweep is retained'); END;

        CREATE TRIGGER sandbox_target_reclaim_intents_v119_identity_immutable
        BEFORE UPDATE ON sandbox_target_reclaim_intents
        WHEN NEW.schedule_id!=OLD.schedule_id
          OR NEW.custody_id!=OLD.custody_id OR NEW.generation!=OLD.generation
          OR NEW.allocation_id!=OLD.allocation_id OR NEW.bucket!=OLD.bucket
          OR NEW.slot_name!=OLD.slot_name OR NEW.payload_name!=OLD.payload_name
          OR NEW.expected_device!=OLD.expected_device OR NEW.expected_inode!=OLD.expected_inode
          OR NEW.prepared_at!=OLD.prepared_at
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent identity is immutable'); END;

        CREATE TRIGGER sandbox_target_reclaim_intents_v119_forward_update
        BEFORE UPDATE ON sandbox_target_reclaim_intents
        WHEN NEW.row_version!=OLD.row_version+1 OR NEW.updated_at<OLD.updated_at
          OR NOT ((OLD.state='Prepared' AND NEW.state='Staged'
                   AND OLD.staged_at IS NULL AND NEW.staged_at IS NOT NULL
                   AND NEW.deleting_at IS NULL)
              OR (OLD.state='Staged' AND NEW.state='Deleting'
                   AND NEW.staged_at=OLD.staged_at AND OLD.deleting_at IS NULL
                   AND NEW.deleting_at IS NOT NULL))
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent transition is not forward'); END;

        CREATE TRIGGER sandbox_target_reclaim_intents_v119_terminal_delete
        BEFORE DELETE ON sandbox_target_reclaim_intents
        WHEN NOT EXISTS (
            SELECT 1 FROM sandbox_target_reclaim_intent_events e
            WHERE e.schedule_id=OLD.schedule_id
              AND e.custody_id=OLD.custody_id AND e.generation=OLD.generation
              AND e.final_row_version=OLD.row_version
              AND ((OLD.state='Deleting' AND e.terminal_state='Completed')
                   OR (OLD.state='Prepared' AND e.terminal_state='Abandoned'))
        )
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent requires terminal evidence'); END;

        CREATE TRIGGER sandbox_target_reclaim_intent_sweeps_v119_forward_update
        BEFORE UPDATE ON sandbox_target_reclaim_intent_sweeps
        WHEN NEW.sweep_id IS NOT OLD.sweep_id
          OR NEW.schema_version IS NOT OLD.schema_version
          OR NEW.cycle<OLD.cycle OR NEW.cycle>OLD.cycle+1
          OR NEW.updated_at<OLD.updated_at
          OR CASE
             WHEN NEW.cycle=OLD.cycle THEN CASE WHEN
                OLD.upper_schedule_id IS NOT NULL
                AND ((NEW.upper_schedule_id=OLD.upper_schedule_id
                      AND NEW.after_schedule_id IS NOT NULL
                      AND (OLD.after_schedule_id IS NULL
                           OR NEW.after_schedule_id>OLD.after_schedule_id))
                     OR (NEW.upper_schedule_id IS NULL
                         AND NEW.after_schedule_id IS NULL))
                THEN 0 ELSE 1 END
             WHEN NEW.cycle=OLD.cycle+1 THEN CASE WHEN
                OLD.upper_schedule_id IS NULL AND OLD.after_schedule_id IS NULL
                THEN 0 ELSE 1 END
             ELSE 1 END
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent sweep is forward-only'); END;
        CREATE TRIGGER sandbox_target_reclaim_intent_sweeps_v119_no_delete
        BEFORE DELETE ON sandbox_target_reclaim_intent_sweeps
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim intent sweep is retained'); END;

        CREATE TRIGGER sandbox_target_reclaim_intent_events_v119_exact_insert
        BEFORE INSERT ON sandbox_target_reclaim_intent_events
        WHEN NOT EXISTS (
            SELECT 1 FROM sandbox_target_reclaim_intents i
            WHERE i.schedule_id=NEW.schedule_id AND i.custody_id=NEW.custody_id
              AND i.generation=NEW.generation AND i.allocation_id=NEW.allocation_id
              AND i.bucket=NEW.bucket AND i.slot_name=NEW.slot_name
              AND i.payload_name=NEW.payload_name
              AND i.expected_device=NEW.expected_device
              AND i.expected_inode=NEW.expected_inode
              AND i.row_version=NEW.final_row_version
              AND ((i.state='Deleting' AND NEW.terminal_state='Completed')
                   OR (i.state='Prepared' AND NEW.terminal_state='Abandoned'))
        )
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim terminal evidence identity mismatch'); END;

        CREATE TRIGGER sandbox_target_reclaim_intent_events_v119_no_update
        BEFORE UPDATE ON sandbox_target_reclaim_intent_events
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim terminal evidence is immutable'); END;
        CREATE TRIGGER sandbox_target_reclaim_intent_events_v119_no_delete
        BEFORE DELETE ON sandbox_target_reclaim_intent_events
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim terminal evidence is retained'); END;",
    )?;
    Ok(())
}

pub(crate) fn validate_v119_catalog(connection: &Connection) -> Result<()> {
    for (kind, name) in V119_CATALOG_OBJECTS {
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type=?1 AND name=?2 AND sql IS NOT NULL",
            params![kind, name],
            |row| row.get(0),
        )?;
        if count != 1 {
            return Err(DaemonError::Store(format!(
                "V119 target reclaim catalog missing {kind} {name}"
            )));
        }
    }
    let rows: i64 = connection.query_row(
        "SELECT count(*) FROM sandbox_reclaim_sweeps
         WHERE sweep_id=1 AND schema_version=1 AND cycle>=0",
        [],
        |row| row.get(0),
    )?;
    if rows != 1 {
        return Err(DaemonError::Store(format!(
            "V119 target reclaim singleton row mismatch: {rows}"
        )));
    }
    let intent_sweep_rows: i64 = connection.query_row(
        "SELECT count(*) FROM sandbox_target_reclaim_intent_sweeps
         WHERE sweep_id=1 AND schema_version=1 AND cycle>=0",
        [],
        |row| row.get(0),
    )?;
    if intent_sweep_rows != 1 {
        return Err(DaemonError::Store(format!(
            "V119 target reclaim intent singleton row mismatch: {intent_sweep_rows}"
        )));
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v119-target-reclaim-sweep-catalog

// RSI-RELEASED-MIGRATION-BEGIN: target-reclaim-identity-events-migration
/// Provisional schema version of the identity-keyed terminal evidence rebuild
/// (#1035). The lander assigns the final number.
pub(crate) const TARGET_RECLAIM_IDENTITY_SCHEMA_VERSION: i32 = 141;

const IDENTITY_EVENTS_TABLE: &str = "sandbox_target_reclaim_intent_events";

/// The events table is rebuilt so that terminal evidence is unique per exact
/// target identity, not per custody generation. A continued and rebuilt target
/// keeps its custody generation but has a new device/inode, and its reclaim
/// must be able to record its own terminal event beside the earlier one.
/// `unique_identity` selects the identity-keyed shape; `false` restores the V119 shape
/// (used by the migration-chain fixtures to rewind).
pub(crate) fn rebuild_events_table_sql(unique_identity: bool) -> String {
    let unique = if unique_identity {
        "UNIQUE(custody_id,generation,expected_device,expected_inode)"
    } else {
        "UNIQUE(custody_id,generation)"
    };
    // The old table is set aside (its triggers and index dropped by name), the
    // final table is created under its final name so the stored catalog text is
    // the canonical statement, and the rows are copied across in event order.
    format!(
        "DROP TRIGGER sandbox_target_reclaim_intent_events_v119_exact_insert;
        DROP TRIGGER sandbox_target_reclaim_intent_events_v119_no_update;
        DROP TRIGGER sandbox_target_reclaim_intent_events_v119_no_delete;
        DROP INDEX idx_sandbox_target_reclaim_intent_events_identity_v119;
        ALTER TABLE {IDENTITY_EVENTS_TABLE} RENAME TO sandbox_target_reclaim_intent_events_old;
        CREATE TABLE sandbox_target_reclaim_intent_events (
            event_id INTEGER PRIMARY KEY AUTOINCREMENT,
            schedule_id INTEGER NOT NULL CHECK(schedule_id>0),
            custody_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(custody_id)),
            generation INTEGER NOT NULL CHECK(generation>0),
            allocation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(allocation_id)),
            bucket INTEGER NOT NULL CHECK(bucket BETWEEN 0 AND 255),
            slot_name TEXT NOT NULL
                CHECK(slot_name='v3_' || custody_id || '_' || CAST(generation AS TEXT)),
            payload_name TEXT NOT NULL CHECK(payload_name='payload'),
            expected_device INTEGER NOT NULL CHECK(expected_device>=0),
            expected_inode INTEGER NOT NULL CHECK(expected_inode>0),
            terminal_state TEXT NOT NULL CHECK(terminal_state IN ('Completed','Abandoned')),
            reason TEXT NOT NULL CHECK(length(reason) BETWEEN 1 AND 96),
            final_row_version INTEGER NOT NULL CHECK(final_row_version>0),
            recorded_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(recorded_at)),
            UNIQUE(schedule_id),
            {unique}
        );
        INSERT INTO sandbox_target_reclaim_intent_events(
            event_id,schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
            payload_name,expected_device,expected_inode,terminal_state,reason,
            final_row_version,recorded_at)
        SELECT event_id,schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
               payload_name,expected_device,expected_inode,terminal_state,reason,
               final_row_version,recorded_at
        FROM sandbox_target_reclaim_intent_events_old ORDER BY event_id;
        DROP TABLE sandbox_target_reclaim_intent_events_old;
        CREATE INDEX idx_sandbox_target_reclaim_intent_events_identity_v119
            ON sandbox_target_reclaim_intent_events(custody_id,generation,event_id);
        CREATE TRIGGER sandbox_target_reclaim_intent_events_v119_exact_insert
        BEFORE INSERT ON sandbox_target_reclaim_intent_events
        WHEN NOT EXISTS (
            SELECT 1 FROM sandbox_target_reclaim_intents i
            WHERE i.schedule_id=NEW.schedule_id AND i.custody_id=NEW.custody_id
              AND i.generation=NEW.generation AND i.allocation_id=NEW.allocation_id
              AND i.bucket=NEW.bucket AND i.slot_name=NEW.slot_name
              AND i.payload_name=NEW.payload_name
              AND i.expected_device=NEW.expected_device
              AND i.expected_inode=NEW.expected_inode
              AND i.row_version=NEW.final_row_version
              AND ((i.state='Deleting' AND NEW.terminal_state='Completed')
                   OR (i.state='Prepared' AND NEW.terminal_state='Abandoned'))
        )
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim terminal evidence identity mismatch'); END;
        CREATE TRIGGER sandbox_target_reclaim_intent_events_v119_no_update
        BEFORE UPDATE ON sandbox_target_reclaim_intent_events
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim terminal evidence is immutable'); END;
        CREATE TRIGGER sandbox_target_reclaim_intent_events_v119_no_delete
        BEFORE DELETE ON sandbox_target_reclaim_intent_events
        BEGIN SELECT RAISE(ABORT,'V119 target reclaim terminal evidence is retained'); END;"
    )
}

pub(crate) fn apply_identity_events_migration(store: &Store, version: i32) -> Result<()> {
    // The intents table's terminal-delete trigger names the events table, so
    // RENAME must not rewrite or reject trigger references while the table is
    // briefly absent.
    store.conn.execute_batch("PRAGMA legacy_alter_table=ON;")?;
    let outcome = (|| -> Result<()> {
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
        let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if prior != version - 1 {
            return Err(DaemonError::Store(format!(
                "target reclaim identity evidence requires V{}, found V{prior}",
                version - 1
            )));
        }
        tx.execute_batch(&rebuild_events_table_sql(true))?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        Ok(())
    })();
    let restored = store.conn.execute_batch("PRAGMA legacy_alter_table=OFF;");
    outcome?;
    restored?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: target-reclaim-identity-events-migration

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetReclaimSweepV119MigrationFault {
    AfterPreflight,
    AfterTableAndRow,
    AfterIndexes,
    AfterTriggers,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

#[cfg(any(test, feature = "test-seam"))]
impl TargetReclaimSweepV119MigrationFault {
    const ALL: [Self; 7] = [
        Self::AfterPreflight,
        Self::AfterTableAndRow,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(any(test, feature = "test-seam"))]
thread_local! {
    static MIGRATION_FAULT: std::cell::Cell<Option<TargetReclaimSweepV119MigrationFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn fail_next_v119_migration(fault: TargetReclaimSweepV119MigrationFault) {
    MIGRATION_FAULT.with(|slot| slot.set(Some(fault)));
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn migration_fault(fault: TargetReclaimSweepV119MigrationFault) -> Result<()> {
    let injected = MIGRATION_FAULT.with(|slot| {
        if slot.get() == Some(fault) {
            slot.set(None);
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V119 target reclaim migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(any(test, feature = "test-seam")))]
pub(crate) fn migration_fault(_: TargetReclaimSweepV119MigrationFault) -> Result<()> {
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetReclaimIntentState {
    Prepared,
    Staged,
    Deleting,
}

impl TargetReclaimIntentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "Prepared",
            Self::Staged => "Staged",
            Self::Deleting => "Deleting",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "Prepared" => Ok(Self::Prepared),
            "Staged" => Ok(Self::Staged),
            "Deleting" => Ok(Self::Deleting),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetReclaimIntent {
    pub schedule_id: u64,
    pub custody_id: Uuid,
    pub generation: u64,
    pub allocation_id: Uuid,
    pub bucket: u8,
    pub slot_name: String,
    pub expected_device: u64,
    pub expected_inode: u64,
    pub state: TargetReclaimIntentState,
    pub row_version: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TargetReclaimIntentCounts {
    pub prepared: u32,
    pub staged: u32,
    pub deleting: u32,
    pub completed: u32,
    pub abandoned: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetReclaimIntentTerminalState {
    Completed,
    Abandoned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TargetReclaimIntentEvent {
    pub schedule_id: u64,
    pub custody_id: Uuid,
    pub generation: u64,
    pub allocation_id: Uuid,
    pub bucket: u8,
    pub slot_name: String,
    pub expected_device: u64,
    pub expected_inode: u64,
    pub terminal_state: TargetReclaimIntentTerminalState,
    /// When the terminal event was durably recorded. A target directory born
    /// after this instant cannot be the target the event settled (#1040).
    pub recorded_at: chrono::DateTime<Utc>,
}

/// Flag bit marking a store device value that carries a target epoch.
const TARGET_EPOCH_FLAG: u64 = 1 << 62;
const TARGET_EPOCH_SHIFT: u32 = 32;
const TARGET_EPOCH_MASK: u64 = (1 << 30) - 1;
/// The largest epoch a store device value can carry.
pub(crate) const TARGET_EPOCH_MAX: u32 = TARGET_EPOCH_MASK as u32;

/// The device value the store keys reclaim evidence by. Epoch 0 is the raw
/// filesystem device. A later target that reuses the device and inode of a
/// reclaimed one (ext4 recycles freed inodes) is keyed under a higher epoch, so
/// its own intent and terminal evidence cannot collide with the earlier
/// target's (#1040). Only devices below 2^32 (every Linux `st_dev`) can carry
/// an epoch; `None` means the epoch is not representable.
pub(crate) fn store_target_device(device: u64, epoch: u32) -> Option<u64> {
    if epoch == 0 {
        return Some(device);
    }
    if device >> TARGET_EPOCH_SHIFT != 0 || u64::from(epoch) > TARGET_EPOCH_MASK {
        return None;
    }
    Some(TARGET_EPOCH_FLAG | (u64::from(epoch) << TARGET_EPOCH_SHIFT) | device)
}

/// Split a store device value into the raw filesystem device and its epoch.
pub fn split_store_target_device(store_device: u64) -> (u64, u32) {
    if store_device & TARGET_EPOCH_FLAG == 0 {
        return (store_device, 0);
    }
    (
        store_device & u64::from(u32::MAX),
        ((store_device >> TARGET_EPOCH_SHIFT) & TARGET_EPOCH_MASK) as u32,
    )
}

/// Whether retained terminal evidence for a (device, inode) belongs to an
/// earlier target than the directory now open at that identity (#1040).
///
/// A recreated `target/` can reuse the reclaimed directory's device and inode
/// but never its birth time. A completed reclaim removed its source, so a target
/// that exists at a completed identity is a new one; without a birth time the
/// answer fails toward reclaiming. Abandoned evidence left its target in place,
/// so it is only superseded by a provably newer directory.
pub(crate) fn terminal_evidence_is_for_earlier_target(
    event: &TargetReclaimIntentEvent,
    birth: Option<chrono::DateTime<Utc>>,
) -> bool {
    match (event.terminal_state, birth) {
        (_, Some(birth)) => birth > event.recorded_at,
        (TargetReclaimIntentTerminalState::Completed, None) => true,
        (TargetReclaimIntentTerminalState::Abandoned, None) => false,
    }
}

/// The next identity epoch to probe. The first jump is derived from the birth
/// time so a rebuilt target lands on a stable, distinct epoch; without one, or
/// on a collision, epochs advance one at a time.
pub(crate) fn next_target_epoch(epoch: u32, birth: Option<chrono::DateTime<Utc>>) -> u32 {
    if epoch > 0 {
        return epoch.saturating_add(1);
    }
    let span = u64::from(TARGET_EPOCH_MAX);
    birth
        .and_then(|birth| birth.timestamp_nanos_opt())
        .map_or(1, |nanos| 1 + (nanos as u64 % span) as u32)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareTargetReclaimIntentResult {
    Active(TargetReclaimIntent),
    Terminal(TargetReclaimIntentEvent),
    SuccessorReservationPending,
}

#[cfg(any(test, feature = "test-seam"))]
impl PrepareTargetReclaimIntentResult {
    fn active(self) -> TargetReclaimIntent {
        match self {
            Self::Active(intent) => intent,
            Self::Terminal(_) | Self::SuccessorReservationPending => {
                panic!("new target reclaim fixture was not prepared")
            }
        }
    }
}

impl TargetReclaimIntentTerminalState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "Completed",
            Self::Abandoned => "Abandoned",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetReclaimIntentSweepEvidence {
    pub cycle_before: u64,
    pub cycle_after: u64,
    pub cursor_before: Option<u64>,
    pub cursor_after: Option<u64>,
    pub upper_bound: Option<u64>,
    pub page_key_digest: String,
    pub reserved: bool,
    pub wrapped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetReclaimIntentSweepPage {
    pub intents: Vec<TargetReclaimIntent>,
    pub has_more: bool,
    pub evidence: TargetReclaimIntentSweepEvidence,
}

pub fn target_reclaim_bucket(custody_id: Uuid, generation: u64) -> u8 {
    let mut digest = Sha256::new();
    digest.update(b"rsi.target-reclaim-bucket.v3\0");
    digest.update(custody_id.as_bytes());
    digest.update(generation.to_be_bytes());
    digest.finalize()[0]
}

pub fn target_reclaim_slot_name(custody_id: Uuid, generation: u64) -> String {
    format!("v3_{custody_id}_{generation}")
}

impl Store {
    /// Record each newly staged cache and queue a durable owning-manager notice.
    /// No paths or process command lines enter friction telemetry.
    pub fn record_target_reclaim_notice(
        &self,
        session_id: Uuid,
        custody_id: Uuid,
        generation: u64,
        bytes: u64,
        pending: bool,
    ) -> Result<Option<Uuid>> {
        use rsi_common::friction::{FrictionKind, NewFrictionEventV1};
        let session = self.get_session(session_id)?;
        let project = session.and_then(|session| session.project_id);
        let outcome = if pending { "pending" } else { "removed" };
        self.record_friction_event(
            &NewFrictionEventV1::new(FrictionKind::SandboxTargetReclaim, &[outcome])
                .session(Some(session_id))
                .evidence("custody", custody_id),
        )?;
        let Some(project) = project else {
            return Ok(None);
        };
        let Some(config) = self.get_harness_manager_notice_config(project)? else {
            return Ok(None);
        };
        if config.current_session_id.is_none() || config.is_revoked() {
            return Ok(None);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version = super::harness_manager_v2::now();
        let subject = format!("sandbox_target_reclaim:{custody_id}");
        let (job_id, _, _) = self.manager_action_watch_identity(&config);
        self.ensure_manager_action_watch(&config, &version)?; // sql-dynamic-ok: watch identity, not SQL
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',?6,?4,'ledger_change',?7,?8,?9,?8,?8)",
            params![
                Uuid::new_v4().to_string(),
                job_id.to_string(),
                project.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                session_id.to_string(),
                subject,
                version,
                serde_json::to_string(&serde_json::json!({
                    "record_kind": "sandbox_target_reclaim", "session_id": session_id,
                    "custody_id": custody_id, "generation": generation,
                    "bytes": bytes, "outcome": outcome,
                }))?
            ],
        )?;
        self.refresh_manager_notice_job(job_id)?;
        tx.commit()?;
        Ok(Some(job_id))
    }

    pub fn target_reclaim_intent(
        &self,
        custody_id: Uuid,
        generation: u64,
    ) -> Result<Option<TargetReclaimIntent>> {
        load_target_reclaim_intent(&self.conn, custody_id, generation)
    }

    pub(crate) fn prepared_target_reclaim_owner(
        &self,
        intent: &TargetReclaimIntent,
    ) -> Result<Option<Uuid>> {
        if intent.state != TargetReclaimIntentState::Prepared {
            return Ok(None);
        }
        self.conn
            .query_row(
                "SELECT s.id
                 FROM sandbox_custody_roots r
                 JOIN sessions s ON s.id=r.owner_session_id
                   AND s.sandbox_custody_id=r.custody_id
                 WHERE r.custody_id=?1 AND r.generation=?2 AND r.allocation_id=?3
                   AND r.state='live' AND r.validation_state='verified'
                   AND r.validated_generation=r.generation
                   AND r.reserved_effects=0 AND r.active_effects=0
                   AND s.status IN ('Completed','Failed','Interrupted','Archived','Deleted')
                   AND s.working_dir=r.canonical_repo_dir
                   AND s.sandbox_kind='GitWorktree'
                   AND s.sandbox_root=r.sandbox_root
                   AND s.sandbox_branch=r.sandbox_branch
                   AND s.sandbox_cleanup_state='Live'",
                params![
                    intent.custody_id.to_string(),
                    sqlite_u64(intent.generation, "target reclaim generation")?,
                    intent.allocation_id.to_string(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| {
                Uuid::parse_str(&value)
                    .map_err(|_| DaemonError::Store("target reclaim owner UUID is invalid".into()))
            })
            .transpose()
    }

    pub fn prepare_target_reclaim_intent(
        &self,
        custody_id: Uuid,
        generation: u64,
        expected_device: u64,
        expected_inode: u64,
    ) -> Result<PrepareTargetReclaimIntentResult> {
        if generation == 0 || expected_inode == 0 {
            return Err(DaemonError::InvalidParam(
                "target reclaim intent identity is invalid".into(),
            ));
        }
        let expected_device = i64::try_from(expected_device)
            .map_err(|_| DaemonError::Store("target reclaim device exceeds SQLite".into()))?;
        let expected_inode = i64::try_from(expected_inode)
            .map_err(|_| DaemonError::Store("target reclaim inode exceeds SQLite".into()))?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(event) = load_target_reclaim_intent_event(
            &tx,
            custody_id,
            generation,
            expected_device,
            expected_inode,
        )? {
            tx.commit()?;
            return Ok(PrepareTargetReclaimIntentResult::Terminal(event));
        }
        if self.target_reclaim_has_live_consumer(custody_id, generation)? {
            return Err(DaemonError::Store(
                "target reclaim prepare has a live consumer".into(),
            ));
        }
        let allocation_id = tx
            .query_row(
                "SELECT r.allocation_id
                 FROM sandbox_custody_roots r
                 JOIN sessions s ON s.id=r.owner_session_id
                   AND s.sandbox_custody_id=r.custody_id
                 WHERE r.custody_id=?1 AND r.generation=?2 AND r.state='live'
                   AND r.validation_state='verified'
                   AND r.validated_generation=r.generation
                   AND r.reserved_effects=0 AND r.active_effects=0
                   AND s.status IN ('Completed','Failed','Interrupted','Archived','Deleted')
                   AND s.sandbox_cleanup_state='Live'",
                params![
                    custody_id.to_string(),
                    sqlite_u64(generation, "target reclaim generation")?
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::Store("target reclaim prepare lost custody fence".into())
            })?;
        let allocation_id = Uuid::parse_str(&allocation_id)
            .map_err(|_| DaemonError::Store("target reclaim allocation UUID is invalid".into()))?;
        // A retry or rotation can persist a Starting successor before custody
        // bind. Its reservation wins over a later reclaim gate in this same
        // SQLite writer order, so bind cannot strand an admitted successor.
        let successor_pending: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sandbox_custody_roots r
                JOIN sessions child ON child.continued_from=r.owner_session_id
                WHERE r.custody_id=?1 AND r.generation=?2
                  AND child.status='Starting' AND child.sandbox_custody_id IS NULL
                  AND child.sandbox_kind='GitWorktree'
                  AND child.sandbox_root=r.sandbox_root
                  AND child.sandbox_branch=r.sandbox_branch
                  AND child.sandbox_cleanup_state='Live'
            )",
            params![
                custody_id.to_string(),
                sqlite_u64(generation, "target reclaim generation")?
            ],
            |row| row.get(0),
        )?;
        if successor_pending {
            tx.commit()?;
            return Ok(PrepareTargetReclaimIntentResult::SuccessorReservationPending);
        }
        let bucket = target_reclaim_bucket(custody_id, generation);
        let slot_name = target_reclaim_slot_name(custody_id, generation);
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        tx.execute(
            "INSERT OR IGNORE INTO sandbox_target_reclaim_intents(
                custody_id,generation,allocation_id,bucket,slot_name,payload_name,
                expected_device,expected_inode,state,row_version,prepared_at,staged_at,
                deleting_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,'payload',?6,?7,'Prepared',1,?8,NULL,NULL,?8)",
            params![
                custody_id.to_string(),
                sqlite_u64(generation, "target reclaim generation")?,
                allocation_id.to_string(),
                i64::from(bucket),
                slot_name,
                expected_device,
                expected_inode,
                now,
            ],
        )?;
        let intent = load_target_reclaim_intent(&tx, custody_id, generation)?
            .ok_or_else(|| DaemonError::Store("target reclaim intent insert was lost".into()))?;
        if intent.allocation_id != allocation_id
            || intent.bucket != bucket
            || intent.slot_name != target_reclaim_slot_name(custody_id, generation)
            || intent.expected_device != expected_device as u64
            || intent.expected_inode != expected_inode as u64
        {
            return Err(DaemonError::Store(
                "target reclaim intent identity conflicts with durable row".into(),
            ));
        }
        tx.commit()?;
        Ok(PrepareTargetReclaimIntentResult::Active(intent))
    }

    pub fn mark_target_reclaim_staged(
        &self,
        intent: &TargetReclaimIntent,
    ) -> Result<TargetReclaimIntent> {
        advance_target_reclaim_intent(
            &self.conn,
            intent,
            TargetReclaimIntentState::Prepared,
            TargetReclaimIntentState::Staged,
        )
    }

    pub fn mark_target_reclaim_deleting(
        &self,
        intent: &TargetReclaimIntent,
    ) -> Result<TargetReclaimIntent> {
        advance_target_reclaim_intent(
            &self.conn,
            intent,
            TargetReclaimIntentState::Staged,
            TargetReclaimIntentState::Deleting,
        )
    }

    pub fn complete_target_reclaim_intent(&self, intent: &TargetReclaimIntent) -> Result<()> {
        settle_target_reclaim_intent(
            &self.conn,
            intent,
            TargetReclaimIntentState::Deleting,
            TargetReclaimIntentTerminalState::Completed,
            "durable_namespace_absent",
        )
    }

    pub fn abandon_target_reclaim_intent(
        &self,
        intent: &TargetReclaimIntent,
        reason: &'static str,
    ) -> Result<()> {
        if reason.is_empty() || reason.len() > 96 || !reason.is_ascii() {
            return Err(DaemonError::InvalidParam(
                "target reclaim abandonment reason is invalid".into(),
            ));
        }
        settle_target_reclaim_intent(
            &self.conn,
            intent,
            TargetReclaimIntentState::Prepared,
            TargetReclaimIntentTerminalState::Abandoned,
            reason,
        )
    }

    pub fn reserve_target_reclaim_intent_page(
        &self,
        limit: u32,
    ) -> Result<TargetReclaimIntentSweepPage> {
        validate_limit(limit)?;
        with_sweep_busy_timeout(&self.conn, || {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let state = load_intent_sweep_state(&tx)?;
            let selected = select_intent_page(&tx, &state, limit, true)?;
            persist_intent_selection(&tx, &selected)?;
            tx.commit()?;
            Ok(selected.page)
        })
    }

    pub fn preview_target_reclaim_intent_page(
        &self,
        limit: u32,
    ) -> Result<TargetReclaimIntentSweepPage> {
        validate_limit(limit)?;
        with_sweep_busy_timeout(&self.conn, || {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            let state = load_intent_sweep_state(&tx)?;
            let page = select_intent_page(&tx, &state, limit, false)?.page;
            tx.commit()?;
            Ok(page)
        })
    }

    /// Read a fair page of gates for the prompt recovery loop. Recovery
    /// revalidates each row before touching the filesystem, so this cursor
    /// is deliberately advisory and does not mutate the durable sweep.
    pub fn prepared_target_reclaim_intent_page(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<Vec<TargetReclaimIntent>> {
        validate_limit(limit)?;
        let read = |after: u64| -> Result<Vec<TargetReclaimIntent>> {
            let mut statement = self.conn.prepare(
                "SELECT schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                        expected_device,expected_inode,state,row_version
                 FROM sandbox_target_reclaim_intents
                 WHERE state='Prepared' AND schedule_id>?1
                 ORDER BY schedule_id ASC LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![
                        sqlite_u64(after, "target reclaim schedule ID")?,
                        i64::from(limit)
                    ],
                    decode_target_reclaim_intent,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Into::into)
        };
        let page = read(after)?;
        if page.is_empty() && after != 0 {
            read(0)
        } else {
            Ok(page)
        }
    }

    pub fn target_reclaim_intent_counts(&self) -> Result<TargetReclaimIntentCounts> {
        self.conn
            .query_row(
                "SELECT
                    (SELECT count(*) FROM sandbox_target_reclaim_intents WHERE state='Prepared'),
                    (SELECT count(*) FROM sandbox_target_reclaim_intents WHERE state='Staged'),
                    (SELECT count(*) FROM sandbox_target_reclaim_intents WHERE state='Deleting'),
                    (SELECT count(*) FROM sandbox_target_reclaim_intent_events
                     WHERE terminal_state='Completed'),
                    (SELECT count(*) FROM sandbox_target_reclaim_intent_events
                     WHERE terminal_state='Abandoned')",
                [],
                |row| {
                    Ok(TargetReclaimIntentCounts {
                        prepared: u32::try_from(row.get::<_, i64>(0)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        staged: u32::try_from(row.get::<_, i64>(1)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        deleting: u32::try_from(row.get::<_, i64>(2)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        completed: u32::try_from(row.get::<_, i64>(3)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        abandoned: u32::try_from(row.get::<_, i64>(4)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                    })
                },
            )
            .map_err(Into::into)
    }
}

fn advance_target_reclaim_intent(
    connection: &Connection,
    intent: &TargetReclaimIntent,
    expected: TargetReclaimIntentState,
    next: TargetReclaimIntentState,
) -> Result<TargetReclaimIntent> {
    if intent.state != expected {
        return Err(DaemonError::Store(
            "target reclaim intent has unexpected source state".into(),
        ));
    }
    let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let transition_column = match next {
        TargetReclaimIntentState::Staged => "staged_at",
        TargetReclaimIntentState::Deleting => "deleting_at",
        TargetReclaimIntentState::Prepared => {
            return Err(DaemonError::Store(
                "target reclaim intent cannot transition backward".into(),
            ));
        }
    };
    let sql = format!(
        "UPDATE sandbox_target_reclaim_intents
         SET state=?4,row_version=row_version+1,
             {transition_column}=CASE WHEN updated_at>?5 THEN updated_at ELSE ?5 END,
             updated_at=CASE WHEN updated_at>?5 THEN updated_at ELSE ?5 END
         WHERE custody_id=?1 AND generation=?2 AND state=?3 AND row_version=?6"
    );
    let changed = tx.execute(
        &sql,
        params![
            intent.custody_id.to_string(),
            sqlite_u64(intent.generation, "target reclaim generation")?,
            expected.as_str(),
            next.as_str(),
            now,
            sqlite_u64(intent.row_version, "target reclaim row version")?,
        ],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "target reclaim intent transition lost row-version fence".into(),
        ));
    }
    let advanced = load_target_reclaim_intent(&tx, intent.custody_id, intent.generation)?
        .ok_or_else(|| DaemonError::Store("target reclaim intent disappeared".into()))?;
    tx.commit()?;
    Ok(advanced)
}

fn settle_target_reclaim_intent(
    connection: &Connection,
    intent: &TargetReclaimIntent,
    expected: TargetReclaimIntentState,
    terminal: TargetReclaimIntentTerminalState,
    reason: &str,
) -> Result<()> {
    if intent.state != expected {
        return Err(DaemonError::Store(
            "target reclaim settlement has unexpected source state".into(),
        ));
    }
    let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    let current = load_target_reclaim_intent(&tx, intent.custody_id, intent.generation)?;
    if current.as_ref() != Some(intent) {
        return Err(DaemonError::Store(
            "target reclaim settlement lost row-version fence".into(),
        ));
    }
    tx.execute(
        "INSERT INTO sandbox_target_reclaim_intent_events(
            schedule_id,custody_id,generation,allocation_id,bucket,slot_name,payload_name,
            expected_device,expected_inode,terminal_state,reason,final_row_version,recorded_at)
         VALUES(?1,?2,?3,?4,?5,?6,'payload',?7,?8,?9,?10,?11,?12)",
        params![
            sqlite_u64(intent.schedule_id, "target reclaim schedule ID")?,
            intent.custody_id.to_string(),
            sqlite_u64(intent.generation, "target reclaim generation")?,
            intent.allocation_id.to_string(),
            i64::from(intent.bucket),
            intent.slot_name,
            i64::try_from(intent.expected_device)
                .map_err(|_| DaemonError::Store("target reclaim device exceeds SQLite".into()))?,
            i64::try_from(intent.expected_inode)
                .map_err(|_| DaemonError::Store("target reclaim inode exceeds SQLite".into()))?,
            terminal.as_str(),
            reason,
            sqlite_u64(intent.row_version, "target reclaim row version")?,
            Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        ],
    )?;
    let changed = tx.execute(
        "DELETE FROM sandbox_target_reclaim_intents
         WHERE schedule_id=?1 AND custody_id=?2 AND generation=?3
           AND state=?4 AND row_version=?5",
        params![
            sqlite_u64(intent.schedule_id, "target reclaim schedule ID")?,
            intent.custody_id.to_string(),
            sqlite_u64(intent.generation, "target reclaim generation")?,
            expected.as_str(),
            sqlite_u64(intent.row_version, "target reclaim row version")?,
        ],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "target reclaim settlement lost row-version fence".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

/// Terminal evidence is keyed by the exact target identity. A later target at
/// the same custody generation (continue + rebuild) has a different device or
/// inode and so has no evidence yet: it gets its own intent (#1035).
fn load_target_reclaim_intent_event(
    connection: &Connection,
    custody_id: Uuid,
    generation: u64,
    expected_device: i64,
    expected_inode: i64,
) -> Result<Option<TargetReclaimIntentEvent>> {
    connection
        .query_row(
            "SELECT schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                    expected_device,expected_inode,terminal_state,recorded_at
             FROM sandbox_target_reclaim_intent_events
             WHERE custody_id=?1 AND generation=?2
               AND expected_device=?3 AND expected_inode=?4",
            params![
                custody_id.to_string(),
                sqlite_u64(generation, "target reclaim generation")?,
                expected_device,
                expected_inode,
            ],
            decode_target_reclaim_intent_event,
        )
        .optional()
        .map_err(Into::into)
}

struct DecodedTargetReclaimIdentity {
    schedule_id: u64,
    custody_id: Uuid,
    generation: u64,
    allocation_id: Uuid,
    bucket: u8,
    slot_name: String,
    expected_device: u64,
    expected_inode: u64,
}

fn decode_target_reclaim_identity(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<DecodedTargetReclaimIdentity> {
    let uuid = |column, value: String| {
        Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    let identity = DecodedTargetReclaimIdentity {
        schedule_id: u64::try_from(row.get::<_, i64>(0)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        custody_id: uuid(1, row.get(1)?)?,
        generation: u64::try_from(row.get::<_, i64>(2)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        allocation_id: uuid(3, row.get(3)?)?,
        bucket: u8::try_from(row.get::<_, i64>(4)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        slot_name: row.get(5)?,
        expected_device: u64::try_from(row.get::<_, i64>(6)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        expected_inode: u64::try_from(row.get::<_, i64>(7)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
    };
    if identity.bucket != target_reclaim_bucket(identity.custody_id, identity.generation)
        || identity.slot_name != target_reclaim_slot_name(identity.custody_id, identity.generation)
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(identity)
}

fn decode_target_reclaim_intent_event(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<TargetReclaimIntentEvent> {
    let active = decode_target_reclaim_identity(row)?;
    let terminal_state = match row.get::<_, String>(8)?.as_str() {
        "Completed" => TargetReclaimIntentTerminalState::Completed,
        "Abandoned" => TargetReclaimIntentTerminalState::Abandoned,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(TargetReclaimIntentEvent {
        schedule_id: active.schedule_id,
        custody_id: active.custody_id,
        generation: active.generation,
        allocation_id: active.allocation_id,
        bucket: active.bucket,
        slot_name: active.slot_name,
        expected_device: active.expected_device,
        expected_inode: active.expected_inode,
        terminal_state,
        recorded_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(9)?)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?
            .with_timezone(&Utc),
    })
}

fn sqlite_u64(value: u64, label: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| DaemonError::Store(format!("{label} exceeds SQLite")))
}

fn load_target_reclaim_intent(
    connection: &Connection,
    custody_id: Uuid,
    generation: u64,
) -> Result<Option<TargetReclaimIntent>> {
    connection
        .query_row(
            "SELECT schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                    expected_device,expected_inode,state,row_version
             FROM sandbox_target_reclaim_intents
             WHERE custody_id=?1 AND generation=?2",
            params![
                custody_id.to_string(),
                sqlite_u64(generation, "target reclaim generation")?
            ],
            decode_target_reclaim_intent,
        )
        .optional()
        .map_err(Into::into)
}

fn decode_target_reclaim_intent(row: &rusqlite::Row<'_>) -> rusqlite::Result<TargetReclaimIntent> {
    let identity = decode_target_reclaim_identity(row)?;
    Ok(TargetReclaimIntent {
        schedule_id: identity.schedule_id,
        custody_id: identity.custody_id,
        generation: identity.generation,
        allocation_id: identity.allocation_id,
        bucket: identity.bucket,
        slot_name: identity.slot_name,
        expected_device: identity.expected_device,
        expected_inode: identity.expected_inode,
        state: TargetReclaimIntentState::parse(&row.get::<_, String>(8)?)?,
        row_version: u64::try_from(row.get::<_, i64>(9)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
    })
}

#[derive(Clone, Copy, Debug)]
struct IntentSweepState {
    cycle: u64,
    after: Option<u64>,
    upper: Option<u64>,
}

struct SelectedIntentPage {
    page: TargetReclaimIntentSweepPage,
    persisted_cycle: u64,
    persisted_after: Option<u64>,
    persisted_upper: Option<u64>,
}

fn load_intent_sweep_state(connection: &Connection) -> Result<IntentSweepState> {
    connection
        .query_row(
            "SELECT cycle,after_schedule_id,upper_schedule_id
             FROM sandbox_target_reclaim_intent_sweeps
             WHERE sweep_id=1 AND schema_version=1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| DaemonError::Store("V119 target reclaim intent sweep row is missing".into()))
        .and_then(|(cycle, after, upper)| {
            Ok(IntentSweepState {
                cycle: u64::try_from(cycle).map_err(|_| {
                    DaemonError::Store("V119 target reclaim intent cycle is negative".into())
                })?,
                after: after
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| DaemonError::Store("invalid intent sweep cursor".into()))?,
                upper: upper
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| DaemonError::Store("invalid intent sweep upper bound".into()))?,
            })
        })
}

fn select_intent_page(
    connection: &Connection,
    state: &IntentSweepState,
    limit: u32,
    reserved: bool,
) -> Result<SelectedIntentPage> {
    let (cycle_after, upper) = match state.upper {
        Some(upper) => (state.cycle, Some(upper)),
        None => (
            state
                .cycle
                .checked_add(1)
                .ok_or_else(|| DaemonError::Store("target reclaim intent cycle overflow".into()))?,
            connection
                .query_row(
                    "SELECT max(schedule_id) FROM sandbox_target_reclaim_intents",
                    [],
                    |row| row.get::<_, Option<i64>>(0),
                )?
                .map(u64::try_from)
                .transpose()
                .map_err(|_| DaemonError::Store("invalid intent sweep upper bound".into()))?,
        ),
    };
    let intents = match upper {
        Some(upper) => select_intents_between(connection, state.after, upper, limit)?,
        None => Vec::new(),
    };
    let cursor_after = intents
        .last()
        .map(|intent| intent.schedule_id)
        .or(state.after);
    let has_more = match (cursor_after, upper) {
        (Some(after), Some(upper)) => connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sandbox_target_reclaim_intents
                           WHERE schedule_id>?1 AND schedule_id<=?2)",
            params![
                sqlite_u64(after, "target reclaim schedule ID")?,
                sqlite_u64(upper, "target reclaim schedule ID")?
            ],
            |row| row.get::<_, bool>(0),
        )?,
        _ => false,
    };
    let completing_active_cycle = state.upper.is_some() && intents.is_empty();
    let persisted_after = (!completing_active_cycle).then_some(cursor_after).flatten();
    let persisted_upper = (!completing_active_cycle).then_some(upper).flatten();
    let wrapped = completing_active_cycle || (state.upper.is_none() && upper.is_none());
    let digest = intent_page_digest(&intents);

    Ok(SelectedIntentPage {
        page: TargetReclaimIntentSweepPage {
            intents,
            has_more,
            evidence: TargetReclaimIntentSweepEvidence {
                cycle_before: state.cycle,
                cycle_after: if reserved { cycle_after } else { state.cycle },
                cursor_before: state.after,
                cursor_after: if reserved {
                    persisted_after
                } else {
                    state.after
                },
                upper_bound: upper,
                page_key_digest: digest,
                reserved,
                wrapped: reserved && wrapped,
            },
        },
        persisted_cycle: cycle_after,
        persisted_after,
        persisted_upper,
    })
}

fn select_intents_between(
    connection: &Connection,
    after: Option<u64>,
    upper: u64,
    limit: u32,
) -> Result<Vec<TargetReclaimIntent>> {
    let mut statement = connection.prepare(
        "SELECT schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                expected_device,expected_inode,state,row_version
         FROM sandbox_target_reclaim_intents
         WHERE schedule_id>?1 AND schedule_id<=?2
         ORDER BY schedule_id ASC LIMIT ?3",
    )?;
    statement
        .query_map(
            params![
                sqlite_u64(after.unwrap_or(0), "target reclaim schedule ID")?,
                sqlite_u64(upper, "target reclaim schedule ID")?,
                i64::from(limit),
            ],
            decode_target_reclaim_intent,
        )?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn persist_intent_selection(tx: &Transaction<'_>, selected: &SelectedIntentPage) -> Result<()> {
    let changed = tx.execute(
        "UPDATE sandbox_target_reclaim_intent_sweeps
         SET cycle=?1,after_schedule_id=?2,upper_schedule_id=?3,
             updated_at=CASE WHEN updated_at>?4 THEN updated_at ELSE ?4 END
         WHERE sweep_id=1 AND schema_version=1",
        params![
            sqlite_u64(selected.persisted_cycle, "target reclaim intent cycle")?,
            selected
                .persisted_after
                .map(|value| sqlite_u64(value, "target reclaim schedule ID"))
                .transpose()?,
            selected
                .persisted_upper
                .map(|value| sqlite_u64(value, "target reclaim schedule ID"))
                .transpose()?,
            Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        ],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "V119 target reclaim intent sweep reservation lost singleton row".into(),
        ));
    }
    Ok(())
}

fn intent_page_digest(intents: &[TargetReclaimIntent]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rsi.target-reclaim-intent-page.v1\0");
    for intent in intents {
        digest.update(intent.schedule_id.to_be_bytes());
        digest.update(intent.custody_id.as_bytes());
        digest.update(intent.generation.to_be_bytes());
        digest.update(intent.row_version.to_be_bytes());
    }
    format!("sha256:{:x}", digest.finalize())
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerminalReclaimSweepKey {
    pub updated_at: String,
    pub session_id: Uuid,
}

impl From<&TerminalCustodyReclaimCandidate> for TerminalReclaimSweepKey {
    fn from(candidate: &TerminalCustodyReclaimCandidate) -> Self {
        Self {
            updated_at: candidate.updated_at.clone(),
            session_id: candidate.session_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalReclaimSweepEvidence {
    pub cycle_before: u64,
    pub cycle_after: u64,
    pub cursor_before: Option<TerminalReclaimSweepKey>,
    pub cursor_after: Option<TerminalReclaimSweepKey>,
    pub upper_bound: Option<TerminalReclaimSweepKey>,
    pub page_key_digest: String,
    pub inspected_terminal_rows: u32,
    pub custody_lookups: u32,
    pub reserved: bool,
    pub wrapped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalReclaimSweepPage {
    pub candidates: Vec<TerminalCustodyReclaimCandidate>,
    /// Rows the page inspected but withheld because a live consumer (an
    /// enabled wake, a restart intent, a manager seat, a running job) still
    /// needs the owner's `target/` (#1564). They spent page budget like any
    /// other row, so the report names them instead of dropping them silently.
    pub fenced: Vec<TerminalFencedCandidate>,
    /// Rows whose `target/` is already gone, passed over without spending page
    /// budget (#1607). Zero unless the page was selected with
    /// `skip_absent_targets`; counted as `TargetAbsent` in the report.
    pub absent_skipped: u32,
    pub has_more: bool,
    pub evidence: TerminalReclaimSweepEvidence,
}

/// A candidate withheld by [`TerminalReclaimSweepPage::fenced`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalFencedCandidate {
    pub candidate: TerminalCustodyReclaimCandidate,
    pub consumer: &'static str,
}

#[derive(Clone, Debug)]
struct SweepState {
    cycle: u64,
    after: Option<TerminalReclaimSweepKey>,
    upper: Option<TerminalReclaimSweepKey>,
}

// One internal runtime record, independent of the fair walk. Reserving recent
// keys advances it even when the filesystem later refuses them (TargetAbsent,
// for example). A new fair cycle retries the newest rows, including rebuilt
// targets; no per-custody history or unbounded refusal set is needed.
const RECENT_SWEEP_STATE_KEY: &str = "target_reclaim_recent_cursor";

#[derive(serde::Serialize, serde::Deserialize)]
struct RecentSweepState {
    cycle: u64,
    after: TerminalReclaimSweepKey,
    /// The newest eligible terminal row the descent had seen when it last
    /// started from the top. A row newer than this finished after the descent
    /// began and sits above `after`, so the descent restarts from the top
    /// instead of leaving it unseen until the next fair cycle.
    #[serde(default)]
    head: Option<TerminalReclaimSweepKey>,
}

/// SQL ordering of sweep keys: `updated_at` text, then the lowercase id.
fn sweep_key_newer(candidate: &TerminalReclaimSweepKey, than: &TerminalReclaimSweepKey) -> bool {
    (
        candidate.updated_at.as_str(),
        candidate.session_id.to_string(),
    ) > (than.updated_at.as_str(), than.session_id.to_string())
}

impl Store {
    /// Atomically reserves one bounded page before any filesystem effect.
    /// Every inspected terminal key consumes its finite-sweep position even if
    /// custody filtering omits it. A crash can lose only this bounded raw page
    /// until the next cycle.
    pub fn reserve_terminal_reclaim_page(&self, limit: u32) -> Result<TerminalReclaimSweepPage> {
        self.reserve_terminal_reclaim_page_with(limit, false)
    }

    /// Like [`Self::reserve_terminal_reclaim_page`]. With `recent_first` (a
    /// pressure pass), the newest terminal rows with unreclaimed custody are
    /// placed ahead of the walk's candidates, including rows inside the
    /// frozen cycle window. The walk cursor, cycle and upper bound are
    /// unaffected. Reserved recent keys yield their pressure slots until the
    /// next fair cycle, regardless of reclaim outcome. Live consumers are
    /// excluded from both lanes.
    pub fn reserve_terminal_reclaim_page_with(
        &self,
        limit: u32,
        recent_first: bool,
    ) -> Result<TerminalReclaimSweepPage> {
        self.reserve_terminal_reclaim_page_filtered(limit, recent_first, false)
    }

    /// Like [`Self::reserve_terminal_reclaim_page_with`]. With
    /// `skip_absent_targets` (a pressure pass, #1607) a row whose
    /// `<sandbox_root>/target` no longer exists is walked past without spending
    /// any of the `limit` budget: a finished sandbox whose target was already
    /// reclaimed has nothing to free, and ~1,500 of them ahead of the real
    /// targets used every pass's budget. Present targets keep their order
    /// (newest first in the recent lane, then the fair walk). Absent rows still
    /// advance both cursors and are counted in `absent_skipped`.
    pub fn reserve_terminal_reclaim_page_filtered(
        &self,
        limit: u32,
        recent_first: bool,
        skip_absent_targets: bool,
    ) -> Result<TerminalReclaimSweepPage> {
        validate_limit(limit)?;
        with_sweep_busy_timeout(&self.conn, || {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let state = load_state(&tx)?;
            let selected = select_page(
                &tx,
                self,
                &state,
                limit,
                true,
                recent_first,
                skip_absent_targets,
            )?;
            persist_selection(&tx, &selected)?;
            tx.commit()?;
            Ok(selected.page)
        })
    }

    /// Computes the exact next page under one read transaction and performs no
    /// cursor write. Repeated previews therefore return identical evidence
    /// until terminal source rows or durable sweep state change.
    pub fn preview_terminal_reclaim_page(&self, limit: u32) -> Result<TerminalReclaimSweepPage> {
        self.preview_terminal_reclaim_page_with(limit, false)
    }

    pub fn preview_terminal_reclaim_page_with(
        &self,
        limit: u32,
        recent_first: bool,
    ) -> Result<TerminalReclaimSweepPage> {
        self.preview_terminal_reclaim_page_filtered(limit, recent_first, false)
    }

    pub fn preview_terminal_reclaim_page_filtered(
        &self,
        limit: u32,
        recent_first: bool,
        skip_absent_targets: bool,
    ) -> Result<TerminalReclaimSweepPage> {
        validate_limit(limit)?;
        with_sweep_busy_timeout(&self.conn, || {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            let state = load_state(&tx)?;
            let page = select_page(
                &tx,
                self,
                &state,
                limit,
                false,
                recent_first,
                skip_absent_targets,
            )?
            .page;
            tx.commit()?;
            Ok(page)
        })
    }
}

fn with_sweep_busy_timeout<T>(
    connection: &Connection,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let previous_millis =
        connection.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, u64>(0))?;
    connection.busy_timeout(SWEEP_BUSY_TIMEOUT)?;
    let result = operation();
    let restore = connection.busy_timeout(Duration::from_millis(previous_millis));
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
    }
}

fn validate_limit(limit: u32) -> Result<()> {
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(DaemonError::InvalidParam(format!(
            "target reclaim page limit must be within 1..={MAX_PAGE_SIZE}"
        )));
    }
    Ok(())
}

fn load_state(connection: &Connection) -> Result<SweepState> {
    connection
        .query_row(
            "SELECT cycle,after_updated_at,after_session_id,upper_updated_at,upper_session_id
             FROM sandbox_reclaim_sweeps WHERE sweep_id=1 AND schema_version=1",
            [],
            |row| {
                let cycle = row.get::<_, i64>(0)?;
                let after = parse_key(row.get(1)?, row.get(2)?, 1)?;
                let upper = parse_key(row.get(3)?, row.get(4)?, 3)?;
                Ok((cycle, after, upper))
            },
        )
        .optional()?
        .ok_or_else(|| DaemonError::Store("V119 target reclaim sweep row is missing".into()))
        .and_then(|(cycle, after, upper)| {
            let cycle = u64::try_from(cycle)
                .map_err(|_| DaemonError::Store("V119 target reclaim cycle is negative".into()))?;
            Ok(SweepState {
                cycle,
                after,
                upper,
            })
        })
}

fn parse_key(
    updated_at: Option<String>,
    session_id: Option<String>,
    column: usize,
) -> rusqlite::Result<Option<TerminalReclaimSweepKey>> {
    match (updated_at, session_id) {
        (None, None) => Ok(None),
        (Some(updated_at), Some(session_id)) => Ok(Some(TerminalReclaimSweepKey {
            updated_at,
            session_id: Uuid::parse_str(&session_id).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    column,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        })),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

struct SelectedPage {
    page: TerminalReclaimSweepPage,
    persisted_cycle: u64,
    persisted_after: Option<TerminalReclaimSweepKey>,
    persisted_upper: Option<TerminalReclaimSweepKey>,
    recent_state: Option<RecentSweepState>,
}

fn select_page(
    connection: &Connection,
    store: &Store,
    state: &SweepState,
    limit: u32,
    reserved: bool,
    recent_first: bool,
    skip_absent: bool,
) -> Result<SelectedPage> {
    let (cycle_after, upper) = match &state.upper {
        Some(upper) => (state.cycle, Some(upper.clone())),
        None => (
            state
                .cycle
                .checked_add(1)
                .ok_or_else(|| DaemonError::Store("target reclaim cycle overflow".into()))?,
            select_upper_bound(connection)?,
        ),
    };

    // Pressure pass: the newest terminal rows that still own an unreclaimed
    // live custody go ahead of the fair walk, wherever the walk cursor is.
    // They spend part of the page budget, so a pass never inspects more than
    // `limit` rows, and they never move the fair cursor, cycle or upper bound.
    // Their own durable descending cursor prevents refused keys from spending
    // the same slots on every pass. It resets with the next fair cycle.
    let mut absent_sessions = std::collections::HashSet::new();
    let mut recent_head = None;
    let (recent_keys, recent_last, recent_scanned) = if recent_first {
        let recent_state = store
            .get_daemon_setting(RECENT_SWEEP_STATE_KEY)?
            .map(|raw| serde_json::from_str::<RecentSweepState>(&raw))
            .transpose()
            .map_err(|error| DaemonError::Store(error.to_string()))?
            .filter(|recent| recent.cycle == cycle_after);
        // A worker that finished after the descent passed its position is
        // newer than every key the cursor will ever reach. Restart from the
        // top when the newest eligible row is newer than the recorded head, so
        // pressure reclaim sees a fresh finisher on its next pass (#1737).
        let newest = select_recent_terminal_keys(connection, None, 1)?
            .into_iter()
            .next();
        let arrivals = match (&newest, recent_state.as_ref().and_then(|r| r.head.as_ref())) {
            (Some(newest), Some(head)) => sweep_key_newer(newest, head),
            (Some(_), None) => true,
            (None, _) => false,
        };
        recent_head = newest;
        let after = recent_state
            .as_ref()
            .filter(|_| !arrivals)
            .map(|recent| &recent.after);
        let want = (limit / 2).min(RECENT_FIRST_LIMIT);
        if skip_absent {
            let scan = select_recent_present_keys(connection, after, want)?;
            absent_sessions.extend(scan.absent.iter().copied());
            (scan.accepted, scan.last_scanned, scan.scanned)
        } else {
            let keys = select_recent_terminal_keys(connection, after, want)?;
            let last = keys.last().cloned();
            let scanned = keys.len() as u32;
            (keys, last, scanned)
        }
    } else {
        (Vec::new(), None, 0)
    };
    let walk_limit = limit - recent_keys.len() as u32;
    let (terminal_keys, walk_last, walk_scanned) = match &upper {
        Some(upper) if skip_absent => {
            let scan =
                select_present_terminal_keys(connection, state.after.as_ref(), upper, walk_limit)?;
            absent_sessions.extend(scan.absent.iter().copied());
            (scan.accepted, scan.last_scanned, scan.scanned)
        }
        Some(upper) => {
            let keys = select_terminal_keys(connection, state.after.as_ref(), upper, walk_limit)?;
            let last = keys.last().cloned();
            let scanned = keys.len() as u32;
            (keys, last, scanned)
        }
        None => (Vec::new(), None, 0),
    };
    let mut seen = std::collections::HashSet::new();
    let selected = recent_keys
        .iter()
        .chain(terminal_keys.iter())
        .filter(|key| seen.insert(key.session_id))
        .map(|key| select_candidate_for_key(connection, key))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .map(|candidate| {
            // Use the same store snapshot and consumer fence as preparation.
            // Every raw key still consumes its fair-walk position, so a wake
            // or detached job cannot stall the cursor. Preparation checks
            // again if a consumer arrives after this page was reserved.
            store
                .target_reclaim_live_consumer(candidate.custody_id, candidate.generation)
                .map(|consumer| (candidate, consumer))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut candidates = Vec::with_capacity(selected.len());
    let mut fenced = Vec::new();
    for (candidate, consumer) in selected {
        match consumer {
            None => candidates.push(candidate),
            Some(consumer) => fenced.push(TerminalFencedCandidate {
                candidate,
                consumer,
            }),
        }
    }
    let has_more = walk_last
        .as_ref()
        .is_some_and(|last| key_less(last, upper.as_ref().unwrap()));
    let cursor_after = walk_last.clone().or_else(|| state.after.clone());
    let completing_active_cycle = state.upper.is_some() && walk_scanned == 0;
    let persisted_after = if completing_active_cycle {
        None
    } else {
        cursor_after.clone()
    };
    let persisted_upper = if completing_active_cycle {
        None
    } else {
        upper.clone()
    };
    let wrapped = completing_active_cycle || (state.upper.is_none() && upper.is_none());

    Ok(SelectedPage {
        page: TerminalReclaimSweepPage {
            evidence: TerminalReclaimSweepEvidence {
                cycle_before: state.cycle,
                cycle_after: if reserved { cycle_after } else { state.cycle },
                cursor_before: state.after.clone(),
                cursor_after: if reserved {
                    persisted_after.clone()
                } else {
                    state.after.clone()
                },
                upper_bound: upper,
                page_key_digest: page_key_digest(
                    &terminal_keys
                        .iter()
                        .chain(recent_keys.iter())
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                inspected_terminal_rows: walk_scanned + recent_scanned,
                custody_lookups: walk_scanned + recent_scanned,
                reserved,
                wrapped: reserved && wrapped,
            },
            candidates,
            fenced,
            absent_skipped: absent_sessions.len() as u32,
            has_more,
        },
        persisted_cycle: cycle_after,
        persisted_after,
        persisted_upper,
        recent_state: recent_last.map(|after| RecentSweepState {
            cycle: cycle_after,
            after,
            head: recent_head,
        }),
    })
}

const SELECT_UPPER_BOUND_SQL: &str = "SELECT updated_at,id FROM sessions
     WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted')
     ORDER BY updated_at DESC,id DESC LIMIT 1";

fn select_upper_bound(connection: &Connection) -> Result<Option<TerminalReclaimSweepKey>> {
    connection
        .query_row(SELECT_UPPER_BOUND_SQL, [], |row| {
            parse_key(Some(row.get(0)?), Some(row.get(1)?), 1).map(Option::unwrap)
        })
        .optional()
        .map_err(Into::into)
}

const SELECT_TERMINAL_KEYS_FROM_START_SQL: &str = "SELECT updated_at,id FROM sessions
     WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted')
       AND (updated_at,id)<=(?1,?2)
     ORDER BY updated_at ASC,id ASC LIMIT ?3";

const SELECT_TERMINAL_KEYS_AFTER_SQL: &str = "SELECT updated_at,id FROM sessions
     WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted')
       AND (updated_at,id)>(?1,?2)
       AND (updated_at,id)<=(?3,?4)
     ORDER BY updated_at ASC,id ASC LIMIT ?5";

fn select_terminal_keys(
    connection: &Connection,
    after: Option<&TerminalReclaimSweepKey>,
    upper: &TerminalReclaimSweepKey,
    limit: u32,
) -> Result<Vec<TerminalReclaimSweepKey>> {
    let map = |row: &rusqlite::Row<'_>| {
        parse_key(Some(row.get(0)?), Some(row.get(1)?), 1).map(Option::unwrap)
    };
    let mut statement;
    let rows = match after {
        Some(after) => {
            statement = connection.prepare(SELECT_TERMINAL_KEYS_AFTER_SQL)?;
            statement.query_map(
                params![
                    after.updated_at,
                    after.session_id.to_string(),
                    upper.updated_at,
                    upper.session_id.to_string(),
                    i64::from(limit)
                ],
                map,
            )?
        }
        None => {
            statement = connection.prepare(SELECT_TERMINAL_KEYS_FROM_START_SQL)?;
            statement.query_map(
                params![
                    upper.updated_at,
                    upper.session_id.to_string(),
                    i64::from(limit)
                ],
                map,
            )?
        }
    };
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Newest-first rows a pressure pass examines ahead of the fair walk.
const RECENT_FIRST_LIMIT: u32 = 256;

/// Terminal sessions whose live, verified custody has no recorded target
/// reclaim yet, newest first. A finished worker's `target/` is the capacity
/// that matters under pressure, and the walk (oldest first) reaches it last.
/// Recorded reclaims drop out through the events table. The descending cursor
/// rotates past reserved rows without events, too; the fair walk still visits
/// them and the next cycle retries them in the recent lane.
const SELECT_RECENT_TERMINAL_KEYS_SQL: &str =
    "SELECT s.updated_at,s.id,r.sandbox_root FROM sessions s
     JOIN sandbox_custody_roots r
       ON r.owner_session_id=s.id AND s.sandbox_custody_id=r.custody_id
     WHERE s.status IN ('Completed','Failed','Interrupted','Archived','Deleted')
       AND r.state='live' AND r.validation_state='verified'
       AND r.validated_generation=r.generation
       AND r.reserved_effects=0 AND r.active_effects=0
       AND NOT EXISTS(SELECT 1 FROM sandbox_target_reclaim_intent_events e
                      WHERE e.custody_id=r.custody_id AND e.generation=r.generation)
       AND (?2 IS NULL OR (s.updated_at,s.id)<(?2,?3))
     ORDER BY s.updated_at DESC,s.id DESC LIMIT ?1";

fn select_recent_terminal_keys(
    connection: &Connection,
    after: Option<&TerminalReclaimSweepKey>,
    limit: u32,
) -> Result<Vec<TerminalReclaimSweepKey>> {
    connection
        .prepare(SELECT_RECENT_TERMINAL_KEYS_SQL)?
        .query_map(
            params![
                i64::from(limit),
                after.map(|key| key.updated_at.as_str()),
                after.map(|key| key.session_id.to_string()),
            ],
            |row| parse_key(Some(row.get(0)?), Some(row.get(1)?), 1).map(Option::unwrap),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Rows examined per statement and per lane while skipping absent targets.
const ABSENT_SCAN_CHUNK: u32 = 256;
const ABSENT_SCAN_CAP: u32 = 4096;

struct PresentScan {
    accepted: Vec<TerminalReclaimSweepKey>,
    /// The last row examined, accepted or not: the cursor moves past it.
    last_scanned: Option<TerminalReclaimSweepKey>,
    scanned: u32,
    /// Sessions whose target was absent; a row both lanes see counts once.
    absent: Vec<Uuid>,
}

/// `<sandbox_root>/target` for the live custody row owned by `key`'s session;
/// `None` when the row has no such custody (it was never a candidate).
fn sandbox_root_of(connection: &Connection, key: &TerminalReclaimSweepKey) -> Option<String> {
    connection
        .query_row(
            "SELECT r.sandbox_root FROM sessions s
             JOIN sandbox_custody_roots r
               ON r.owner_session_id=s.id AND s.sandbox_custody_id=r.custody_id
             WHERE s.id=?1 AND s.updated_at=?2 AND r.state='live'",
            params![key.session_id.to_string(), key.updated_at],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
}

fn target_present(sandbox_root: &str) -> bool {
    std::fs::symlink_metadata(Path::new(sandbox_root).join("target")).is_ok()
}

/// Oldest-first walk keys, skipping rows whose target is gone, until `want`
/// present rows were found or [`ABSENT_SCAN_CAP`] rows were examined.
fn select_present_terminal_keys(
    connection: &Connection,
    after: Option<&TerminalReclaimSweepKey>,
    upper: &TerminalReclaimSweepKey,
    want: u32,
) -> Result<PresentScan> {
    let mut scan = PresentScan {
        accepted: Vec::new(),
        last_scanned: None,
        scanned: 0,
        absent: Vec::new(),
    };
    let mut cursor = after.cloned();
    'chunks: while scan.scanned < ABSENT_SCAN_CAP && (scan.accepted.len() as u32) < want {
        let chunk = select_terminal_keys(connection, cursor.as_ref(), upper, ABSENT_SCAN_CHUNK)?;
        if chunk.is_empty() {
            break;
        }
        for key in chunk {
            scan.scanned += 1;
            scan.last_scanned = Some(key.clone());
            cursor = Some(key.clone());
            match sandbox_root_of(connection, &key) {
                Some(root) if target_present(&root) => scan.accepted.push(key),
                Some(_) => scan.absent.push(key.session_id),
                None => {}
            }
            if scan.accepted.len() as u32 >= want {
                break 'chunks;
            }
        }
    }
    Ok(scan)
}

/// Newest-first recent lane, skipping rows whose target is gone.
fn select_recent_present_keys(
    connection: &Connection,
    after: Option<&TerminalReclaimSweepKey>,
    want: u32,
) -> Result<PresentScan> {
    let mut scan = PresentScan {
        accepted: Vec::new(),
        last_scanned: None,
        scanned: 0,
        absent: Vec::new(),
    };
    let mut cursor = after.cloned();
    'chunks: while scan.scanned < ABSENT_SCAN_CAP && (scan.accepted.len() as u32) < want {
        let chunk = connection
            .prepare(SELECT_RECENT_TERMINAL_KEYS_SQL)?
            .query_map(
                params![
                    i64::from(ABSENT_SCAN_CHUNK),
                    cursor.as_ref().map(|key| key.updated_at.as_str()),
                    cursor.as_ref().map(|key| key.session_id.to_string()),
                ],
                |row| {
                    let key =
                        parse_key(Some(row.get(0)?), Some(row.get(1)?), 1).map(Option::unwrap)?;
                    Ok((key, row.get::<_, String>(2)?))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if chunk.is_empty() {
            break;
        }
        for (key, root) in chunk {
            scan.scanned += 1;
            scan.last_scanned = Some(key.clone());
            cursor = Some(key.clone());
            if target_present(&root) {
                scan.accepted.push(key);
            } else {
                scan.absent.push(key.session_id);
            }
            if scan.accepted.len() as u32 >= want {
                break 'chunks;
            }
        }
    }
    Ok(scan)
}

const SELECT_CANDIDATE_FOR_KEY_SQL: &str = "SELECT r.custody_id,r.generation,s.id,s.updated_at
     FROM sessions s
     JOIN sandbox_custody_roots r
       ON r.owner_session_id=s.id AND s.sandbox_custody_id=r.custody_id
     WHERE s.id=?1 AND s.updated_at=?2
       AND r.state='live' AND r.validation_state='verified'
       AND r.validated_generation=r.generation
       AND r.reserved_effects=0 AND r.active_effects=0";

fn select_candidate_for_key(
    connection: &Connection,
    key: &TerminalReclaimSweepKey,
) -> Result<Option<TerminalCustodyReclaimCandidate>> {
    connection
        .query_row(
            SELECT_CANDIDATE_FOR_KEY_SQL,
            params![key.session_id.to_string(), key.updated_at],
            |row| {
                let custody_id = row.get::<_, String>(0)?;
                let session_id = row.get::<_, String>(2)?;
                Ok(TerminalCustodyReclaimCandidate {
                    custody_id: Uuid::parse_str(&custody_id).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    generation: u64::try_from(row.get::<_, i64>(1)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Integer,
                            Box::new(error),
                        )
                    })?,
                    session_id: Uuid::parse_str(&session_id).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    updated_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn key_less(left: &TerminalReclaimSweepKey, right: &TerminalReclaimSweepKey) -> bool {
    (left.updated_at.as_str(), left.session_id) < (right.updated_at.as_str(), right.session_id)
}

fn persist_selection(tx: &Transaction<'_>, selected: &SelectedPage) -> Result<()> {
    let changed = tx.execute(
        "UPDATE sandbox_reclaim_sweeps
         SET cycle=?1,after_updated_at=?2,after_session_id=?3,
             upper_updated_at=?4,upper_session_id=?5,
             updated_at=CASE WHEN updated_at>?6 THEN updated_at ELSE ?6 END
         WHERE sweep_id=1 AND schema_version=1",
        params![
            i64::try_from(selected.persisted_cycle)
                .map_err(|_| DaemonError::Store("target reclaim cycle exceeds SQLite".into()))?,
            selected
                .persisted_after
                .as_ref()
                .map(|key| key.updated_at.as_str()),
            selected
                .persisted_after
                .as_ref()
                .map(|key| key.session_id.to_string()),
            selected
                .persisted_upper
                .as_ref()
                .map(|key| key.updated_at.as_str()),
            selected
                .persisted_upper
                .as_ref()
                .map(|key| key.session_id.to_string()),
            Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        ],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "V119 target reclaim sweep reservation lost singleton row".into(),
        ));
    }
    if let Some(recent) = &selected.recent_state {
        let value =
            serde_json::to_string(recent).map_err(|error| DaemonError::Store(error.to_string()))?;
        tx.execute(
            "INSERT INTO daemon_settings(key,value,updated_at) VALUES(?1,?2,?3)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
            params![
                RECENT_SWEEP_STATE_KEY,
                value,
                Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
            ],
        )?;
    }
    Ok(())
}

fn page_key_digest(keys: &[TerminalReclaimSweepKey]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rsi.target-reclaim-page.v1\0");
    for key in keys {
        for value in [key.updated_at.as_bytes(), key.session_id.as_bytes()] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value);
        }
    }
    format!("sha256:{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        capacity_recovery,
        sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding},
        tests::{make_test_session, rewind_store_to_schema_version},
    };
    use rsi_common::types::{
        SandboxCleanupState, SandboxCustodyErrorCodeV1, SandboxCustodyTransitionV1, SandboxKind,
        SessionStatus,
    };
    use std::path::{Path, PathBuf};

    struct FixtureDirectory(PathBuf);

    impl FixtureDirectory {
        fn create(label: &str) -> Self {
            let base = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target"));
            let path = base
                .join("rsid-test-fixtures")
                .join(format!("{label}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create disk-backed V119 fixture");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).expect("remove isolated V119 fixture");
        }
    }

    fn fingerprint(connection: &Connection) -> String {
        let tx = connection.unchecked_transaction().unwrap();
        let result = capacity_recovery::v88_full_catalog_fingerprint(&tx).unwrap();
        tx.commit().unwrap();
        result
    }

    fn seed_candidate(store: &mut Store, fixture: &Path, ordinal: u32) -> Uuid {
        let mut session = make_test_session();
        session.id = Uuid::from_u128(u128::from(ordinal) + 1);
        session.status = SessionStatus::Completed;
        session.created_at = chrono::DateTime::parse_from_rfc3339(&format!(
            "2026-09-13T12:{:02}:00.000000000Z",
            ordinal
        ))
        .unwrap()
        .with_timezone(&chrono::Utc);
        session.updated_at = session.created_at;
        session.working_dir = fixture.join("repository");
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(fixture.join(format!("sandbox-{ordinal}")));
        session.sandbox_branch = Some(format!("rsi/v119-{ordinal}"));
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        let custody_id = Uuid::from_u128(u128::from(ordinal) + 10_000);
        store
            .insert_session_with_custody(
                &session,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id,
                    canonical_repo_dir: session.working_dir.to_string_lossy().into_owned(),
                    sandbox_root: session
                        .sandbox_root
                        .as_ref()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    sandbox_branch: session.sandbox_branch.clone().unwrap(),
                    repository_identity: "repo:v119-fixture".into(),
                    source_commit: "a".repeat(40),
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .unwrap();
        session.id
    }

    fn seed_terminal_without_custody(store: &Store, fixture: &Path, ordinal: u32) -> Uuid {
        let mut session = make_test_session();
        session.id = Uuid::from_u128(u128::from(ordinal) + 100_000);
        session.status = SessionStatus::Completed;
        session.created_at = chrono::DateTime::parse_from_rfc3339("2026-09-13T11:00:00.000000000Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(i64::from(ordinal));
        session.updated_at = session.created_at;
        session.working_dir = fixture.join("repository");
        store.insert_session(&session).unwrap();
        session.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_epoch_keys_round_trip_and_keep_epoch_zero_raw() {
        assert_eq!(store_target_device(0x1234, 0), Some(0x1234));
        assert_eq!(split_store_target_device(0x1234), (0x1234, 0));
        for epoch in [1, 2, 0xABCDE, TARGET_EPOCH_MAX] {
            let key = store_target_device(0x1234, epoch).unwrap();
            assert_ne!(key, 0x1234);
            assert!(i64::try_from(key).is_ok(), "the key fits SQLite");
            assert_eq!(split_store_target_device(key), (0x1234, epoch));
        }
        // A device that cannot carry an epoch is refused, never aliased.
        assert_eq!(store_target_device(1 << 32, 1), None);
        assert_eq!(store_target_device(0x1234, TARGET_EPOCH_MAX + 1), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn terminal_evidence_is_superseded_only_by_a_newer_or_unknown_age_target() {
        let recorded_at = Utc::now();
        let event = |terminal_state| TargetReclaimIntentEvent {
            schedule_id: 1,
            custody_id: Uuid::new_v4(),
            generation: 1,
            allocation_id: Uuid::new_v4(),
            bucket: 0,
            slot_name: String::new(),
            expected_device: 1,
            expected_inode: 1,
            terminal_state,
            recorded_at,
        };
        let completed = event(TargetReclaimIntentTerminalState::Completed);
        let abandoned = event(TargetReclaimIntentTerminalState::Abandoned);
        let later = recorded_at + chrono::Duration::seconds(1);
        let earlier = recorded_at - chrono::Duration::seconds(1);
        // Born after the evidence: a rebuilt target that reused the inode.
        assert!(terminal_evidence_is_for_earlier_target(
            &completed,
            Some(later)
        ));
        assert!(terminal_evidence_is_for_earlier_target(
            &abandoned,
            Some(later)
        ));
        // Born before the evidence: an exact replay of the same target.
        assert!(!terminal_evidence_is_for_earlier_target(
            &completed,
            Some(earlier)
        ));
        assert!(!terminal_evidence_is_for_earlier_target(
            &abandoned,
            Some(earlier)
        ));
        // No birth time: a completed reclaim removed its source, so fail toward
        // reclaiming; abandoned evidence keeps its target and stays refused.
        assert!(terminal_evidence_is_for_earlier_target(&completed, None));
        assert!(!terminal_evidence_is_for_earlier_target(&abandoned, None));
        // Epochs are stable per birth time, distinct from raw, and advance.
        let first = next_target_epoch(0, Some(later));
        assert!((1..=TARGET_EPOCH_MAX).contains(&first));
        assert_eq!(first, next_target_epoch(0, Some(later)));
        assert_eq!(next_target_epoch(0, None), 1);
        assert_eq!(next_target_epoch(first, Some(later)), first + 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn terminal_custody_reclaim_candidates_feed_durable_sweep_and_survive_reopen() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-reopen");
        let database = fixture.path().join("reclaim.db");
        let mut store = Store::open(&database).unwrap();
        let ids = (0..5)
            .map(|ordinal| seed_candidate(&mut store, fixture.path(), ordinal))
            .collect::<Vec<_>>();

        let preview = store.preview_terminal_reclaim_page(2).unwrap();
        assert!(!preview.evidence.reserved);
        assert_eq!(preview.evidence.cycle_before, 0);
        assert_eq!(preview.evidence.cycle_after, 0);
        assert_eq!(
            preview.evidence.cursor_before,
            preview.evidence.cursor_after
        );
        assert!(!preview.evidence.wrapped);
        assert_eq!(
            preview
                .candidates
                .iter()
                .map(|candidate| candidate.session_id)
                .collect::<Vec<_>>(),
            ids[..2]
        );
        assert_eq!(store.preview_terminal_reclaim_page(2).unwrap(), preview);

        let reserved = store.reserve_terminal_reclaim_page(2).unwrap();
        assert!(reserved.evidence.reserved);
        assert_eq!(reserved.candidates, preview.candidates);
        drop(store);

        let reopened = Store::open(&database).unwrap();
        let second = reopened.reserve_terminal_reclaim_page(2).unwrap();
        assert_eq!(second.evidence.cycle_before, 1);
        assert_eq!(
            second
                .candidates
                .iter()
                .map(|candidate| candidate.session_id)
                .collect::<Vec<_>>(),
            ids[2..4]
        );
        let third = reopened.reserve_terminal_reclaim_page(2).unwrap();
        assert_eq!(third.candidates[0].session_id, ids[4]);
        assert!(!third.has_more);
        let wrap = reopened.reserve_terminal_reclaim_page(2).unwrap();
        assert!(wrap.candidates.is_empty());
        assert!(wrap.evidence.wrapped);
        let next_cycle = reopened.reserve_terminal_reclaim_page(2).unwrap();
        assert_eq!(next_cycle.evidence.cycle_before, 1);
        assert_eq!(next_cycle.evidence.cycle_after, 2);
        assert_eq!(next_cycle.candidates[0].session_id, ids[0]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pressure_page_takes_terminal_rows_newer_than_the_frozen_upper_bound_first() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-recent-first");
        let mut store = Store::open_in_memory().unwrap();
        let ids = (0..4)
            .map(|ordinal| seed_candidate(&mut store, fixture.path(), ordinal))
            .collect::<Vec<_>>();
        // A long cycle is in progress: upper bound frozen at ids[3].
        let first = store.reserve_terminal_reclaim_page(1).unwrap();
        assert_eq!(first.candidates[0].session_id, ids[0]);
        // A session becomes terminal after the cycle started.
        let late = seed_candidate(&mut store, fixture.path(), 5);

        // Without pressure the fair walk defers it until the cycle wraps.
        let preview = store.preview_terminal_reclaim_page(1).unwrap();
        assert_eq!(preview.candidates[0].session_id, ids[1]);
        assert!(preview.candidates.iter().all(|c| c.session_id != late));

        // Under pressure the late session is examined first; the walk
        // cursor, cycle and upper bound are untouched by the extra rows.
        let pressure_preview = store.preview_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(pressure_preview.candidates[0].session_id, late);
        assert_eq!(pressure_preview.candidates[1].session_id, ids[1]);
        let pressure = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(pressure.candidates[0].session_id, late);
        assert_eq!(pressure.candidates[1].session_id, ids[1]);
        assert_eq!(pressure.evidence.upper_bound, first.evidence.upper_bound);
        assert_eq!(pressure.evidence.cycle_after, first.evidence.cycle_after);
        assert_eq!(
            pressure
                .evidence
                .cursor_after
                .as_ref()
                .map(|k| k.session_id),
            Some(ids[1])
        );
    }

    /// #1607: ~1,500 finished sandboxes whose `target/` was already reclaimed
    /// sat ahead of the real targets and used every pass's whole budget.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pressure_page_skips_absent_targets_without_spending_budget() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-absent-skip");
        let mut store = Store::open_in_memory().unwrap();
        let ids = (0..40)
            .map(|ordinal| seed_candidate(&mut store, fixture.path(), ordinal))
            .collect::<Vec<_>>();
        // Only the oldest and one middle row still own a target.
        let present = [0usize, 20];
        for ordinal in present {
            std::fs::create_dir_all(fixture.path().join(format!("sandbox-{ordinal}/target")))
                .unwrap();
        }

        // Unfiltered: the page of 4 is spent on absent rows (the defect).
        let preview = store.preview_terminal_reclaim_page_with(4, true).unwrap();
        assert_eq!(preview.absent_skipped, 0);

        let page = store
            .reserve_terminal_reclaim_page_filtered(4, true, true)
            .unwrap();
        let reached = page
            .candidates
            .iter()
            .map(|candidate| candidate.session_id)
            .collect::<Vec<_>>();
        assert_eq!(
            reached,
            vec![ids[20], ids[0]],
            "present targets are tried, newest first, in the first pass"
        );
        assert_eq!(page.absent_skipped, 38);
        // Every row was walked past, so the next pass does not revisit them.
        let again = store
            .reserve_terminal_reclaim_page_filtered(4, true, true)
            .unwrap();
        assert!(again.candidates.is_empty(), "{again:?}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn unfiltered_pages_are_unchanged_by_absent_targets() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-absent-unfiltered");
        let mut store = Store::open_in_memory().unwrap();
        let ids = (0..6)
            .map(|ordinal| seed_candidate(&mut store, fixture.path(), ordinal))
            .collect::<Vec<_>>();
        let page = store.reserve_terminal_reclaim_page_with(3, false).unwrap();
        assert_eq!(page.absent_skipped, 0);
        assert_eq!(
            page.candidates
                .iter()
                .map(|candidate| candidate.session_id)
                .collect::<Vec<_>>(),
            vec![ids[0], ids[1], ids[2]]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pressure_page_reaches_newest_rows_inside_the_frozen_window_without_moving_the_walk() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-recent-inside");
        let mut store = Store::open_in_memory().unwrap();
        let ids = (0..6)
            .map(|ordinal| seed_candidate(&mut store, fixture.path(), ordinal))
            .collect::<Vec<_>>();
        // The walk starts at the oldest row; the newest rows sit at the end of
        // the frozen window, one full cycle away.
        let first = store.reserve_terminal_reclaim_page(1).unwrap();
        assert_eq!(first.candidates[0].session_id, ids[0]);

        let pressure = store.reserve_terminal_reclaim_page_with(4, true).unwrap();
        let reached = pressure
            .candidates
            .iter()
            .map(|candidate| candidate.session_id)
            .collect::<Vec<_>>();
        // Two newest rows lead; the walk continues from its own cursor.
        assert_eq!(reached, vec![ids[5], ids[4], ids[1], ids[2]]);
        assert_eq!(
            pressure
                .evidence
                .cursor_after
                .as_ref()
                .map(|k| k.session_id),
            Some(ids[2])
        );
        assert_eq!(pressure.evidence.upper_bound, first.evidence.upper_bound);

        // A row reached by both sources is attempted once.
        let overlap = store.reserve_terminal_reclaim_page_with(8, true).unwrap();
        let mut seen = overlap
            .candidates
            .iter()
            .map(|candidate| candidate.session_id)
            .collect::<Vec<_>>();
        let total = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), total);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pressure_page_keeps_walking_when_recent_rows_have_no_reclaim_event() {
        let fixture = FixtureDirectory::create("target-reclaim-recent-refusals");
        let mut store = Store::open_in_memory().unwrap();
        let ids = (0..6)
            .map(|ordinal| seed_candidate(&mut store, fixture.path(), ordinal))
            .collect::<Vec<_>>();
        // Refusals leave no intent/event. The recent lane rotates while the
        // fair walk retains at least half the page and finishes its cycle.
        for (ordinal, owner) in ids.iter().enumerate() {
            let page = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
            assert_eq!(page.candidates[0].session_id, ids[5 - ordinal]);
            assert_eq!(
                page.evidence.cursor_after.as_ref().unwrap().session_id,
                *owner
            );
            assert!(
                page.candidates
                    .iter()
                    .any(|candidate| candidate.session_id == *owner)
            );
            assert!(page.evidence.inspected_terminal_rows <= 2);
        }
        let wrapped = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        assert!(wrapped.evidence.wrapped);
    }

    /// #1737: the recent lane's descending cursor sat below a worker that
    /// finished after the descent began, so a fresh finisher stayed unseen
    /// under disk pressure until the next fair cycle. It is examined first.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pressure_page_sees_a_worker_that_finished_after_the_recent_cursor_passed() {
        let fixture = FixtureDirectory::create("target-reclaim-recent-arrival");
        let mut store = Store::open_in_memory().unwrap();
        let ids = (0..6)
            .map(|ordinal| seed_candidate(&mut store, fixture.path(), ordinal))
            .collect::<Vec<_>>();
        let first = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(first.candidates[0].session_id, ids[5]);
        let second = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(second.candidates[0].session_id, ids[4]);

        let late = seed_candidate(&mut store, fixture.path(), 7);
        let preview = store.preview_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(preview.candidates[0].session_id, late);
        let third = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(third.candidates[0].session_id, late);
        // The descent resumes below the arrival rather than looping on it.
        let fourth = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        assert_ne!(fourth.candidates[0].session_id, late);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pressure_page_advances_past_absent_targets_durably_and_retries_next_cycle() {
        let fixture = FixtureDirectory::create("target-reclaim-absent-rotation");
        let database = fixture.path().join("rotation.db");
        let mut store = Store::open(&database).unwrap();
        let ids = (0..6)
            .map(|ordinal| seed_candidate(&mut store, fixture.path(), ordinal))
            .collect::<Vec<_>>();
        let absent_target = fixture.path().join("sandbox-5/target");
        std::fs::create_dir_all(absent_target.parent().unwrap()).unwrap();
        assert_eq!(
            std::fs::metadata(&absent_target).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        std::fs::create_dir_all(fixture.path().join("sandbox-4/target")).unwrap();

        let first = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(first.candidates[0].session_id, ids[5]);
        // TargetAbsent creates no reclaim intent/event. Closing the store
        // models a restart after that refusal, before another pressure pass.
        drop(store);
        let store = Store::open(&database).unwrap();
        let preview = store.preview_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(preview.candidates[0].session_id, ids[4]);
        assert_eq!(
            store.preview_terminal_reclaim_page_with(2, true).unwrap(),
            preview
        );
        let next = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(next.candidates, preview.candidates);
        assert_eq!(next.evidence.upper_bound, first.evidence.upper_bound);
        assert_eq!(
            next.evidence.cursor_after.as_ref().unwrap().session_id,
            ids[1]
        );

        for _ in 0..4 {
            store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        }
        assert!(
            store
                .reserve_terminal_reclaim_page_with(2, true)
                .unwrap()
                .evidence
                .wrapped
        );
        // A rebuilt target remains eligible in the next recent cycle without
        // changing custody generation or manufacturing a deletion event.
        std::fs::create_dir_all(&absent_target).unwrap();
        let retry = store.reserve_terminal_reclaim_page_with(2, true).unwrap();
        assert_eq!(retry.candidates[0].session_id, ids[5]);
        assert_eq!(retry.evidence.cycle_after, first.evidence.cycle_after + 1);
        assert_eq!(store.target_reclaim_intent_counts().unwrap().completed, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_sweep_upper_bound_defers_new_rows_and_forward_guards_reject_rewind() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-upper");
        let mut store = Store::open_in_memory().unwrap();
        seed_candidate(&mut store, fixture.path(), 0);
        seed_candidate(&mut store, fixture.path(), 1);
        let first = store.reserve_terminal_reclaim_page(1).unwrap();
        seed_candidate(&mut store, fixture.path(), 2);
        let second = store.reserve_terminal_reclaim_page(2).unwrap();
        assert_eq!(second.candidates.len(), 1);
        assert_eq!(second.evidence.upper_bound, first.evidence.upper_bound);

        assert!(
            store
                .conn
                .execute(
                    "UPDATE sandbox_reclaim_sweeps SET cycle=0 WHERE sweep_id=1",
                    []
                )
                .is_err()
        );
        assert!(
            store
                .conn
                .execute(
                    "UPDATE sandbox_reclaim_sweeps SET upper_session_id=?1 WHERE sweep_id=1",
                    [Uuid::new_v4().to_string()],
                )
                .is_err()
        );
        assert!(
            store
                .conn
                .execute("DELETE FROM sandbox_reclaim_sweeps WHERE sweep_id=1", [])
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_sweep_bounds_raw_rows_and_custody_lookups_before_later_candidate() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-raw-bound");
        let mut store = Store::open_in_memory().unwrap();
        for ordinal in 0..40 {
            seed_terminal_without_custody(&store, fixture.path(), ordinal);
        }
        let eligible = seed_candidate(&mut store, fixture.path(), 50);

        for _ in 0..5 {
            let page = store.reserve_terminal_reclaim_page(8).unwrap();
            assert!(page.candidates.is_empty());
            assert_eq!(page.evidence.inspected_terminal_rows, 8);
            assert_eq!(page.evidence.custody_lookups, 8);
            assert!(page.has_more);
        }
        let later = store.reserve_terminal_reclaim_page(8).unwrap();
        assert_eq!(later.evidence.inspected_terminal_rows, 1);
        assert_eq!(later.evidence.custody_lookups, 1);
        assert_eq!(later.candidates.len(), 1);
        assert_eq!(later.candidates[0].session_id, eligible);
        assert!(!later.has_more);
    }

    fn seed_intent(store: &mut Store, fixture: &Path, ordinal: u32) -> TargetReclaimIntent {
        seed_candidate(store, fixture, ordinal);
        store
            .prepare_target_reclaim_intent(
                Uuid::from_u128(u128::from(ordinal) + 10_000),
                1,
                1,
                u64::from(ordinal) + 1,
            )
            .unwrap()
            .active()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn prepared_recovery_page_wraps_past_stuck_gate_and_skips_staged_rows() {
        let fixture = FixtureDirectory::create("prepared-recovery-page");
        let mut store = Store::open_in_memory().unwrap();
        let first = seed_intent(&mut store, fixture.path(), 0);
        let staged = seed_intent(&mut store, fixture.path(), 1);
        store.mark_target_reclaim_staged(&staged).unwrap();
        let third = seed_intent(&mut store, fixture.path(), 2);

        let first_page = store.prepared_target_reclaim_intent_page(0, 1).unwrap();
        assert_eq!(first_page, vec![first.clone()]);
        let second_page = store
            .prepared_target_reclaim_intent_page(first.schedule_id, 1)
            .unwrap();
        assert_eq!(second_page, vec![third.clone()]);
        let wrapped = store
            .prepared_target_reclaim_intent_page(third.schedule_id, 1)
            .unwrap();
        assert_eq!(wrapped, vec![first]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn prepared_reclaim_blocks_effect_and_tombstone_then_staged_allows_effect() {
        let fixture = FixtureDirectory::create("prepared-reclaim-gate");
        let mut store = Store::open_in_memory().unwrap();
        let prepared = seed_intent(&mut store, fixture.path(), 0);
        let boot = Uuid::new_v4();

        let effect = store
            .reserve_effect(prepared.custody_id, prepared.generation, boot)
            .unwrap_err();
        assert!(effect.to_string().contains("reclaim_prepared"));
        let tombstone = store
            .tombstone_custody_root(
                prepared.custody_id,
                prepared.generation,
                CustodyCause::Purge,
            )
            .unwrap_err();
        assert!(tombstone.to_string().contains("reclaim_prepared"));

        let staged = store.mark_target_reclaim_staged(&prepared).unwrap();
        let reservation = store
            .reserve_effect(staged.custody_id, staged.generation, boot)
            .unwrap();
        store.settle_effect(reservation, false).unwrap();
        let deleting = store.mark_target_reclaim_deleting(&staged).unwrap();
        let reservation = store
            .reserve_effect(deleting.custody_id, deleting.generation, boot)
            .unwrap();
        store.settle_effect(reservation, false).unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_retains_an_idle_appointed_manager() {
        let fixture = FixtureDirectory::create("target-manager-seat");
        let mut store = Store::open_in_memory().unwrap();
        let owner = seed_candidate(&mut store, fixture.path(), 0);
        let (config, _) = crate::store::manager_coordinator::tests::fixture(
            &store,
            rsi_common::harness_manager_v2::ManagerPolicyV2::default(),
        );
        // Appoint the terminal custody owner through the real operator path.
        store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?2 WHERE id=?1",
                params![owner.to_string(), config.project_id.to_string()],
            )
            .unwrap();
        store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: config.project_id,
                    session_id: owner,
                    group_ids: vec![],
                    epic_ids: config.selected_epic_ids.clone(),
                    expected_row_version: config.row_version,
                },
            )
            .unwrap();
        assert!(
            store
                .target_reclaim_has_live_consumer(Uuid::from_u128(10_000), 1)
                .unwrap()
        );
        assert!(
            store
                .reclaim_terminal_target_locked(Uuid::from_u128(10_000), 1)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .prepare_target_reclaim_intent(Uuid::from_u128(10_000), 1, 1, 1)
                .is_err()
        );
    }

    fn assert_continuation_protects_target(store: &Store, custody: Uuid) {
        for pressure in [false, true] {
            let page = store
                .preview_terminal_reclaim_page_with(4, pressure)
                .unwrap();
            assert!(
                page.candidates.is_empty(),
                "continuation must retain target: {page:?}"
            );
        }
        assert!(
            store
                .reclaim_terminal_target_locked(custody, 1)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .prepare_target_reclaim_intent(custody, 1, 1, 1)
                .is_err()
        );
        assert!(store.target_reclaim_intent(custody, 1).unwrap().is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_continuation_wakes_fence_both_lanes_and_preparation() {
        use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
        use rsi_common::wake_predicate::{WakePredicate, WakeWhenState};
        let fixture = FixtureDirectory::create("target-continuation-wakes");
        for (mode, predicate) in [
            (WakeMode::Resume, false),
            (WakeMode::Resume, true),
            (WakeMode::OnTerminal(Uuid::new_v4()), false),
        ] {
            let mut store = Store::open_in_memory().unwrap();
            let owner = seed_candidate(&mut store, fixture.path(), 0);
            let custody = Uuid::from_u128(10_000);
            let now = Utc::now();
            let job = ScheduledJob {
                id: Uuid::new_v4(),
                name: "continuation".into(),
                message: "continue after work settles".into(),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: now,
                },
                last_fired_at: None,
                next_fire_at: now,
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: now,
                updated_at: now,
                wake_mode: mode,
                wake_session_id: Some(owner),
            };
            if predicate {
                store
                    .insert_wake_when(
                        &job,
                        &WakeWhenState {
                            predicate: WakePredicate {
                                jobs_terminal: None,
                                sha_on_rolling: Some("a".repeat(40)),
                            },
                            armed_at: now,
                            deadline: None,
                            repo_dir: Some(fixture.path().display().to_string()),
                        },
                        false,
                    )
                    .unwrap();
            } else {
                store.insert_scheduled_job(&job).unwrap();
            }
            assert_continuation_protects_target(&store, custody);
            // Cancelling the continuation restores eligibility in both lanes.
            store
                .cancel_owned_scheduled_jobs(owner, Some(job.id), None)
                .unwrap();
            for pressure in [false, true] {
                let page = store
                    .preview_terminal_reclaim_page_with(4, pressure)
                    .unwrap();
                assert_eq!(page.candidates.len(), 1);
                assert_eq!(page.candidates[0].session_id, owner);
            }
            assert!(
                store
                    .reclaim_terminal_target_locked(custody, 1)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_pending_and_claimed_restart_intents_retain_target() {
        let fixture = FixtureDirectory::create("target-restart-intent");
        let mut store = Store::open_in_memory().unwrap();
        let owner = seed_candidate(&mut store, fixture.path(), 0);
        let custody = Uuid::from_u128(10_000);
        let invocation = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
             (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
              trigger_source,session_id,created_at)
             VALUES(?1,'session.launch','session','foreground','paid','admitted','running',
                    'launch_session',?2,?3)",
                params![
                    invocation.to_string(),
                    owner.to_string(),
                    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        store
            .set_session_model_invocation(owner, Some(invocation))
            .unwrap();
        store
            .update_session_status(owner, SessionStatus::Running)
            .unwrap();
        let boot = Uuid::new_v4();
        assert!(store.record_restart_intent(owner, boot).unwrap());
        store.mark_restart_interrupt_sent(owner, boot).unwrap();
        store
            .update_session_status(owner, SessionStatus::Interrupted)
            .unwrap();
        assert_continuation_protects_target(&store, custody);
        let intent = store.next_restart_intent(None).unwrap().unwrap();
        assert!(store.claim_restart_intent(&intent, Uuid::new_v4()).unwrap());
        assert_continuation_protects_target(&store, custody);
        store
            .conn
            .execute(
                "UPDATE daemon_restart_intents SET state='failed' WHERE id=?1",
                [intent.id.to_string()],
            )
            .unwrap();
        let page = store.preview_terminal_reclaim_page_with(4, true).unwrap();
        assert_eq!(page.candidates.len(), 1);
        assert_eq!(page.candidates[0].session_id, owner);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_live_job_fences_selection_and_preparation() {
        use crate::store::agent_jobs::NewAgentJob;
        use rsi_common::agent_jobs::{BuildCommand, BuildJobParams, JobParams, JobWake};
        let fixture = FixtureDirectory::create("target-live-job");
        let mut store = Store::open_in_memory().unwrap();
        let owner = seed_candidate(&mut store, fixture.path(), 0);
        let custody = Uuid::from_u128(10_000);
        assert!(
            store
                .reclaim_terminal_target_locked(custody, 1)
                .unwrap()
                .is_some()
        );
        let id = Uuid::new_v4();
        store
            .insert_agent_job(
                &NewAgentJob {
                    id,
                    owner_session_id: owner,
                    project_id: None,
                    name: None,
                    params: JobParams::Build(BuildJobParams {
                        command: BuildCommand::Check,
                        package: None,
                        workspace: false,
                        all_targets: false,
                        release: false,
                    }),
                    // The job belongs to the owner even if it builds from a gate clone.
                    cwd: fixture.path().join("gate").display().to_string(),
                    unit_name: format!("rsi-test-{id}"),
                    log_path: "/tmp/job.log".into(),
                    status_path: "/tmp/job.status".into(),
                    idempotency_key: None,
                    wake: JobWake::None,
                },
                Utc::now(),
            )
            .unwrap();
        assert_continuation_protects_target(&store, custody);
        assert!(
            store
                .reclaim_terminal_target_locked(custody, 1)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .prepare_target_reclaim_intent(custody, 1, 1, 1)
                .is_err()
        );
        assert!(store.target_reclaim_intent(custody, 1).unwrap().is_none());
        // Reverse writer order: Prepared wins, so a new job cannot enter.
        let next_owner = seed_candidate(&mut store, fixture.path(), 1);
        let next_custody = Uuid::from_u128(10_001);
        store
            .prepare_target_reclaim_intent(next_custody, 1, 1, 2)
            .unwrap();
        let row = store.get_agent_job(id).unwrap().unwrap();
        let next_id = Uuid::new_v4();
        let mut next_job = NewAgentJob {
            id: next_id,
            owner_session_id: next_owner,
            project_id: None,
            name: None,
            params: row.job.params.clone(),
            cwd: fixture.path().join("sandbox-1").display().to_string(),
            unit_name: format!("rsi-test-{next_id}"),
            log_path: "/tmp/job2.log".into(),
            status_path: "/tmp/job2.status".into(),
            idempotency_key: None,
            wake: JobWake::None,
        };
        assert!(
            store
                .insert_agent_job(&next_job, Utc::now())
                .unwrap_err()
                .to_string()
                .contains("sandbox_reclaim_prepared")
        );
        // The other session's job is protected by cwd as well as owner id.
        next_job.owner_session_id = owner;
        assert!(store.insert_agent_job(&next_job, Utc::now()).is_err());
        assert!(store.get_agent_job(next_id).unwrap().is_none());
        let prepared = store
            .target_reclaim_intent(next_custody, 1)
            .unwrap()
            .unwrap();
        store.mark_target_reclaim_staged(&prepared).unwrap();
        assert!(store.insert_agent_job(&next_job, Utc::now()).is_ok());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_routes_friction_and_manager_notice() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = crate::store::manager_coordinator::tests::fixture(
            &store,
            rsi_common::harness_manager_v2::ManagerPolicyV2::default(),
        );
        let custody = Uuid::new_v4();
        let job = store
            .record_target_reclaim_notice(lead.id, custody, 3, 4096, false)
            .unwrap()
            .unwrap();
        let state: String = store.conn.query_row(
            "SELECT state_json FROM harness_manager_notices WHERE job_id=?1 AND source_session_id=?2",
            params![job.to_string(), lead.id.to_string()], |row| row.get(0)).unwrap();
        let state: serde_json::Value = serde_json::from_str(&state).unwrap();
        assert_eq!(state["record_kind"], "sandbox_target_reclaim");
        assert_eq!(state["generation"], 3);
        assert_eq!(state["bytes"], 4096);
        assert_eq!(state["outcome"], "removed");
        let signature: String = store
            .conn
            .query_row(
                "SELECT signature FROM friction_events WHERE session_id=?1 AND project_id=?2",
                params![lead.id.to_string(), config.project_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(signature, "sandbox_target_reclaim:removed");
        assert_eq!(store.manager_notice_undelivered_count(job).unwrap(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn unbound_starting_successor_prevents_prepared_reclaim() {
        let fixture = FixtureDirectory::create("successor-before-reclaim");
        let mut store = Store::open_in_memory().unwrap();
        let owner = seed_candidate(&mut store, fixture.path(), 0);
        let mut child = make_test_session();
        child.id = Uuid::new_v4();
        child.status = SessionStatus::Starting;
        child.continued_from = Some(owner);
        child.working_dir = fixture.path().join("repository");
        child.sandbox_kind = Some(SandboxKind::GitWorktree);
        child.sandbox_root = Some(fixture.path().join("sandbox-0"));
        child.sandbox_branch = Some("rsi/v119-0".into());
        child.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store.insert_session(&child).unwrap();

        assert_eq!(
            store
                .prepare_target_reclaim_intent(Uuid::from_u128(10_000), 1, 1, 1)
                .unwrap(),
            PrepareTargetReclaimIntentResult::SuccessorReservationPending
        );
        assert!(
            store
                .target_reclaim_intent(Uuid::from_u128(10_000), 1)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn prepared_reclaim_refuses_retry_and_rotation_before_successor_insert() {
        use super::super::daemon_settings::{
            AutofileCause, C5TransitionError, c5_autofile_pending_key,
        };

        let fixture = FixtureDirectory::create("prepared-successor-reservations");
        let mut store = Store::open_in_memory().unwrap();
        let owner = seed_candidate(&mut store, fixture.path(), 0);
        store
            .update_failed_and_stage_c5_autofile(owner, AutofileCause::NonZeroExit)
            .unwrap();
        let marker_key = c5_autofile_pending_key(owner);
        let marker = store.get_c5_autofile_pending(&marker_key).unwrap().unwrap();
        let prepared = store
            .prepare_target_reclaim_intent(Uuid::from_u128(10_000), 1, 1, 1)
            .unwrap()
            .active();
        let mut child = make_test_session();
        child.id = Uuid::new_v4();
        child.status = SessionStatus::Starting;
        child.continued_from = Some(owner);
        child.working_dir = fixture.path().join("repository");
        child.sandbox_kind = Some(SandboxKind::GitWorktree);
        child.sandbox_root = Some(fixture.path().join("sandbox-0"));
        child.sandbox_branch = Some("rsi/v119-0".into());
        child.sandbox_cleanup_state = Some(SandboxCleanupState::Live);

        let retry = store
            .admit_c5_retry_successor(owner, &child, 0, 2, Uuid::new_v4(), &marker)
            .unwrap_err();
        assert!(
            matches!(retry, C5TransitionError::ReclaimPrepared { session_id } if session_id == owner)
        );
        assert_eq!(
            store.get_session(owner).unwrap().unwrap().status,
            SessionStatus::Failed
        );
        assert!(store.get_session(child.id).unwrap().is_none());
        assert_eq!(
            store.get_c5_autofile_pending(&marker_key).unwrap(),
            Some(marker)
        );

        let rotation = store
            .insert_reserved_rotation_session_with_invocation(
                &child,
                Uuid::new_v4(),
                "test-rotation",
            )
            .unwrap_err();
        assert!(rotation.to_string().contains("reclaim_prepared"));
        assert!(store.get_session(child.id).unwrap().is_none());

        let staged = store.mark_target_reclaim_staged(&prepared).unwrap();
        let tx = rusqlite::Transaction::new_unchecked(
            &store.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .unwrap();
        assert!(
            !super::super::sandbox_custody::prepared_reclaim_for_successor_on(
                &tx,
                owner,
                child.sandbox_root.as_ref().unwrap(),
                child.sandbox_branch.as_deref().unwrap(),
            )
            .unwrap()
        );
        drop(tx);
        assert_eq!(staged.state, TargetReclaimIntentState::Staged);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_intent_scheduler_crosses_two_pages_after_reopen_and_defers_new_tail() {
        let fixture = FixtureDirectory::create("target-reclaim-intent-pages");
        let database = fixture.path().join("intents.db");
        let mut store = Store::open(&database).unwrap();
        let expected = (0..7)
            .map(|ordinal| seed_intent(&mut store, fixture.path(), ordinal).schedule_id)
            .collect::<Vec<_>>();

        let first = store.reserve_target_reclaim_intent_page(2).unwrap();
        assert_eq!(
            first
                .intents
                .iter()
                .map(|intent| intent.schedule_id)
                .collect::<Vec<_>>(),
            expected[..2]
        );
        let frozen_upper = first.evidence.upper_bound;
        drop(store);

        let mut reopened = Store::open(&database).unwrap();
        let inserted_late = seed_intent(&mut reopened, fixture.path(), 8).schedule_id;
        for chunk in expected[2..].chunks(2) {
            let page = reopened.reserve_target_reclaim_intent_page(2).unwrap();
            assert_eq!(page.evidence.upper_bound, frozen_upper);
            assert_eq!(
                page.intents
                    .iter()
                    .map(|intent| intent.schedule_id)
                    .collect::<Vec<_>>(),
                chunk
            );
        }
        let wrap = reopened.reserve_target_reclaim_intent_page(2).unwrap();
        assert!(wrap.intents.is_empty());
        assert!(wrap.evidence.wrapped);
        let next = reopened.reserve_target_reclaim_intent_page(8).unwrap();
        assert!(
            next.intents
                .iter()
                .any(|intent| intent.schedule_id == inserted_late)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_intent_survives_fail_closed_custody_lifecycle_transition() {
        let fixture = FixtureDirectory::create("target-reclaim-intent-lifecycle");
        let mut store = Store::open_in_memory().unwrap();
        let intent = seed_intent(&mut store, fixture.path(), 0);

        store
            .record_failed_revalidation(
                intent.custody_id,
                intent.generation,
                SandboxCustodyErrorCodeV1::RootIdentityMismatch,
                SandboxCustodyTransitionV1::EffectRevalidation,
            )
            .unwrap();

        assert_eq!(
            store
                .target_reclaim_intent(intent.custody_id, intent.generation)
                .unwrap(),
            Some(intent.clone())
        );
        assert_eq!(store.prepared_target_reclaim_owner(&intent).unwrap(), None);
        let page = store.reserve_target_reclaim_intent_page(1).unwrap();
        assert_eq!(page.intents, vec![intent]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_intent_fsm_retains_terminal_evidence_and_cas_fences() {
        let fixture = FixtureDirectory::create("target-reclaim-intent-fsm");
        let mut store = Store::open_in_memory().unwrap();
        let prepared = seed_intent(&mut store, fixture.path(), 0);
        assert!(
            store
                .conn
                .execute(
                    "INSERT INTO sandbox_target_reclaim_intent_events(
                        schedule_id,custody_id,generation,allocation_id,bucket,slot_name,
                        payload_name,expected_device,expected_inode,terminal_state,reason,
                        final_row_version,recorded_at)
                     VALUES(?1,?2,?3,?4,?5,?6,'payload',?7,?8,'Abandoned','forged',?9,?10)",
                    params![
                        prepared.schedule_id as i64,
                        prepared.custody_id.to_string(),
                        prepared.generation as i64,
                        prepared.allocation_id.to_string(),
                        i64::from(prepared.bucket),
                        prepared.slot_name.as_str(),
                        prepared.expected_device as i64,
                        prepared.expected_inode.saturating_add(1) as i64,
                        prepared.row_version as i64,
                        Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                    ],
                )
                .is_err()
        );
        let stale = prepared.clone();
        let staged = store.mark_target_reclaim_staged(&prepared).unwrap();
        assert!(store.mark_target_reclaim_staged(&stale).is_err());
        let deleting = store.mark_target_reclaim_deleting(&staged).unwrap();
        store.complete_target_reclaim_intent(&deleting).unwrap();
        assert!(
            store
                .target_reclaim_intent(deleting.custody_id, deleting.generation)
                .unwrap()
                .is_none()
        );

        let abandoned = seed_intent(&mut store, fixture.path(), 1);
        let abandoned_identity = abandoned.clone();
        store
            .abandon_target_reclaim_intent(&abandoned, "custody_revalidation_drift")
            .unwrap();
        let revisited = store
            .prepare_target_reclaim_intent(
                abandoned_identity.custody_id,
                abandoned_identity.generation,
                abandoned_identity.expected_device,
                abandoned_identity.expected_inode,
            )
            .unwrap();
        let PrepareTargetReclaimIntentResult::Terminal(event) = revisited else {
            panic!("terminal candidate revisit recreated an active intent");
        };
        assert_eq!(event.schedule_id, abandoned_identity.schedule_id);
        assert_eq!(event.custody_id, abandoned_identity.custody_id);
        assert_eq!(event.generation, abandoned_identity.generation);
        assert_eq!(event.allocation_id, abandoned_identity.allocation_id);
        assert_eq!(event.bucket, abandoned_identity.bucket);
        assert_eq!(event.slot_name, abandoned_identity.slot_name);
        assert_eq!(event.expected_device, abandoned_identity.expected_device);
        assert_eq!(event.expected_inode, abandoned_identity.expected_inode);
        assert_eq!(
            event.terminal_state,
            TargetReclaimIntentTerminalState::Abandoned
        );
        assert!(
            store
                .target_reclaim_intent(abandoned_identity.custody_id, abandoned_identity.generation)
                .unwrap()
                .is_none()
        );
        let counts = store.target_reclaim_intent_counts().unwrap();
        assert_eq!(counts.completed, 1);
        assert_eq!(counts.abandoned, 1);
        assert!(
            store
                .conn
                .execute(
                    "UPDATE sandbox_target_reclaim_intent_events SET reason='changed'",
                    [],
                )
                .is_err()
        );
        assert!(
            store
                .conn
                .execute("DELETE FROM sandbox_target_reclaim_intent_events", [])
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn terminal_evidence_is_keyed_by_exact_target_identity() {
        let fixture = FixtureDirectory::create("target-reclaim-identity-evidence");
        let mut store = Store::open_in_memory().unwrap();
        let first = seed_intent(&mut store, fixture.path(), 0);
        let staged = store.mark_target_reclaim_staged(&first).unwrap();
        let deleting = store.mark_target_reclaim_deleting(&staged).unwrap();
        store.complete_target_reclaim_intent(&deleting).unwrap();

        // An exact replay of the same identity is settled by the retained event.
        let replay = store
            .prepare_target_reclaim_intent(
                first.custody_id,
                first.generation,
                first.expected_device,
                first.expected_inode,
            )
            .unwrap();
        let PrepareTargetReclaimIntentResult::Terminal(event) = replay else {
            panic!("exact replay must be settled by retained terminal evidence");
        };
        assert_eq!(event.expected_inode, first.expected_inode);
        assert_eq!(
            event.terminal_state,
            TargetReclaimIntentTerminalState::Completed
        );

        // A rebuilt target at the same generation has a new inode: it gets its
        // own intent and its own terminal evidence.
        let rebuilt_inode = first.expected_inode + 500;
        let second = store
            .prepare_target_reclaim_intent(
                first.custody_id,
                first.generation,
                first.expected_device,
                rebuilt_inode,
            )
            .unwrap()
            .active();
        assert_eq!(second.expected_inode, rebuilt_inode);
        assert_ne!(second.schedule_id, first.schedule_id);
        let staged = store.mark_target_reclaim_staged(&second).unwrap();
        let deleting = store.mark_target_reclaim_deleting(&staged).unwrap();
        store.complete_target_reclaim_intent(&deleting).unwrap();
        assert_eq!(store.target_reclaim_intent_counts().unwrap().completed, 2);

        for inode in [first.expected_inode, rebuilt_inode] {
            let PrepareTargetReclaimIntentResult::Terminal(event) = store
                .prepare_target_reclaim_intent(
                    first.custody_id,
                    first.generation,
                    first.expected_device,
                    inode,
                )
                .unwrap()
            else {
                panic!("each identity keeps its own terminal evidence");
            };
            assert_eq!(event.expected_inode, inode);
        }
        assert_eq!(store.target_reclaim_intent_counts().unwrap().completed, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn identity_evidence_migration_preserves_events_and_relaxes_uniqueness() {
        let fixture = FixtureDirectory::create("target-reclaim-identity-migration");
        let mut store = Store::open_in_memory().unwrap();
        let first = seed_intent(&mut store, fixture.path(), 0);
        let staged = store.mark_target_reclaim_staged(&first).unwrap();
        let deleting = store.mark_target_reclaim_deleting(&staged).unwrap();
        store.complete_target_reclaim_intent(&deleting).unwrap();

        crate::store::tests::rewind_post_v121_tail_to(
            &store.conn,
            TARGET_RECLAIM_IDENTITY_SCHEMA_VERSION - 1,
        );
        assert_eq!(store.target_reclaim_intent_counts().unwrap().completed, 1);
        apply_identity_events_migration(&store, TARGET_RECLAIM_IDENTITY_SCHEMA_VERSION).unwrap();
        assert_eq!(store.target_reclaim_intent_counts().unwrap().completed, 1);
        let PrepareTargetReclaimIntentResult::Terminal(event) = store
            .prepare_target_reclaim_intent(
                first.custody_id,
                first.generation,
                first.expected_device,
                first.expected_inode,
            )
            .unwrap()
        else {
            panic!("the migrated event still settles its exact identity");
        };
        assert_eq!(event.schedule_id, first.schedule_id);
        let second = store
            .prepare_target_reclaim_intent(
                first.custody_id,
                first.generation,
                first.expected_device,
                first.expected_inode + 1,
            )
            .unwrap()
            .active();
        let staged = store.mark_target_reclaim_staged(&second).unwrap();
        let deleting = store.mark_target_reclaim_deleting(&staged).unwrap();
        store.complete_target_reclaim_intent(&deleting).unwrap();
        assert_eq!(store.target_reclaim_intent_counts().unwrap().completed, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_sweep_clamps_future_observation_and_rejects_non_rfc3339_cursor() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-clock");
        let mut store = Store::open_in_memory().unwrap();
        seed_candidate(&mut store, fixture.path(), 0);
        seed_candidate(&mut store, fixture.path(), 1);
        seed_candidate(&mut store, fixture.path(), 2);
        let first = store.reserve_terminal_reclaim_page(1).unwrap();
        let second_key = TerminalReclaimSweepKey {
            updated_at: "2026-09-13T12:01:00+00:00".into(),
            session_id: Uuid::from_u128(2),
        };
        let future = "2999-01-01T00:00:00.000000000Z";
        store
            .conn
            .execute(
                "UPDATE sandbox_reclaim_sweeps
                 SET after_updated_at=?1,after_session_id=?2,updated_at=?3
                 WHERE sweep_id=1",
                params![
                    second_key.updated_at,
                    second_key.session_id.to_string(),
                    future
                ],
            )
            .unwrap();

        let third = store.reserve_terminal_reclaim_page(1).unwrap();
        assert_eq!(third.candidates[0].session_id, Uuid::from_u128(3));
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT updated_at FROM sandbox_reclaim_sweeps WHERE sweep_id=1",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            future
        );
        assert!(
            store
                .reserve_terminal_reclaim_page(1)
                .unwrap()
                .evidence
                .wrapped
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT updated_at FROM sandbox_reclaim_sweeps WHERE sweep_id=1",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            future
        );

        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT rsi_rfc3339_is_valid(?1)",
                    [second_key.updated_at],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT rsi_rfc3339_is_valid('2026-09-13 12:00:00.123456')",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert!(first.evidence.upper_bound.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_sweep_v119_migration_faults_rollback_retry_and_reopen() -> anyhow::Result<()>
    {
        for fault in TargetReclaimSweepV119MigrationFault::ALL {
            let fixture = FixtureDirectory::create("target-reclaim-sweep-migration");
            let database = fixture.path().join("migration.db");
            let store = Store::open(&database).unwrap();
            rewind_store_to_schema_version(&store.conn, 118);
            assert_eq!(fingerprint(&store.conn), V118_FULL_CATALOG_FINGERPRINT);
            let before = fingerprint(&store.conn);
            fail_next_v119_migration(fault);
            let error = store
                .apply_target_reclaim_sweep_v119_migration()
                .expect_err("V119 failpoint must roll back transaction");
            assert!(error.to_string().contains("injected V119"), "{error}");
            assert_eq!(fingerprint(&store.conn), before);
            assert_eq!(
                store
                    .conn
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                    .unwrap(),
                118
            );
            store.apply_target_reclaim_sweep_v119_migration().unwrap();
            assert_eq!(fingerprint(&store.conn), V119_FULL_CATALOG_FINGERPRINT);
            drop(store);
            let reopened = Store::open(&database).unwrap();
            assert_eq!(
                reopened
                    .conn
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))?,
                crate::store::LATEST_SCHEMA_VERSION
            );
            validate_v119_catalog(&reopened.conn)?;
            crate::store::source_worktree_v120::validate_v120_catalog(&reopened.conn)?;
            crate::store::restart_intents::validate_v134_catalog(&reopened.conn)?;
        }
        Ok(())
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_sweep_limits_and_catalog_indexes_are_bounded() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.preview_terminal_reclaim_page(0).is_err());
        assert!(
            store
                .reserve_terminal_reclaim_page(MAX_PAGE_SIZE + 1)
                .is_err()
        );
        validate_v119_catalog(&store.conn).unwrap();
        let plan = store
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id,updated_at FROM sessions
                 WHERE status IN ('Completed','Failed','Interrupted','Archived','Deleted')
                 ORDER BY updated_at,id LIMIT 32",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" ");
        assert!(
            plan.contains("idx_sessions_terminal_reclaim_page_v119"),
            "terminal sweep must use bounded page index: {plan}"
        );

        for (sql, parameters) in [
            (
                SELECT_TERMINAL_KEYS_FROM_START_SQL,
                params![
                    "2026-09-13T12:00:02+00:00",
                    Uuid::from_u128(3).to_string(),
                    32_i64
                ],
            ),
            (
                SELECT_TERMINAL_KEYS_AFTER_SQL,
                params![
                    "2026-09-13T12:00:00+00:00",
                    Uuid::from_u128(1).to_string(),
                    "2026-09-13T12:00:02+00:00",
                    Uuid::from_u128(3).to_string(),
                    32_i64
                ],
            ),
        ] {
            let query_plan = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(parameters, |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join(" ");
            assert!(
                query_plan.contains("idx_sessions_terminal_reclaim_page_v119"),
                "production terminal page must use V119 keyset index: {query_plan}"
            );
        }

        let lookup_plan = store
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {SELECT_CANDIDATE_FOR_KEY_SQL}"
            ))
            .unwrap()
            .query_map(
                params![Uuid::from_u128(1).to_string(), "2026-09-13T12:00:00+00:00"],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" ");
        assert!(
            lookup_plan.contains("sqlite_autoindex_sandbox_custody_roots_3 (owner_session_id=?)")
                && (lookup_plan.contains(
                    "idx_sessions_sandbox_custody_id_id (sandbox_custody_id=? AND id=?)"
                ) || lookup_plan
                    .contains("idx_swc_v120_custody_participants (sandbox_custody_id=? AND id=?)"))
                && !lookup_plan.contains("SCAN"),
            "custody lookup must use bounded custody indexes: {lookup_plan}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn target_reclaim_sweep_write_contention_is_bounded_and_restores_timeout() {
        let fixture = FixtureDirectory::create("target-reclaim-sweep-busy");
        let database = fixture.path().join("reclaim.db");
        let mut store = Store::open(&database).unwrap();
        seed_candidate(&mut store, fixture.path(), 0);
        let blocker = Connection::open(&database).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let error = store
            .reserve_terminal_reclaim_page(1)
            .expect_err("competing writer must return bounded contention");
        assert!(
            matches!(
                error,
                DaemonError::Database(rusqlite::Error::SqliteFailure(failure, _))
                    if matches!(
                        failure.code,
                        rusqlite::ErrorCode::DatabaseBusy
                            | rusqlite::ErrorCode::DatabaseLocked
                    )
            ),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "sweep contention exceeded bounded wait: {:?}",
            started.elapsed()
        );
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, u64>(0))
                .unwrap(),
            10_000
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT cycle,after_updated_at FROM sandbox_reclaim_sweeps WHERE sweep_id=1",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .unwrap(),
            (0, None)
        );

        blocker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            store
                .reserve_terminal_reclaim_page(1)
                .unwrap()
                .candidates
                .len(),
            1
        );
    }
}
