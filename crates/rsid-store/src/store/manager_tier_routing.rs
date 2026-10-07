//! Fractal manager hierarchy S4 (#1238, plan §3 "Up, N levels" and "Down",
//! §4 I5, §5 M2): reports and escalations climb the chain one parent at a
//! time as mail and end at the operator; down-mail reaches any descendant.
//!
//! [`Store::tier_parent_of`] is the one routing primitive:
//! - `Area a` -> its parent area, or `Project(p)` under the project root;
//! - `Project p` -> the deepest active portfolio node covering `p`, else the
//!   operator;
//! - `Portfolio n` -> its active grant's live parent node, else the operator.
//!
//! Mail never carries authority. A message to a seat is a durable one-shot
//! scheduled resume wake on the `global_manager_messages` delivery path: it
//! is delivered at the recipient's next idle boundary and wakes an idle
//! recipient. It is not #1183 agent mail, so it is never delivered mid-turn.
//! Its delivery fence re-checks both ends, and grant
//! replacement, revocation and seat displacement retire queued rows. A report
//! from a root is an operator notice row. An escalation that leaves a project
//! root becomes a chain of hops here (`manager_tier_escalations`), each hop
//! change an immutable event; a ruling returns down the recorded hops to the
//! source seat and never answers a human approval.

use chrono::{SecondsFormat, Utc};
use rsi_common::global_manager::{
    AgentGlobalSendRequestV1, AgentReportToGlobalRequestV1, GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT,
    GLOBAL_MANAGER_MAILBOX_FULL, GLOBAL_MANAGER_NOT_SEAT, GLOBAL_PROJECT_HAS_NO_MANAGER,
    GLOBAL_PROJECT_NOT_IN_GRANT, GLOBAL_REPORT_NOT_AUTHORIZED, GlobalManagerMessageReceiptV1,
    MANAGER_PROJECT_NOT_IN_SCOPE,
};
use rsi_common::harness_manager::{
    AgentManagerResolveEscalationRequestV1, ManagerNodeEscalationStateV1, ManagerNodeEscalationV1,
};
use rsi_common::manager_tier_routing::{
    AgentReportUpRequestV1, AgentSendDownRequestV1, ListOperatorEscalationsResultV1,
    MANAGER_ESCALATION_FORWARDED_ABOVE, MANAGER_TARGET_NOT_DESCENDANT,
    MANAGER_TIER_IDEMPOTENCY_CONFLICT, MANAGER_TIER_INVALID_REQUEST, MANAGER_TIER_MAILBOX_FULL,
    MANAGER_TIER_MAX_PENDING, MANAGER_TIER_NOT_NODE_SEAT, MANAGER_TIER_TARGET_UNKNOWN,
    MANAGER_TIER_TARGET_VACANT, ManagerNodeRefV1, ManagerTierEscalationHopV1,
    ManagerTierMessageReceiptV1, OPERATOR_ESCALATION_NOT_FOUND, OPERATOR_ESCALATION_NOT_OPEN,
    OPERATOR_NOTICE_NOT_FOUND, OPERATOR_QUEUE_LIMIT, OPERATOR_REF, OperatorNoticeV1,
    RuleOperatorEscalationRequestV1, UndeliveredTierMailV1,
};
use rsi_common::types::{
    Recurrence, ScheduleSpec, ScheduledJob, SessionKind, SessionStatus, WakeMode,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::Store;
use super::portfolio_nodes;
use crate::error::{DaemonError, Result};

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: tier-routing-migration
/// One stored node reference, checked in SQL: `operator`, `portfolio:<uuid>`,
/// `project:<uuid>` or `area:<uuid>` (canonical lowercase UUIDs).
macro_rules! ref_check {
    ($col:literal) => {
        concat!(
            "(",
            $col,
            "='operator'",
            " OR (substr(",
            $col,
            ",1,10)='portfolio:' AND length(",
            $col,
            ")=46 AND rsi_uuid_is_canonical(substr(",
            $col,
            ",11)))",
            " OR (substr(",
            $col,
            ",1,8)='project:' AND length(",
            $col,
            ")=44 AND rsi_uuid_is_canonical(substr(",
            $col,
            ",9)))",
            " OR (substr(",
            $col,
            ",1,5)='area:' AND length(",
            $col,
            ")=41 AND rsi_uuid_is_canonical(substr(",
            $col,
            ",6))))"
        )
    };
}

/// Provisional schema version of N-level routing (M2).
pub(crate) const TIER_ROUTING_SCHEMA_VERSION: i32 = 155;

/// Tier mail: rows are retained; only `state`, `settle_reason` and
/// `updated_at` change. `state`: queued -> claimed|delivered|retired|failed|
/// uncertain; a continuation's effect claim moves queued -> claimed and its
/// settlement claimed -> delivered|failed|uncertain (#1266, the #945 rule: at
/// most once, an uncertain result is shown and never auto-replayed). Every
/// other state is final; `failed` and `uncertain` carry their reason.
const CATALOG_MESSAGES: &str = concat!(
    "CREATE TABLE manager_tier_messages (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    direction TEXT NOT NULL CHECK(direction IN ('up','down')),
    kind TEXT NOT NULL CHECK(kind IN ('message','report','escalation','ruling')),
    source_ref TEXT NOT NULL CHECK",
    ref_check!("source_ref"),
    ",
    target_ref TEXT NOT NULL CHECK",
    ref_check!("target_ref"),
    ",
    project_id TEXT CHECK(project_id IS NULL OR rsi_uuid_is_canonical(project_id)),
    source_session_id TEXT CHECK(source_session_id IS NULL OR rsi_uuid_is_canonical(source_session_id)),
    target_session_id TEXT CHECK(target_session_id IS NULL OR rsi_uuid_is_canonical(target_session_id)),
    source_grant_version INTEGER CHECK(source_grant_version IS NULL OR source_grant_version>0),
    target_grant_version INTEGER CHECK(target_grant_version IS NULL OR target_grant_version>0),
    body TEXT NOT NULL CHECK(length(body) BETWEEN 1 AND 65536),
    body_digest TEXT NOT NULL CHECK(length(body_digest)=64),
    idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 160),
    state TEXT NOT NULL CHECK(state IN ('queued','claimed','delivered','retired','failed','uncertain')),
    settle_reason TEXT CHECK(settle_reason IS NULL OR length(settle_reason) BETWEEN 1 AND 1024),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    UNIQUE(source_session_id, idempotency_key),
    CHECK((state IN ('failed','uncertain'))=(settle_reason IS NOT NULL)),
    CHECK(source_ref<>target_ref),
    CHECK((source_ref='operator')=(source_session_id IS NULL)),
    CHECK((target_ref='operator')=(target_session_id IS NULL)),
    CHECK(target_ref<>'operator' OR direction='up')
);
CREATE UNIQUE INDEX manager_tier_messages_operator_key ON manager_tier_messages(idempotency_key) WHERE source_session_id IS NULL;
CREATE INDEX manager_tier_messages_by_target ON manager_tier_messages(target_ref,state,created_at);
CREATE INDEX manager_tier_messages_by_source ON manager_tier_messages(source_ref,state);
CREATE INDEX manager_tier_messages_by_state ON manager_tier_messages(state,updated_at,id);
CREATE TRIGGER manager_tier_messages_no_delete BEFORE DELETE ON manager_tier_messages
 BEGIN SELECT RAISE(ABORT,'manager tier messages are retained'); END;
CREATE TRIGGER manager_tier_messages_identity_immutable BEFORE UPDATE ON manager_tier_messages
 WHEN NEW.id IS NOT OLD.id OR NEW.direction IS NOT OLD.direction OR NEW.kind IS NOT OLD.kind
   OR NEW.source_ref IS NOT OLD.source_ref OR NEW.target_ref IS NOT OLD.target_ref
   OR NEW.project_id IS NOT OLD.project_id OR NEW.source_session_id IS NOT OLD.source_session_id
   OR NEW.target_session_id IS NOT OLD.target_session_id
   OR NEW.source_grant_version IS NOT OLD.source_grant_version
   OR NEW.target_grant_version IS NOT OLD.target_grant_version OR NEW.body IS NOT OLD.body
   OR NEW.body_digest IS NOT OLD.body_digest OR NEW.idempotency_key IS NOT OLD.idempotency_key
   OR NEW.created_at IS NOT OLD.created_at
 BEGIN SELECT RAISE(ABORT,'only state, settle_reason and updated_at of a manager tier message change'); END;
CREATE TRIGGER manager_tier_messages_state_final BEFORE UPDATE ON manager_tier_messages
 WHEN (NEW.state IS NOT OLD.state AND NOT (
         (OLD.state='queued' AND NEW.state IN ('claimed','delivered','retired','failed','uncertain'))
         OR (OLD.state='claimed' AND NEW.state IN ('delivered','failed','uncertain'))))
   OR (OLD.state NOT IN ('queued','claimed') AND NEW.settle_reason IS NOT OLD.settle_reason)
 BEGIN SELECT RAISE(ABORT,'a settled manager tier message is final'); END;"
);

/// Escalation hops above a project root and their immutable events. A hop
/// changes only `state` (open -> forwarded|ruled|retired, then final),
/// `ruling` and `updated_at`.
const CATALOG_ESCALATIONS: &str = concat!(
    "CREATE TABLE manager_tier_escalations (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    escalation_id TEXT NOT NULL REFERENCES manager_node_escalations(id) ON DELETE RESTRICT,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(project_id)),
    hop INTEGER NOT NULL CHECK(hop>0),
    source_ref TEXT NOT NULL CHECK(source_ref<>'operator' AND ",
    ref_check!("source_ref"),
    "),
    target_ref TEXT NOT NULL CHECK(target_ref='operator' OR (substr(target_ref,1,10)='portfolio:' AND length(target_ref)=46 AND rsi_uuid_is_canonical(substr(target_ref,11)))),
    actor_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(actor_session_id)),
    target_session_id TEXT CHECK(target_session_id IS NULL OR rsi_uuid_is_canonical(target_session_id)),
    target_grant_version INTEGER CHECK(target_grant_version IS NULL OR target_grant_version>0),
    state TEXT NOT NULL CHECK(state IN ('open','forwarded','ruled','retired')),
    ruling TEXT CHECK(ruling IS NULL OR length(ruling) BETWEEN 1 AND 8192),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    UNIQUE(escalation_id, hop),
    CHECK((target_ref='operator')=(target_session_id IS NULL)),
    CHECK((target_ref='operator')=(target_grant_version IS NULL)),
    CHECK((state='ruled')=(ruling IS NOT NULL))
);
CREATE UNIQUE INDEX manager_tier_escalations_one_open ON manager_tier_escalations(escalation_id) WHERE state='open';
CREATE INDEX manager_tier_escalations_addressed ON manager_tier_escalations(target_ref,state,created_at);
CREATE TRIGGER manager_tier_escalations_no_delete BEFORE DELETE ON manager_tier_escalations
 BEGIN SELECT RAISE(ABORT,'manager tier escalations are retained'); END;
