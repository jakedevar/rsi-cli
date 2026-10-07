//! Friction telemetry, the "andon" (#1333): append-only friction events, the
//! rollup managers and the operator read, and the sweep that files one
//! deduplicated kaizen Issue per repeating signature (V157,
//! `store/migrations/v157.rs`).
//!
//! Decisions:
//! - Events are written at the existing refusal and failure points with code
//!   tokens and ids only; the schema CHECKs refuse anything else.
//! - A signature is due when it occurs [`ANDON_MIN_OCCURRENCES`] times across
//!   [`ANDON_MIN_SESSIONS`] sessions of one project within
//!   [`ANDON_WINDOW_HOURS`]. Project-less events (an operator deploy) are
//!   rolled up but never filed: an Issue belongs to a project.
//! - One Issue per `(project, signature)`, ever: the `andon_filings` primary
//!   key and a deterministic UUIDv5 Issue id both enforce it. A recurrence
//!   after the Issue closes shows in the rollup against the filed Issue.
//! - At most [`ANDON_DAILY_FILING_CAP`] filings in any rolling 24 hours,
//!   rechecked inside each filing's IMMEDIATE transaction.

use super::Store;
use super::issues::{IssueWriteActor, issue_write_before_commit};
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::agent_jobs::{AgentJobV1, JobKind, JobState};
use rsi_common::friction::{
    ANDON_DAILY_FILING_CAP, ANDON_ISSUE_LABELS, ANDON_MIN_OCCURRENCES, ANDON_MIN_SESSIONS,
    ANDON_WINDOW_HOURS, AndonFilingV1, FRICTION_MAX_WINDOW_HOURS, FRICTION_ROLLUP_DEFAULT_LIMIT,
    FRICTION_ROLLUP_MAX_LIMIT, FrictionRollupRowV1, ListFrictionRollupRequestV1,
    ListFrictionRollupResultV1, NewFrictionEventV1, is_friction_evidence, is_friction_signature,
    signature_kind,
};
use rsi_common::friction::{FrictionKind, UNCLASSIFIED};
use rsi_common::rolling_queue::{RollingQueueEntryState, RollingQueueEntryV1, RollingQueueOutcome};
use rsi_common::types::NewIssue;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

/// Schema version of `friction_events` and `andon_filings`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const FRICTION_SCHEMA_VERSION: i32 = 157;

/// V157 catalog objects, for presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 11] = [
    ("table", "friction_events"),
    ("index", "friction_events_by_signature"),
    ("index", "friction_events_by_time"),
    ("trigger", "friction_events_no_update"),
    ("trigger", "friction_events_no_delete"),
    ("trigger", "friction_events_no_replace"),
    ("table", "andon_filings"),
    ("index", "andon_filings_by_time"),
    ("trigger", "andon_filings_no_update"),
    ("trigger", "andon_filings_no_delete"),
    ("trigger", "andon_filings_no_replace"),
];

/// Teardown for the fixture rewind (back to V156), newest object first.
#[cfg(test)]
pub(crate) const REWIND_SQL: &str = "DROP TRIGGER andon_filings_no_replace;
DROP TRIGGER andon_filings_no_delete;
DROP TRIGGER andon_filings_no_update;
DROP INDEX andon_filings_by_time;
DROP TABLE andon_filings;
DROP TRIGGER friction_events_no_replace;
DROP TRIGGER friction_events_no_delete;
DROP TRIGGER friction_events_no_update;
DROP INDEX friction_events_by_time;
DROP INDEX friction_events_by_signature;
DROP TABLE friction_events;";

/// UUIDv5 namespace of andon Issue ids (`<project>\0<signature>`).
const ANDON_ISSUE_NAMESPACE: Uuid = Uuid::from_u128(0x6a1d_0e5c_8f3b_4c7e_9a51_1333_0a4d_0f11);
/// Newest distinct evidence references shown per signature.
const EVIDENCE_PER_ROW: i64 = 5;

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn window_start(now: DateTime<Utc>, hours: u32) -> String {
    stamp(now - Duration::hours(i64::from(hours)))
}

fn parse_at(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|error| DaemonError::Store(format!("invalid friction timestamp: {error}")))
}

fn parse_uuid(raw: &str) -> Result<Uuid> {
    Uuid::parse_str(raw)
        .map_err(|error| DaemonError::Store(format!("invalid friction id: {error}")))
}

