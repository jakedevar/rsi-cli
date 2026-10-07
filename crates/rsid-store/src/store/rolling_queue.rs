//! Durable rolling merge queue (#1007 slice S1).
//!
//! One row per accepted source, FIFO by `sequence`. A run is a `gating`
//! entry plus one `rolling_queue_batches` row (S1 batches hold exactly one
//! member). Settlement CASes `gating -> terminal` and, in the same
//! transaction, inserts the single owner wake (`scheduled_jobs`, mode
//! `resume`), so a second settle writes nothing.

use super::Store;
use super::scheduled_jobs::insert_scheduled_job_conn;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::rolling_queue::{
    QUEUE_DUPLICATE_SOURCE, RollingQueueBinding, RollingQueueEntryState, RollingQueueEntryV1,
    RollingQueueOutcome,
};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};
use uuid::Uuid;

/// Bound on the `continued_from` walk for an archived source's wake target.
const QUEUE_WAKE_LINEAGE_LIMIT: usize = 16;

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: rolling-queue-migration
/// Provisional schema version of the queue catalog; the lander renumbers it.
pub(crate) const ROLLING_QUEUE_SCHEMA_VERSION: i32 = 140;

const OID_CHECK: &str = "length({c})=40 AND {c}=lower({c}) AND {c} NOT GLOB '*[^0-9a-f]*'";

fn catalog() -> String {
    let oid = |column: &str| OID_CHECK.replace("{c}", column);
    format!(
        "CREATE TABLE rolling_queue_entries (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE CHECK(rsi_uuid_is_canonical(id)),
    project_id TEXT CHECK(project_id IS NULL OR rsi_uuid_is_canonical(project_id)),
    repo_path TEXT NOT NULL CHECK(length(repo_path)>0),
    source_commit TEXT NOT NULL CHECK({source}),
    source_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(source_session_id)),
    owner_epic_id TEXT CHECK(owner_epic_id IS NULL OR rsi_uuid_is_canonical(owner_epic_id)),
    binding TEXT NOT NULL CHECK(binding IN ('bound','unbound')),
    work_key TEXT CHECK(work_key IS NULL OR length(work_key)>0),
    migration_version INTEGER CHECK(migration_version IS NULL OR migration_version>0),
    hot_files_json TEXT NOT NULL CHECK(json_valid(hot_files_json)),
    test_filters_json TEXT NOT NULL CHECK(json_valid(test_filters_json)),
    state TEXT NOT NULL CHECK(state IN ('queued','admitted','gating','published','refused','failed','superseded')),
    outcome_json TEXT CHECK(outcome_json IS NULL OR json_valid(outcome_json)),
    idempotency_key TEXT NOT NULL CHECK(length(idempotency_key)>0),
    batch_id TEXT CHECK(batch_id IS NULL OR rsi_uuid_is_canonical(batch_id)) REFERENCES rolling_queue_batches(id) ON DELETE RESTRICT,
    wake_job_id TEXT UNIQUE CHECK(wake_job_id IS NULL OR rsi_uuid_is_canonical(wake_job_id)),
    enqueued_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(enqueued_at)),
    admitted_at TEXT CHECK(admitted_at IS NULL OR rsi_rfc3339_nanos_is_canonical(admitted_at)),
    finished_at TEXT CHECK(finished_at IS NULL OR rsi_rfc3339_nanos_is_canonical(finished_at)),
    row_version INTEGER NOT NULL CHECK(row_version>0),
    UNIQUE(source_session_id, idempotency_key)
);
CREATE UNIQUE INDEX rolling_queue_live_source ON rolling_queue_entries(source_commit) WHERE state IN ('queued','admitted','gating');
CREATE INDEX rolling_queue_by_state ON rolling_queue_entries(state, sequence);
CREATE TABLE rolling_queue_batches (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    project_id TEXT CHECK(project_id IS NULL OR rsi_uuid_is_canonical(project_id)),
    state TEXT NOT NULL CHECK(state IN ('forming','gating','publishing','bisecting','published','refused')),
    base_tip TEXT CHECK(base_tip IS NULL OR ({base})),
    candidate_oid TEXT CHECK(candidate_oid IS NULL OR ({candidate})),
    member_ids_json TEXT NOT NULL CHECK(json_valid(member_ids_json)),
    member_count INTEGER NOT NULL CHECK(member_count>0),
    segment_json TEXT CHECK(segment_json IS NULL OR json_valid(segment_json)),
    speculative_of_batch_id TEXT CHECK(speculative_of_batch_id IS NULL OR rsi_uuid_is_canonical(speculative_of_batch_id)) REFERENCES rolling_queue_batches(id) ON DELETE RESTRICT,
    gate_summary_json TEXT CHECK(gate_summary_json IS NULL OR json_valid(gate_summary_json)),
    unit_name TEXT UNIQUE,
    job_id TEXT,
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    settled_at TEXT CHECK(settled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(settled_at)),
    row_version INTEGER NOT NULL CHECK(row_version>0)
);
CREATE TABLE rolling_queue_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id TEXT NOT NULL REFERENCES rolling_queue_batches(id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK(length(kind)>0),
    entry_id TEXT REFERENCES rolling_queue_entries(id) ON DELETE RESTRICT,
    detail_json TEXT CHECK(detail_json IS NULL OR json_valid(detail_json)),
    at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(at))
);
CREATE INDEX rolling_queue_events_by_batch ON rolling_queue_events(batch_id, id);
CREATE TRIGGER rolling_queue_entries_no_delete BEFORE DELETE ON rolling_queue_entries BEGIN SELECT RAISE(ABORT,'rolling queue entries are append only'); END;
CREATE TRIGGER rolling_queue_entries_terminal_immutable BEFORE UPDATE ON rolling_queue_entries WHEN OLD.state IN ('published','refused','failed','superseded') BEGIN SELECT RAISE(ABORT,'terminal rolling queue entries are immutable'); END;
CREATE TRIGGER rolling_queue_batches_no_delete BEFORE DELETE ON rolling_queue_batches BEGIN SELECT RAISE(ABORT,'rolling queue batches are append only'); END;
CREATE TRIGGER rolling_queue_batches_terminal_immutable BEFORE UPDATE ON rolling_queue_batches WHEN OLD.state IN ('published','refused') BEGIN SELECT RAISE(ABORT,'terminal rolling queue batches are immutable'); END;
CREATE TRIGGER rolling_queue_events_no_update BEFORE UPDATE ON rolling_queue_events BEGIN SELECT RAISE(ABORT,'rolling queue events are immutable'); END;
CREATE TRIGGER rolling_queue_events_no_delete BEFORE DELETE ON rolling_queue_events BEGIN SELECT RAISE(ABORT,'rolling queue events are append only'); END;",
        source = oid("source_commit"),
        base = oid("base_tip"),
        candidate = oid("candidate_oid"),
    )
}

/// Catalog objects, for the fixture rewind and presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 12] = [
    ("table", "rolling_queue_entries"),
    ("table", "rolling_queue_batches"),
    ("table", "rolling_queue_events"),
    ("index", "rolling_queue_live_source"),
    ("index", "rolling_queue_by_state"),
    ("index", "rolling_queue_events_by_batch"),
    ("trigger", "rolling_queue_entries_no_delete"),
    ("trigger", "rolling_queue_entries_terminal_immutable"),
    ("trigger", "rolling_queue_batches_no_delete"),
    ("trigger", "rolling_queue_batches_terminal_immutable"),
    ("trigger", "rolling_queue_events_no_update"),
    ("trigger", "rolling_queue_events_no_delete"),
];

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "rolling queue requires V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(&catalog())?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: rolling-queue-migration

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

