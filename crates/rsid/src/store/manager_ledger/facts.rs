//! Durable manager ledger facts keyed by the work, never by the manager seat
//! (decision D19, Issues bf09774c / 559be8f1 / 8e05cffa).
//!
//! Work (with its embedded stage evidence, acceptance and integration),
//! ownership, dependency and migration-reservation records are identified by
//! `(project, kind, record_key)` and indexed by `(project, epic, kind,
//! work_key)`. The writing seat, scope and policy are provenance and write-time
//! fences only. In-flight authority (requests, decisions, intents, lifecycle
//! holds, operations, events) stays in the seat-scoped V2 records table.
use super::super::Store;
use super::super::harness_manager_v2::{ManagerRecordV2, now, refused};
use crate::error::{DaemonError, Result};
use rsi_common::{
    harness_manager::HarnessManagerConfigV1,
    harness_manager_v2::{MANAGER_V2_MAX_RECORDS, text},
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::Value;
use uuid::Uuid;

/// Record kinds whose identity is the work rather than the manager seat.
pub(crate) const WORK_FACT_KINDS: [&str; 4] = ["work", "dependency", "ownership", "migration"];

pub(crate) fn is_work_fact(kind: &str) -> bool {
    WORK_FACT_KINDS.contains(&kind)
}

/// Visibility of a durable fact listing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FactReach {
    /// Live facts of Epics inside the current manager scope: work limit,
    /// readiness and display of live work.
    Scoped,
    /// Live facts of every Epic of the project: correctness gates (ownership
    /// conflicts, dependency cycles and blockers) a narrowed scope must not hide.
    Project,
}

type FactRow = (String, String, i64, String, bool, String, String);

/// SQL predicate (fact alias `f`) for a LIVE fact: not archived, and its work
/// (the work row itself for `kind='work'`, whose `work_key` is its own key) is
/// neither archived nor integrated. The work payload has no cancelled state;
/// every Stage or Work revision clears `integration`, so a present integration
/// always means delivered at the current source. Terminal facts stay readable
/// by key (landed work is never re-admitted) but no longer count against the
/// budget or feed project-wide gates, so history cannot exhaust a project.
const LIVE_FACT: &str = "f.archived=0 AND NOT EXISTS(
    SELECT 1 FROM harness_manager_v2_work_facts w
     WHERE w.project_id=f.project_id AND w.kind='work' AND w.record_key=f.work_key
       AND (w.archived=1 OR json_type(w.payload_json,'$.integration')='object'))";

/// Durable fact budget per project, as an SQL integer.
fn fact_budget() -> i64 {
    i64::try_from(MANAGER_V2_MAX_RECORDS).unwrap_or(i64::MAX)
}

fn fact_record(kind: &str, row: FactRow) -> Result<ManagerRecordV2> {
    let (key, epic, row_version, payload, archived, created_at, updated_at) = row;
    Ok(ManagerRecordV2 {
        kind: kind.into(),
        key,
        epic_id: Some(
            Uuid::parse_str(&epic).map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
        ),
        row_version,
        payload: serde_json::from_str(&payload)?,
        archived,
        created_at,
        updated_at,
    })
}

fn fact_work_key(kind: &str, key: &str, payload: &Value) -> Result<String> {
    if kind == "work" {
        return Ok(key.into());
    }
    payload["work_key"]
        .as_str()
        .filter(|work_key| !work_key.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| refused("manager_v2_invalid_record"))
}