CREATE TRIGGER manager_tier_escalations_identity_immutable BEFORE UPDATE ON manager_tier_escalations
 WHEN NEW.id IS NOT OLD.id OR NEW.escalation_id IS NOT OLD.escalation_id
   OR NEW.project_id IS NOT OLD.project_id OR NEW.hop IS NOT OLD.hop
   OR NEW.source_ref IS NOT OLD.source_ref OR NEW.target_ref IS NOT OLD.target_ref
   OR NEW.actor_session_id IS NOT OLD.actor_session_id
   OR NEW.target_session_id IS NOT OLD.target_session_id
   OR NEW.target_grant_version IS NOT OLD.target_grant_version OR NEW.created_at IS NOT OLD.created_at
 BEGIN SELECT RAISE(ABORT,'only state, ruling and updated_at of a manager tier escalation change'); END;
CREATE TRIGGER manager_tier_escalations_closed_final BEFORE UPDATE ON manager_tier_escalations
 WHEN OLD.state<>'open'
 BEGIN SELECT RAISE(ABORT,'a closed manager tier escalation hop is final'); END;
CREATE TABLE manager_tier_escalation_events (
    hop_id TEXT NOT NULL REFERENCES manager_tier_escalations(id) ON DELETE RESTRICT,
    seq INTEGER NOT NULL CHECK(seq>0),
    action TEXT NOT NULL CHECK(action IN ('opened','forwarded','ruled','returned','retired')),
    actor_ref TEXT NOT NULL CHECK",
    ref_check!("actor_ref"),
    ",
    actor_session_id TEXT CHECK(actor_session_id IS NULL OR rsi_uuid_is_canonical(actor_session_id)),
    ruling TEXT CHECK(ruling IS NULL OR length(ruling) BETWEEN 1 AND 8192),
    idempotency_key TEXT CHECK(idempotency_key IS NULL OR length(idempotency_key) BETWEEN 1 AND 160),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    PRIMARY KEY(hop_id, seq),
    CHECK((actor_ref='operator')=(actor_session_id IS NULL))
);
CREATE UNIQUE INDEX manager_tier_escalation_events_operator_key ON manager_tier_escalation_events(idempotency_key)
 WHERE actor_ref='operator' AND idempotency_key IS NOT NULL;
CREATE TRIGGER manager_tier_escalation_events_no_update BEFORE UPDATE ON manager_tier_escalation_events
 BEGIN SELECT RAISE(ABORT,'manager tier escalation events are immutable'); END;
CREATE TRIGGER manager_tier_escalation_events_no_delete BEFORE DELETE ON manager_tier_escalation_events
 BEGIN SELECT RAISE(ABORT,'manager tier escalation events are retained'); END;"
);

/// M2 catalog objects, for presence assertions and the fixture rewind.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 18] = [
    ("table", "manager_tier_messages"),
    ("index", "manager_tier_messages_operator_key"),
    ("index", "manager_tier_messages_by_target"),
    ("index", "manager_tier_messages_by_source"),
    ("index", "manager_tier_messages_by_state"),
    ("trigger", "manager_tier_messages_no_delete"),
    ("trigger", "manager_tier_messages_identity_immutable"),
    ("trigger", "manager_tier_messages_state_final"),
    ("table", "manager_tier_escalations"),
    ("index", "manager_tier_escalations_one_open"),
    ("index", "manager_tier_escalations_addressed"),
    ("trigger", "manager_tier_escalations_no_delete"),
    ("trigger", "manager_tier_escalations_identity_immutable"),
    ("trigger", "manager_tier_escalations_closed_final"),
    ("table", "manager_tier_escalation_events"),
    ("index", "manager_tier_escalation_events_operator_key"),
    ("trigger", "manager_tier_escalation_events_no_update"),
    ("trigger", "manager_tier_escalation_events_no_delete"),
];

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != TIER_ROUTING_SCHEMA_VERSION || prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "manager tier routing requires V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(CATALOG_MESSAGES)?;
    tx.execute_batch(CATALOG_ESCALATIONS)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: tier-routing-migration

/// Test fixture: undo M2 exactly (V155 to V154); the tables are new, so
/// dropping them (their indexes and triggers go with them) restores V154.
#[cfg(test)]
pub(crate) fn rewind_to_v154(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         DROP TABLE manager_tier_escalation_events;
         DROP TABLE manager_tier_escalations;
         DROP TABLE manager_tier_messages;
         PRAGMA user_version=154;
         COMMIT;",
    )
}

/// How a claimed tier message's continuation ended (#1266).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierMailSettlement {
    /// The continuation launched the recipient's provider turn with it.
    Delivered,
    /// The continuation failed before any provider could have seen it.
    Failed,
    /// The provider may or may not have seen it.
    Uncertain,
}

impl TierMailSettlement {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }
}

/// The reason recorded when a restart finds a claim unsettled.
pub const TIER_RESTART_REASON: &str =
    "daemon_restarted: the delivery was claimed but its outcome was never recorded";

/// Stale claims settled per batch of [`Store::settle_stale_claimed_tier_messages`].
const STALE_CLAIM_BATCH: i64 = 256;

/// Most failed or uncertain messages one seat's inbox lists (newest first).
pub const UNDELIVERED_TIER_MAIL_LIMIT: usize = 32;

/// A settle reason fits the column (1..=1024 bytes, cut on a char boundary).
fn settle_reason_text(reason: &str) -> String {
    let reason = reason.trim();
    let reason = if reason.is_empty() { "unknown" } else { reason };
    let mut end = reason.len().min(1024);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

/// The reason recorded when a claim outlives its continuation with no
/// recorded outcome (#1294).
pub const TIER_SETTLEMENT_LOST_REASON: &str =
    "settlement_lost: the delivery was claimed but no outcome was recorded in time";

/// One page of failed and uncertain tier mail, newest first
/// (`updated_at DESC, id DESC`). `sessions` keeps only rows whose sender or
/// recipient session is in the set, filtered in SQL before the limit, and
/// fills `role`. `after` is an exclusive `(updated_at, id)` cursor. Returns
/// the rows, each with its cursor, and whether more rows follow.
fn undelivered_page_on(
    conn: &Connection,
    sessions: Option<&[Uuid]>,
    after: Option<(&str, &str)>,
    limit: usize,
) -> Result<(Vec<(UndeliveredTierMailV1, String)>, bool)> {
    let sessions_json = sessions
        .map(|ids| serde_json::to_string(&ids.iter().map(Uuid::to_string).collect::<Vec<_>>()))
        .transpose()?;
    let mut statement = conn.prepare(
        "SELECT id,kind,source_ref,target_ref,source_session_id,target_session_id,project_id,body,state,settle_reason,updated_at
         FROM manager_tier_messages WHERE state IN ('failed','uncertain')
          AND (?1 IS NULL OR source_session_id IN (SELECT value FROM json_each(?1))
               OR target_session_id IN (SELECT value FROM json_each(?1)))
          AND (?2 IS NULL OR updated_at<?2 OR (updated_at=?2 AND id<?3))
         ORDER BY updated_at DESC,id DESC LIMIT ?4",
    )?;
    #[allow(clippy::type_complexity)]
    let raw: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
        String,
    )> = statement
        .query_map(
            params![
                sessions_json,
                after.map(|(at, _)| at),
                after.map(|(_, id)| id),
                i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1)
            ],
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
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    let more = raw.len() > limit;
    let rows = raw
        .into_iter()
        .take(limit)
        .map(
            |(
                id,
                kind,
                source_ref,
                target_ref,
                source,
                target,
                project,
                body,
                state,
                reason,
                at,
            )| {
                let source_session_id = source.as_deref().map(parse_uuid).transpose()?;
                let role = match sessions {
                    Some(ids) if source_session_id.is_some_and(|s| ids.contains(&s)) => "sender",
                    Some(_) => "recipient",
                    None => "",
                };
                let cursor = format!("{at}|{id}"); // sql-dynamic-ok: a paging cursor, not SQL
                Ok((
                    UndeliveredTierMailV1 {
                        message_id: parse_uuid(&id)?,
                        role: role.into(),
                        kind,
                        source_ref,
                        target_ref,
                        source_session_id,
                        target_session_id: target.as_deref().map(parse_uuid).transpose()?,
                        project_id: project.as_deref().map(parse_uuid).transpose()?,
                        body,
                        state,
                        settle_reason: reason.unwrap_or_default(),
                        settled_at: parse_time(&at)?,
                    },
                    cursor,
                ))
            },
        )
        .collect::<Result<Vec<_>>>()?;
    Ok((rows, more))
}

/// Parse an `undelivered_after` cursor (`<updated_at>|<id>`).
fn undelivered_cursor(cursor: &str) -> Result<(&str, &str)> {
    let (at, id) = cursor
        .split_once('|')
        .ok_or_else(|| refused(MANAGER_TIER_INVALID_REQUEST))?;
    let canonical = parse_uuid(id).is_ok_and(|parsed| parsed.to_string() == id);
    if !canonical || parse_time(at).is_err() {
        return Err(refused(MANAGER_TIER_INVALID_REQUEST));
    }
    Ok((at, id))
}

fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

fn stamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|_| DaemonError::Store("invalid manager tier identity".into()))
}

fn parse_time(text: &str) -> Result<chrono::DateTime<Utc>> {
    super::parse_timestamp(text)
        .map_err(|_| DaemonError::Store("invalid manager tier timestamp".into()))
}

fn digest(value: &serde_json::Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

/// A node as a stored reference, or the operator.
fn ref_text(node: Option<ManagerNodeRefV1>) -> String {
    node.map_or_else(|| OPERATOR_REF.to_string(), |node| node.as_ref_text())
}

/// One end of a tier message or hop: a node with its live seat, or the
/// operator (no seat).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TierEndpoint {
    pub node: Option<ManagerNodeRefV1>,
    pub session: Option<Uuid>,
    pub grant_version: Option<i64>,
    pub project: Option<Uuid>,
}

impl TierEndpoint {
    const OPERATOR: Self = Self {
        node: None,
        session: None,
        grant_version: None,
        project: None,
    };

    fn ref_text(&self) -> String {
        ref_text(self.node)
    }
}

/// An area node row (`project` from its root or its scope).
struct AreaRow {
    project: Uuid,
    parent: Option<Uuid>,
    seat_root: Uuid,
    grant_version: i64,
    live: bool,
}

