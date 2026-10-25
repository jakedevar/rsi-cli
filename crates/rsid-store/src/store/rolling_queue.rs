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
use rsi_common::harness_manager_v2::ManagerOperatingModeV2;
use rsi_common::rolling_queue::{
    AgentListRollingQueueRequestV1, AgentRollingQueueEntryV1, QUEUE_DUPLICATE_SOURCE,
    QUEUE_ENTRY_CANCELLED, QUEUE_ENTRY_IN_FLIGHT, QUEUE_ENTRY_NOT_FOUND, QUEUE_ENTRY_SETTLED,
    QUEUE_ENTRY_SUPERSEDED, QUEUE_EXPEDITE_CAP_REACHED, QUEUE_INVALID_REQUEST,
    QUEUE_SUPERSEDES_NOT_DESCENDANT, QueueFiltersState, ROLLING_QUEUE_MAX_EXPEDITED,
    RollingQaGreenV1, RollingQueueBinding, RollingQueueEntryState, RollingQueueEntryV1,
    RollingQueueOutcome, RollingQueueOutcomeSummaryV1,
};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};
use std::collections::BTreeSet;
use uuid::Uuid;

mod bounded_qa;
mod gate_base_red;
mod health;
pub use bounded_qa::{BoundedQaIntent, BoundedQaLanding, BoundedQaPlacement, BoundedQaRun};
pub use gate_base_red::{GateBaseRed, GateBaseRedIssue};

/// Bound on the `continued_from` walk for an archived source's wake target.
const QUEUE_WAKE_LINEAGE_LIMIT: usize = 16;

// The provisional number is assigned at landing.
#[cfg(test)]
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

/// The stored filter list of an entry: its filters and fix claims, then the
/// #2993 marker while the derivation is pending. #3264: while it is pending the
/// filters are the caller's extra ones alone (none when it named none); the
/// derivation's end merges the derived set into them.
fn stored_filters(filters: &[String], fixes: &[String], pending: bool) -> Vec<String> {
    let mut stored = rsi_common::rolling_queue::encode_fix_claims(filters, fixes);
    if pending {
        stored.push(rsi_common::rolling_queue::FILTERS_DERIVING_MARKER.to_owned());
    }
    stored
}

/// [`stored_filters`] for an entry whose derivation exceeded its budget (#3372):
/// the caller's filters and claims, then the lander-selects marker.
fn stored_filters_lander_selects(filters: &[String], fixes: &[String]) -> Vec<String> {
    let mut stored = rsi_common::rolling_queue::encode_fix_claims(filters, fixes);
    stored.push(rsi_common::rolling_queue::FILTERS_LANDER_SELECTS_MARKER.to_owned());
    stored
}

fn with_derived_filters(mut stored: Vec<String>, derived: &[String]) -> Vec<String> {
    stored.extend(derived.iter().map(|filter| {
        format!(
            "{}{filter}",
            rsi_common::rolling_queue::DERIVED_FILTER_PREFIX
        )
    }));
    stored
}

/// #648: `stored` with the marker that makes the entry resume the queue
/// position of the refused entry it replaces (that entry's own resumed
/// position when it was itself a resumed successor).
fn with_resumed_position(mut stored: Vec<String>, refused: Option<&EntryRow>) -> Vec<String> {
    if let Some(row) = refused {
        let position = row
            .resume
            .map_or(row.entry.sequence, |(position, _)| position);
        stored.push(rsi_common::rolling_queue::encode_resume_position(
            position,
            row.entry.id,
        ));
    }
    stored
}

/// #648: whether a descendant already took over the refused entry `prior`
/// (any state: the refused row cannot be marked, so the successor carries the
/// link and a second supersede of the same entry is settled).
fn resumed_successor_exists(tx: &Transaction<'_>, prior: Uuid) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM rolling_queue_entries, json_each(test_filters_json) AS j \
         WHERE substr(j.value, 1, 16) = 'resume_position=' AND substr(j.value, -36) = ?1)",
        [prior.to_string()],
        |row| row.get(0),
    )?)
}

/// #648: the refusal already woke the enqueuer, or its wake job has not fired
/// yet. The successor's settlement is the one wake from here on, so a refusal
/// wake that never fired is disabled and cannot fire after it.
fn silence_unfired_refusal_wake(
    tx: &Transaction<'_>,
    refused: &EntryRow,
    now: DateTime<Utc>,
) -> Result<()> {
    tx.execute(
        "UPDATE scheduled_jobs SET enabled=0, updated_at=?2 \
         WHERE id=(SELECT wake_job_id FROM rolling_queue_entries WHERE id=?1) \
           AND enabled=1 AND last_fired_at IS NULL",
        params![refused.entry.id.to_string(), stamp(now)],
    )?;
    Ok(())
}

/// #3487: `stored` with the expedite marker appended when the entry is expedited.
fn with_expedite(mut stored: Vec<String>, reason: Option<&str>) -> Vec<String> {
    if let Some(reason) = reason {
        stored.push(rsi_common::rolling_queue::encode_expedite(reason));
    }
    stored
}

/// #648: the queue position an entry is admitted at: the position it resumes
/// when it carries a `resume_position=` marker (a descendant of an entry
/// refused `queue_conflicts_with_rolling`), else its own `sequence`.
macro_rules! queue_position_sql {
    () => {
        "COALESCE((SELECT CAST(substr(j.value, 17) AS INTEGER) FROM json_each(test_filters_json) AS j \
         WHERE substr(j.value, 1, 16) = 'resume_position='), sequence)"
    };
}

/// #3487: the order the queue admits `queued` entries in: expedited entries
/// first, then everything else, each group in enqueue (`sequence`) order. One
/// expression for every query that picks the claim window, so the repository
/// probe, the lane candidates and the claim see the same order.
const QUEUED_ADMISSION_ORDER: &str = concat!(
    "(test_filters_json LIKE '%\"expedited=%') DESC, ",
    queue_position_sql!(),
    ", sequence"
);

/// #3487: how an enqueue asks to be expedited.
#[derive(Debug, Clone)]
pub struct ExpediteAdmission {
    /// Recorded on the entry.
    pub reason: String,
    /// The automatic `qa` rule rather than a manager's `expedite`: past the cap
    /// the entry stays ordinary instead of being refused.
    pub automatic: bool,
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
    /// #2167: the tests the landing claims to fix, stored with the filters as
    /// reserved `fixes_test=NAME` items (no schema change).
    pub fixes_tests: Vec<String>,
    pub idempotency_key: String,
}

/// The first N queued entries of one project's repository, claimed as one
/// batch.
#[derive(Debug, Clone)]
pub struct ClaimedQueueBatch {
    pub batch_id: Uuid,
    /// The head entry's checkout.
    pub repo_path: String,
    /// Each member's own checkout, parallel to `entries` (members of one
    /// repository come from different worktrees, #1945).
    pub repo_paths: Vec<String>,
    /// Admission order: expedited entries (#3487), then FIFO (enqueue) order.
    pub entries: Vec<RollingQueueEntryV1>,
    /// The batch was claimed while another batch was gating, because its
    /// packages are disjoint from every gating batch's (#1224).
    pub parallel_lane: bool,
}

/// The packages a queued source changes plus their reverse dependencies:
/// `Some(set)` when the daemon could resolve them, `None` when the source
/// touches (or may touch) the whole workspace or could not be read. A batch
/// is only ever claimed beside a gating batch when both sets are `Some` and
/// disjoint (#1224).
pub type LanePackages = Option<BTreeSet<String>>;

/// Most batches that gate at once, one per lane. A second lane exists only for
/// a batch whose packages are disjoint from the first's (#1224).
pub const ROLLING_QUEUE_MAX_LANES: usize = 2;

/// How many of the oldest queued entries one claim considers when it forms a
/// batch (an entry further back waits for a later claim).
pub const ROLLING_QUEUE_CLAIM_WINDOW: usize = 256;

/// #4007: most test filters one batch may sum before the claim stops adding
/// members. Recent lander logs: about 3.8k tests took 2400 s of gate wall time
/// (base and candidate runs) and about 1.3k took 1636 s, so 4000 keeps a batch
/// well under the default 60-minute `rolling_queue_gate_timeout_mins`; the batch
/// that timed out summed about 10k. The head entry is always admitted, so one
/// wide entry still gates alone.
pub const ROLLING_QUEUE_BATCH_FILTER_BUDGET: usize = 4000;

/// #4007: an expedited entry, or one that claims to fix known-red tests
/// (`fixes_issue` / `fixes_tests`), shares a gate only with entries whose
/// filter count is at most this, and itself gates alone when it is wider.
pub const ROLLING_QUEUE_PRIORITY_COMPANION_FILTERS: usize = 600;

/// #4007 (review #4027): how many filters an entry weighs against the batch
/// budget. Normally its filter count. An entry whose derivation went over
/// budget (`lander_selects`) and that carries no filters of its own is
/// unbounded: the lander selects its tests itself, and `union_filters` gives a
/// Rust-touching member with no filters a full affected gate for the whole
/// batch. It counts as infinitely wide, so it gates alone and nothing (an
/// expedited fix above all) is batched with it. A docs-only entry in that
/// state is treated the same: the store holds no diff, so it cannot tell, and
/// gating alone is the safe side.
fn batch_width(entry: &RollingQueueEntryV1) -> usize {
    if entry.filters_state == Some(QueueFiltersState::LanderSelects)
        && entry.test_filters.is_empty()
    {
        usize::MAX
    } else {
        entry.test_filters.len()
    }
}

/// #4007: whether `next` may join a batch already holding `members` (a
/// non-empty list; the head is always admitted). Admission order is kept: the
/// claim stops at the first entry that does not fit.
fn batch_admits(members: &[RollingQueueEntryV1], next: &RollingQueueEntryV1) -> bool {
    let priority = |entry: &RollingQueueEntryV1| {
        entry.expedite_reason.is_some() || !entry.fixes_tests.is_empty()
    };
    let width = batch_width;
    let total = members
        .iter()
        .chain([next])
        .fold(0usize, |sum, entry| sum.saturating_add(width(entry)));
    if total > ROLLING_QUEUE_BATCH_FILTER_BUDGET {
        return false;
    }
    if members.iter().chain([next]).any(priority) {
        return members
            .iter()
            .chain([next])
            .all(|entry| width(entry) <= ROLLING_QUEUE_PRIORITY_COMPANION_FILTERS);
    }
    true
}

/// One entry claimed for a run, with the batch that records it.
#[derive(Debug, Clone)]
pub struct ClaimedQueueEntry {
    pub entry: RollingQueueEntryV1,
    pub repo_path: String,
    pub batch_id: Uuid,
}

const ENTRY_COLUMNS: &str = "sequence, id, project_id, repo_path, source_commit, source_session_id, owner_epic_id, binding, work_key, migration_version, hot_files_json, test_filters_json, state, outcome_json, enqueued_at, finished_at, wake_session_id, batch_id, issue";