/// Called in the same proven seat-publication transaction as durable job
/// transfer. Source and replay identity remain unchanged; only delivery moves.
pub(crate) fn transfer_manager_queue_wakes_on(
    conn: &rusqlite::Connection,
    predecessor: Uuid,
    successor: Uuid,
) -> Result<()> {
    if conn.is_autocommit() {
        return Err(DaemonError::Store(
            "manager queue wake transfer requires a transaction".into(),
        ));
    }
    if predecessor == successor {
        return Ok(());
    }
    let old = predecessor.to_string();
    let new = successor.to_string();
    conn.execute(
        "UPDATE rolling_queue_entries SET wake_session_id=?2,row_version=row_version+1
         WHERE COALESCE(wake_session_id,source_session_id)=?1
           AND state IN ('queued','admitted','gating')",
        params![old, new],
    )?;
    // Settlement can win the transaction race with succession. Move an
    // unfired outcome too, without touching its immutable terminal entry.
    conn.execute(
        "UPDATE scheduled_jobs SET wake_session_id=?2,updated_at=?3
         WHERE enabled=1 AND wake_mode='resume' AND wake_session_id=?1
           AND id IN (SELECT wake_job_id FROM rolling_queue_entries WHERE wake_job_id IS NOT NULL)",
        params![old, new, stamp(Utc::now())],
    )?;
    Ok(())
}

/// Everything the enqueue verb resolves before the store write.
#[derive(Debug, Clone)]
pub struct NewQueueEntry {
    pub project_id: Option<Uuid>,
    pub repo_path: String,
    pub source_commit: String,
    pub source_session_id: Uuid,
    pub owner_epic_id: Option<Uuid>,
    pub binding: RollingQueueBinding,
    pub work_key: Option<String>,
    pub migration_version: Option<u32>,
    pub hot_files: Vec<String>,
    pub test_filters: Vec<String>,
    pub idempotency_key: String,
}

/// The first N queued entries of one repository, claimed as one batch.
#[derive(Debug, Clone)]
pub struct ClaimedQueueBatch {
    pub batch_id: Uuid,
    pub repo_path: String,
    /// FIFO (enqueue) order.
    pub entries: Vec<RollingQueueEntryV1>,
}

/// One entry claimed for a run, with the batch that records it.
#[derive(Debug, Clone)]
pub struct ClaimedQueueEntry {
    pub entry: RollingQueueEntryV1,
    pub repo_path: String,
    pub batch_id: Uuid,
}

const ENTRY_COLUMNS: &str = "sequence, id, project_id, repo_path, source_commit, source_session_id, owner_epic_id, binding, work_key, migration_version, hot_files_json, test_filters_json, state, outcome_json, enqueued_at, finished_at, wake_session_id";

struct EntryRow {
    entry: RollingQueueEntryV1,
    repo_path: String,
    project_id: Option<String>,
    wake_session_id: Option<Uuid>,
}

fn parse_uuid(value: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn json_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn map_entry(row: &Row<'_>) -> rusqlite::Result<EntryRow> {
    let binding = match row.get::<_, String>(7)?.as_str() {
        "bound" => RollingQueueBinding::Bound,
        _ => RollingQueueBinding::Unbound,
    };
    let state_text: String = row.get(12)?;
    let state = RollingQueueEntryState::parse(&state_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            12,
            rusqlite::types::Type::Text,
            format!("unknown queue state {state_text}").into(),
        )
    })?;
    let outcome = row
        .get::<_, Option<String>>(13)?
        .map(|text| serde_json::from_str::<RollingQueueOutcome>(&text).map_err(json_error))
        .transpose()?;
    Ok(EntryRow {
        wake_session_id: row
            .get::<_, Option<String>>(16)?
            .as_deref()
            .map(parse_uuid)
            .transpose()?,
        repo_path: row.get(3)?,
        project_id: row.get(2)?,
        entry: RollingQueueEntryV1 {
            sequence: row.get(0)?,
            id: parse_uuid(&row.get::<_, String>(1)?)?,
            source_commit: row.get(4)?,
            source_session_id: parse_uuid(&row.get::<_, String>(5)?)?,
            owner_epic_id: row
                .get::<_, Option<String>>(6)?
                .map(|value| parse_uuid(&value))
                .transpose()?,
            binding,
            work_key: row.get(8)?,
            migration_version: row.get::<_, Option<u32>>(9)?,
            hot_files: serde_json::from_str(&row.get::<_, String>(10)?).map_err(json_error)?,
            test_filters: serde_json::from_str(&row.get::<_, String>(11)?).map_err(json_error)?,
            state,
            outcome,
            enqueued_at: row.get(14)?,
            finished_at: row.get(15)?,
        },
    })
}

fn entry_by_key(
    tx: &Transaction<'_>,
    session: Uuid,
    key: &str,
) -> Result<Option<RollingQueueEntryV1>> {
    Ok(tx
        .query_row(
            &format!(
                "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
                 WHERE source_session_id=?1 AND idempotency_key=?2"
            ),
            params![session.to_string(), key],
            map_entry,
        )
        .optional()?
        .map(|row| row.entry))
}

fn record_event(
    tx: &Transaction<'_>,
    batch: Uuid,
    kind: &str,
    entry: Option<Uuid>,
    detail: Option<serde_json::Value>,
    now: DateTime<Utc>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO rolling_queue_events(batch_id, kind, entry_id, detail_json, at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            batch.to_string(),
            kind,
            entry.map(|id| id.to_string()),
            detail.map(|value| value.to_string()),
            stamp(now)
        ],
    )?;
    Ok(())
}

/// Human/agent-readable wake body for one terminal outcome.
fn wake_message(entry: &RollingQueueEntryV1, outcome: &RollingQueueOutcome) -> String {
    let mut text = format!(
        "[merge-queue] source {} entry {}: {}",
        entry.source_commit,
        entry.id,
        outcome.refusal.as_deref().map_or_else(
            || "published".to_string(),
            |code| format!("not landed ({code})")
        )
    );
    if let Some(sha) = &outcome.landed_sha {
        text.push_str(&format!("; landed_sha={sha}"));
    }
    if !outcome.failing_tests.is_empty() {
        text.push_str(&format!(
            "; failing_tests={}",
            outcome.failing_tests.join(",")
        ));
    }
    if let Some(detail) = &outcome.detail {
        text.push_str(&format!("; detail={detail}"));
    }
    text
}

/// Entries settled together that wake the same resolved owner (#1146).
struct WakeGroup {
    target: Uuid,
    wake_id: Uuid,
    lead: Uuid,
    lines: Vec<String>,
}

