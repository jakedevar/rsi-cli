//! Durable, repository-scoped migration numbers in seal order.
//!
//! Git observations are supplied by the caller. The manager and lander must
//! verify source custody, remote ancestry, and the released manifest before
//! invoking these store operations. A `publishing` claim is never expired or
//! released automatically: an uncertain push needs an explicit settlement.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// RSI-RELEASED-MIGRATION-BEGIN: migration-allocation-catalog
const CATALOG: &str = "CREATE TABLE migration_allocation_repositories (
    repository TEXT PRIMARY KEY CHECK(length(repository)>0 AND length(CAST(repository AS BLOB))<=1024),
    remote_tip TEXT NOT NULL CHECK(length(remote_tip)=40 AND remote_tip=lower(remote_tip) AND remote_tip NOT GLOB '*[^0-9a-f]*'),
    landed_version INTEGER NOT NULL CHECK(landed_version>=0 AND landed_version<2147483647),
    row_version INTEGER NOT NULL CHECK(row_version>0),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at))
);
CREATE TABLE migration_allocation_claims (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE CHECK(rsi_uuid_is_canonical(id)),
    repository TEXT NOT NULL REFERENCES migration_allocation_repositories(repository) ON DELETE RESTRICT,
    source_commit TEXT NOT NULL CHECK(length(source_commit)=40 AND source_commit=lower(source_commit) AND source_commit NOT GLOB '*[^0-9a-f]*'),
    work_key TEXT NOT NULL CHECK(length(work_key)>0 AND length(CAST(work_key AS BLOB))<=256),
    epic_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(epic_id)),
    assigned_version INTEGER NOT NULL CHECK(assigned_version>0 AND assigned_version<=2147483647),
    state TEXT NOT NULL CHECK(state IN ('active','publishing','reassigning','released','expired','consumed')),
    remote_tip TEXT NOT NULL CHECK(length(remote_tip)=40 AND remote_tip=lower(remote_tip) AND remote_tip NOT GLOB '*[^0-9a-f]*'),
    candidate_commit TEXT CHECK(candidate_commit IS NULL OR (length(candidate_commit)=40 AND candidate_commit=lower(candidate_commit) AND candidate_commit NOT GLOB '*[^0-9a-f]*')),
    expires_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(expires_at)),
    row_version INTEGER NOT NULL CHECK(row_version>0),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at))
);
CREATE UNIQUE INDEX migration_allocation_live_version ON migration_allocation_claims(repository,assigned_version) WHERE state IN ('active','publishing');
CREATE UNIQUE INDEX migration_allocation_live_source ON migration_allocation_claims(repository,source_commit) WHERE state IN ('active','publishing');
CREATE INDEX migration_allocation_order ON migration_allocation_claims(repository,state,sequence);
CREATE TABLE migration_allocation_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository TEXT NOT NULL REFERENCES migration_allocation_repositories(repository) ON DELETE RESTRICT,
    claim_id TEXT REFERENCES migration_allocation_claims(id) ON DELETE RESTRICT,
    action TEXT NOT NULL,
    old_version INTEGER,
    new_version INTEGER,
    old_source_commit TEXT,
    new_source_commit TEXT,
    detail_json TEXT CHECK(detail_json IS NULL OR json_valid(detail_json)),
    request_key TEXT NOT NULL,
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at))
);
CREATE INDEX migration_allocation_events_by_repo ON migration_allocation_events(repository,id);
CREATE TABLE migration_allocation_operations (
    repository TEXT NOT NULL REFERENCES migration_allocation_repositories(repository) ON DELETE RESTRICT,
    idempotency_key TEXT NOT NULL,
    request_json TEXT NOT NULL CHECK(json_valid(request_json)),
    receipt_json TEXT NOT NULL CHECK(json_valid(receipt_json)),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    PRIMARY KEY(repository,idempotency_key)
);
CREATE TRIGGER migration_allocation_claims_no_delete BEFORE DELETE ON migration_allocation_claims BEGIN SELECT RAISE(ABORT,'migration allocation claims are append only'); END;
CREATE TRIGGER migration_allocation_events_no_update BEFORE UPDATE ON migration_allocation_events BEGIN SELECT RAISE(ABORT,'migration allocation events are immutable'); END;
CREATE TRIGGER migration_allocation_events_no_delete BEFORE DELETE ON migration_allocation_events BEGIN SELECT RAISE(ABORT,'migration allocation events are append only'); END;
CREATE TRIGGER migration_allocation_operations_no_update BEFORE UPDATE ON migration_allocation_operations BEGIN SELECT RAISE(ABORT,'migration allocation operations are immutable'); END;
CREATE TRIGGER migration_allocation_operations_no_delete BEFORE DELETE ON migration_allocation_operations BEGIN SELECT RAISE(ABORT,'migration allocation operations are append only'); END;";
// RSI-RELEASED-MIGRATION-END: migration-allocation-catalog