struct EntryRow {
    entry: RollingQueueEntryV1,
    /// #648: the `(position, refused entry)` this entry resumes.
    resume: Option<(i64, Uuid)>,
    repo_path: String,
    project_id: Option<String>,
    wake_session_id: Option<Uuid>,
    batch_id: Option<Uuid>,
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
    // #2167: fix claims are stored with the filters as reserved items; so is
    // the #2993 marker of a derivation that has not finished.
    let (stored, filters_state) = rsi_common::rolling_queue::split_filters_state(
        serde_json::from_str(&row.get::<_, String>(11)?).map_err(json_error)?,
    );
    let (stored, derived_test_filters) = rsi_common::rolling_queue::split_derived_filters(stored);
    let (stored, expedite_reason) = rsi_common::rolling_queue::split_expedite(stored);
    let (stored, resume) = rsi_common::rolling_queue::split_resume_position(stored);
    let (test_filters, fixes_tests) = rsi_common::rolling_queue::split_fix_claims(stored);
    Ok(EntryRow {
        resume,
        wake_session_id: row
            .get::<_, Option<String>>(16)?
            .as_deref()
            .map(parse_uuid)
            .transpose()?,
        batch_id: row
            .get::<_, Option<String>>(17)?
            .as_deref()
            .map(parse_uuid)
            .transpose()?,
        repo_path: row.get(3)?,
        project_id: row.get(2)?,
        entry: RollingQueueEntryV1 {
            issue: row.get(18)?,
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
            test_filters,
            derived_test_filters,
            fixes_tests,
            filters_state,
            expedite_reason,
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

/// The event kind that counts remote-read requeues of one entry (hub #3873).
const REMOTE_READ_REQUEUE_EVENT: &str = "requeued_remote_read";

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

/// #2386: whether the entry's idempotency key is the replay key of a topology land node
/// whose execution can still deliver a settled wake carrying the outcome. Such
/// an entry's outcome reaches the owner through the execution (the land node
/// reads the entry, then the execution settles and fires its one
/// `topology_execution_settled` wake), so the queue writes no wake of its own.
/// A final execution cannot record it, and neither can a `blocked` one (#2407):
/// `topology_execution_settled` fires on `blocked`, so that wake is already
/// delivered (or fires at once) without this entry's later outcome. In both
/// cases the owner is woken as for a hand-enqueued entry.
fn topology_execution_records(conn: &rusqlite::Connection, entry_id: Uuid) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM rolling_queue_entries q \
         JOIN topology_node_attempts a ON a.dedup_key=q.idempotency_key \
         JOIN topology_executions e ON e.id=a.execution_id \
         WHERE q.id=?1 AND q.idempotency_key LIKE 'topology.node:%' \
         AND a.node_kind='land' \
         AND e.status NOT IN ('succeeded','failed','cancelled','blocked'))",
        [entry_id.to_string()],
        |row| row.get(0),
    )?)
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
        self.enqueue_rolling_queue_source_for(new, None, now)
    }

    /// [`Self::enqueue_rolling_queue_source`] with an explicit settlement-wake
    /// target (#1641 S2). A topology land node queues a topology node session
    /// as the source (its replay identity); the outcome is delivered to the
    /// owning manager or Epic lead instead of resuming a finished node.
    pub fn enqueue_rolling_queue_source_for(
        &self,
        new: &NewQueueEntry,
        wake_session_id: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<(RollingQueueEntryV1, bool)> {
        self.enqueue_rolling_queue_source_superseding(new, wake_session_id, None, now)
    }

    /// Enqueue with an optional supersession proven by the daemon's git probe:
    /// `(entry id, probed source commit)`. Recheck owner, project, source and
    /// queued state under the same immediate transaction as admission. The
    /// replacement inherits the queue position and settlement-wake target;
    /// the old entry settles without waking the enqueuer.
    pub fn enqueue_rolling_queue_source_superseding(
        &self,
        new: &NewQueueEntry,
        wake_session_id: Option<Uuid>,
        supersedes: Option<(Uuid, &str)>,
        now: DateTime<Utc>,
    ) -> Result<(RollingQueueEntryV1, bool)> {
        self.enqueue_rolling_queue_source_linked(new, wake_session_id, supersedes, false, None, now)
    }

    /// [`Self::enqueue_rolling_queue_source_superseding`] for an entry whose
    /// filter derivation is still running (#2993): the entry is stored with
    /// `filters_state: deriving` and `new.test_filters` as its extra filters
    /// only (#3264: the derived set joins them at the derivation's end), and
    /// the lander admits nothing carrying that state. The
    /// derivation's end calls [`Self::finish_rolling_queue_filter_derivation`]
    /// or [`Self::refuse_rolling_queue_filter_derivation`]. A replay of the key
    /// returns the existing entry whatever its filters have become.
    pub fn enqueue_rolling_queue_source_deriving(
        &self,
        new: &NewQueueEntry,
        wake_session_id: Option<Uuid>,
        supersedes: Option<(Uuid, &str)>,
        now: DateTime<Utc>,
    ) -> Result<(RollingQueueEntryV1, bool)> {
        self.enqueue_rolling_queue_source_linked(new, wake_session_id, supersedes, true, None, now)
    }

    /// Admit a source with its optional project-local Issue display number.
    /// Existing callers omit the link; hand-enqueued sources pass it explicitly.
    pub fn enqueue_rolling_queue_source_linked(
        &self,
        new: &NewQueueEntry,
        wake_session_id: Option<Uuid>,
        supersedes: Option<(Uuid, &str)>,
        filters_pending: bool,
        issue: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<(RollingQueueEntryV1, bool)> {
        self.enqueue_rolling_queue_source_with_filter_provenance(
            new,
            wake_session_id,
            supersedes,
            filters_pending,
            issue,
            &[],
            now,
        )
    }

    /// Admit with the derived-only subset, excluding every caller-named filter.
    /// Persist it atomically so a restart cannot reinterpret handwritten filters.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_rolling_queue_source_with_filter_provenance(
        &self,
        new: &NewQueueEntry,
        wake_session_id: Option<Uuid>,
        supersedes: Option<(Uuid, &str)>,
        filters_pending: bool,
        issue: Option<i64>,
        derived_filters: &[String],
        now: DateTime<Utc>,
    ) -> Result<(RollingQueueEntryV1, bool)> {
        self.enqueue_rolling_queue_source_expediting(
            new,
            wake_session_id,
            supersedes,
            filters_pending,
            issue,
            derived_filters,
            None,
            now,
        )
    }

    /// [`Self::enqueue_rolling_queue_source_with_filter_provenance`] with an
    /// optional expedite (#3487). The cap of
    /// [`ROLLING_QUEUE_MAX_EXPEDITED`] live expedited entries per project is
    /// checked in the insert's own transaction, so two concurrent enqueues cannot
    /// both take the last slot. A manual expedite over the cap is refused
    /// `queue_expedite_cap_reached`, naming the live expedited entries; an
    /// automatic one is recorded as an ordinary entry (the caller reads that off
    /// the returned entry). The entry being superseded does not count against
    /// the cap: its replacement takes its place.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_rolling_queue_source_expediting(
        &self,
        new: &NewQueueEntry,
        wake_session_id: Option<Uuid>,
        supersedes: Option<(Uuid, &str)>,
        filters_pending: bool,
        issue: Option<i64>,
        derived_filters: &[String],
        expedite: Option<&ExpediteAdmission>,
        now: DateTime<Utc>,
    ) -> Result<(RollingQueueEntryV1, bool)> {
        // #655: caller fields cannot supply metadata, including on a pending
        // derivation replay. The store appends its own markers after this guard.
        if new
            .test_filters
            .iter()
            .chain(&new.fixes_tests)
            .any(|value| rsi_common::rolling_queue::is_reserved_queue_marker(value))
        {
            return Err(refused(QUEUE_INVALID_REQUEST));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = entry_by_key(&tx, new.source_session_id, &new.idempotency_key)? {
            // A derivation that was pending when the caller asked has no
            // filters to compare: the key replays the commit and the claims.
            let same = existing.source_commit == new.source_commit
                && (filters_pending || existing.test_filters == new.test_filters)
                && existing.fixes_tests == new.fixes_tests
                && existing.issue == issue;
            return if same {
                Ok((existing, true))
            } else {
                Err(refused("queue_idempotency_conflict"))
            };
        }
        if issue.is_some_and(|number| number < 1) || (issue.is_some() && new.project_id.is_none()) {
            return Err(refused(QUEUE_INVALID_REQUEST));
        }
        // The link names an Issue of the source's own project; a number that
        // resolves nowhere, or only in another project, is never stored.
        if let (Some(number), Some(project)) = (issue, new.project_id) {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM issues WHERE project_id=?1 AND display_number=?2)",
                params![project.to_string(), number],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(refused(&format!(
                    "{QUEUE_INVALID_REQUEST}: issue {number} is not an Issue of this project"
                )));
            }
        }
        let prior = if let Some((id, probed_commit)) = supersedes {
            let row = tx
                .query_row(
                    &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1"), // sql-dynamic-ok: static column list, id is a bound parameter
                    [id.to_string()],
                    map_entry,
                )
                .optional()?
                .ok_or_else(|| self.queue_entry_not_found_refusal(new.source_session_id, id))?;
            if !self.may_supersede_rolling_queue_entry(new.source_session_id, &row)?
                || row.project_id != new.project_id.map(|id| id.to_string())
            {
                return Err(self.queue_entry_not_found_refusal(new.source_session_id, id));
            }
            match row.entry.state {
                RollingQueueEntryState::Queued => {}
                RollingQueueEntryState::Admitted | RollingQueueEntryState::Gating => {
                    return Err(refused(&format!(
                        "{QUEUE_ENTRY_IN_FLIGHT}: enqueue normally without supersedes; the lander owns this entry"
                    )));
                }
                _ if rsi_common::rolling_queue::resumable_conflict_refusal(&row.entry, now) => {
                    // #648: a repeat supersede of the same refused entry is settled.
                    if resumed_successor_exists(&tx, row.entry.id)? {
                        return Err(refused(QUEUE_ENTRY_SETTLED));
                    }
                }
                _ => return Err(refused(QUEUE_ENTRY_SETTLED)),
            }
            if row.entry.source_commit != probed_commit {
                return Err(refused(QUEUE_SUPERSEDES_NOT_DESCENDANT));
            }
            Some(row)
        } else {
            None
        };
        let live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rolling_queue_entries \
             WHERE source_commit=?1 AND state IN ('queued','admitted','gating') AND (?2 IS NULL OR id!=?2))",
            params![new.source_commit, supersedes.map(|(id, _)| id.to_string())],
            |row| row.get(0),
        )?;
        if live {
            return Err(refused(QUEUE_DUPLICATE_SOURCE));
        }
        // #3487: the entry is expedited unless the project's cap is spent.
        let expedite_reason = match expedite {
            None => None,
            Some(request) => {
                let held = live_expedited_in(&tx, new.project_id)?
                    .into_iter()
                    .filter(|held| Some(held.id) != supersedes.map(|(id, _)| id))
                    .collect::<Vec<_>>();
                if held.len() < ROLLING_QUEUE_MAX_EXPEDITED {
                    Some(request.reason.as_str())
                } else if request.automatic {
                    None
                } else {
                    return Err(refused(&expedite_cap_detail(&held)));
                }
            }
        };
        // sequence is the existing FIFO key. Move the old row to a fresh key
        // before freezing it, then insert its replacement at the vacated key.
        // No row is deleted and a claim sees the whole swap or none of it.
        let id = Uuid::new_v4();
        // #648: a refused entry is terminal and immutable, so its successor takes
        // a fresh key and resumes the refused entry's position by marker.
        let resumed = prior
            .as_ref()
            .filter(|row| row.entry.state == RollingQueueEntryState::Refused);
        if let Some(refused_row) = resumed {
            silence_unfired_refusal_wake(&tx, refused_row, now)?;
        }
        if let Some(prior) = prior.as_ref().filter(|_| resumed.is_none()) {
            // #3358: the outcome names the successor entry so a land node that
            // waits on the old entry follows it instead of failing.
            let outcome = RollingQueueOutcome {
                refusal: Some(QUEUE_ENTRY_SUPERSEDED.into()),
                detail: Some(format!("replaced by {}", new.source_commit)), // sql-dynamic-ok: outcome prose, not SQL
                superseded_by: Some(id),
                ..RollingQueueOutcome::default()
            };
            tx.execute(
                "UPDATE rolling_queue_entries SET sequence=(SELECT MAX(sequence)+1 FROM rolling_queue_entries), \
                 state='superseded',outcome_json=?2,finished_at=?3,row_version=row_version+1 \
                 WHERE id=?1 AND state='queued'",
                params![prior.entry.id.to_string(), serde_json::to_string(&outcome).map_err(|e| DaemonError::Store(e.to_string()))?, stamp(now)],
            )?;
        }
        // hub #2969: an enqueuer's recorded owner can be an archived manager
        // seat (a topology land node names the execution's requester, which
        // predates a later rotation). Store the seat that holds that authority
        // now, the same resolution the settle-time wake applies, so the entry
        // never carries an archived wake target.
        let wake_session_id = prior.as_ref().map_or_else(
            || wake_session_id.map(|target| self.queue_wake_target(target)),
            |row| row.wake_session_id,
        );
        tx.execute(
            "INSERT INTO rolling_queue_entries(sequence, id, project_id, repo_path, source_commit, \
             source_session_id, owner_epic_id, binding, work_key, migration_version, \
             hot_files_json, test_filters_json, state, idempotency_key, enqueued_at, row_version, \
             wake_session_id, issue) \
             VALUES (?15,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'queued',?12,?13,1,?14,?16)",
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
                serde_json::to_string(&with_resumed_position(
                    with_expedite(
                        with_derived_filters(
                            stored_filters(&new.test_filters, &new.fixes_tests, filters_pending),
                            derived_filters,
                        ),
                        expedite_reason,
                    ),
                    resumed,
                ))
                .map_err(|e| DaemonError::Store(e.to_string()))?,
                new.idempotency_key,
                stamp(now),
                wake_session_id.map(|value| value.to_string()),
                prior
                    .as_ref()
                    .filter(|_| resumed.is_none())
                    .map(|row| row.entry.sequence),
                issue,
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

    /// #3487: the live expedited entries of `project` (what the automatic
    /// expedite's cap warning lists).
    pub fn live_expedited_rolling_queue_entries(
        &self,
        project: Option<Uuid>,
    ) -> Result<Vec<RollingQueueEntryV1>> {
        live_expedited_in(&self.conn, project)
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
                ..
            } = batch;
            entries.into_iter().next().map(|entry| ClaimedQueueEntry {
                entry,
                repo_path,
                batch_id,
            })
        }))
    }

    /// Claim the first `max` queued entries that share the head's project and
    /// checkout path, in admission order (expedited entries first, #3487, then
    /// FIFO by enqueue order), as one batch. See
    /// [`Self::claim_rolling_queue_batch_by`] for a repository identity wider
    /// than the checkout path.
    pub fn claim_rolling_queue_batch(
        &self,
        now: DateTime<Utc>,
        max: usize,
    ) -> Result<Option<ClaimedQueueBatch>> {
        self.claim_rolling_queue_batch_by(now, max, &|path| path.to_string())
    }

    /// Claim the first `max` queued entries of the head's project and
    /// repository, in admission order (expedited entries first, #3487, then FIFO
    /// by enqueue order), as one batch. `repository_of` names
    /// the repository a queued entry's checkout path belongs to: two entries
    /// batch together only when their project ids are equal and it returns the
    /// same key for both. Every Issue worker lands from its own sandbox
    /// worktree, so the checkout path alone never repeats across entries
    /// (#1945); the daemon keys by git common directory instead. It runs inside
    /// the claim's write transaction, so it must be a lookup, never a probe.
    /// Single-flight: nothing is claimed while another entry is `gating`
    /// (`claim_rolling_queue_batch_lanes` opens a second lane, #1224). A
    /// migration-carrying entry is only ever claimed together with, or after,
    /// every earlier entry of its repository, so migration numbers are assigned
    /// in queue order and never skip ahead of a predecessor. Only the oldest
    /// [`ROLLING_QUEUE_CLAIM_WINDOW`] queued entries in admission order are
    /// considered. An expedited entry (#3487) heads that order, so it is claimed
    /// by the next batch ahead of ordinary entries; a gating batch is never
    /// preempted, and a lane claim still needs its disjoint packages.
    pub fn claim_rolling_queue_batch_by(
        &self,
        now: DateTime<Utc>,
        max: usize,
        repository_of: &dyn Fn(&str) -> String,
    ) -> Result<Option<ClaimedQueueBatch>> {
        self.claim_rolling_queue_batch_lanes(now, max, repository_of, &|_| None)
    }

    /// [`Self::claim_rolling_queue_batch_by`] with lanes (#1224). Single-flight
    /// is one batch per lane: while a batch is `gating`, a second batch is
    /// claimed only when `packages_of` (source commit to its packages plus
    /// their reverse dependencies) resolves every gating source and the
    /// candidate's members, the resolved sets are disjoint from every gating
    /// batch's, the project and repository are the gating batches', and no
    /// migration-carrying entry is involved. A lane member must also be
    /// disjoint from every earlier queued entry it overtakes, so entries that
    /// share a package keep their FIFO order. At most
    /// [`ROLLING_QUEUE_MAX_LANES`] batches gate at once. A source `packages_of`
    /// cannot resolve (`None`) never shares the gate, so a lookup that always
    /// answers `None` is the plain single-flight claim. It runs inside the
    /// write transaction, so it must be a lookup, never a probe.
    pub fn claim_rolling_queue_batch_lanes(
        &self,
        now: DateTime<Utc>,
        max: usize,
        repository_of: &dyn Fn(&str) -> String,
        packages_of: &dyn Fn(&str) -> LanePackages,
    ) -> Result<Option<ClaimedQueueBatch>> {
        self.claim_rolling_queue_batch_lanes_held(now, max, repository_of, packages_of, &|_| false)
    }

    /// [`Self::claim_rolling_queue_batch_lanes`] that does not claim the first
    /// queued entry `held` names (a source commit), nor anything behind it
    /// (#3394): the daemon holds a wide entry until its compile probe has a
    /// verdict, and a later entry may not overtake it, because the probe judges
    /// the entry against the ones ahead of it. Entries ahead of the held one
    /// are claimed as usual. `held` runs inside the write transaction, so it
    /// must be a lookup, never a probe.
    pub fn claim_rolling_queue_batch_lanes_held(
        &self,
        now: DateTime<Utc>,
        max: usize,
        repository_of: &dyn Fn(&str) -> String,
        packages_of: &dyn Fn(&str) -> LanePackages,
        held: &dyn Fn(&str) -> bool,
    ) -> Result<Option<ClaimedQueueBatch>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if max == 0 {
            return Ok(None);
        }
        let mut gating = Vec::new();
        {
            let mut statement = tx.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
                 WHERE state='gating' ORDER BY sequence"
            ))?;
            for row in statement.query_map([], map_entry)? {
                gating.push(row?);
            }
        }
        // The packages every gating batch holds; `None` when a lane cannot be
        // opened beside them (nothing resolvable, a migration, or no free lane).
        let mut busy = BTreeSet::new();
        let lane_open = !gating.is_empty();
        if lane_open {
            let mut batches = BTreeSet::new();
            for row in &gating {
                let Some(batch) = row.batch_id else {
                    return Ok(None);
                };
                batches.insert(batch);
                if row.entry.migration_version.is_some() {
                    return Ok(None);
                }
                match packages_of(&row.entry.source_commit) {
                    Some(set) => busy.extend(set),
                    None => return Ok(None),
                }
            }
            if batches.len() >= ROLLING_QUEUE_MAX_LANES {
                return Ok(None);
            }
        }
        let mut window = Vec::new();
        {
            let mut statement = tx.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
                 WHERE state='queued' AND test_filters_json NOT LIKE '%\"filters_state=deriving\"%' \
                 ORDER BY {QUEUED_ADMISSION_ORDER} LIMIT ?1"
            ))?;
            let rows =
                statement.query_map(params![ROLLING_QUEUE_CLAIM_WINDOW as i64], map_entry)?;
            for row in rows {
                window.push(row?);
            }
        }
        if let Some(at) = window.iter().position(|row| held(&row.entry.source_commit)) {
            window.truncate(at);
        }
        let Some(head) = window.first() else {
            return Ok(None);
        };
        let mut head_path = head.repo_path.clone();
        let mut head_project = head.project_id.clone();
        let mut members = Vec::new();
        let mut repo_paths = Vec::new();
        let mut lane_packages = BTreeSet::new();
        if lane_open {
            let gating_family: Vec<(Option<String>, String)> = gating
                .iter()
                .map(|row| (row.project_id.clone(), repository_of(&row.repo_path)))
                .collect();
            // The first window entry that fits the lane anchors it: later
            // members must share its project and repository.
            let mut anchor: Option<(Option<String>, String)> = None;
            let mut overtaken = BTreeSet::new();
            let mut overtaken_whole = false;
            for row in window {
                if members.len() == max {
                    break;
                }
                let repository = repository_of(&row.repo_path);
                let packages = match row.entry.migration_version {
                    Some(_) => None,
                    None => packages_of(&row.entry.source_commit),
                };
                let fits = match &packages {
                    Some(set) => {
                        let same_family = |family: &(Option<String>, String)| {
                            family.0 == row.project_id && family.1 == repository
                        };
                        !overtaken_whole
                            && set.is_disjoint(&busy)
                            && set.is_disjoint(&overtaken)
                            && gating_family.iter().all(same_family)
                            && anchor.as_ref().is_none_or(same_family)
                    }
                    None => false,
                };
                let fits = fits && (members.is_empty() || batch_admits(&members, &row.entry));
                match packages {
                    Some(set) if fits => {
                        if anchor.is_none() {
                            head_path = row.repo_path.clone();
                            head_project = row.project_id.clone();
                            anchor = Some((row.project_id.clone(), repository));
                        }
                        lane_packages.extend(set);
                        repo_paths.push(row.repo_path);
                        members.push(row.entry);
                    }
                    Some(set) => overtaken.extend(set),
                    None => overtaken_whole = true,
                }
            }
        } else {
            let head_repository = repository_of(&head.repo_path);
            for row in window {
                if members.len() == max {
                    break;
                }
                if row.project_id == head_project
                    && repository_of(&row.repo_path) == head_repository
                {
                    if !members.is_empty() && !batch_admits(&members, &row.entry) {
                        break;
                    }
                    repo_paths.push(row.repo_path);
                    members.push(row.entry);
                }
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
                head_project,
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
        if lane_open {
            record_event(
                &tx,
                batch_id,
                "lane_claimed",
                None,
                Some(serde_json::json!({
                    "packages": lane_packages,
                    "beside": busy,
                })),
                now,
            )?;
        }
        tx.commit()?;
        Ok(Some(ClaimedQueueBatch {
            batch_id,
            repo_path: head_path,
            repo_paths,
            entries: members,
            parallel_lane: lane_open,
        }))
    }

    /// The distinct checkout paths of the oldest queued entries, by first
    /// appearance in FIFO order: what the daemon resolves to repositories
    /// before [`Self::claim_rolling_queue_batch_by`] (the same window).
    pub fn queued_rolling_queue_repo_paths(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(concat!(
            "SELECT repo_path FROM (SELECT repo_path, ",
            queue_position_sql!(),
            " AS position, \
                    (test_filters_json LIKE '%\"expedited=%') AS expedited \
             FROM rolling_queue_entries \
             WHERE state='queued' AND test_filters_json NOT LIKE '%\"filters_state=deriving\"%' \
                 ORDER BY expedited DESC, position LIMIT ?1) \
                 GROUP BY repo_path ORDER BY MAX(expedited) DESC, MIN(position)"
        ))?;
        let rows = statement.query_map([ROLLING_QUEUE_CLAIM_WINDOW as i64], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<String>, _>>()?)
    }

    /// The `(source commit, checkout path)` of every `gating` entry and of the
    /// first `queued_limit` queued entries in FIFO order: what the daemon
    /// resolves to package sets before
    /// [`Self::claim_rolling_queue_batch_lanes`] (#1224).
    pub fn rolling_queue_lane_candidates(
        &self,
        queued_limit: usize,
    ) -> Result<(Vec<(String, String)>, Vec<(String, String)>)> {
        fn rows(
            mut statement: rusqlite::Statement<'_>,
            limit: i64,
        ) -> Result<Vec<(String, String)>> {
            let rows = statement.query_map([limit], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        }
        let gating = rows(
            self.conn.prepare(
                "SELECT source_commit, repo_path FROM rolling_queue_entries \
                 WHERE state='gating' ORDER BY sequence LIMIT ?1",
            )?,
            ROLLING_QUEUE_CLAIM_WINDOW as i64,
        )?;
        let queued = rows(
            self.conn.prepare(&format!(
                "SELECT source_commit, repo_path FROM rolling_queue_entries \
                 WHERE state='queued' AND test_filters_json NOT LIKE '%\"filters_state=deriving\"%' \
                 ORDER BY {QUEUED_ADMISSION_ORDER} LIMIT ?1"
            ))?,
            queued_limit.min(ROLLING_QUEUE_CLAIM_WINDOW) as i64,
        )?;
        Ok((gating, queued))
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

    /// #3480: how many daemon restarts have already found `id` `gating` and
    /// re-driven it (`requeued_after_restart` events, one per entry per
    /// restart). The crash-loop breaker reads it before re-gating again.
    pub fn rolling_queue_restart_requeues(&self, id: Uuid) -> Result<u32> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM rolling_queue_events \
             WHERE kind='requeued_after_restart' AND entry_id=?1",
            [id.to_string()],
            |row| row.get(0),
        )?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
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

    /// Return members of a still-open batch to `queued` (#3267): a bisect that
    /// ran out of wall time proved these members were not the cause of the red,
    /// so they are neither refused nor re-gated by this batch. Each keeps its
    /// `sequence`, so it is claimed again at its original FIFO position. Only
    /// an entry that is `gating` in `batch_id` moves; the batch row is left
    /// open unless this was its last open member (it then settles exactly as
    /// the last `settle_rolling_queue_entries` would). One `requeued_after_bisect`
    /// event per entry carries `detail`. Returns the entries returned.
    pub fn requeue_bisected_rolling_queue_entries(
        &self,
        batch_id: Uuid,
        ids: &[Uuid],
        detail: &serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<Vec<Uuid>> {
        self.requeue_open_batch_entries(batch_id, ids, "requeued_after_bisect", detail, now)
    }

    /// Hub #3873: return members of a still-open batch to `queued` because the
    /// lander could not read the remote `rolling` ref. It is infrastructure, not a
    /// verdict on the sources, so each keeps its `sequence` (its FIFO position)
    /// and one `requeued_remote_read` event per entry carries `detail`; the
    /// events are what [`Self::rolling_queue_remote_read_requeues`] counts to
    /// bound the retries. Returns the entries returned.
    pub fn requeue_remote_read_rolling_queue_entries(
        &self,
        batch_id: Uuid,
        ids: &[Uuid],
        detail: &serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<Vec<Uuid>> {
        self.requeue_open_batch_entries(batch_id, ids, REMOTE_READ_REQUEUE_EVENT, detail, now)
    }

    /// Hub #3873: how many times `id` was returned to the queue because the
    /// remote `rolling` ref could not be read.
    pub fn rolling_queue_remote_read_requeues(&self, id: Uuid) -> Result<u32> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM rolling_queue_events \
             WHERE kind='requeued_remote_read' AND entry_id=?1",
            [id.to_string()],
            |row| row.get(0),
        )?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    fn requeue_open_batch_entries(
        &self,
        batch_id: Uuid,
        ids: &[Uuid],
        event_kind: &str,
        detail: &serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<Vec<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut returned = Vec::new();
        for id in ids {
            let changed = tx.execute(
                "UPDATE rolling_queue_entries SET state='queued', batch_id=NULL, \
                 admitted_at=NULL, row_version=row_version+1 \
                 WHERE id=?1 AND state='gating' AND batch_id=?2",
                params![id.to_string(), batch_id.to_string()],
            )?;
            if changed == 1 {
                record_event(
                    &tx,
                    batch_id,
                    event_kind,
                    Some(*id),
                    Some(detail.clone()),
                    now,
                )?;
                returned.push(*id);
            }
        }
        if !returned.is_empty() {
            let batch = batch_id.to_string();
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
                    params![batch, if landed { "published" } else { "refused" }, stamp(now)],
                )?;
            }
        }
        tx.commit()?;
        Ok(returned)
    }

    /// CAS `gating -> terminal` and insert the owner's single wake in the same
    /// transaction (none for an entry a running topology execution records, #2386; a blocked one does not, #2407).
    /// Returns the wake job id (a marker id when no wake is written), or `None`
    /// when the entry was not `gating` (already settled): nothing is written then.
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
            // #2386: a live topology execution records this outcome itself
            // (the land node reads the entry) and settles on its own single
            // `topology_execution_settled` wake, so no owner wake is written.
            if topology_execution_records(&tx, entry.id)? {
                let changed = tx.execute(
                    "UPDATE rolling_queue_entries SET state=?2, outcome_json=?3, \
                     finished_at=?4, row_version=row_version+1 WHERE id=?1 AND state='gating'",
                    params![
                        id.to_string(),
                        terminal.as_str(),
                        serde_json::to_string(outcome)
                            .map_err(|e| DaemonError::Store(e.to_string()))?,
                        stamp(now)
                    ],
                )?;
                if changed == 1 {
                    // No job exists for this entry; the id only marks it settled.
                    settled.push((*id, Uuid::new_v4()));
                    self.settle_batch_bookkeeping(&tx, *id, terminal.as_str(), outcome, now)?;
                    super::friction::note_lander_friction_in(&tx, &entry, *terminal, outcome, now);
                    self.note_published_issue_results(
                        row.project_id.as_deref(),
                        &entry,
                        *terminal,
                        outcome,
                    );
                }
                continue;
            }
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
            self.note_published_issue_results(
                row.project_id.as_deref(),
                &entry,
                *terminal,
                outcome,
            );
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

    /// Hub #2911: a `published` entry signals the project manager about each
    /// Open or InProgress Issue whose latest handoff names the published SHA.
    fn note_published_issue_results(
        &self,
        project_id: Option<&str>,
        entry: &RollingQueueEntryV1,
        terminal: RollingQueueEntryState,
        outcome: &RollingQueueOutcome,
    ) {
        if terminal != RollingQueueEntryState::Published {
            return;
        }
        let Some(project_id) = project_id.and_then(|id| Uuid::parse_str(id).ok()) else {
            return;
        };
        self.note_issue_result_landed_in_tx(project_id, entry, outcome);
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

    /// The entry a `(source session, replay key)` pair already created, if
    /// any (#1641 S2: a land node adopts it after a restart).
    pub fn get_rolling_queue_entry_by_key(
        &self,
        source_session_id: Uuid,
        idempotency_key: &str,
    ) -> Result<Option<RollingQueueEntryV1>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
                     WHERE source_session_id=?1 AND idempotency_key=?2"
                ),
                params![source_session_id.to_string(), idempotency_key],
                map_entry,
            )
            .optional()?
            .map(|row| row.entry))
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

    /// `(repo_path, source_commit)` of queue entry `id` (#3369: the daemon reads
    /// the commits a publication put on rolling from that repository).
    pub fn rolling_queue_entry_repo_and_source(
        &self,
        id: Uuid,
    ) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT repo_path, source_commit FROM rolling_queue_entries WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// The batch that gated queue entry `id`, or `None` before it was claimed
    /// (#2570: it names the gate's log file).
    pub fn rolling_queue_entry_batch_id(&self, id: Uuid) -> Result<Option<Uuid>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1"),
                [id.to_string()],
                map_entry,
            )
            .optional()?
            .and_then(|row| row.batch_id))
    }

    /// Live entries linked to an Issue in this project, FIFO and bounded for
    /// topology admission warnings. A terminal entry no longer warns.
    pub fn live_rolling_queue_entries_for_issue(
        &self,
        project: Uuid,
        issue: i64,
    ) -> Result<Vec<AgentRollingQueueEntryV1>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
             WHERE project_id=?1 AND issue=?2 AND state IN ('queued','admitted','gating') \
             ORDER BY sequence LIMIT 32"
        ))?;
        let rows = statement.query_map(params![project.to_string(), issue], map_entry)?;
        rows.map(|row| row.map(|row| agent_view(&row)).map_err(DaemonError::from))
            .collect()
    }

    /// #2118: the `(id, sequence, source_commit)` of every live entry
    /// (`queued`, `admitted`, `gating`) of `project_id`, oldest first: the
    /// sources a newly enqueued one would be merged after. Bounded by `limit`.
    pub fn live_rolling_queue_sources(
        &self,
        project_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<(Uuid, i64, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT id, sequence, source_commit FROM rolling_queue_entries \
             WHERE state IN ('queued','admitted','gating') AND project_id IS ?1 \
             ORDER BY sequence LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![project_id.map(|value| value.to_string()), limit as i64],
            |row| {
                let id: String = row.get(0)?;
                Ok((id, row.get::<_, i64>(1)?, row.get::<_, String>(2)?))
            },
        )?;
        let mut sources = Vec::new();
        for row in rows {
            let (id, sequence, commit) = row?;
            if let Ok(id) = Uuid::parse_str(&id) {
                sources.push((id, sequence, commit));
            }
        }
        Ok(sources)
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

    /// What `caller` may do to queue entries now (#1964), read in the open
    /// transaction so a seat rotation committed before it is already seen.
    ///
    /// - The project's current appointed manager (the live tip of the
    ///   appointment, so a retired predecessor holds nothing) reads and
    ///   cancels every entry of its project.
    /// - A portfolio seat reads every project of its grant and cancels in
    ///   them while its policy executes (the `AgentEnqueueLandingSource`
    ///   rule).
    /// - Any other session holds only the entries whose outcome it would
    ///   receive: `COALESCE(wake_session_id, source_session_id)`. A manager
    ///   seat transfer (#1604) moves that column, so the successor inherits it.
    fn rolling_queue_scope(&self, caller: Uuid) -> Result<QueueScope> {
        let mut scope = QueueScope::default();
        let project = self.get_session(caller)?.and_then(|row| row.project_id);
        if let Some(project) = project
            && self
                .get_harness_manager(project)?
                .is_some_and(|config| config.current_session_id == Some(caller))
        {
            scope.read.push(project);
            scope.cancel.push(project);
        }
        if let Some(grant) = self.portfolio_seat_grant(caller)? {
            let executes = grant.project_policy.mode == ManagerOperatingModeV2::Execute
                && !grant.project_policy.paused;
            for project in grant.project_ids {
                if !scope.read.contains(&project) {
                    scope.read.push(project);
                }
                if executes && !scope.cancel.contains(&project) {
                    scope.cancel.push(project);
                }
            }
        }
        Ok(scope)
    }

    /// `AgentCheckLandingCandidate` (#2310): `(job id, owner session)` of every
    /// job whose id starts with `hex_prefix` (the id's lowercase hex digits
    /// without hyphens), newest first, at most `limit`. The caller has validated
    /// the prefix as hex and scopes the rows; the verb then reads the one job
    /// with `get_agent_job`. Kept out of `agent_jobs.rs`, which holds the
    /// released job catalog.
    pub fn agent_job_owners_by_id_prefix(
        &self,
        hex_prefix: &str,
        limit: usize,
    ) -> Result<Vec<(Uuid, Uuid)>> {
        let mut statement = self.conn.prepare(
            "SELECT id, owner_session_id FROM agent_jobs \
             WHERE substr(replace(id, '-', ''), 1, length(?1)) = ?1 \
             ORDER BY sequence DESC LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![hex_prefix, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, owner)| {
                Some((Uuid::parse_str(&id).ok()?, Uuid::parse_str(&owner).ok()?))
            })
            .collect())
    }

    /// The `queue_entry_not_found` refusal of a cancel or supersede (#3331).
    /// The code stays one for "no such entry" and "outside your scope" (the
    /// verb never confirms an entry the caller cannot see), but the text now
    /// says what was checked and, when an entry the caller CAN see shares the
    /// first eight characters of the id, names it: a mistyped or truncated id
    /// was the commonest cause and was otherwise impossible to tell from a
    /// scope refusal. Only entries inside the caller's own scope are named.
    pub fn queue_entry_not_found_refusal(&self, caller: Uuid, id: Uuid) -> DaemonError {
        let mut text = format!(
            "{QUEUE_ENTRY_NOT_FOUND}: no queue entry {id} is in your scope \
             (it does not exist, or it belongs to another project or session \
             that you may not act on). Copy the full entry id from \
             AgentListRollingQueue (`id`, not `sequence`). Scope checks \
             that did not match: {}",
            self.failed_scope_checks(caller)
        );
        if let Ok(similar) = self.in_scope_entries_sharing_id_prefix(caller, id)
            && !similar.is_empty()
        {
            text.push_str("; did you mean ");
            text.push_str(&similar.join(", "));
            text.push('?');
        }
        refused(&text)
    }

    /// The scope checks the caller's authority could pass, each named with
    /// its own verdict (#3392). A scope refusal and a missing entry read the
    /// same, so this describes only the caller, never the entry.
    fn failed_scope_checks(&self, caller: Uuid) -> String {
        let project = self
            .get_session(caller)
            .ok()
            .flatten()
            .and_then(|row| row.project_id);
        let seat = match project {
            None => "project manager seat: this session has no project".to_owned(),
            Some(project) => {
                let current = self
                    .get_harness_manager(project)
                    .ok()
                    .flatten()
                    .and_then(|config| config.current_session_id);
                if current == Some(caller) {
                    format!("project manager seat: yes, but only for project {project}")
                } else {
                    format!(
                        "project manager seat: this session is not the current seat of \
                         project {project}"
                    )
                }
            }
        };
        let grant = match self.portfolio_seat_grant(caller) {
            Ok(Some(grant)) => format!(
                "portfolio grant: covers {} project(s), the entry is in none of them",
                grant.project_ids.len()
            ),
            _ => "portfolio grant: none".to_owned(),
        };
        format!(
            "{seat}; {grant}; enqueuer or outcome-wake holder: this session is neither \
             for the entry"
        )
    }

    fn in_scope_entries_sharing_id_prefix(&self, caller: Uuid, id: Uuid) -> Result<Vec<String>> {
        let scope = self.rolling_queue_scope(caller)?;
        let projects = serde_json::json!(
            scope
                .read
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<String>>()
        )
        .to_string();
        let prefix = id.to_string()[..8].to_string();
        let mut statement = self.conn.prepare(
            "SELECT id, sequence, state FROM rolling_queue_entries \
             WHERE substr(id, 1, 8)=?1 AND id<>?2 \
               AND (project_id IN (SELECT value FROM json_each(?3)) \
                    OR COALESCE(wake_session_id, source_session_id)=?4) \
             ORDER BY sequence DESC LIMIT 3",
        )?;
        let rows = statement
            .query_map(
                params![prefix, id.to_string(), projects, caller.to_string()],
                |row| {
                    Ok(format!(
                        "{} (sequence {}, {})",
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn may_cancel_rolling_queue_entry(&self, caller: Uuid, row: &EntryRow) -> Result<bool> {
        let scope = self.rolling_queue_scope(caller)?;
        let owner = row.wake_session_id.unwrap_or(row.entry.source_session_id);
        let in_project = row
            .project_id
            .as_deref()
            .and_then(|project| Uuid::parse_str(project).ok())
            .is_some_and(|project| scope.cancel.contains(&project));
        Ok(in_project || owner == caller)
    }

    /// Who may replace a queued entry: the session that enqueued it, the
    /// session its outcome wakes (a manager seat transfer moves that column to
    /// the successor), or the project's current manager seat when the enqueuer
    /// is an earlier seat of that same manager lineage. Project-wide cancel
    /// scope is deliberately not enough: it would let a manager replace an Epic
    /// lead's or another worker's entry.
    fn may_supersede_rolling_queue_entry(&self, caller: Uuid, row: &EntryRow) -> Result<bool> {
        if row.entry.source_session_id == caller || row.wake_session_id == Some(caller) {
            return Ok(true);
        }
        let Some(project) = row
            .project_id
            .as_deref()
            .and_then(|project| Uuid::parse_str(project).ok())
        else {
            return Ok(false);
        };
        if self
            .get_harness_manager(project)?
            .and_then(|config| config.current_session_id)
            != Some(caller)
        {
            return Ok(false);
        }
        Ok(
            super::harness_manager::manager_lineage_tip_on(&self.conn, row.entry.source_session_id)
                .ok()
                == Some(caller),
        )
    }

    /// Supersession authority is [`Self::may_supersede_rolling_queue_entry`].
    /// Admission rechecks it transactionally.
    pub fn get_rolling_queue_entry_for_supersession(
        &self,
        caller: Uuid,
        id: Uuid,
    ) -> Result<Option<RollingQueueEntryV1>> {
        let row = self
            .conn
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1"),
                [id.to_string()],
                map_entry,
            )
            .optional()?;
        match row {
            Some(row) if self.may_supersede_rolling_queue_entry(caller, &row)? => {
                Ok(Some(row.entry))
            }
            _ => Ok(None),
        }
    }

    /// `AgentListRollingQueue` (#1964): the caller's scope of the queue,
    /// newest first, at most `limit` entries (one more is read to tell whether
    /// an older page exists). The default filters before paging to live entries
    /// and refusals/failures finished in the last hour, with compact outcomes.
    /// Full or state-filtered reads preserve the historical output.
    ///
    /// # Errors
    /// `manager_project_not_in_scope` when `project` is neither inside the
    /// caller's manager or portfolio scope nor the caller's own project.
    pub fn list_rolling_queue_for_agent(
        &self,
        caller: Uuid,
        request: &AgentListRollingQueueRequestV1,
    ) -> Result<(Vec<AgentRollingQueueEntryV1>, Option<i64>)> {
        self.list_rolling_queue_for_agent_at(caller, request, Utc::now())
    }

    fn list_rolling_queue_for_agent_at(
        &self,
        caller: Uuid,
        request: &AgentListRollingQueueRequestV1,
        now: DateTime<Utc>,
    ) -> Result<(Vec<AgentRollingQueueEntryV1>, Option<i64>)> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let limit = request.page_size();
        let project = request.project_id;
        let full = request.full_output();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let scope = self.rolling_queue_scope(caller)?;
        if let Some(project) = project
            && !scope.read.contains(&project)
            && self.get_session(caller)?.and_then(|row| row.project_id) != Some(project)
        {
            return Err(DaemonError::InvalidParam(
                rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE.into(),
            ));
        }
        let projects = serde_json::json!(
            scope
                .read
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<String>>()
        )
        .to_string();
        let mut statement = tx.prepare(&format!(
            "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
             WHERE (project_id IN (SELECT value FROM json_each(?1)) \
                    OR COALESCE(wake_session_id, source_session_id)=?2) \
               AND (?3 IS NULL OR state=?3) \
               AND (?4 IS NULL OR project_id=?4) \
               AND (?5 IS NULL OR sequence<?5) \
               AND (?7 OR state IN ('queued','admitted','gating') \
                    OR (state IN ('refused','failed') AND finished_at>=?8)) \
             ORDER BY sequence DESC LIMIT ?6"
        ))?;
        let rows = statement.query_map(
            params![
                projects,
                caller.to_string(),
                request.state.map(RollingQueueEntryState::as_str),
                project.map(|value| value.to_string()),
                request.before_sequence,
                (limit + 1) as i64,
                full,
                (now - chrono::Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Nanos, true),
            ],
            map_entry,
        )?;
        let mut entries = Vec::new();
        for row in rows {
            let mut entry = agent_view(&row?);
            if !full && let Some(outcome) = &mut entry.outcome {
                outcome.clip_for_default();
            }
            entries.push(entry);
        }
        let next = (entries.len() > limit).then(|| entries[limit - 1].sequence);
        entries.truncate(limit);
        Ok((entries, next))
    }

    /// The latest `rolling` commit a full-library QA run proved green (#2646)
    /// among the queue entries `caller` may read (narrowed to `project`): the
    /// newest passing `bounded_qa_finished` receipt. A receipt passes only when
    /// the run tested every landing it covered, so this is the SHA the
    /// automatic full QA vouches for, with no pointer file to land. `project`
    /// is checked by [`Self::list_rolling_queue_for_agent`], which the verb
    /// calls first.
    pub fn rolling_qa_green_for_agent(
        &self,
        caller: Uuid,
        project: Option<Uuid>,
    ) -> Result<Option<RollingQaGreenV1>> {
        let scope = self.rolling_queue_scope(caller)?;
        let projects = serde_json::json!(
            scope
                .read
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<String>>()
        )
        .to_string();
        let found = self
            .conn
            .query_row(
                "SELECT json_extract(e.detail_json, '$.sha'), e.at, \
                        json_extract(e.detail_json, '$.job_id') \
                 FROM rolling_queue_events e \
                 JOIN rolling_queue_entries q ON q.id = e.entry_id \
                 WHERE e.kind = 'bounded_qa_finished' \
                   AND json_extract(e.detail_json, '$.passed') = 1 \
                   AND (q.project_id IN (SELECT value FROM json_each(?1)) \
                        OR COALESCE(q.wake_session_id, q.source_session_id) = ?2) \
                   AND (?3 IS NULL OR q.project_id = ?3) \
                 ORDER BY e.id DESC LIMIT 1",
                params![
                    projects,
                    caller.to_string(),
                    project.map(|value| value.to_string()),
                ],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(found.and_then(|(sha, passed_at, job)| {
            Some(RollingQaGreenV1 {
                sha: sha?,
                passed_at,
                job_id: job.and_then(|job| Uuid::parse_str(&job).ok()),
            })
        }))
    }

    /// `AgentCancelRollingQueueEntry` (#1964): compare-and-swap one `queued`
    /// entry to `superseded` (outcome refusal `queue_entry_cancelled`). An
    /// admitted or gating entry belongs to the lander and is refused; a
    /// settled one has nothing to cancel; an entry already cancelled returns
    /// as a replay. When the entry's outcome would have woken someone other
    /// than the caller, that session gets one wake naming the cancel, in the
    /// same transaction. Returns the entry and whether it was a replay.
    ///
    /// # Errors
    /// `queue_entry_not_found` (missing or outside the caller's scope, one
    /// code for both), `queue_entry_in_flight`, `queue_entry_settled`.
    pub fn cancel_rolling_queue_entry_for_agent(
        &self,
        caller: Uuid,
        id: Uuid,
        reason: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(AgentRollingQueueEntryV1, bool)> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1"),
                [id.to_string()],
                map_entry,
            )
            .optional()?
            .ok_or_else(|| self.queue_entry_not_found_refusal(caller, id))?;
        let owner = row.wake_session_id.unwrap_or(row.entry.source_session_id);
        if !self.may_cancel_rolling_queue_entry(caller, &row)? {
            return Err(self.queue_entry_not_found_refusal(caller, id));
        }
        match row.entry.state {
            RollingQueueEntryState::Queued => {}
            RollingQueueEntryState::Admitted | RollingQueueEntryState::Gating => {
                return Err(refused(QUEUE_ENTRY_IN_FLIGHT));
            }
            RollingQueueEntryState::Superseded
                if row
                    .entry
                    .outcome
                    .as_ref()
                    .and_then(|outcome| outcome.refusal.as_deref())
                    == Some(QUEUE_ENTRY_CANCELLED) =>
            {
                return Ok((agent_view(&row), true));
            }
            _ => return Err(refused(QUEUE_ENTRY_SETTLED)),
        }
        let outcome = RollingQueueOutcome {
            landed_sha: None,
            refusal: Some(QUEUE_ENTRY_CANCELLED.to_owned()),
            failing_tests: Vec::new(),
            detail: Some(match reason {
                Some(reason) => format!("cancelled by {caller}: {reason}"),
                None => format!("cancelled by {caller}"),
            }),
            superseded_by: None,
            published_commits: Vec::new(),
        };
        let target = self.queue_wake_target(owner);
        let wake_id = (target != caller).then(Uuid::new_v4);
        let changed = tx.execute(
            "UPDATE rolling_queue_entries SET state='superseded', outcome_json=?2, \
             wake_job_id=?3, finished_at=?4, row_version=row_version+1 \
             WHERE id=?1 AND state='queued'",
            params![
                id.to_string(),
                serde_json::to_string(&outcome).map_err(|e| DaemonError::Store(e.to_string()))?,
                wake_id.map(|wake| wake.to_string()),
                stamp(now),
            ],
        )?;
        if changed != 1 {
            // Unreachable under the immediate transaction; a claim that won
            // anyway means the lander has it now.
            return Err(refused(QUEUE_ENTRY_IN_FLIGHT));
        }
        if let Some(wake_id) = wake_id {
            insert_scheduled_job_conn(
                &tx,
                &ScheduledJob {
                    id: wake_id,
                    name: format!("merge-queue-cancel-{id}"),
                    message: wake_message(&row.entry, &outcome),
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
                    wake_session_id: Some(target),
                },
            )?;
        }
        let cancelled = tx
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1"),
                [id.to_string()],
                map_entry,
            )
            .map_err(|_| refused(QUEUE_INVALID_REQUEST))?;
        tx.commit()?;
        Ok((agent_view(&cancelled), false))
    }

    /// The entries recorded with their filter derivation still pending
    /// (#2993), oldest first: `(id, checkout path, source commit)`. What the
    /// daemon re-attaches a derivation to after a restart.
    pub fn rolling_queue_entries_deriving_filters(&self) -> Result<Vec<(Uuid, String, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT id, repo_path, source_commit FROM rolling_queue_entries \
             WHERE state='queued' AND test_filters_json LIKE '%\"filters_state=deriving\"%' \
             ORDER BY sequence LIMIT ?1",
        )?;
        let rows = statement.query_map([ROLLING_QUEUE_CLAIM_WINDOW as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
        })?;
        let mut pending = Vec::new();
        for row in rows {
            let (id, repo, commit) = row?;
            pending.push((parse_uuid(&id)?, repo, commit));
        }
        Ok(pending)
    }

    /// The derivation of a pending entry finished (#2993): store `filters` in
    /// place of the marker so the lander can admit it. A CAS on the entry still
    /// being `queued` with the marker, so an entry cancelled, superseded or
    /// already finished meanwhile is left alone. Returns whether it applied.
    pub fn finish_rolling_queue_filter_derivation(
        &self,
        id: Uuid,
        filters: &[String],
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(row) = tx
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1 AND state='queued'"),
                [id.to_string()],
                map_entry,
            )
            .optional()?
        else {
            return Ok(false);
        };
        if row.entry.filters_state != Some(QueueFiltersState::Deriving) {
            return Ok(false);
        }
        // #3264: the filters the caller named stay; the derived set joins them.
        let merged =
            rsi_common::rolling_queue::merge_filters(filters.to_vec(), &row.entry.test_filters);
        let derived: Vec<String> = filters
            .iter()
            .filter(|filter| !row.entry.test_filters.contains(filter))
            .cloned()
            .collect();
        let stored = serde_json::to_string(&with_expedite(
            with_derived_filters(
                stored_filters(&merged, &row.entry.fixes_tests, false),
                &derived,
            ),
            row.entry.expedite_reason.as_deref(),
        ))
        .map_err(|e| DaemonError::Store(e.to_string()))?;
        let changed = tx.execute(
            "UPDATE rolling_queue_entries SET test_filters_json=?2, row_version=row_version+1 \
             WHERE id=?1 AND state='queued'",
            params![id.to_string(), stored],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// #3372: the filter derivation of `id` exceeded its budget (it timed out,
    /// or the diff is too wide to derive in time). Instead of refusing the
    /// entry, keep it with no derived filters: the caller's own filters stay,
    /// the marker is replaced by `lander_selects`, so the lander admits it and
    /// selects the tests itself (its bounded closure selection plus the
    /// automatic full-library QA). A CAS on the entry still being `queued`:
    /// `from_deriving` expects the pending marker (the background path);
    /// otherwise the entry must carry no filters-state marker yet (the inline
    /// path, right after the insert). Returns whether it applied.
    pub fn mark_rolling_queue_filters_lander_selects(
        &self,
        id: Uuid,
        from_deriving: bool,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(row) = tx
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1 AND state='queued'"),
                [id.to_string()],
                map_entry,
            )
            .optional()?
        else {
            return Ok(false);
        };
        let expected = from_deriving.then_some(QueueFiltersState::Deriving);
        if row.entry.filters_state != expected {
            return Ok(false);
        }
        let stored = serde_json::to_string(&with_expedite(
            stored_filters_lander_selects(&row.entry.test_filters, &row.entry.fixes_tests),
            row.entry.expedite_reason.as_deref(),
        ))
        .map_err(|e| DaemonError::Store(e.to_string()))?;
        let changed = tx.execute(
            "UPDATE rolling_queue_entries SET test_filters_json=?2, row_version=row_version+1 \
             WHERE id=?1 AND state='queued'",
            params![id.to_string(), stored],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// The derivation of a pending entry failed (#2993): settle it `refused`
    /// with `refusal` and `detail`, and wake its enqueuer once, like any other
    /// refusal of the queue. A CAS on the entry still being `queued` with the
    /// marker, so a second call, or one for an entry the enqueuer cancelled,
    /// writes nothing and returns `None`. Otherwise the wake job id.
    pub fn refuse_rolling_queue_filter_derivation(
        &self,
        id: Uuid,
        refusal: &str,
        detail: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>> {
        self.refuse_queued_rolling_queue_entry(id, None, true, refusal, detail, now)
    }

    /// #3293: settle a still-`queued` entry `refused` before any batch claims
    /// it, because its source (`expected_commit`) conflicts with the rolling
    /// tip, and wake its enqueuer once. The same CAS as the derivation refusal
    /// (the entry must still be `queued`, so a claimed, cancelled, superseded
    /// or already refused entry writes nothing) plus the source commit the
    /// caller probed, so a superseding entry on the same id can never be
    /// refused for another commit's conflict. Returns the wake job id.
    pub fn refuse_queued_rolling_queue_entry_conflicting_with_rolling(
        &self,
        id: Uuid,
        expected_commit: &str,
        detail: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>> {
        self.refuse_queued_rolling_queue_entry(
            id,
            Some(expected_commit),
            false,
            rsi_common::rolling_queue::QUEUE_CONFLICTS_WITH_ROLLING,
            detail,
            now,
        )
    }

    /// #3346: [`Self::refuse_queued_rolling_queue_entry_conflicting_with_rolling`]
    /// with the refusal `code` the sweep settles it under (a declared
    /// provisional migration the lander could not prove is
    /// `provisional_unprovable`), under the same CAS and the one wake.
    pub fn refuse_queued_rolling_queue_entry_for_source(
        &self,
        id: Uuid,
        expected_commit: &str,
        code: &str,
        detail: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>> {
        self.refuse_queued_rolling_queue_entry(id, Some(expected_commit), false, code, detail, now)
    }

    fn refuse_queued_rolling_queue_entry(
        &self,
        id: Uuid,
        expected_commit: Option<&str>,
        require_deriving: bool,
        refusal: &str,
        detail: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(row) = tx
            .query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries WHERE id=?1 AND state='queued'"),
                [id.to_string()],
                map_entry,
            )
            .optional()?
        else {
            return Ok(None);
        };
        if (require_deriving && row.entry.filters_state != Some(QueueFiltersState::Deriving))
            || expected_commit.is_some_and(|commit| row.entry.source_commit != commit)
        {
            return Ok(None);
        }
        let outcome = RollingQueueOutcome {
            landed_sha: None,
            refusal: Some(refusal.to_owned()),
            failing_tests: Vec::new(),
            detail: Some(detail.to_owned()),
            superseded_by: None,
            published_commits: Vec::new(),
        };
        let owner = row.wake_session_id.unwrap_or(row.entry.source_session_id);
        let target = self.queue_wake_target(owner);
        let wake_id = Uuid::new_v4();
        let changed = tx.execute(
            "UPDATE rolling_queue_entries SET state='refused', outcome_json=?2, wake_job_id=?3, \
             finished_at=?4, row_version=row_version+1 WHERE id=?1 AND state='queued'",
            params![
                id.to_string(),
                serde_json::to_string(&outcome).map_err(|e| DaemonError::Store(e.to_string()))?,
                wake_id.to_string(),
                stamp(now),
            ],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        insert_scheduled_job_conn(
            &tx,
            &ScheduledJob {
                id: wake_id,
                name: format!("merge-queue-{id}"),
                message: wake_message(&row.entry, &outcome),
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
                wake_session_id: Some(target),
            },
        )?;
        super::friction::note_lander_friction_in(
            &tx,
            &row.entry,
            RollingQueueEntryState::Refused,
            &outcome,
            now,
        );
        tx.commit()?;
        Ok(Some(wake_id))
    }
}

/// #3487: the live (`queued`, `admitted` or `gating`) expedited entries of
/// `project`, in admission order.
fn live_expedited_in(
    tx: &rusqlite::Connection,
    project: Option<Uuid>,
) -> Result<Vec<RollingQueueEntryV1>> {
    let mut statement = tx.prepare(&format!(
        "SELECT {ENTRY_COLUMNS} FROM rolling_queue_entries \
         WHERE state IN ('queued','admitted','gating') AND project_id IS ?1 \
           AND test_filters_json LIKE '%\"expedited=%' ORDER BY sequence"
    ))?;
    let rows = statement.query_map(params![project.map(|id| id.to_string())], map_entry)?;
    let mut live = Vec::new();
    for row in rows {
        let entry = row?.entry;
        if entry.expedite_reason.is_some() {
            live.push(entry);
        }
    }
    Ok(live)
}

/// #3487: the `queue_expedite_cap_reached` refusal text: the code, the cap, then
/// each live expedited entry with its state, source and recorded reason.
#[must_use]
pub fn expedite_cap_detail(live: &[RollingQueueEntryV1]) -> String {
    let listed = live
        .iter()
        .map(|entry| {
            format!(
                "entry {} (queue #{}, {}, source {}): {}",
                entry.id,
                entry.sequence,
                entry.state.as_str(),
                &entry.source_commit[..entry.source_commit.len().min(9)],
                entry.expedite_reason.as_deref().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "{QUEUE_EXPEDITE_CAP_REACHED}: the project already holds {} live expedited entries (the cap is {ROLLING_QUEUE_MAX_EXPEDITED}): {listed}",
        live.len()
    )
}

/// The queue entries a session may act on (see [`Store::rolling_queue_scope`]).
#[derive(Default)]
struct QueueScope {
    read: Vec<Uuid>,
    cancel: Vec<Uuid>,
}

/// The bounded agent-facing view of one entry.
fn agent_view(row: &EntryRow) -> AgentRollingQueueEntryV1 {
    let entry = &row.entry;
    AgentRollingQueueEntryV1 {
        issue: entry.issue,
        id: entry.id,
        sequence: entry.sequence,
        state: entry.state,
        source_commit: entry.source_commit.clone(),
        project_id: row
            .project_id
            .as_deref()
            .and_then(|project| Uuid::parse_str(project).ok()),
        enqueuer_session_id: entry.source_session_id,
        wake_session_id: row.wake_session_id,
        owner_epic_id: entry.owner_epic_id,
        batch_id: row.batch_id,
        migration_version: entry.migration_version,
        test_filter_count: u32::try_from(entry.test_filters.len()).unwrap_or(u32::MAX),
        expedited: entry.expedite_reason.is_some(),
        expedite_reason: entry.expedite_reason.clone(),
        outcome: entry.outcome.as_ref().map(RollingQueueOutcomeSummaryV1::of),
        enqueued_at: entry.enqueued_at.clone(),
        finished_at: entry.finished_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    /// The project the helper's entries share: only entries of one project
    /// batch together (#1945).
    fn shared_project() -> Uuid {
        Uuid::from_u128(0x1945)
    }

    fn new_entry(session: Uuid, commit: char, key: &str) -> NewQueueEntry {
        NewQueueEntry {
            project_id: Some(shared_project()),
            repo_path: "/tmp/repo".into(),
            source_commit: commit.to_string().repeat(40),
            source_session_id: session,
            owner_epic_id: Some(Uuid::new_v4()),
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: Some(141),
            hot_files: vec!["crates/rsid/src/rpc.rs".into()],
            test_filters: vec!["rsid=rolling_queue".into()],
            fixes_tests: Vec::new(),
            idempotency_key: key.into(),
        }
    }

    /// An Issue of `project`, created through the store (the project row is
    /// added when the test has not made one). Returns its display number.
    fn open_issue(store: &Store, project: Uuid) -> i64 {
        let _ = store.insert_project(&rsi_common::types::Project {
            id: project,
            name: format!("queue project {project}"),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        store
            .create_issue(&rsi_common::types::NewIssue {
                project_id: project,
                title: "queue link".into(),
                body: String::new(),
                priority: None,
                labels: Vec::new(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap()
            .display_number
    }

    fn published(sha: &str) -> RollingQueueOutcome {
        RollingQueueOutcome {
            landed_sha: Some(sha.into()),
            ..RollingQueueOutcome::default()
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn issue_link_migration_replays_without_changing_existing_entries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("queue-upgrade.sqlite");
        let store = Store::open_test_fixture(&path).unwrap();
        let new = new_entry(Uuid::new_v4(), 'a', "legacy-entry");
        let before = store
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap()
            .0;
        crate::store::tests::rewind_post_v121_tail_to(&store.conn, 185 - 1);
        let catalog = crate::store::catalog_sql_by_name(&store.conn);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_rolling_queue_entry(before.id).unwrap().unwrap(),
            before
        );
        crate::store::assert_catalog_preserved_across_replay(&catalog, &store.conn, 185 - 1);
        assert_eq!(list(&store, new.source_session_id)[0].issue, None);
        let number = open_issue(&store, shared_project());
        let linked = new_entry(new.source_session_id, 'b', "after-upgrade");
        let after = store
            .enqueue_rolling_queue_source_linked(
                &linked,
                None,
                None,
                false,
                Some(number),
                Utc::now(),
            )
            .unwrap()
            .0;
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened
                .get_rolling_queue_entry(after.id)
                .unwrap()
                .unwrap()
                .issue,
            Some(number)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn linked_issue_roundtrips_lists_and_is_part_of_the_replay_identity() {
        let store = store();
        let number = open_issue(&store, shared_project());
        let owner = Uuid::new_v4();
        let new = new_entry(owner, 'a', "linked");
        let (entry, replayed) = store
            .enqueue_rolling_queue_source_linked(&new, None, None, false, Some(number), Utc::now())
            .unwrap();
        assert!(!replayed);
        assert_eq!(entry.issue, Some(number));
        assert_eq!(
            store
                .get_rolling_queue_entry(entry.id)
                .unwrap()
                .unwrap()
                .issue,
            Some(number)
        );
        assert_eq!(list(&store, owner)[0].issue, Some(number));
        let (replay, replayed) = store
            .enqueue_rolling_queue_source_linked(&new, None, None, false, Some(number), Utc::now())
            .unwrap();
        assert!(replayed);
        assert_eq!(replay.id, entry.id);
        for different in [None, Some(number + 1)] {
            assert_eq!(
                code_of(
                    store
                        .enqueue_rolling_queue_source_linked(
                            &new,
                            None,
                            None,
                            false,
                            different,
                            Utc::now(),
                        )
                        .unwrap_err()
                ),
                "queue_idempotency_conflict"
            );
        }
        assert_eq!(
            store
                .live_rolling_queue_entries_for_issue(shared_project(), number)
                .unwrap()[0]
                .id,
            entry.id
        );
        assert!(
            store
                .live_rolling_queue_entries_for_issue(Uuid::new_v4(), number)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .live_rolling_queue_entries_for_issue(shared_project(), number + 1)
                .unwrap()
                .is_empty()
        );
        cancel(&store, owner, entry.id).unwrap();
        assert!(
            store
                .live_rolling_queue_entries_for_issue(shared_project(), number)
                .unwrap()
                .is_empty()
        );
        let old = store
            .enqueue_rolling_queue_source(&new_entry(owner, 'b', "unlinked"), Utc::now())
            .unwrap()
            .0;
        assert_eq!(old.issue, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn linked_issue_must_be_an_issue_of_the_source_project() {
        let store = store();
        let own = open_issue(&store, shared_project());
        let other_project = Uuid::new_v4();
        let foreign = open_issue(&store, other_project);
        let owner = Uuid::new_v4();
        // An unknown number, and a number that exists only in another
        // project, are refused and leave no row behind.
        for (key, number) in [("unknown", own + 1000), ("foreign", foreign)] {
            let error = store
                .enqueue_rolling_queue_source_linked(
                    &new_entry(owner, 'a', key),
                    None,
                    None,
                    false,
                    Some(number),
                    Utc::now(),
                )
                .unwrap_err();
            assert!(code_of(error).starts_with(QUEUE_INVALID_REQUEST), "{key}");
        }
        assert!(list(&store, owner).is_empty());
        let (entry, _) = store
            .enqueue_rolling_queue_source_linked(
                &new_entry(owner, 'a', "own"),
                None,
                None,
                false,
                Some(own),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(entry.issue, Some(own));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn supersession_is_limited_to_the_enqueuer_its_wake_target_and_the_managers_lineage() {
        use crate::store::manager_coordinator::tests::fixture;
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let project = config.project_id;
        let lead = Uuid::new_v4();
        let supersede = |caller: Uuid, entry: &RollingQueueEntryV1, commit: char, key: &str| {
            let mut replacement = new_entry(caller, commit, key);
            replacement.project_id = Some(project);
            store.enqueue_rolling_queue_source_superseding(
                &replacement,
                None,
                Some((entry.id, &entry.source_commit)),
                Utc::now(),
            )
        };

        // The project's manager holds cancel scope over a lead's entry, but
        // that is not authority to replace it.
        let leads = enqueue_in(&store, project, lead, 'a', "lead-entry");
        assert!(
            store
                .get_rolling_queue_entry_for_supersession(seat, leads.id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            code_of(supersede(seat, &leads, 'b', "seat-replaces-lead").unwrap_err()),
            QUEUE_ENTRY_NOT_FOUND
        );
        assert_eq!(
            store
                .get_rolling_queue_entry(leads.id)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Queued
        );
        assert!(
            cancel(&store, seat, leads.id).is_ok(),
            "cancel scope is unchanged"
        );

        // The session an entry's outcome wakes may replace it.
        let mut woken = new_entry(lead, 'c', "wakes-seat");
        woken.project_id = Some(project);
        let (woken, _) = store
            .enqueue_rolling_queue_source(&woken, Utc::now())
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET wake_session_id=?2 WHERE id=?1",
                params![woken.id.to_string(), seat.to_string()],
            )
            .unwrap();
        assert!(
            store
                .get_rolling_queue_entry_for_supersession(seat, woken.id)
                .unwrap()
                .is_some()
        );
        assert!(supersede(seat, &woken, 'd', "seat-replaces-woken").is_ok());

        // The enqueuer replaces its own entry.
        let own = enqueue_in(&store, project, lead, 'e', "lead-own");
        assert!(supersede(lead, &own, 'f', "lead-replaces-own").is_ok());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn supersession_preserves_fifo_replays_and_wakes_only_the_replacement() {
        let store = store();
        let owner = Uuid::new_v4();
        let wake_target = Uuid::new_v4();
        let old = store
            .enqueue_rolling_queue_source_for(
                &new_entry(owner, 'a', "old"),
                Some(wake_target),
                Utc::now(),
            )
            .unwrap()
            .0;
        let later = store
            .enqueue_rolling_queue_source(&new_entry(owner, 'b', "later"), Utc::now())
            .unwrap()
            .0;
        let replacement = new_entry(owner, 'c', "replacement");
        let (new, replayed) = store
            .enqueue_rolling_queue_source_superseding(
                &replacement,
                None,
                Some((old.id, &old.source_commit)),
                Utc::now(),
            )
            .unwrap();
        assert!(!replayed);
        assert_eq!(new.sequence, old.sequence);
        assert!(new.sequence < later.sequence);
        let retired = store.get_rolling_queue_entry(old.id).unwrap().unwrap();
        assert_eq!(retired.state, RollingQueueEntryState::Superseded);
        assert!(retired.finished_at.is_some());
        // #3358: the retired entry names its successor so a land node follows it.
        let outcome = retired.outcome.unwrap();
        assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_ENTRY_SUPERSEDED));
        assert_eq!(outcome.superseded_by, Some(new.id));
        assert!(retired.sequence > later.sequence);
        assert!(queue_wakes(&store).is_empty());
        let replay = store
            .enqueue_rolling_queue_source_superseding(
                &replacement,
                None,
                Some((old.id, &old.source_commit)),
                Utc::now(),
            )
            .unwrap();
        assert!(replay.1);
        assert_eq!(replay.0.id, new.id);
        assert_eq!(
            store
                .get_rolling_queue_entry_by_key(owner, "old")
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Superseded
        );

        // A second replacement retains the original position, and automatic
        // sequence allocation continues beyond every retained historical row.
        let next = store
            .enqueue_rolling_queue_source_superseding(
                &new_entry(owner, 'd', "next"),
                None,
                Some((new.id, &new.source_commit)),
                Utc::now(),
            )
            .unwrap()
            .0;
        assert_eq!(next.sequence, old.sequence);
        let tail = store
            .enqueue_rolling_queue_source(&new_entry(owner, 'e', "tail"), Utc::now())
            .unwrap()
            .0;
        assert!(
            tail.sequence
                > store
                    .get_rolling_queue_entry(new.id)
                    .unwrap()
                    .unwrap()
                    .sequence
        );
        let claimed = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        assert_eq!(claimed.entry.id, next.id);
        store
            .settle_rolling_queue_entry(
                next.id,
                RollingQueueEntryState::Published,
                &published(&"f".repeat(40)),
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        assert!(
            store
                .settle_rolling_queue_entry(
                    next.id,
                    RollingQueueEntryState::Published,
                    &published(&"f".repeat(40)),
                    Utc::now()
                )
                .unwrap()
                .is_none()
        );
        let wakes = queue_wakes(&store);
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].wake_session_id, Some(wake_target));
        assert!(wakes[0].message.contains(&next.source_commit));
        assert_eq!(
            store
                .claim_next_rolling_queue_entry(Utc::now())
                .unwrap()
                .unwrap()
                .entry
                .id,
            later.id
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn supersession_refuses_in_flight_terminal_foreign_and_stale_sources() {
        for state in [
            "admitted",
            "gating",
            "published",
            "refused",
            "failed",
            "superseded",
        ] {
            let store = store();
            let owner = Uuid::new_v4();
            let old = store
                .enqueue_rolling_queue_source(&new_entry(owner, 'a', "old"), Utc::now())
                .unwrap()
                .0;
            store
                .conn
                .execute(
                    "UPDATE rolling_queue_entries SET state=?2 WHERE id=?1",
                    params![old.id.to_string(), state],
                )
                .unwrap();
            let error = store
                .enqueue_rolling_queue_source_superseding(
                    &new_entry(owner, 'b', "new"),
                    None,
                    Some((old.id, &old.source_commit)),
                    Utc::now(),
                )
                .unwrap_err();
            let text = code_of(error);
            if matches!(state, "admitted" | "gating") {
                assert!(text.starts_with(QUEUE_ENTRY_IN_FLIGHT));
                assert!(text.contains("enqueue normally without supersedes"));
            } else {
                assert_eq!(text, QUEUE_ENTRY_SETTLED);
            }
            assert_eq!(
                store
                    .get_rolling_queue_entry(old.id)
                    .unwrap()
                    .unwrap()
                    .state
                    .as_str(),
                state
            );
            assert!(
                store
                    .get_rolling_queue_entry_by_key(owner, "new")
                    .unwrap()
                    .is_none()
            );
        }
        let store = store();
        let owner = Uuid::new_v4();
        let old = store
            .enqueue_rolling_queue_source(&new_entry(owner, 'a', "old"), Utc::now())
            .unwrap()
            .0;
        let mut new = new_entry(Uuid::new_v4(), 'b', "new");
        assert_eq!(
            code_of(
                store
                    .enqueue_rolling_queue_source_superseding(
                        &new,
                        None,
                        Some((old.id, &old.source_commit)),
                        Utc::now()
                    )
                    .unwrap_err()
            ),
            QUEUE_ENTRY_NOT_FOUND
        );
        new.source_session_id = owner;
        new.project_id = Some(Uuid::new_v4());
        assert_eq!(
            code_of(
                store
                    .enqueue_rolling_queue_source_superseding(
                        &new,
                        None,
                        Some((old.id, &old.source_commit)),
                        Utc::now()
                    )
                    .unwrap_err()
            ),
            QUEUE_ENTRY_NOT_FOUND
        );
        new.project_id = Some(shared_project());
        assert_eq!(
            code_of(
                store
                    .enqueue_rolling_queue_source_superseding(
                        &new,
                        None,
                        Some((old.id, &"f".repeat(40))),
                        Utc::now()
                    )
                    .unwrap_err()
            ),
            QUEUE_SUPERSEDES_NOT_DESCENDANT
        );
        assert_eq!(
            code_of(
                store
                    .enqueue_rolling_queue_source_superseding(
                        &new,
                        None,
                        Some((Uuid::new_v4(), &old.source_commit)),
                        Utc::now()
                    )
                    .unwrap_err()
            ),
            QUEUE_ENTRY_NOT_FOUND
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn supersession_rolls_back_when_replacement_insertion_fails() {
        let store = store();
        let owner = Uuid::new_v4();
        let old = store
            .enqueue_rolling_queue_source(&new_entry(owner, 'a', "old"), Utc::now())
            .unwrap()
            .0;
        let mut new = new_entry(owner, 'b', "new");
        new.repo_path.clear(); // CHECK failure after the old entry was moved.
        assert!(
            store
                .enqueue_rolling_queue_source_superseding(
                    &new,
                    None,
                    Some((old.id, &old.source_commit)),
                    Utc::now()
                )
                .is_err()
        );
        let retained = store.get_rolling_queue_entry(old.id).unwrap().unwrap();
        assert_eq!(retained.state, RollingQueueEntryState::Queued);
        assert_eq!(retained.sequence, old.sequence);
        assert!(retained.outcome.is_none());
        assert!(queue_wakes(&store).is_empty());
        assert!(
            store
                .get_rolling_queue_entry_by_key(owner, "new")
                .unwrap()
                .is_none()
        );
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
    fn derived_filter_provenance_survives_store_reload_and_legacy_stays_strict() {
        let store = store();
        let new = new_entry(Uuid::new_v4(), 'a', "derived-provenance");
        let (entry, _) = store
            .enqueue_rolling_queue_source_with_filter_provenance(
                &new,
                None,
                None,
                false,
                None,
                &new.test_filters,
                Utc::now(),
            )
            .unwrap();
        let loaded = store.get_rolling_queue_entry(entry.id).unwrap().unwrap();
        assert_eq!(loaded.derived_test_filters, new.test_filters);
        assert_eq!(loaded.test_filters, new.test_filters);
        let legacy = new_entry(Uuid::new_v4(), 'b', "legacy-provenance");
        let (legacy, _) = store
            .enqueue_rolling_queue_source(&legacy, Utc::now())
            .unwrap();
        assert_eq!(legacy.derived_test_filters, Vec::<String>::new());
    }

    /// #3264: the extra filters a caller names beside `derive_filters` are kept
    /// on the pending entry and the derived set joins them (never replaces them)
    /// when the derivation lands; the lander still admits nothing meanwhile.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_pending_derivation_keeps_the_extra_filters_and_the_derived_set_joins_them_3264() {
        use rsi_common::rolling_queue::QueueFiltersState;
        let store = store();
        let mut new = new_entry(Uuid::new_v4(), 'a', "k-extra");
        new.test_filters = vec!["rsi-common=test:manifest_dir_guard".to_owned()];
        new.fixes_tests = vec!["manifest_dir_guard::guard".to_owned()];
        let (entry, _) = store
            .enqueue_rolling_queue_source_deriving(&new, None, None, Utc::now())
            .unwrap();
        assert_eq!(entry.filters_state, Some(QueueFiltersState::Deriving));
        assert_eq!(entry.test_filters, new.test_filters, "the extras are kept");
        assert!(
            store
                .claim_rolling_queue_batch(Utc::now(), 4)
                .unwrap()
                .is_none(),
            "a pending entry is never admitted, extras or not"
        );
        let derived = vec![
            "rsid=test:e2e_seed".to_owned(),
            "rsi-common=test:manifest_dir_guard".to_owned(),
        ];
        assert!(
            store
                .finish_rolling_queue_filter_derivation(entry.id, &derived)
                .unwrap()
        );
        let done = store.get_rolling_queue_entry(entry.id).unwrap().unwrap();
        assert_eq!(done.filters_state, None);
        assert_eq!(
            done.test_filters, derived,
            "derived plus extra, no duplicate"
        );
        assert_eq!(
            done.derived_test_filters,
            vec!["rsid=test:e2e_seed".to_owned()]
        );
        assert_eq!(done.fixes_tests, new.fixes_tests);
    }

    /// #2993: an entry recorded with its filters pending carries
    /// `filters_state: deriving` and no filters, is never claimed, replays by
    /// key whatever its filters have become, takes its filters once (a CAS), and
    /// is then claimed with them.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_pending_filter_derivation_is_never_claimed_and_finishes_once_2993() {
        use rsi_common::rolling_queue::QueueFiltersState;
        let store = store();
        let session = Uuid::new_v4();
        let mut new = new_entry(session, 'a', "k-deriving");
        new.test_filters.clear();
        let (entry, replayed) = store
            .enqueue_rolling_queue_source_deriving(&new, None, None, Utc::now())
            .unwrap();
        assert!(!replayed);
        assert_eq!(entry.filters_state, Some(QueueFiltersState::Deriving));
        assert!(entry.test_filters.is_empty(), "the filters are pending");
        assert_eq!(
            store
                .rolling_queue_entries_deriving_filters()
                .unwrap()
                .len(),
            1
        );
        assert!(store.queued_rolling_queue_repo_paths().unwrap().is_empty());
        assert!(store.rolling_queue_lane_candidates(8).unwrap().1.is_empty());
        assert!(
            store
                .claim_rolling_queue_batch(Utc::now(), 4)
                .unwrap()
                .is_none(),
            "the lander admits nothing until the filters land"
        );

        // A pending entry in front does not hold up a later, final one.
        let (later, _) = store
            .enqueue_rolling_queue_source(&new_entry(session, 'b', "k-final"), Utc::now())
            .unwrap();
        let claimed = store
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .expect("the entry with final filters is admitted");
        assert_eq!(claimed.entries.len(), 1);
        assert_eq!(claimed.entries[0].id, later.id);

        let (again, replayed) = store
            .enqueue_rolling_queue_source_deriving(&new, None, None, Utc::now())
            .unwrap();
        assert!(replayed);
        assert_eq!(again.id, entry.id);

        let filters = vec!["rsid=rolling_queue".to_string()];
        assert!(
            store
                .finish_rolling_queue_filter_derivation(entry.id, &filters)
                .unwrap()
        );
        assert!(
            !store
                .finish_rolling_queue_filter_derivation(entry.id, &["rsid=other".to_string()])
                .unwrap(),
            "the filters land once"
        );
        let done = store.get_rolling_queue_entry(entry.id).unwrap().unwrap();
        assert_eq!(done.filters_state, None);
        assert_eq!(done.test_filters, filters);
        // The derivation outlived the first call: the replay with its filters
        // now final is still the same entry.
        let (replay_after, replayed) = store
            .enqueue_rolling_queue_source_deriving(&new, None, None, Utc::now())
            .unwrap();
        assert!(replayed);
        assert_eq!(replay_after.id, entry.id);
        assert!(
            store
                .rolling_queue_entries_deriving_filters()
                .unwrap()
                .is_empty()
        );
    }

    /// #2993: a derivation that fails settles the pending entry `refused` with
    /// its typed reason and writes the enqueuer's one wake; a second settle, a
    /// late finish and a cancelled entry write nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_failed_filter_derivation_refuses_the_entry_with_one_wake_2993() {
        let store = store();
        let session = Uuid::new_v4();
        let (entry, _) = store
            .enqueue_rolling_queue_source_deriving(
                &new_entry(session, 'a', "k-fail"),
                Some(session),
                None,
                Utc::now(),
            )
            .unwrap();
        let wake = store
            .refuse_rolling_queue_filter_derivation(
                entry.id,
                "queue_filter_derivation_failed",
                "the checker timed out",
                Utc::now(),
            )
            .unwrap();
        assert!(wake.is_some());
        let refused = store.get_rolling_queue_entry(entry.id).unwrap().unwrap();
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        assert_eq!(
            refused
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.refusal.as_deref()),
            Some("queue_filter_derivation_failed")
        );
        assert_eq!(queue_wakes(&store).len(), 1);
        assert_eq!(
            store
                .refuse_rolling_queue_filter_derivation(entry.id, "x", "y", Utc::now())
                .unwrap(),
            None
        );
        assert!(
            !store
                .finish_rolling_queue_filter_derivation(entry.id, &[])
                .unwrap()
        );
        assert_eq!(queue_wakes(&store).len(), 1, "settled once, woken once");
        assert!(
            store
                .claim_rolling_queue_batch(Utc::now(), 4)
                .unwrap()
                .is_none()
        );
    }

    /// #3293: a queued entry whose source conflicts with the rolling tip is
    /// refused `queue_conflicts_with_rolling` with one wake; an entry already
    /// claimed, a different commit and a second call write nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_queued_entry_conflicting_with_rolling_is_refused_once_3293() {
        let store = store();
        let session = Uuid::new_v4();
        let (entry, _) = store
            .enqueue_rolling_queue_source(&new_entry(session, 'a', "k-conflict"), Utc::now())
            .unwrap();
        let commit = entry.source_commit.clone();
        assert_eq!(
            store
                .refuse_queued_rolling_queue_entry_conflicting_with_rolling(
                    entry.id,
                    &"f".repeat(40),
                    "other commit",
                    Utc::now(),
                )
                .unwrap(),
            None,
            "a verdict about another commit never refuses this entry"
        );
        assert_eq!(queue_wakes(&store).len(), 0);
        let wake = store
            .refuse_queued_rolling_queue_entry_conflicting_with_rolling(
                entry.id,
                &commit,
                "conflict_paths=a.rs,b.rs",
                Utc::now(),
            )
            .unwrap();
        assert!(wake.is_some());
        let refused = store.get_rolling_queue_entry(entry.id).unwrap().unwrap();
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        let outcome = refused.outcome.expect("outcome");
        assert_eq!(
            outcome.refusal.as_deref(),
            Some("queue_conflicts_with_rolling")
        );
        assert_eq!(outcome.detail.as_deref(), Some("conflict_paths=a.rs,b.rs"));
        assert_eq!(queue_wakes(&store).len(), 1);
        assert_eq!(
            store
                .refuse_queued_rolling_queue_entry_conflicting_with_rolling(
                    entry.id,
                    &commit,
                    "again",
                    Utc::now(),
                )
                .unwrap(),
            None
        );
        assert_eq!(queue_wakes(&store).len(), 1, "settled once, woken once");

        // A claimed (gating) entry is never refused by the sweep.
        let (gating, _) = store
            .enqueue_rolling_queue_source(&new_entry(session, 'b', "k-gating"), Utc::now())
            .unwrap();
        store
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .expect("claimed");
        assert_eq!(
            store
                .refuse_queued_rolling_queue_entry_conflicting_with_rolling(
                    gating.id,
                    &gating.source_commit,
                    "late",
                    Utc::now(),
                )
                .unwrap(),
            None
        );
    }

    /// #648 (hub #3721): a refused-`queue_conflicts_with_rolling` entry, its
    /// owner and a refused-at time to supersede it from.
    fn refused_for_conflict(
        store: &Store,
        owner: Uuid,
        commit: char,
        key: &str,
        at: DateTime<Utc>,
    ) -> RollingQueueEntryV1 {
        let (entry, _) = store
            .enqueue_rolling_queue_source(&new_entry(owner, commit, key), at)
            .unwrap();
        store
            .refuse_queued_rolling_queue_entry_conflicting_with_rolling(
                entry.id,
                &entry.source_commit,
                "conflict_paths=a.rs",
                at,
            )
            .unwrap()
            .expect("refused");
        store.get_rolling_queue_entry(entry.id).unwrap().unwrap()
    }

    fn supersede_refused(
        store: &Store,
        owner: Uuid,
        old: &RollingQueueEntryV1,
        commit: char,
        key: &str,
        at: DateTime<Utc>,
    ) -> Result<(RollingQueueEntryV1, bool)> {
        store.enqueue_rolling_queue_source_superseding(
            &new_entry(owner, commit, key),
            None,
            Some((old.id, &old.source_commit)),
            at,
        )
    }

    fn claimed_commits(store: &Store) -> Vec<String> {
        store
            .claim_rolling_queue_batch(Utc::now(), 8)
            .unwrap()
            .expect("claimed")
            .entries
            .into_iter()
            .map(|entry| entry.source_commit[..1].to_owned())
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_descendant_of_a_conflict_refused_entry_resumes_its_position_and_single_wake_648() {
        let store = store();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        let refused = refused_for_conflict(&store, owner, 'a', "a", now);
        let later_b = store
            .enqueue_rolling_queue_source(&new_entry(owner, 'b', "b"), now)
            .unwrap()
            .0;
        let later_c = store
            .enqueue_rolling_queue_source(&new_entry(owner, 'c', "c"), now)
            .unwrap()
            .0;
        assert_eq!(queue_wakes(&store).len(), 1, "the refusal wakes once");

        let (resumed, replayed) =
            supersede_refused(&store, owner, &refused, 'd', "d", now).unwrap();
        assert!(!replayed);
        assert_eq!(resumed.state, RollingQueueEntryState::Queued);
        assert_eq!(resumed.test_filters, vec!["rsid=rolling_queue".to_string()]);
        // Nobody else moves, the refused row stays what it was.
        let b_now = store.get_rolling_queue_entry(later_b.id).unwrap().unwrap();
        let c_now = store.get_rolling_queue_entry(later_c.id).unwrap().unwrap();
        assert_eq!(
            (b_now.sequence, c_now.sequence),
            (later_b.sequence, later_c.sequence)
        );
        assert_eq!(
            store.get_rolling_queue_entry(refused.id).unwrap().unwrap(),
            refused
        );
        // The successor is admitted at the refused entry's place, ahead of the
        // entries that were queued after it.
        assert_eq!(claimed_commits(&store), vec!["d", "b", "c"]);
        let replay = supersede_refused(&store, owner, &refused, 'd', "d", now).unwrap();
        assert!(replay.1);
        assert_eq!(replay.0.id, resumed.id);

        // One wake in all: the refusal's unfired wake is retired and the
        // successor's settlement is the single wake that fires.
        let enabled = |store: &Store| {
            queue_wakes(store)
                .into_iter()
                .filter(|job| job.enabled)
                .count()
        };
        assert_eq!(enabled(&store), 0);
        store
            .settle_rolling_queue_entry(
                resumed.id,
                RollingQueueEntryState::Published,
                &published(&"1".repeat(40)),
                now,
            )
            .unwrap()
            .expect("settled");
        assert_eq!(enabled(&store), 1);
        assert_eq!(
            queue_wakes(&store)
                .iter()
                .filter(|job| job.enabled)
                .map(|job| job.wake_session_id)
                .collect::<Vec<_>>(),
            vec![Some(owner)]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_refusal_wake_that_already_fired_is_left_alone_when_the_descendant_resumes_648() {
        let store = store();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        let refused = refused_for_conflict(&store, owner, 'a', "a", now);
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET last_fired_at=?1 WHERE name LIKE 'merge-queue-%'",
                [stamp(now)],
            )
            .unwrap();
        let before = queue_wakes(&store);
        supersede_refused(&store, owner, &refused, 'd', "d", now).unwrap();
        let wakes = queue_wakes(&store);
        assert_eq!(wakes.len(), 1);
        assert!(wakes[0].last_fired_at.is_some());
        assert_eq!(
            wakes[0].enabled, before[0].enabled,
            "a fired wake is untouched"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_resumed_successor_that_conflicts_again_hands_the_same_position_on_648() {
        let store = store();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        let refused = refused_for_conflict(&store, owner, 'a', "a", now);
        store
            .enqueue_rolling_queue_source(&new_entry(owner, 'b', "b"), now)
            .unwrap();
        let first = supersede_refused(&store, owner, &refused, 'c', "c", now)
            .unwrap()
            .0;
        store
            .refuse_queued_rolling_queue_entry_conflicting_with_rolling(
                first.id,
                &first.source_commit,
                "conflict_paths=a.rs",
                now,
            )
            .unwrap()
            .expect("refused again");
        let first = store.get_rolling_queue_entry(first.id).unwrap().unwrap();
        supersede_refused(&store, owner, &first, 'd', "d", now).unwrap();
        assert_eq!(claimed_commits(&store), vec!["d", "b"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn only_one_descendant_resumes_a_refused_entry_and_the_window_is_bounded_648() {
        let store = store();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        let refused = refused_for_conflict(&store, owner, 'a', "a", now);
        let inside =
            now + chrono::Duration::seconds(rsi_common::rolling_queue::QUEUE_RESUME_WINDOW_SECS);
        let past = inside + chrono::Duration::seconds(1);
        assert_eq!(
            code_of(supersede_refused(&store, owner, &refused, 'd', "late", past).unwrap_err()),
            QUEUE_ENTRY_SETTLED
        );
        // The source must still descend from the refused entry's commit.
        assert_eq!(
            code_of(
                store
                    .enqueue_rolling_queue_source_superseding(
                        &new_entry(owner, 'd', "stranger"),
                        None,
                        Some((refused.id, &"f".repeat(40))),
                        now,
                    )
                    .unwrap_err()
            ),
            QUEUE_SUPERSEDES_NOT_DESCENDANT
        );
        // A caller outside the entry's authority cannot see it at all.
        assert_eq!(
            code_of(
                supersede_refused(&store, Uuid::new_v4(), &refused, 'd', "foreign", now)
                    .unwrap_err()
            ),
            QUEUE_ENTRY_NOT_FOUND
        );
        supersede_refused(&store, owner, &refused, 'd', "edge", inside).unwrap();
        assert_eq!(
            code_of(supersede_refused(&store, owner, &refused, 'e', "second", now).unwrap_err()),
            QUEUE_ENTRY_SETTLED,
            "an entry a descendant already took over stays settled"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn no_other_settled_class_can_be_resumed_by_a_descendant_648() {
        let now = Utc::now();
        let cases = [
            ("refused", "gate_failed"),
            ("refused", "queue_conflicts_with_entry"),
            ("refused", "provisional_unprovable"),
            ("failed", "queue_conflicts_with_rolling"),
            ("superseded", "queue_entry_cancelled"),
            ("superseded", "queue_conflicts_with_rolling"),
            ("published", "queue_conflicts_with_rolling"),
        ];
        for (state, refusal) in cases {
            let store = store();
            let owner = Uuid::new_v4();
            let old = store
                .enqueue_rolling_queue_source(&new_entry(owner, 'a', "old"), now)
                .unwrap()
                .0;
            store
                .conn
                .execute(
                    "UPDATE rolling_queue_entries SET state=?2, outcome_json=?3, finished_at=?4 WHERE id=?1",
                    params![
                        old.id.to_string(),
                        state,
                        serde_json::json!({ "refusal": refusal }).to_string(),
                        stamp(now),
                    ],
                )
                .unwrap();
            let old = store.get_rolling_queue_entry(old.id).unwrap().unwrap();
            assert_eq!(
                code_of(supersede_refused(&store, owner, &old, 'b', "new", now).unwrap_err()),
                QUEUE_ENTRY_SETTLED,
                "{state} {refusal}"
            );
            assert!(
                store
                    .get_rolling_queue_entry_by_key(owner, "new")
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn assert_caller_marker_is_refused(marker: &str) {
        let store = store();
        let mut seed = new_entry(Uuid::new_v4(), 'a', "pending");
        seed.fixes_tests = vec!["tests::red".to_owned()];
        let (entry, _) = store
            .enqueue_rolling_queue_source_deriving(&seed, None, None, Utc::now())
            .unwrap();
        for value in [marker.to_owned(), format!("{marker}forged")] {
            for fixes in [false, true] {
                for pending in [false, true] {
                    for replay in [false, true] {
                        let mut new = seed.clone();
                        if !replay {
                            new.idempotency_key = "forged".to_owned();
                        }
                        if fixes {
                            new.fixes_tests.push(value.clone());
                        } else {
                            new.test_filters.push(value.clone());
                        }
                        assert_eq!(
                            code_of(
                                store
                                    .enqueue_rolling_queue_source_expediting(
                                        &new,
                                        None,
                                        None,
                                        pending,
                                        None,
                                        &[],
                                        None,
                                        Utc::now(),
                                    )
                                    .unwrap_err()
                            ),
                            QUEUE_INVALID_REQUEST,
                            "{value}, fixes={fixes}, pending={pending}, replay={replay}"
                        );
                    }
                }
            }
        }
        let loaded = store.get_rolling_queue_entry(entry.id).unwrap().unwrap();
        assert_eq!(loaded.test_filters, seed.test_filters);
        assert_eq!(loaded.fixes_tests, seed.fixes_tests);
        assert_eq!(loaded.filters_state, Some(QueueFiltersState::Deriving));
        assert!(
            store
                .get_rolling_queue_entry_by_key(seed.source_session_id, "forged")
                .unwrap()
                .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn caller_expedited_marker_is_refused_655() {
        assert_caller_marker_is_refused("expedited=");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn caller_derived_filter_marker_is_refused_655() {
        assert_caller_marker_is_refused("derived_filter=");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn caller_filters_state_marker_is_refused_655() {
        assert_caller_marker_is_refused("filters_state=");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn caller_fixes_test_marker_is_refused_655() {
        assert_caller_marker_is_refused("fixes_test=");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn caller_resume_position_marker_is_refused_655() {
        assert_caller_marker_is_refused("resume_position=");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_caller_cannot_inject_the_resume_marker_648() {
        let store = store();
        let mut new = new_entry(Uuid::new_v4(), 'a', "k");
        new.test_filters = vec![format!("resume_position=1:{}", Uuid::new_v4())];
        assert!(!rsi_common::rolling_queue::valid_test_filter(
            &new.test_filters[0]
        ));
        assert!(
            code_of(
                store
                    .enqueue_rolling_queue_source(&new, Utc::now())
                    .unwrap_err()
            )
            .starts_with(QUEUE_INVALID_REQUEST)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_resume_marker_round_trips_and_a_malformed_one_is_dropped_648() {
        let prior = Uuid::new_v4();
        let item = rsi_common::rolling_queue::encode_resume_position(41, prior);
        let (rest, found) =
            rsi_common::rolling_queue::split_resume_position(vec!["rsid=a".to_string(), item]);
        assert_eq!(rest, vec!["rsid=a".to_string()]);
        assert_eq!(found, Some((41, prior)));
        let (rest, found) = rsi_common::rolling_queue::split_resume_position(vec![
            "resume_position=nope".to_string(),
        ]);
        assert!(rest.is_empty());
        assert_eq!(found, None);
    }

    /// #2167: fix claims ride in the stored filter list without a schema
    /// change, come back as `fixes_tests` beside unchanged filters, and are
    /// part of the replay identity.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn fix_claims_round_trip_and_are_part_of_the_replay_identity() {
        let store = store();
        let session = Uuid::new_v4();
        let mut new = new_entry(session, 'a', "k1");
        new.fixes_tests = vec!["tests::red".into(), "store::tests::other".into()];
        let (entry, _) = store
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap();
        assert_eq!(entry.test_filters, vec!["rsid=rolling_queue".to_string()]);
        assert_eq!(entry.fixes_tests, new.fixes_tests);
        let stored: String = store
            .conn
            .query_row(
                "SELECT test_filters_json FROM rolling_queue_entries WHERE id=?1",
                [entry.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            r#"["rsid=rolling_queue","fixes_test=tests::red","fixes_test=store::tests::other"]"#
        );
        let (again, replayed) = store
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap();
        assert!(replayed);
        assert_eq!(again, entry);
        let mut unclaimed = new.clone();
        unclaimed.fixes_tests.clear();
        let error = store
            .enqueue_rolling_queue_source(&unclaimed, Utc::now())
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
    fn topology_land_entry_wakes_its_owner_not_the_node_session_and_replays_by_key() {
        let store = store();
        let node_session = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let new = new_entry(node_session, 'c', "attempt-key");
        let (entry, replayed) = store
            .enqueue_rolling_queue_source_for(&new, Some(owner), Utc::now())
            .unwrap();
        assert!(!replayed);
        // A restart finds the same entry by its replay identity, and a
        // duplicate enqueue returns it instead of a second row.
        assert_eq!(
            store
                .get_rolling_queue_entry_by_key(node_session, "attempt-key")
                .unwrap()
                .map(|found| found.id),
            Some(entry.id)
        );
        assert!(
            store
                .get_rolling_queue_entry_by_key(node_session, "other-key")
                .unwrap()
                .is_none()
        );
        let (again, replayed) = store
            .enqueue_rolling_queue_source_for(&new, Some(owner), Utc::now())
            .unwrap();
        assert!(replayed);
        assert_eq!(again.id, entry.id);
        // The settlement wake reaches the owning manager or Epic lead, never
        // a finished topology node session.
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
            .expect("settle wakes");
        let wakes: Vec<_> = store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .collect();
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].wake_session_id, Some(owner));
    }

    /// A topology land attempt of an execution in `execution_status`, and the
    /// replay key its queue entry carries.
    fn topology_land_key(store: &Store, execution_status: &str) -> String {
        let execution = Uuid::new_v4();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO topology_executions (id,topology_name_snapshot,workflow_id,definition_json,definition_digest,requested_by_kind,repo_root,base_commit,custody_plan_json,status,created_at,updated_at) \
                 VALUES (?1,'land','wf','{}','d','manager','/tmp/repo','0000000000000000000000000000000000000000','{}',?2,?3,?3)",
                params![execution.to_string(), execution_status, now],
            )
            .unwrap();
        let key = format!("topology.node:{execution}:land:0:1");
        store
            .conn
            .execute(
                "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,session_id,input_json,created_at,updated_at) \
                 VALUES (?1,?2,'land',0,1,'land','launching',?3,NULL,'{}',?4,?4)",
                params![Uuid::new_v4().to_string(), execution.to_string(), key, now],
            )
            .unwrap();
        key
    }

    fn settle_only_entry(
        store: &Store,
        terminal: RollingQueueEntryState,
        outcome: &RollingQueueOutcome,
    ) -> Uuid {
        let claimed = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        store
            .settle_rolling_queue_entry(claimed.entry.id, terminal, outcome, Utc::now())
            .unwrap()
            .expect("the entry settles");
        claimed.entry.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_published_topology_land_entry_settles_without_waking_the_owner_2386() {
        let store = store();
        let (node_session, owner) = (Uuid::new_v4(), Uuid::new_v4());
        let key = topology_land_key(&store, "running");
        store
            .enqueue_rolling_queue_source_for(
                &new_entry(node_session, 'c', &key),
                Some(owner),
                Utc::now(),
            )
            .unwrap();
        let id = settle_only_entry(
            &store,
            RollingQueueEntryState::Published,
            &published(&"9".repeat(40)),
        );
        assert!(queue_wakes(&store).is_empty());
        // The execution still reads the outcome from the entry.
        let entry = store.get_rolling_queue_entry(id).unwrap().unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Published);
        assert_eq!(entry.outcome.unwrap().landed_sha, Some("9".repeat(40)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_refused_topology_land_entry_keeps_its_refusal_for_the_execution_2386() {
        let store = store();
        let (node_session, owner) = (Uuid::new_v4(), Uuid::new_v4());
        let key = topology_land_key(&store, "running");
        store
            .enqueue_rolling_queue_source_for(
                &new_entry(node_session, 'd', &key),
                Some(owner),
                Utc::now(),
            )
            .unwrap();
        let refusal = RollingQueueOutcome {
            refusal: Some("queue_gate_red".into()),
            ..RollingQueueOutcome::default()
        };
        let id = settle_only_entry(&store, RollingQueueEntryState::Refused, &refusal);
        assert!(queue_wakes(&store).is_empty());
        let entry = store.get_rolling_queue_entry(id).unwrap().unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Refused);
        assert_eq!(
            entry.outcome.unwrap().refusal.as_deref(),
            Some("queue_gate_red")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_topology_land_entry_of_a_blocked_execution_wakes_the_owner_2407() {
        let store = store();
        let owner = Uuid::new_v4();
        // The blocked execution's settled wake has already fired, so it cannot
        // carry the outcome of the land entry that settles afterwards.
        let key = topology_land_key(&store, "blocked");
        store
            .enqueue_rolling_queue_source_for(
                &new_entry(Uuid::new_v4(), 'a', &key),
                Some(owner),
                Utc::now(),
            )
            .unwrap();
        settle_only_entry(
            &store,
            RollingQueueEntryState::Published,
            &published(&"6".repeat(40)),
        );
        let wakes = queue_wakes(&store);
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].wake_session_id, Some(owner));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_topology_land_entry_of_a_settled_execution_and_a_hand_entry_still_wake_2386() {
        let store = store();
        let owner = Uuid::new_v4();
        // The execution already settled, so it cannot record the outcome.
        let key = topology_land_key(&store, "cancelled");
        store
            .enqueue_rolling_queue_source_for(
                &new_entry(Uuid::new_v4(), 'e', &key),
                Some(owner),
                Utc::now(),
            )
            .unwrap();
        settle_only_entry(
            &store,
            RollingQueueEntryState::Published,
            &published(&"8".repeat(40)),
        );
        let wakes = queue_wakes(&store);
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].wake_session_id, Some(owner));
        // A hand-enqueued entry naming a wake session wakes it, even when its
        // key mimics a topology key that no land attempt owns.
        let hand_owner = Uuid::new_v4();
        store
            .enqueue_rolling_queue_source_for(
                &new_entry(Uuid::new_v4(), 'f', "topology.node:no-such-attempt"),
                Some(hand_owner),
                Utc::now(),
            )
            .unwrap();
        settle_only_entry(
            &store,
            RollingQueueEntryState::Published,
            &published(&"7".repeat(40)),
        );
        assert_eq!(queue_wakes(&store).len(), 2);
        assert!(
            queue_wakes(&store)
                .iter()
                .any(|job| job.wake_session_id == Some(hand_owner))
        );
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

    /// Enqueue `commit` from the checkout `repo_path` of `project`.
    fn enqueue_from(store: &Store, commit: char, repo_path: &str, project: Option<Uuid>) {
        let mut new = new_entry(Uuid::new_v4(), commit, &format!("k-{commit}"));
        new.repo_path = repo_path.into();
        new.project_id = project;
        store
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap();
    }

    /// Every `/sandboxes/...` checkout is a worktree of one repository.
    fn worktree_family(path: &str) -> String {
        match path.starts_with("/sandboxes/") {
            true => "git:/repo/.git".into(),
            false => format!("path:{path}"),
        }
    }

    fn claimed_sources(batch: &ClaimedQueueBatch) -> Vec<String> {
        batch
            .entries
            .iter()
            .map(|entry| entry.source_commit[..1].to_string())
            .collect()
    }

    /// Enqueue `commit` (no migration) from one sandbox of the shared repository.
    fn enqueue_lane_entry(store: &Store, commit: char, migration: Option<u32>) {
        let mut new = new_entry(Uuid::new_v4(), commit, &format!("lane-{commit}"));
        new.repo_path = format!("/sandboxes/{commit}");
        new.migration_version = migration;
        store
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap();
    }

    /// The packages a source's commit letter stands for in the lane tests: a
    /// space-separated word list; `*` is a source nothing resolved.
    fn lane_table(table: &'static [(char, &'static str)]) -> impl Fn(&str) -> LanePackages {
        move |source: &str| {
            let letter = source.chars().next()?;
            let (_, words) = table.iter().find(|(c, _)| *c == letter)?;
            match *words {
                "*" => None,
                words => Some(words.split(' ').map(str::to_string).collect()),
            }
        }
    }

    fn claim_in_lanes(
        store: &Store,
        max: usize,
        table: &'static [(char, &'static str)],
    ) -> Option<ClaimedQueueBatch> {
        store
            .claim_rolling_queue_batch_lanes(Utc::now(), max, &worktree_family, &lane_table(table))
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_disjoint_batch_claims_a_second_lane_beside_a_gating_batch_1224() {
        const TABLE: &[(char, &str)] = &[
            ('a', "rsid rsid-store"),
            ('b', "rsi"),
            ('c', "rsi-common"),
            ('d', "rsi"),
        ];
        let store = store();
        for commit in ['a', 'b', 'c', 'd'] {
            enqueue_lane_entry(&store, commit, None);
        }
        let first = claim_in_lanes(&store, 1, TABLE).unwrap();
        assert_eq!(claimed_sources(&first), ["a"]);
        assert!(!first.parallel_lane, "the first batch gates alone");
        // b shares no package with the gating batch: a second lane.
        let second = claim_in_lanes(&store, 1, TABLE).unwrap();
        assert_eq!(claimed_sources(&second), ["b"]);
        assert!(second.parallel_lane);
        assert_ne!(first.batch_id, second.batch_id);
        let events: Vec<String> = store
            .conn
            .prepare("SELECT kind FROM rolling_queue_events WHERE batch_id=?1 ORDER BY id")
            .unwrap()
            .query_map([second.batch_id.to_string()], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(events, ["gating", "lane_claimed"]);
        // Two lanes is the most: c is disjoint from both yet waits.
        assert!(claim_in_lanes(&store, 1, TABLE).is_none());
        // The first lane settling frees a lane; c (still disjoint from the
        // second lane's `rsi`) takes it, d (overlapping `rsi`) does not.
        store
            .settle_rolling_queue_entry(
                first.entries[0].id,
                RollingQueueEntryState::Published,
                &published(&"a".repeat(40)),
                Utc::now(),
            )
            .unwrap();
        let third = claim_in_lanes(&store, 4, TABLE).unwrap();
        assert_eq!(claimed_sources(&third), ["c"]);
        assert!(third.parallel_lane);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_second_lane_needs_every_source_resolved_and_disjoint_1224() {
        const TABLE: &[(char, &str)] =
            &[('a', "rsid"), ('b', "rsid-store"), ('c', "rsi"), ('d', "*")];
        // The plain single-flight claim never opens a lane.
        let store = store();
        enqueue_lane_entry(&store, 'a', None);
        enqueue_lane_entry(&store, 'c', None);
        claim_in_lanes(&store, 1, TABLE).unwrap();
        assert!(
            store
                .claim_rolling_queue_batch_by(Utc::now(), 1, &worktree_family)
                .unwrap()
                .is_none()
        );
        // An unresolved gating source holds the whole queue.
        let unresolved = self::store();
        enqueue_lane_entry(&unresolved, 'd', None);
        enqueue_lane_entry(&unresolved, 'c', None);
        claim_in_lanes(&unresolved, 1, TABLE).unwrap();
        assert!(claim_in_lanes(&unresolved, 1, TABLE).is_none());
        // An overlapping candidate waits for the gating batch.
        let overlap = self::store();
        enqueue_lane_entry(&overlap, 'a', None);
        enqueue_lane_entry(&overlap, 'e', None);
        claim_in_lanes(&overlap, 1, TABLE).unwrap();
        assert!(claim_in_lanes(&overlap, 1, TABLE).is_none());
        // An unresolved or migration-carrying candidate never overtakes, and
        // blocks the entries behind it so they keep their order.
        let blocked = self::store();
        enqueue_lane_entry(&blocked, 'a', None);
        enqueue_lane_entry(&blocked, 'd', None);
        enqueue_lane_entry(&blocked, 'c', None);
        claim_in_lanes(&blocked, 1, TABLE).unwrap();
        assert!(claim_in_lanes(&blocked, 1, TABLE).is_none());
        let migrating = self::store();
        enqueue_lane_entry(&migrating, 'a', None);
        enqueue_lane_entry(&migrating, 'b', Some(142));
        enqueue_lane_entry(&migrating, 'c', None);
        claim_in_lanes(&migrating, 1, TABLE).unwrap();
        assert!(claim_in_lanes(&migrating, 1, TABLE).is_none());
        // A gating migration entry holds the queue too.
        let gating_migration = self::store();
        enqueue_lane_entry(&gating_migration, 'b', Some(142));
        enqueue_lane_entry(&gating_migration, 'c', None);
        claim_in_lanes(&gating_migration, 1, TABLE).unwrap();
        assert!(claim_in_lanes(&gating_migration, 1, TABLE).is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_lane_never_overtakes_an_entry_it_shares_a_package_with_1224() {
        const TABLE: &[(char, &str)] = &[
            ('a', "rsid"),
            ('b', "rsi rsid"),
            ('c', "rsi"),
            ('d', "rsi-common"),
            ('e', "rsi-common"),
        ];
        let store = store();
        for commit in ['a', 'b', 'c', 'd', 'e'] {
            enqueue_lane_entry(&store, commit, None);
        }
        claim_in_lanes(&store, 1, TABLE).unwrap();
        // b overlaps the gating batch and waits; c shares `rsi` with b, so it
        // stays behind it; d is disjoint from all of them and takes the lane
        // with e, which shares d's package.
        let lane = claim_in_lanes(&store, 4, TABLE).unwrap();
        assert_eq!(claimed_sources(&lane), ["d", "e"]);
        assert!(lane.parallel_lane);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn entries_from_different_sandboxes_of_one_repository_claim_as_one_batch() {
        let store = store();
        let project = Some(shared_project());
        for (commit, path) in [
            ('a', "/sandboxes/one"),
            ('b', "/sandboxes/two"),
            ('c', "/elsewhere"),
            ('d', "/sandboxes/three"),
            ('e', "/sandboxes/four"),
        ] {
            enqueue_from(&store, commit, path, project);
        }
        // Path equality alone never batches two sandboxes: the head is alone.
        let alone = store
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        assert_eq!(claimed_sources(&alone), ["a"]);
        store
            .requeue_gating_rolling_queue_entry(alone.entries[0].id, Utc::now())
            .unwrap();
        let batch = store
            .claim_rolling_queue_batch_by(Utc::now(), 4, &worktree_family)
            .unwrap()
            .unwrap();
        // FIFO within the repository; the other repository's entry waits.
        assert_eq!(claimed_sources(&batch), ["a", "b", "d", "e"]);
        assert_eq!(
            batch.repo_paths,
            [
                "/sandboxes/one",
                "/sandboxes/two",
                "/sandboxes/three",
                "/sandboxes/four"
            ]
        );
        assert_eq!(batch.repo_path, "/sandboxes/one");
        let (count, batch_project): (i64, Option<String>) = store
            .conn
            .query_row(
                "SELECT member_count, project_id FROM rolling_queue_batches WHERE id=?1",
                [batch.batch_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 4);
        assert_eq!(batch_project, project.map(|id| id.to_string()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn entries_of_different_projects_or_repositories_never_share_a_batch() {
        let store = store();
        let (project, other) = (Some(shared_project()), Some(Uuid::from_u128(0x2002)));
        enqueue_from(&store, 'a', "/sandboxes/one", project);
        enqueue_from(&store, 'b', "/sandboxes/two", other);
        enqueue_from(&store, 'c', "/sandboxes/three", None);
        enqueue_from(&store, 'd', "/other-repo", project);
        enqueue_from(&store, 'e', "/sandboxes/four", project);
        enqueue_from(&store, 'f', "/sandboxes/five", None);
        let mut order = Vec::new();
        while let Some(batch) = store
            .claim_rolling_queue_batch_by(Utc::now(), 8, &worktree_family)
            .unwrap()
        {
            order.push(claimed_sources(&batch));
            for entry in &batch.entries {
                store
                    .settle_rolling_queue_entry(
                        entry.id,
                        RollingQueueEntryState::Published,
                        &published("f1"),
                        Utc::now(),
                    )
                    .unwrap()
                    .unwrap();
            }
        }
        // The same project and repository batch; a different project (a
        // missing one included) or a different repository starts its own.
        assert_eq!(
            order,
            [vec!["a", "e"], vec!["b"], vec!["c", "f"], vec!["d"]]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_batch_is_capped_and_the_remaining_sandboxes_wait_in_fifo_order() {
        let store = store();
        for (commit, path) in [
            ('a', "/sandboxes/one"),
            ('b', "/sandboxes/two"),
            ('c', "/sandboxes/three"),
        ] {
            enqueue_from(&store, commit, path, Some(shared_project()));
        }
        let first = store
            .claim_rolling_queue_batch_by(Utc::now(), 2, &worktree_family)
            .unwrap()
            .unwrap();
        assert_eq!(claimed_sources(&first), ["a", "b"]);
        for entry in &first.entries {
            store
                .settle_rolling_queue_entry(
                    entry.id,
                    RollingQueueEntryState::Published,
                    &published("f1"),
                    Utc::now(),
                )
                .unwrap()
                .unwrap();
        }
        let second = store
            .claim_rolling_queue_batch_by(Utc::now(), 2, &worktree_family)
            .unwrap()
            .unwrap();
        assert_eq!(claimed_sources(&second), ["c"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_queued_checkouts_are_listed_once_each_in_fifo_order() {
        let store = store();
        for (commit, path) in [
            ('a', "/sandboxes/two"),
            ('b', "/sandboxes/one"),
            ('c', "/sandboxes/two"),
        ] {
            enqueue_from(&store, commit, path, Some(shared_project()));
        }
        assert_eq!(
            store.queued_rolling_queue_repo_paths().unwrap(),
            ["/sandboxes/two", "/sandboxes/one"]
        );
        store
            .claim_rolling_queue_batch_by(Utc::now(), 1, &worktree_family)
            .unwrap()
            .unwrap();
        // A claimed entry is no longer queued.
        assert_eq!(
            store.queued_rolling_queue_repo_paths().unwrap(),
            ["/sandboxes/one", "/sandboxes/two"]
        );
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
        // Rewind every post-V159 migration from the current schema head before
        // replaying the queue wake migration and its successors. This keeps
        // the fixture aligned as new migrations are added.
        let schema_head = super::super::LATEST_SCHEMA_VERSION;
        super::super::tests::rewind_post_v121_tail_to(&store.conn, 159);
        store.init_schema().unwrap();
        store.init_schema().unwrap();
        let replayed_schema: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(replayed_schema, schema_head);
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

    /// hub #2969: an entry enqueued AFTER the seat rotated, naming the archived
    /// predecessor as its owner, stores the current seat and wakes it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn enqueue_naming_an_archived_manager_seat_stores_the_current_seat() {
        use crate::store::manager_coordinator::tests::fixture;
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat_a = config.current_session_id.unwrap();
        let seat_b = insert_continuation(&store, seat_a);
        archive(&store, seat_a);
        store
            .record_harness_manager_rotation(seat_a, seat_b)
            .unwrap();
        let (entry, _) = store
            .enqueue_rolling_queue_source_for(
                &new_entry(Uuid::new_v4(), 'a', "late"),
                Some(seat_a),
                Utc::now(),
            )
            .unwrap();
        let stored: Option<String> = store
            .conn
            .query_row(
                "SELECT wake_session_id FROM rolling_queue_entries WHERE id=?1",
                [entry.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, Some(seat_b.to_string()));
        store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        store
            .settle_rolling_queue_entry(
                entry.id,
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

    /// An entry in `project`, enqueued as `session`.
    fn enqueue_in(
        store: &Store,
        project: Uuid,
        session: Uuid,
        commit: char,
        key: &str,
    ) -> RollingQueueEntryV1 {
        let mut new = new_entry(session, commit, key);
        new.project_id = Some(project);
        store
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap()
            .0
    }

    fn cancel(store: &Store, caller: Uuid, id: Uuid) -> Result<(AgentRollingQueueEntryV1, bool)> {
        store.cancel_rolling_queue_entry_for_agent(caller, id, None, Utc::now())
    }

    fn list(store: &Store, caller: Uuid) -> Vec<AgentRollingQueueEntryV1> {
        store
            .list_rolling_queue_for_agent(
                caller,
                &AgentListRollingQueueRequestV1 {
                    fields: Some(rsi_common::rolling_queue::RollingQueueFieldsV1::Full),
                    limit: Some(100),
                    ..Default::default()
                },
            )
            .unwrap()
            .0
    }

    fn code_of(error: DaemonError) -> String {
        match error {
            // The not-found refusal carries a diagnostic after its code (#3331).
            DaemonError::InvalidParam(code) | DaemonError::PolicyDenied(code) => {
                match code.strip_prefix(QUEUE_ENTRY_NOT_FOUND) {
                    Some(_) => QUEUE_ENTRY_NOT_FOUND.to_string(),
                    None => code,
                }
            }
            other => panic!("not a refusal code: {other:?}"),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn agent_list_is_newest_first_bounded_filtered_and_project_scoped() {
        use crate::store::manager_coordinator::tests::fixture;
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let project = config.project_id;
        let first = enqueue_in(&store, project, seat, 'a', "k-a");
        let second = enqueue_in(&store, project, seat, 'b', "k-b");
        let third = enqueue_in(&store, project, seat, 'c', "k-c");
        // Another project's entry is never in this manager's scope.
        let foreign_project = Uuid::new_v4();
        let foreign = enqueue_in(&store, foreign_project, Uuid::new_v4(), 'd', "k-d");
        // The FIFO head (the first entry) is gating now.
        let claimed = store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        assert_eq!(claimed.entry.id, first.id);

        let all = list(&store, seat);
        assert_eq!(
            all.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![third.id, second.id, first.id]
        );
        assert!(all.iter().all(|entry| entry.id != foreign.id));
        let head = all.last().unwrap();
        assert_eq!(head.state, RollingQueueEntryState::Gating);
        assert_eq!(head.batch_id, Some(claimed.batch_id));
        assert_eq!(head.enqueuer_session_id, seat);
        assert_eq!(head.project_id, Some(project));
        assert_eq!(all[0].batch_id, None);

        // A page of two, then the cursor yields the rest.
        let (page, next) = store
            .list_rolling_queue_for_agent(
                seat,
                &AgentListRollingQueueRequestV1 {
                    limit: Some(2),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(next, Some(second.sequence));
        let (rest, next) = store
            .list_rolling_queue_for_agent(
                seat,
                &AgentListRollingQueueRequestV1 {
                    limit: Some(2),
                    before_sequence: next,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            rest.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![first.id]
        );
        assert_eq!(next, None);

        let queued = store
            .list_rolling_queue_for_agent(
                seat,
                &AgentListRollingQueueRequestV1 {
                    state: Some(RollingQueueEntryState::Queued),
                    project_id: Some(project),
                    limit: Some(100),
                    ..Default::default()
                },
            )
            .unwrap()
            .0;
        assert_eq!(queued.len(), 2);

        // A project outside the manager's scope is named, not silently empty.
        let refusal = store
            .list_rolling_queue_for_agent(
                seat,
                &AgentListRollingQueueRequestV1 {
                    project_id: Some(foreign_project),
                    limit: Some(10),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert_eq!(
            code_of(refusal),
            rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE
        );

        // A settled entry reports a bounded outcome summary.
        store
            .settle_rolling_queue_entry(
                first.id,
                RollingQueueEntryState::Refused,
                &RollingQueueOutcome {
                    refusal: Some("gate_failed".into()),
                    failing_tests: vec!["a::b".into(), "c::d".into()],
                    detail: Some("x".repeat(1000)),
                    ..RollingQueueOutcome::default()
                },
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        let settled = list(&store, seat).pop().unwrap();
        let summary = settled.outcome.unwrap();
        assert_eq!(summary.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(summary.failing_test_count, Some(2));
        assert_eq!(
            summary.detail.unwrap().len(),
            rsi_common::rolling_queue::ROLLING_QUEUE_OUTCOME_DETAIL_BYTES
        );
        assert!(settled.finished_at.is_some());

        // A session that is neither the manager nor an owner sees nothing.
        assert!(list(&store, Uuid::new_v4()).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn agent_list_defaults_to_live_and_recent_problems_with_compact_outcomes() {
        use crate::store::manager_coordinator::tests::fixture;
        use rsi_common::rolling_queue::RollingQueueFieldsV1;
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let project = config.project_id;
        let now = Utc::now();
        let mut ids = Vec::new();
        for (index, (state, age)) in [
            (RollingQueueEntryState::Queued, 7200),
            (RollingQueueEntryState::Admitted, 7200),
            (RollingQueueEntryState::Gating, 7200),
            (RollingQueueEntryState::Refused, 3600),
            (RollingQueueEntryState::Failed, 3599),
            (RollingQueueEntryState::Refused, 3601),
            (RollingQueueEntryState::Failed, 7200),
            (RollingQueueEntryState::Published, 1),
            (RollingQueueEntryState::Superseded, 1),
        ]
        .into_iter()
        .enumerate()
        {
            let commit = char::from(b'0' + u8::try_from(index).unwrap());
            let entry = enqueue_in(&store, project, seat, commit, &format!("view-{index}"));
            let detail = if index == 3 {
                format!(
                    "gate refused: {}\ngate_timing: noisy diagnostics",
                    "界".repeat(100)
                )
            } else {
                format!(
                    "gate refused: summary\n{}",
                    "gate_timing: noisy diagnostics ".repeat(30)
                )
            };
            let outcome = serde_json::json!({
                "refusal": "gate_failed", "landed_sha": "f".repeat(40),
                "failing_tests": ["one::test", "other::test"], "detail": detail,
            });
            // Seed lifecycle rows without running a lander; terminal rows are
            // written once so the immutability guards remain active.
            store.conn.execute(
                "UPDATE rolling_queue_entries SET state=?2,outcome_json=?3,finished_at=?4 WHERE id=?1",
                params![entry.id.to_string(), state.as_str(),
                    state.is_terminal().then(|| outcome.to_string()),
                    state.is_terminal().then(|| (now - chrono::Duration::seconds(age)).to_rfc3339_opts(SecondsFormat::Nanos, true))],
            ).unwrap();
            ids.push(entry.id);
        }
        let read = |request: AgentListRollingQueueRequestV1| {
            store
                .list_rolling_queue_for_agent_at(seat, &request, now)
                .unwrap()
        };
        let (default, _) = read(Default::default());
        assert_eq!(
            default.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            ids[..5].iter().rev().copied().collect::<Vec<_>>()
        );
        let outcome = serde_json::to_value(default[0].outcome.as_ref().unwrap()).unwrap();
        assert_eq!(outcome["refusal"], "gate_failed");
        assert_eq!(outcome["landed_sha"], "f".repeat(40));
        let detail = outcome["detail"].as_str().unwrap();
        assert_eq!(detail, "gate refused: summary");
        let unicode = default[1]
            .outcome
            .as_ref()
            .unwrap()
            .detail
            .as_ref()
            .unwrap();
        assert!(unicode.starts_with("gate refused: 界"));
        assert!(unicode.len() <= 160 && unicode.lines().count() == 1);
        assert_eq!(outcome.as_object().unwrap().len(), 3);
        let (page, next) = read(AgentListRollingQueueRequestV1 {
            limit: Some(2),
            ..Default::default()
        });
        assert_eq!(page.len(), 2);
        let (rest, _) = read(AgentListRollingQueueRequestV1 {
            before_sequence: next,
            ..Default::default()
        });
        assert_eq!(rest.len(), 3);
        let (full, _) = read(AgentListRollingQueueRequestV1 {
            fields: Some(RollingQueueFieldsV1::Full),
            ..Default::default()
        });
        assert_eq!(full.len(), ids.len());
        let full_outcome = full[0].outcome.as_ref().unwrap();
        assert_eq!(full_outcome.failing_test_count, Some(2));
        assert_eq!(full_outcome.detail.as_ref().unwrap().len(), 240);
        let (history, _) = read(AgentListRollingQueueRequestV1 {
            state: Some(RollingQueueEntryState::Refused),
            ..Default::default()
        });
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0].outcome.as_ref().unwrap().failing_test_count,
            Some(2)
        );
        assert_eq!(
            history[0]
                .outcome
                .as_ref()
                .unwrap()
                .detail
                .as_ref()
                .unwrap()
                .len(),
            240
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn agent_cancel_withdraws_only_queued_entries_and_replays() {
        use crate::store::manager_coordinator::tests::fixture;
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let project = config.project_id;
        let gating = enqueue_in(&store, project, seat, 'a', "k-a");
        let queued = enqueue_in(&store, project, seat, 'b', "k-b");
        let admitted = enqueue_in(&store, project, seat, 'c', "k-c");
        store
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET state='admitted' WHERE id=?1",
                [admitted.id.to_string()],
            )
            .unwrap();
        store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();

        // The lander owns an admitted or gating entry: refused, untouched.
        for held in [gating.id, admitted.id] {
            let refusal = cancel(&store, seat, held).unwrap_err();
            assert_eq!(code_of(refusal), QUEUE_ENTRY_IN_FLIGHT);
            assert_ne!(
                store.get_rolling_queue_entry(held).unwrap().unwrap().state,
                RollingQueueEntryState::Superseded
            );
        }

        let (cancelled, replayed) = store
            .cancel_rolling_queue_entry_for_agent(seat, queued.id, Some("replaced"), Utc::now())
            .unwrap();
        assert!(!replayed);
        assert_eq!(cancelled.state, RollingQueueEntryState::Superseded);
        assert_eq!(
            cancelled.outcome.as_ref().unwrap().refusal.as_deref(),
            Some(QUEUE_ENTRY_CANCELLED)
        );
        assert!(
            cancelled
                .outcome
                .unwrap()
                .detail
                .unwrap()
                .ends_with(": replaced")
        );
        assert!(cancelled.finished_at.is_some());
        // The caller is the entry's owner: no wake for its own cancel.
        assert!(queue_wakes(&store).is_empty());

        // A second cancel replays; the stored row is unchanged.
        let before = store.get_rolling_queue_entry(queued.id).unwrap().unwrap();
        let (again, replayed) = cancel(&store, seat, queued.id).unwrap();
        assert!(replayed);
        assert_eq!(again.state, RollingQueueEntryState::Superseded);
        assert_eq!(
            store.get_rolling_queue_entry(queued.id).unwrap().unwrap(),
            before
        );

        // The claim never picks a cancelled entry, and the commit may be
        // enqueued again under a new key.
        assert!(
            store
                .claim_next_rolling_queue_entry(Utc::now())
                .unwrap()
                .is_none(),
            "the gating entry still holds the single-flight slot"
        );
        let again = enqueue_in(&store, project, seat, 'b', "k-b-again");
        assert_eq!(again.state, RollingQueueEntryState::Queued);

        // A settled entry has nothing to cancel.
        let gating_row = store.get_rolling_queue_entry(gating.id).unwrap().unwrap();
        assert_eq!(gating_row.state, RollingQueueEntryState::Gating);
        store
            .settle_rolling_queue_entry(
                gating.id,
                RollingQueueEntryState::Published,
                &published(&"9".repeat(40)),
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            code_of(cancel(&store, seat, gating.id).unwrap_err()),
            QUEUE_ENTRY_SETTLED
        );
        // An unknown id and an out-of-scope entry share one refusal.
        assert_eq!(
            code_of(cancel(&store, seat, Uuid::new_v4()).unwrap_err()),
            QUEUE_ENTRY_NOT_FOUND
        );
        let foreign = enqueue_in(&store, Uuid::new_v4(), Uuid::new_v4(), 'e', "k-e");
        assert_eq!(
            code_of(cancel(&store, seat, foreign.id).unwrap_err()),
            QUEUE_ENTRY_NOT_FOUND
        );
        assert_eq!(
            store
                .get_rolling_queue_entry(foreign.id)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Queued
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn agent_cancel_by_the_manager_wakes_a_different_owner_once() {
        use crate::store::manager_coordinator::tests::fixture;
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let project = config.project_id;
        let lead = Uuid::new_v4();
        let owned = enqueue_in(&store, project, lead, 'a', "k-a");

        // The enqueuer (not the manager) can also withdraw its own entry.
        assert_eq!(list(&store, lead).len(), 1);
        let (cancelled, _) = cancel(&store, seat, owned.id).unwrap();
        assert_eq!(cancelled.state, RollingQueueEntryState::Superseded);
        let wakes = queue_wakes(&store);
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].name, format!("merge-queue-cancel-{}", owned.id));
        assert_eq!(wakes[0].wake_session_id, Some(lead));
        assert!(wakes[0].message.contains(QUEUE_ENTRY_CANCELLED));
        assert_eq!(
            recorded_wake(&store, owned.id),
            Some(wakes[0].id.to_string())
        );
        // A replayed cancel writes no second wake.
        assert!(cancel(&store, seat, owned.id).unwrap().1);
        assert_eq!(queue_wakes(&store).len(), 1);

        let own = enqueue_in(&store, project, lead, 'b', "k-b");
        assert!(
            cancel(&store, lead, own.id).unwrap().0.state == RollingQueueEntryState::Superseded
        );
        assert_eq!(queue_wakes(&store).len(), 1, "the owner needs no wake");
        // Another worker in the project owns nothing here.
        let other = enqueue_in(&store, project, Uuid::new_v4(), 'c', "k-c");
        assert_eq!(
            code_of(cancel(&store, lead, other.id).unwrap_err()),
            QUEUE_ENTRY_NOT_FOUND
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn agent_scope_follows_the_manager_seat_across_a_rotation() {
        use crate::store::manager_coordinator::tests::fixture;
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        let store = store();
        let (config, lead) = fixture(&store, Default::default());
        let seat_a = config.current_session_id.unwrap();
        store
            .update_session_status(seat_a, rsi_common::types::SessionStatus::Running)
            .unwrap();
        let project = config.project_id;
        let entry = enqueue_in(&store, project, seat_a, 'a', "k-a");
        let worker_entry = enqueue_in(&store, project, Uuid::new_v4(), 'b', "k-b");
        assert_eq!(list(&store, seat_a).len(), 2);

        // Displace the (still running) manager with a successor seat.
        let mut next = store.get_session(seat_a).unwrap().unwrap();
        next.id = Uuid::new_v4();
        next.status = rsi_common::types::SessionStatus::Completed;
        next.continued_from = None;
        store.insert_session(&next).unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project,
                session_id: next.id,
                epic_ids: Some(vec![lead.parent_id.unwrap()]),
                group_ids: Vec::new(),
                expected_row_version: config.row_version,
            })
            .unwrap();
        let seat_b = next.id;

        // The displaced seat holds nothing, even for an entry it enqueued:
        // the outcome wake moved with the seat.
        assert!(list(&store, seat_a).is_empty());
        assert_eq!(
            code_of(cancel(&store, seat_a, entry.id).unwrap_err()),
            QUEUE_ENTRY_NOT_FOUND
        );
        assert_eq!(
            store
                .get_rolling_queue_entry(entry.id)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Queued
        );

        // The successor sees both entries and cancels its predecessor's.
        let seen = list(&store, seat_b);
        assert_eq!(
            seen.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![worker_entry.id, entry.id]
        );
        let carried = seen.iter().find(|e| e.id == entry.id).unwrap();
        assert_eq!(carried.enqueuer_session_id, seat_a);
        assert_eq!(carried.wake_session_id, Some(seat_b));
        assert_eq!(
            store
                .get_rolling_queue_entry_for_supersession(seat_b, entry.id)
                .unwrap()
                .unwrap()
                .id,
            entry.id
        );
        let mut replacement = new_entry(seat_b, 'c', "successor-replacement");
        replacement.project_id = Some(project);
        let (new, replayed) = store
            .enqueue_rolling_queue_source_superseding(
                &replacement,
                None,
                Some((entry.id, &entry.source_commit)),
                Utc::now(),
            )
            .unwrap();
        assert!(!replayed);
        assert_eq!(new.sequence, entry.sequence);
        assert!(new.sequence < worker_entry.sequence);
        assert_eq!(
            store
                .get_rolling_queue_entry(entry.id)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Superseded
        );
        assert_eq!(
            list(&store, seat_b)
                .iter()
                .find(|e| e.id == new.id)
                .unwrap()
                .wake_session_id,
            Some(seat_b)
        );
        let (cancelled, replayed) = cancel(&store, seat_b, new.id).unwrap();
        assert!(!replayed);
        assert_eq!(cancelled.state, RollingQueueEntryState::Superseded);
        // The wake target was the successor itself: no extra wake.
        assert!(queue_wakes(&store).is_empty());
        // The enqueuer's immutable identity is unchanged.
        assert_eq!(
            store
                .get_rolling_queue_entry(entry.id)
                .unwrap()
                .unwrap()
                .source_session_id,
            seat_a
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn not_found_refusal_names_the_checked_id_and_a_visible_near_match_3331() {
        use crate::store::manager_coordinator::tests::fixture;
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let entry = enqueue_in(&store, config.project_id, seat, 'a', "k-3331");
        // A wrong id that shares the first eight characters of a real entry
        // (the id the PM typed in #3331 diverged after the first group).
        let mut wrong = entry.id.to_string();
        wrong.replace_range(
            9..13,
            if &wrong[9..13] == "0000" {
                "1111"
            } else {
                "0000"
            },
        );
        let wrong = Uuid::parse_str(&wrong).unwrap();
        assert_ne!(wrong, entry.id);

        let text = cancel(&store, seat, wrong).unwrap_err().to_string();
        assert!(text.contains(QUEUE_ENTRY_NOT_FOUND), "{text}");
        assert!(text.contains(&wrong.to_string()), "{text}");
        assert!(text.contains("AgentListRollingQueue"), "{text}");
        assert!(
            text.contains(&format!("did you mean {}", entry.id)),
            "{text}"
        );

        // A caller with no scope over the entry is told no near match, so the
        // refusal never confirms an entry it cannot see.
        let stranger = Uuid::new_v4();
        let text = cancel(&store, stranger, wrong).unwrap_err().to_string();
        assert!(text.contains(QUEUE_ENTRY_NOT_FOUND), "{text}");
        assert!(!text.contains(&entry.id.to_string()), "{text}");
        assert!(!text.contains("did you mean"), "{text}");
        let text = cancel(&store, stranger, entry.id).unwrap_err().to_string();
        assert!(!text.contains("did you mean"), "{text}");
    }
    /// #3487: enqueue `commit` in `project`, optionally expedited.
    fn enqueue_expediting(
        store: &Store,
        project: Uuid,
        commit: char,
        key: &str,
        expedite: Option<(&str, bool)>,
        supersedes: Option<(Uuid, &str)>,
    ) -> Result<RollingQueueEntryV1> {
        let mut new = new_entry(Uuid::from_u128(0x3487), commit, key);
        new.project_id = Some(project);
        let admission = expedite.map(|(reason, automatic)| ExpediteAdmission {
            reason: reason.into(),
            automatic,
        });
        store
            .enqueue_rolling_queue_source_expediting(
                &new,
                None,
                supersedes,
                false,
                None,
                &[],
                admission.as_ref(),
                Utc::now(),
            )
            .map(|(entry, _)| entry)
    }

    /// #3487: an expedited entry is claimed ahead of every ordinary queued entry,
    /// expedited entries keep their enqueue order among themselves, and ordinary
    /// entries stay FIFO.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn expedited_entries_are_claimed_first_and_the_rest_stay_fifo_3487() {
        let store = store();
        let project = shared_project();
        let a = enqueue_expediting(&store, project, 'a', "ka", None, None).unwrap();
        let b = enqueue_expediting(&store, project, 'b', "kb", None, None).unwrap();
        let c = enqueue_expediting(&store, project, 'c', "kc", Some(("incident", false)), None)
            .unwrap();
        let d = enqueue_expediting(&store, project, 'd', "kd", None, None).unwrap();
        let e =
            enqueue_expediting(&store, project, 'e', "ke", Some(("qa fix", true)), None).unwrap();
        assert_eq!(c.expedite_reason.as_deref(), Some("incident"));
        assert_eq!(e.expedite_reason.as_deref(), Some("qa fix"));
        assert_eq!(a.expedite_reason, None);
        // The marker is bookkeeping, never a filter the gate would run.
        assert_eq!(c.test_filters, vec!["rsid=rolling_queue".to_string()]);

        let first = store
            .claim_rolling_queue_batch(Utc::now(), 3)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.entries.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![c.id, e.id, a.id],
            "expedited in enqueue order, then the oldest ordinary entry"
        );
        // A gating batch is never preempted: nothing else is claimed beside it,
        // and once it settles the next claim sees only what is left.
        assert!(
            store
                .claim_rolling_queue_batch(Utc::now(), 3)
                .unwrap()
                .is_none()
        );
        let settled: Vec<_> = first
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
            .settle_rolling_queue_entries(&settled, Utc::now())
            .unwrap();
        let second = store
            .claim_rolling_queue_batch(Utc::now(), 3)
            .unwrap()
            .unwrap();
        assert_eq!(
            second.entries.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![b.id, d.id]
        );
    }

    /// #4007: enqueue `commit` carrying `width` distinct filters (and the given
    /// fix claims), optionally expedited, in the shared project.
    fn enqueue_wide(
        store: &Store,
        commit: char,
        width: usize,
        fixes: &[&str],
        expedite: bool,
    ) -> RollingQueueEntryV1 {
        let mut new = new_entry(Uuid::from_u128(0x4007), commit, &format!("wide-{commit}"));
        new.migration_version = None;
        new.test_filters = (0..width)
            .map(|index| format!("rsid=t{commit}{index}"))
            .collect();
        new.fixes_tests = fixes.iter().map(|name| (*name).to_owned()).collect();
        let admission = expedite.then(|| ExpediteAdmission {
            reason: "red fix".into(),
            automatic: false,
        });
        store
            .enqueue_rolling_queue_source_expediting(
                &new,
                None,
                None,
                false,
                None,
                &[],
                admission.as_ref(),
                Utc::now(),
            )
            .map(|(entry, _)| entry)
            .unwrap()
    }

    fn claimed_ids(store: &Store) -> Vec<Uuid> {
        store
            .claim_rolling_queue_batch(Utc::now(), 8)
            .unwrap()
            .map(|batch| batch.entries.iter().map(|entry| entry.id).collect())
            .unwrap_or_default()
    }

    /// #4007: a batch whose summed filters would pass the budget is split before
    /// it is admitted; the entry that does not fit heads the next batch.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_batch_is_split_when_its_summed_filters_pass_the_budget_4007() {
        let store = store();
        let a = enqueue_wide(&store, 'a', 2500, &[], false);
        let b = enqueue_wide(&store, 'b', 1400, &[], false);
        let c = enqueue_wide(&store, 'c', 300, &[], false);
        let d = enqueue_wide(&store, 'd', 20, &[], false);
        assert_eq!(
            claimed_ids(&store),
            vec![a.id, b.id],
            "2500 + 1400 fits, +300 does not"
        );
        let first = [a.id, b.id];
        let settled: Vec<_> = first
            .iter()
            .map(|id| {
                (
                    *id,
                    RollingQueueEntryState::Published,
                    published(&"9".repeat(40)),
                )
            })
            .collect();
        store
            .settle_rolling_queue_entries(&settled, Utc::now())
            .unwrap();
        assert_eq!(claimed_ids(&store), vec![c.id, d.id]);
    }

    /// #4007: one entry wider than the whole budget still gates (alone).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_entry_wider_than_the_budget_gates_alone_4007() {
        let store = store();
        let wide = enqueue_wide(
            &store,
            'a',
            ROLLING_QUEUE_BATCH_FILTER_BUDGET + 500,
            &[],
            false,
        );
        let _small = enqueue_wide(&store, 'b', 5, &[], false);
        assert_eq!(claimed_ids(&store), vec![wide.id]);
    }

    /// #4007: an expedited red-fix entry and a fix-claiming entry never share a
    /// gate with a wide entry, but still batch with small ones.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_expedited_or_fix_claiming_entry_gates_alone_or_with_small_entries_4007() {
        let store = store();
        let fix = enqueue_wide(&store, 'a', 500, &["x::y"], true);
        let small = enqueue_wide(&store, 'b', 40, &[], false);
        let wide = enqueue_wide(&store, 'c', 2000, &[], false);
        let wide2 = enqueue_wide(&store, 'd', 900, &[], false);
        // The expedited fix heads the order, takes the small entry behind it and
        // stops at the wide one (admission order is kept).
        assert_eq!(claimed_ids(&store), vec![fix.id, small.id]);
        let settled: Vec<_> = [fix.id, small.id]
            .iter()
            .map(|id| {
                (
                    *id,
                    RollingQueueEntryState::Published,
                    published(&"9".repeat(40)),
                )
            })
            .collect();
        store
            .settle_rolling_queue_entries(&settled, Utc::now())
            .unwrap();
        // With no priority entry left the ordinary batch forms as before.
        assert_eq!(claimed_ids(&store), vec![wide.id, wide2.id]);
    }

    /// #4007: a fix claim that is not expedited behaves the same: it is not
    /// added to a batch of wide entries.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_fix_claim_is_not_batched_behind_a_wide_entry_4007() {
        let store = store();
        let wide = enqueue_wide(&store, 'a', 1000, &[], false);
        let fix = enqueue_wide(&store, 'b', 100, &["x::y"], false);
        assert_eq!(claimed_ids(&store), vec![wide.id]);
        let settled = [(
            wide.id,
            RollingQueueEntryState::Published,
            published(&"9".repeat(40)),
        )];
        store
            .settle_rolling_queue_entries(&settled, Utc::now())
            .unwrap();
        assert_eq!(claimed_ids(&store), vec![fix.id]);
    }

    /// Review #4027 of #4007: enqueue `commit` with no filters of its own and
    /// `hot` changed files, whose filter derivation went over budget, so it is
    /// stored `lander_selects` (a Rust diff the lander must select tests for).
    fn enqueue_lander_selects(store: &Store, commit: char, hot: usize) -> RollingQueueEntryV1 {
        let mut new = new_entry(Uuid::from_u128(0x4007), commit, &format!("sel-{commit}"));
        new.migration_version = None;
        new.test_filters = Vec::new();
        new.hot_files = (0..hot)
            .map(|i| format!("crates/rsid/src/f{i}.rs"))
            .collect();
        let (entry, _) = store
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap();
        assert!(
            store
                .mark_rolling_queue_filters_lander_selects(entry.id, false)
                .unwrap()
        );
        let marked = store.get_rolling_queue_entry(entry.id).unwrap().unwrap();
        assert_eq!(marked.filters_state, Some(QueueFiltersState::LanderSelects));
        assert!(marked.test_filters.is_empty());
        marked
    }

    fn settle_published(store: &Store, ids: &[Uuid]) {
        let settled: Vec<_> = ids
            .iter()
            .map(|id| {
                (
                    *id,
                    RollingQueueEntryState::Published,
                    published(&"9".repeat(40)),
                )
            })
            .collect();
        store
            .settle_rolling_queue_entries(&settled, Utc::now())
            .unwrap();
    }

    /// Review #4027: the 502-filter expedited fix and an unfiltered
    /// `lander_selects` entry of 400 files never share a gate, whichever was
    /// enqueued first.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_expedited_fix_is_not_batched_with_an_unfiltered_rust_entry_4027() {
        let store = store();
        let fix = enqueue_wide(&store, 'a', 502, &[], true);
        let unfiltered = enqueue_lander_selects(&store, 'b', 400);
        let small = enqueue_wide(&store, 'c', 10, &[], false);
        assert_eq!(
            claimed_ids(&store),
            vec![fix.id],
            "the fix stops at the unfiltered entry behind it"
        );
        settle_published(&store, &[fix.id]);
        assert_eq!(
            claimed_ids(&store),
            vec![unfiltered.id],
            "the unfiltered entry gates alone, not with the small one behind it"
        );
        settle_published(&store, &[unfiltered.id]);
        assert_eq!(claimed_ids(&store), vec![small.id]);

        // The reverse order: the unfiltered head takes nobody with it.
        let store = self::store();
        let unfiltered = enqueue_lander_selects(&store, 'd', 400);
        let fix = enqueue_wide(&store, 'e', 502, &[], true);
        // Expedited entries claim first, so the fix heads and the unfiltered
        // entry stays behind; the fix still gates alone.
        assert_eq!(claimed_ids(&store), vec![fix.id]);
        settle_published(&store, &[fix.id]);
        assert_eq!(claimed_ids(&store), vec![unfiltered.id]);
    }

    /// Review #4027: an unfiltered entry counts as over budget, so it gates
    /// alone behind and ahead of ordinary small entries, and a fix claim is not
    /// batched with it. A filtered entry of the same diff size is unaffected.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn an_unfiltered_rust_entry_gates_alone_4027() {
        let store = store();
        let first = enqueue_wide(&store, 'a', 20, &[], false);
        let unfiltered = enqueue_lander_selects(&store, 'b', 400);
        let after = enqueue_wide(&store, 'c', 20, &[], false);
        assert_eq!(claimed_ids(&store), vec![first.id]);
        settle_published(&store, &[first.id]);
        assert_eq!(claimed_ids(&store), vec![unfiltered.id]);
        settle_published(&store, &[unfiltered.id]);
        assert_eq!(claimed_ids(&store), vec![after.id]);

        let store = self::store();
        let unfiltered = enqueue_lander_selects(&store, 'd', 400);
        let fix = enqueue_wide(&store, 'e', 100, &["x::y"], false);
        assert_eq!(claimed_ids(&store), vec![unfiltered.id]);
        settle_published(&store, &[unfiltered.id]);
        assert_eq!(claimed_ids(&store), vec![fix.id]);
    }

    /// Review #4027: the width of a `lander_selects` entry is unbounded and sums
    /// without overflow; any other entry weighs its filter count.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn batch_width_is_unbounded_only_for_an_unfiltered_lander_selects_entry_4027() {
        let store = store();
        let unfiltered = enqueue_lander_selects(&store, 'a', 3);
        let plain = enqueue_wide(&store, 'b', 7, &[], false);
        assert_eq!(batch_width(&unfiltered), usize::MAX);
        assert_eq!(batch_width(&plain), 7);
        assert!(!batch_admits(
            &[unfiltered.clone(), unfiltered.clone()],
            &unfiltered
        ));
        assert!(!batch_admits(&[plain.clone()], &unfiltered));
        assert!(!batch_admits(&[unfiltered], &plain));
    }

    /// #3487: with nothing expedited the order is the enqueue order, and the
    /// repository probe and the lane candidates see the same order as the claim.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_probes_see_the_claim_order_and_ordinary_entries_stay_fifo_3487() {
        let store = store();
        let project = shared_project();
        let mut ordinary = Vec::new();
        for (index, commit) in ['a', 'b', 'c'].into_iter().enumerate() {
            let mut new = new_entry(Uuid::new_v4(), commit, &format!("k{index}"));
            new.repo_path = format!("/tmp/repo-{commit}");
            ordinary.push(
                store
                    .enqueue_rolling_queue_source(&new, Utc::now())
                    .unwrap()
                    .0,
            );
        }
        assert_eq!(
            store.queued_rolling_queue_repo_paths().unwrap(),
            vec!["/tmp/repo-a", "/tmp/repo-b", "/tmp/repo-c"]
        );
        let mut urgent = new_entry(Uuid::new_v4(), 'f', "kz");
        urgent.repo_path = "/tmp/repo-f".into();
        store
            .enqueue_rolling_queue_source_expediting(
                &urgent,
                None,
                None,
                false,
                None,
                &[],
                Some(&ExpediteAdmission {
                    reason: "incident".into(),
                    automatic: false,
                }),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(
            store.queued_rolling_queue_repo_paths().unwrap(),
            vec!["/tmp/repo-f", "/tmp/repo-a", "/tmp/repo-b", "/tmp/repo-c"]
        );
        let (_, queued) = store.rolling_queue_lane_candidates(8).unwrap();
        assert_eq!(
            queued
                .iter()
                .map(|(_, path)| path.as_str())
                .collect::<Vec<_>>(),
            vec!["/tmp/repo-f", "/tmp/repo-a", "/tmp/repo-b", "/tmp/repo-c"]
        );
        let _ = (project, ordinary);
    }

    /// #3487: at most two live expedited entries per project. A manual expedite
    /// over the cap is refused with the live entries named; an automatic one
    /// stays ordinary; another project is unaffected; a superseded expedited
    /// entry does not count against its replacement; a settled one frees a slot.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_expedite_cap_is_two_live_entries_per_project_3487() {
        let store = store();
        let project = shared_project();
        let one =
            enqueue_expediting(&store, project, 'a', "ka", Some(("one", false)), None).unwrap();
        let two =
            enqueue_expediting(&store, project, 'b', "kb", Some(("two", false)), None).unwrap();
        let refusal = enqueue_expediting(&store, project, 'c', "kc", Some(("three", false)), None)
            .unwrap_err();
        let text = code_of(refusal);
        assert!(text.starts_with(QUEUE_EXPEDITE_CAP_REACHED), "{text}");
        for held in [&one, &two] {
            assert!(text.contains(&held.id.to_string()), "{text}");
            assert!(
                text.contains(held.expedite_reason.as_deref().unwrap()),
                "{text}"
            );
        }
        assert!(
            store
                .get_rolling_queue_entry_by_key(Uuid::from_u128(0x3487), "kc")
                .unwrap()
                .is_none(),
            "a refused enqueue records nothing"
        );

        // The automatic rule never refuses: it is queued as an ordinary entry.
        let auto =
            enqueue_expediting(&store, project, 'd', "kd", Some(("qa", true)), None).unwrap();
        assert_eq!(auto.expedite_reason, None);
        assert_eq!(auto.state, RollingQueueEntryState::Queued);
        assert_eq!(
            store
                .live_expedited_rolling_queue_entries(Some(project))
                .unwrap()
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![one.id, two.id]
        );

        // Another project has its own cap.
        let other = Uuid::new_v4();
        assert!(
            enqueue_expediting(&store, other, 'e', "ke", Some(("other", false)), None)
                .unwrap()
                .expedite_reason
                .is_some()
        );

        // A replacement of an expedited entry takes its slot instead of needing a third.
        let replacement = enqueue_expediting(
            &store,
            project,
            'f',
            "kf",
            Some(("one, rebased", false)),
            Some((one.id, &one.source_commit)),
        )
        .unwrap();
        assert_eq!(replacement.expedite_reason.as_deref(), Some("one, rebased"));
        assert_eq!(
            store
                .live_expedited_rolling_queue_entries(Some(project))
                .unwrap()
                .len(),
            2
        );

        // A settled expedited entry frees its slot.
        let claimed = store
            .claim_rolling_queue_batch(Utc::now(), 1)
            .unwrap()
            .unwrap();
        store
            .settle_rolling_queue_entries(
                &[(
                    claimed.entries[0].id,
                    RollingQueueEntryState::Published,
                    published(&"9".repeat(40)),
                )],
                Utc::now(),
            )
            .unwrap();
        assert!(
            enqueue_expediting(&store, project, '1', "kg", Some(("again", false)), None)
                .unwrap()
                .expedite_reason
                .is_some()
        );
    }

    /// #3487: the expedite survives the filter derivation finishing and the
    /// lander-selects fallback, which both rewrite the stored filter list.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_expedite_survives_a_filter_derivation_rewrite_3487() {
        let store = store();
        let project = shared_project();
        let mut new = new_entry(Uuid::new_v4(), 'a', "ka");
        new.project_id = Some(project);
        let admission = ExpediteAdmission {
            reason: "incident".into(),
            automatic: false,
        };
        let (pending, _) = store
            .enqueue_rolling_queue_source_expediting(
                &new,
                None,
                None,
                true,
                None,
                &[],
                Some(&admission),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(pending.filters_state, Some(QueueFiltersState::Deriving));
        assert_eq!(pending.expedite_reason.as_deref(), Some("incident"));
        assert!(
            store
                .finish_rolling_queue_filter_derivation(pending.id, &["rsid=derived".to_string()])
                .unwrap()
        );
        let finished = store.get_rolling_queue_entry(pending.id).unwrap().unwrap();
        assert_eq!(finished.expedite_reason.as_deref(), Some("incident"));
        assert!(finished.test_filters.contains(&"rsid=derived".to_string()));

        let mut other = new_entry(Uuid::new_v4(), 'b', "kb");
        other.project_id = Some(project);
        let (entry, _) = store
            .enqueue_rolling_queue_source_expediting(
                &other,
                None,
                None,
                false,
                None,
                &[],
                Some(&admission),
                Utc::now(),
            )
            .unwrap();
        assert!(
            store
                .mark_rolling_queue_filters_lander_selects(entry.id, false)
                .unwrap()
        );
        let marked = store.get_rolling_queue_entry(entry.id).unwrap().unwrap();
        assert_eq!(marked.filters_state, Some(QueueFiltersState::LanderSelects));
        assert_eq!(marked.expedite_reason.as_deref(), Some("incident"));
    }

    /// #3487: `AgentListRollingQueue` rows (summary and full) say whether an
    /// entry is expedited and why; an ordinary row says neither.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_agent_list_shows_expedited_and_its_reason_3487() {
        use crate::store::manager_coordinator::tests::fixture;
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let project = config.project_id;
        let ordinary = enqueue_in(&store, project, seat, 'a', "k-a");
        let mut new = new_entry(seat, 'b', "k-b");
        new.project_id = Some(project);
        let (urgent, _) = store
            .enqueue_rolling_queue_source_expediting(
                &new,
                None,
                None,
                false,
                None,
                &[],
                Some(&ExpediteAdmission {
                    reason: "hub rsid SIGINT loop".into(),
                    automatic: false,
                }),
                Utc::now(),
            )
            .unwrap();
        for fields in [
            None,
            Some(rsi_common::rolling_queue::RollingQueueFieldsV1::Summary),
            Some(rsi_common::rolling_queue::RollingQueueFieldsV1::Full),
        ] {
            let rows = store
                .list_rolling_queue_for_agent(
                    seat,
                    &AgentListRollingQueueRequestV1 {
                        fields,
                        limit: Some(100),
                        ..Default::default()
                    },
                )
                .unwrap()
                .0;
            let find = |id: Uuid| rows.iter().find(|row| row.id == id).unwrap();
            assert!(find(urgent.id).expedited, "{fields:?}");
            assert_eq!(
                find(urgent.id).expedite_reason.as_deref(),
                Some("hub rsid SIGINT loop")
            );
            assert!(!find(ordinary.id).expedited);
            assert_eq!(find(ordinary.id).expedite_reason, None);
        }
    }
}