fn area_row_on(conn: &Connection, node: Uuid) -> Result<Option<AreaRow>> {
    let row: Option<(Option<String>, Option<String>, String, i64, String, String)> = conn
        .query_row(
            "SELECT COALESCE(n.legacy_project_id,(SELECT s.project_id FROM manager_node_scopes s WHERE s.node_id=n.id LIMIT 1)),
                    n.parent_node_id,n.seat_root_session_id,n.grant_version,n.state,
                    COALESCE((SELECT g.state FROM manager_node_grants g WHERE g.node_id=n.id AND g.grant_version=n.grant_version),'')
             FROM manager_nodes n WHERE n.id=?1",
            [node.to_string()],
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
        )
        .optional()?;
    let Some((project, parent, seat, grant_version, state, grant_state)) = row else {
        return Ok(None);
    };
    let Some(project) = project else {
        return Ok(None);
    };
    Ok(Some(AreaRow {
        project: parse_uuid(&project)?,
        parent: parent.as_deref().map(parse_uuid).transpose()?,
        seat_root: parse_uuid(&seat)?,
        grant_version,
        live: state == "active" && grant_state == "granted",
    }))
}

/// One stored tier message, for the delivery fence.
struct MessageRow {
    kind: String,
    state: String,
    source_ref: String,
    target_ref: String,
    target_session: Option<Uuid>,
    source_grant_version: Option<i64>,
    target_grant_version: Option<i64>,
}

fn message_row_on(conn: &Connection, id: Uuid) -> Result<Option<MessageRow>> {
    type Raw = (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
    );
    let row: Option<Raw> = conn
        .query_row(
            "SELECT kind,state,source_ref,target_ref,target_session_id,source_grant_version,target_grant_version
             FROM manager_tier_messages WHERE id=?1",
            [id.to_string()],
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
    row.map(|(kind, state, source_ref, target_ref, target, sgv, tgv)| {
        Ok(MessageRow {
            kind,
            state,
            source_ref,
            target_ref,
            target_session: target.as_deref().map(parse_uuid).transpose()?,
            source_grant_version: sgv,
            target_grant_version: tgv,
        })
    })
    .transpose()
}

const HOP_SELECT: &str = "SELECT h.id,h.escalation_id,h.project_id,e.subject_id,e.reason,h.hop,h.source_ref,h.target_ref,
        h.target_session_id,h.target_grant_version,h.state,h.ruling,h.created_at,h.updated_at
 FROM manager_tier_escalations h JOIN manager_node_escalations e ON e.id=h.escalation_id";

type HopRaw = (
    String,
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    Option<String>,
    Option<i64>,
    String,
    Option<String>,
    String,
    String,
);

fn read_hop(row: &rusqlite::Row<'_>) -> rusqlite::Result<HopRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    ))
}

fn hop_from(raw: HopRaw) -> Result<ManagerTierEscalationHopV1> {
    let (
        id,
        escalation,
        project,
        subject,
        reason,
        hop,
        source_ref,
        target_ref,
        target_session,
        target_grant_version,
        state,
        ruling,
        created,
        updated,
    ) = raw;
    Ok(ManagerTierEscalationHopV1 {
        hop_id: parse_uuid(&id)?,
        escalation_id: parse_uuid(&escalation)?,
        project_id: parse_uuid(&project)?,
        subject_id: parse_uuid(&subject)?,
        reason,
        hop,
        source_ref,
        target_ref,
        target_session_id: target_session.as_deref().map(parse_uuid).transpose()?,
        target_grant_version,
        state,
        ruling,
        created_at: parse_time(&created)?,
        updated_at: parse_time(&updated)?,
    })
}

/// `clause` is always a static literal from this module.
fn hops_where(
    conn: &Connection,
    clause: &'static str,
    args: impl rusqlite::Params,
) -> Result<Vec<ManagerTierEscalationHopV1>> {
    let sql = format!("{HOP_SELECT} {clause}"); // sql-dynamic-ok: static clause
    let mut statement = conn.prepare(&sql)?;
    let rows = statement
        .query_map(args, read_hop)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(hop_from).collect()
}

/// The open hop of an in-project escalation, if it is held above the root.
pub(crate) fn open_hop_on(
    conn: &Connection,
    escalation: Uuid,
) -> Result<Option<ManagerTierEscalationHopV1>> {
    Ok(hops_where(
        conn,
        "WHERE h.escalation_id=?1 AND h.state='open'",
        [escalation.to_string()],
    )?
    .pop())
}

/// The newest hop of an in-project escalation (the `above_project` view).
pub(crate) fn latest_hop_on(
    conn: &Connection,
    escalation: Uuid,
) -> Result<Option<ManagerTierEscalationHopV1>> {
    Ok(hops_where(
        conn,
        "WHERE h.escalation_id=?1 ORDER BY h.hop DESC LIMIT 1",
        [escalation.to_string()],
    )?
    .pop())
}

fn hop_by_id_on(conn: &Connection, hop: Uuid) -> Result<Option<ManagerTierEscalationHopV1>> {
    Ok(hops_where(conn, "WHERE h.id=?1", [hop.to_string()])?.pop())
}

fn hop_event(
    conn: &Connection,
    hop: Uuid,
    action: &str,
    actor_ref: &str,
    actor_session: Option<Uuid>,
    ruling: Option<&str>,
    idempotency_key: Option<&str>,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO manager_tier_escalation_events(hop_id,seq,action,actor_ref,actor_session_id,ruling,idempotency_key,created_at)
         VALUES(?1,(SELECT COALESCE(MAX(seq),0)+1 FROM manager_tier_escalation_events WHERE hop_id=?1),?2,?3,?4,?5,?6,?7)",
        params![
            hop.to_string(),
            action,
            actor_ref,
            actor_session.map(|id| id.to_string()),
            ruling,
            idempotency_key,
            now
        ],
    )?;
    Ok(())
}