impl Store {
    /// Exact fact by durable identity. Deliberately not Epic-filtered: callers
    /// enforce Epic scope, and a filtered read would let an in-scope write
    /// silently replace an out-of-scope row sharing the key.
    pub(crate) fn manager_v2_fact(
        &self,
        project: Uuid,
        kind: &str,
        key: &str,
    ) -> Result<Option<ManagerRecordV2>> {
        let row: Option<FactRow> = self
            .conn
            .query_row(
                "SELECT record_key,epic_id,row_version,payload_json,archived,created_at,updated_at
                   FROM harness_manager_v2_work_facts
                  WHERE project_id=?1 AND kind=?2 AND record_key=?3",
                params![project.to_string(), kind, key],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(|row| fact_record(kind, row)).transpose()
    }

    /// Whether the exact fact is LIVE under the same predicate the scoped
    /// listings use (not archived; its work neither archived nor integrated).
    pub(crate) fn manager_v2_fact_is_live(
        &self,
        project: Uuid,
        kind: &str,
        key: &str,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_work_facts f
                  WHERE f.project_id=?1 AND f.kind=?2 AND f.record_key=?3 AND {LIVE_FACT})"
            ),
            params![project.to_string(), kind, key],
            |r| r.get(0),
        )?)
    }

    pub(crate) fn manager_v2_facts(
        &self,
        config: &HarnessManagerConfigV1,
        kind: &str,
        reach: FactReach,
    ) -> Result<Vec<ManagerRecordV2>> {
        let epics = match reach {
            FactReach::Scoped => Some(serde_json::to_string(&config.epic_ids)?),
            FactReach::Project => None,
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT f.record_key,f.epic_id,f.row_version,f.payload_json,f.archived,
                    f.created_at,f.updated_at
               FROM harness_manager_v2_work_facts f
              WHERE f.project_id=?1 AND f.kind=?2
                AND (?3 IS NULL OR f.epic_id IN (SELECT value FROM json_each(?3)))
                AND {LIVE_FACT}
              ORDER BY f.record_key LIMIT ?4"
        ))?;
        let raw = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    kind,
                    epics,
                    fact_budget() + 1
                ],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<FactRow>>>()?;
        if raw.len() > MANAGER_V2_MAX_RECORDS {
            return Err(refused("manager_v2_record_budget"));
        }
        raw.into_iter().map(|row| fact_record(kind, row)).collect()
    }

    /// Most recently updated terminal (integrated or archived) works in scope,
    /// newest first, at most `limit`: the bounded delivery history shown beside
    /// live work.
    /// The window is also bounded by association: it is the longest recency
    /// prefix whose works plus all their claims, edges and reservations fit the
    /// fact budget, so a history of heavily claimed works never overflows the
    /// association reads that follow.
    pub(crate) fn manager_v2_terminal_work_keys(
        &self,
        config: &HarnessManagerConfigV1,
        limit: usize,
    ) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT record_key FROM (
                SELECT f.record_key,f.updated_at,
                       sum(1 + (SELECT count(*) FROM harness_manager_v2_work_facts a
                                 WHERE a.project_id=f.project_id AND a.epic_id=f.epic_id
                                   AND a.kind<>'work' AND a.work_key=f.record_key))
                         OVER (ORDER BY f.updated_at DESC,f.record_key
                               ROWS UNBOUNDED PRECEDING) AS used
                  FROM harness_manager_v2_work_facts f
                 WHERE f.project_id=?1 AND f.kind='work'
                   AND f.epic_id IN (SELECT value FROM json_each(?2)) AND NOT ({LIVE_FACT}))
              WHERE used<=?4
              ORDER BY updated_at DESC,record_key LIMIT ?3"
        ))?;
        let keys = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    serde_json::to_string(&config.epic_ids)?,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    fact_budget()
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(keys)
    }

    /// Facts of `kind` that belong to the given works (bounded by the caller).
    pub(crate) fn manager_v2_facts_of_works(
        &self,
        project: Uuid,
        kind: &str,
        work_keys: &[String],
    ) -> Result<Vec<ManagerRecordV2>> {
        if work_keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT record_key,epic_id,row_version,payload_json,archived,created_at,updated_at
               FROM harness_manager_v2_work_facts
              WHERE project_id=?1 AND kind=?2 AND work_key IN (SELECT value FROM json_each(?3))
              ORDER BY record_key LIMIT ?4",
        )?;
        let raw = stmt
            .query_map(
                params![
                    project.to_string(),
                    kind,
                    serde_json::to_string(work_keys)?,
                    fact_budget() + 1
                ],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<FactRow>>>()?;
        if raw.len() > MANAGER_V2_MAX_RECORDS {
            return Err(refused("manager_v2_record_budget"));
        }
        raw.into_iter().map(|row| fact_record(kind, row)).collect()
    }

    /// Keyset page of one Epic's live facts for lead notices.
    pub(crate) fn manager_v2_fact_keys_after(
        &self,
        project: Uuid,
        epic: Uuid,
        kind: &str,
        after: &str,
        limit: i64,
    ) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT f.record_key FROM harness_manager_v2_work_facts f
              WHERE f.project_id=?1 AND f.epic_id=?2 AND f.kind=?3 AND f.record_key>?4
                AND {LIVE_FACT}
              ORDER BY f.record_key LIMIT ?5"
        ))?;
        let keys = stmt
            .query_map(
                params![project.to_string(), epic.to_string(), kind, after, limit],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(keys)
    }

    /// Write-time fence: only the project's current seat, at its current scope,
    /// may write a durable fact, and never under a revoked grant or one bound
    /// to another seat or scope. Returns the policy version recorded as
    /// provenance (`None` only when the project never had a V2 grant; every
    /// agent path has already required one through `manager_v2_authorize`).
    fn manager_v2_fact_writer(&self, config: &HarnessManagerConfigV1) -> Result<Option<i64>> {
        let current: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT manager_session_id,row_version FROM harness_manager_scopes WHERE project_id=?1",
                [config.project_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if current != Some((config.manager_session_id.to_string(), config.row_version)) {
            return Err(refused("manager_v2_scope_changed"));
        }
        match self.get_harness_manager_policy(config.project_id)? {
            // NULL provenance only for a project that never had a V2 grant:
            // every grant appends a `policy` event, so history, not the
            // one-row projection, is the proof.
            None if !self.manager_v2_policy_ever_granted(config.project_id)? => Ok(None),
            Some(policy)
                if !policy.revoked
                    && policy.manager_session_id == config.manager_session_id
                    && policy.scope_version == config.row_version =>
            {
                Ok(Some(policy.row_version))
            }
            // A revoked or foreign grant, or a lost projection after a grant.
            _ => Err(refused("manager_v2_policy_changed")),
        }
    }

    fn manager_v2_policy_ever_granted(&self, project: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_events
                            WHERE project_id=?1 AND kind='policy')
                 OR EXISTS(SELECT 1 FROM harness_manager_v2_operations
                            WHERE project_id=?1 AND kind='configure_policy')",
            [project.to_string()],
            |r| r.get(0),
        )?)
    }

    /// Compare-and-set write of one durable fact. Called by
    /// `manager_v2_put_record` after its shared key/size validation.
    pub(crate) fn manager_v2_put_fact(
        &self,
        config: &HarnessManagerConfigV1,
        kind: &str,
        key: &str,
        epic: Option<Uuid>,
        expected: i64,
        payload: &Value,
    ) -> Result<ManagerRecordV2> {
        let work_key = fact_work_key(kind, key, payload)?;
        text(&work_key, 256).map_err(refused)?;
        // A claim, edge or reservation belongs to its work's Epic.
        let epic = if kind == "work" {
            epic.ok_or_else(|| refused("manager_v2_invalid_record"))?
        } else {
            let owner = self
                .manager_v2_fact(config.project_id, "work", &work_key)?
                .and_then(|work| work.epic_id)
                .ok_or_else(|| refused("manager_v2_work_missing"))?;
            if epic.is_some_and(|epic| epic != owner) {
                return Err(refused("manager_v2_work_identity_changed"));
            }
            owner
        };
        if !config.epic_ids.contains(&epic) {
            return Err(refused("manager_v2_epic_out_of_scope"));
        }
        let policy_version = self.manager_v2_fact_writer(config)?;
        let prior = self.manager_v2_fact(config.project_id, kind, key)?;
        if let Some(prior) = &prior {
            if prior
                .epic_id
                .is_none_or(|id| !config.epic_ids.contains(&id))
            {
                return Err(refused("manager_v2_epic_out_of_scope"));
            }
            if kind != "migration" && prior.epic_id != Some(epic) {
                return Err(refused("manager_v2_work_identity_changed"));
            }
        }
        if prior.as_ref().map_or(0, |r| r.row_version) != expected {
            return Err(refused("manager_v2_record_changed"));
        }
        if prior.is_none() {
            // Only live facts consume the budget; delivered history does not.
            let count: i64 = self.conn.query_row(
                &format!(
                    "SELECT count(*) FROM harness_manager_v2_work_facts f
                      WHERE f.project_id=?1 AND {LIVE_FACT}"
                ),
                [config.project_id.to_string()],
                |r| r.get(0),
            )?;
            if count >= fact_budget() {
                return Err(refused("manager_v2_record_limit"));
            }
        }
        let next = expected
            .checked_add(1)
            .ok_or_else(|| refused("manager_v2_version_exhausted"))?;
        let stamp = now();
        self.conn.execute(
            "INSERT INTO harness_manager_v2_work_facts(project_id,kind,record_key,epic_id,work_key,
                 row_version,payload_json,archived,manager_session_id,scope_version,policy_version,
                 created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,0,?8,?9,?10,?11,?11)
             ON CONFLICT(project_id,kind,record_key) DO UPDATE SET
                 epic_id=excluded.epic_id,work_key=excluded.work_key,
                 row_version=excluded.row_version,payload_json=excluded.payload_json,
                 manager_session_id=excluded.manager_session_id,
                 scope_version=excluded.scope_version,policy_version=excluded.policy_version,
                 updated_at=excluded.updated_at",
            params![
                config.project_id.to_string(),
                kind,
                key,
                epic.to_string(),
                work_key,
                next,
                serde_json::to_string(payload)?,
                config.manager_session_id.to_string(),
                config.row_version,
                policy_version,
                stamp
            ],
        )?;
        self.manager_v2_fact(config.project_id, kind, key)?
            .ok_or_else(|| refused("manager_v2_record_unavailable"))
    }
    /// K14 (#672) retention: true when `session` (or a session sharing its
    /// sandbox custody, i.e. a rotation tip holding an author's custody) is
    /// the source of LIVE work that is sealed for review (a source commit is
    /// recorded) or accepted, and not yet integrated (`LIVE_FACT`).
    pub(crate) fn manager_v2_session_is_sealed_source(
        &self,
        project_id: Uuid,
        session: Uuid,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_work_facts f
                  WHERE f.project_id=?1 AND f.kind='work' AND {LIVE_FACT}
                    AND json_extract(f.payload_json,'$.source_session_id') IN (
                        SELECT s.id FROM sessions s
                         WHERE s.id=?2 OR (s.sandbox_custody_id IS NOT NULL
                           AND s.sandbox_custody_id=(SELECT c.sandbox_custody_id
                                FROM sessions c WHERE c.id=?2)))
                    AND (json_type(f.payload_json,'$.source_commit')='text'
                         OR json_type(f.payload_json,'$.acceptance')='object'))"
            ),
            params![project_id.to_string(), session.to_string()],
            |row| row.get(0),
        )?)
    }
}