// RSI-RELEASED-MIGRATION-BEGIN: migration-allocation-migration
pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "migration allocation requires V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(CATALOG)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: migration-allocation-migration

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationAllocationHead {
    pub repository: String,
    pub remote_tip: String,
    pub landed_version: u32,
    pub row_version: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationAllocationClaim {
    pub sequence: i64,
    pub id: Uuid,
    pub repository: String,
    pub source_commit: String,
    pub work_key: String,
    pub epic_id: Uuid,
    pub assigned_version: u32,
    pub state: String,
    pub remote_tip: String,
    pub candidate_commit: Option<String>,
    pub expires_at: String,
    pub row_version: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MigrationAllocationRequest {
    pub repository: String,
    pub idempotency_key: String,
    pub change: MigrationAllocationChange,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum MigrationAllocationChange {
    Seal {
        source_commit: String,
        work_key: String,
        epic_id: Uuid,
        remote_tip: String,
        landed_version: u32,
        expires_at: DateTime<Utc>,
    },
    BeginPublish {
        claim_id: Uuid,
        source_commit: String,
        work_key: String,
        epic_id: Uuid,
        expected_row_version: i64,
        remote_tip: String,
        candidate_commit: String,
    },
    /// `published` is an externally verified Git ancestry fact. The caller
    /// must retain an uncertain push until it can establish this fact.
    SettlePublish {
        claim_id: Uuid,
        source_commit: String,
        work_key: String,
        epic_id: Uuid,
        expected_row_version: i64,
        candidate_commit: String,
        published: bool,
        observed_tip: String,
        observed_version: u32,
    },
    Release {
        claim_id: Uuid,
        source_commit: String,
        work_key: String,
        epic_id: Uuid,
        expected_row_version: i64,
    },
    Transfer {
        claim_id: Uuid,
        source_commit: String,
        work_key: String,
        epic_id: Uuid,
        expected_row_version: i64,
        new_source_commit: String,
        new_work_key: String,
        new_epic_id: Uuid,
        expires_at: DateTime<Utc>,
    },
    /// Advance after independently verifying the remote tip and catalog.
    ReconcileHead {
        expected_tip: String,
        observed_tip: String,
        observed_version: u32,
        /// Source commits independently proved ancestral to observed_tip.
        published_sources: Vec<String>,
    },
    SweepExpired,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct MigrationAllocationReceipt {
    pub head: MigrationAllocationHead,
    pub claim: Option<MigrationAllocationClaim>,
    pub event_id: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct MigrationAllocationEvent {
    pub id: i64,
    pub claim_id: Option<Uuid>,
    pub action: String,
    pub old_version: Option<u32>,
    pub new_version: Option<u32>,
    pub old_source_commit: Option<String>,
    pub new_source_commit: Option<String>,
    pub detail: Option<serde_json::Value>,
    pub request_key: String,
    pub created_at: String,
}

fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn validate_request(request: &MigrationAllocationRequest, now: DateTime<Utc>) -> Result<()> {
    if request.repository.is_empty()
        || request.repository.len() > 1024
        || request.repository.contains('\0')
        || request.idempotency_key.is_empty()
        || request.idempotency_key.len() > 128
        || request.idempotency_key.contains('\0')
    {
        return Err(refused("migration_allocation_invalid_request"));
    }
    let identity = |source: &str, work: &str, epic: Uuid| {
        sha(source)
            && !work.is_empty()
            && work.len() <= 256
            && !work.contains('\0')
            && !epic.is_nil()
    };
    let valid = match &request.change {
        MigrationAllocationChange::Seal {
            source_commit,
            work_key,
            epic_id,
            remote_tip,
            landed_version,
            expires_at,
        } => {
            identity(source_commit, work_key, *epic_id)
                && sha(remote_tip)
                && *landed_version < i32::MAX as u32
                && *expires_at > now
        }
        MigrationAllocationChange::BeginPublish {
            claim_id,
            source_commit,
            work_key,
            epic_id,
            expected_row_version,
            remote_tip,
            candidate_commit,
        } => {
            !claim_id.is_nil()
                && identity(source_commit, work_key, *epic_id)
                && *expected_row_version > 0
                && sha(remote_tip)
                && sha(candidate_commit)
        }
        MigrationAllocationChange::SettlePublish {
            claim_id,
            source_commit,
            work_key,
            epic_id,
            expected_row_version,
            candidate_commit,
            observed_tip,
            observed_version,
            ..
        } => {
            !claim_id.is_nil()
                && identity(source_commit, work_key, *epic_id)
                && *expected_row_version > 0
                && sha(candidate_commit)
                && sha(observed_tip)
                && *observed_version < i32::MAX as u32
        }
        MigrationAllocationChange::Release {
            claim_id,
            source_commit,
            work_key,
            epic_id,
            expected_row_version,
        } => {
            !claim_id.is_nil()
                && identity(source_commit, work_key, *epic_id)
                && *expected_row_version > 0
        }
        MigrationAllocationChange::Transfer {
            claim_id,
            source_commit,
            work_key,
            epic_id,
            expected_row_version,
            new_source_commit,
            new_work_key,
            new_epic_id,
            expires_at,
        } => {
            !claim_id.is_nil()
                && identity(source_commit, work_key, *epic_id)
                && identity(new_source_commit, new_work_key, *new_epic_id)
                && *expected_row_version > 0
                && *expires_at > now
        }
        MigrationAllocationChange::ReconcileHead {
            expected_tip,
            observed_tip,
            observed_version,
            published_sources,
        } => {
            sha(expected_tip)
                && sha(observed_tip)
                && *observed_version < i32::MAX as u32
                && published_sources.len() <= 256
                && published_sources.iter().all(|source| sha(source))
                && published_sources
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == published_sources.len()
        }
        MigrationAllocationChange::SweepExpired => true,
    };
    if !valid {
        return Err(refused("migration_allocation_invalid_request"));
    }
    Ok(())
}

fn head(conn: &Connection, repository: &str) -> Result<Option<MigrationAllocationHead>> {
    Ok(conn.query_row(
        "SELECT repository,remote_tip,landed_version,row_version FROM migration_allocation_repositories WHERE repository=?1",
        [repository],
        |r| Ok(MigrationAllocationHead {
            repository: r.get(0)?, remote_tip: r.get(1)?, landed_version: r.get(2)?, row_version: r.get(3)?,
        }),
    ).optional()?)
}

fn claim(
    conn: &Connection,
    repository: &str,
    id: Uuid,
) -> Result<Option<MigrationAllocationClaim>> {
    Ok(conn.query_row(
        "SELECT sequence,id,repository,source_commit,work_key,epic_id,assigned_version,state,remote_tip,candidate_commit,expires_at,row_version FROM migration_allocation_claims WHERE repository=?1 AND id=?2",
        params![repository, id.to_string()],
        |r| Ok(MigrationAllocationClaim {
            sequence: r.get(0)?, id: Uuid::parse_str(&r.get::<_, String>(1)?).map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?,
            repository: r.get(2)?, source_commit: r.get(3)?, work_key: r.get(4)?,
            epic_id: Uuid::parse_str(&r.get::<_, String>(5)?).map_err(|e| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e)))?,
            assigned_version: r.get(6)?, state: r.get(7)?, remote_tip: r.get(8)?,
            candidate_commit: r.get(9)?, expires_at: r.get(10)?, row_version: r.get(11)?,
        }),
    ).optional()?)
}

fn live_claims(conn: &Connection, repository: &str) -> Result<Vec<MigrationAllocationClaim>> {
    let mut query = conn.prepare("SELECT id FROM migration_allocation_claims WHERE repository=?1 AND state IN ('active','publishing') ORDER BY sequence")?;
    let ids = query
        .query_map([repository], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ids.into_iter()
        .map(|id| {
            claim(
                conn,
                repository,
                Uuid::parse_str(&id)
                    .map_err(|_| refused("migration_allocation_invalid_stored_identity"))?,
            )?
            .ok_or_else(|| refused("migration_allocation_claim_missing"))
        })
        .collect()
}

fn event(
    tx: &Transaction<'_>,
    repository: &str,
    claim_id: Option<Uuid>,
    action: &str,
    old_version: Option<u32>,
    new_version: Option<u32>,
    old_source: Option<&str>,
    new_source: Option<&str>,
    key: &str,
    now: &str,
) -> Result<i64> {
    tx.execute("INSERT INTO migration_allocation_events(repository,claim_id,action,old_version,new_version,old_source_commit,new_source_commit,request_key,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![repository, claim_id.map(|id| id.to_string()), action, old_version, new_version, old_source, new_source, key, now])?;
    Ok(tx.last_insert_rowid())
}

fn transfer_event(
    tx: &Transaction<'_>,
    old: &MigrationAllocationClaim,
    new_source: &str,
    new_work: &str,
    new_epic: Uuid,
    key: &str,
    now: &str,
) -> Result<i64> {
    let detail = serde_json::json!({
        "old_work_key": old.work_key,
        "old_epic_id": old.epic_id,
        "new_work_key": new_work,
        "new_epic_id": new_epic,
    });
    tx.execute("INSERT INTO migration_allocation_events(repository,claim_id,action,old_version,new_version,old_source_commit,new_source_commit,detail_json,request_key,created_at) VALUES(?1,?2,'transfer',?3,?3,?4,?5,?6,?7,?8)",
        params![old.repository, old.id.to_string(), old.assigned_version, old.source_commit, new_source, detail.to_string(), key, now])?;
    Ok(tx.last_insert_rowid())
}

fn compact(tx: &Transaction<'_>, repository: &str, key: &str, now: &str) -> Result<Option<i64>> {
    let current =
        head(tx, repository)?.ok_or_else(|| refused("migration_allocation_repository_missing"))?;
    let claims = live_claims(tx, repository)?;
    let publishing = claims
        .iter()
        .filter(|claim| claim.state == "publishing")
        .collect::<Vec<_>>();
    if publishing.len() > 1
        || publishing
            .first()
            .is_some_and(|claim| claim.sequence != claims[0].sequence)
    {
        return Err(refused("migration_allocation_publish_order_changed"));
    }
    let mut next = current
        .landed_version
        .checked_add(1)
        .ok_or_else(|| refused("migration_allocation_version_exhausted"))?;
    if let Some(claim) = publishing.first() {
        if claim.assigned_version != next {
            return Err(refused("migration_allocation_publish_order_changed"));
        }
        next = next
            .checked_add(1)
            .ok_or_else(|| refused("migration_allocation_version_exhausted"))?;
    }
    let active = claims
        .into_iter()
        .filter(|claim| claim.state == "active")
        .collect::<Vec<_>>();
    for claim in &active {
        tx.execute(
            "UPDATE migration_allocation_claims SET state='reassigning' WHERE id=?1",
            [claim.id.to_string()],
        )?;
    }
    let mut last = None;
    for claim in active {
        if next > i32::MAX as u32 {
            return Err(refused("migration_allocation_version_exhausted"));
        }
        let changed = claim.assigned_version != next;
        tx.execute("UPDATE migration_allocation_claims SET state='active',assigned_version=?2,row_version=?3,updated_at=?4,remote_tip=?5 WHERE id=?1",
            params![claim.id.to_string(), next, claim.row_version + i64::from(changed), now, current.remote_tip])?;
        if changed {
            last = Some(event(
                tx,
                repository,
                Some(claim.id),
                "renumber",
                Some(claim.assigned_version),
                Some(next),
                Some(&claim.source_commit),
                Some(&claim.source_commit),
                key,
                now,
            )?);
        }
        next = next
            .checked_add(1)
            .ok_or_else(|| refused("migration_allocation_version_exhausted"))?;
    }
    Ok(last)
}

fn expire_due(tx: &Transaction<'_>, repository: &str, key: &str, now: &str) -> Result<Option<i64>> {
    let mut stmt = tx.prepare("SELECT id FROM migration_allocation_claims WHERE repository=?1 AND state='active' AND expires_at<=?2 ORDER BY sequence")?;
    let ids = stmt
        .query_map(params![repository, now], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let had_expired = !ids.is_empty();
    let mut last = None;
    for id in ids {
        let id = Uuid::parse_str(&id)
            .map_err(|_| refused("migration_allocation_invalid_stored_identity"))?;
        let old = claim(tx, repository, id)?
            .ok_or_else(|| refused("migration_allocation_claim_missing"))?;
        tx.execute("UPDATE migration_allocation_claims SET state='expired',row_version=row_version+1,updated_at=?2 WHERE id=?1",
            params![id.to_string(), now])?;
        last = Some(event(
            tx,
            repository,
            Some(id),
            "expire",
            Some(old.assigned_version),
            None,
            Some(&old.source_commit),
            None,
            key,
            now,
        )?);
    }
    if had_expired {
        if let Some(renumber) = compact(tx, repository, key, now)? {
            last = Some(renumber);
        }
    }
    Ok(last)
}

fn fenced_claim(
    tx: &Transaction<'_>,
    repository: &str,
    id: Uuid,
    source: &str,
    work: &str,
    epic: Uuid,
    row_version: i64,
) -> Result<MigrationAllocationClaim> {
    let claim =
        claim(tx, repository, id)?.ok_or_else(|| refused("migration_allocation_claim_missing"))?;
    if claim.source_commit != source || claim.work_key != work || claim.epic_id != epic {
        return Err(refused("migration_allocation_source_changed"));
    }
    if claim.row_version != row_version {
        return Err(refused("migration_allocation_row_changed"));
    }
    Ok(claim)
}

impl Store {
    /// The caller authenticates Git observations and source custody. Every
    /// accepted operation, including expiry side effects, commits atomically.
    pub(crate) fn migration_allocation_apply(
        &self,
        request: &MigrationAllocationRequest,
    ) -> Result<MigrationAllocationReceipt> {
        self.migration_allocation_apply_at(request, Utc::now())
    }

    fn migration_allocation_apply_at(
        &self,
        request: &MigrationAllocationRequest,
        now: DateTime<Utc>,
    ) -> Result<MigrationAllocationReceipt> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let repository = &request.repository;
        let payload = serde_json::to_string(request)?;
        let replay: Option<(String, String)> = tx.query_row(
            "SELECT request_json,receipt_json FROM migration_allocation_operations WHERE repository=?1 AND idempotency_key=?2",
            params![repository, request.idempotency_key], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        if let Some((prior, receipt)) = replay {
            if prior != payload {
                return Err(refused("migration_allocation_idempotency_conflict"));
            }
            return Ok(serde_json::from_str(&receipt)?);
        }
        validate_request(request, now)?;
        let now = stamp(now);
        let key = &request.idempotency_key;
        let mut changed_claim = None;
        let mut last_event: Option<i64>;
        match &request.change {
            MigrationAllocationChange::Seal {
                source_commit,
                work_key,
                epic_id,
                remote_tip,
                landed_version,
                expires_at,
            } => {
                let prior = head(&tx, repository)?;
                if let Some(head) = prior {
                    if head.remote_tip != *remote_tip || head.landed_version != *landed_version {
                        return Err(refused("migration_allocation_remote_changed"));
                    }
                } else {
                    tx.execute("INSERT INTO migration_allocation_repositories(repository,remote_tip,landed_version,row_version,updated_at) VALUES(?1,?2,?3,1,?4)",
                        params![repository, remote_tip, landed_version, now])?;
                }
                let _ = expire_due(&tx, repository, key, &now)?;
                if live_claims(&tx, repository)?
                    .iter()
                    .any(|claim| claim.source_commit == *source_commit)
                {
                    return Err(refused("migration_allocation_source_already_live"));
                }
                let version = live_claims(&tx, repository)?
                    .last()
                    .map_or(*landed_version, |claim| claim.assigned_version)
                    .checked_add(1)
                    .ok_or_else(|| refused("migration_allocation_version_exhausted"))?;
                if version > i32::MAX as u32 {
                    return Err(refused("migration_allocation_version_exhausted"));
                }
                let id = Uuid::new_v4();
                tx.execute("INSERT INTO migration_allocation_claims(id,repository,source_commit,work_key,epic_id,assigned_version,state,remote_tip,candidate_commit,expires_at,row_version,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,'active',?7,NULL,?8,1,?9,?9)",
                    params![id.to_string(), repository, source_commit, work_key, epic_id.to_string(), version, remote_tip, stamp(*expires_at), now])?;
                changed_claim = Some(id);
                last_event = Some(event(
                    &tx,
                    repository,
                    Some(id),
                    "seal",
                    None,
                    Some(version),
                    None,
                    Some(source_commit),
                    key,
                    &now,
                )?);
            }
            change => {
                let current = head(&tx, repository)?
                    .ok_or_else(|| refused("migration_allocation_repository_missing"))?;
                last_event = if matches!(change, MigrationAllocationChange::ReconcileHead { .. }) {
                    None
                } else {
                    expire_due(&tx, repository, key, &now)?
                };
                match change {
                    MigrationAllocationChange::BeginPublish {
                        claim_id,
                        source_commit,
                        work_key,
                        epic_id,
                        expected_row_version,
                        remote_tip,
                        candidate_commit,
                    } => {
                        let old = fenced_claim(
                            &tx,
                            repository,
                            *claim_id,
                            source_commit,
                            work_key,
                            *epic_id,
                            *expected_row_version,
                        )?;
                        let first = live_claims(&tx, repository)?.into_iter().next();
                        if old.state != "active"
                            || first.as_ref().map(|first| first.id) != Some(*claim_id)
                            || old.assigned_version != current.landed_version + 1
                            || old.remote_tip != *remote_tip
                            || current.remote_tip != *remote_tip
                        {
                            return Err(refused("migration_allocation_publish_order_changed"));
                        }
                        tx.execute("UPDATE migration_allocation_claims SET state='publishing',candidate_commit=?2,row_version=row_version+1,updated_at=?3 WHERE id=?1",
                            params![claim_id.to_string(), candidate_commit, now])?;
                        changed_claim = Some(*claim_id);
                        last_event = Some(event(
                            &tx,
                            repository,
                            Some(*claim_id),
                            "begin_publish",
                            Some(old.assigned_version),
                            Some(old.assigned_version),
                            Some(source_commit),
                            Some(source_commit),
                            key,
                            &now,
                        )?);
                    }
                    MigrationAllocationChange::SettlePublish {
                        claim_id,
                        source_commit,
                        work_key,
                        epic_id,
                        expected_row_version,
                        candidate_commit,
                        published,
                        observed_tip,
                        observed_version,
                    } => {
                        let old = fenced_claim(
                            &tx,
                            repository,
                            *claim_id,
                            source_commit,
                            work_key,
                            *epic_id,
                            *expected_row_version,
                        )?;
                        if old.state != "publishing"
                            || old.candidate_commit.as_deref() != Some(candidate_commit)
                            || *observed_version < current.landed_version
                            || (*published && *observed_version < old.assigned_version)
                        {
                            return Err(refused("migration_allocation_settlement_changed"));
                        }
                        let state = if *published { "consumed" } else { "active" };
                        tx.execute("UPDATE migration_allocation_claims SET state=?2,candidate_commit=CASE WHEN ?3 THEN candidate_commit ELSE NULL END,row_version=row_version+1,updated_at=?4,remote_tip=?5 WHERE id=?1",
                            params![claim_id.to_string(), state, published, now, observed_tip])?;
                        tx.execute("UPDATE migration_allocation_repositories SET remote_tip=?2,landed_version=?3,row_version=row_version+1,updated_at=?4 WHERE repository=?1",
                            params![repository, observed_tip, observed_version, now])?;
                        changed_claim = Some(*claim_id);
                        last_event = Some(event(
                            &tx,
                            repository,
                            Some(*claim_id),
                            if *published {
                                "consume"
                            } else {
                                "publish_failed"
                            },
                            Some(old.assigned_version),
                            if *published {
                                None
                            } else {
                                Some(old.assigned_version)
                            },
                            Some(source_commit),
                            if *published {
                                None
                            } else {
                                Some(source_commit)
                            },
                            key,
                            &now,
                        )?);
                        if let Some(renumber) = compact(&tx, repository, key, &now)? {
                            last_event = Some(renumber);
                        }
                    }
                    MigrationAllocationChange::Release {
                        claim_id,
                        source_commit,
                        work_key,
                        epic_id,
                        expected_row_version,
                    } => {
                        let old = fenced_claim(
                            &tx,
                            repository,
                            *claim_id,
                            source_commit,
                            work_key,
                            *epic_id,
                            *expected_row_version,
                        )?;
                        if old.state != "active" {
                            return Err(refused("migration_allocation_claim_not_releasable"));
                        }
                        tx.execute("UPDATE migration_allocation_claims SET state='released',row_version=row_version+1,updated_at=?2 WHERE id=?1", params![claim_id.to_string(), now])?;
                        changed_claim = Some(*claim_id);
                        last_event = Some(event(
                            &tx,
                            repository,
                            Some(*claim_id),
                            "release",
                            Some(old.assigned_version),
                            None,
                            Some(source_commit),
                            None,
                            key,
                            &now,
                        )?);
                        if let Some(renumber) = compact(&tx, repository, key, &now)? {
                            last_event = Some(renumber);
                        }
                    }
                    MigrationAllocationChange::Transfer {
                        claim_id,
                        source_commit,
                        work_key,
                        epic_id,
                        expected_row_version,
                        new_source_commit,
                        new_work_key,
                        new_epic_id,
                        expires_at,
                    } => {
                        let old = fenced_claim(
                            &tx,
                            repository,
                            *claim_id,
                            source_commit,
                            work_key,
                            *epic_id,
                            *expected_row_version,
                        )?;
                        if old.state != "active" {
                            return Err(refused("migration_allocation_claim_not_transferable"));
                        }
                        if live_claims(&tx, repository)?.iter().any(|claim| {
                            claim.id != *claim_id && claim.source_commit == *new_source_commit
                        }) {
                            return Err(refused("migration_allocation_source_already_live"));
                        }
                        tx.execute("UPDATE migration_allocation_claims SET source_commit=?2,work_key=?3,epic_id=?4,expires_at=?5,candidate_commit=NULL,row_version=row_version+1,updated_at=?6 WHERE id=?1",
                            params![claim_id.to_string(), new_source_commit, new_work_key, new_epic_id.to_string(), stamp(*expires_at), now])?;
                        changed_claim = Some(*claim_id);
                        last_event = Some(transfer_event(
                            &tx,
                            &old,
                            new_source_commit,
                            new_work_key,
                            *new_epic_id,
                            key,
                            &now,
                        )?);
                    }
                    MigrationAllocationChange::ReconcileHead {
                        expected_tip,
                        observed_tip,
                        observed_version,
                        published_sources,
                    } => {
                        if current.remote_tip != *expected_tip
                            || *observed_version < current.landed_version
                            || live_claims(&tx, repository)?
                                .iter()
                                .any(|claim| claim.state == "publishing")
                        {
                            return Err(refused("migration_allocation_remote_changed"));
                        }
                        let live = live_claims(&tx, repository)?;
                        for source in published_sources {
                            let published = live
                                .iter()
                                .find(|claim| claim.source_commit == *source)
                                .ok_or_else(|| refused("migration_allocation_source_changed"))?;
                            if published.assigned_version > *observed_version {
                                return Err(refused("migration_allocation_publish_order_changed"));
                            }
                            tx.execute(
                                "UPDATE migration_allocation_claims SET state='consumed',row_version=row_version+1,updated_at=?2,remote_tip=?3 WHERE id=?1",
                                params![published.id.to_string(), now, observed_tip],
                            )?;
                            event(
                                &tx,
                                repository,
                                Some(published.id),
                                "observe_published",
                                Some(published.assigned_version),
                                None,
                                Some(source),
                                None,
                                key,
                                &now,
                            )?;
                        }
                        tx.execute("UPDATE migration_allocation_repositories SET remote_tip=?2,landed_version=?3,row_version=row_version+1,updated_at=?4 WHERE repository=?1",
                            params![repository, observed_tip, observed_version, now])?;
                        last_event = Some(event(
                            &tx,
                            repository,
                            None,
                            "reconcile_head",
                            Some(current.landed_version),
                            Some(*observed_version),
                            Some(expected_tip),
                            Some(observed_tip),
                            key,
                            &now,
                        )?);
                        if let Some(expired) = expire_due(&tx, repository, key, &now)? {
                            last_event = Some(expired);
                        }
                        if let Some(renumber) = compact(&tx, repository, key, &now)? {
                            last_event = Some(renumber);
                        }
                    }
                    MigrationAllocationChange::SweepExpired => {}
                    MigrationAllocationChange::Seal { .. } => unreachable!(),
                }
            }
        }
        let receipt = MigrationAllocationReceipt {
            head: head(&tx, repository)?
                .ok_or_else(|| refused("migration_allocation_repository_missing"))?,
            claim: changed_claim
                .map(|id| claim(&tx, repository, id))
                .transpose()?
                .flatten(),
            event_id: last_event,
        };
        tx.execute("INSERT INTO migration_allocation_operations(repository,idempotency_key,request_json,receipt_json,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![repository, key, payload, serde_json::to_string(&receipt)?, now])?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Bounded projection for manager and operator inspection. The caller
    /// decides visibility; this store method does not grant agent authority.
    pub fn migration_allocation_view(
        &self,
        repository: &str,
    ) -> Result<(
        Option<MigrationAllocationHead>,
        Vec<MigrationAllocationClaim>,
    )> {
        let mut stmt = self.conn.prepare("SELECT id FROM migration_allocation_claims WHERE repository=?1 ORDER BY sequence DESC LIMIT 256")?;
        let ids = stmt
            .query_map([repository], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut claims = ids
            .into_iter()
            .map(|id| {
                claim(
                    &self.conn,
                    repository,
                    Uuid::parse_str(&id)
                        .map_err(|_| refused("migration_allocation_invalid_stored_identity"))?,
                )?
                .ok_or_else(|| refused("migration_allocation_claim_missing"))
            })
            .collect::<Result<Vec<_>>>()?;
        claims.reverse();
        Ok((head(&self.conn, repository)?, claims))
    }

    /// Page immutable audit events without exposing the operations payload.
    pub(crate) fn migration_allocation_events(
        &self,
        repository: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<MigrationAllocationEvent>> {
        if repository.is_empty() || after_id < 0 || !(1..=256).contains(&limit) {
            return Err(refused("migration_allocation_invalid_request"));
        }
        let mut stmt = self.conn.prepare("SELECT id,claim_id,action,old_version,new_version,old_source_commit,new_source_commit,detail_json,request_key,created_at FROM migration_allocation_events WHERE repository=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
        let rows = stmt.query_map(params![repository, after_id, limit as i64], |r| {
            let claim_id: Option<String> = r.get(1)?;
            Ok(MigrationAllocationEvent {
                id: r.get(0)?,
                claim_id: claim_id
                    .map(|id| {
                        Uuid::parse_str(&id).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })
                    })
                    .transpose()?,
                action: r.get(2)?,
                old_version: r.get(3)?,
                new_version: r.get(4)?,
                old_source_commit: r.get(5)?,
                new_source_commit: r.get(6)?,
                detail: r
                    .get::<_, Option<String>>(7)?
                    .map(|raw| {
                        serde_json::from_str(&raw).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                7,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })
                    })
                    .transpose()?,
                request_key: r.get(8)?,
                created_at: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::sync::{Arc, Barrier};

    fn base() -> u32 {
        (super::super::LATEST_SCHEMA_VERSION - 1) as u32
    }

    fn source(n: char) -> String {
        std::iter::repeat_n(n, 40).collect()
    }
    fn seal(
        repo: &str,
        key: &str,
        source_commit: String,
        work_key: &str,
        epic_id: Uuid,
        version: u32,
        expires_at: DateTime<Utc>,
    ) -> MigrationAllocationRequest {
        MigrationAllocationRequest {
            repository: repo.into(),
            idempotency_key: key.into(),
            change: MigrationAllocationChange::Seal {
                source_commit,
                work_key: work_key.into(),
                epic_id,
                remote_tip: source('a'),
                landed_version: version,
                expires_at,
            },
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn seal_order_release_and_replay_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("allocation.db");
        let store = Store::open(&path).unwrap();
        let now = Utc::now();
        let epic = Uuid::new_v4();
        let first = seal(
            "repo",
            "first",
            source('b'),
            "work-a",
            epic,
            base(),
            now + Duration::hours(1),
        );
        let second = seal(
            "repo",
            "second",
            source('c'),
            "work-b",
            epic,
            base(),
            now + Duration::hours(1),
        );
        let a = store.migration_allocation_apply_at(&first, now).unwrap();
        let b = store.migration_allocation_apply_at(&second, now).unwrap();
        assert_eq!(a.claim.as_ref().unwrap().assigned_version, base() + 1);
        assert_eq!(b.claim.as_ref().unwrap().assigned_version, base() + 2);
        assert_eq!(store.migration_allocation_apply_at(&first, now).unwrap(), a);
        drop(store);
        let store = Store::open(&path).unwrap();
        let a = a.claim.unwrap();
        let released = store
            .migration_allocation_apply_at(
                &MigrationAllocationRequest {
                    repository: "repo".into(),
                    idempotency_key: "release".into(),
                    change: MigrationAllocationChange::Release {
                        claim_id: a.id,
                        source_commit: a.source_commit,
                        work_key: a.work_key,
                        epic_id: epic,
                        expected_row_version: a.row_version,
                    },
                },
                now,
            )
            .unwrap();
        assert_eq!(released.claim.unwrap().state, "released");
        let (_, claims) = store.migration_allocation_view("repo").unwrap();
        assert_eq!(claims[1].assigned_version, base() + 1);
        assert_eq!(claims[1].row_version, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn publish_requires_queue_head_and_settlement_preserves_reviewed_source() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let epic = Uuid::new_v4();
        let a = store
            .migration_allocation_apply_at(
                &seal(
                    "repo",
                    "first",
                    source('b'),
                    "a",
                    epic,
                    base(),
                    now + Duration::hours(1),
                ),
                now,
            )
            .unwrap()
            .claim
            .unwrap();
        let b = store
            .migration_allocation_apply_at(
                &seal(
                    "repo",
                    "second",
                    source('c'),
                    "b",
                    epic,
                    base(),
                    now + Duration::hours(1),
                ),
                now,
            )
            .unwrap()
            .claim
            .unwrap();
        let begin = |claim: &MigrationAllocationClaim, key: &str| MigrationAllocationRequest {
            repository: "repo".into(),
            idempotency_key: key.into(),
            change: MigrationAllocationChange::BeginPublish {
                claim_id: claim.id,
                source_commit: claim.source_commit.clone(),
                work_key: claim.work_key.clone(),
                epic_id: epic,
                expected_row_version: claim.row_version,
                remote_tip: source('a'),
                candidate_commit: source('d'),
            },
        };
        assert!(
            store
                .migration_allocation_apply_at(&begin(&b, "early"), now)
                .is_err()
        );
        let publishing = store
            .migration_allocation_apply_at(&begin(&a, "begin"), now)
            .unwrap()
            .claim
            .unwrap();
        assert!(
            store
                .migration_allocation_apply_at(
                    &MigrationAllocationRequest {
                        repository: "repo".into(),
                        idempotency_key: "release-publishing".into(),
                        change: MigrationAllocationChange::Release {
                            claim_id: publishing.id,
                            source_commit: publishing.source_commit.clone(),
                            work_key: publishing.work_key.clone(),
                            epic_id: epic,
                            expected_row_version: publishing.row_version,
                        },
                    },
                    now
                )
                .is_err()
        );
        let consumed = store
            .migration_allocation_apply_at(
                &MigrationAllocationRequest {
                    repository: "repo".into(),
                    idempotency_key: "settle".into(),
                    change: MigrationAllocationChange::SettlePublish {
                        claim_id: publishing.id,
                        source_commit: publishing.source_commit.clone(),
                        work_key: publishing.work_key.clone(),
                        epic_id: epic,
                        expected_row_version: publishing.row_version,
                        candidate_commit: source('d'),
                        published: true,
                        observed_tip: source('d'),
                        observed_version: base() + 1,
                    },
                },
                now,
            )
            .unwrap();
        assert_eq!(consumed.claim.unwrap().state, "consumed");
        assert_eq!(consumed.head.landed_version, base() + 1);
        let (_, claims) = store.migration_allocation_view("repo").unwrap();
        assert_eq!(claims[0].source_commit, source('b'));
        assert_eq!(claims[1].assigned_version, base() + 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn expiry_and_transfer_have_audit_and_fences() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let epic = Uuid::new_v4();
        let first = store
            .migration_allocation_apply_at(
                &seal(
                    "repo",
                    "first",
                    source('b'),
                    "a",
                    epic,
                    base(),
                    now + Duration::minutes(1),
                ),
                now,
            )
            .unwrap()
            .claim
            .unwrap();
        let transfer = MigrationAllocationRequest {
            repository: "repo".into(),
            idempotency_key: "transfer".into(),
            change: MigrationAllocationChange::Transfer {
                claim_id: first.id,
                source_commit: first.source_commit.clone(),
                work_key: first.work_key.clone(),
                epic_id: epic,
                expected_row_version: first.row_version,
                new_source_commit: source('c'),
                new_work_key: "new-work".into(),
                new_epic_id: epic,
                expires_at: now + Duration::minutes(3),
            },
        };
        let moved = store.migration_allocation_apply_at(&transfer, now).unwrap();
        assert_eq!(moved.claim.as_ref().unwrap().source_commit, source('c'));
        let audit = store.migration_allocation_events("repo", 0, 16).unwrap();
        assert_eq!(audit[1].action, "transfer");
        assert_eq!(audit[1].detail.as_ref().unwrap()["old_work_key"], "a");
        assert_eq!(
            audit[1].detail.as_ref().unwrap()["new_work_key"],
            "new-work"
        );
        assert!(
            store
                .migration_allocation_apply_at(
                    &MigrationAllocationRequest {
                        repository: "repo".into(),
                        idempotency_key: "stale".into(),
                        change: MigrationAllocationChange::Release {
                            claim_id: first.id,
                            source_commit: first.source_commit,
                            work_key: first.work_key,
                            epic_id: epic,
                            expected_row_version: first.row_version,
                        },
                    },
                    now
                )
                .is_err()
        );
        store
            .migration_allocation_apply_at(
                &MigrationAllocationRequest {
                    repository: "repo".into(),
                    idempotency_key: "sweep".into(),
                    change: MigrationAllocationChange::SweepExpired,
                },
                now + Duration::minutes(4),
            )
            .unwrap();
        assert_eq!(
            store.migration_allocation_view("repo").unwrap().1[0].state,
            "expired"
        );
        let events: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM migration_allocation_events WHERE repository='repo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn concurrent_seals_receive_distinct_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent.db");
        // Create and migrate once before the two independent Store connections race.
        drop(Store::open(&path).unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let epic = Uuid::new_v4();
        let handles = ['b', 'c']
            .into_iter()
            .map(|letter| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let store = Store::open(&path).unwrap();
                    barrier.wait();
                    store
                        .migration_allocation_apply(&seal(
                            "repo",
                            &format!("seal-{letter}"),
                            source(letter),
                            &format!("work-{letter}"),
                            epic,
                            base(),
                            Utc::now() + Duration::hours(1),
                        ))
                        .unwrap()
                        .claim
                        .unwrap()
                        .assigned_version
                })
            })
            .collect::<Vec<_>>();
        let mut versions = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        versions.sort_unstable();
        assert_eq!(versions, vec![base() + 1, base() + 2]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn publishing_claim_survives_restart_and_requires_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uncertain.db");
        let store = Store::open(&path).unwrap();
        let now = Utc::now();
        let epic = Uuid::new_v4();
        let sealed = store
            .migration_allocation_apply_at(
                &seal(
                    "repo",
                    "seal",
                    source('b'),
                    "work",
                    epic,
                    base(),
                    now + Duration::minutes(1),
                ),
                now,
            )
            .unwrap()
            .claim
            .unwrap();
        let publishing = store
            .migration_allocation_apply_at(
                &MigrationAllocationRequest {
                    repository: "repo".into(),
                    idempotency_key: "begin".into(),
                    change: MigrationAllocationChange::BeginPublish {
                        claim_id: sealed.id,
                        source_commit: sealed.source_commit.clone(),
                        work_key: sealed.work_key.clone(),
                        epic_id: epic,
                        expected_row_version: sealed.row_version,
                        remote_tip: source('a'),
                        candidate_commit: source('d'),
                    },
                },
                now,
            )
            .unwrap()
            .claim
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        store
            .migration_allocation_apply_at(
                &MigrationAllocationRequest {
                    repository: "repo".into(),
                    idempotency_key: "sweep".into(),
                    change: MigrationAllocationChange::SweepExpired,
                },
                now + Duration::minutes(2),
            )
            .unwrap();
        assert_eq!(
            store.migration_allocation_view("repo").unwrap().1[0].state,
            "publishing"
        );
        let settled = store
            .migration_allocation_apply_at(
                &MigrationAllocationRequest {
                    repository: "repo".into(),
                    idempotency_key: "settle".into(),
                    change: MigrationAllocationChange::SettlePublish {
                        claim_id: publishing.id,
                        source_commit: publishing.source_commit,
                        work_key: publishing.work_key,
                        epic_id: epic,
                        expected_row_version: publishing.row_version,
                        candidate_commit: source('d'),
                        published: true,
                        observed_tip: source('e'),
                        observed_version: base() + 1,
                    },
                },
                now + Duration::minutes(2),
            )
            .unwrap();
        assert_eq!(settled.claim.unwrap().state, "consumed");
        assert_eq!(settled.head.remote_tip, source('e'));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn observed_landing_consumes_claim_before_next_seal() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let epic = Uuid::new_v4();
        let first = store
            .migration_allocation_apply_at(
                &seal(
                    "repo",
                    "first",
                    source('b'),
                    "work-a",
                    epic,
                    base(),
                    now + Duration::hours(1),
                ),
                now,
            )
            .unwrap()
            .claim
            .unwrap();
        let second = store
            .migration_allocation_apply_at(
                &seal(
                    "repo",
                    "second",
                    source('c'),
                    "work-b",
                    epic,
                    base(),
                    now + Duration::hours(1),
                ),
                now,
            )
            .unwrap()
            .claim
            .unwrap();
        let reconcile = MigrationAllocationRequest {
            repository: "repo".into(),
            idempotency_key: "observed-landing".into(),
            change: MigrationAllocationChange::ReconcileHead {
                expected_tip: source('a'),
                observed_tip: source('d'),
                observed_version: base() + 1,
                published_sources: vec![first.source_commit.clone()],
            },
        };
        assert_eq!(
            store
                .migration_allocation_apply_at(&reconcile, now)
                .unwrap()
                .head
                .landed_version,
            base() + 1
        );
        assert_eq!(
            store
                .migration_allocation_apply_at(&reconcile, now)
                .unwrap()
                .head
                .landed_version,
            base() + 1
        );
        let (_, claims) = store.migration_allocation_view("repo").unwrap();
        assert_eq!(claims[0].state, "consumed");
        assert_eq!(claims[1].id, second.id);
        assert_eq!(claims[1].assigned_version, base() + 2);
        let third = store
            .migration_allocation_apply_at(
                &MigrationAllocationRequest {
                    repository: "repo".into(),
                    idempotency_key: "third".into(),
                    change: MigrationAllocationChange::Seal {
                        source_commit: source('e'),
                        work_key: "work-c".into(),
                        epic_id: epic,
                        remote_tip: source('d'),
                        landed_version: base() + 1,
                        expires_at: now + Duration::hours(1),
                    },
                },
                now,
            )
            .unwrap()
            .claim
            .unwrap();
        assert_eq!(third.assigned_version, base() + 3);
    }
}