/// An area node was revoked: retire the open hops above the root of every
/// escalation it is the source of (#1238). Runs in the revocation
/// transaction.
pub(crate) fn retire_hops_from_source_on(
    conn: &Connection,
    source: Uuid,
    source_seat: Uuid,
    now: &str,
) -> Result<()> {
    let hops: Vec<String> = {
        let mut statement = conn.prepare(
            "SELECT h.id FROM manager_tier_escalations h JOIN manager_node_escalations e ON e.id=h.escalation_id
             WHERE e.source_node_id=?1 AND h.state='open'",
        )?;
        statement
            .query_map([source.to_string()], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let actor = format!("area:{source}"); // sql-dynamic-ok: a node reference, not SQL
    for hop in hops {
        let changed = conn.execute(
            "UPDATE manager_tier_escalations SET state='retired',updated_at=?2 WHERE id=?1 AND state='open'",
            params![hop, now],
        )?;
        if changed == 1 {
            hop_event(
                conn,
                parse_uuid(&hop)?,
                "retired",
                &actor,
                Some(source_seat),
                None,
                None,
                now,
            )?;
        }
    }
    Ok(())
}

/// One tier message to queue.
struct TierMessage<'a> {
    direction: &'static str,
    kind: &'static str,
    source: &'a TierEndpoint,
    target: &'a TierEndpoint,
    /// The sending session (`None` only for the operator).
    sender: Option<Uuid>,
    project: Option<Uuid>,
    body: &'a str,
    idempotency_key: &'a str,
    /// The text the recipient is resumed with.
    delivery: String,
    /// Mailbox-capped (agent mail); escalation and ruling wakes are not.
    capped: bool,
}

impl Store {
    // ----- routing primitive -------------------------------------------------

    /// The manager node whose seat `caller` holds: a portfolio seat (exact),
    /// a project's live PM, or an active area node's seat lineage tip.
    pub(crate) fn tier_caller_node(&self, caller: Uuid) -> Result<Option<ManagerNodeRefV1>> {
        if let Some(node) =
            portfolio_nodes::seat_grant_on(&self.conn, caller)?.and_then(|record| record.node_id)
        {
            return Ok(Some(ManagerNodeRefV1::Portfolio { node_id: node }));
        }
        let Some(project) = self.get_session(caller)?.and_then(|s| s.project_id) else {
            return Ok(None);
        };
        if self.global_live_manager(project)? == Some(caller) {
            return Ok(Some(ManagerNodeRefV1::Project {
                project_id: project,
            }));
        }
        let seats: Vec<(String, String)> = {
            let mut statement = self.conn.prepare(
                "SELECT n.id,n.seat_root_session_id FROM manager_nodes n
                   JOIN manager_node_scopes s ON s.node_id=n.id AND s.project_id=?1
                   JOIN manager_node_grants g ON g.node_id=n.id AND g.grant_version=n.grant_version AND g.state='granted'
                 WHERE n.state='active' AND n.parent_node_id IS NOT NULL ORDER BY n.id",
            )?;
            statement
                .query_map([project.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (node, seat) in seats {
            if self.manager_lineage_tip(parse_uuid(&seat)?)? == caller {
                return Ok(Some(ManagerNodeRefV1::Area {
                    node_id: parse_uuid(&node)?,
                }));
            }
        }
        Ok(None)
    }

    /// `parent_of(node)`: the next node up, or `None` for the operator.
    ///
    /// # Errors
    /// `manager_tier_target_unknown` when `node` does not exist.
    pub fn tier_parent_of(&self, node: ManagerNodeRefV1) -> Result<Option<ManagerNodeRefV1>> {
        match node {
            ManagerNodeRefV1::Area { node_id } => {
                let row = area_row_on(&self.conn, node_id)?
                    .ok_or_else(|| refused(MANAGER_TIER_TARGET_UNKNOWN))?;
                let Some(parent) = row.parent else {
                    // A project root is addressed as its project.
                    return self.tier_parent_of(ManagerNodeRefV1::Project {
                        project_id: row.project,
                    });
                };
                let parent_row = area_row_on(&self.conn, parent)?
                    .ok_or_else(|| refused(MANAGER_TIER_TARGET_UNKNOWN))?;
                Ok(Some(match parent_row.parent {
                    None => ManagerNodeRefV1::Project {
                        project_id: row.project,
                    },
                    Some(_) => ManagerNodeRefV1::Area { node_id: parent },
                }))
            }
            ManagerNodeRefV1::Project { project_id } => {
                Ok(portfolio_nodes::covering_node_on(&self.conn, project_id)?
                    .map(|node_id| ManagerNodeRefV1::Portfolio { node_id }))
            }
            ManagerNodeRefV1::Portfolio { node_id } => {
                let parent = portfolio_nodes::node_grant_on(&self.conn, node_id)?
                    .and_then(|record| record.parent_node_id);
                let Some(parent) = parent else {
                    return Ok(None);
                };
                let live = portfolio_nodes::node_row_on(&self.conn, parent)?
                    .is_some_and(|row| row.active)
                    && portfolio_nodes::node_grant_on(&self.conn, parent)?.is_some();
                Ok(live.then_some(ManagerNodeRefV1::Portfolio { node_id: parent }))
            }
        }
    }

    /// Every strict ancestor of `node`, nearest first (bounded).
    fn tier_ancestors(&self, node: ManagerNodeRefV1) -> Result<Vec<ManagerNodeRefV1>> {
        let mut chain = Vec::new();
        let mut cursor = node;
        for _ in 0..64 {
            match self.tier_parent_of(cursor)? {
                Some(parent) => {
                    if chain.contains(&parent) {
                        break;
                    }
                    chain.push(parent);
                    cursor = parent;
                }
                None => return Ok(chain),
            }
        }
        Ok(chain)
    }

    /// `node` with its live seat and grant version.
    ///
    /// # Errors
    /// `manager_tier_target_unknown`, `manager_tier_target_vacant`.
    pub(crate) fn tier_endpoint(&self, node: ManagerNodeRefV1) -> Result<TierEndpoint> {
        match node {
            ManagerNodeRefV1::Portfolio { node_id } => {
                if portfolio_nodes::node_row_on(&self.conn, node_id)?.is_none() {
                    return Err(refused(MANAGER_TIER_TARGET_UNKNOWN));
                }
                let record = portfolio_nodes::node_grant_on(&self.conn, node_id)?
                    .ok_or_else(|| refused(MANAGER_TIER_TARGET_VACANT))?;
                Ok(TierEndpoint {
                    node: Some(node),
                    session: Some(record.grant.seat_session_id),
                    grant_version: Some(record.grant.grant_version),
                    project: None,
                })
            }
            ManagerNodeRefV1::Project { project_id } => {
                if self.get_project(project_id)?.is_none() {
                    return Err(refused(MANAGER_TIER_TARGET_UNKNOWN));
                }
                let config = self
                    .get_harness_manager(project_id)?
                    .filter(|config| !config.is_revoked());
                let (Some(config), Some(session)) = (
                    config.as_ref(),
                    config.as_ref().and_then(|c| c.current_session_id),
                ) else {
                    return Err(refused(MANAGER_TIER_TARGET_VACANT));
                };
                Ok(TierEndpoint {
                    node: Some(node),
                    session: Some(session),
                    grant_version: Some(config.row_version),
                    project: Some(project_id),
                })
            }
            ManagerNodeRefV1::Area { node_id } => {
                let row = area_row_on(&self.conn, node_id)?
                    .filter(|row| row.parent.is_some())
                    .ok_or_else(|| refused(MANAGER_TIER_TARGET_UNKNOWN))?;
                if !row.live {
                    return Err(refused(MANAGER_TIER_TARGET_VACANT));
                }
                let tip = self.manager_lineage_tip(row.seat_root)?;
                if self.get_session(tip)?.is_none_or(|session| {
                    matches!(
                        session.status,
                        SessionStatus::Archived | SessionStatus::Deleted
                    )
                }) {
                    return Err(refused(MANAGER_TIER_TARGET_VACANT));
                }
                Ok(TierEndpoint {
                    node: Some(node),
                    session: Some(tip),
                    grant_version: Some(row.grant_version),
                    project: Some(row.project),
                })
            }
        }
    }

    /// The parent endpoint of `node` (the operator at the top).
    fn tier_parent_endpoint(&self, node: ManagerNodeRefV1) -> Result<TierEndpoint> {
        match self.tier_parent_of(node)? {
            Some(parent) => self.tier_endpoint(parent),
            None => Ok(TierEndpoint::OPERATOR),
        }
    }

    // ----- mail ----------------------------------------------------------------

    /// Queue one tier message inside the caller's IMMEDIATE transaction. A
    /// replay under the same `(sender, key)` returns the stored row; a changed
    /// request under that key is refused.
    fn queue_tier_message(&self, message: &TierMessage<'_>) -> Result<ManagerTierMessageReceiptV1> {
        let source_ref = message.source.ref_text();
        let target_ref = message.target.ref_text();
        let body_digest = digest(&serde_json::json!({
            "direction": message.direction,
            "kind": message.kind,
            "source_ref": source_ref,
            "target_ref": target_ref,
            "body": message.body,
        }))?;
        let existing: Option<(String, String, String, Option<String>)> = match message.sender {
            Some(sender) => self.conn.query_row(
                "SELECT id,body_digest,target_ref,target_session_id FROM manager_tier_messages
                 WHERE source_session_id=?1 AND idempotency_key=?2",
                params![sender.to_string(), message.idempotency_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ),
            None => self.conn.query_row(
                "SELECT id,body_digest,target_ref,target_session_id FROM manager_tier_messages
                 WHERE source_session_id IS NULL AND idempotency_key=?1",
                [message.idempotency_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ),
        }
        .optional()?;
        if let Some((id, stored, stored_target, target_session)) = existing {
            if stored != body_digest {
                return Err(refused(MANAGER_TIER_IDEMPOTENCY_CONFLICT));
            }
            return Ok(ManagerTierMessageReceiptV1 {
                message_id: parse_uuid(&id)?,
                source_ref,
                target_ref: stored_target,
                target_session_id: target_session.as_deref().map(parse_uuid).transpose()?,
                deduplicated: true,
            });
        }
        if message.capped {
            let (to_target, from_sender): (i64, i64) = self.conn.query_row(
                "SELECT (SELECT count(*) FROM manager_tier_messages WHERE target_ref=?1 AND state IN ('queued','claimed')),
                        (SELECT count(*) FROM manager_tier_messages WHERE source_session_id=?2 AND state IN ('queued','claimed'))",
                params![target_ref, message.sender.map(|id| id.to_string())],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if to_target >= MANAGER_TIER_MAX_PENDING || from_sender >= MANAGER_TIER_MAX_PENDING {
                return Err(refused(MANAGER_TIER_MAILBOX_FULL));
            }
        }
        let id = Uuid::new_v4();
        let now = Utc::now();
        let now_text = now.to_rfc3339_opts(SecondsFormat::Nanos, true);
        if let Some(target) = message.target.session {
            let job = ScheduledJob {
                id,
                name: match message.kind {
                    "report" => "Manager report".into(),
                    "escalation" => "Manager escalation".into(),
                    "ruling" => "Manager escalation ruling".into(),
                    _ => "Manager message".into(),
                },
                message: message.delivery.clone(),
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
                project_id: message.project,
                created_at: now,
                updated_at: now,
                wake_mode: WakeMode::Resume,
                wake_session_id: Some(target),
            };
            super::scheduled_jobs::insert_scheduled_job_conn(&self.conn, &job)?;
        }
        self.conn.execute(
            "INSERT INTO manager_tier_messages(id,direction,kind,source_ref,target_ref,project_id,source_session_id,target_session_id,source_grant_version,target_grant_version,body,body_digest,idempotency_key,state,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,'queued',?14,?14)",
            params![
                id.to_string(),
                message.direction,
                message.kind,
                source_ref,
                target_ref,
                message.project.map(|id| id.to_string()),
                message.sender.map(|id| id.to_string()),
                message.target.session.map(|id| id.to_string()),
                message.source.grant_version,
                message.target.grant_version,
                message.body,
                body_digest,
                message.idempotency_key,
                now_text,
            ],
        )?;
        Ok(ManagerTierMessageReceiptV1 {
            message_id: id,
            source_ref,
            target_ref,
            target_session_id: message.target.session,
            deduplicated: false,
        })
    }

    fn tier_label(&self, node: Option<ManagerNodeRefV1>) -> Result<String> {
        Ok(match node {
            None => "the operator".into(),
            Some(ManagerNodeRefV1::Portfolio { node_id }) => {
                let label = portfolio_nodes::node_row_on(&self.conn, node_id)?
                    .map_or_else(|| "portfolio".into(), |row| row.tier_label);
                format!("the {label} manager (node {node_id})")
            }
            Some(ManagerNodeRefV1::Project { project_id }) => {
                let name = self
                    .get_project(project_id)?
                    .map_or_else(|| project_id.to_string(), |project| project.name);
                format!("the project manager of {name} ({project_id})")
            }
            Some(ManagerNodeRefV1::Area { node_id }) => {
                format!("the area manager (node {node_id})")
            }
        })
    }

    /// `AgentReportUp`: durable mail from the caller's node to
    /// `parent_of(that node)`; at the top an operator notice row. Carries no
    /// authority.
    ///
    /// # Errors
    /// `manager_tier_not_node_seat`, `manager_tier_target_vacant`, an
    /// idempotency conflict, a full mailbox or a persistence error.
    pub fn tier_report_up(
        &self,
        caller: Uuid,
        request: &AgentReportUpRequestV1,
    ) -> Result<ManagerTierMessageReceiptV1> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let node = self
            .tier_caller_node(caller)?
            .ok_or_else(|| refused(MANAGER_TIER_NOT_NODE_SEAT))?;
        let mut source = self.tier_endpoint(node)?;
        source.session = Some(caller);
        let target = self.tier_parent_endpoint(node)?;
        let delivery = format!(
            "Report from {} (session {caller}) to you, {}:\n\n{}\n\nReports carry no authority; act on them within your own grant.",
            self.tier_label(Some(node))?,
            self.tier_label(target.node)?,
            request.message,
        );
        let receipt = self.queue_tier_message(&TierMessage {
            direction: "up",
            kind: "report",
            source: &source,
            target: &target,
            sender: Some(caller),
            project: source.project,
            body: &request.message,
            idempotency_key: &request.idempotency_key,
            delivery,
            capped: true,
        })?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Refuse `target` unless it is a strict descendant of `caller_node`:
    /// `manager_target_not_descendant` for the caller itself or an ancestor,
    /// `manager_project_not_in_scope` for anything outside its coverage.
    fn tier_require_descendant(
        &self,
        caller_node: ManagerNodeRefV1,
        target: ManagerNodeRefV1,
    ) -> Result<()> {
        if target == caller_node {
            return Err(refused(MANAGER_TARGET_NOT_DESCENDANT));
        }
        if self.tier_ancestors(target)?.contains(&caller_node) {
            return Ok(());
        }
        if self.tier_ancestors(caller_node)?.contains(&target) {
            return Err(refused(MANAGER_TARGET_NOT_DESCENDANT));
        }
        Err(refused(MANAGER_PROJECT_NOT_IN_SCOPE))
    }

    /// `AgentSendDown`: durable mail from the caller's node to a descendant
    /// node's live seat inside its coverage.
    ///
    /// # Errors
    /// `manager_tier_not_node_seat`, `manager_target_not_descendant`,
    /// `manager_project_not_in_scope`, `manager_tier_target_unknown`,
    /// `manager_tier_target_vacant`, an idempotency conflict, a full mailbox
    /// or a persistence error.
    pub fn tier_send_down(
        &self,
        caller: Uuid,
        request: &AgentSendDownRequestV1,
    ) -> Result<ManagerTierMessageReceiptV1> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let node = self
            .tier_caller_node(caller)?
            .ok_or_else(|| refused(MANAGER_TIER_NOT_NODE_SEAT))?;
        self.tier_require_descendant(node, request.target)?;
        let mut source = self.tier_endpoint(node)?;
        source.session = Some(caller);
        let target = self.tier_endpoint(request.target)?;
        let delivery = format!(
            "Message from {} (session {caller}) to you, {}:\n\n{}\n\nReport results, blockers and handoffs up with AgentReportUp.",
            self.tier_label(Some(node))?,
            self.tier_label(target.node)?,
            request.message,
        );
        let receipt = self.queue_tier_message(&TierMessage {
            direction: "down",
            kind: "message",
            source: &source,
            target: &target,
            sender: Some(caller),
            project: target.project,
            body: &request.message,
            idempotency_key: &request.idempotency_key,
            delivery,
            capped: true,
        })?;
        tx.commit()?;
        Ok(receipt)
    }

    /// The v0 receipt of a pre-#1238 message replayed under its key. The v0
    /// replay rule holds: the stored request digest (direction, project and
    /// request) must match, else `global_manager_idempotency_conflict`.
    fn legacy_global_receipt(
        &self,
        sender: Uuid,
        key: &str,
        direction: &str,
        request: &serde_json::Value,
    ) -> Result<Option<GlobalManagerMessageReceiptV1>> {
        let row: Option<(String, String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT id,project_id,target_session_id,direction,request_digest FROM global_manager_messages
                 WHERE sender_session_id=?1 AND idempotency_key=?2",
                params![sender.to_string(), key],
                |row| {
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
        let Some((id, project, target, stored_direction, stored_digest)) = row else {
            return Ok(None);
        };
        let project_id = parse_uuid(&project)?;
        let replayed = digest(&serde_json::json!({
            "direction": direction,
            "project_id": project_id,
            "request": request,
        }))?;
        if stored_direction != direction || stored_digest != replayed {
            return Err(refused(GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT));
        }
        Ok(Some(GlobalManagerMessageReceiptV1 {
            message_id: parse_uuid(&id)?,
            project_id,
            target_session_id: parse_uuid(&target)?,
            deduplicated: true,
        }))
    }

    /// The v0 refusal codes of the `AgentGlobalSend`/`AgentReportToGlobal`
    /// aliases.
    fn alias_code(error: DaemonError) -> DaemonError {
        match error {
            DaemonError::InvalidParam(code) if code == MANAGER_TIER_IDEMPOTENCY_CONFLICT => {
                refused(GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT)
            }
            DaemonError::InvalidParam(code) if code == MANAGER_TIER_MAILBOX_FULL => {
                refused(GLOBAL_MANAGER_MAILBOX_FULL)
            }
            other => other,
        }
    }

    /// `AgentReportToGlobal` (alias of `AgentReportUp` for one release): only
    /// a covered project's live PM, to the deepest node covering it, with the
    /// v0 receipt, delivery text and refusal codes.
    ///
    /// # Errors
    /// `global_report_not_authorized`, an idempotency conflict, a full
    /// mailbox or a persistence error.
    pub fn tier_report_to_global(
        &self,
        caller: Uuid,
        request: &AgentReportToGlobalRequestV1,
    ) -> Result<GlobalManagerMessageReceiptV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(receipt) = self.legacy_global_receipt(
            caller,
            &request.idempotency_key,
            "to_global",
            &serde_json::to_value(request)?,
        )? {
            tx.commit()?;
            return Ok(receipt);
        }
        let (grant, project_id) = self.global_report_grant(caller)?;
        let node = portfolio_nodes::covering_node_on(&self.conn, project_id)?
            .ok_or_else(|| refused(GLOBAL_REPORT_NOT_AUTHORIZED))?;
        let project_node = ManagerNodeRefV1::Project { project_id };
        let mut source = self.tier_endpoint(project_node)?;
        source.session = Some(caller);
        let target = TierEndpoint {
            node: Some(ManagerNodeRefV1::Portfolio { node_id: node }),
            session: Some(grant.seat_session_id),
            grant_version: Some(grant.grant_version),
            project: None,
        };
        let name = self
            .get_project(project_id)?
            .map_or_else(|| project_id.to_string(), |project| project.name);
        let delivery = format!(
            "Report from the project manager of {name} ({project_id}, session {caller}):\n\n{message}",
            message = request.message,
        );
        let receipt = self
            .queue_tier_message(&TierMessage {
                direction: "up",
                kind: "report",
                source: &source,
                target: &target,
                sender: Some(caller),
                project: Some(project_id),
                body: &request.message,
                idempotency_key: &request.idempotency_key,
                delivery,
                capped: true,
            })
            .map_err(Self::alias_code)?;
        tx.commit()?;
        Ok(GlobalManagerMessageReceiptV1 {
            message_id: receipt.message_id,
            project_id,
            target_session_id: receipt.target_session_id.unwrap_or(grant.seat_session_id),
            deduplicated: receipt.deduplicated,
        })
    }

    /// `AgentGlobalSend` (alias of `AgentSendDown` to a project for one
    /// release): a portfolio seat to a project in its grant, with the v0
    /// receipt, delivery text and refusal codes.
    ///
    /// # Errors
    /// `global_manager_not_seat`, `global_project_not_in_grant`,
    /// `global_project_has_no_manager`, an idempotency conflict, a full
    /// mailbox or a persistence error.
    pub fn tier_global_send(
        &self,
        caller: Uuid,
        request: &AgentGlobalSendRequestV1,
    ) -> Result<GlobalManagerMessageReceiptV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = portfolio_nodes::seat_grant_on(&self.conn, caller)?
            .filter(|record| record.node_id.is_some())
            .ok_or_else(|| refused(GLOBAL_MANAGER_NOT_SEAT))?;
        if let Some(receipt) = self.legacy_global_receipt(
            caller,
            &request.idempotency_key,
            "to_manager",
            &serde_json::to_value(request)?,
        )? {
            tx.commit()?;
            return Ok(receipt);
        }
        if !record.grant.project_ids.contains(&request.project_id) {
            return Err(refused(GLOBAL_PROJECT_NOT_IN_GRANT));
        }
        let pm = self
            .global_live_manager(request.project_id)?
            .ok_or_else(|| refused(GLOBAL_PROJECT_HAS_NO_MANAGER))?;
        let node = record.node_id.unwrap_or_default();
        let source = TierEndpoint {
            node: Some(ManagerNodeRefV1::Portfolio { node_id: node }),
            session: Some(caller),
            grant_version: Some(record.grant.grant_version),
            project: None,
        };
        let project_node = ManagerNodeRefV1::Project {
            project_id: request.project_id,
        };
        let target = self
            .tier_endpoint(project_node)
            .map_err(|_| refused(GLOBAL_PROJECT_HAS_NO_MANAGER))?;
        let name = self
            .get_project(request.project_id)?
            .map_or_else(|| request.project_id.to_string(), |project| project.name);
        let delivery = format!(
            "Message from the global manager (session {caller}) to you, the project manager of {name} ({project}):\n\n{message}\n\nReport results, blockers and handoffs up with AgentReportToGlobal.",
            project = request.project_id,
            message = request.message,
        );
        let receipt = self
            .queue_tier_message(&TierMessage {
                direction: "down",
                kind: "message",
                source: &source,
                target: &target,
                sender: Some(caller),
                project: Some(request.project_id),
                body: &request.message,
                idempotency_key: &request.idempotency_key,
                delivery,
                capped: true,
            })
            .map_err(Self::alias_code)?;
        tx.commit()?;
        Ok(GlobalManagerMessageReceiptV1 {
            message_id: receipt.message_id,
            project_id: request.project_id,
            target_session_id: receipt.target_session_id.unwrap_or(pm),
            deduplicated: receipt.deduplicated,
        })
    }

    /// Lead -> manager mail in a project with no live PM: the current lead of
    /// a live Epic reaches the deepest portfolio node covering its project.
    /// `Ok(None)` when that does not apply (the caller keeps the PM path's
    /// refusal).
    ///
    /// # Errors
    /// An idempotency conflict, a full mailbox or a persistence error.
    pub fn tier_lead_notice(
        &self,
        caller: Uuid,
        message: &str,
        idempotency_key: &str,
    ) -> Result<Option<ManagerTierMessageReceiptV1>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(lead) = self.get_session(caller)? else {
            return Ok(None);
        };
        let (Some(project), Some(epic)) = (lead.project_id, lead.parent_id) else {
            return Ok(None);
        };
        if matches!(
            lead.status,
            SessionStatus::Archived | SessionStatus::Deleted
        ) || self.global_live_manager(project)?.is_some()
        {
            return Ok(None);
        }
        let leads_epic = self.get_session(epic)?.is_some_and(|epic| {
            epic.session_kind == SessionKind::Epic
                && epic.project_id == Some(project)
                && epic.lead_session_id == Some(caller)
                && !matches!(
                    epic.status,
                    SessionStatus::Archived | SessionStatus::Deleted
                )
        });
        if !leads_epic {
            return Ok(None);
        }
        let Some(node) = portfolio_nodes::covering_node_on(&self.conn, project)? else {
            return Ok(None);
        };
        let target = self.tier_endpoint(ManagerNodeRefV1::Portfolio { node_id: node })?;
        let source = TierEndpoint {
            node: Some(ManagerNodeRefV1::Project {
                project_id: project,
            }),
            session: Some(caller),
            grant_version: None,
            project: Some(project),
        };
        let name = self
            .get_project(project)?
            .map_or_else(|| project.to_string(), |project| project.name);
        let delivery = format!(
            "Message from the lead of Epic {epic} (session {caller}) in {name} ({project}), which has no project manager, to you, {}:\n\n{message}\n\nReply to the lead with AgentSendMessage.",
            self.tier_label(target.node)?,
        );
        let receipt = self.queue_tier_message(&TierMessage {
            direction: "up",
            kind: "message",
            source: &source,
            target: &target,
            sender: Some(caller),
            project: Some(project),
            body: message,
            idempotency_key,
            delivery,
            capped: true,
        })?;
        tx.commit()?;
        Ok(Some(receipt))
    }

    // ----- delivery fence and retirement --------------------------------------

    /// Whether a source or target end still holds the grant the message was
    /// queued under (`tip`: the session the delivery resumes, for a target).
    fn tier_end_current(
        &self,
        reference: &str,
        grant_version: Option<i64>,
        session: Option<Uuid>,
        tip: Option<Uuid>,
    ) -> Result<bool> {
        let Some(node) = ManagerNodeRefV1::parse_ref_text(reference) else {
            return Ok(false);
        };
        match node {
            ManagerNodeRefV1::Portfolio { node_id } => {
                let Some(record) = portfolio_nodes::node_grant_on(&self.conn, node_id)? else {
                    return Ok(false);
                };
                Ok(Some(record.grant.grant_version) == grant_version
                    && session.is_none_or(|seat| seat == record.grant.seat_session_id)
                    && tip.is_none_or(|tip| Some(tip) == session))
            }
            ManagerNodeRefV1::Project { project_id } => match tip {
                // Down mail follows the PM's rotation to its live seat.
                Some(tip) => Ok(self.global_live_manager(project_id)? == Some(tip)),
                None => Ok(true),
            },
            ManagerNodeRefV1::Area { node_id } => {
                let Some(row) = area_row_on(&self.conn, node_id)? else {
                    return Ok(false);
                };
                if !row.live || Some(row.grant_version) != grant_version {
                    return Ok(false);
                }
                match tip {
                    Some(tip) => Ok(self.manager_lineage_tip(row.seat_root)? == tip),
                    None => Ok(true),
                }
            }
        }
    }

    /// The tier half of the scheduler's delivery fence: `None` when `job_id`
    /// is not a tier message. A queued message is deliverable only while its
    /// target seat (and, for agent mail, its source grant) is current.
    pub(crate) fn tier_message_deliverable(
        &self,
        job_id: Uuid,
        delivery_tip: Option<Uuid>,
    ) -> Result<Option<bool>> {
        let Some(row) = message_row_on(&self.conn, job_id)? else {
            return Ok(None);
        };
        if row.state != "queued" {
            return Ok(Some(false));
        }
        let Some(target) = row.target_session else {
            return Ok(Some(false));
        };
        let tip = match delivery_tip {
            Some(tip) => tip,
            None => self.session_lineage_tip(target)?,
        };
        let agent_mail = matches!(row.kind.as_str(), "message" | "report");
        if agent_mail
            && row.source_ref != OPERATOR_REF
            && !self.tier_end_current(&row.source_ref, row.source_grant_version, None, None)?
        {
            return Ok(Some(false));
        }
        Ok(Some(self.tier_end_current(
            &row.target_ref,
            row.target_grant_version,
            Some(target),
            Some(tip),
        )?))
    }

    /// Whether `job_id` is a tier message.
    pub(crate) fn is_tier_message(&self, job_id: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_tier_messages WHERE id=?1)",
            [job_id.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Claim a queued tier message for the continuation that delivers it
    /// (no-op for other jobs). The row stays `claimed` until
    /// [`Store::settle_tier_messages`] records the outcome; a restart in
    /// between settles it `uncertain` ([`Store::reconcile_tier_messages_at_startup`]).
    pub(crate) fn claim_tier_message(&self, job_id: Uuid) -> Result<()> {
        self.conn.execute(
            "UPDATE manager_tier_messages SET state='claimed',updated_at=?2 WHERE id=?1 AND state='queued'",
            params![job_id.to_string(), stamp()],
        )?;
        Ok(())
    }

    /// #1266: record the outcome of the continuation that claimed `job_ids`
    /// and disable their wakes. Only `claimed` tier rows change; other jobs
    /// and rows in any other state are untouched, so nothing reopens and
    /// nothing is replayed. Returns the number of rows settled.
    ///
    /// # Errors
    /// A persistence error.
    pub fn settle_tier_messages(
        &self,
        job_ids: &[Uuid],
        outcome: TierMailSettlement,
        reason: Option<&str>,
    ) -> Result<usize> {
        let reason = match outcome {
            TierMailSettlement::Delivered => None,
            TierMailSettlement::Failed | TierMailSettlement::Uncertain => {
                Some(settle_reason_text(reason.unwrap_or(outcome.as_str())))
            }
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = stamp();
        let mut settled = 0;
        for job_id in job_ids {
            let changed = tx.execute(
                "UPDATE manager_tier_messages SET state=?2,settle_reason=?3,updated_at=?4 WHERE id=?1 AND state='claimed'",
                params![job_id.to_string(), outcome.as_str(), reason, now],
            )?;
            if changed > 0 {
                tx.execute(
                    "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1 AND enabled=1",
                    params![job_id.to_string(), now],
                )?;
                settled += changed;
            }
        }
        tx.commit()?;
        Ok(settled)
    }

    /// Call once at daemon startup, before the scheduler can fire: a tier
    /// message still `claimed` belonged to a continuation the restart cut
    /// short, and the provider may already have seen it, so it settles
    /// `uncertain` (shown, never replayed). Returns the number settled.
    ///
    /// # Errors
    /// A persistence error.
    pub fn reconcile_tier_messages_at_startup(&self) -> Result<usize> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = stamp();
        tx.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?1 WHERE enabled=1
             AND id IN (SELECT id FROM manager_tier_messages WHERE state='claimed')",
            [&now],
        )?;
        let settled = tx.execute(
            "UPDATE manager_tier_messages SET state='uncertain',settle_reason=?1,updated_at=?2 WHERE state='claimed'",
            params![TIER_RESTART_REASON, now],
        )?;
        tx.commit()?;
        Ok(settled)
    }

    /// Failed and uncertain tier mail `session` sent or was sent (its own
    /// rows and its rotation ancestors'), newest first, at most
    /// [`UNDELIVERED_TIER_MAIL_LIMIT`]. The seat filter runs in SQL before
    /// the limit (#1295), so other seats' failures never hide this seat's.
    ///
    /// # Errors
    /// A persistence error.
    pub fn undelivered_tier_mail_for_session(
        &self,
        session: Uuid,
    ) -> Result<Vec<UndeliveredTierMailV1>> {
        Ok(self.undelivered_tier_mail_page_for_session(session)?.0)
    }

    /// [`Store::undelivered_tier_mail_for_session`] plus whether older rows
    /// exist beyond the page.
    ///
    /// # Errors
    /// A persistence error.
    pub fn undelivered_tier_mail_page_for_session(
        &self,
        session: Uuid,
    ) -> Result<(Vec<UndeliveredTierMailV1>, bool)> {
        let lineage = self.tier_lineage_with_ancestors(session)?;
        let (rows, more) = undelivered_page_on(
            &self.conn,
            Some(&lineage),
            None,
            UNDELIVERED_TIER_MAIL_LIMIT,
        )?;
        Ok((rows.into_iter().map(|(row, _)| row).collect(), more))
    }

    /// `session` and every incarnation it continues (bounded walk of
    /// `continued_from`).
    fn tier_lineage_with_ancestors(&self, session: Uuid) -> Result<Vec<Uuid>> {
        let mut lineage = vec![session];
        let mut current = session;
        while lineage.len() < 1024 {
            let previous: Option<String> = self
                .conn
                .query_row(
                    "SELECT continued_from FROM sessions WHERE id=?1",
                    [current.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let Some(previous) = previous.as_deref().map(parse_uuid).transpose()? else {
                break;
            };
            if lineage.contains(&previous) {
                break;
            }
            lineage.push(previous);
            current = previous;
        }
        Ok(lineage)
    }

    /// #1294: settle `uncertain` every tier message still `claimed` since
    /// before `claimed_before` whose continuation is neither in flight nor
    /// holding a recorded outcome (`exclude`), and disable its wake. Nothing
    /// is replayed. Returns the number settled.
    ///
    /// # Errors
    /// A persistence error.
    pub fn settle_stale_claimed_tier_messages(
        &self,
        exclude: &[Uuid],
        claimed_before: chrono::DateTime<Utc>,
    ) -> Result<usize> {
        let cutoff = claimed_before.to_rfc3339_opts(SecondsFormat::Nanos, true);
        // #1308: the exclusions apply in SQL before the batch limit, so
        // excluded claims can never starve an orphan; batches repeat until
        // every stale orphan is settled (each batch settles all it selects).
        let exclude =
            serde_json::to_string(&exclude.iter().map(Uuid::to_string).collect::<Vec<_>>())?;
        let mut settled = 0;
        loop {
            let ids: Vec<String> = {
                let mut statement = self.conn.prepare(
                    "SELECT id FROM manager_tier_messages WHERE state='claimed' AND updated_at<?1
                     AND id NOT IN (SELECT value FROM json_each(?2)) ORDER BY updated_at,id LIMIT ?3",
                )?;
                statement
                    .query_map(params![cutoff, exclude, STALE_CLAIM_BATCH], |row| {
                        row.get(0)
                    })?
                    .collect::<rusqlite::Result<_>>()?
            };
            let stale = ids
                .iter()
                .map(|id| parse_uuid(id))
                .collect::<Result<Vec<_>>>()?;
            let count = self.settle_tier_messages(
                &stale,
                TierMailSettlement::Uncertain,
                Some(TIER_SETTLEMENT_LOST_REASON),
            )?;
            settled += count;
            if i64::try_from(stale.len()).unwrap_or(i64::MAX) < STALE_CLAIM_BATCH || count == 0 {
                return Ok(settled);
            }
        }
    }

    /// The state of tier message `id`, if it is one (diagnostics and tests).
    ///
    /// # Errors
    /// A persistence error.
    pub fn tier_message_state(&self, id: Uuid) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state FROM manager_tier_messages WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Retire one undeliverable tier message and disable its wake (no-op for
    /// other jobs). A settled row keeps its state but its wake is still
    /// disabled; a `claimed` row belongs to an in-flight continuation, which
    /// settles it.
    pub(crate) fn retire_tier_message(&self, job_id: Uuid) -> Result<()> {
        let now = stamp();
        self.conn.execute(
            "UPDATE manager_tier_messages SET state='retired',updated_at=?2 WHERE id=?1 AND state='queued'",
            params![job_id.to_string(), now],
        )?;
        self.conn.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1 AND enabled=1
             AND id IN (SELECT id FROM manager_tier_messages WHERE state<>'claimed')",
            params![job_id.to_string(), now],
        )?;
        Ok(())
    }

    /// Retire queued tier mail matching `clause` (a static literal) and
    /// disable its wakes. Operator notices are never retired here: they are
    /// the operator's record.
    fn retire_tier_messages_where(
        &self,
        clause: &'static str,
        args: &[&dyn rusqlite::ToSql],
        now: &str,
    ) -> Result<()> {
        let select = format!(
            // sql-dynamic-ok: static clause
            "SELECT id FROM manager_tier_messages WHERE state='queued' AND target_ref<>'operator' AND ({clause})"
        );
        let ids: Vec<String> = {
            let mut statement = self.conn.prepare(&select)?;
            statement
                .query_map(args, |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            self.conn.execute(
                "UPDATE manager_tier_messages SET state='retired',updated_at=?2 WHERE id=?1 AND state='queued'",
                params![id, now],
            )?;
            self.conn.execute(
                "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1 AND enabled=1",
                params![id, now],
            )?;
        }
        Ok(())
    }

    /// A portfolio grant was replaced, revoked or moved to a successor seat:
    /// retire the queued mail at either end under that grant and the open
    /// escalation hops addressed to it (the escalation returns to the project
    /// root, which may forward it again).
    pub(crate) fn retire_tier_grant(&self, grant_id: Uuid, now: &str) -> Result<()> {
        let grant: Option<(i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT grant_version,node_id FROM global_manager_grants WHERE id=?1",
                [grant_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((version, Some(node))) = grant else {
            return Ok(());
        };
        let reference = format!("portfolio:{node}");
        self.retire_tier_messages_where(
            "(source_ref=?1 AND source_grant_version=?2) OR (target_ref=?1 AND target_grant_version=?2)",
            &[&reference, &version],
            now,
        )?;
        let hops: Vec<(String, Option<String>)> = {
            let mut statement = self.conn.prepare(
                "SELECT id,target_session_id FROM manager_tier_escalations
                 WHERE target_ref=?1 AND target_grant_version=?2 AND state='open'",
            )?;
            statement
                .query_map(params![reference, version], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?
        };
        for (hop, seat) in hops {
            self.conn.execute(
                "UPDATE manager_tier_escalations SET state='retired',updated_at=?2 WHERE id=?1 AND state='open'",
                params![hop, now],
            )?;
            hop_event(
                &self.conn,
                parse_uuid(&hop)?,
                "retired",
                &reference,
                seat.as_deref().map(parse_uuid).transpose()?,
                None,
                None,
                now,
            )?;
        }
        Ok(())
    }

    /// A project's PM seat was displaced or cleared: retire the queued down
    /// mail to it and its own queued reports.
    pub(crate) fn retire_tier_project_seat(&self, project: Uuid, now: &str) -> Result<()> {
        let reference = format!("project:{project}");
        self.retire_tier_messages_where(
            "target_ref=?1 OR (source_ref=?1 AND kind='report')",
            &[&reference],
            now,
        )
    }

    // ----- escalations above the project root ---------------------------------

    /// Queue the wake that tells a portfolio seat an escalation hop is
    /// addressed to it (not capped: the hop row is the authority record).
    fn tier_escalation_wake(
        &self,
        hop: Uuid,
        escalation: &ManagerNodeEscalationV1,
        source: &TierEndpoint,
        actor: Uuid,
        target: &TierEndpoint,
    ) -> Result<()> {
        if target.session.is_none() {
            return Ok(());
        }
        let key = format!("escalation-hop:{hop}");
        let delivery = format!(
            "Escalation {id} from project {project} (subject {subject}) is now addressed to you, {label}:\n\n{reason}\n\nRule or forward it with AgentManagerResolveEscalation (escalation_id {id}; read the expected versions with AgentManagerListEscalations). A ruling is a manager decision and never answers a human approval.",
            id = escalation.id,
            project = escalation.project_id,
            subject = escalation.subject_id,
            label = self.tier_label(target.node)?,
            reason = escalation.reason,
        );
        self.queue_tier_message(&TierMessage {
            direction: "up",
            kind: "escalation",
            source,
            target,
            sender: Some(actor),
            project: Some(escalation.project_id),
            body: &escalation.reason,
            idempotency_key: &key,
            delivery,
            capped: false,
        })?;
        Ok(())
    }

    /// Open the next hop of `escalation` from `source` to `target`.
    fn open_tier_hop(
        &self,
        escalation: &ManagerNodeEscalationV1,
        source: &TierEndpoint,
        actor: Uuid,
        target: &TierEndpoint,
        now: &str,
    ) -> Result<Uuid> {
        let hop_number: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(hop),0)+1 FROM manager_tier_escalations WHERE escalation_id=?1",
            [escalation.id.to_string()],
            |row| row.get(0),
        )?;
        let id = Uuid::new_v4();
        self.conn.execute(
            "INSERT INTO manager_tier_escalations(id,escalation_id,project_id,hop,source_ref,target_ref,actor_session_id,target_session_id,target_grant_version,state,ruling,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'open',NULL,?10,?10)",
            params![
                id.to_string(),
                escalation.id.to_string(),
                escalation.project_id.to_string(),
                hop_number,
                source.ref_text(),
                target.ref_text(),
                actor.to_string(),
                target.session.map(|id| id.to_string()),
                target.grant_version,
                now,
            ],
        )?;
        hop_event(
            &self.conn,
            id,
            "opened",
            &source.ref_text(),
            Some(actor),
            None,
            None,
            now,
        )?;
        self.tier_escalation_wake(id, escalation, source, actor, target)?;
        Ok(id)
    }

    /// The project root forwards an escalation above its project: the first
    /// hop goes to `parent_of(Project p)`. Runs inside the resolve
    /// transaction after its in-project fences.
    pub(crate) fn tier_cross_project_root(
        &self,
        escalation: &ManagerNodeEscalationV1,
        caller: Uuid,
        now: &str,
    ) -> Result<()> {
        let project = ManagerNodeRefV1::Project {
            project_id: escalation.project_id,
        };
        let source = TierEndpoint {
            node: Some(project),
            session: Some(caller),
            grant_version: None,
            project: Some(escalation.project_id),
        };
        let target = self.tier_parent_endpoint(project)?;
        self.open_tier_hop(escalation, &source, caller, &target, now)?;
        Ok(())
    }

    /// Rule `hop` and return the ruling down the recorded chain: earlier hops
    /// get a `returned` event, the in-project escalation is ruled at its root
    /// and the source seat is woken with the ruling.
    fn rule_tier_hop(
        &self,
        escalation: &ManagerNodeEscalationV1,
        hop: &ManagerTierEscalationHopV1,
        actor: &TierEndpoint,
        ruling: &str,
        idempotency_key: Option<&str>,
        now: &str,
    ) -> Result<()> {
        let actor_ref = actor.ref_text();
        let changed = self.conn.execute(
            "UPDATE manager_tier_escalations SET state='ruled',ruling=?2,updated_at=?3 WHERE id=?1 AND state='open'",
            params![hop.hop_id.to_string(), ruling, now],
        )?;
        if changed != 1 {
            return Err(refused(OPERATOR_ESCALATION_NOT_OPEN));
        }
        hop_event(
            &self.conn,
            hop.hop_id,
            "ruled",
            &actor_ref,
            actor.session,
            Some(ruling),
            idempotency_key,
            now,
        )?;
        let earlier: Vec<String> = {
            let mut statement = self.conn.prepare(
                "SELECT id FROM manager_tier_escalations WHERE escalation_id=?1 AND hop<?2 AND state='forwarded'
                 ORDER BY hop DESC",
            )?;
            statement
                .query_map(params![escalation.id.to_string(), hop.hop], |row| {
                    row.get(0)
                })?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in earlier {
            hop_event(
                &self.conn,
                parse_uuid(&id)?,
                "returned",
                &actor_ref,
                actor.session,
                Some(ruling),
                None,
                now,
            )?;
        }
        let next = escalation
            .version
            .checked_add(1)
            .ok_or_else(|| refused("manager_node_version_exhausted"))?;
        let ruled = self.conn.execute(
            "UPDATE manager_node_escalations SET version=?2,state='ruled',ruling=?3,updated_at=?4
             WHERE id=?1 AND version=?5 AND state='open'",
            params![
                escalation.id.to_string(),
                next,
                ruling,
                now,
                escalation.version
            ],
        )?;
        if ruled != 1 {
            return Err(refused("manager_node_escalation_stale_target"));
        }
        self.conn.execute(
            "INSERT INTO manager_node_escalation_events(escalation_id,version,actor_node_id,target_node_id,action,ruling,created_at)
             VALUES(?1,?2,?3,?3,'ruled',?4,?5)",
            params![
                escalation.id.to_string(),
                next,
                escalation.target_node_id.to_string(),
                ruling,
                now
            ],
        )?;
        // Wake the source seat with the ruling, when it still has a seat.
        let source_node = match area_row_on(&self.conn, escalation.source_node_id)? {
            Some(row) if row.parent.is_some() => ManagerNodeRefV1::Area {
                node_id: escalation.source_node_id,
            },
            _ => ManagerNodeRefV1::Project {
                project_id: escalation.project_id,
            },
        };
        let Ok(target) = self.tier_endpoint(source_node) else {
            return Ok(());
        };
        let key = format!("ruling:{}", hop.hop_id);
        let delivery = format!(
            "Ruling on your escalation {id} (subject {subject}) from {from}:\n\n{ruling}\n\nA ruling is a manager decision; it never answers a human approval.",
            id = escalation.id,
            subject = escalation.subject_id,
            from = self.tier_label(actor.node)?,
        );
        self.queue_tier_message(&TierMessage {
            direction: "down",
            kind: "ruling",
            source: actor,
            target: &target,
            sender: actor.session,
            project: Some(escalation.project_id),
            body: ruling,
            idempotency_key: &key,
            delivery,
            capped: false,
        })?;
        Ok(())
    }

    /// `AgentManagerResolveEscalation` on an escalation held above its project
    /// root: only the addressed portfolio seat rules or forwards it. Runs in
    /// the resolve transaction; replays return the stored result.
    pub(crate) fn tier_resolve_hop(
        &self,
        tx: &Transaction<'_>,
        caller: Uuid,
        escalation: &ManagerNodeEscalationV1,
        hop: &ManagerTierEscalationHopV1,
        request: &AgentManagerResolveEscalationRequestV1,
    ) -> Result<ManagerNodeEscalationV1> {
        let addressed = ManagerNodeRefV1::parse_ref_text(&hop.target_ref);
        let Some(ManagerNodeRefV1::Portfolio { node_id }) = addressed else {
            return Err(refused("manager_node_escalation_not_addressed"));
        };
        let own = portfolio_nodes::seat_grant_on(&self.conn, caller)?;
        if own.as_ref().and_then(|record| record.node_id) != Some(node_id) {
            // A seat inside the escalation's project sees why it cannot act.
            let in_project = match self.tier_caller_node(caller)? {
                Some(ManagerNodeRefV1::Project { project_id }) => {
                    project_id == escalation.project_id
                }
                Some(ManagerNodeRefV1::Area { node_id }) => area_row_on(&self.conn, node_id)?
                    .is_some_and(|row| row.project == escalation.project_id),
                _ => false,
            };
            return Err(refused(if in_project {
                MANAGER_ESCALATION_FORWARDED_ABOVE
            } else {
                "manager_node_escalation_not_addressed"
            }));
        }
        // Replays were answered by `replay_resolve_any_identity` before any
        // fence; the key is the caller's own node, which is the addressee.
        let replay_key = (&hop.target_ref, request);
        let record = own.ok_or_else(|| refused("manager_node_escalation_not_addressed"))?;
        let epoch =
            portfolio_nodes::node_row_on(&self.conn, node_id)?.map_or(0, |row| row.authority_epoch);
        let shown_version = escalation.version + hop.hop;
        if Some(record.grant.grant_version) != hop.target_grant_version
            || request.expected_version != shown_version
            || request.expected_target_grant_version != record.grant.grant_version
            || request.expected_target_authority_epoch != epoch
            || request.expected_target_session_id != caller
        {
            return Err(refused("manager_node_escalation_stale_target"));
        }
        let now = stamp();
        let actor = TierEndpoint {
            node: addressed,
            session: Some(caller),
            grant_version: Some(record.grant.grant_version),
            project: None,
        };
        match &request.ruling {
            Some(ruling) => self.rule_tier_hop(escalation, hop, &actor, ruling, None, &now)?,
            None => {
                self.conn.execute(
                    "UPDATE manager_tier_escalations SET state='forwarded',updated_at=?2 WHERE id=?1 AND state='open'",
                    params![hop.hop_id.to_string(), now],
                )?;
                hop_event(
                    &self.conn,
                    hop.hop_id,
                    "forwarded",
                    &hop.target_ref,
                    Some(caller),
                    None,
                    None,
                    &now,
                )?;
                let target = self.tier_parent_endpoint(ManagerNodeRefV1::Portfolio { node_id })?;
                self.open_tier_hop(escalation, &actor, caller, &target, &now)?;
            }
        }
        let result = super::harness_manager::escalation_on(tx, escalation.id)?
            .ok_or_else(|| DaemonError::Store("escalation update missing".into()))?;
        super::manager_nodes::record_operation(
            tx,
            escalation.project_id,
            Some(&request.idempotency_key),
            &replay_key,
            &result,
            &now,
        )?;
        Ok(result)
    }

    /// The stored-reference identity of the portfolio node `caller` seats,
    /// the key its resolves are recorded under (#1268).
    /// The `portfolio:<id>` reference of the node `caller` seats, if any.
    ///
    /// # Errors
    /// A persistence error.
    pub fn tier_portfolio_ref(&self, caller: Uuid) -> Result<Option<String>> {
        Ok(portfolio_nodes::seat_grant_on(&self.conn, caller)?
            .and_then(|record| record.node_id)
            .map(|node| format!("portfolio:{node}"))) // sql-dynamic-ok: a node reference, not SQL
    }

    /// Retire one open hop and record who retired it.
    fn retire_open_hop(
        &self,
        hop: Uuid,
        actor_ref: &str,
        actor_session: Option<Uuid>,
        now: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE manager_tier_escalations SET state='retired',updated_at=?2 WHERE id=?1 AND state='open'",
            params![hop.to_string(), now],
        )?;
        if changed == 1 {
            hop_event(
                &self.conn,
                hop,
                "retired",
                actor_ref,
                actor_session,
                None,
                None,
                now,
            )?;
        }
        Ok(())
    }

    /// The escalation's source lost its grant while a hop above the root was
    /// open: the addressed tier retires the hop.
    pub(crate) fn tier_retire_hop_for_stale_source(
        &self,
        hop: &ManagerTierEscalationHopV1,
    ) -> Result<()> {
        let session = (hop.target_ref != OPERATOR_REF)
            .then_some(hop.target_session_id)
            .flatten();
        let actor = if session.is_some() {
            hop.target_ref.as_str()
        } else {
            OPERATOR_REF
        };
        self.retire_open_hop(hop.hop_id, actor, session, &stamp())
    }

    /// The escalations addressed to a portfolio seat, as the in-project view
    /// with the hop's target fields (`version` is the escalation's version
    /// plus the hop number, the value `AgentManagerResolveEscalation`
    /// expects). `None` when `caller` holds no portfolio seat.
    pub fn tier_escalations_for_seat(
        &self,
        caller: Uuid,
    ) -> Result<Option<Vec<ManagerNodeEscalationV1>>> {
        let Some(record) = portfolio_nodes::seat_grant_on(&self.conn, caller)? else {
            return Ok(None);
        };
        let Some(node) = record.node_id else {
            return Ok(None);
        };
        let epoch =
            portfolio_nodes::node_row_on(&self.conn, node)?.map_or(0, |row| row.authority_epoch);
        let reference = format!("portfolio:{node}");
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        // `escalation_on` reads through the transaction handle.
        let hops = hops_where(
            &self.conn,
            "WHERE h.target_ref=?1 AND h.state='open' ORDER BY h.created_at,h.id LIMIT 256",
            [reference],
        )?;
        let mut views = Vec::with_capacity(hops.len());
        for hop in hops {
            let Some(mut view) = super::harness_manager::escalation_on(&tx, hop.escalation_id)?
            else {
                continue;
            };
            view.target_node_id = node;
            view.target_authority_epoch = epoch;
            view.target_grant_version = hop.target_grant_version.unwrap_or_default();
            view.target_session_id = hop.target_session_id.unwrap_or(caller);
            view.version += hop.hop;
            views.push(view);
        }
        tx.commit()?;
        Ok(Some(views))
    }

    /// Open escalation hops addressed to a portfolio node (tree counts).
    pub(crate) fn tier_open_hops_for_node(&self, node: Uuid) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT count(*) FROM manager_tier_escalations WHERE target_ref=?1 AND state='open'",
                [format!("portfolio:{node}")],
                |row| row.get(0),
            )
            .ok()
    }

    // ----- operator queue (operator-only RPCs) ---------------------------------

    /// Operator-only `ListOperatorEscalations`: escalations at the top of
    /// their chain and top-of-chain reports, newest first.
    pub fn list_operator_escalations(
        &self,
        include_closed: bool,
    ) -> Result<ListOperatorEscalationsResultV1> {
        self.list_operator_escalations_page(include_closed, None)
    }

    /// `ListOperatorEscalations` with its failed and uncertain tier mail
    /// paged (#1295): `undelivered_after` is the previous page's
    /// `next_undelivered_after`; every row is reachable.
    ///
    /// # Errors
    /// `manager_tier_invalid_request` for a malformed cursor, or a
    /// persistence error.
    pub fn list_operator_escalations_page(
        &self,
        include_closed: bool,
        undelivered_after: Option<&str>,
    ) -> Result<ListOperatorEscalationsResultV1> {
        let after = undelivered_after.map(undelivered_cursor).transpose()?;
        let escalations = hops_where(
            &self.conn,
            "WHERE h.target_ref='operator' AND (?1 OR h.state='open') ORDER BY h.created_at DESC,h.id LIMIT 256",
            [include_closed],
        )?;
        let rows: Vec<(
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
        )> = {
            let mut statement = self.conn.prepare(
                "SELECT id,source_ref,source_session_id,project_id,body,state,created_at FROM manager_tier_messages
                 WHERE target_ref='operator' AND (?1 OR state='queued') ORDER BY created_at DESC,id LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![include_closed, OPERATOR_QUEUE_LIMIT as i64],
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
                )?
                .collect::<rusqlite::Result<_>>()?
        };
        let notices = rows
            .into_iter()
            .map(|(id, source_ref, session, project, body, state, created)| {
                Ok(OperatorNoticeV1 {
                    message_id: parse_uuid(&id)?,
                    source_ref,
                    source_session_id: session.as_deref().map(parse_uuid).transpose()?,
                    project_id: project.as_deref().map(parse_uuid).transpose()?,
                    body,
                    state,
                    created_at: parse_time(&created)?,
                })
            })
            .collect::<Result<_>>()?;
        let (page, more) = undelivered_page_on(&self.conn, None, after, OPERATOR_QUEUE_LIMIT)?;
        let next_undelivered_after = if more {
            page.last().map(|(_, cursor)| cursor.clone())
        } else {
            None
        };
        let undelivered = page.into_iter().map(|(row, _)| row).collect();
        Ok(ListOperatorEscalationsResultV1 {
            escalations,
            notices,
            undelivered,
            next_undelivered_after,
        })
    }

    /// Operator-only `RuleOperatorEscalation`: rule an escalation at the top
    /// of its chain; the ruling returns down the recorded hops to the source
    /// seat. A replay under the same key returns the ruled hop. It never
    /// answers a human approval.
    ///
    /// # Errors
    /// `manager_tier_invalid_request`, `operator_escalation_not_found`,
    /// `operator_escalation_not_open`, `manager_tier_idempotency_conflict`.
    pub fn rule_operator_escalation(
        &self,
        request: &RuleOperatorEscalationRequestV1,
    ) -> Result<ManagerTierEscalationHopV1> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let prior: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT hop_id,ruling FROM manager_tier_escalation_events
                 WHERE actor_ref='operator' AND idempotency_key=?1",
                [&request.idempotency_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((hop, ruling)) = prior {
            if hop != request.hop_id.to_string() || ruling.as_deref() != Some(&request.ruling) {
                return Err(refused(MANAGER_TIER_IDEMPOTENCY_CONFLICT));
            }
            let view = hop_by_id_on(&self.conn, request.hop_id)?
                .ok_or_else(|| refused(OPERATOR_ESCALATION_NOT_FOUND))?;
            tx.commit()?;
            return Ok(view);
        }
        let hop = hop_by_id_on(&self.conn, request.hop_id)?
            .ok_or_else(|| refused(OPERATOR_ESCALATION_NOT_FOUND))?;
        if hop.target_ref != OPERATOR_REF || hop.state != "open" {
            return Err(refused(OPERATOR_ESCALATION_NOT_OPEN));
        }
        let escalation = super::harness_manager::escalation_on(&tx, hop.escalation_id)?
            .ok_or_else(|| refused(OPERATOR_ESCALATION_NOT_FOUND))?;
        if escalation.state != ManagerNodeEscalationStateV1::Open {
            return Err(refused(OPERATOR_ESCALATION_NOT_OPEN));
        }
        if let Err(error) = super::harness_manager::escalation_source_current_on(&tx, &escalation) {
            self.tier_retire_hop_for_stale_source(&hop)?;
            tx.commit()?;
            return Err(error);
        }
        let now = stamp();
        self.rule_tier_hop(
            &escalation,
            &hop,
            &TierEndpoint::OPERATOR,
            &request.ruling,
            Some(&request.idempotency_key),
            &now,
        )?;
        let view = hop_by_id_on(&self.conn, request.hop_id)?
            .ok_or_else(|| refused(OPERATOR_ESCALATION_NOT_FOUND))?;
        tx.commit()?;
        Ok(view)
    }

    /// Operator-only `AcknowledgeOperatorNotice`: mark a top-of-chain report
    /// read. Idempotent.
    ///
    /// # Errors
    /// `operator_notice_not_found`.
    pub fn acknowledge_operator_notice(&self, message_id: Uuid) -> Result<OperatorNoticeV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let found: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_tier_messages WHERE id=?1 AND target_ref='operator')",
            [message_id.to_string()],
            |row| row.get(0),
        )?;
        if !found {
            return Err(refused(OPERATOR_NOTICE_NOT_FOUND));
        }
        self.conn.execute(
            "UPDATE manager_tier_messages SET state='delivered',updated_at=?2 WHERE id=?1 AND state='queued'",
            params![message_id.to_string(), stamp()],
        )?;
        let (source_ref, session, project, body, state, created): (
            String,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
        ) = self.conn.query_row(
            "SELECT source_ref,source_session_id,project_id,body,state,created_at FROM manager_tier_messages WHERE id=?1",
            [message_id.to_string()],
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
        tx.commit()?;
        Ok(OperatorNoticeV1 {
            message_id,
            source_ref,
            source_session_id: session.as_deref().map(parse_uuid).transpose()?,
            project_id: project.as_deref().map(parse_uuid).transpose()?,
            body,
            state,
            created_at: parse_time(&created)?,
        })
    }
}

// The fixtures come from the store-01 shard's area-node tests.
#[cfg(test)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[path = "manager_tier_routing_tests.rs"]
mod tests;