/// Catalog objects created by V122, for fixtures and presence checks.
#[cfg(test)]
pub(crate) const V122_CATALOG_OBJECTS: [(&str, &str); 2] = [
    ("table", "harness_manager_v2_work_facts"),
    ("index", "harness_manager_v2_work_fact_identity"),
];

// RSI-RELEASED-MIGRATION-BEGIN: v122-manager-work-facts-migration
/// Schema version that introduces the work-facts table (V122, assigned by the
/// manager). This is the single source for the migration's version checks;
/// the `if version < 122` block in `store/mod.rs` must name the same literal
/// because the released-migration inventory parses it.
pub(crate) const WORK_FACTS_SCHEMA_VERSION: i32 = 122;

/// First defect that would make the V122 carry-forward lossy or ambiguous,
/// as `(defect, kind, record_key)` in deterministic order.
fn v122_carry_forward_defect(tx: &Transaction<'_>) -> Result<Option<(String, String, String)>> {
    Ok(tx
        .query_row(
            "SELECT defect,kind,record_key FROM (
                SELECT n.project_id,n.kind,n.record_key,
                       CASE WHEN n.epic_id IS NULL THEN 'epic_missing'
                            WHEN g.epics>1 THEN 'identity_conflict_epic'
                            WHEN n.kind<>'work' AND g.work_keys>1 THEN 'identity_conflict_work_key'
                            WHEN n.kind<>'work' AND json_type(n.payload_json,'$.work_key') IS NOT 'text'
                                THEN 'work_key_missing'
                            WHEN n.kind<>'work' AND length(CAST(json_extract(n.payload_json,'$.work_key')
                                     AS BLOB)) NOT BETWEEN 1 AND 256
                                THEN 'work_key_invalid'
                            WHEN n.kind='work' AND json_type(n.payload_json,'$.key') IS NOT NULL
                                 AND json_extract(n.payload_json,'$.key') IS NOT n.record_key
                                THEN 'key_mismatch'
                            WHEN json_type(n.payload_json,'$.epic_id') IS NOT NULL
                                 AND json_extract(n.payload_json,'$.epic_id') IS NOT n.epic_id
                                THEN 'epic_mismatch'
                       END AS defect
                  FROM (SELECT r.*, row_number() OVER (
                               PARTITION BY r.project_id,r.kind,r.record_key
                               ORDER BY r.scope_version DESC,r.updated_at DESC,r.row_version DESC,
                                        r.manager_session_id DESC) AS newest
                          FROM harness_manager_v2_records r
                         WHERE r.kind IN ('work','dependency','ownership','migration')) n
                  -- Rows of one identity across seats/scopes must agree on Epic
                  -- (and on work for claims, edges and reservations): a key
                  -- reused by another Epic or work is a collision, not history.
                  JOIN (SELECT project_id,kind,record_key,count(DISTINCT epic_id) AS epics,
                               count(DISTINCT json_extract(payload_json,'$.work_key')) AS work_keys
                          FROM harness_manager_v2_records
                         WHERE kind IN ('work','dependency','ownership','migration')
                         GROUP BY project_id,kind,record_key) g
                    ON g.project_id=n.project_id AND g.kind=n.kind AND g.record_key=n.record_key
                 WHERE n.newest=1)
              WHERE defect IS NOT NULL ORDER BY project_id,kind,record_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

/// Forward migration: create the work-keyed facts table and carry every
/// durable identity forward from the seat-scoped V2 records table.
///
/// Carry-forward rule: for each `(project_id, kind, record_key)` of a durable
/// kind, the single newest row wins, ordered by
/// `scope_version DESC, updated_at DESC, row_version DESC, manager_session_id
/// DESC`. `scope_version` is the project's monotone manager-scope row version,
/// so the newest scope wins across seats; the rest are deterministic
/// tie-breaks. Payload, row version, archive flag and timestamps copy verbatim
/// (policy digests reproduce byte-for-byte); provenance names the source seat
/// and scope, with an unknown (NULL) policy version. Source rows are untouched.
///
/// Nothing is dropped silently: if an identity's rows disagree on Epic (or on
/// work, for claims, edges and reservations) across seats, or its newest row
/// lacks an Epic, lacks a textual 1..=256-byte `work_key`, or carries a payload
/// `key`/`epic_id` that disagrees with its row identity, the migration fails
/// with `manager_v2_fact_carry_forward_invalid` and the database stays at V121.
/// The same key reused by later seats within one Epic is history and carries.
pub(crate) fn apply_work_facts_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != WORK_FACTS_SCHEMA_VERSION - 1 {
        return Err(DaemonError::Store(format!(
            "V{WORK_FACTS_SCHEMA_VERSION} requires exact V{} source, found V{version}",
            WORK_FACTS_SCHEMA_VERSION - 1
        )));
    }
    let defect = v122_carry_forward_defect(&tx)?;
    if let Some((defect, kind, key)) = defect {
        return Err(DaemonError::Store(format!(
            "V{WORK_FACTS_SCHEMA_VERSION} manager_v2_fact_carry_forward_invalid: {defect} ({kind} {key})"
        )));
    }
    tx.execute_batch(
        "CREATE TABLE harness_manager_v2_work_facts (
            project_id TEXT NOT NULL REFERENCES projects(id),
            kind TEXT NOT NULL CHECK(kind IN ('work','dependency','ownership','migration')),
            record_key TEXT NOT NULL CHECK(length(CAST(record_key AS BLOB)) BETWEEN 1 AND 256),
            epic_id TEXT NOT NULL REFERENCES sessions(id),
            work_key TEXT NOT NULL CHECK(length(CAST(work_key AS BLOB)) BETWEEN 1 AND 256),
            row_version INTEGER NOT NULL CHECK(row_version>0),
            payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(payload_json)<=65536),
            archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
            manager_session_id TEXT NOT NULL REFERENCES sessions(id),
            scope_version INTEGER NOT NULL CHECK(scope_version>0),
            policy_version INTEGER CHECK(policy_version IS NULL OR policy_version>0),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY(project_id,kind,record_key)
         );
         CREATE UNIQUE INDEX harness_manager_v2_work_fact_identity
            ON harness_manager_v2_work_facts(project_id,epic_id,kind,work_key,record_key);
         INSERT INTO harness_manager_v2_work_facts(project_id,kind,record_key,epic_id,work_key,
             row_version,payload_json,archived,manager_session_id,scope_version,policy_version,
             created_at,updated_at)
         SELECT project_id,kind,record_key,epic_id,
                CASE kind WHEN 'work' THEN record_key ELSE json_extract(payload_json,'$.work_key') END,
                row_version,payload_json,archived,manager_session_id,scope_version,NULL,
                created_at,updated_at
           FROM (SELECT r.*, row_number() OVER (
                        PARTITION BY r.project_id,r.kind,r.record_key
                        ORDER BY r.scope_version DESC,r.updated_at DESC,r.row_version DESC,
                                 r.manager_session_id DESC) AS newest
                   FROM harness_manager_v2_records r
                  WHERE r.kind IN ('work','dependency','ownership','migration'))
          WHERE newest=1;",
    )?;
    let foreign_key_errors: i64 = tx.query_row(
        "SELECT count(*) FROM pragma_foreign_key_check('harness_manager_v2_work_facts')",
        [],
        |row| row.get(0),
    )?;
    if foreign_key_errors != 0 {
        return Err(DaemonError::Store(format!(
            "V{WORK_FACTS_SCHEMA_VERSION} work facts migration found {foreign_key_errors} foreign-key violation(s)"
        )));
    }
    tx.execute(
        &format!("PRAGMA user_version = {WORK_FACTS_SCHEMA_VERSION}"),
        [],
    )?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v122-manager-work-facts-migration
