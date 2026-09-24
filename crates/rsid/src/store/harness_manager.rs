//! Operator-owned scope and a provider-independent, correlated manager inbox.
//!
//! The inbox is the durable exchange. Watches carry only a notice to read it;
//! reading and replying always revalidate the current grant and Epic lead.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::Utc;
use rsi_common::harness_manager::*;
use rsi_common::types::{
    Recurrence, ScheduleSpec, ScheduledJob, Session, SessionKind, SessionStatus, WakeMode,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::Store;
use crate::error::{DaemonError, Result};

const LINEAGE_LIMIT: usize = 256;
const MAX_ACTIVE_MANAGERS: usize = 64;
/// #664 (c): open manager requests one Epic may hold. A full Epic refuses
/// only its own sends; other Epics keep their capacity.
pub(super) const MAX_PENDING_REQUESTS_PER_EPIC: u32 = 32;
/// #664 (c): project-wide ceiling across every Epic in the current scope.
pub(super) const MAX_PENDING_REQUESTS_PER_PROJECT: u32 = 256;
/// Surfaced when the project or any Epic reaches 75% of its limit.
const MAIL_CAPACITY_WARNING: &str = "manager_mail_capacity_75pct";
pub(crate) const MAX_NOTICE_EPICS_PER_PASS: usize = 32;
const MANAGER_SCOPE_WALK_LIMIT: usize = 32;

/// A session the current appointed manager reaches through its live scope.
#[derive(Debug, Clone)]
pub(crate) struct ManagerSessionScope {
    pub config: HarnessManagerConfigV1,
    /// Nearest Epic at or above `target` (the target itself for an Epic).
    pub epic_id: Uuid,
    pub target: Session,
}

fn refused(code: &'static str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

fn stamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn uuid(value: String) -> Result<Uuid> {
    Uuid::parse_str(&value).map_err(|_| refused("manager_invalid_stored_identity"))
}

/// Lineage facts `manager_session` enforces, read through any connection or
/// open transaction: a leaf that is not Deleted.
struct ManagerLineageSession {
    project_id: Option<Uuid>,
    parent_id: Option<Uuid>,
    status: SessionStatus,
}

fn manager_lineage_session_on(
    conn: &rusqlite::Connection,
    id: Uuid,
) -> Result<ManagerLineageSession> {
    let (project_id, parent_id, kind, status): (Option<String>, Option<String>, String, String) =
        conn.query_row(
            "SELECT project_id,parent_id,session_kind,status FROM sessions WHERE id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| refused("manager_session_unavailable"))?;
    let status = super::row_mappers::str_to_session_status(&status)?;
    if !rsi_common::is_leaf_kind(super::row_mappers::str_to_session_kind(&kind)?)
        || matches!(status, SessionStatus::Deleted)
    {
        return Err(refused("manager_live_leaf_required"));
    }
    Ok(ManagerLineageSession {
        project_id: project_id.map(uuid).transpose()?,
        parent_id: parent_id.map(uuid).transpose()?,
        status,
    })
}

/// Committed successor edges of `predecessor` (at most two, so a fork is
/// detectable without scanning further).
fn manager_successors_on(conn: &rusqlite::Connection, predecessor: Uuid) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT child.id FROM sessions child
         JOIN sessions predecessor ON predecessor.id=child.continued_from
         LEFT JOIN agent_successor_reservations reservation ON reservation.candidate_session_id=child.id
         LEFT JOIN harness_manager_rotation_edges edge
           ON edge.predecessor_session_id=predecessor.id AND edge.successor_session_id=child.id
              AND edge.retired_at IS NULL
         WHERE child.continued_from=?1 AND
           (reservation.state='committed' OR (reservation.reservation_id IS NULL AND
             edge.successor_session_id IS NOT NULL AND predecessor.status='Archived'))
         ORDER BY child.id LIMIT 2",
    )?;
    Ok(statement
        .query_map([predecessor.to_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Bounded, unambiguous same-project/same-parent lineage tip; never guess a
/// fork. Every successor must share the ORIGIN's project and parent. Reads go
/// through `conn`, so a caller holding a transaction sees exactly its snapshot.
pub(super) fn manager_lineage_tip_on(conn: &rusqlite::Connection, origin: Uuid) -> Result<Uuid> {
    let base = manager_lineage_session_on(conn, origin)?;
    let mut current = origin;
    let mut seen = HashSet::new();
    for _ in 0..LINEAGE_LIMIT {
        if !seen.insert(current) {
            return Err(refused("manager_lineage_cycle"));
        }
        let next = manager_successors_on(conn, current)?;
        if next.is_empty() {
            return Ok(current);
        }
        if next.len() != 1 {
            return Err(refused("manager_lineage_ambiguous"));
        }
        let id = uuid(next[0].clone())?;
        let session = manager_lineage_session_on(conn, id)?;
        if session.project_id != base.project_id || session.parent_id != base.parent_id {
            return Err(refused("manager_lineage_scope_changed"));
        }
        current = id;
    }
    Err(refused("manager_lineage_limit"))
}

/// The current appointed manager session for `project_id` whose appointment
/// anchor is `anchor`: the lineage tip, provided it is a live (not Archived or
/// Deleted) leaf in that project. This is the single definition behind
/// `HarnessManagerConfigV1::current_session_id` and the guarded Issue
/// coordinator path.
pub(super) fn current_manager_session_on(
    conn: &rusqlite::Connection,
    project_id: Uuid,
    anchor: Uuid,
) -> Option<Uuid> {
    let tip = manager_lineage_tip_on(conn, anchor).ok()?;
    manager_lineage_session_on(conn, tip)
        .ok()
        .filter(|session| {
            session.project_id == Some(project_id)
                && !matches!(
                    session.status,
                    SessionStatus::Archived | SessionStatus::Deleted
                )
        })
        .map(|_| tip)
}

/// Server-owned fingerprint namespace marking a lead-origin informational
/// notice (#404). Manager requests and lead replies store a bare 64-hex
/// SHA-256, so no existing row can carry this prefix; callers never choose
/// fingerprints. This is the durable, insert-time origin witness that keeps
/// every `request_id IS NULL` projection exact without a direction column.
const LEAD_NOTICE_FINGERPRINT_PREFIX: &str = "lead-notice:v1:";

/// SQL predicate selecting manager-origin requests only. Lead-origin notices
/// also have `request_id IS NULL` but are never requests: they do not count
/// toward the pending request budget, are never reply targets, and never
/// appear as `recent_requests` or v2 request rows.
pub(super) const MANAGER_REQUEST_ROW: &str =
    "request_id IS NULL AND request_fingerprint NOT GLOB 'lead-notice:*'";

/// Settlement markers (#664). Each is an append-only v2 record keyed by the
/// request id in the request's own (project, anchor, scope) — never a message
/// rewrite. `request_settle` is a settlement (today only the daemon's
/// `lead_replaced` orphan disposition); `request_released` is the permanent
/// capacity release written when the lead reports a terminal state.
pub(super) const REQUEST_SETTLE_KIND: &str = "request_settle";
pub(super) const REQUEST_RELEASED_KIND: &str = "request_released";
/// Voids one daemon `lead_replaced` settlement (manager ruling on review of
/// 3e1177de1): written when that request's recipient lineage is again the
/// Epic's current lead and it replies. Payload `{settle_row_version,
/// disposition, actor}`; it voids only the settle row version it names. The
/// reply is inserted in the same transaction, so an unsettled request is
/// always replied and never re-enters the open set.
pub(super) const REQUEST_UNSETTLED_KIND: &str = "request_unsettled";
/// #656 rollover chain record: ONE row per rolled-over standing request,
/// key = the root request id, payload `{root_request_id, generations:
/// [root, successor 1, ...]}` (at most `LINEAGE_LIMIT` ids). Created at CAS 0
/// on the first rollover and CAS-extended on each later one, so a scope holds
/// one row per rolled chain, never one per generation (review 1cdaa85f).
pub(super) const REQUEST_ROLLOVER_KIND: &str = "request_rollover";
/// Server-owned idempotency-key prefix of a rollover successor, which is keyed
/// `rollover:{root}:{generation}`. Origin is recognized only when the root's
/// chain record lists the message at that generation; `manager_send` refuses
/// caller keys with this prefix.
const ROLLOVER_KEY_PREFIX: &str = "rollover:";
/// Replies one request generation stores before its standing request rolls
/// over to a successor generation (#656).
pub(super) const MAX_REPLIES_PER_REQUEST: i64 = 32;

/// One standing request's rollover chain (#656), resolved from any generation.
#[derive(Debug)]
struct RolloverChain {
    /// Generation 0: the request the manager actually sent.
    root: Uuid,
    /// `[root, successor 1, ...]`, at most `LINEAGE_LIMIT` ids.
    ids: Vec<Uuid>,
    /// Row version of the root's chain record; 0 while it never rolled over.
    version: i64,
}

impl RolloverChain {
    fn json(&self) -> String {
        serde_json::Value::from(self.ids.iter().map(ToString::to_string).collect::<Vec<_>>())
            .to_string()
    }
}

/// Open requests one orphan-sweep pass may inspect (bounded work per call).
const ORPHAN_SWEEP_LIMIT: i64 = 256;

/// The single "open request" definition (#664 foundation): the capacity cap,
/// Inspect `unanswered_only` and the readdress candidate query all use it, so
/// they can no longer disagree. The outer row must be aliased `m`. A request
/// is open iff it is a manager request row with no reply, no readdress /
/// settle / release marker, and no lead-terminal lifecycle state. A release
/// marker is permanent, so a `failed -> accepted` reopen never reclaims a slot
/// (plan r2 "Permanent release"): the cap is an admission budget on manager
/// sends, not an invariant on the active count. The reopened request stays
/// visible to attention through [`manager_request_unanswered_sql`].
pub(super) fn manager_request_open_sql() -> String {
    format!(
        "{MANAGER_REQUEST_ROW}
         AND NOT EXISTS(SELECT 1 FROM harness_manager_messages reply WHERE reply.request_id=m.id)
         AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_records v WHERE v.project_id=m.project_id
           AND v.manager_session_id=m.manager_session_id AND v.scope_version=m.scope_version AND v.record_key=m.id
           AND (v.kind IN ('request_readdress','{REQUEST_SETTLE_KIND}','{REQUEST_RELEASED_KIND}')
             OR (v.kind='request' AND json_extract(v.payload_json,'$.state') IN ('completed','failed','declined'))))"
    )
}

/// The attention ("unanswered") predicate: the open set plus reopened
/// requests, i.e. released rows whose current lifecycle state is again
/// non-terminal after `failed -> accepted`. Inspect `unanswered_only` and
/// Health read it, so a reopen stays visible there while the send caps,
/// Progress `pending_requests` and mail capacity keep reading
/// [`manager_request_open_sql`] and never give the slot back. The outer row
/// must be aliased `m`.
pub(super) fn manager_request_unanswered_sql() -> String {
    format!(
        "{MANAGER_REQUEST_ROW}
         AND NOT EXISTS(SELECT 1 FROM harness_manager_messages reply WHERE reply.request_id=m.id)
         AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_records v WHERE v.project_id=m.project_id
           AND v.manager_session_id=m.manager_session_id AND v.scope_version=m.scope_version AND v.record_key=m.id
           AND (v.kind IN ('request_readdress','{REQUEST_SETTLE_KIND}')
             OR (v.kind='request' AND json_extract(v.payload_json,'$.state') IN ('completed','failed','declined'))))
         AND (NOT EXISTS(SELECT 1 FROM harness_manager_v2_records v WHERE v.project_id=m.project_id
             AND v.manager_session_id=m.manager_session_id AND v.scope_version=m.scope_version
             AND v.record_key=m.id AND v.kind='{REQUEST_RELEASED_KIND}')
           OR EXISTS(SELECT 1 FROM harness_manager_v2_records v WHERE v.project_id=m.project_id
             AND v.manager_session_id=m.manager_session_id AND v.scope_version=m.scope_version
             AND v.record_key=m.id AND v.kind='request'))"
    )
}

/// Unsettled lead notices one Epic may hold for its manager before the lead
/// must wait for the manager to read them (bounded backpressure).
const MAX_PENDING_LEAD_NOTICES: i64 = 128;

fn fingerprint(value: &impl serde::Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|_| refused("manager_invalid_request"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[derive(Debug)]
struct StoredMessage {
    public: HarnessManagerMessageV1,
    project_id: Uuid,
    manager_session_id: Uuid,
    scope_version: i64,
    request_fingerprint: String,
    idempotency_key: String,
}

impl StoredMessage {
    /// Lead-origin informational notice (#404), fixed at insert time.
    fn is_lead_notice(&self) -> bool {
        self.request_fingerprint
            .starts_with(LEAD_NOTICE_FINGERPRINT_PREFIX)
    }
}

const MESSAGE_COLUMNS: &str =
    "id,sequence,request_id,epic_id,sender_session_id,recipient_session_id,
    message,created_at,project_id,manager_session_id,scope_version,request_fingerprint,idempotency_key";

fn read_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    fn id(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Uuid> {
        let text: String = row.get(column)?;
        Uuid::parse_str(&text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }
    let request: Option<String> = row.get(2)?;
    let created: String = row.get(7)?;
    Ok(StoredMessage {
        public: HarnessManagerMessageV1 {
            message_id: id(row, 0)?,
            sequence: row.get(1)?,
            request_id: request
                .map(|text| Uuid::parse_str(&text))
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            epic_id: id(row, 3)?,
            sender_session_id: id(row, 4)?,
            recipient_session_id: id(row, 5)?,
            message: row.get(6)?,
            created_at: super::parse_timestamp(&created).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(error)),
                )
            })?,
            replied: false,
            readdressed_from: None,
            standing_root_id: None,
        },
        project_id: id(row, 8)?,
        manager_session_id: id(row, 9)?,
        scope_version: row.get(10)?,
        request_fingerprint: row.get(11)?,
        idempotency_key: row.get(12)?,
    })
}

impl Store {
    /// One bounded page includes empty Groups and preserves each row's identity.
    pub fn list_harness_manager_scope(
        &self,
        request: &ListHarnessManagerScopeRequestV1,
    ) -> Result<ListHarnessManagerScopeResultV1> {
        request.validate().map_err(refused)?;
        let mut statement = self.conn.prepare(
            "SELECT g.id,COALESCE(NULLIF(trim(g.title),''),g.query),'Group',NULL,NULL
             FROM sessions g WHERE g.project_id=?1 AND g.session_kind='Group'
               AND g.parent_id IS NULL AND g.status NOT IN ('Archived','Deleted') AND g.id>?2
             UNION ALL
             SELECT e.id,COALESCE(NULLIF(trim(e.title),''),e.query),'Epic',g.id,
                    COALESCE(NULLIF(trim(g.title),''),g.query)
             FROM sessions e JOIN sessions g ON g.id=e.parent_id
             WHERE e.project_id=?1 AND e.session_kind='Epic'
               AND e.status NOT IN ('Archived','Deleted')
               AND g.project_id=e.project_id AND g.session_kind='Group'
               AND g.parent_id IS NULL AND g.status NOT IN ('Archived','Deleted') AND e.id>?2
             ORDER BY 1 LIMIT ?3",
        )?;
        let mut rows = Vec::new();
        for row in statement.query_map(
            params![
                request.project_id.to_string(),
                request
                    .after_id
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
                i64::from(request.limit) + 1
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )? {
            let (id, title, kind, group_id, group_title) = row?;
            rows.push(HarnessManagerScopeCandidateV1 {
                id: uuid(id)?,
                title,
                kind: if kind == "Group" {
                    SessionKind::Group
                } else {
                    SessionKind::Epic
                },
                group_id: group_id.map(uuid).transpose()?,
                group_title,
            });
        }
        let has_more = rows.len() > usize::from(request.limit);
        rows.truncate(usize::from(request.limit));
        Ok(ListHarnessManagerScopeResultV1 {
            next_after_id: has_more.then(|| rows.last().expect("nonempty page").id),
            rows,
        })
    }

    /// Keyset-paged legal project Epics, including rows never opened in the TUI.
    pub fn list_harness_manager_epics(
        &self,
        request: &ListHarnessManagerEpicsRequestV1,
    ) -> Result<ListHarnessManagerEpicsResultV1> {
        request.validate().map_err(refused)?;
        let mut statement = self.conn.prepare(
            "SELECT e.id,COALESCE(NULLIF(trim(e.title),''),e.query),
                    COALESCE(NULLIF(trim(g.title),''),g.query)
             FROM sessions e JOIN sessions g ON g.id=e.parent_id
             WHERE e.project_id=?1 AND e.session_kind='Epic'
               AND e.status NOT IN ('Archived','Deleted')
               AND g.project_id=e.project_id AND g.session_kind='Group'
               AND g.parent_id IS NULL AND g.status NOT IN ('Archived','Deleted')
               AND e.id>?2 ORDER BY e.id LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                request.project_id.to_string(),
                request
                    .after_id
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
                i64::from(request.limit) + 1
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut epics = Vec::new();
        for row in rows {
            let (id, title, group_title) = row?;
            epics.push(HarnessManagerEpicCandidateV1 {
                id: uuid(id)?,
                title,
                group_title,
            });
        }
        let has_more = epics.len() > usize::from(request.limit);
        epics.truncate(usize::from(request.limit));
        Ok(ListHarnessManagerEpicsResultV1 {
            next_after_id: has_more.then(|| epics.last().expect("nonempty page").id),
            epics,
        })
    }

    /// Read the explicit durable scope without expanding dynamic Project/Group
    /// membership. Reconciliation uses this form so scope discovery happens
    /// only through the retained keyset cursor below.
    pub(crate) fn get_harness_manager_notice_config(
        &self,
        project_id: Uuid,
    ) -> Result<Option<HarnessManagerConfigV1>> {
        let row: Option<(String, String, i64, String, String, String)> = self.conn.query_row(
            "SELECT manager_session_id,epic_ids_json,row_version,updated_at,scope_mode,group_ids_json
             FROM harness_manager_scopes WHERE project_id=?1",
            [project_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).optional()?;
        row.map(|(manager, epics, row_version, updated_at, mode, groups)| {
            let manager_session_id = uuid(manager)?;
            let current_session_id =
                current_manager_session_on(&self.conn, project_id, manager_session_id);
            let selected_epic_ids: Vec<Uuid> = serde_json::from_str(&epics)
                .map_err(|_| refused("manager_invalid_stored_scope"))?;
            let group_ids: Vec<Uuid> = serde_json::from_str(&groups)
                .map_err(|_| refused("manager_invalid_stored_scope"))?;
            let scope_mode = match mode.as_str() {
                "project" => HarnessManagerScopeModeV1::Project,
                "selected" => HarnessManagerScopeModeV1::Selected,
                _ => return Err(refused("manager_invalid_stored_scope")),
            };
            if selected_epic_ids.len() > HARNESS_MANAGER_MAX_EPICS
                || group_ids.len() > HARNESS_MANAGER_MAX_GROUPS
            {
                return Err(refused("manager_invalid_stored_scope"));
            }
            Ok(HarnessManagerConfigV1 {
                project_id,
                manager_session_id,
                current_session_id,
                epic_ids: selected_epic_ids.clone(),
                scope_mode,
                selected_epic_ids: Some(selected_epic_ids),
                group_ids,
                row_version,
                updated_at: super::parse_timestamp(&updated_at)
                    .map_err(|_| refused("manager_invalid_stored_timestamp"))?,
            })
        })
        .transpose()
    }

    /// Operator read. Effective membership follows current legal topology.
    pub fn get_harness_manager(&self, project_id: Uuid) -> Result<Option<HarnessManagerConfigV1>> {
        let Some(mut config) = self.get_harness_manager_notice_config(project_id)? else {
            return Ok(None);
        };
        let mut epic_ids = config.explicit_epic_ids().to_vec();
        if config.scope_mode == HarnessManagerScopeModeV1::Project || !config.group_ids.is_empty() {
            let groups = serde_json::to_string(&config.group_ids)
                .map_err(|_| refused("manager_invalid_stored_scope"))?;
            let mode = if config.scope_mode == HarnessManagerScopeModeV1::Project {
                "project"
            } else {
                "selected"
            };
            let mut statement = self.conn.prepare(
                "SELECT e.id FROM sessions e JOIN sessions g ON g.id=e.parent_id
                 WHERE e.project_id=?1 AND e.session_kind='Epic'
                   AND e.status NOT IN ('Archived','Deleted')
                   AND g.project_id=e.project_id AND g.session_kind='Group'
                   AND g.parent_id IS NULL AND g.status NOT IN ('Archived','Deleted')
                   AND (?2='project' OR g.id IN (SELECT value FROM json_each(?3))) ORDER BY e.id",
            )?;
            for id in statement.query_map(params![project_id.to_string(), mode, groups], |row| {
                row.get::<_, String>(0)
            })? {
                epic_ids.push(uuid(id?)?);
            }
        }
        let mut created = self.conn.prepare(
            "SELECT session_id FROM harness_manager_v2_entities
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND kind='Epic' ORDER BY session_id",
        )?;
        for id in created.query_map(
            params![
                project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version
            ],
            |row| row.get::<_, String>(0),
        )? {
            let id = uuid(id?)?;
            if config.scope_mode == HarnessManagerScopeModeV1::Selected
                || self.manager_epic(project_id, id).is_ok()
            {
                epic_ids.push(id);
            }
        }
        epic_ids.sort_unstable();
        epic_ids.dedup();
        if config.scope_mode == HarnessManagerScopeModeV1::Selected
            && config.group_ids.is_empty()
            && epic_ids.len() > HARNESS_MANAGER_MAX_EPICS
        {
            return Err(refused("manager_invalid_stored_scope"));
        }
        config.epic_ids = epic_ids;
        Ok(Some(config))
    }

    pub(crate) fn manager_config_covers_epic(
        &self,
        config: &HarnessManagerConfigV1,
        epic_id: Uuid,
    ) -> Result<bool> {
        if config.explicit_epic_ids().contains(&epic_id) {
            return Ok(true);
        }
        let created: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_entities
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND kind='Epic' AND session_id=?4)",
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                epic_id.to_string()
            ],
            |row| row.get(0),
        )?;
        if created
            && (config.scope_mode == HarnessManagerScopeModeV1::Selected
                || self.manager_epic(config.project_id, epic_id).is_ok())
        {
            return Ok(true);
        }
        if config.scope_mode == HarnessManagerScopeModeV1::Project {
            return Ok(self.manager_epic(config.project_id, epic_id).is_ok());
        }
        if config.group_ids.is_empty() {
            return Ok(false);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sessions e JOIN sessions g ON g.id=e.parent_id
                WHERE e.id=?1 AND e.project_id=?2 AND e.session_kind='Epic'
                  AND e.status NOT IN ('Archived','Deleted')
                  AND g.id IN (SELECT value FROM json_each(?3))
                  AND g.project_id=e.project_id AND g.session_kind='Group'
                  AND g.parent_id IS NULL AND g.status NOT IN ('Archived','Deleted'))",
            params![
                epic_id.to_string(),
                config.project_id.to_string(),
                serde_json::to_string(&config.group_ids)
                    .map_err(|_| refused("manager_invalid_stored_scope"))?
            ],
            |row| row.get(0),
        )?)
    }

    /// Server-bound reach of the appointed manager over one session.
    ///
    /// Returns `Some` only when `caller` is the daemon-resolved current
    /// manager principal of `target`'s project and the nearest Epic at or
    /// above `target` is covered by the live scope. Every other caller —
    /// leads, workers, a stale manager predecessor, a revoked or foreign
    /// manager — gets `None`, so callers fall through to their ordinary
    /// self/child/lead rules. Scope only: capability, mode and pause checks
    /// stay with each verb because reads and mutations differ.
    pub(crate) fn manager_session_scope(
        &self,
        caller: Uuid,
        target: Uuid,
    ) -> Result<Option<ManagerSessionScope>> {
        if caller == target {
            return Ok(None);
        }
        let Some(project_id) = self.get_session(caller)?.and_then(|s| s.project_id) else {
            return Ok(None);
        };
        let Some(config) = self.get_harness_manager_notice_config(project_id)? else {
            return Ok(None);
        };
        if config.current_session_id != Some(caller) || config.is_revoked() {
            return Ok(None);
        }
        let Some(target) = self.get_session(target)? else {
            return Ok(None);
        };
        if target.project_id != Some(project_id) {
            return Ok(None);
        }
        // Nearest Epic at or above the target. Bounded: legal hierarchy is
        // Group > Epic > leaf > nested leaves, so a long walk means a cycle
        // or corrupt topology, and corrupt topology grants nothing.
        let mut cursor = Some(target.clone());
        let mut epic_id = None;
        for _ in 0..MANAGER_SCOPE_WALK_LIMIT {
            let Some(row) = cursor else { break };
            if row.project_id != Some(project_id) {
                break;
            }
            if row.session_kind == SessionKind::Epic {
                epic_id = Some(row.id);
                break;
            }
            cursor = match row.parent_id {
                Some(parent) => self.get_session(parent)?,
                None => None,
            };
        }
        let Some(epic_id) = epic_id else {
            return Ok(None);
        };
        if !self.manager_config_covers_epic(&config, epic_id)? {
            return Ok(None);
        }
        Ok(Some(ManagerSessionScope {
            config,
            epic_id,
            target,
        }))
    }

    pub(crate) fn manager_notice_cursor_after(
        &self,
        config: &HarnessManagerConfigV1,
        lane: &str,
        owner: Option<Uuid>,
    ) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT after_key FROM harness_manager_notice_reconcile_cursors
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND lane=?4 AND owner_id=?5",
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    lane,
                    owner.map(|id| id.to_string()).unwrap_or_default()
                ],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    pub(crate) fn advance_manager_notice_cursor(
        &self,
        config: &HarnessManagerConfigV1,
        lane: &str,
        owner: Option<Uuid>,
        after_key: Option<&str>,
    ) -> Result<()> {
        let after_key = after_key.unwrap_or_default();
        self.conn.execute(
            "INSERT INTO harness_manager_notice_reconcile_cursors(
                project_id,manager_session_id,scope_version,lane,owner_id,
                after_key,cycle,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,CASE WHEN ?6='' THEN 1 ELSE 0 END,?7)
             ON CONFLICT(project_id,manager_session_id,scope_version,lane,owner_id)
             DO UPDATE SET after_key=excluded.after_key,
                 cycle=cycle+CASE WHEN excluded.after_key='' THEN 1 ELSE 0 END,
                 updated_at=excluded.updated_at",
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                lane,
                owner.map(|id| id.to_string()).unwrap_or_default(),
                after_key,
                stamp()
            ],
        )?;
        Ok(())
    }

    /// One fixed keyset page over explicit, Project/Group, and v2-created
    /// membership. Each source is independently indexed and limited before
    /// the bounded merge, so a large scope cannot expand inside a mutation.
    pub(crate) fn manager_notice_epic_page(
        &self,
        config: &HarnessManagerConfigV1,
        lane: &str,
    ) -> Result<Vec<Uuid>> {
        let after = self.manager_notice_cursor_after(config, lane, None)?;
        let limit = MAX_NOTICE_EPICS_PER_PASS as i64;
        let mut candidates = BTreeSet::new();
        for epic in config.explicit_epic_ids() {
            if epic.to_string() > after {
                candidates.insert(*epic);
            }
        }
        if config.scope_mode == HarnessManagerScopeModeV1::Project {
            let mut statement = self.conn.prepare(
                "SELECT e.id FROM sessions e INDEXED BY manager_project_scope_candidates
                 JOIN sessions g ON g.id=e.parent_id
                 WHERE e.project_id=?1 AND e.session_kind='Epic' AND e.id>?2
                   AND e.status NOT IN ('Archived','Deleted')
                   AND g.project_id=e.project_id AND g.session_kind='Group'
                   AND g.parent_id IS NULL AND g.status NOT IN ('Archived','Deleted')
                 ORDER BY e.id LIMIT ?3",
            )?;
            for id in statement.query_map(
                params![config.project_id.to_string(), after, limit],
                |row| row.get::<_, String>(0),
            )? {
                candidates.insert(uuid(id?)?);
            }
        } else {
            for group in &config.group_ids {
                if self.manager_scope_group(config.project_id, *group).is_err() {
                    continue;
                }
                let mut statement = self.conn.prepare(
                    "SELECT id FROM sessions INDEXED BY harness_manager_notice_group_epic_page
                     WHERE parent_id=?1 AND session_kind='Epic' AND id>?2
                       AND project_id=?3 AND status NOT IN ('Archived','Deleted')
                     ORDER BY id LIMIT ?4",
                )?;
                for id in statement.query_map(
                    params![
                        group.to_string(),
                        after,
                        config.project_id.to_string(),
                        limit
                    ],
                    |row| row.get::<_, String>(0),
                )? {
                    candidates.insert(uuid(id?)?);
                }
            }
        }
        let mut created = self.conn.prepare(
            "SELECT session_id FROM harness_manager_v2_entities
                 INDEXED BY harness_manager_notice_entity_epic_page
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND kind='Epic' AND session_id>?4
             ORDER BY session_id LIMIT ?5",
        )?;
        for id in created.query_map(
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                after,
                limit
            ],
            |row| row.get::<_, String>(0),
        )? {
            let id = uuid(id?)?;
            if config.scope_mode == HarnessManagerScopeModeV1::Selected
                || self.manager_epic(config.project_id, id).is_ok()
            {
                candidates.insert(id);
            }
        }
        let page: Vec<_> = candidates
            .into_iter()
            .take(MAX_NOTICE_EPICS_PER_PASS)
            .collect();
        if page.is_empty() && !after.is_empty() {
            self.advance_manager_notice_cursor(config, lane, None, None)?;
            return self.manager_notice_epic_page(config, lane);
        }
        self.advance_manager_notice_cursor(
            config,
            lane,
            None,
            page.last().map(|id| id.to_string()).as_deref(),
        )?;
        Ok(page)
    }

    pub(crate) fn manager_scope_group(&self, project_id: Uuid, id: Uuid) -> Result<Session> {
        let group = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_legal_group_required"))?;
        if group.project_id != Some(project_id)
            || group.session_kind != SessionKind::Group
            || group.parent_id.is_some()
            || matches!(
                group.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        {
            return Err(refused("manager_legal_group_required"));
        }
        Ok(group)
    }

    /// One operator CAS updates scope and its notices atomically.
    pub fn configure_harness_manager(
        &self,
        request: &ConfigureHarnessManagerRequestV1,
    ) -> Result<HarnessManagerConfigV1> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let session = if request.is_revocation() {
            // A deleted/retired anchor must still allow the operator to revoke
            // its existing scope without resurrecting the session.
            self.get_session(request.session_id)?
                .ok_or_else(|| refused("manager_session_unavailable"))?
        } else {
            self.manager_session(request.session_id)?
        };
        let project_id = request.project_id;
        let previous = self.get_harness_manager_notice_config(project_id)?;
        if session.project_id != Some(project_id)
            && (!request.is_revocation()
                || previous
                    .as_ref()
                    .is_none_or(|config| config.manager_session_id != session.id))
        {
            return Err(refused("manager_project_mismatch"));
        }
        if previous.is_none() && request.is_revocation() {
            self.manager_session(session.id)?;
        }
        // A scope edit may name the persisted anchor after rotation. Explicit
        // appointment of another session starts a new grant at that session.
        let anchor = previous
            .as_ref()
            .filter(|config| {
                request.is_revocation()
                    || config.manager_session_id == session.id
                    || self.manager_lineage_tip(config.manager_session_id).ok() == Some(session.id)
            })
            .map_or(session.id, |config| config.manager_session_id);
        // Revocation must work even if the appointed lineage is broken or
        // retired. Granting nonempty scope still requires a live principal.
        let current = if request.is_revocation() {
            None
        } else {
            let current = self.manager_session(self.manager_lineage_tip(anchor)?)?;
            if matches!(
                current.status,
                SessionStatus::Archived | SessionStatus::Deleted
            ) {
                return Err(refused("manager_current_session_required"));
            }
            Some(current)
        };
        for group_id in &request.group_ids {
            self.manager_scope_group(project_id, *group_id)?;
        }
        for epic_id in request.epic_ids.as_deref().unwrap_or_default() {
            self.manager_epic(project_id, *epic_id)?;
            if self.manager_lead(project_id, *epic_id).is_ok_and(|lead| {
                current
                    .as_ref()
                    .is_some_and(|current| lead.id == current.id)
            }) {
                return Err(refused("manager_cannot_supervise_itself"));
            }
        }
        let actual = previous.as_ref().map_or(0, |config| config.row_version);
        if actual != request.expected_row_version {
            return Err(refused("manager_stale_scope_refresh_required"));
        }
        if previous.is_none() {
            let count: i64 =
                self.conn
                    .query_row("SELECT count(*) FROM harness_manager_scopes", [], |row| {
                        row.get(0)
                    })?;
            if count >= MAX_ACTIVE_MANAGERS as i64 {
                return Err(refused("manager_project_limit_reached"));
            }
        }
        let mut epic_ids = request.epic_ids.clone().unwrap_or_default();
        let mut group_ids = request.group_ids.clone();
        group_ids.sort_unstable();
        let scope_mode = match request.scope_mode() {
            HarnessManagerScopeModeV1::Project => "project",
            HarnessManagerScopeModeV1::Selected => "selected",
        };
        epic_ids.sort_unstable();
        if let Some(previous) = &previous {
            let mut previous_epics = previous.explicit_epic_ids().to_vec();
            previous_epics.sort_unstable();
            let mut previous_groups = previous.group_ids.clone();
            previous_groups.sort_unstable();
            if previous.manager_session_id == anchor
                && previous.scope_mode == request.scope_mode()
                && previous_epics == epic_ids
                && previous_groups == group_ids
            {
                tx.commit()?;
                return self
                    .get_harness_manager(project_id)?
                    .ok_or_else(|| refused("manager_scope_missing"));
            }
        }
        let next = actual
            .checked_add(1)
            .ok_or_else(|| refused("manager_scope_version_exhausted"))?;
        let now = stamp();
        if let Some(previous) = &previous {
            self.enqueue_manager_notice_scope_retirement(previous, &now)?;
        }
        self.conn.execute(
            "INSERT INTO harness_manager_scopes(project_id,manager_session_id,epic_ids_json,row_version,updated_at,scope_mode,group_ids_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(project_id) DO UPDATE SET
             manager_session_id=excluded.manager_session_id,epic_ids_json=excluded.epic_ids_json,
             row_version=excluded.row_version,updated_at=excluded.updated_at,scope_mode=excluded.scope_mode,group_ids_json=excluded.group_ids_json",
            params![project_id.to_string(), anchor.to_string(), serde_json::to_string(&epic_ids).map_err(|_| refused("manager_invalid_scope"))?, next, now, scope_mode, serde_json::to_string(&group_ids).map_err(|_| refused("manager_invalid_scope"))?],
        )?;
        self.conn.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id IN
             (SELECT job_id FROM harness_manager_watches WHERE project_id=?1)",
            params![project_id.to_string(), now],
        )?;
        if let Some(previous) = &previous {
            self.reconcile_manager_notice_scope_retirement_owner(
                previous.project_id,
                previous.manager_session_id,
                previous.row_version,
                &now,
            )?;
        }
        let config = self
            .get_harness_manager_notice_config(project_id)?
            .ok_or_else(|| refused("manager_scope_missing"))?;
        for epic in self.manager_notice_epic_page(&config, "v1_epics")? {
            if let Ok(lead) = self.manager_lead(project_id, epic) {
                if matches!(
                    lead.status,
                    SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Interrupted
                        | SessionStatus::WaitingApproval
                ) || lead.pending_question.is_some()
                {
                    self.record_manager_session_notice(&config, &lead)?;
                }
            }
        }
        tx.commit()?;
        self.get_harness_manager(project_id)?
            .ok_or_else(|| refused("manager_scope_missing"))
    }

    /// Bounded, unambiguous same-project/same-parent lineage; never guess a fork.
    pub(crate) fn manager_lineage_tip(&self, origin: Uuid) -> Result<Uuid> {
        manager_lineage_tip_on(&self.conn, origin)
    }

    fn manager_successors(&self, predecessor: Uuid) -> Result<Vec<String>> {
        manager_successors_on(&self.conn, predecessor)
    }

    /// Fixture-only receipt creation. Production must use the guarded custody
    /// finalizer, which archives the predecessor in this same transaction.
    #[cfg(test)]
    pub(crate) fn record_harness_manager_rotation(
        &self,
        predecessor: Uuid,
        successor: Uuid,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let recorded = Self::record_harness_manager_rotation_on(&tx, predecessor, successor)?;
        tx.commit()?;
        Ok(recorded)
    }

    /// Called within the guarded predecessor archival transaction, after
    /// provider establishment. Restoration permanently retires that receipt;
    /// a replay may acknowledge an active edge but can never reactivate one.
    pub(super) fn record_harness_manager_rotation_on(
        tx: &Transaction<'_>,
        predecessor: Uuid,
        successor: Uuid,
    ) -> Result<bool> {
        let load = |id| {
            let (session, _, _, _) =
                super::sandbox_custody::load_rotation_authority_session_on(tx, id)?
                    .ok_or_else(|| refused("manager_session_unavailable"))?;
            if !rsi_common::is_leaf_kind(session.session_kind)
                || session.status == SessionStatus::Deleted
            {
                return Err(refused("manager_live_leaf_required"));
            }
            Ok(session)
        };
        let previous = load(predecessor)?;
        let current = load(successor)?;
        let existing: Option<Option<String>> = tx
            .query_row(
                "SELECT retired_at FROM harness_manager_rotation_edges
                 WHERE predecessor_session_id=?1 AND successor_session_id=?2",
                params![predecessor.to_string(), successor.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if matches!(existing, Some(Some(_))) {
            return Err(refused("manager_rotation_receipt_retired"));
        }
        let Some(project) = previous.project_id else {
            return Ok(false);
        };
        let configured: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_scopes WHERE project_id=?1)",
            [project.to_string()],
            |row| row.get(0),
        )?;
        if !configured {
            return Ok(false);
        }
        if previous.status != SessionStatus::Archived
            || current.continued_from != Some(predecessor)
            || current.project_id != Some(project)
            || current.parent_id != previous.parent_id
            || previous.rotation_depth.checked_add(1) != Some(current.rotation_depth)
        {
            return Err(refused("manager_rotation_commit_mismatch"));
        }
        if existing.is_some() {
            return Ok(true);
        }
        let now = stamp();
        tx.execute(
            "UPDATE harness_manager_rotation_edges SET retired_at=?3
            WHERE predecessor_session_id=?1 AND successor_session_id<>?2 AND retired_at IS NULL",
            params![predecessor.to_string(), successor.to_string(), now],
        )?;
        tx.execute("INSERT INTO harness_manager_rotation_edges(predecessor_session_id,successor_session_id,committed_at)
            VALUES(?1,?2,?3)",
            params![predecessor.to_string(),successor.to_string(),now])?;
        Ok(true)
    }

    pub(crate) fn manager_lineage_root(&self, origin: Uuid) -> Result<Uuid> {
        let base = self.manager_session(origin)?;
        let mut current = base.clone();
        let mut seen = HashSet::new();
        for _ in 0..LINEAGE_LIMIT {
            if !seen.insert(current.id) {
                return Err(refused("manager_lineage_cycle"));
            }
            let Some(parent) = current.continued_from else {
                return Ok(current.id);
            };
            let successors = self.manager_successors(parent)?;
            if successors.is_empty() {
                return Ok(current.id);
            }
            if successors.len() != 1 || uuid(successors[0].clone())? != current.id {
                return Ok(current.id);
            }
            let previous = self.manager_session(parent)?;
            if previous.project_id != base.project_id || previous.parent_id != base.parent_id {
                return Ok(current.id);
            }
            current = previous;
        }
        Err(refused("manager_lineage_limit"))
    }

    pub(crate) fn manager_session(&self, id: Uuid) -> Result<Session> {
        let row = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_session_unavailable"))?;
        if !rsi_common::is_leaf_kind(row.session_kind)
            || matches!(row.status, SessionStatus::Deleted)
        {
            return Err(refused("manager_live_leaf_required"));
        }
        Ok(row)
    }

    pub(crate) fn manager_epic(&self, project: Uuid, id: Uuid) -> Result<Session> {
        let epic = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_epic_unavailable"))?;
        if epic.project_id != Some(project)
            || epic.session_kind != SessionKind::Epic
            || matches!(
                epic.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        {
            return Err(refused("manager_epic_out_of_scope"));
        }
        let group = epic
            .parent_id
            .and_then(|id| self.get_session(id).ok().flatten())
            .ok_or_else(|| refused("manager_legal_epic_required"))?;
        if group.session_kind != SessionKind::Group
            || group.project_id != Some(project)
            || group.parent_id.is_some()
            || matches!(
                group.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        {
            return Err(refused("manager_legal_epic_required"));
        }
        Ok(epic)
    }

    pub(crate) fn manager_lead(&self, project: Uuid, epic_id: Uuid) -> Result<Session> {
        let epic = self.manager_epic(project, epic_id)?;
        let lead = self.manager_session(
            epic.lead_session_id
                .ok_or_else(|| refused("manager_lead_missing"))?,
        )?;
        if lead.parent_id != Some(epic_id)
            || lead.project_id != Some(project)
            || matches!(
                lead.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
            || !rsi_common::legal_children(Some(SessionKind::Epic)).contains(&lead.session_kind)
        {
            return Err(refused("manager_lead_invalid_or_stale"));
        }
        Ok(lead)
    }

    pub(crate) fn manager_config_for_caller(
        &self,
        caller: Uuid,
    ) -> Result<(HarnessManagerConfigV1, bool)> {
        let session = self.manager_session(caller)?;
        if matches!(
            session.status,
            SessionStatus::Archived | SessionStatus::Deleted
        ) {
            return Err(refused("manager_current_session_required"));
        }
        let project = session
            .project_id
            .ok_or_else(|| refused("manager_project_required"))?;
        let config = self
            .get_harness_manager(project)?
            .ok_or_else(|| refused("manager_not_configured"))?;
        let manager = self.manager_session(
            config
                .current_session_id
                .ok_or_else(|| refused("manager_current_session_required"))?,
        )?;
        if matches!(
            manager.status,
            SessionStatus::Archived | SessionStatus::Deleted
        ) {
            return Err(refused("manager_current_session_required"));
        }
        let is_manager = manager.id == caller;
        if !is_manager {
            let epic = session
                .parent_id
                .ok_or_else(|| refused("manager_scope_denied"))?;
            if !config.epic_ids.contains(&epic) || self.manager_lead(project, epic)?.id != caller {
                return Err(refused("manager_scope_denied"));
            }
        }
        Ok((config, is_manager))
    }

    #[cfg(test)]
    pub(crate) fn manager_progress(&self, caller: Uuid) -> Result<AgentManagerProgressResultV1> {
        self.manager_progress_page(caller, &AgentManagerProgressRequestV1::default())
    }

    pub(crate) fn manager_progress_page(
        &self,
        caller: Uuid,
        request: &AgentManagerProgressRequestV1,
    ) -> Result<AgentManagerProgressResultV1> {
        request.validate().map_err(refused)?;
        // Immediate: the #664 orphan sweep may append settle markers.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        if !is_manager {
            return Err(refused("manager_scope_denied"));
        }
        self.settle_orphaned_manager_requests(&config)?;
        let (mail_capacity, per_epic) = self.manager_mail_capacity(&config)?;
        let mut rows = Vec::new();
        let first = config
            .epic_ids
            .partition_point(|id| request.after_epic_id.is_some_and(|after| *id <= after));
        let last = (first + usize::from(request.limit.unwrap_or(32))).min(config.epic_ids.len());
        let next_after_epic_id = (last < config.epic_ids.len()).then(|| config.epic_ids[last - 1]);
        for epic_id in &config.epic_ids[first..last] {
            let epic = self.get_session(*epic_id)?;
            let title = epic.as_ref().map_or_else(
                || epic_id.to_string(),
                |epic| epic.title.clone().unwrap_or_else(|| epic.query.clone()),
            );
            let mut row = HarnessManagerEpicProgressV1 {
                epic_id: *epic_id,
                title,
                lead_session_id: None,
                lead_title: None,
                status: None,
                updated_at: None,
                pending_question: None,
                pipeline_artifact: None,
                last_response: None,
                safe_error_class: None,
                pending_requests: per_epic.get(epic_id).copied().unwrap_or(0),
            };
            match self.manager_lead(config.project_id, *epic_id) {
                Ok(lead) => {
                    row.lead_session_id = Some(lead.id);
                    row.lead_title = lead.title.clone();
                    row.status = Some(lead.status);
                    row.updated_at = Some(lead.updated_at);
                    row.pipeline_artifact = lead.pipeline_artifact;
                    if serde_json::to_vec(&lead.pending_question)
                        .is_ok_and(|bytes| bytes.len() <= 8192)
                    {
                        row.pending_question = lead.pending_question;
                    } else {
                        row.safe_error_class =
                            Some("manager_question_requires_session_view".into());
                    }
                    row.last_response = self.conn.query_row(
                        "SELECT substr(content,1,2048) FROM conversation_events WHERE session_id=?1
                         AND role='Assistant' AND event_type='Message' AND length(content)>0 ORDER BY sequence DESC,id DESC LIMIT 1",
                        [lead.id.to_string()], |event| event.get(0),
                    ).optional()?;
                }
                Err(_) => {
                    row.safe_error_class = Some("manager_lead_unavailable_refresh_scope".into());
                }
            }
            rows.push(row);
        }
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM harness_manager_messages WHERE project_id=?1 AND manager_session_id=?2 AND {MANAGER_REQUEST_ROW} ORDER BY sequence DESC LIMIT 32"
        );
        let mut statement = self.conn.prepare(&query)?;
        let messages = statement
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string()
                ],
                read_message,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut recent_requests = Vec::new();
        for record in messages {
            let readdressed_to =
                self.manager_request_readdressed(&config, record.public.message_id)?;
            let rolled_over_to =
                self.manager_request_rolled_over(&config, record.public.message_id)?;
            let state = if record.scope_version != config.row_version
                || !config.epic_ids.contains(&record.public.epic_id)
            {
                "scope_revoked"
            } else if readdressed_to.is_some() {
                "readdressed"
            } else if self
                .manager_request_settlement(&config, record.public.message_id)?
                .is_some()
            {
                "settled"
            } else if !self.manager_message_live(&config, &record)? {
                "lead_changed"
            } else if rolled_over_to.is_some() {
                "rolled_over"
            } else if self.manager_request_replied(record.public.message_id)? {
                "replied"
            } else {
                "pending_reply"
            };
            recent_requests.push(HarnessManagerRequestSummaryV1 {
                request_id: record.public.message_id,
                epic_id: record.public.epic_id,
                state: state.into(),
                readdressed_to,
                rolled_over_to,
                created_at: record.public.created_at,
            });
        }
        tx.commit()?;
        Ok(AgentManagerProgressResultV1 {
            observed_at: Utc::now(),
            config,
            rows,
            recent_requests,
            next_after_epic_id,
            mail_capacity,
        })
    }

    pub(crate) fn manager_inbox(
        &self,
        caller: Uuid,
        request: &AgentManagerInboxRequestV1,
    ) -> Result<AgentManagerInboxResultV1> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        let epic = if is_manager {
            None
        } else {
            self.manager_session(caller)?.parent_id
        };
        // #656: a request filter expands to the whole rollover chain, so
        // filtering by the standing root returns every generation.
        let filter = request
            .request_id
            .map(|id| self.manager_request_chain(&config, id))
            .transpose()?
            .map(|chain| chain.json());
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM harness_manager_messages
            WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND sequence>?4
            AND (?5 IS NULL OR epic_id=?5) AND (?6 IS NULL
              OR id IN (SELECT value FROM json_each(?6)) OR request_id IN (SELECT value FROM json_each(?6)))
            ORDER BY sequence LIMIT ?7"
        );
        let mut statement = self.conn.prepare(&query)?;
        let records = statement
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    request.after_sequence,
                    epic.map(|id| id.to_string()),
                    filter,
                    i64::from(request.limit) + 1
                ],
                read_message,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let has_more = records.len() > usize::from(request.limit);
        let mut messages = Vec::new();
        let mut last_sequence = request.after_sequence;
        for mut record in records.into_iter().take(usize::from(request.limit)) {
            last_sequence = record.public.sequence;
            if self.manager_message_live(&config, &record)? {
                // A lead notice is never a reply target, so never "replied".
                record.public.replied = record.public.request_id.is_none()
                    && !record.is_lead_notice()
                    && self.manager_request_replied(record.public.message_id)?;
                let source = if let Some(request_id) = record.public.request_id {
                    self.get_manager_message(request_id)?
                } else {
                    None
                };
                let linked = source.as_ref().unwrap_or(&record);
                let readdressed_from = self.manager_readdress_origin(&config, linked)?;
                let standing_root_id = if record.is_lead_notice() {
                    None
                } else {
                    self.manager_rollover_origin(&config, linked)?
                        .map(|(chain, _)| chain.root)
                };
                record.public.readdressed_from = readdressed_from;
                record.public.standing_root_id = standing_root_id;
                messages.push(record.public);
            }
        }
        self.manager_v2_track_retrieval(&config, caller, &messages)?;
        let (notices, more_notices) = self.retrieve_manager_notices(
            &config,
            caller,
            is_manager,
            request.limit,
            request.request_id,
        )?;
        let manager_seat = self.manager_seat_state(&config)?;
        tx.commit()?;
        Ok(AgentManagerInboxResultV1 {
            messages,
            next_after_sequence: has_more.then_some(last_sequence),
            notices,
            more_notices,
            manager_seat,
        })
    }

    fn manager_message_live(
        &self,
        config: &HarnessManagerConfigV1,
        record: &StoredMessage,
    ) -> Result<bool> {
        if config.project_id != record.project_id
            || config.manager_session_id != record.manager_session_id
            || config.row_version != record.scope_version
            || !config.epic_ids.contains(&record.public.epic_id)
        {
            return Ok(false);
        }
        let Ok(lead) = self.manager_lead(config.project_id, record.public.epic_id) else {
            return Ok(false);
        };
        let manager = self.manager_lineage_tip(config.manager_session_id)?;
        // Direction is fixed at insert time: replies and lead notices flow
        // lead->manager, manager requests manager->lead. Either binding still
        // requires both lineage tips to be the CURRENT lead and manager, so
        // rotation routes and replacement/revocation fail closed alike.
        let lead_origin = record.is_lead_notice();
        if lead_origin && record.public.request_id.is_some() {
            return Ok(false);
        }
        let (sender, target) = if record.public.request_id.is_some() || lead_origin {
            (lead.id, manager)
        } else {
            (manager, lead.id)
        };
        Ok(self
            .manager_lineage_tip(record.public.sender_session_id)
            .ok()
            == Some(sender)
            && self
                .manager_lineage_tip(record.public.recipient_session_id)
                .ok()
                == Some(target))
    }

    pub(crate) fn manager_v2_operator_request_live(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        id: Uuid,
    ) -> Result<bool> {
        let Some(record) = self.get_manager_message(id)? else {
            return Ok(false);
        };
        Ok(record.public.epic_id == epic
            && record.public.request_id.is_none()
            && !record.is_lead_notice()
            && self.manager_message_live(config, &record)?)
    }

    pub(crate) fn manager_request_replied(&self, id: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_messages WHERE request_id=?1)",
            [id.to_string()],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn manager_request_readdressed(
        &self,
        config: &HarnessManagerConfigV1,
        id: Uuid,
    ) -> Result<Option<Uuid>> {
        self.manager_v2_record(config, "request_readdress", &id.to_string())?
            .map(|record| {
                record.payload["new_request_id"]
                    .as_str()
                    .ok_or_else(|| refused("manager_invalid_stored_identity"))
                    .and_then(|value| uuid(value.to_owned()))
            })
            .transpose()
    }

    /// The effective `request_settle` disposition for `id` in the current
    /// scope, if any: a settlement voided by a `request_unsettled` marker for
    /// its row version is no settlement.
    pub(crate) fn manager_request_settlement(
        &self,
        config: &HarnessManagerConfigV1,
        id: Uuid,
    ) -> Result<Option<String>> {
        Ok(self
            .manager_request_settle_record(config, id)?
            .map(|record| {
                record.payload["disposition"]
                    .as_str()
                    .unwrap_or("settled")
                    .to_owned()
            }))
    }

    fn manager_request_settle_record(
        &self,
        config: &HarnessManagerConfigV1,
        id: Uuid,
    ) -> Result<Option<super::harness_manager_v2::ManagerRecordV2>> {
        let key = id.to_string();
        let Some(settle) = self.manager_v2_record(config, REQUEST_SETTLE_KIND, &key)? else {
            return Ok(None);
        };
        let voided = self
            .manager_v2_record(config, REQUEST_UNSETTLED_KIND, &key)?
            .is_some_and(|u| u.payload["settle_row_version"].as_i64() == Some(settle.row_version));
        Ok((!voided).then_some(settle))
    }

    /// Manager ruling (2) on the #664 settled-reply rule: a DAEMON
    /// `lead_replaced` settlement is lifted when the replying caller is again
    /// the Epic's current lead (e.g. the original lead restored). Writes the
    /// `request_unsettled` marker (its own bookkeeping class) and returns
    /// `true`; any other settlement (a manager settle) stays refused.
    fn manager_request_unsettle_for_reply(
        &self,
        config: &HarnessManagerConfigV1,
        caller: Uuid,
        epic: Uuid,
        id: Uuid,
    ) -> Result<bool> {
        let Some(settle) = self.manager_request_settle_record(config, id)? else {
            return Ok(true);
        };
        let daemon_lead_replaced =
            settle.payload["disposition"] == "lead_replaced" && settle.payload["actor"] == "daemon";
        if !daemon_lead_replaced || self.manager_lead(config.project_id, epic)?.id != caller {
            return Ok(false);
        }
        let key = id.to_string();
        let expected = self
            .manager_v2_record(config, REQUEST_UNSETTLED_KIND, &key)?
            .map_or(0, |record| record.row_version);
        self.manager_v2_put_record(
            config,
            REQUEST_UNSETTLED_KIND,
            &key,
            Some(epic),
            expected,
            &serde_json::json!({
                "settle_row_version": settle.row_version,
                "disposition": "lead_replaced",
                "actor": caller,
            }),
        )?;
        Ok(true)
    }

    /// Open manager requests in the current scope: `(project_total, per_epic)`.
    /// One GROUP BY over the shared open predicate; the single source for the
    /// send caps, Progress and Inspect capacity.
    pub(crate) fn manager_open_request_counts(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<(i64, BTreeMap<Uuid, u32>)> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT epic_id,count(*) FROM harness_manager_messages m WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND {} GROUP BY epic_id",
            manager_request_open_sql()
        ))?;
        let rows = statement
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut total = 0;
        let mut per_epic = BTreeMap::new();
        for (epic, count) in rows {
            total += count;
            per_epic.insert(
                uuid(epic)?,
                u32::try_from(count).map_err(|_| refused("manager_invalid_stored_identity"))?,
            );
        }
        Ok((total, per_epic))
    }

    /// #664 (f): capacity summary plus the per-Epic open counts behind it.
    pub(crate) fn manager_mail_capacity(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<(HarnessManagerMailCapacityV1, BTreeMap<Uuid, u32>)> {
        let (total, per_epic) = self.manager_open_request_counts(config)?;
        let project_pending =
            u32::try_from(total).map_err(|_| refused("manager_invalid_stored_identity"))?;
        let near = |pending: u32, limit: u32| u64::from(pending) * 4 >= u64::from(limit) * 3;
        let warning = (near(project_pending, MAX_PENDING_REQUESTS_PER_PROJECT)
            || per_epic
                .values()
                .any(|pending| near(*pending, MAX_PENDING_REQUESTS_PER_EPIC)))
        .then(|| MAIL_CAPACITY_WARNING.to_owned());
        Ok((
            HarnessManagerMailCapacityV1 {
                project_pending,
                project_limit: MAX_PENDING_REQUESTS_PER_PROJECT,
                epic_limit: MAX_PENDING_REQUESTS_PER_EPIC,
                warning,
            },
            per_epic,
        ))
    }

    /// #664 (d): settle open requests addressed to a lead that has since been
    /// REPLACED (not rotated) as `lead_replaced`. Such a request is
    /// unanswerable: the reply guard requires the recipient's lineage tip to be
    /// the current lead. Runs only inside a manager-only Immediate transaction.
    /// - vacant or invalid lead: skipped; a later authorized change readdresses;
    /// - recipient lineage tip is the current lead (rotation): still live;
    /// - otherwise: append a daemon `request_settle` marker.
    ///
    /// The marker is its own unmetered bookkeeping class (one row per request
    /// id, see `bookkeeping_class_limit`), so neither a full coordination
    /// budget nor settled history ever stops the sweep; any write error
    /// propagates rather than silently leaving orphans open.
    pub(crate) fn settle_orphaned_manager_requests(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<usize> {
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM harness_manager_messages m WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND {} ORDER BY sequence LIMIT ?4",
            manager_request_open_sql()
        );
        let mut statement = self.conn.prepare(&query)?;
        let open = statement
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    ORPHAN_SWEEP_LIMIT
                ],
                read_message,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut leads = std::collections::HashMap::new();
        let mut settled = 0;
        for row in open {
            let epic = row.public.epic_id;
            if !config.epic_ids.contains(&epic) {
                continue;
            }
            let lead = *leads.entry(epic).or_insert_with(|| {
                self.manager_lead(config.project_id, epic)
                    .ok()
                    .map(|s| s.id)
            });
            let Some(lead) = lead else {
                continue;
            };
            let recipient = row.public.recipient_session_id;
            if recipient == lead {
                continue;
            }
            let recipient_tip = self.manager_lineage_tip(recipient).ok();
            if recipient_tip == Some(lead) {
                continue;
            }
            self.manager_v2_put_record(
                config,
                REQUEST_SETTLE_KIND,
                &row.public.message_id.to_string(),
                Some(epic),
                0,
                &serde_json::json!({
                    "disposition": "lead_replaced",
                    "actor": "daemon",
                    "recipient_tip": recipient_tip,
                    "current_lead": lead,
                }),
            )?;
            settled += 1;
        }
        Ok(settled)
    }

    fn manager_readdress_origin(
        &self,
        config: &HarnessManagerConfigV1,
        record: &StoredMessage,
    ) -> Result<Option<Uuid>> {
        let Some((old, _)) = record
            .idempotency_key
            .strip_prefix("readdress:")
            .and_then(|suffix| suffix.split_once(':'))
        else {
            return Ok(None);
        };
        let old = Uuid::parse_str(old).map_err(|_| refused("manager_invalid_stored_identity"))?;
        Ok(
            (self.manager_request_readdressed(config, old)? == Some(record.public.message_id))
                .then_some(old),
        )
    }

    pub(crate) fn manager_readdress_origin_id(
        &self,
        config: &HarnessManagerConfigV1,
        id: Uuid,
    ) -> Result<Option<Uuid>> {
        self.get_manager_message(id)?
            .map(|record| self.manager_readdress_origin(config, &record))
            .transpose()
            .map(Option::flatten)
    }

    /// #656: the rollover chain rooted at `root` in the current scope, read
    /// from its single chain record. A root that never rolled over is its own
    /// one-element chain at version 0.
    fn manager_rollover_chain(
        &self,
        config: &HarnessManagerConfigV1,
        root: Uuid,
    ) -> Result<RolloverChain> {
        let Some(record) =
            self.manager_v2_record(config, REQUEST_ROLLOVER_KIND, &root.to_string())?
        else {
            return Ok(RolloverChain {
                root,
                ids: vec![root],
                version: 0,
            });
        };
        let invalid = || refused("manager_invalid_stored_identity");
        let ids = record.payload["generations"]
            .as_array()
            .ok_or_else(invalid)?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(invalid)
                    .and_then(|text| uuid(text.to_owned()))
            })
            .collect::<Result<Vec<_>>>()?;
        let unique = ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len();
        if ids.len() < 2
            || ids.len() > LINEAGE_LIMIT
            || ids[0] != root
            || !unique
            || record.payload["root_request_id"].as_str() != Some(root.to_string().as_str())
        {
            return Err(invalid());
        }
        Ok(RolloverChain {
            root,
            ids,
            version: record.row_version,
        })
    }

    /// #656: `(chain, generation)` when `record` is a rollover successor. The
    /// `rollover:{root}:{generation}` key alone is spoofable; origin requires
    /// the root's chain record to list this message at that generation.
    fn manager_rollover_origin(
        &self,
        config: &HarnessManagerConfigV1,
        record: &StoredMessage,
    ) -> Result<Option<(RolloverChain, usize)>> {
        let Some((root, generation)) = record
            .idempotency_key
            .strip_prefix(ROLLOVER_KEY_PREFIX)
            .and_then(|rest| rest.split_once(':'))
        else {
            return Ok(None);
        };
        let (Ok(root), Ok(generation)) = (Uuid::parse_str(root), generation.parse::<usize>())
        else {
            return Ok(None);
        };
        if generation == 0 {
            return Ok(None);
        }
        let chain = self.manager_rollover_chain(config, root)?;
        Ok(
            (chain.ids.get(generation) == Some(&record.public.message_id))
                .then_some((chain, generation)),
        )
    }

    /// #656: the rollover chain containing `id`, resolved from any generation
    /// in one chain-record read. An id that never rolled over is its own
    /// one-element chain.
    fn manager_request_chain(
        &self,
        config: &HarnessManagerConfigV1,
        id: Uuid,
    ) -> Result<RolloverChain> {
        if let Some(record) = self.get_manager_message(id)?
            && let Some((chain, _)) = self.manager_rollover_origin(config, &record)?
        {
            return Ok(chain);
        }
        self.manager_rollover_chain(config, id)
    }

    /// #656: the generation that follows `id` in its rollover chain, if any.
    fn manager_request_rolled_over(
        &self,
        config: &HarnessManagerConfigV1,
        id: Uuid,
    ) -> Result<Option<Uuid>> {
        let chain = self.manager_request_chain(config, id)?;
        Ok(chain
            .ids
            .iter()
            .position(|generation| *generation == id)
            .and_then(|index| chain.ids.get(index + 1))
            .copied())
    }

    /// Called only inside an authorized lead-change transaction, after its lead CAS.
    /// Raw lead links deliberately do not call this helper.
    pub(crate) fn readdress_open_manager_requests(
        &self,
        epic: Uuid,
        old_lead: Uuid,
        new_lead: Uuid,
    ) -> Result<Vec<Uuid>> {
        if old_lead == new_lead {
            return Ok(Vec::new());
        }
        let Some(epic_session) = self.get_session(epic)? else {
            return Ok(Vec::new());
        };
        let Some(project) = epic_session.project_id else {
            return Ok(Vec::new());
        };
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(Vec::new());
        };
        if config.current_session_id.is_none() || !self.manager_config_covers_epic(&config, epic)? {
            return Ok(Vec::new());
        }
        let lead = self.manager_lead(project, epic)?;
        if lead.id != new_lead {
            return Err(refused("manager_lead_invalid_or_stale"));
        }
        let manager = self.manager_lineage_tip(config.manager_session_id)?;
        let old_tip = self.manager_lineage_tip(old_lead)?;
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM harness_manager_messages m WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND epic_id=?4 AND {}
             ORDER BY sequence",
            manager_request_open_sql()
        );
        let mut statement = self.conn.prepare(&query)?;
        let requests = statement
            .query_map(
                params![
                    project.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    epic.to_string()
                ],
                read_message,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut jobs = Vec::new();
        for original in requests {
            if let Some(job) =
                self.reissue_manager_request(&config, &lead, manager, old_tip, &original)?
            {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    fn reissue_manager_request(
        &self,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        manager: Uuid,
        old_tip: Uuid,
        original: &StoredMessage,
    ) -> Result<Option<Uuid>> {
        let id = original.public.message_id;
        let epic = original.public.epic_id;
        if self.manager_lineage_tip(original.public.recipient_session_id)? != old_tip
            || self.manager_lineage_tip(original.public.sender_session_id)? != manager
            || self.manager_request_replied(id)?
            || self.manager_request_readdressed(config, id)?.is_some()
        {
            return Ok(None);
        }
        if let Some(record) = self.manager_v2_record(config, "request", &id.to_string())?
            && matches!(
                record.payload["state"].as_str(),
                Some("completed" | "failed" | "declined")
            )
        {
            return Ok(None);
        }
        let key = format!("readdress:{id}:{}", lead.id);
        let digest = fingerprint(&AgentManagerSendRequestV1 {
            epic_id: epic,
            message: original.public.message.clone(),
            idempotency_key: key.clone(),
        })?;
        let receipt = if let Some(receipt) = self.manager_replay(manager, config, &key, &digest)? {
            let replayed = self
                .get_manager_message(receipt.message_id)?
                .ok_or_else(|| refused("manager_idempotency_conflict"))?;
            if replayed.public.recipient_session_id != lead.id
                || replayed.public.epic_id != epic
                || replayed.scope_version != config.row_version
            {
                return Err(refused("manager_idempotency_conflict"));
            }
            receipt
        } else {
            self.insert_manager_message(
                config,
                manager,
                lead.id,
                epic,
                None,
                &original.public.message,
                &key,
                &digest,
            )?
        };
        self.manager_v2_put_record(
            config,
            "request_readdress",
            &id.to_string(),
            Some(epic),
            0,
            &serde_json::json!({"new_request_id":receipt.message_id}),
        )?;
        self.ensure_manager_watch(config, lead, false, &receipt.sequence.to_string(), true)?;
        self.record_manager_message_notice(config, lead, &receipt, false)
    }

    fn get_manager_message(&self, id: Uuid) -> Result<Option<StoredMessage>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM harness_manager_messages WHERE id=?1"),
                [id.to_string()],
                read_message,
            )
            .optional()?)
    }

    pub(crate) fn manager_send(
        &self,
        caller: Uuid,
        request: &AgentManagerSendRequestV1,
    ) -> Result<HarnessManagerMessageReceiptV1> {
        validate_manager_message(request.epic_id, &request.message, &request.idempotency_key)
            .map_err(refused)?;
        // #656: rollover successor keys are server-owned; a caller-chosen one
        // could pre-claim a successor and break the chain's replay identity.
        if request.idempotency_key.starts_with(ROLLOVER_KEY_PREFIX) {
            return Err(refused("manager_reserved_idempotency_key"));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        if !is_manager {
            return Err(refused("manager_scope_denied"));
        }
        let digest = fingerprint(request)?;
        if let Some(mut receipt) =
            self.manager_replay(caller, &config, &request.idempotency_key, &digest)?
        {
            receipt.manager_seat = self.manager_seat_state(&config)?;
            tx.commit()?;
            return Ok(receipt);
        }
        if !config.epic_ids.contains(&request.epic_id) {
            return Err(refused("manager_epic_out_of_scope"));
        }
        let lead = self.manager_lead(config.project_id, request.epic_id)?;
        if lead.id == caller {
            return Err(refused("manager_cannot_supervise_itself"));
        }
        self.settle_orphaned_manager_requests(&config)?;
        // #664 (c): both caps read the post-sweep open counts inside this
        // Immediate transaction; the open count grows only through this path.
        let (pending, per_epic) = self.manager_open_request_counts(&config)?;
        if per_epic.get(&request.epic_id).copied().unwrap_or(0) >= MAX_PENDING_REQUESTS_PER_EPIC {
            return Err(refused(
                "manager_epic_pending_request_limit: next_action=settle_or_send_notice",
            ));
        }
        if pending >= i64::from(MAX_PENDING_REQUESTS_PER_PROJECT) {
            return Err(refused("manager_pending_request_limit"));
        }
        let mut receipt = self.insert_manager_message(
            &config,
            caller,
            lead.id,
            request.epic_id,
            None,
            &request.message,
            &request.idempotency_key,
            &digest,
        )?;
        self.ensure_manager_watch(&config, &lead, false, &receipt.sequence.to_string(), true)?;
        self.record_manager_message_notice(&config, &lead, &receipt, false)?;
        receipt.manager_seat = self.manager_seat_state(&config)?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn manager_reply(
        &self,
        caller: Uuid,
        request: &AgentManagerReplyRequestV1,
    ) -> Result<HarnessManagerMessageReceiptV1> {
        validate_manager_message(
            request.request_id,
            &request.message,
            &request.idempotency_key,
        )
        .map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        if is_manager {
            return Err(refused("manager_reply_requires_feature_lead"));
        }
        let original = self
            .get_manager_message(request.request_id)?
            .ok_or_else(|| refused("manager_request_not_in_scope"))?;
        if original.public.request_id.is_none()
            && Some(original.public.epic_id) == self.manager_session(caller)?.parent_id
            && let Some(next) = self.manager_request_readdressed(&config, request.request_id)?
        {
            return Err(DaemonError::InvalidParam(format!(
                "manager_request_readdressed: next_action=reply_to_request:{next}"
            )));
        }
        if original.public.request_id.is_some()
            || original.is_lead_notice()
            || !self.manager_message_live(&config, &original)?
            || self.manager_lineage_tip(original.public.recipient_session_id)? != caller
        {
            return Err(refused("manager_request_not_in_scope"));
        }
        // #656 chain-wide replay, resolved before any insert or rollover. The
        // digest is root-canonical: for a request that never rolled over the
        // root is the named id, so it is byte-identical to the pre-#656 digest.
        let chain = self.manager_request_chain(&config, request.request_id)?;
        let digest = fingerprint(&AgentManagerReplyRequestV1 {
            request_id: chain.root,
            message: request.message.clone(),
            idempotency_key: request.idempotency_key.clone(),
        })?;
        if let Some(mut receipt) = self.manager_reply_replay(
            caller,
            &config,
            original.public.epic_id,
            &chain,
            &request.idempotency_key,
            &digest,
        )? {
            receipt.manager_seat = self.manager_seat_state(&config)?;
            tx.commit()?;
            return Ok(receipt);
        }
        // #664 settled-reply rule: an exact replay above still returns its
        // original receipt; any NEW reply to a manager-settled request is
        // refused. A daemon `lead_replaced` settlement is lifted when the
        // caller is again the current lead: the request is unsettled and the
        // reply below lands in the same transaction. A lead-terminal
        // `request_released` marker does not refuse, so a lead can still
        // explain a decline or failure.
        if !self.manager_request_unsettle_for_reply(
            &config,
            caller,
            original.public.epic_id,
            request.request_id,
        )? {
            return Err(refused(
                "manager_request_settled: next_action=send_notice_or_reply_to_standing",
            ));
        }
        let target = self.manager_lineage_tip(config.manager_session_id)?;
        // #656: store on the first generation (from the named one) with room;
        // earlier generations are full by construction. A full tip rolls over.
        let named = chain
            .ids
            .iter()
            .position(|id| *id == request.request_id)
            .unwrap_or(0);
        let mut generation = None;
        for id in &chain.ids[named..] {
            let replies: i64 = self.conn.query_row(
                "SELECT count(*) FROM harness_manager_messages WHERE request_id=?1",
                [id.to_string()],
                |row| row.get(0),
            )?;
            if replies < MAX_REPLIES_PER_REQUEST {
                generation = Some(*id);
                break;
            }
        }
        let generation = match generation {
            Some(id) => id,
            None => self.roll_over_manager_request(&config, &chain, &original, target, caller)?,
        };
        let mut receipt = self.insert_manager_message(
            &config,
            caller,
            target,
            original.public.epic_id,
            Some(generation),
            &request.message,
            &request.idempotency_key,
            &digest,
        )?;
        receipt.rolled_over_from = (generation != chain.root).then_some(chain.root);
        let lead = self.manager_lead(config.project_id, original.public.epic_id)?;
        self.ensure_manager_watch(&config, &lead, true, &lead.updated_at.to_rfc3339(), true)?;
        self.record_manager_message_notice(&config, &lead, &receipt, true)?;
        // Mail stays durably queued; the lead learns the seat is down (#669).
        receipt.manager_seat = self.manager_seat_state(&config)?;
        tx.commit()?;
        Ok(receipt)
    }

    /// #656: append the next generation of a standing request whose tip is
    /// full. The successor carries the request text verbatim under the
    /// server-owned key `rollover:{root}:{generation}`, sender = manager
    /// lineage tip and recipient = the replying lead (the readdress
    /// precedent). It is replied in the same transaction, so it never counts
    /// as pending, and it records no `to_lead` wake because the rollover is
    /// the lead's own action. The root's chain record is CAS-extended from the
    /// version read with the chain, so concurrent rollovers serialize on one
    /// successor.
    fn roll_over_manager_request(
        &self,
        config: &HarnessManagerConfigV1,
        chain: &RolloverChain,
        original: &StoredMessage,
        manager: Uuid,
        lead: Uuid,
    ) -> Result<Uuid> {
        if chain.ids.len() >= LINEAGE_LIMIT {
            return Err(refused("manager_request_rollover_limit"));
        }
        let epic = original.public.epic_id;
        let key = format!("{ROLLOVER_KEY_PREFIX}{}:{}", chain.root, chain.ids.len());
        let digest = fingerprint(&AgentManagerSendRequestV1 {
            epic_id: epic,
            message: original.public.message.clone(),
            idempotency_key: key.clone(),
        })?;
        let successor = self.insert_manager_message(
            config,
            manager,
            lead,
            epic,
            None,
            &original.public.message,
            &key,
            &digest,
        )?;
        let generations = chain
            .ids
            .iter()
            .chain(std::iter::once(&successor.message_id))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        self.manager_v2_put_record(
            config,
            REQUEST_ROLLOVER_KIND,
            &chain.root.to_string(),
            Some(epic),
            chain.version,
            &serde_json::json!({
                "root_request_id": chain.root,
                "generations": generations,
            }),
        )?;
        Ok(successor.message_id)
    }

    /// #656 chain-wide reply replay: keyed on (project, manager, scope, Epic,
    /// key, `request_id IN chain`), independent of the sender, so a retry by a
    /// rotated lead or naming any generation finds the original reply. The
    /// `sender=caller` branch still catches key reuse across reply links.
    fn manager_reply_replay(
        &self,
        caller: Uuid,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        chain: &RolloverChain,
        key: &str,
        digest: &str,
    ) -> Result<Option<HarnessManagerMessageReceiptV1>> {
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM harness_manager_messages WHERE idempotency_key=?1
            AND (sender_session_id=?2 OR (project_id=?3 AND manager_session_id=?4 AND scope_version=?5
              AND epic_id=?6 AND request_id IN (SELECT value FROM json_each(?7))))
            ORDER BY sequence LIMIT 2"
        );
        let mut statement = self.conn.prepare(&query)?;
        let found = statement
            .query_map(
                params![
                    key,
                    caller.to_string(),
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    epic.to_string(),
                    chain.json()
                ],
                read_message,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if found.len() > 1 {
            return Err(refused("manager_idempotency_conflict"));
        }
        let Some(record) = found.into_iter().next() else {
            return Ok(None);
        };
        if record.request_fingerprint != digest {
            return Err(refused("manager_idempotency_conflict"));
        }
        let generation = record.public.request_id;
        Ok(Some(HarnessManagerMessageReceiptV1 {
            message_id: record.public.message_id,
            sequence: record.public.sequence,
            request_id: generation,
            deduplicated: true,
            rolled_over_from: generation
                .filter(|id| *id != chain.root && chain.ids.contains(id))
                .map(|_| chain.root),
            // The caller stamps the live seat observation (#669).
            manager_seat: None,
        }))
    }

    /// #404 S1: one durable, unsolicited informational notice from the
    /// current legal Epic lead to the current appointed manager whose live
    /// scope covers that Epic. Caller, Epic, project, manager, scope version
    /// and recipient lineage are all server-derived; only bounded content and
    /// an idempotency key are accepted. The notice is mail, never a request,
    /// reply target, approval, acceptance or lifecycle authority.
    pub(crate) fn manager_lead_notice(
        &self,
        caller: Uuid,
        message: &str,
        idempotency_key: &str,
    ) -> Result<HarnessManagerMessageReceiptV1> {
        validate_manager_message(caller, message, idempotency_key).map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // Refuses non-leads, stale/replaced leads, out-of-scope Epics, revoked
        // scope and absent/retired managers before anything is read or written.
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        if is_manager {
            return Err(refused("manager_notice_requires_feature_lead"));
        }
        let epic = self
            .manager_session(caller)?
            .parent_id
            .ok_or_else(|| refused("manager_scope_denied"))?;
        let lead = self.manager_lead(config.project_id, epic)?;
        if lead.id != caller {
            return Err(refused("manager_scope_denied"));
        }
        let digest = format!(
            "{LEAD_NOTICE_FINGERPRINT_PREFIX}{}",
            fingerprint(&serde_json::json!({
                "message": message,
                "idempotency_key": idempotency_key,
            }))?
        );
        // Exact replay is per (sender, key), matching the table's UNIQUE key.
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM harness_manager_messages
             WHERE sender_session_id=?1 AND idempotency_key=?2"
        );
        if let Some(record) = self
            .conn
            .query_row(
                &query,
                params![caller.to_string(), idempotency_key],
                read_message,
            )
            .optional()?
        {
            if record.request_fingerprint != digest
                || record.project_id != config.project_id
                || record.manager_session_id != config.manager_session_id
                || record.scope_version != config.row_version
                || record.public.epic_id != epic
            {
                return Err(refused("manager_idempotency_conflict"));
            }
            tx.commit()?;
            return Ok(HarnessManagerMessageReceiptV1 {
                message_id: record.public.message_id,
                sequence: record.public.sequence,
                request_id: None,
                deduplicated: true,
                rolled_over_from: None,
                manager_seat: None,
            });
        }
        let manager = config
            .current_session_id
            .ok_or_else(|| refused("manager_current_session_required"))?;
        if manager == caller {
            return Err(refused("manager_cannot_supervise_itself"));
        }
        let pending: i64 = self.conn.query_row(
            "SELECT count(*) FROM harness_manager_notices
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND epic_id=?4
               AND direction='to_manager' AND kind='message'
               AND retired_at IS NULL AND settled_at IS NULL",
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                epic.to_string()
            ],
            |row| row.get(0),
        )?;
        if pending >= MAX_PENDING_LEAD_NOTICES {
            return Err(refused("manager_pending_notice_limit"));
        }
        let receipt = self.insert_manager_message(
            &config,
            caller,
            manager,
            epic,
            None,
            message,
            idempotency_key,
            &digest,
        )?;
        self.ensure_manager_watch(&config, &lead, true, &receipt.sequence.to_string(), true)?;
        self.record_manager_message_notice(&config, &lead, &receipt, true)?;
        tx.commit()?;
        Ok(receipt)
    }

    fn manager_replay(
        &self,
        caller: Uuid,
        config: &HarnessManagerConfigV1,
        key: &str,
        digest: &str,
    ) -> Result<Option<HarnessManagerMessageReceiptV1>> {
        // Manager-origin requests: the same logical manager survives rotation.
        // Actual sender remains immutable attribution. Lead replies replay
        // chain-wide through `manager_reply_replay` (#656).
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM harness_manager_messages WHERE idempotency_key=?1
            AND (sender_session_id=?2 OR (project_id=?3 AND manager_session_id=?4 AND
              {MANAGER_REQUEST_ROW})) ORDER BY sequence LIMIT 2"
        );
        let mut statement = self.conn.prepare(&query)?;
        let found = statement
            .query_map(
                params![
                    key,
                    caller.to_string(),
                    config.project_id.to_string(),
                    config.manager_session_id.to_string()
                ],
                read_message,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if found.len() > 1 {
            return Err(refused("manager_idempotency_conflict"));
        }
        let Some(record) = found.into_iter().next() else {
            return Ok(None);
        };
        if record.request_fingerprint != digest {
            return Err(refused("manager_idempotency_conflict"));
        }
        Ok(Some(HarnessManagerMessageReceiptV1 {
            message_id: record.public.message_id,
            sequence: record.public.sequence,
            request_id: record.public.request_id,
            deduplicated: true,
            rolled_over_from: None,
            manager_seat: None,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_manager_message(
        &self,
        config: &HarnessManagerConfigV1,
        sender: Uuid,
        target: Uuid,
        epic: Uuid,
        request_id: Option<Uuid>,
        message: &str,
        key: &str,
        digest: &str,
    ) -> Result<HarnessManagerMessageReceiptV1> {
        let id = Uuid::new_v4();
        self.conn.execute("INSERT INTO harness_manager_messages(id,project_id,manager_session_id,epic_id,scope_version,
            sender_session_id,recipient_session_id,request_id,idempotency_key,request_fingerprint,message,created_at)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![id.to_string(), config.project_id.to_string(), config.manager_session_id.to_string(), epic.to_string(), config.row_version,
                sender.to_string(), target.to_string(), request_id.map(|id| id.to_string()), key, digest, message, stamp()])?;
        Ok(HarnessManagerMessageReceiptV1 {
            message_id: id,
            sequence: self.conn.last_insert_rowid(),
            request_id,
            deduplicated: false,
            rolled_over_from: None,
            manager_seat: None,
        })
    }

    pub(crate) fn manager_watch_identity(
        &self,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        to_manager: bool,
    ) -> Result<(Uuid, Uuid, Uuid, &'static str)> {
        let lead_root = self.manager_lineage_root(lead.id)?;
        let (source, target, direction) = if to_manager {
            (lead_root, config.manager_session_id, "to_manager")
        } else {
            (config.manager_session_id, lead_root, "to_lead")
        };
        let key = format!(
            "harness-manager:v1:{}:{}:{}:{direction}:{source}:{target}",
            config.project_id,
            config.row_version,
            lead.parent_id
                .ok_or_else(|| refused("manager_legal_epic_required"))?
        );
        Ok((
            Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes()),
            source,
            target,
            direction,
        ))
    }

    /// Project-wide manager action results use one exact durable transport.
    /// Its source and target are the manager lineage anchor: the scheduler
    /// resolves both to the current tip and fires only after that tip is idle.
    /// This covers topology, session-creation, vacant-lead, and succession
    /// actions that cannot truthfully borrow an arbitrary Epic lead as source.
    pub(crate) fn manager_action_watch_identity(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> (Uuid, Uuid, Uuid) {
        let source = config.manager_session_id;
        let target = config.manager_session_id;
        let key = format!(
            "harness-manager:v2:{}:{}:manager-action:{source}:{target}",
            config.project_id, config.row_version
        );
        (
            Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes()),
            source,
            target,
        )
    }

    pub(crate) fn ensure_manager_action_watch(
        &self,
        config: &HarnessManagerConfigV1,
        signature: &str,
    ) -> Result<()> {
        let (job_id, source, target) = self.manager_action_watch_identity(config);
        let previous: Option<String> = self
            .conn
            .query_row(
                "SELECT attention_signature FROM harness_manager_watches
                 WHERE job_id=?1 AND route_kind='manager_action'",
                [job_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if previous.as_deref() == Some(signature) {
            return Ok(());
        }
        let scheduled = self.get_scheduled_job(&job_id)?;
        if let Some(job) = &scheduled
            && (job.project_id != Some(config.project_id)
                || job.wake_mode != WakeMode::OnTerminal(source)
                || job.wake_session_id != Some(target))
        {
            return Err(refused("manager_action_watch_identity_collision"));
        }
        let now = Utc::now();
        let can_enable = if scheduled.as_ref().is_some_and(|job| job.enabled) {
            true
        } else {
            let count: i64 = self.conn.query_row(
                "SELECT count(*) FROM scheduled_jobs
                 WHERE enabled=1 AND wake_session_id=?1
                   AND wake_mode LIKE 'on_terminal:%'",
                [target.to_string()],
                |row| row.get(0),
            )?;
            count < crate::session::agent_verbs::MAX_TERMINAL_WATCHES_PER_MASTER as i64
        };
        if previous.is_none() {
            if scheduled.is_none() && !can_enable {
                return Ok(());
            }
            if scheduled.is_none() {
                let job = ScheduledJob {
                    id: job_id,
                    name: "Manager action results".into(),
                    message: "Harness manager action notice: retrieve AgentManagerInbox to settle exact durable action results, then inspect the named operation receipt. This notice is not human approval.".into(),
                    schedule: ScheduleSpec {
                        recurrence: Recurrence::EverySeconds(60),
                        anchor: now,
                    },
                    last_fired_at: None,
                    next_fire_at: now,
                    enabled: can_enable,
                    working_dir: None,
                    provider: None,
                    model: None,
                    project_id: Some(config.project_id),
                    created_at: now,
                    updated_at: now,
                    wake_mode: WakeMode::OnTerminal(source),
                    wake_session_id: Some(target),
                };
                super::scheduled_jobs::insert_harness_manager_watch_job_conn(&self.conn, &job)?;
            }
            // `epic_id` predates project-wide notices and remains NOT NULL in
            // the released V102 table. `route_kind` makes this carrier value
            // unambiguous; public action notices carry their truthful optional
            // affected Epic separately.
            self.conn.execute(
                "INSERT INTO harness_manager_watches(
                    job_id,project_id,epic_id,scope_version,direction,
                    source_session_id,target_session_id,attention_signature,route_kind)
                 VALUES(?1,?2,?3,?4,'to_manager',?3,?3,?5,'manager_action')",
                params![
                    job_id.to_string(),
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    signature
                ],
            )?;
            let binding: Option<(String, String, i64, String, String, String)> = self
                .conn
                .query_row(
                    "SELECT project_id,epic_id,scope_version,direction,
                            source_session_id,target_session_id
                     FROM harness_manager_watches
                     WHERE job_id=?1 AND route_kind='manager_action'",
                    [job_id.to_string()],
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
            if binding
                != Some((
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    "to_manager".into(),
                    source.to_string(),
                    target.to_string(),
                ))
            {
                return Err(refused("manager_action_watch_identity_collision"));
            }
            if scheduled.is_some() && can_enable {
                self.conn.execute(
                    "UPDATE scheduled_jobs SET enabled=1,last_fired_at=NULL,
                     next_fire_at=?2,updated_at=?2 WHERE id=?1",
                    params![job_id.to_string(), stamp()],
                )?;
            }
        } else {
            if scheduled.is_none() {
                return Err(refused("manager_action_watch_transport_missing"));
            }
            if can_enable {
                self.conn.execute(
                    "UPDATE scheduled_jobs SET enabled=1,last_fired_at=NULL,
                     next_fire_at=?2,updated_at=?2 WHERE id=?1",
                    params![job_id.to_string(), stamp()],
                )?;
            }
            self.conn.execute(
                "UPDATE harness_manager_watches SET attention_signature=?2
                 WHERE job_id=?1 AND route_kind='manager_action'",
                params![job_id.to_string(), signature],
            )?;
        }
        Ok(())
    }

    pub(crate) fn ensure_manager_watch(
        &self,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        to_manager: bool,
        signature: &str,
        force: bool,
    ) -> Result<()> {
        let semantic_signature = self.manager_v2_notice_signature(config, lead, to_manager)?;
        let signature = semantic_signature.as_deref().unwrap_or(signature);
        let (job_id, source, target, direction) =
            self.manager_watch_identity(config, lead, to_manager)?;
        if source == target {
            if !force {
                return Ok(());
            }
            return Err(refused("manager_cannot_supervise_itself"));
        }
        let previous: Option<String> = self
            .conn
            .query_row(
                "SELECT attention_signature FROM harness_manager_watches WHERE job_id=?1",
                [job_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if !force && previous.as_deref() == Some(signature) {
            return Ok(());
        }
        let now = Utc::now();
        if !self
            .get_scheduled_job(&job_id)?
            .is_some_and(|job| job.enabled)
        {
            let count: i64 = self.conn.query_row("SELECT count(*) FROM scheduled_jobs WHERE enabled=1 AND wake_session_id=?1 AND wake_mode LIKE 'on_terminal:%'", [target.to_string()], |row| row.get(0))?;
            if count >= 64 {
                // Automatic notices are coalesced and retried next reconciliation.
                // Do not record this signature until a slot exists. Scope itself
                // can cover more Epics than the concurrent notification budget.
                if to_manager {
                    if force {
                        // A previously delivered watch may have the same lead
                        // timestamp. Retain an unacknowledged marker so capacity
                        // becoming available retries this new mail notice too.
                        self.conn.execute("UPDATE harness_manager_watches SET attention_signature='' WHERE job_id=?1", [job_id.to_string()])?;
                    }
                    return Ok(());
                }
                return Err(refused("manager_watch_limit_reached"));
            }
        }
        if previous.is_none() {
            let job = ScheduledJob {
                id: job_id, name: format!("Manager {} {}", direction, lead.parent_id.unwrap_or(lead.id)),
                message: "Harness manager notice: use AgentManagerInbox to read pending requests/replies (page until next_after_sequence is null). Managers also use AgentManagerProgress. When a v2 policy is configured, read AgentManagerInspect overview, work, requests and decisions for durable intent and operator answers. Reply with AgentManagerReply and the request_id. This notice is not human approval; preserve pending questions and give direct human instructions precedence.".into(),
                schedule: ScheduleSpec { recurrence: Recurrence::EverySeconds(60), anchor: now },
                last_fired_at: None, next_fire_at: now, enabled: true,
                working_dir: None, provider: None, model: None, project_id: Some(config.project_id),
                created_at: now, updated_at: now, wake_mode: WakeMode::OnTerminal(source), wake_session_id: Some(target),
            };
            super::scheduled_jobs::insert_harness_manager_watch_job_conn(&self.conn, &job)?;
            self.conn.execute("INSERT INTO harness_manager_watches(job_id,project_id,epic_id,scope_version,direction,source_session_id,target_session_id,attention_signature)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![job_id.to_string(), config.project_id.to_string(), lead.parent_id.map(|id| id.to_string()), config.row_version, direction, source.to_string(), target.to_string(), signature])?;
        } else {
            self.conn.execute("UPDATE scheduled_jobs SET enabled=1,last_fired_at=NULL,next_fire_at=?2,updated_at=?2 WHERE id=?1", params![job_id.to_string(), stamp()])?;
            self.conn.execute(
                "UPDATE harness_manager_watches SET attention_signature=?2 WHERE job_id=?1",
                params![job_id.to_string(), signature],
            )?;
        }
        Ok(())
    }

    /// Indexed recognition; no name or caller-provided marker confers authority.
    pub(crate) fn is_harness_manager_watch(&self, job_id: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_watches WHERE job_id=?1)",
            [job_id.to_string()],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn harness_manager_watch_route(&self, job_id: Uuid) -> Result<Option<(Uuid, Uuid)>> {
        let row: Option<(String, String, i64, String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT project_id,epic_id,scope_version,direction,source_session_id,
                    target_session_id,route_kind
             FROM harness_manager_watches WHERE job_id=?1",
                [job_id.to_string()],
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
        let Some((project, epic, version, direction, source, target, route_kind)) = row else {
            return Ok(None);
        };
        let project = uuid(project)?;
        let epic = uuid(epic)?;
        let source = uuid(source)?;
        let target = uuid(target)?;
        let Some(job) = self.get_scheduled_job(&job_id)? else {
            return Ok(None);
        };
        if job.wake_mode != WakeMode::OnTerminal(source)
            || job.wake_session_id != Some(target)
            || job.project_id != Some(project)
            || job.working_dir.is_some()
            || job.provider.is_some()
            || job.model.is_some()
        {
            return Ok(None);
        }
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(None);
        };
        if config.row_version != version {
            return Ok(None);
        }
        let Some(manager) = config.current_session_id else {
            return Ok(None);
        };
        if !self.manager_session(manager).is_ok_and(|session| {
            !matches!(
                session.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        }) {
            return Ok(None);
        }
        if route_kind == "manager_action" {
            if direction != "to_manager"
                || epic != config.manager_session_id
                || source != config.manager_session_id
                || target != config.manager_session_id
                || self.manager_lineage_tip(source).ok() != Some(manager)
                || self.manager_lineage_tip(target).ok() != Some(manager)
            {
                return Ok(None);
            }
            return Ok(Some((manager, manager)));
        }
        if route_kind != "epic" || !self.manager_config_covers_epic(&config, epic)? {
            return Ok(None);
        }
        let lead = match self.manager_lead(project, epic) {
            Ok(lead) => lead,
            Err(_) if direction == "to_manager" => {
                let retained_source: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT source_session_id FROM harness_manager_notices
                         WHERE job_id=?1 AND direction='to_manager'
                           AND kind='session_state' AND retired_at IS NULL
                           AND settled_at IS NULL AND source_session_id IS NOT NULL
                         ORDER BY sequence DESC LIMIT 1",
                        [job_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(retained_source) = retained_source else {
                    return Ok(None);
                };
                let retained_source = uuid(retained_source)?;
                let Some(retained) = self.get_session(retained_source)? else {
                    return Ok(None);
                };
                if retained.parent_id != Some(epic)
                    || !matches!(
                        retained.status,
                        SessionStatus::Completed
                            | SessionStatus::Failed
                            | SessionStatus::Interrupted
                            | SessionStatus::WaitingApproval
                            | SessionStatus::Archived
                    )
                    || self.manager_lineage_root(retained.id).ok() != Some(source)
                {
                    return Ok(None);
                }
                retained
            }
            Err(_) => return Ok(None),
        };
        let (expected_source, expected_target) = if direction == "to_manager" {
            (lead.id, manager)
        } else {
            (manager, lead.id)
        };
        if self.manager_lineage_tip(source).ok() != Some(expected_source)
            || self.manager_lineage_tip(target).ok() != Some(expected_target)
        {
            return Ok(None);
        }
        Ok(Some((expected_source, expected_target)))
    }

    pub(crate) fn harness_manager_wake_authorized(
        &self,
        job_id: Uuid,
        target_session_id: Uuid,
    ) -> Result<bool> {
        let enabled = self
            .get_scheduled_job(&job_id)?
            .is_some_and(|job| job.enabled);
        Ok(enabled
            && self
                .harness_manager_watch_route(job_id)?
                .is_some_and(|(_, target)| target == target_session_id))
    }

    /// Durable reconciliation, also accelerated by session bus events. A
    /// changed source fingerprint re-arms one notice, never one job per event.
    pub(crate) fn reconcile_harness_manager_watches(&self) -> Result<()> {
        let retirement_tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.reconcile_manager_notice_scope_retirements()?;
        retirement_tx.commit()?;
        let mut statement = self.conn.prepare(
            "SELECT project_id FROM harness_manager_scopes ORDER BY project_id LIMIT ?1",
        )?;
        let projects = statement
            .query_map([MAX_ACTIVE_MANAGERS as i64 + 1], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        if projects.len() > MAX_ACTIVE_MANAGERS {
            return Err(refused("manager_project_limit_reached"));
        }
        let mut first_error = None;
        for project in projects {
            let project = uuid(project)?;
            let result = (|| -> Result<()> {
                let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
                let Some(config) = self.get_harness_manager_notice_config(project)? else {
                    return Ok(());
                };
                if config.current_session_id.is_some() {
                    for epic in self.manager_notice_epic_page(&config, "v1_epics")? {
                        let Ok(lead) = self.manager_lead(project, epic) else {
                            continue;
                        };
                        // One fingerprint per actual terminal/question state;
                        // active token updates never re-arm acknowledged rows.
                        if matches!(
                            lead.status,
                            SessionStatus::Completed
                                | SessionStatus::Failed
                                | SessionStatus::Interrupted
                                | SessionStatus::WaitingApproval
                        ) || lead.pending_question.is_some()
                        {
                            self.record_manager_session_notice(&config, &lead)?;
                        }
                    }
                    self.reconcile_deferred_manager_notice_transports(&config)?;
                }
                tx.commit()?;
                Ok(())
            })();
            if let Err(error) = result {
                tracing::warn!(project_id=%project,error=%error,
                    "manager notice project reconciliation deferred");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod lead_notice_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use rsi_common::types::{PendingQuestion, Project, QuestionItem};
    use std::path::PathBuf;

    struct Fixture {
        store: Store,
        project: Uuid,
        manager: Uuid,
        epics: [Uuid; 2],
        leads: [Uuid; 2],
        config: HarnessManagerConfigV1,
    }

    fn fixture(store: Store) -> Fixture {
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "Manager pilot".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/tmp/manager-pilot"));
        manager.session_kind = SessionKind::Standard;
        manager.project_id = Some(project);
        manager.status = SessionStatus::Completed;
        store.insert_session(&manager).unwrap();
        let mut group = manager.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        group.title = Some("Pilot features".into());
        store.insert_session(&group).unwrap();
        let epics = [Uuid::new_v4(), Uuid::new_v4()];
        let leads = [Uuid::new_v4(), Uuid::new_v4()];
        for index in 0..2 {
            let mut epic = group.clone();
            epic.id = epics[index];
            epic.session_kind = SessionKind::Epic;
            epic.parent_id = Some(group.id);
            epic.title = Some(format!("Feature {index}"));
            store.insert_session(&epic).unwrap();
            let mut lead = manager.clone();
            lead.id = leads[index];
            lead.session_kind = SessionKind::Feature;
            lead.parent_id = Some(epic.id);
            lead.title = Some(format!("Demiurge {index}"));
            store.insert_session(&lead).unwrap();
            store.set_lead_session(epic.id, Some(lead.id)).unwrap();
        }
        let config = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager.id,
                epic_ids: Some(epics.to_vec()),
                expected_row_version: 0,
            })
            .unwrap();
        Fixture {
            store,
            project,
            manager: manager.id,
            epics,
            leads,
            config,
        }
    }

    fn request(epic_id: Uuid, key: &str) -> AgentManagerSendRequestV1 {
        AgentManagerSendRequestV1 {
            epic_id,
            message: "Report readiness with verification evidence.".into(),
            idempotency_key: key.into(),
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn identical_scope_resave_preserves_policy_watches_and_retirement_state() {
        use rsi_common::harness_manager_v2::{
            ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerPolicyV2,
        };

        let f = fixture(Store::open_in_memory().unwrap());
        f.store.reconcile_harness_manager_watches().unwrap();
        let watch_state = || {
            f.store
                .list_scheduled_jobs()
                .unwrap()
                .into_iter()
                .filter(|job| job.wake_session_id == Some(f.manager))
                .map(|job| (job.id, job.enabled))
                .collect::<Vec<_>>()
        };
        let watches_before = watch_state();
        assert!(!watches_before.is_empty());
        let policy = f
            .store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: f.project,
                expected_scope_version: f.config.row_version,
                expected_policy_version: 0,
                idempotency_key: "scope-resave-policy".into(),
                policy: ManagerPolicyV2 {
                    capabilities: vec![ManagerCapabilityV2::WorkPlan],
                    ..Default::default()
                },
            })
            .unwrap();
        let retirement_count = || {
            f.store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_notice_scope_retirements",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        let before_retirements = retirement_count();
        let same = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(f.epics.to_vec()),
                group_ids: Vec::new(),
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        assert_eq!(same.row_version, f.config.row_version);
        assert_eq!(same.updated_at, f.config.updated_at);
        assert_eq!(watch_state(), watches_before);
        assert_eq!(retirement_count(), before_retirements);
        let live_policy = f
            .store
            .get_harness_manager_policy(f.project)
            .unwrap()
            .unwrap();
        assert_eq!(live_policy.row_version, policy.row_version);
        assert!(!live_policy.revoked);

        let changed = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![f.epics[0]]),
                group_ids: Vec::new(),
                expected_row_version: same.row_version,
            })
            .unwrap();
        assert_eq!(changed.row_version, same.row_version + 1);
        assert!(
            f.store
                .get_harness_manager_policy(f.project)
                .unwrap()
                .unwrap()
                .revoked
        );
        assert!(retirement_count() > before_retirements);
        let changed_watches = watch_state();
        for (id, was_enabled) in watches_before {
            if was_enabled {
                assert!(
                    !changed_watches
                        .iter()
                        .find(|(changed_id, _)| *changed_id == id)
                        .unwrap()
                        .1
                );
            }
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn identical_scope_resave_canonicalizes_modes_ids_and_lineage_tip() {
        let f = fixture(Store::open_in_memory().unwrap());
        let assert_noop = |request: ConfigureHarnessManagerRequestV1,
                           expected: &HarnessManagerConfigV1| {
            let same = f.store.configure_harness_manager(&request).unwrap();
            assert_eq!(same.row_version, expected.row_version);
            assert_eq!(same.updated_at, expected.updated_at);
            same
        };

        let project = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: None,
                group_ids: Vec::new(),
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        assert_eq!(project.scope_mode, HarnessManagerScopeModeV1::Project);
        assert_noop(
            ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: None,
                group_ids: Vec::new(),
                expected_row_version: project.row_version,
            },
            &project,
        );

        let first_group = add_group(&f, "First selected group");
        let second_group = add_group(&f, "Second selected group");
        let (extra_epic, _) = add_epic(&f, second_group.id, "Mixed-scope feature");
        let group_only = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(Vec::new()),
                group_ids: vec![second_group.id, first_group.id],
                expected_row_version: project.row_version,
            })
            .unwrap();
        assert_eq!(group_only.scope_mode, HarnessManagerScopeModeV1::Selected);
        assert_noop(
            ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(Vec::new()),
                group_ids: vec![first_group.id, second_group.id],
                expected_row_version: group_only.row_version,
            },
            &group_only,
        );

        let mixed = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![extra_epic, f.epics[1], f.epics[0]]),
                group_ids: vec![second_group.id, first_group.id],
                expected_row_version: group_only.row_version,
            })
            .unwrap();
        assert_noop(
            ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![f.epics[0], f.epics[1], extra_epic]),
                group_ids: vec![first_group.id, second_group.id],
                expected_row_version: mixed.row_version,
            },
            &mixed,
        );

        let stale = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![extra_epic, f.epics[1], f.epics[0]]),
                group_ids: vec![second_group.id, first_group.id],
                expected_row_version: mixed.row_version - 1,
            })
            .unwrap_err();
        assert!(format!("{stale:#}").contains("manager_stale_scope_refresh_required"));
        let after_stale = f.store.get_harness_manager(f.project).unwrap().unwrap();
        assert_eq!(after_stale.row_version, mixed.row_version);
        assert_eq!(after_stale.updated_at, mixed.updated_at);
    }

    fn add_group(f: &Fixture, title: &str) -> Session {
        let template = f.store.get_session(f.epics[0]).unwrap().unwrap();
        let mut group = f
            .store
            .get_session(template.parent_id.unwrap())
            .unwrap()
            .unwrap();
        group.id = Uuid::new_v4();
        group.title = Some(title.into());
        f.store.insert_session(&group).unwrap();
        group
    }

    fn add_epic(f: &Fixture, group: Uuid, title: &str) -> (Uuid, Uuid) {
        let mut epic = f.store.get_session(f.epics[0]).unwrap().unwrap();
        epic.id = Uuid::new_v4();
        epic.parent_id = Some(group);
        epic.title = Some(title.into());
        epic.lead_session_id = None;
        f.store.insert_session(&epic).unwrap();
        let mut lead = f.store.get_session(f.leads[0]).unwrap().unwrap();
        lead.id = Uuid::new_v4();
        lead.parent_id = Some(epic.id);
        f.store.insert_session(&lead).unwrap();
        f.store.set_lead_session(epic.id, Some(lead.id)).unwrap();
        (epic.id, lead.id)
    }

    #[test]
    fn harness_manager_groups_include_future_epics_and_union_explicit_selection() {
        let f = fixture(Store::open_in_memory().unwrap());
        let existing_group = f
            .store
            .get_session(f.epics[0])
            .unwrap()
            .unwrap()
            .parent_id
            .unwrap();
        let empty = add_group(&f, "Future platform features");
        let other = add_group(&f, "Other features");
        let (explicit, _) = add_epic(&f, other.id, "Chosen separately");
        let grant = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![explicit]),
                group_ids: vec![existing_group, empty.id],
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        assert_eq!(grant.epic_ids.len(), 3);
        assert_eq!(grant.explicit_epic_ids(), &[explicit]);
        assert_eq!(grant.scope_mode, HarnessManagerScopeModeV1::Selected);
        let (future, lead) = add_epic(&f, empty.id, "New platform Epic");
        let live = f.store.get_harness_manager(f.project).unwrap().unwrap();
        assert_eq!(live.epic_ids.len(), 4);
        assert!(live.epic_ids.contains(&future));
        assert_eq!(live.row_version, grant.row_version);
        let sent = f
            .store
            .manager_send(f.manager, &request(future, "future"))
            .unwrap();
        assert_eq!(
            f.store
                .manager_inbox(lead, &AgentManagerInboxRequestV1::default())
                .unwrap()
                .messages[0]
                .message_id,
            sent.message_id
        );
        // Moving an inherited Epic outside selected Groups revokes its route.
        f.store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                params![future.to_string(), other.id.to_string()],
            )
            .unwrap();
        let moved = f.store.get_harness_manager(f.project).unwrap().unwrap();
        assert_eq!(moved.epic_ids, grant.epic_ids);
        assert!(
            f.store
                .manager_inbox(lead, &AgentManagerInboxRequestV1::default())
                .is_err()
        );
        assert!(
            f.store
                .manager_send(f.manager, &request(future, "moved"))
                .is_err()
        );
        let selected = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![explicit]),
                group_ids: Vec::new(),
                expected_row_version: grant.row_version,
            })
            .unwrap();
        assert_eq!(selected.epic_ids, vec![explicit]);
        assert_eq!(selected.group_ids, Vec::<Uuid>::new());
        assert!(
            f.store
                .manager_inbox(f.leads[0], &AgentManagerInboxRequestV1::default())
                .is_err()
        );
    }

    #[test]
    fn harness_manager_project_scope_pages_more_than_32_epics_and_defers_notice_overflow() {
        let f = fixture(Store::open_in_memory().unwrap());
        let group = f
            .store
            .get_session(f.epics[0])
            .unwrap()
            .unwrap()
            .parent_id
            .unwrap();
        for n in 0..65 {
            add_epic(&f, group, &format!("Project feature {n}"));
        }
        let config = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: None,
                group_ids: Vec::new(),
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        assert_eq!(config.scope_mode, HarnessManagerScopeModeV1::Project);
        assert_eq!(config.epic_ids.len(), 67);
        assert_eq!(config.explicit_epic_ids(), &[] as &[Uuid]);
        let mut page = AgentManagerProgressRequestV1 {
            after_epic_id: None,
            limit: Some(16),
        };
        let mut seen = Vec::new();
        loop {
            let result = f.store.manager_progress_page(f.manager, &page).unwrap();
            assert!(result.rows.len() <= 16);
            seen.extend(result.rows.iter().map(|row| row.epic_id));
            page.after_epic_id = result.next_after_epic_id;
            if page.after_epic_id.is_none() {
                break;
            }
        }
        assert_eq!(seen, config.epic_ids);
        let enabled = || {
            f.store
                .list_scheduled_jobs()
                .unwrap()
                .into_iter()
                .filter(|job| job.enabled && job.wake_session_id == Some(f.manager))
                .count()
        };
        assert_eq!(enabled(), MAX_NOTICE_EPICS_PER_PASS);
        f.store.reconcile_harness_manager_watches().unwrap();
        assert_eq!(enabled(), 64);
        let source = f.store.manager_lead(f.project, config.epic_ids[0]).unwrap();
        let last = (config.epic_ids[0], source.id);
        let watch = f
            .store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .find(|job| {
                job.enabled
                    && job.wake_session_id == Some(f.manager)
                    && job.wake_mode == WakeMode::OnTerminal(source.id)
            })
            .unwrap();
        f.store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                [watch.id.to_string()],
            )
            .unwrap();
        // Treat this watch as delivered. Deferred sources occupy its free slot.
        f.store.reconcile_harness_manager_watches().unwrap();
        assert_eq!(enabled(), 64);
        // Durable reply delivery does not require another available watch slot.
        let sent = f
            .store
            .manager_send(f.manager, &request(last.0, "overflow"))
            .unwrap();
        let reply = f
            .store
            .manager_reply(
                last.1,
                &AgentManagerReplyRequestV1 {
                    request_id: sent.message_id,
                    message: "Ready for review".into(),
                    idempotency_key: "overflow-reply".into(),
                },
            )
            .unwrap();
        let inbox = f
            .store
            .manager_inbox(f.manager, &AgentManagerInboxRequestV1::default())
            .unwrap();
        assert!(
            inbox
                .messages
                .iter()
                .any(|message| message.request_id == Some(sent.message_id)
                    && message.sender_session_id == last.1
                    && message.message == "Ready for review")
        );
        f.store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE wake_session_id=?1",
                [f.manager.to_string()],
            )
            .unwrap();
        f.store.reconcile_harness_manager_watches().unwrap();
        assert!(enabled() >= 3);
        let settled_for_watch: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE job_id=?1 AND settled_at IS NOT NULL",
                [watch.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(settled_for_watch >= 1);
        let pending_reply_for_watch: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE job_id=?1 AND kind='message' AND subject_id=?2
                   AND settled_at IS NULL",
                params![watch.id.to_string(), reply.message_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending_reply_for_watch, 1);
        assert!(
            f.store
                .get_scheduled_job(&watch.id)
                .unwrap()
                .unwrap()
                .enabled,
            "the pending exact reply re-arms its transport while retrieved subjects stay settled"
        );
        let future_group = add_group(&f, "Future project group");
        let (future, _) = add_epic(&f, future_group.id, "Future project feature");
        let refreshed = f.store.get_harness_manager(f.project).unwrap().unwrap();
        assert_eq!(refreshed.epic_ids.len(), 68);
        assert!(refreshed.epic_ids.contains(&future));
    }

    #[test]
    fn group_scope_notice_discovery_advances_in_fixed_indexed_cursor_pages() {
        let f = fixture(Store::open_in_memory().unwrap());
        let selected_group = add_group(&f, "Large selected group");
        let mut selected = Vec::new();
        for index in 0..67 {
            selected.push(add_epic(
                &f,
                selected_group.id,
                &format!("Selected feature {index}"),
            ));
        }
        let excluded_group = add_group(&f, "Excluded group");
        let (excluded_epic, _) = add_epic(&f, excluded_group.id, "Excluded feature");
        let config = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(Vec::new()),
                group_ids: vec![selected_group.id],
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        let mut notices: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE project_id=?1 AND scope_version=?2 AND kind='session_state'",
                params![f.project.to_string(), config.row_version],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(notices, MAX_NOTICE_EPICS_PER_PASS as i64);
        while notices < selected.len() as i64 {
            let before = notices;
            f.store.reconcile_harness_manager_watches().unwrap();
            notices = f
                .store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_notices
                     WHERE project_id=?1 AND scope_version=?2 AND kind='session_state'",
                    params![f.project.to_string(), config.row_version],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(notices - before <= MAX_NOTICE_EPICS_PER_PASS as i64);
        }
        assert_eq!(notices, selected.len() as i64);
        let excluded: bool = f
            .store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
                 WHERE project_id=?1 AND scope_version=?2 AND epic_id=?3)",
                params![
                    f.project.to_string(),
                    config.row_version,
                    excluded_epic.to_string()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!excluded);
    }

    #[test]
    fn project_scope_retains_exact_notices_beyond_transport_capacity() {
        let f = fixture(Store::open_in_memory().unwrap());
        let group = f
            .store
            .get_session(f.epics[0])
            .unwrap()
            .unwrap()
            .parent_id
            .unwrap();
        for n in 0..65 {
            add_epic(&f, group, &format!("Deferred feature {n}"));
        }
        let config = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: None,
                group_ids: Vec::new(),
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        let mut notices: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE project_id=?1 AND scope_version=?2 AND kind='session_state'",
                params![f.project.to_string(), config.row_version],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(notices as usize, MAX_NOTICE_EPICS_PER_PASS);
        while notices < config.epic_ids.len() as i64 {
            let previous = notices;
            f.store.reconcile_harness_manager_watches().unwrap();
            notices = f
                .store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_notices
                     WHERE project_id=?1 AND scope_version=?2 AND kind='session_state'",
                    params![f.project.to_string(), config.row_version],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(notices - previous <= MAX_NOTICE_EPICS_PER_PASS as i64);
        }
        assert_eq!(notices as usize, config.epic_ids.len());
        let enabled = || {
            f.store
                .list_scheduled_jobs()
                .unwrap()
                .into_iter()
                .filter(|job| job.enabled && job.wake_session_id == Some(f.manager))
                .count()
        };
        assert_eq!(enabled(), 64);
        let deferred: Vec<(String, String)> = {
            let mut statement = f
                .store
                .conn
                .prepare(
                    "SELECT n.job_id,n.epic_id FROM harness_manager_notices n
                     LEFT JOIN scheduled_jobs j ON j.id=n.job_id
                     WHERE n.project_id=?1 AND n.scope_version=?2 AND j.id IS NULL
                     ORDER BY n.sequence",
                )
                .unwrap();
            statement
                .query_map(params![f.project.to_string(), config.row_version], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(deferred.len(), 3);

        // Deferred durable ownership must not depend on another terminal
        // session-state event. Retire one admitted transport, keep every
        // deferred lead running, and let reconciliation promote the oldest
        // exact pending notice into the free slot.
        for (_, epic) in &deferred {
            let epic = Uuid::parse_str(epic).unwrap();
            let lead = f.store.manager_lead(f.project, epic).unwrap();
            f.store
                .update_session_status(lead.id, SessionStatus::Running)
                .unwrap();
        }
        let retired = f
            .store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .find(|job| job.enabled && job.wake_session_id == Some(f.manager))
            .unwrap();
        let settled_at = stamp();
        f.store
            .conn
            .execute(
                "UPDATE harness_manager_notices SET retrieved_at=?2,settled_at=?2
                 WHERE job_id=?1 AND settled_at IS NULL",
                params![retired.id.to_string(), settled_at],
            )
            .unwrap();
        f.store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                [retired.id.to_string()],
            )
            .unwrap();
        f.store.reconcile_harness_manager_watches().unwrap();
        assert_eq!(enabled(), 64);
        let oldest_deferred = Uuid::parse_str(&deferred[0].0).unwrap();
        assert!(
            f.store
                .get_scheduled_job(&oldest_deferred)
                .unwrap()
                .is_some_and(|job| job.enabled)
        );

        let inbox = f
            .store
            .manager_inbox(
                f.manager,
                &AgentManagerInboxRequestV1 {
                    after_sequence: 0,
                    request_id: None,
                    limit: 1,
                },
            )
            .unwrap();
        assert_eq!(inbox.notices.len(), 1);
        f.store.reconcile_harness_manager_watches().unwrap();
        assert_eq!(enabled(), 64);
        let admitted = Uuid::parse_str(&deferred[0].0).unwrap();
        assert!(
            f.store
                .get_scheduled_job(&admitted)
                .unwrap()
                .is_some_and(|job| job.enabled)
        );
        let pending: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE job_id=?1 AND settled_at IS NULL",
                [admitted.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
    }

    #[test]
    fn harness_manager_scope_discovery_includes_empty_groups_and_rejects_foreign_groups() {
        let f = fixture(Store::open_in_memory().unwrap());
        let empty = add_group(&f, "Empty group identity");
        let foreign = add_group(&f, "Foreign group identity");
        f.store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?2 WHERE id=?1",
                params![foreign.id.to_string(), Uuid::new_v4().to_string()],
            )
            .unwrap();
        let mut request = ListHarnessManagerScopeRequestV1 {
            project_id: f.project,
            after_id: None,
            limit: 1,
        };
        let mut rows = Vec::new();
        loop {
            let page = f.store.list_harness_manager_scope(&request).unwrap();
            assert_eq!(page.rows.len(), 1);
            rows.extend(page.rows);
            request.after_id = page.next_after_id;
            if request.after_id.is_none() {
                break;
            }
        }
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().any(|row| row.id == empty.id
            && row.title == "Empty group identity"
            && row.kind == SessionKind::Group));
        for epic in f.epics {
            assert!(
                rows.iter()
                    .any(|row| row.id == epic && row.group_id.is_some())
            );
        }
        let rejected = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: None,
                group_ids: vec![foreign.id],
                expected_row_version: f.config.row_version,
            });
        assert!(
            rejected
                .unwrap_err()
                .to_string()
                .contains("manager_legal_group_required")
        );
        assert_eq!(
            f.store.get_harness_manager(f.project).unwrap().unwrap(),
            f.config
        );
    }

    #[test]
    fn harness_manager_v110_migration_preserves_legacy_selection_and_revocation() {
        for revoked in [false, true] {
            let f = fixture(Store::open_in_memory().unwrap());
            if revoked {
                f.store
                    .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                        project_id: f.project,
                        session_id: f.manager,
                        epic_ids: Some(Vec::new()),
                        group_ids: Vec::new(),
                        expected_row_version: f.config.row_version,
                    })
                    .unwrap();
            }
            let expected = f.store.get_harness_manager(f.project).unwrap().unwrap();
            super::super::tests::rewind_store_to_schema_version(&f.store.conn, 109);
            f.store.init_schema().unwrap();
            let migrated = f.store.get_harness_manager(f.project).unwrap().unwrap();
            assert_eq!(migrated, expected);
            assert_eq!(migrated.scope_mode, HarnessManagerScopeModeV1::Selected);
            assert_eq!(migrated.is_revoked(), revoked);
        }
    }

    #[test]
    fn harness_manager_group_selection_survives_restart_and_future_enrollment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("groups.db");
        let f = fixture(Store::open(&path).unwrap());
        let group = add_group(&f, "Persisted empty group");
        let grant = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.project,
                session_id: f.manager,
                epic_ids: None,
                group_ids: vec![group.id],
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        let Fixture {
            store,
            project,
            manager,
            epics,
            leads,
            config,
        } = f;
        drop(store);
        let f = Fixture {
            store: Store::open(&path).unwrap(),
            project,
            manager,
            epics,
            leads,
            config,
        };
        assert_eq!(
            f.store.get_harness_manager(project).unwrap().unwrap(),
            grant
        );
        let (future, _) = add_epic(&f, group.id, "After restart feature");
        let refreshed = f.store.get_harness_manager(project).unwrap().unwrap();
        assert_eq!(refreshed.epic_ids, vec![future]);
        assert_eq!(refreshed.group_ids, vec![group.id]);
        assert_eq!(refreshed.row_version, grant.row_version);
    }

    #[test]
    fn harness_manager_picker_pages_all_legal_epics_beyond_selection_limit() {
        let f = fixture(Store::open_in_memory().unwrap());
        let template = f.store.get_session(f.epics[0]).unwrap().unwrap();
        let mut expected = f.epics.to_vec();
        for index in 0..65 {
            let mut epic = template.clone();
            epic.id = Uuid::new_v4();
            epic.title = Some(format!("Unopened feature {index}"));
            f.store.insert_session(&epic).unwrap();
            expected.push(epic.id);
        }
        for (status, parent, project) in [
            (SessionStatus::Archived, template.parent_id, Some(f.project)),
            (SessionStatus::Deleted, template.parent_id, Some(f.project)),
            (SessionStatus::Completed, None, Some(f.project)),
            (
                SessionStatus::Completed,
                template.parent_id,
                Some(Uuid::new_v4()),
            ),
        ] {
            let mut epic = template.clone();
            epic.id = Uuid::new_v4();
            epic.status = status;
            epic.parent_id = parent;
            epic.project_id = project;
            f.store.insert_session(&epic).unwrap();
        }
        expected.sort_unstable();
        let mut request = ListHarnessManagerEpicsRequestV1 {
            project_id: f.project,
            after_id: None,
            limit: 64,
        };
        let first = f.store.list_harness_manager_epics(&request).unwrap();
        assert_eq!(first.epics.len(), 64);
        assert_eq!(first.next_after_id, Some(expected[63]));
        assert!(
            first
                .epics
                .iter()
                .all(|epic| epic.group_title == "Pilot features")
        );
        request.after_id = first.next_after_id;
        let second = f.store.list_harness_manager_epics(&request).unwrap();
        assert_eq!(second.next_after_id, None);
        let listed = first
            .epics
            .into_iter()
            .chain(second.epics)
            .map(|epic| epic.id)
            .collect::<Vec<_>>();
        assert_eq!(listed, expected);
        request.limit = 65;
        assert!(f.store.list_harness_manager_epics(&request).is_err());
    }

    #[test]
    fn harness_manager_two_epic_round_trip_keeps_identity_and_correlation() {
        let f = fixture(Store::open_in_memory().unwrap());
        let progress = f.store.manager_progress(f.manager).unwrap();
        assert_eq!(progress.rows.len(), 2);
        assert!(progress.rows.iter().any(|row| row.title == "Feature 0"));
        assert!(progress.rows.iter().any(|row| row.title == "Feature 1"));
        for index in 0..2 {
            let sent = f
                .store
                .manager_send(
                    f.manager,
                    &request(f.epics[index], &format!("request-{index}")),
                )
                .unwrap();
            let inbox = f
                .store
                .manager_inbox(f.leads[index], &AgentManagerInboxRequestV1::default())
                .unwrap();
            assert_eq!(inbox.messages.len(), 1);
            assert_eq!(inbox.messages[0].message_id, sent.message_id);
            assert_eq!(inbox.messages[0].sender_session_id, f.manager);
            let reply = f
                .store
                .manager_reply(
                    f.leads[index],
                    &AgentManagerReplyRequestV1 {
                        request_id: sent.message_id,
                        message: format!("Feature {index} is ready; evidence: commit abc{index}."),
                        idempotency_key: format!("reply-{index}"),
                    },
                )
                .unwrap();
            assert_eq!(reply.request_id, Some(sent.message_id));
        }
        let inbox = f
            .store
            .manager_inbox(f.manager, &AgentManagerInboxRequestV1::default())
            .unwrap();
        assert_eq!(inbox.messages.len(), 4);
        assert_eq!(
            inbox
                .messages
                .iter()
                .filter(|message| message.request_id.is_some())
                .count(),
            2
        );
        assert!(
            f.store
                .manager_progress(f.manager)
                .unwrap()
                .recent_requests
                .iter()
                .all(|request| request.state == "replied")
        );
        assert_eq!(
            f.store
                .get_session(f.epics[0])
                .unwrap()
                .unwrap()
                .lead_session_id,
            Some(f.leads[0])
        );
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::too_many_lines)]
    fn operator_lead_replacement_reissues_open_mail_once_and_routes_replies() {
        use rsi_common::harness_manager_v2::{
            AgentManagerInspectRequestV2, ManagerInspectSectionV2,
        };
        let f = fixture(Store::open_in_memory().unwrap());
        let original = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "replace-open"))
            .unwrap();
        let mut next = f.store.get_session(f.leads[0]).unwrap().unwrap();
        next.id = Uuid::new_v4();
        next.continued_from = None;
        f.store.insert_session(&next).unwrap();
        let jobs = f
            .store
            .set_epic_lead_and_readdress(f.epics[0], Some(next.id))
            .unwrap();
        assert!(!jobs.is_empty());
        assert!(
            f.store
                .set_epic_lead_and_readdress(f.epics[0], Some(next.id))
                .unwrap()
                .is_empty()
        );
        let inbox = f
            .store
            .manager_inbox(next.id, &AgentManagerInboxRequestV1::default())
            .unwrap();
        assert_eq!(inbox.messages.len(), 1);
        let new_id = inbox.messages[0].message_id;
        assert_eq!(
            inbox.messages[0].message,
            "Report readiness with verification evidence."
        );
        assert_eq!(
            inbox.messages[0].readdressed_from,
            Some(original.message_id)
        );
        let stale = f
            .store
            .manager_reply(
                next.id,
                &AgentManagerReplyRequestV1 {
                    request_id: original.message_id,
                    message: "old correlation".into(),
                    idempotency_key: "old-reply".into(),
                },
            )
            .unwrap_err();
        assert!(format!("{stale}").contains("manager_request_readdressed"));
        assert!(format!("{stale}").contains(&new_id.to_string()));
        let unanswered = f
            .store
            .manager_v2_request_rows(&f.config, Some(f.epics[0]), "", 32, true)
            .unwrap();
        assert!(
            unanswered
                .iter()
                .any(|row| row["request_id"] == new_id.to_string())
        );
        f.store
            .manager_reply(
                next.id,
                &AgentManagerReplyRequestV1 {
                    request_id: new_id,
                    message: "new lead evidence".into(),
                    idempotency_key: "new-reply".into(),
                },
            )
            .unwrap();
        let manager_inbox = f
            .store
            .manager_inbox(f.manager, &AgentManagerInboxRequestV1::default())
            .unwrap();
        assert_eq!(
            manager_inbox
                .messages
                .iter()
                .filter(|m| m.request_id == Some(new_id))
                .count(),
            1
        );
        assert_eq!(
            manager_inbox
                .messages
                .iter()
                .find(|m| m.request_id == Some(new_id))
                .unwrap()
                .readdressed_from,
            Some(original.message_id)
        );
        let progress = f.store.manager_progress(f.manager).unwrap();
        assert!(
            progress
                .recent_requests
                .iter()
                .any(|r| r.request_id == original.message_id
                    && r.state == "readdressed"
                    && r.readdressed_to == Some(new_id))
        );
        assert!(
            progress
                .recent_requests
                .iter()
                .any(|r| r.request_id == new_id && r.state == "replied")
        );
        let rows = f
            .store
            .manager_v2_inspect(
                f.manager,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Requests,
                    ..Default::default()
                },
            )
            .unwrap()
            .rows;
        let old = rows
            .iter()
            .find(|r| r["request_id"] == original.message_id.to_string())
            .unwrap();
        let new = rows
            .iter()
            .find(|r| r["request_id"] == new_id.to_string())
            .unwrap();
        assert_eq!(old["state"], "readdressed");
        assert_eq!(old["readdressed_to"], new_id.to_string());
        assert_eq!(new["readdressed_from"], original.message_id.to_string());
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::too_many_lines)]
    fn authorized_replacement_preserves_answered_terminal_and_revoked_requests() {
        let f = fixture(Store::open_in_memory().unwrap());
        let answered = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "answered"))
            .unwrap();
        f.store
            .manager_reply(
                f.leads[0],
                &AgentManagerReplyRequestV1 {
                    request_id: answered.message_id,
                    message: "done".into(),
                    idempotency_key: "answered-reply".into(),
                },
            )
            .unwrap();
        let terminal = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "terminal"))
            .unwrap();
        f.store
            .manager_v2_put_record(
                &f.config,
                "request",
                &terminal.message_id.to_string(),
                Some(f.epics[0]),
                0,
                &serde_json::json!({"state":"completed"}),
            )
            .unwrap();
        let open = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "still-open"))
            .unwrap();
        let mut next = f.store.get_session(f.leads[0]).unwrap().unwrap();
        next.id = Uuid::new_v4();
        next.continued_from = None;
        f.store.insert_session(&next).unwrap();
        let jobs = f
            .store
            .set_epic_lead_and_readdress(f.epics[0], Some(next.id))
            .unwrap();
        assert!(!jobs.is_empty());
        let inbox = f
            .store
            .manager_inbox(next.id, &AgentManagerInboxRequestV1::default())
            .unwrap();
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.messages[0].readdressed_from, Some(open.message_id));
        assert!(
            f.store
                .manager_request_replied(answered.message_id)
                .unwrap()
        );
        let rows = f
            .store
            .manager_v2_request_rows(&f.config, Some(f.epics[0]), "", 32, false)
            .unwrap();
        assert_eq!(
            rows.iter()
                .find(|r| r["request_id"] == terminal.message_id.to_string())
                .unwrap()["state"],
            "completed"
        );
        let revoked = f
            .store
            .manager_send(f.manager, &request(f.epics[1], "revoked"))
            .unwrap();
        let changed = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![f.epics[0]]),
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        let mut other = f.store.get_session(f.leads[1]).unwrap().unwrap();
        other.id = Uuid::new_v4();
        other.continued_from = None;
        f.store.insert_session(&other).unwrap();
        assert!(
            f.store
                .set_epic_lead_and_readdress(f.epics[1], Some(other.id))
                .unwrap()
                .is_empty()
        );
        assert!(
            f.store
                .manager_request_readdressed(&changed, revoked.message_id)
                .unwrap()
                .is_none()
        );
        assert!(
            f.store
                .manager_progress(f.manager)
                .unwrap()
                .recent_requests
                .iter()
                .any(|r| r.request_id == revoked.message_id && r.state == "scope_revoked")
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn raw_lead_link_does_not_reissue_mail_without_authorized_change() {
        let f = fixture(Store::open_in_memory().unwrap());
        let sent = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "raw-link"))
            .unwrap();
        let mut next = f.store.get_session(f.leads[0]).unwrap().unwrap();
        next.id = Uuid::new_v4();
        next.continued_from = None;
        f.store.insert_session(&next).unwrap();
        f.store.set_lead_session(f.epics[0], Some(next.id)).unwrap();
        // #664 (d): the raw link is not readdressed; Progress's orphan sweep
        // settles the unanswerable request as `lead_replaced` instead.
        assert_eq!(
            f.store.manager_progress(f.manager).unwrap().recent_requests[0].state,
            "settled"
        );
        assert_eq!(
            f.store
                .manager_request_settlement(&f.config, sent.message_id)
                .unwrap()
                .as_deref(),
            Some("lead_replaced")
        );
        assert!(
            f.store
                .manager_request_readdressed(&f.config, sent.message_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn harness_manager_config_cas_and_non_lead_calls_preserve_scope() {
        let f = fixture(Store::open_in_memory().unwrap());
        assert!(
            f.store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    group_ids: Vec::new(),
                    project_id: f.project,
                    session_id: f.manager,
                    epic_ids: Some(vec![]),
                    expected_row_version: 0,
                })
                .is_err()
        );
        let mut worker = f.store.get_session(f.leads[0]).unwrap().unwrap();
        worker.id = Uuid::new_v4();
        f.store.insert_session(&worker).unwrap();
        assert!(f.store.manager_progress(worker.id).is_err());
        assert!(
            f.store
                .manager_inbox(worker.id, &AgentManagerInboxRequestV1::default())
                .is_err()
        );
        assert!(
            f.store
                .manager_send(f.leads[0], &request(f.epics[1], "unauthorized"))
                .is_err()
        );
        assert_eq!(
            f.store.get_harness_manager(f.project).unwrap().unwrap(),
            f.config
        );
        assert!(
            f.store
                .manager_inbox(f.manager, &AgentManagerInboxRequestV1::default())
                .unwrap()
                .messages
                .is_empty()
        );
    }

    #[test]
    fn harness_manager_revoke_and_regrant_does_not_revive_old_exchange_or_wakes() {
        let f = fixture(Store::open_in_memory().unwrap());
        let sent = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "one"))
            .unwrap();
        let jobs = f.store.list_scheduled_jobs().unwrap();
        let revoked = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![]),
                expected_row_version: 1,
            })
            .unwrap();
        assert!(
            f.store
                .manager_inbox(f.leads[0], &AgentManagerInboxRequestV1::default())
                .is_err()
        );
        assert!(
            f.store
                .manager_reply(
                    f.leads[0],
                    &AgentManagerReplyRequestV1 {
                        request_id: sent.message_id,
                        message: "reply".into(),
                        idempotency_key: "reply".into(),
                    }
                )
                .is_err()
        );
        for job in jobs {
            assert!(
                !f.store
                    .harness_manager_wake_authorized(job.id, job.wake_session_id.unwrap())
                    .unwrap()
            );
        }
        f.store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(f.epics.to_vec()),
                expected_row_version: revoked.row_version,
            })
            .unwrap();
        assert!(
            f.store
                .manager_inbox(f.leads[0], &AgentManagerInboxRequestV1::default())
                .unwrap()
                .messages
                .is_empty()
        );
        let progress = f.store.manager_progress(f.manager).unwrap();
        assert_eq!(progress.recent_requests[0].state, "scope_revoked");
        let replay = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "one"))
            .unwrap();
        assert!(replay.deduplicated);
        assert_eq!(replay.message_id, sent.message_id);
    }

    #[test]
    fn harness_manager_current_session_id_is_the_manager_classification_source() {
        let f = fixture(Store::open_in_memory().unwrap());
        assert_eq!(f.config.current_session_id, Some(f.manager));

        let mut fresh = f.store.get_session(f.manager).unwrap().unwrap();
        fresh.id = Uuid::new_v4();
        fresh.continued_from = Some(f.manager);
        f.store.insert_session(&fresh).unwrap();
        assert_eq!(
            f.store
                .get_harness_manager(f.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(f.manager),
        );

        let mut successor = fresh.clone();
        successor.id = Uuid::new_v4();
        successor.rotation_depth += 1;
        f.store.insert_session(&successor).unwrap();
        f.store
            .update_session_status(f.manager, SessionStatus::Archived)
            .unwrap();
        f.store
            .record_harness_manager_rotation(f.manager, successor.id)
            .unwrap();
        assert_eq!(
            f.store
                .get_harness_manager(f.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(successor.id),
        );

        let cleared = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(Vec::new()),
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        assert_eq!(cleared.current_session_id, Some(successor.id));
        assert_eq!(
            f.store
                .get_harness_manager(f.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(successor.id),
        );
    }

    #[test]
    fn harness_manager_rotation_follows_lineage_but_replacement_refuses_old_request() {
        let f = fixture(Store::open_in_memory().unwrap());
        let sent = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "one"))
            .unwrap();
        let mut next = f.store.get_session(f.leads[0]).unwrap().unwrap();
        next.id = Uuid::new_v4();
        next.continued_from = Some(f.leads[0]);
        next.rotation_depth += 1;
        f.store.insert_session(&next).unwrap();
        f.store
            .update_session_status(f.leads[0], SessionStatus::Archived)
            .unwrap();
        f.store
            .record_harness_manager_rotation(f.leads[0], next.id)
            .unwrap();
        f.store.set_lead_session(f.epics[0], Some(next.id)).unwrap();
        assert_eq!(
            f.store
                .manager_inbox(next.id, &AgentManagerInboxRequestV1::default())
                .unwrap()
                .messages[0]
                .message_id,
            sent.message_id
        );
        let mut manager = f.store.get_session(f.manager).unwrap().unwrap();
        manager.id = Uuid::new_v4();
        manager.continued_from = Some(f.manager);
        manager.rotation_depth += 1;
        f.store.insert_session(&manager).unwrap();
        f.store
            .update_session_status(f.manager, SessionStatus::Archived)
            .unwrap();
        f.store
            .record_harness_manager_rotation(f.manager, manager.id)
            .unwrap();
        let replay = f
            .store
            .manager_send(manager.id, &request(f.epics[0], "one"))
            .unwrap();
        assert_eq!(replay.message_id, sent.message_id);
        assert!(replay.deduplicated);
        f.store
            .manager_reply(
                next.id,
                &AgentManagerReplyRequestV1 {
                    request_id: sent.message_id,
                    message: "Current lead reply".into(),
                    idempotency_key: "reply".into(),
                },
            )
            .unwrap();
        assert_eq!(
            f.store
                .manager_inbox(manager.id, &AgentManagerInboxRequestV1::default())
                .unwrap()
                .messages
                .len(),
            2
        );
        let mut unrelated = next.clone();
        unrelated.id = Uuid::new_v4();
        // A generic linked session made Epic lead is a replacement, not a
        // committed rotation. Its new lead rights do not revive old requests.
        unrelated.continued_from = Some(next.id);
        f.store.insert_session(&unrelated).unwrap();
        f.store
            .set_lead_session(f.epics[0], Some(unrelated.id))
            .unwrap();
        assert!(
            f.store
                .manager_reply(
                    unrelated.id,
                    &AgentManagerReplyRequestV1 {
                        request_id: sent.message_id,
                        message: "Unrelated lead".into(),
                        idempotency_key: "other".into()
                    }
                )
                .is_err()
        );
        assert_eq!(
            f.store
                .manager_progress(manager.id)
                .unwrap()
                .recent_requests[0]
                .state,
            "lead_changed"
        );
    }

    #[test]
    fn harness_manager_restart_replays_receipts_and_retains_inbox_and_notices() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manager.sqlite");
        let f = fixture(Store::open(&path).unwrap());
        let sent = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "one"))
            .unwrap();
        let manager = f.manager;
        let lead = f.leads[0];
        let epic = f.epics[0];
        drop(f);
        let store = Store::open(&path).unwrap();
        let replay = store.manager_send(manager, &request(epic, "one")).unwrap();
        assert_eq!(replay.message_id, sent.message_id);
        assert!(replay.deduplicated);
        let mut changed = request(epic, "one");
        changed.message = "Different instruction".into();
        assert!(store.manager_send(manager, &changed).is_err());
        let inbox = store
            .manager_inbox(lead, &AgentManagerInboxRequestV1::default())
            .unwrap();
        assert_eq!(inbox.messages.len(), 1);
        let notice = inbox
            .notices
            .iter()
            .find(|notice| notice.subject_id == sent.message_id.to_string())
            .expect("restart retains exact message notice for attributed retrieval");
        assert_eq!(notice.recipient_session_id, lead);
        assert_eq!(notice.retrieved_at, notice.settled_at);
        let notice_job: String = store
            .conn
            .query_row(
                "SELECT job_id FROM harness_manager_notices WHERE id=?1",
                [notice.notice_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !store
                .get_scheduled_job(&Uuid::parse_str(&notice_job).unwrap())
                .unwrap()
                .unwrap()
                .enabled,
            "retrieval settles the retained notice transport after restart"
        );
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            i64::from(crate::store::LATEST_SCHEMA_VERSION)
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn harness_manager_fresh_lineage_does_not_transfer_authority_but_rotation_does() {
        let f = fixture(Store::open_in_memory().unwrap());
        let mut candidate = f.store.get_session(f.manager).unwrap().unwrap();
        candidate.id = Uuid::new_v4();
        candidate.continued_from = Some(f.manager);
        f.store.insert_session(&candidate).unwrap();
        assert!(f.store.manager_progress(f.manager).is_ok());
        assert!(f.store.manager_progress(candidate.id).is_err());
        // An operator archive alone cannot promote generic Fresh lineage.
        f.store
            .update_session_status(f.manager, SessionStatus::Archived)
            .unwrap();
        assert!(f.store.manager_progress(candidate.id).is_err());
        assert_eq!(f.store.manager_lineage_tip(f.manager).unwrap(), f.manager);
        // A real rotation also advances depth. The archived anchor remains
        // valid for scope edits and the old Fresh sibling is ignored.
        candidate.id = Uuid::new_v4();
        candidate.rotation_depth += 1;
        f.store.insert_session(&candidate).unwrap();
        assert!(
            f.store.manager_progress(candidate.id).is_err(),
            "depth alone is not a receipt"
        );
        f.store
            .record_harness_manager_rotation(f.manager, candidate.id)
            .unwrap();
        assert!(f.store.manager_progress(candidate.id).is_ok());
        let unchanged = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: candidate.id,
                epic_ids: Some(f.epics.to_vec()),
                expected_row_version: 1,
            })
            .unwrap();
        assert_eq!(unchanged.manager_session_id, f.manager);
        assert_eq!(unchanged.row_version, 1);
        let edited = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![f.epics[0]]),
                expected_row_version: 1,
            })
            .unwrap();
        assert_eq!(edited.manager_session_id, f.manager);
        assert_eq!(edited.row_version, 2);
        let mut fork = candidate.clone();
        fork.status = SessionStatus::Failed;
        fork.id = Uuid::new_v4();
        f.store.insert_session(&fork).unwrap();
        assert_eq!(
            f.store.manager_lineage_tip(f.manager).unwrap(),
            candidate.id
        );
        assert_eq!(
            f.store
                .get_harness_manager(f.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(candidate.id)
        );
        // Restoring the predecessor atomically retires its receipt. A later
        // ordinary archive cannot resurrect the old successor's authority.
        f.store
            .update_session_status(f.manager, SessionStatus::Completed)
            .unwrap();
        f.store
            .update_session_status(f.manager, SessionStatus::Archived)
            .unwrap();
        assert!(f.store.manager_progress(candidate.id).is_err());
        let revoked = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(Vec::new()),
                expected_row_version: 2,
            })
            .unwrap();
        assert!(revoked.epic_ids.is_empty());
        assert!(
            f.store
                .list_scheduled_jobs()
                .unwrap()
                .iter()
                .all(|job| !job.enabled)
        );
    }

    #[test]
    fn harness_manager_watch_capacity_failure_rolls_back_message_acceptance() {
        let f = fixture(Store::open_in_memory().unwrap());
        let now = Utc::now();
        for _ in 0..64 {
            f.store
                .insert_scheduled_job(&ScheduledJob {
                    id: Uuid::new_v4(),
                    name: "existing watch".into(),
                    message: "existing".into(),
                    schedule: ScheduleSpec {
                        recurrence: Recurrence::EverySeconds(60),
                        anchor: now,
                    },
                    last_fired_at: None,
                    next_fire_at: now,
                    enabled: true,
                    working_dir: None,
                    provider: None,
                    model: None,
                    project_id: Some(f.project),
                    created_at: now,
                    updated_at: now,
                    wake_mode: WakeMode::OnTerminal(f.manager),
                    wake_session_id: Some(f.leads[0]),
                })
                .unwrap();
        }
        let error = f
            .store
            .manager_send(f.manager, &request(f.epics[0], "full-watch"))
            .unwrap_err();
        assert!(error.to_string().contains("manager_watch_limit_reached"));
        let inbox = f
            .store
            .manager_inbox(f.manager, &AgentManagerInboxRequestV1::default())
            .unwrap();
        assert!(inbox.messages.is_empty());
    }

    #[test]
    fn harness_manager_cross_project_enrollment_is_refused_without_changes() {
        let f = fixture(Store::open_in_memory().unwrap());
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: "Other project".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        f.store.insert_project(&project).unwrap();
        let mut group = test_session(Uuid::new_v4(), PathBuf::from("/tmp/other"));
        group.session_kind = SessionKind::Group;
        group.project_id = Some(project.id);
        f.store.insert_session(&group).unwrap();
        let mut epic = group.clone();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        f.store.insert_session(&epic).unwrap();
        assert!(
            f.store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    group_ids: Vec::new(),
                    project_id: f.project,
                    session_id: f.manager,
                    epic_ids: Some(vec![epic.id]),
                    expected_row_version: 1,
                })
                .is_err()
        );
        assert_eq!(
            f.store.get_harness_manager(f.project).unwrap().unwrap(),
            f.config
        );
    }

    #[test]
    fn harness_manager_moved_principal_revokes_old_project_inbox_and_wakes() {
        let f = fixture(Store::open_in_memory().unwrap());
        f.store
            .manager_send(f.manager, &request(f.epics[0], "before-move"))
            .unwrap();
        let mut other = f.store.get_project(f.project).unwrap().unwrap();
        other.id = Uuid::new_v4();
        other.name = "Other project".into();
        f.store.insert_project(&other).unwrap();
        f.store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?2 WHERE id=?1",
                params![f.manager.to_string(), other.id.to_string()],
            )
            .unwrap();
        assert_eq!(
            f.store
                .get_harness_manager(f.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            None
        );
        assert!(
            f.store
                .manager_inbox(f.leads[0], &AgentManagerInboxRequestV1::default())
                .is_err()
        );
        assert!(f.store.manager_progress(f.manager).is_err());
        f.store.reconcile_harness_manager_watches().unwrap();
        for job in f.store.list_scheduled_jobs().unwrap() {
            assert_eq!(f.store.harness_manager_watch_route(job.id).unwrap(), None);
        }
        let new_project_config = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: other.id,
                session_id: f.manager,
                epic_ids: Some(Vec::new()),
                expected_row_version: 0,
            })
            .unwrap();
        // The same anchor may now have a new appointment elsewhere. Explicit
        // project identity keeps the old project's clear from changing it.
        let cleared = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(Vec::new()),
                expected_row_version: 1,
            })
            .unwrap();
        assert_eq!(cleared.manager_session_id, f.manager);
        assert_eq!(
            f.store.get_harness_manager(other.id).unwrap().unwrap(),
            new_project_config
        );
        assert!(cleared.epic_ids.is_empty());
        assert!(
            f.store
                .list_scheduled_jobs()
                .unwrap()
                .iter()
                .all(|job| !job.enabled)
        );
    }

    #[test]
    fn harness_manager_deleted_anchor_can_revoke_without_resurrection() {
        let f = fixture(Store::open_in_memory().unwrap());
        f.store
            .update_session_status(f.manager, SessionStatus::Deleted)
            .unwrap();
        let cleared = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(Vec::new()),
                expected_row_version: 1,
            })
            .unwrap();
        assert!(cleared.epic_ids.is_empty());
        assert_eq!(cleared.current_session_id, None);
        assert_eq!(
            f.store.get_session(f.manager).unwrap().unwrap().status,
            SessionStatus::Deleted
        );
        assert!(
            f.store
                .list_scheduled_jobs()
                .unwrap()
                .iter()
                .all(|job| !job.enabled)
        );
    }

    #[test]
    fn harness_manager_inbox_pagination_and_human_question_survive_reads_and_mail() {
        let f = fixture(Store::open_in_memory().unwrap());
        let pending = PendingQuestion {
            questions: vec![QuestionItem {
                question: "May I change the deployment target?".into(),
                header: "Deployment".into(),
                options: vec![],
                multi_select: false,
            }],
        };
        f.store
            .update_session_pending_question_json(
                f.leads[0],
                Some(serde_json::to_string(&pending).unwrap().as_str()),
            )
            .unwrap();
        f.store
            .update_session_status(f.leads[0], SessionStatus::WaitingApproval)
            .unwrap();
        for index in 0..3 {
            f.store
                .manager_send(f.manager, &request(f.epics[0], &format!("page-{index}")))
                .unwrap();
        }
        let first = f
            .store
            .manager_inbox(
                f.leads[0],
                &AgentManagerInboxRequestV1 {
                    limit: 2,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(first.messages.len(), 2);
        let second = f
            .store
            .manager_inbox(
                f.leads[0],
                &AgentManagerInboxRequestV1 {
                    after_sequence: first.next_after_sequence.unwrap(),
                    limit: 2,
                    request_id: None,
                },
            )
            .unwrap();
        assert_eq!(second.messages.len(), 1);
        assert_eq!(second.next_after_sequence, None);
        let lead = f.store.get_session(f.leads[0]).unwrap().unwrap();
        assert_eq!(lead.pending_question, Some(pending));
        assert_eq!(lead.status, SessionStatus::WaitingApproval);
    }

    #[test]
    fn manager_session_scope_reaches_leads_and_descendants_only_for_the_current_manager() {
        let f = fixture(Store::open_in_memory().unwrap());
        // A lead-spawned worker is an Epic sibling of the lead; its own child
        // nests under the worker. Both sit inside the covered Epic.
        let mut worker = f.store.get_session(f.leads[0]).unwrap().unwrap();
        worker.id = Uuid::new_v4();
        worker.parent_id = Some(f.epics[0]);
        f.store.insert_session(&worker).unwrap();
        let mut nested = worker.clone();
        nested.id = Uuid::new_v4();
        nested.parent_id = Some(worker.id);
        f.store.insert_session(&nested).unwrap();

        for (target, epic) in [
            (f.leads[0], f.epics[0]),
            (f.leads[1], f.epics[1]),
            (worker.id, f.epics[0]),
            (nested.id, f.epics[0]),
            (f.epics[0], f.epics[0]),
        ] {
            let scope = f
                .store
                .manager_session_scope(f.manager, target)
                .unwrap()
                .expect("current manager reaches a covered session");
            assert_eq!(scope.epic_id, epic);
            assert_eq!(scope.target.id, target);
            assert_eq!(scope.config.project_id, f.project);
        }

        // Leads and workers never inherit manager reach, even over siblings.
        for caller in [f.leads[0], f.leads[1], worker.id] {
            assert!(
                f.store
                    .manager_session_scope(caller, f.leads[1])
                    .unwrap()
                    .is_none()
            );
        }
        // Self is the ordinary self rule's business, not manager reach.
        assert!(
            f.store
                .manager_session_scope(f.manager, f.manager)
                .unwrap()
                .is_none()
        );
        // An Epic outside the selected scope grants nothing.
        let other_group = add_group(&f, "Unselected");
        let (_other_epic, other_lead) = add_epic(&f, other_group.id, "Unselected epic");
        assert!(
            f.store
                .manager_session_scope(f.manager, other_lead)
                .unwrap()
                .is_none()
        );
        // A session outside any Epic grants nothing.
        assert!(
            f.store
                .manager_session_scope(f.manager, other_group.id)
                .unwrap()
                .is_none()
        );

        // Revocation removes reach immediately.
        f.store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(vec![]),
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        assert!(
            f.store
                .manager_session_scope(f.manager, f.leads[0])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn manager_session_scope_refuses_a_foreign_project_session() {
        let f = fixture(Store::open_in_memory().unwrap());
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: "Other project".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        f.store.insert_project(&project).unwrap();
        let mut foreign = f.store.get_session(f.leads[0]).unwrap().unwrap();
        foreign.id = Uuid::new_v4();
        foreign.project_id = Some(project.id);
        f.store.insert_session(&foreign).unwrap();
        assert!(
            f.store
                .manager_session_scope(f.manager, foreign.id)
                .unwrap()
                .is_none()
        );
    }
}