fn count(raw: i64) -> u64 {
    u64::try_from(raw).unwrap_or(0)
}

/// The andon Issue id of one `(project, signature)`.
#[must_use]
pub fn andon_issue_id(project_id: Uuid, signature: &str) -> Uuid {
    Uuid::new_v5(
        &ANDON_ISSUE_NAMESPACE,
        format!("{project_id}\0{signature}").as_bytes(),
    )
}

/// Append one friction event on `conn` (a connection or an open transaction).
/// The project comes from the session when the event names none. A signature
/// that is not code tokens is refused; an unclean evidence reference is
/// dropped. A refused statement leaves an enclosing transaction usable.
pub(crate) fn record_friction_in(
    conn: &Connection,
    event: &NewFrictionEventV1,
    now: DateTime<Utc>,
) -> Result<()> {
    if !is_friction_signature(&event.signature) {
        return Err(DaemonError::InvalidParam(
            "friction_signature_invalid".into(),
        ));
    }
    let project = match (event.project_id, event.session_id) {
        (Some(project), _) => Some(project),
        (None, Some(session)) => conn
            .query_row(
                "SELECT project_id FROM sessions WHERE id=?1",
                [session.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .and_then(|raw| Uuid::parse_str(&raw).ok()),
        (None, None) => None,
    };
    let evidence = event
        .evidence_ref
        .as_deref()
        .filter(|reference| is_friction_evidence(reference));
    conn.execute(
        "INSERT INTO friction_events(signature, session_id, project_id, evidence_ref, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            event.signature,
            event.session_id.map(|id| id.to_string()),
            project.map(|id| id.to_string()),
            evidence,
            stamp(now)
        ],
    )?;
    Ok(())
}

/// Best-effort [`record_friction_in`] for a recording point: friction
/// telemetry never fails the operation it observes, so an error is logged.
pub(crate) fn note_friction_in(conn: &Connection, event: &NewFrictionEventV1, now: DateTime<Utc>) {
    if let Err(error) = record_friction_in(conn, event, now) {
        tracing::warn!(signature = %event.signature, %error, "friction event not recorded");
    }
}

/// Fingerprint failing results in existing event metadata, then emit only at
/// the third occurrence. A separate transaction serializes counting and keeps
/// it durable across restarts, without another table or storing prose in
/// telemetry. Include the raw tool name in the hash before sanitizing its code.
/// Run after event commit so a failed telemetry write cannot roll it back.
pub(crate) fn note_repeated_tool_error(
    conn: &Connection,
    event: &rsi_common::types::ConversationEvent,
    event_id: i64,
) -> Result<()> {
    use rsi_common::types::EventType;
    use sha2::{Digest, Sha256};

    if event.event_type != EventType::ToolResult
        || event.metadata.as_deref().and_then(|m| m.get("is_error")) != Some(&Value::Bool(true))
    {
        return Ok(());
    }
    let Some(tool_use_id) = event.tool_use_id.as_deref() else {
        return Ok(());
    };
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let conn = &tx;
    let name: Option<String> = conn
        .query_row(
            "SELECT tool_name FROM conversation_events
         WHERE session_id=?1 AND event_type='ToolUse' AND tool_use_id=?2 AND id<?3
         ORDER BY id DESC LIMIT 1",
            params![event.session_id.to_string(), tool_use_id, event_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let name = name.as_deref().unwrap_or(UNCLASSIFIED);
    let mut hash = Sha256::new();
    hash.update((name.len() as u64).to_be_bytes());
    hash.update(name.as_bytes());
    hash.update(event.content.as_bytes());
    let fingerprint = format!("{:x}", hash.finalize()); // sql-dynamic-ok: a bound digest, not SQL
    conn.execute(
        "UPDATE conversation_events
         SET metadata=json_set(metadata, '$.friction_tool_error_sha256', ?2) WHERE id=?1",
        params![event_id, fingerprint],
    )?;
    let occurrences: i64 = conn.query_row(
        "SELECT COUNT(*) FROM conversation_events
         WHERE session_id=?1 AND event_type='ToolResult'
           AND json_extract(metadata, '$.is_error')=1
           AND json_extract(metadata, '$.friction_tool_error_sha256')=?2",
        params![event.session_id.to_string(), fingerprint],
        |row| row.get(0),
    )?;
    if occurrences == 3 {
        let friction = NewFrictionEventV1::new(FrictionKind::ToolError, &[name, "repeated"])
            .session(Some(event.session_id))
            .evidence("session", event.session_id);
        record_friction_in(conn, &friction, Utc::now())?;
    }
    tx.commit()?;
    Ok(())
}

/// Merge-queue recording point: a refused or failed landing, as
/// `lander:<state>:<refusal code | tests_failed | unclassified>`.
pub(crate) fn note_lander_friction_in(
    conn: &Connection,
    entry: &RollingQueueEntryV1,
    terminal: RollingQueueEntryState,
    outcome: &RollingQueueOutcome,
    now: DateTime<Utc>,
) {
    if !matches!(
        terminal,
        RollingQueueEntryState::Refused | RollingQueueEntryState::Failed
    ) {
        return;
    }
    let cause = match outcome.refusal.as_deref() {
        Some(code) => code,
        None if !outcome.failing_tests.is_empty() => "tests_failed",
        None => UNCLASSIFIED,
    };
    let event = NewFrictionEventV1::new(FrictionKind::Lander, &[terminal.as_str(), cause])
        .session(Some(entry.source_session_id))
        .evidence("merge_queue", entry.id);
    note_friction_in(conn, &event, now);
}

/// Background-job recording point: any lost job, or a failed landing job, as
/// `agent_job:<kind>:<state>`. Failed test and build jobs are ordinary
/// iteration, not friction.
pub(crate) fn note_agent_job_friction_in(
    conn: &Connection,
    job: &AgentJobV1,
    terminal: JobState,
    now: DateTime<Utc>,
) {
    let friction = match terminal {
        JobState::Lost => true,
        JobState::Failed => job.kind == JobKind::Landing,
        JobState::Queued | JobState::Running | JobState::Succeeded => false,
    };
    if friction {
        let event = NewFrictionEventV1::new(
            FrictionKind::AgentJob,
            &[job.kind.as_str(), terminal.as_str()],
        )
        .session(Some(job.owner_session_id))
        .evidence("job", job.id);
        note_friction_in(conn, &event, now);
    }
}

fn andon_filings_since(conn: &Connection, since: &str) -> Result<u64> {
    let filed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM andon_filings WHERE filed_at>=?1",
        [since],
        |row| row.get(0),
    )?;
    Ok(count(filed))
}

struct Candidate {
    project_id: Uuid,
    signature: String,
    occurrences: u64,
    sessions: u64,
    first_at: String,
    last_at: String,
}

impl Store {
    /// Record one friction event now.
    ///
    /// # Errors
    /// `friction_signature_invalid`, or a persistence error.
    pub fn record_friction_event(&self, event: &NewFrictionEventV1) -> Result<()> {
        record_friction_in(&self.conn, event, Utc::now())
    }

    /// [`Self::record_friction_event`] at a fixed instant (tests and replays).
    ///
    /// # Errors
    /// As [`Self::record_friction_event`].
    pub fn record_friction_event_at(
        &self,
        event: &NewFrictionEventV1,
        now: DateTime<Utc>,
    ) -> Result<()> {
        record_friction_in(&self.conn, event, now)
    }

    fn evidence_refs(
        &self,
        project: Option<&str>,
        signature: &str,
        since: &str,
        epic: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare_cached(
            "WITH RECURSIVE tree(id) AS (
                 SELECT ?5 UNION SELECT s.id FROM sessions s JOIN tree ON s.parent_id=tree.id),
             scope(id) AS (
                 SELECT id FROM tree
                 UNION SELECT lead_session_id FROM sessions WHERE id=?5 AND lead_session_id IS NOT NULL)
             SELECT evidence_ref FROM friction_events
             WHERE project_id IS ?1 AND signature=?2 AND recorded_at>=?3
               AND evidence_ref IS NOT NULL
               AND (?5 IS NULL OR session_id IN (SELECT id FROM scope))
             GROUP BY evidence_ref ORDER BY MAX(id) DESC LIMIT ?4",
        )?;
        let rows = statement
            .query_map(
                params![project, signature, since, EVIDENCE_PER_ROW, epic],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The friction rollup: one row per `(project, signature)` seen in the
    /// window, most occurrences first.
    ///
    /// # Errors
    /// `friction_window_invalid`, `friction_limit_invalid`, or a persistence
    /// error.
    pub fn friction_rollup(
        &self,
        request: &ListFrictionRollupRequestV1,
        now: DateTime<Utc>,
    ) -> Result<ListFrictionRollupResultV1> {
        self.friction_rollup_scoped(request, now, None)
    }

    /// [`Self::friction_rollup`] over one Epic's sessions only (an Epic view,
    /// #1395): the Epic, its lead and every descendant, the session set of
    /// the #1328 area fleet scope, walked without a cap. Counts, sessions and
    /// evidence come from those sessions alone. `due` stays sound: a scoped
    /// row over the threshold implies the project-wide row is too.
    fn friction_rollup_scoped(
        &self,
        request: &ListFrictionRollupRequestV1,
        now: DateTime<Utc>,
        epic: Option<Uuid>,
    ) -> Result<ListFrictionRollupResultV1> {
        let window_hours = request.window_hours.unwrap_or(ANDON_WINDOW_HOURS);
        if !(1..=FRICTION_MAX_WINDOW_HOURS).contains(&window_hours) {
            return Err(DaemonError::InvalidParam("friction_window_invalid".into()));
        }
        let limit = request.limit.unwrap_or(FRICTION_ROLLUP_DEFAULT_LIMIT);
        if !(1..=FRICTION_ROLLUP_MAX_LIMIT).contains(&limit) {
            return Err(DaemonError::InvalidParam("friction_limit_invalid".into()));
        }
        let since = window_start(now, window_hours);
        let project_filter = request.project_id.map(|id| id.to_string());
        let epic_filter = epic.map(|id| id.to_string());
        let mut statement = self.conn.prepare(
            "WITH RECURSIVE tree(id) AS (
                 SELECT ?4 UNION SELECT s.id FROM sessions s JOIN tree ON s.parent_id=tree.id),
             scope(id) AS (
                 SELECT id FROM tree
                 UNION SELECT lead_session_id FROM sessions WHERE id=?4 AND lead_session_id IS NOT NULL)
             SELECT e.project_id, e.signature, COUNT(*), COUNT(DISTINCT e.session_id),
                    MIN(e.recorded_at), MAX(e.recorded_at), f.issue_id, i.display_number
             FROM friction_events e
             LEFT JOIN andon_filings f ON f.project_id=e.project_id AND f.signature=e.signature
             LEFT JOIN issues i ON i.id=f.issue_id
             WHERE e.recorded_at>=?1 AND (?2 IS NULL OR e.project_id=?2)
               AND (?4 IS NULL OR e.session_id IN (SELECT id FROM scope))
             GROUP BY e.project_id, e.signature
             ORDER BY COUNT(*) DESC, MAX(e.recorded_at) DESC, e.signature, e.project_id
             LIMIT ?3",
        )?;
        type Raw = (
            Option<String>,
            String,
            i64,
            i64,
            String,
            String,
            Option<String>,
            Option<i64>,
        );
        let raw: Vec<Raw> = statement
            .query_map(
                params![since, project_filter, i64::from(limit) + 1, epic_filter],
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
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let truncated = raw.len() > limit as usize;
        let mut rows = Vec::with_capacity(raw.len().min(limit as usize));
        for (project, signature, occurrences, sessions, first, last, issue, number) in
            raw.into_iter().take(limit as usize)
        {
            let occurrences = count(occurrences);
            let sessions = count(sessions);
            let filed_issue_id = issue.as_deref().map(parse_uuid).transpose()?;
            let evidence_refs = self.evidence_refs(
                project.as_deref(),
                &signature,
                &since,
                epic_filter.as_deref(),
            )?;
            let project_id = project.as_deref().map(parse_uuid).transpose()?;
            // The andon's own window decides `due`; another window cannot.
            let fileable = window_hours == ANDON_WINDOW_HOURS && project_id.is_some();
            let over_threshold =
                occurrences >= ANDON_MIN_OCCURRENCES && sessions >= ANDON_MIN_SESSIONS;
            rows.push(FrictionRollupRowV1 {
                project_id,
                kind: signature_kind(&signature).to_string(),
                due: fileable && over_threshold && filed_issue_id.is_none(),
                signature,
                occurrences,
                sessions,
                first_at: parse_at(&first)?,
                last_at: parse_at(&last)?,
                evidence_refs,
                filed_issue_id,
                filed_display_number: number.map(count),
            });
        }
        Ok(ListFrictionRollupResultV1 {
            window_hours,
            observed_at: now,
            rows,
            truncated,
            filings_last_24h: andon_filings_since(
                &self.conn,
                &window_start(now, ANDON_WINDOW_HOURS),
            )?,
            daily_filing_cap: ANDON_DAILY_FILING_CAP,
        })
    }

    /// `AgentManagerInspect {section:"friction"}` rows: the project's rollup
    /// over the default window (at most the largest rollup page), keyed by
    /// signature for the inspect pager and shaped for the board renderer.
    /// With `epic` (an area's required Epic filter, or a manager's chosen
    /// one) only that Epic's sessions count, so a sibling Epic's telemetry
    /// never reaches an area manager (#1395). The flag is false when the
    /// rollup was truncated.
    pub(crate) fn manager_v2_friction_rows(
        &self,
        project_id: Uuid,
        epic: Option<Uuid>,
    ) -> Result<(Vec<Value>, bool)> {
        let rollup = self.friction_rollup_scoped(
            &ListFrictionRollupRequestV1 {
                project_id: Some(project_id),
                window_hours: None,
                limit: Some(FRICTION_ROLLUP_MAX_LIMIT),
            },
            Utc::now(),
            epic,
        )?;
        let rows = rollup
            .rows
            .into_iter()
            .map(|row| {
                let state = match row.filed_display_number {
                    Some(number) => format!("filed #{number}"),
                    None if row.due => "due".to_string(),
                    None => "watching".to_string(),
                };
                let mut value = serde_json::to_value(&row).unwrap_or(Value::Null);
                if let Value::Object(map) = &mut value {
                    map.insert("type".into(), json!("friction"));
                    map.insert("key".into(), json!(row.signature));
                    map.insert(
                        "title".into(),
                        json!(format!(
                            "{} ×{} · {} sessions",
                            row.signature, row.occurrences, row.sessions
                        )),
                    );
                    map.insert("state".into(), json!(state));
                }
                value
            })
            .collect();
        Ok((rows, !rollup.truncated))
    }

    fn andon_candidates(&self, since: &str, room: u64) -> Result<Vec<Candidate>> {
        let mut statement = self.conn.prepare(
            "SELECT e.project_id, e.signature, COUNT(*), COUNT(DISTINCT e.session_id),
                    MIN(e.recorded_at), MAX(e.recorded_at)
             FROM friction_events e
             WHERE e.recorded_at>=?1 AND e.project_id IS NOT NULL
               AND EXISTS(SELECT 1 FROM projects p WHERE p.id=e.project_id)
               AND NOT EXISTS(SELECT 1 FROM andon_filings f
                              WHERE f.project_id=e.project_id AND f.signature=e.signature)
             GROUP BY e.project_id, e.signature
             HAVING COUNT(*)>=?2 AND COUNT(DISTINCT e.session_id)>=?3
             ORDER BY COUNT(*) DESC, MIN(e.recorded_at), e.signature, e.project_id
             LIMIT ?4",
        )?;
        let raw = statement
            .query_map(
                params![
                    since,
                    i64::try_from(ANDON_MIN_OCCURRENCES).unwrap_or(i64::MAX),
                    i64::try_from(ANDON_MIN_SESSIONS).unwrap_or(i64::MAX),
                    i64::try_from(room).unwrap_or(i64::MAX)
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(
                |(project, signature, occurrences, sessions, first_at, last_at)| {
                    Ok(Candidate {
                        project_id: parse_uuid(&project)?,
                        signature,
                        occurrences: count(occurrences),
                        sessions: count(sessions),
                        first_at,
                        last_at,
                    })
                },
            )
            .collect()
    }

    fn andon_sessions(&self, candidate: &Candidate, since: &str) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT session_id FROM friction_events
             WHERE project_id=?1 AND signature=?2 AND recorded_at>=?3 AND session_id IS NOT NULL
             GROUP BY session_id ORDER BY MAX(id) DESC LIMIT ?4",
        )?;
        let rows = statement
            .query_map(
                params![
                    candidate.project_id.to_string(),
                    candidate.signature,
                    since,
                    EVIDENCE_PER_ROW
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// File one kaizen Issue for each due signature, up to the daily cap.
    /// Idempotent: a filed signature is never due again, and a replay of the
    /// same filing reuses its deterministic Issue id.
    ///
    /// # Errors
    /// A persistence error; filings committed before it remain.
    pub fn andon_sweep(&self, now: DateTime<Utc>) -> Result<Vec<AndonFilingV1>> {
        let since = window_start(now, ANDON_WINDOW_HOURS);
        let filed = andon_filings_since(&self.conn, &since)?;
        let room = ANDON_DAILY_FILING_CAP.saturating_sub(filed);
        if room == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for candidate in self.andon_candidates(&since, room)? {
            let evidence = self.evidence_refs(
                Some(&candidate.project_id.to_string()),
                &candidate.signature,
                &since,
                None,
            )?;
            let sessions = self.andon_sessions(&candidate, &since)?;
            if let Some(filing) = self.file_andon_issue(&candidate, &evidence, &sessions, now)? {
                out.push(filing);
            }
        }
        Ok(out)
    }

    fn file_andon_issue(
        &self,
        candidate: &Candidate,
        evidence: &[String],
        sessions: &[String],
        now: DateTime<Utc>,
    ) -> Result<Option<AndonFilingV1>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let already: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM andon_filings WHERE project_id=?1 AND signature=?2)",
            params![candidate.project_id.to_string(), candidate.signature],
            |row| row.get(0),
        )?;
        if already
            || andon_filings_since(&tx, &window_start(now, ANDON_WINDOW_HOURS))?
                >= ANDON_DAILY_FILING_CAP
        {
            return Ok(None);
        }
        let issue_id = andon_issue_id(candidate.project_id, &candidate.signature);
        let new = NewIssue {
            project_id: candidate.project_id,
            title: format!(
                "andon: {} repeated {}× across {} sessions",
                candidate.signature, candidate.occurrences, candidate.sessions
            ),
            body: andon_issue_body(candidate, evidence, sessions),
            priority: None,
            labels: ANDON_ISSUE_LABELS.iter().map(ToString::to_string).collect(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        };
        let created = Self::idempotent_issue_in_tx(
            &tx,
            issue_id,
            &new,
            IssueWriteActor::System {
                label: "rsi:andon".to_string(),
            },
        )?;
        tx.execute(
            "INSERT INTO andon_filings(project_id, signature, issue_id, occurrences, sessions, filed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                candidate.project_id.to_string(),
                candidate.signature,
                issue_id.to_string(),
                i64::try_from(candidate.occurrences).unwrap_or(i64::MAX),
                i64::try_from(candidate.sessions).unwrap_or(i64::MAX),
                stamp(now)
            ],
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(Some(AndonFilingV1 {
            project_id: candidate.project_id,
            signature: candidate.signature.clone(),
            issue_id,
            display_number: count(created.issue.display_number),
            occurrences: candidate.occurrences,
            sessions: candidate.sessions,
        }))
    }
}

fn andon_issue_body(candidate: &Candidate, evidence: &[String], sessions: &[String]) -> String {
    let list = |items: &[String]| {
        if items.is_empty() {
            "none recorded".to_string()
        } else {
            items
                .iter()
                .map(|item| format!("`{item}`"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    format!(
        "## Friction signal (auto-filed by the andon, #1333)\n\
         - signature: `{signature}`\n\
         - kind: `{kind}`\n\
         - {occurrences} occurrences across {session_count} sessions in the last {window} h \
         (first {first}, last {last})\n\
         - evidence: {evidence}\n\
         - recent sessions: {sessions}\n\n\
         ## Intent\n\
         Find why this signal repeats and remove its cause (code, guidance or process), \
         or record why it is expected. The daemon files one Issue per signature; live counts \
         are in the operator's `ListFrictionRollup` and in \
         `AgentManagerInspect {{section:\"friction\"}}`.\n",
        signature = candidate.signature,
        kind = signature_kind(&candidate.signature),
        occurrences = candidate.occurrences,
        session_count = candidate.sessions,
        window = ANDON_WINDOW_HOURS,
        first = candidate.first_at,
        last = candidate.last_at,
        evidence = list(evidence),
        sessions = list(sessions),
    )
}

#[cfg(test)]
#[path = "friction_tests.rs"]
mod tests;