impl Store {
    /// Admit one source. A replay of the same `(session, key)` returns the
    /// existing entry; a live duplicate of the same commit is refused.
    pub fn enqueue_rolling_queue_source(
        &self,
        new: &NewQueueEntry,
        now: DateTime<Utc>,
    ) -> Result<(RollingQueueEntryV1, bool)> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = entry_by_key(&tx, new.source_session_id, &new.idempotency_key)? {
            let same = existing.source_commit == new.source_commit
                && existing.test_filters == new.test_filters;
            return if same {
                Ok((existing, true))
            } else {
                Err(refused("queue_idempotency_conflict"))
            };
        }
        let live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rolling_queue_entries \
             WHERE source_commit=?1 AND state IN ('queued','admitted','gating'))",
            [&new.source_commit],
            |row| row.get(0),
        )?;
        if live {
            return Err(refused(QUEUE_DUPLICATE_SOURCE));
        }
        let id = Uuid::new_v4();
        tx.execute(
            "INSERT INTO rolling_queue_entries(id, project_id, repo_path, source_commit, \
             source_session_id, owner_epic_id, binding, work_key, migration_version, \
             hot_files_json, test_filters_json, state, idempotency_key, enqueued_at, row_version) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'queued',?12,?13,1)",
            params![
                id.to_string(),
                new.project_id.map(|value| value.to_string()),
                new.repo_path,
                new.source_commit,
                new.source_session_id.to_string(),
                new.owner_epic_id.map(|value| value.to_string()),
                new.binding.as_str(),
                new.work_key,
                new.migration_version,
                serde_json::to_string(&new.hot_files)
                    .map_err(|e| DaemonError::Store(e.to_string()))?,
                serde_json::to_string(&new.test_filters)
                    .map_err(|e| DaemonError::Store(e.to_string()))?,
                new.idempotency_key,
                stamp(now),
            ],
        )?;
        let entry = tx
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1"),
                [id.to_string()],
                map_entry,
            )?
            .entry;
        tx.commit()?;
        Ok((entry, false))
    }

    /// Claim the FIFO head for a run. Single-flight: nothing is claimed while
    /// another entry is `gating`.
    pub fn claim_next_rolling_queue_entry(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<ClaimedQueueEntry>> {
        Ok(self.claim_rolling_queue_batch(now, 1)?.and_then(|batch| {
            let ClaimedQueueBatch {
                batch_id,
                repo_path,
                entries,
            } = batch;
            entries.into_iter().next().map(|entry| ClaimedQueueEntry {
                entry,
                repo_path,
                batch_id,
            })
        }))
    }

    /// Claim the first `max` queued entries of the head's repository, in FIFO
    /// (enqueue) order, as one batch. Single-flight: nothing is claimed while
    /// another entry is `gating`. A migration-carrying entry is only ever
    /// claimed together with, or after, every earlier entry of its repository,
    /// so migration numbers are assigned in queue order and never skip ahead of
    /// a predecessor.
    pub fn claim_rolling_queue_batch(
        &self,
        now: DateTime<Utc>,
        max: usize,
    ) -> Result<Option<ClaimedQueueBatch>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let gating: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rolling_queue_entries WHERE state='gating')",
            [],
            |row| row.get(0),
        )?;
        if gating || max == 0 {
            return Ok(None);
        }
        let Some(head) = tx
            .query_row(
                &format!(
                    "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
                     WHERE state='queued' ORDER BY sequence LIMIT 1"
                ),
                [],
                map_entry,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let mut members = Vec::new();
        {
            let mut statement = tx.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
                 WHERE state='queued' AND repo_path=?1 ORDER BY sequence LIMIT ?2"
            ))?;
            let rows = statement.query_map(params![head.repo_path, max as i64], map_entry)?;
            for row in rows {
                members.push(row?.entry);
            }
        }
        if members.is_empty() {
            return Ok(None);
        }
        let batch_id = Uuid::new_v4();
        let ids: Vec<Uuid> = members.iter().map(|entry| entry.id).collect();
        tx.execute(
            "INSERT INTO rolling_queue_batches(id, project_id, state, member_ids_json, \
             member_count, created_at, row_version) VALUES (?1,?2,'gating',?3,?4,?5,1)",
            params![
                batch_id.to_string(),
                head.project_id,
                serde_json::json!(ids).to_string(),
                ids.len() as i64,
                stamp(now),
            ],
        )?;
        for entry in &mut members {
            let claimed = tx.execute(
                "UPDATE rolling_queue_entries SET state='gating', batch_id=?2, admitted_at=?3, \
                 row_version=row_version+1 WHERE id=?1 AND state='queued'",
                params![entry.id.to_string(), batch_id.to_string(), stamp(now)],
            )?;
            if claimed != 1 {
                return Ok(None);
            }
            record_event(&tx, batch_id, "gating", Some(entry.id), None, now)?;
            entry.state = RollingQueueEntryState::Gating;
        }
        tx.commit()?;
        Ok(Some(ClaimedQueueBatch {
            batch_id,
            repo_path: head.repo_path,
            entries: members,
        }))
    }

    /// The batch gate was not green (or its merge/policy step refused): mark
    /// the batch `bisecting`; its members stay `gating` while the runner
    /// narrows the red down and settles each of them once.
    pub fn begin_rolling_queue_bisect(
        &self,
        batch_id: Uuid,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.begin_rolling_queue_bisect_with(batch_id, serde_json::json!({ "reason": reason }), now)
    }

    /// As `begin_rolling_queue_bisect`, with the whole `bisect_started` event
    /// detail (the reason plus the failing run's cause and log path, #1135).
    pub fn begin_rolling_queue_bisect_with(
        &self,
        batch_id: Uuid,
        detail: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE rolling_queue_batches SET state='bisecting', row_version=row_version+1 \
             WHERE id=?1 AND state='gating'",
            [batch_id.to_string()],
        )?;
        if changed == 1 {
            record_event(&tx, batch_id, "bisect_started", None, Some(detail), now)?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Append one audit event (the bisect trail) to a batch.
    pub fn record_rolling_queue_batch_event(
        &self,
        batch_id: Uuid,
        kind: &str,
        entry: Option<Uuid>,
        detail: Option<serde_json::Value>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        record_event(&tx, batch_id, kind, entry, detail, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Record the tip a batch was cut from and the candidate it published (or
    /// gated), plus a summary of its members, on the still-open batch.
    pub fn record_rolling_queue_batch_gate(
        &self,
        batch_id: Uuid,
        base_tip: Option<&str>,
        candidate_oid: Option<&str>,
        summary: &serde_json::Value,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE rolling_queue_batches SET base_tip=COALESCE(?2, base_tip), \
             candidate_oid=COALESCE(?3, candidate_oid), gate_summary_json=?4, \
             row_version=row_version+1 \
             WHERE id=?1 AND state NOT IN ('published','refused')",
            params![
                batch_id.to_string(),
                base_tip,
                candidate_oid,
                summary.to_string()
            ],
        )?;
        Ok(())
    }

    /// Record the migration number the queue assigned a `gating` entry.
    pub fn record_rolling_queue_assigned_migration(&self, id: Uuid, version: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE rolling_queue_entries SET migration_version=?2, row_version=row_version+1 \
             WHERE id=?1 AND state='gating'",
            params![id.to_string(), version],
        )?;
        Ok(())
    }

    /// The migration number the queue last planned for `id` in its batch's
    /// durable `migration_plan` events (recorded before each run pushes), or
    /// `None`. A restart uses it when a run published before the queue could
    /// record the lander's report.
    pub fn planned_rolling_queue_migration(&self, id: Uuid) -> Result<Option<u32>> {
        let mut statement = self.conn.prepare(
            "SELECT detail_json FROM rolling_queue_events \
             WHERE kind='migration_plan' AND batch_id=(SELECT batch_id FROM rolling_queue_entries WHERE id=?1) \
             ORDER BY id DESC",
        )?;
        let details = statement
            .query_map([id.to_string()], |row| row.get::<_, Option<String>>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let wanted = id.to_string();
        for detail in details.into_iter().flatten() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&detail) else {
                continue;
            };
            let found = value["assigned"].as_array().and_then(|list| {
                list.iter().find_map(|item| {
                    (item["id"].as_str() == Some(wanted.as_str()))
                        .then(|| item["version"].as_u64())
                        .flatten()
                })
            });
            if let Some(version) = found.and_then(|v| u32::try_from(v).ok()) {
                return Ok(Some(version));
            }
        }
        Ok(None)
    }

    /// The migration number the lander's own receipt assigned `id`, from the
    /// durable `migration_receipt` event `publish_group` records before it
    /// updates the entry. `None` when no receipt was saved. A restart never
    /// overwrites this actual number with a plan.
    pub fn rolling_queue_migration_receipt(&self, id: Uuid) -> Result<Option<u32>> {
        let detail: Option<String> = self
            .conn
            .query_row(
                "SELECT detail_json FROM rolling_queue_events \
                 WHERE kind='migration_receipt' AND entry_id=?1 ORDER BY id DESC LIMIT 1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(detail
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| value["version"].as_u64())
            .and_then(|version| u32::try_from(version).ok()))
    }

    /// Return a `gating` entry to `queued` (restart re-drive). The crashed
    /// run's batch is refused with an audit event.
    pub fn requeue_gating_rolling_queue_entry(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let batch: Option<String> = tx
            .query_row(
                "SELECT batch_id FROM rolling_queue_entries WHERE id=?1 AND state='gating'",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(batch) = batch else {
            return Ok(false);
        };
        let batch_id = parse_uuid(&batch)?;
        tx.execute(
            "UPDATE rolling_queue_entries SET state='queued', batch_id=NULL, admitted_at=NULL, \
             row_version=row_version+1 WHERE id=?1 AND state='gating'",
            [id.to_string()],
        )?;
        tx.execute(
            "UPDATE rolling_queue_batches SET state='refused', settled_at=?2, \
             row_version=row_version+1 WHERE id=?1 AND state IN ('gating','bisecting')",
            params![batch, stamp(now)],
        )?;
        record_event(&tx, batch_id, "requeued_after_restart", Some(id), None, now)?;
        tx.commit()?;
        Ok(true)
    }

    /// CAS `gating -> terminal` and insert the owner's single wake in the same
    /// transaction. Returns the wake job id, or `None` when the entry was not
    /// `gating` (already settled): nothing is written then.
    pub fn settle_rolling_queue_entry(
        &self,
        id: Uuid,
        terminal: RollingQueueEntryState,
        outcome: &RollingQueueOutcome,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>> {
        let settled = self.settle_rolling_queue_entries(&[(id, terminal, outcome.clone())], now)?;
        Ok(settled.first().map(|(_, wake)| *wake))
    }

    /// Settle a group of entries in one transaction and wake each resolved
    /// owner once (#1146). Every entry keeps its own CAS `gating -> terminal`
    /// (an already-settled entry is skipped, so a replay writes nothing) and
    /// its own outcome row; only the wake is shared: entries whose resolved
    /// wake target (#1134) is the same get ONE wake whose message lists every
    /// entry. The wake is named `merge-queue-<lead entry id>` (the first
    /// settled entry of that owner, unique because an entry settles once) and
    /// is recorded as the lead entry's `wake_job_id`; the other members keep
    /// `wake_job_id` NULL (the column is UNIQUE, one wake cannot be theirs
    /// too). Returns `(entry id, covering wake id)` for each entry settled
    /// here, in input order.
    pub fn settle_rolling_queue_entries(
        &self,
        items: &[(Uuid, RollingQueueEntryState, RollingQueueOutcome)],
        now: DateTime<Utc>,
    ) -> Result<Vec<(Uuid, Uuid)>> {
        if items.iter().any(|(_, state, _)| !state.is_terminal()) {
            return Err(DaemonError::InvalidParam(
                "queue_settle_state_not_terminal".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut groups: Vec<WakeGroup> = Vec::new();
        let mut settled = Vec::new();
        for (id, terminal, outcome) in items {
            let Some(row) = tx
                .query_row(
                    &format!(
                        "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
                         WHERE id=?1 AND state='gating'"
                    ),
                    [id.to_string()],
                    map_entry,
                )
                .optional()?
            else {
                continue;
            };
            let entry = row.entry;
            let target =
                self.queue_wake_target(row.wake_session_id.unwrap_or(entry.source_session_id));
            let position = groups.iter().position(|group| group.target == target);
            let group = match position {
                Some(position) => &mut groups[position],
                None => {
                    groups.push(WakeGroup {
                        target,
                        wake_id: Uuid::new_v4(),
                        lead: entry.id,
                        lines: Vec::new(),
                    });
                    groups.last_mut().expect("just pushed")
                }
            };
            let is_lead = group.lead == entry.id;
            let outcome_json =
                serde_json::to_string(outcome).map_err(|e| DaemonError::Store(e.to_string()))?;
            let changed = tx.execute(
                "UPDATE rolling_queue_entries SET state=?2, outcome_json=?3, wake_job_id=?4, \
                 finished_at=?5, row_version=row_version+1 WHERE id=?1 AND state='gating'",
                params![
                    id.to_string(),
                    terminal.as_str(),
                    outcome_json,
                    is_lead.then(|| group.wake_id.to_string()),
                    stamp(now)
                ],
            )?;
            if changed != 1 {
                continue;
            }
            group.lines.push(wake_message(&entry, outcome));
            settled.push((*id, group.wake_id));
            self.settle_batch_bookkeeping(&tx, *id, terminal.as_str(), outcome, now)?;
            super::friction::note_lander_friction_in(&tx, &entry, *terminal, outcome, now);
        }
        for group in groups {
            if group.lines.is_empty() {
                continue;
            }
            let message = if group.lines.len() == 1 {
                group.lines[0].clone()
            } else {
                let mut text = format!(
                    "[merge-queue] {} entries settled together",
                    group.lines.len()
                );
                for line in &group.lines {
                    text.push_str("\n- ");
                    text.push_str(line);
                }
                text
            };
            let wake = ScheduledJob {
                id: group.wake_id,
                name: format!("merge-queue-{}", group.lead),
                message,
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
                wake_mode: WakeMode::Resume,
                wake_session_id: Some(group.target),
            };
            insert_scheduled_job_conn(&tx, &wake)?;
        }
        tx.commit()?;
        Ok(settled)
    }

    /// The batch settles with its last member (published when any member
    /// landed, refused when none did); every settled member is an event.
    fn settle_batch_bookkeeping(
        &self,
        tx: &Transaction<'_>,
        id: Uuid,
        terminal: &str,
        outcome: &RollingQueueOutcome,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let batch: Option<String> = tx
            .query_row(
                "SELECT batch_id FROM rolling_queue_entries WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(batch) = batch else {
            return Ok(());
        };
        let (open, landed): (i64, bool) = tx.query_row(
            "SELECT COUNT(*) FILTER (WHERE state='gating'), \
             COALESCE(MAX(state='published'), 0) \
             FROM rolling_queue_entries WHERE batch_id=?1",
            [&batch],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if open == 0 {
            tx.execute(
                "UPDATE rolling_queue_batches SET state=?2, settled_at=?3, \
                 row_version=row_version+1 WHERE id=?1 AND state NOT IN ('published','refused')",
                params![
                    batch,
                    if landed { "published" } else { "refused" },
                    stamp(now)
                ],
            )?;
        }
        record_event(
            tx,
            parse_uuid(&batch)?,
            terminal,
            Some(id),
            Some(serde_json::to_value(outcome).map_err(|e| DaemonError::Store(e.to_string()))?),
            now,
        )?;
        Ok(())
    }

    /// Settle-time wake target (#1134). An outcome reaches whoever holds the
    /// enqueuer's authority now: a live source keeps its own wake; an
    /// archived manager seat resolves to the logical manager's current seat;
    /// any other archived source resolves to its sole continuation tip. Every
    /// ambiguity or lookup failure fails closed to the original source.
    fn queue_wake_target(&self, source: Uuid) -> Uuid {
        self.try_queue_wake_target(source)
            .ok()
            .flatten()
            .unwrap_or(source)
    }

    fn try_queue_wake_target(&self, source: Uuid) -> Result<Option<Uuid>> {
        let row: Option<(Option<String>, String)> = self
            .conn
            .query_row(
                "SELECT project_id, status FROM sessions WHERE id=?1",
                [source.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((project, status)) = row else {
            return Ok(None);
        };
        if status != "Archived" {
            return Ok(None);
        }
        if let Some(project) = project.and_then(|p| Uuid::parse_str(&p).ok())
            && let Some(current) = self
                .get_harness_manager(project)?
                .and_then(|config| config.current_session_id)
            && super::harness_manager::manager_lineage_tip_on(&self.conn, source).ok()
                == Some(current)
        {
            return Ok(Some(current));
        }
        let mut tip = source;
        for _ in 0..QUEUE_WAKE_LINEAGE_LIMIT {
            let mut statement = self
                .conn
                .prepare("SELECT id FROM sessions WHERE continued_from=?1 ORDER BY id LIMIT 2")?;
            let next = statement
                .query_map([tip.to_string()], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            match next.as_slice() {
                [] => break,
                [only] => tip = parse_uuid(only)?,
                _ => return Ok(None),
            }
        }
        if tip == source {
            return Ok(None);
        }
        let tip_status: String = self.conn.query_row(
            "SELECT status FROM sessions WHERE id=?1",
            [tip.to_string()],
            |row| row.get(0),
        )?;
        Ok((!matches!(tip_status.as_str(), "Archived" | "Deleted")).then_some(tip))
    }

    pub fn get_rolling_queue_entry(&self, id: Uuid) -> Result<Option<RollingQueueEntryV1>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1"),
                [id.to_string()],
                map_entry,
            )
            .optional()?
            .map(|row| row.entry))
    }

    /// Entries in `state`, oldest first, for the operator view and restart
    /// reconcile. `repo_path` is returned alongside for the latter.
    pub fn list_rolling_queue_entries(
        &self,
        state: Option<RollingQueueEntryState>,
        limit: usize,
    ) -> Result<Vec<(RollingQueueEntryV1, String)>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
             WHERE (?1 IS NULL OR state=?1) ORDER BY sequence LIMIT ?2"
        ))?;
        let rows = statement.query_map(
            params![state.map(RollingQueueEntryState::as_str), limit as i64],
            map_entry,
        )?;
        let mut entries = Vec::new();
        for row in rows {
            let row = row?;
            entries.push((row.entry, row.repo_path));
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    fn new_entry(session: Uuid, commit: char, key: &str) -> NewQueueEntry {
        NewQueueEntry {
            project_id: Some(Uuid::new_v4()),
            repo_path: "/tmp/repo".into(),
            source_commit: commit.to_string().repeat(40),
            source_session_id: session,
            owner_epic_id: Some(Uuid::new_v4()),
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: Some(141),
            hot_files: vec!["crates/rsid/src/rpc.rs".into()],
            test_filters: vec!["rsid=rolling_queue".into()],
            idempotency_key: key.into(),
        }
    }

    fn published(sha: &str) -> RollingQueueOutcome {
        RollingQueueOutcome {
            landed_sha: Some(sha.into()),
            ..RollingQueueOutcome::default()
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn entry_records_every_field_and_replays_by_key() {
        let store = store();
        let session = Uuid::new_v4();
        let new = new_entry(session, 'a', "k1");
        let (entry, replayed) = store
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap();
        assert!(!replayed);
        assert_eq!(entry.state, RollingQueueEntryState::Queued);
        assert_eq!(entry.source_commit, "a".repeat(40));
        assert_eq!(entry.source_session_id, session);
        assert_eq!(entry.owner_epic_id, new.owner_epic_id);
        assert_eq!(entry.binding, RollingQueueBinding::Unbound);
        assert_eq!(entry.migration_version, Some(141));
        assert_eq!(entry.hot_files, new.hot_files);
        assert_eq!(entry.test_filters, new.test_filters);

        let (again, replayed) = store
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap();
        assert!(replayed);
        assert_eq!(again.id, entry.id);

        let mut changed = new.clone();
        changed.source_commit = "b".repeat(40);
        let error = store
            .enqueue_rolling_queue_source(&changed, Utc::now())
            .unwrap_err();
        assert!(error.to_string().contains("queue_idempotency_conflict"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_live_duplicate_source_is_refused_but_a_settled_one_may_return() {
        let store = store();
        let session = Uuid::new_v4();
        store
            .enqueue_rolling_queue_source(&new_entry(session, 'a', "k1"), Utc::now())
            .unwrap();
        let error = store
            .enqueue_rolling_queue_source(&new_entry(Uuid::new_v4(), 'a', "k2"), Utc::now())
            .unwrap_err();
        assert!(error.to_string().contains(QUEUE_DUPLICATE_SOURCE));

        let claimed = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        let refusal = RollingQueueOutcome {
            refusal: Some("gate_failed".into()),
            ..RollingQueueOutcome::default()
        };
        store
            .settle_rolling_queue_entry(
                claimed.entry.id,
                RollingQueueEntryState::Refused,
                &refusal,
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        store
            .enqueue_rolling_queue_source(&new_entry(Uuid::new_v4(), 'a', "k3"), Utc::now())
            .unwrap();
    }

    /// #1333: a refused landing records `lander:refused:<code>` friction for
    /// the source session, with the entry as evidence.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn refused_landings_record_lander_friction() {
        let store = store();
        let session = Uuid::new_v4();
        store
            .enqueue_rolling_queue_source(&new_entry(session, 'a', "k1"), Utc::now())
            .unwrap();
        let claimed = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        let refusal = RollingQueueOutcome {
            refusal: Some("queue_empty_filter".into()),
            detail: Some("free text that must never be recorded".into()),
            ..RollingQueueOutcome::default()
        };
        store
            .settle_rolling_queue_entry(
                claimed.entry.id,
                RollingQueueEntryState::Refused,
                &refusal,
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        let row: (String, String, String) = store
            .conn
            .query_row(
                "SELECT signature, session_id, evidence_ref FROM friction_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "lander:refused:queue_empty_filter".to_string(),
                session.to_string(),
                format!("merge_queue:{}", claimed.entry.id)
            )
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn claims_are_fifo_and_single_flight() {
        let store = store();
        let session = Uuid::new_v4();
        for (commit, key) in [('a', "1"), ('b', "2"), ('c', "3")] {
            store
                .enqueue_rolling_queue_source(&new_entry(session, commit, key), Utc::now())
                .unwrap();
        }
        let first = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        assert_eq!(first.entry.source_commit, "a".repeat(40));
        assert!(
            store
                .claim_next_rolling_queue_entry(Utc::now())
                .unwrap()
                .is_none()
        );
        store
            .settle_rolling_queue_entry(
                first.entry.id,
                RollingQueueEntryState::Published,
                &published(&"9".repeat(40)),
                Utc::now(),
            )
            .unwrap();
        let second = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        assert_eq!(second.entry.source_commit, "b".repeat(40));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn settling_twice_inserts_exactly_one_owner_wake() {
        let store = store();
        let session = Uuid::new_v4();
        store
            .enqueue_rolling_queue_source(&new_entry(session, 'a', "k"), Utc::now())
            .unwrap();
        let claimed = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        let outcome = published(&"9".repeat(40));
        let wake = store
            .settle_rolling_queue_entry(
                claimed.entry.id,
                RollingQueueEntryState::Published,
                &outcome,
                Utc::now(),
            )
            .unwrap()
            .expect("first settle wakes");
        assert!(
            store
                .settle_rolling_queue_entry(
                    claimed.entry.id,
                    RollingQueueEntryState::Failed,
                    &outcome,
                    Utc::now(),
                )
                .unwrap()
                .is_none()
        );
        let wakes: Vec<_> = store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .collect();
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].id, wake);
        assert_eq!(wakes[0].wake_mode, WakeMode::Resume);
        assert_eq!(wakes[0].wake_session_id, Some(session));
        assert!(wakes[0].message.contains(&"9".repeat(40)));
        let entry = store
            .get_rolling_queue_entry(claimed.entry.id)
            .unwrap()
            .unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Published);
        assert_eq!(entry.outcome, Some(outcome));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn terminal_entries_are_immutable_and_undeletable() {
        let store = store();
        store
            .enqueue_rolling_queue_source(&new_entry(Uuid::new_v4(), 'a', "k"), Utc::now())
            .unwrap();
        let claimed = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        store
            .settle_rolling_queue_entry(
                claimed.entry.id,
                RollingQueueEntryState::Published,
                &published(&"9".repeat(40)),
                Utc::now(),
            )
            .unwrap();
        assert!(
            store
                .conn
                .execute("UPDATE rolling_queue_entries SET state='queued'", [])
                .is_err()
        );
        assert!(
            store
                .conn
                .execute("DELETE FROM rolling_queue_entries", [])
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn batch_references_must_name_an_existing_batch() {
        let store = store();
        store
            .enqueue_rolling_queue_source(&new_entry(Uuid::new_v4(), 'a', "k"), Utc::now())
            .unwrap();
        let missing = Uuid::new_v4().to_string();
        assert!(
            store
                .conn
                .execute(
                    "UPDATE rolling_queue_entries SET batch_id=?1",
                    rusqlite::params![missing],
                )
                .is_err(),
            "an entry may not point at a batch that does not exist"
        );
        let claimed = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        let batch: String = store
            .conn
            .query_row(
                "SELECT batch_id FROM rolling_queue_entries WHERE id=?1",
                rusqlite::params![claimed.entry.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            store
                .conn
                .execute(
                    "UPDATE rolling_queue_batches SET speculative_of_batch_id=?1 WHERE id=?2",
                    rusqlite::params![missing, batch],
                )
                .is_err(),
            "a speculative batch must name an existing batch"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn requeue_after_restart_reclaims_the_same_entry_once() {
        let store = store();
        store
            .enqueue_rolling_queue_source(&new_entry(Uuid::new_v4(), 'a', "k"), Utc::now())
            .unwrap();
        let first = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        assert!(
            store
                .requeue_gating_rolling_queue_entry(first.entry.id, Utc::now())
                .unwrap()
        );
        assert!(
            !store
                .requeue_gating_rolling_queue_entry(first.entry.id, Utc::now())
                .unwrap()
        );
        let second = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        assert_eq!(second.entry.id, first.entry.id);
        assert_ne!(second.batch_id, first.batch_id);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_batch_claims_the_fifo_prefix_of_one_repository_and_is_single_flight() {
        let store = store();
        let session = Uuid::new_v4();
        for (index, commit) in ['a', 'b', 'c', 'd', 'e'].into_iter().enumerate() {
            let mut new = new_entry(session, commit, &format!("k{index}"));
            if commit == 'c' {
                new.repo_path = "/tmp/other".into();
            }
            store
                .enqueue_rolling_queue_source(&new, Utc::now())
                .unwrap();
        }
        let batch = store
            .claim_rolling_queue_batch(Utc::now(), 3)
            .unwrap()
            .unwrap();
        let order: Vec<String> = batch
            .entries
            .iter()
            .map(|entry| entry.source_commit[..1].to_string())
            .collect();
        // FIFO within the head's repository; the other repository's entry waits.
        assert_eq!(order, ["a", "b", "d"]);
        assert!(
            batch
                .entries
                .iter()
                .all(|entry| entry.state == RollingQueueEntryState::Gating)
        );
        // Single-flight: nothing is claimed while the batch is gating.
        assert!(
            store
                .claim_rolling_queue_batch(Utc::now(), 3)
                .unwrap()
                .is_none()
        );
        let count: i64 = store
            .conn
            .query_row(
                "SELECT member_count FROM rolling_queue_batches WHERE id=?1",
                [batch.batch_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_batch_settles_with_its_last_member_and_wakes_each_owner_once() {
        let store = store();
        for (index, commit) in ['a', 'b'].into_iter().enumerate() {
            store
                .enqueue_rolling_queue_source(
                    &new_entry(Uuid::new_v4(), commit, &format!("k{index}")),
                    Utc::now(),
                )
                .unwrap();
        }
        let batch = store
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        assert!(
            store
                .begin_rolling_queue_bisect(batch.batch_id, "gate_failed", Utc::now())
                .unwrap()
        );
        let state = |id: Uuid| -> String {
            store
                .conn
                .query_row(
                    "SELECT state FROM rolling_queue_batches WHERE id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(state(batch.batch_id), "bisecting");
        let first = batch.entries[0].id;
        let second = batch.entries[1].id;
        store
            .settle_rolling_queue_entry(
                first,
                RollingQueueEntryState::Published,
                &published("f1"),
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        // One member is still gating, so the batch has not settled.
        assert_eq!(state(batch.batch_id), "bisecting");
        store
            .settle_rolling_queue_entry(
                second,
                RollingQueueEntryState::Refused,
                &RollingQueueOutcome {
                    refusal: Some("gate_failed".into()),
                    ..RollingQueueOutcome::default()
                },
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(state(batch.batch_id), "published");
        let wakes = store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 2);
    }
    fn insert_continuation(store: &Store, from: Uuid) -> Uuid {
        let mut next = store.get_session(from).unwrap().unwrap();
        next.id = Uuid::new_v4();
        next.continued_from = Some(from);
        next.rotation_depth += 1;
        next.status = rsi_common::types::SessionStatus::Completed;
        store.insert_session(&next).unwrap();
        next.id
    }

    fn archive(store: &Store, id: Uuid) {
        store
            .update_session_status(id, rsi_common::types::SessionStatus::Archived)
            .unwrap();
    }

    /// Enqueue as `source`, settle, and return every merge-queue wake target.
    fn settle_wake_targets(store: &Store, source: Uuid) -> Vec<Option<Uuid>> {
        store
            .enqueue_rolling_queue_source(&new_entry(source, 'a', "wake-target"), Utc::now())
            .unwrap();
        let claimed = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        store
            .settle_rolling_queue_entry(
                claimed.entry.id,
                RollingQueueEntryState::Published,
                &published(&"9".repeat(40)),
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .map(|job| job.wake_session_id)
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn outcome_wake_follows_displaced_live_manager_through_repeated_appointments() {
        use crate::store::manager_coordinator::tests::fixture;
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;

        for state in ["queued", "admitted", "gating"] {
            let store = store();
            let (mut config, lead) = fixture(&store, Default::default());
            let seat_a = config.current_session_id.unwrap();
            store
                .update_session_status(seat_a, rsi_common::types::SessionStatus::Running)
                .unwrap();
            let mut new = new_entry(seat_a, 'a', "seat");
            new.project_id = Some(config.project_id);
            let entry = store
                .enqueue_rolling_queue_source(&new, Utc::now())
                .unwrap()
                .0;
            if state == "gating" {
                store
                    .claim_next_rolling_queue_entry(Utc::now())
                    .unwrap()
                    .unwrap();
            } else if state == "admitted" {
                store
                    .conn
                    .execute(
                        "UPDATE rolling_queue_entries SET state='admitted' WHERE id=?1",
                        [entry.id.to_string()],
                    )
                    .unwrap();
            }
            let unrelated = new_entry(Uuid::new_v4(), 'b', "worker");
            let foreign = store
                .enqueue_rolling_queue_source(&unrelated, Utc::now())
                .unwrap()
                .0;
            let mut target = seat_a;
            for _ in 0..2 {
                let mut next = store.get_session(target).unwrap().unwrap();
                next.id = Uuid::new_v4();
                next.status = rsi_common::types::SessionStatus::Completed;
                next.continued_from = None;
                store.insert_session(&next).unwrap();
                config = store
                    .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                        project_id: config.project_id,
                        session_id: next.id,
                        epic_ids: Some(vec![lead.parent_id.unwrap()]),
                        group_ids: Vec::new(),
                        expected_row_version: config.row_version,
                    })
                    .unwrap();
                target = next.id;
            }
            assert_eq!(
                store.get_session(seat_a).unwrap().unwrap().status,
                rsi_common::types::SessionStatus::Running
            );
            // A retry by the original enqueuer still addresses the same row.
            let (replay, duplicate) = store
                .enqueue_rolling_queue_source(&new, Utc::now())
                .unwrap();
            assert!(duplicate);
            assert_eq!(replay.id, entry.id);
            assert_eq!(replay.source_session_id, seat_a);
            let untouched: Option<String> = store
                .conn
                .query_row(
                    "SELECT wake_session_id FROM rolling_queue_entries WHERE id=?1",
                    [foreign.id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(untouched, None);
            // Admitted entries are normally promoted by the queue runner.
            store
                .conn
                .execute(
                    "UPDATE rolling_queue_entries SET state='gating' WHERE id=?1",
                    [entry.id.to_string()],
                )
                .unwrap();
            let wake_id = store
                .settle_rolling_queue_entry(
                    entry.id,
                    RollingQueueEntryState::Refused,
                    &RollingQueueOutcome::default(),
                    Utc::now(),
                )
                .unwrap()
                .unwrap();
            let wake = store.get_scheduled_job(&wake_id).unwrap().unwrap();
            assert_eq!(wake.wake_session_id, Some(target), "pending state {state}");
            assert!(wake.enabled);
            assert_eq!(
                store
                    .get_rolling_queue_entry(entry.id)
                    .unwrap()
                    .unwrap()
                    .source_session_id,
                seat_a
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn queue_wake_transfer_preserves_terminal_rows_and_rolls_back_with_publication() {
        let store = store();
        let old = Uuid::new_v4();
        let next = Uuid::new_v4();
        let source = new_entry(old, 'a', "settled");
        let terminal = store
            .enqueue_rolling_queue_source(&source, Utc::now())
            .unwrap()
            .0;
        store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        let wake_id = store
            .settle_rolling_queue_entry(
                terminal.id,
                RollingQueueEntryState::Published,
                &published(&"9".repeat(40)),
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        let pending = store
            .enqueue_rolling_queue_source(&new_entry(old, 'b', "pending"), Utc::now())
            .unwrap()
            .0;
        let before: (Option<String>, i64) = store
            .conn
            .query_row(
                "SELECT wake_session_id,row_version FROM rolling_queue_entries WHERE id=?1",
                [terminal.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(transfer_manager_queue_wakes_on(&store.conn, old, next).is_err());
        {
            let tx =
                Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
            transfer_manager_queue_wakes_on(&tx, old, next).unwrap();
            assert_eq!(
                store
                    .get_scheduled_job(&wake_id)
                    .unwrap()
                    .unwrap()
                    .wake_session_id,
                Some(next)
            );
            tx.rollback().unwrap();
        }
        assert_eq!(
            store
                .get_scheduled_job(&wake_id)
                .unwrap()
                .unwrap()
                .wake_session_id,
            Some(old)
        );
        let pending_target: Option<String> = store
            .conn
            .query_row(
                "SELECT wake_session_id FROM rolling_queue_entries WHERE id=?1",
                [pending.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending_target, None);
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        transfer_manager_queue_wakes_on(&tx, old, next).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            store
                .get_scheduled_job(&wake_id)
                .unwrap()
                .unwrap()
                .wake_session_id,
            Some(next)
        );
        let after: (Option<String>, i64) = store
            .conn
            .query_row(
                "SELECT wake_session_id,row_version FROM rolling_queue_entries WHERE id=?1",
                [terminal.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, before);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn queue_wake_migration_preserves_existing_rows_and_replays() {
        let store = store();
        let source = Uuid::new_v4();
        let entry = store
            .enqueue_rolling_queue_source(&new_entry(source, 'a', "legacy"), Utc::now())
            .unwrap()
            .0;
        store
            .conn
            .execute_batch("ALTER TABLE rolling_queue_entries DROP COLUMN wake_session_id")
            .unwrap();
        // The latest step is the queue wake migration; derive its predecessor
        // so the fixture remains valid if landing renumbers it.
        store
            .conn
            .pragma_update(
                None,
                "user_version",
                super::super::LATEST_SCHEMA_VERSION - 1,
            )
            .unwrap();
        store.init_schema().unwrap();
        store.init_schema().unwrap();
        assert_eq!(
            store.get_rolling_queue_entry(entry.id).unwrap().unwrap(),
            entry
        );
        store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        let wake = store
            .settle_rolling_queue_entry(
                entry.id,
                RollingQueueEntryState::Published,
                &published(&"9".repeat(40)),
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .get_scheduled_job(&wake)
                .unwrap()
                .unwrap()
                .wake_session_id,
            Some(source)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn outcome_wake_follows_manager_seat_after_baton_pass() {
        use crate::store::manager_coordinator::tests::fixture;
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat_a = config.current_session_id.unwrap();
        let queued = {
            store
                .enqueue_rolling_queue_source(&new_entry(seat_a, 'a', "seat"), Utc::now())
                .unwrap();
            store
                .claim_next_rolling_queue_entry(Utc::now())
                .unwrap()
                .unwrap()
        };
        let seat_b = insert_continuation(&store, seat_a);
        archive(&store, seat_a);
        store
            .record_harness_manager_rotation(seat_a, seat_b)
            .unwrap();
        assert_eq!(
            store
                .get_harness_manager(config.project_id)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(seat_b)
        );
        store
            .settle_rolling_queue_entry(
                queued.entry.id,
                RollingQueueEntryState::Published,
                &published(&"9".repeat(40)),
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        let targets: Vec<_> = store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .map(|job| job.wake_session_id)
            .collect();
        assert_eq!(targets, vec![Some(seat_b)]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn outcome_wake_follows_archived_non_seat_to_continuation_tip() {
        use crate::test_support::test_session;
        let store = store();
        let origin = test_session(Uuid::new_v4(), std::path::PathBuf::from("/tmp/queue-wake"));
        store.insert_session(&origin).unwrap();
        let middle = insert_continuation(&store, origin.id);
        let tip = insert_continuation(&store, middle);
        archive(&store, origin.id);
        archive(&store, middle);
        assert_eq!(settle_wake_targets(&store, origin.id), vec![Some(tip)]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn outcome_wake_fails_closed_on_forked_continuation() {
        use crate::test_support::test_session;
        let store = store();
        let origin = test_session(Uuid::new_v4(), std::path::PathBuf::from("/tmp/queue-wake"));
        store.insert_session(&origin).unwrap();
        insert_continuation(&store, origin.id);
        insert_continuation(&store, origin.id);
        archive(&store, origin.id);
        assert_eq!(
            settle_wake_targets(&store, origin.id),
            vec![Some(origin.id)]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn outcome_wake_keeps_live_source() {
        use crate::test_support::test_session;
        let store = store();
        let source = test_session(Uuid::new_v4(), std::path::PathBuf::from("/tmp/queue-wake"));
        store.insert_session(&source).unwrap();
        insert_continuation(&store, source.id);
        assert_eq!(
            settle_wake_targets(&store, source.id),
            vec![Some(source.id)]
        );
    }

    /// Enqueue one source per `(session, commit)` and claim them as one batch.
    fn claimed_batch(store: &Store, sources: &[(Uuid, char)]) -> ClaimedQueueBatch {
        for (index, (session, commit)) in sources.iter().enumerate() {
            store
                .enqueue_rolling_queue_source(
                    &new_entry(*session, *commit, &format!("k{index}")),
                    Utc::now(),
                )
                .unwrap();
        }
        store
            .claim_rolling_queue_batch(Utc::now(), sources.len())
            .unwrap()
            .unwrap()
    }

    fn recorded_wake(store: &Store, entry: Uuid) -> Option<String> {
        store
            .conn
            .query_row(
                "SELECT wake_job_id FROM rolling_queue_entries WHERE id=?1",
                [entry.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn queue_wakes(store: &Store) -> Vec<ScheduledJob> {
        store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_three_entry_batch_from_one_owner_settles_with_one_wake_naming_all_three() {
        let store = store();
        let owner = Uuid::new_v4();
        let batch = claimed_batch(&store, &[(owner, 'a'), (owner, 'b'), (owner, 'c')]);
        let red = RollingQueueOutcome {
            refusal: Some("gate_failed".into()),
            failing_tests: vec!["rolling_queue::red_test".into()],
            ..RollingQueueOutcome::default()
        };
        let items: Vec<_> = batch
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| match index {
                2 => (entry.id, RollingQueueEntryState::Refused, red.clone()),
                _ => (
                    entry.id,
                    RollingQueueEntryState::Published,
                    published(&"9".repeat(40)),
                ),
            })
            .collect();
        let settled = store
            .settle_rolling_queue_entries(&items, Utc::now())
            .unwrap();
        assert_eq!(settled.len(), 3);
        let wakes = queue_wakes(&store);
        assert_eq!(wakes.len(), 1, "one wake per owner per batch settlement");
        let wake = &wakes[0];
        assert_eq!(wake.wake_session_id, Some(owner));
        assert_eq!(wake.wake_mode, WakeMode::Resume);
        assert!(settled.iter().all(|(_, wake_id)| *wake_id == wake.id));
        for (entry, commit) in batch.entries.iter().zip(['a', 'b', 'c']) {
            assert!(
                wake.message.contains(&commit.to_string().repeat(40)),
                "{}",
                wake.message
            );
            assert!(wake.message.contains(&entry.id.to_string()));
        }
        assert!(
            wake.message
                .contains(&format!("landed_sha={}", "9".repeat(40)))
        );
        assert!(wake.message.contains("not landed (gate_failed)"));
        assert!(
            wake.message
                .contains("failing_tests=rolling_queue::red_test")
        );
        // Per-entry outcome rows are unchanged: every entry holds its own.
        for (entry, (_, state, outcome)) in batch.entries.iter().zip(&items) {
            let row = store.get_rolling_queue_entry(entry.id).unwrap().unwrap();
            assert_eq!(row.state, *state);
            assert_eq!(row.outcome.as_ref(), Some(outcome));
        }
        // The lead entry records the wake; the batch settled with its members.
        assert_eq!(
            recorded_wake(&store, batch.entries[0].id),
            Some(wake.id.to_string())
        );
        assert_eq!(wake.name, format!("merge-queue-{}", batch.entries[0].id));
        let batch_state: String = store
            .conn
            .query_row(
                "SELECT state FROM rolling_queue_batches WHERE id=?1",
                [batch.batch_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(batch_state, "published");
        // An idempotent replay settles nothing and schedules nothing.
        let replay = store
            .settle_rolling_queue_entries(&items, Utc::now())
            .unwrap();
        assert!(replay.is_empty());
        assert_eq!(queue_wakes(&store).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn entries_from_two_owners_settled_together_wake_each_owner_once() {
        let store = store();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let batch = claimed_batch(
            &store,
            &[(first, 'a'), (second, 'b'), (first, 'c'), (second, 'd')],
        );
        let items: Vec<_> = batch
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.id,
                    RollingQueueEntryState::Published,
                    published(&"9".repeat(40)),
                )
            })
            .collect();
        store
            .settle_rolling_queue_entries(&items, Utc::now())
            .unwrap();
        let wakes = queue_wakes(&store);
        assert_eq!(wakes.len(), 2);
        let wake_for = |owner: Uuid| {
            wakes
                .iter()
                .find(|job| job.wake_session_id == Some(owner))
                .expect("each owner has a wake")
        };
        let (a, b) = (wake_for(first), wake_for(second));
        assert_ne!(a.id, b.id);
        assert_ne!(a.name, b.name);
        for commit in ['a', 'c'] {
            assert!(a.message.contains(&commit.to_string().repeat(40)));
        }
        for commit in ['b', 'd'] {
            assert!(b.message.contains(&commit.to_string().repeat(40)));
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_single_entry_batch_keeps_the_per_entry_wake_content() {
        let store = store();
        let owner = Uuid::new_v4();
        let batch = claimed_batch(&store, &[(owner, 'a')]);
        let entry = &batch.entries[0];
        let outcome = published(&"9".repeat(40));
        store
            .settle_rolling_queue_entries(
                &[(entry.id, RollingQueueEntryState::Published, outcome.clone())],
                Utc::now(),
            )
            .unwrap();
        let wakes = queue_wakes(&store);
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].message, wake_message(entry, &outcome));
        assert_eq!(wakes[0].name, format!("merge-queue-{}", entry.id));
        assert_eq!(
            recorded_wake(&store, entry.id),
            Some(wakes[0].id.to_string())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn separate_settle_calls_for_one_owner_still_wake_separately() {
        let store = store();
        let owner = Uuid::new_v4();
        let batch = claimed_batch(&store, &[(owner, 'a'), (owner, 'b')]);
        for entry in &batch.entries {
            store
                .settle_rolling_queue_entries(
                    &[(
                        entry.id,
                        RollingQueueEntryState::Published,
                        published(&"9".repeat(40)),
                    )],
                    Utc::now(),
                )
                .unwrap();
        }
        assert_eq!(queue_wakes(&store).len(), 2);
    }
}
