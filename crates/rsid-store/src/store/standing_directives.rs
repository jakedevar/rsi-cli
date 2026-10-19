//! Standing operator directives (#2649): storage, authority and delivery reads
//! behind `AgentStandingDirective` and the operator-only
//! `StandingDirectiveSet/Clear/List`.
//!
//! A directive is state on a manager scope (`project:<id>`, `area:<id>` or
//! `portfolio:<id>`), never on a session, so it outlives every seat rotation.
//! Authority: the operator writes any scope. A manager seat writes only to a
//! scope strictly below its own (`Store::tier_ancestors` contains the caller's
//! node) and must name provenance; it cannot write its own scope and no author
//! is read from request JSON. A manager clears a directive of a scope below it
//! only when a manager (not the operator) wrote it. Rows are never deleted:
//! `cleared_at` and `expires_at` end delivery and keep the audit trail.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::manager_tier_routing::{MANAGER_TIER_TARGET_UNKNOWN, ManagerNodeRefV1};
use rsi_common::standing_directives::{
    STANDING_DIRECTIVE_IDEMPOTENCY_CONFLICT, STANDING_DIRECTIVE_LIMIT,
    STANDING_DIRECTIVE_MAX_ACTIVE_PER_SCOPE, STANDING_DIRECTIVE_NOT_AUTHORIZED,
    STANDING_DIRECTIVE_NOT_FOUND, STANDING_DIRECTIVE_OPERATOR, STANDING_DIRECTIVE_SESSION_PREFIX,
    StandingDirectiveV1,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{Store, portfolio_nodes};
use crate::error::{DaemonError, Result};

fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_time(text: &str) -> Result<DateTime<Utc>> {
    super::parse_timestamp(text)
        .map_err(|_| DaemonError::Store("invalid standing directive timestamp".into()))
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text)
        .map_err(|_| DaemonError::Store("invalid standing directive identity".into()))
}

const COLUMNS: &str = "id,scope,body,author,provenance,created_at,expires_at,cleared_at,cleared_by,\
     idempotency_key,request_digest";

/// Active at `?1`: not cleared, and not past its expiry.
const ACTIVE_FILTER: &str = "cleared_at IS NULL AND (expires_at IS NULL OR expires_at > ?1)";

struct RawDirective {
    id: String,
    scope: String,
    body: String,
    author: String,
    provenance: Option<String>,
    created_at: String,
    expires_at: Option<String>,
    cleared_at: Option<String>,
    cleared_by: Option<String>,
    request_digest: String,
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawDirective> {
    Ok(RawDirective {
        id: row.get(0)?,
        scope: row.get(1)?,
        body: row.get(2)?,
        author: row.get(3)?,
        provenance: row.get(4)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        cleared_at: row.get(7)?,
        cleared_by: row.get(8)?,
        request_digest: row.get(10)?,
    })
}

impl RawDirective {
    fn into_directive(self) -> Result<StandingDirectiveV1> {
        Ok(StandingDirectiveV1 {
            id: parse_uuid(&self.id)?,
            scope: self.scope,
            text: self.body,
            author: self.author,
            provenance: self.provenance,
            created_at: parse_time(&self.created_at)?,
            expires_at: self.expires_at.as_deref().map(parse_time).transpose()?,
            cleared_at: self.cleared_at.as_deref().map(parse_time).transpose()?,
            cleared_by: self.cleared_by,
        })
    }
}

/// Who writes a directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StandingDirectiveActor {
    /// The operator (the operator-only RPCs).
    Operator,
    /// A manager seat, resolved from the caller's token by
    /// [`Store::standing_directive_agent_actor`].
    Seat {
        session: Uuid,
        node: ManagerNodeRefV1,
    },
}

impl StandingDirectiveActor {
    fn author(&self) -> String {
        match self {
            Self::Operator => STANDING_DIRECTIVE_OPERATOR.to_string(),
            Self::Seat { session, .. } => format!("{STANDING_DIRECTIVE_SESSION_PREFIX}{session}"),
        }
    }
}

/// One `set` as the store takes it.
#[derive(Debug, Clone, Copy)]
pub struct StandingDirectiveSetInput<'a> {
    pub scope: ManagerNodeRefV1,
    pub text: &'a str,
    pub provenance: Option<&'a str>,
    pub expires_in_secs: Option<u64>,
    pub idempotency_key: &'a str,
}

/// A write's directive and whether the call replayed an earlier one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandingDirectiveWrite {
    pub directive: StandingDirectiveV1,
    pub deduplicated: bool,
}

impl Store {
    /// The actor an agent writes as, derived from its seat. Never from
    /// request JSON.
    ///
    /// # Errors
    /// `standing_directive_not_authorized` for a caller that holds no manager
    /// seat, or a persistence error.
    pub fn standing_directive_agent_actor(&self, caller: Uuid) -> Result<StandingDirectiveActor> {
        let node = self
            .tier_caller_node(caller)?
            .ok_or_else(|| refused(STANDING_DIRECTIVE_NOT_AUTHORIZED))?;
        Ok(StandingDirectiveActor::Seat {
            session: caller,
            node,
        })
    }

    /// The manager scope `caller` holds, for delivery reads. `None` for a
    /// worker or a session that holds no manager seat.
    ///
    /// # Errors
    /// A persistence error.
    pub fn standing_directive_seat_scope(&self, caller: Uuid) -> Result<Option<ManagerNodeRefV1>> {
        self.tier_caller_node(caller)
    }

    fn standing_directive_scope_exists(&self, scope: ManagerNodeRefV1) -> Result<bool> {
        Ok(match scope {
            ManagerNodeRefV1::Project { project_id } => self.get_project(project_id)?.is_some(),
            ManagerNodeRefV1::Portfolio { node_id } => {
                portfolio_nodes::node_row_on(&self.conn, node_id)?.is_some()
            }
            ManagerNodeRefV1::Area { .. } => match self.tier_parent_of(scope) {
                Ok(_) => true,
                Err(DaemonError::InvalidParam(code)) if code == MANAGER_TIER_TARGET_UNKNOWN => {
                    false
                }
                Err(error) => return Err(error),
            },
        })
    }

    /// Whether a seat at `node` is strictly above `target`.
    fn standing_directive_covers(
        &self,
        node: ManagerNodeRefV1,
        target: ManagerNodeRefV1,
    ) -> Result<bool> {
        match self.tier_ancestors(target) {
            Ok(chain) => Ok(chain.contains(&node)),
            Err(DaemonError::InvalidParam(code)) if code == MANAGER_TIER_TARGET_UNKNOWN => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Write a directive: the operator to any scope, a manager seat to a
    /// scope strictly below its own with provenance.
    ///
    /// # Errors
    /// `standing_directive_not_authorized`, `manager_tier_target_unknown`,
    /// `standing_directive_limit`, `standing_directive_idempotency_conflict`,
    /// `standing_directive_invalid_request` or a persistence error.
    pub fn standing_directive_set(
        &self,
        actor: &StandingDirectiveActor,
        input: &StandingDirectiveSetInput<'_>,
        now: DateTime<Utc>,
    ) -> Result<StandingDirectiveWrite> {
        use rsi_common::standing_directives::{
            STANDING_DIRECTIVE_INVALID_REQUEST, STANDING_DIRECTIVE_MAX_PROVENANCE_BYTES,
            STANDING_DIRECTIVE_MAX_TEXT_BYTES, STANDING_DIRECTIVE_MAX_TTL_SECS,
        };
        if input.text.trim().is_empty()
            || input.text.len() > STANDING_DIRECTIVE_MAX_TEXT_BYTES
            || input.text.contains('\0')
            || input.provenance.is_some_and(|p| {
                p.trim().is_empty()
                    || p.len() > STANDING_DIRECTIVE_MAX_PROVENANCE_BYTES
                    || p.contains('\0')
            })
            || input
                .expires_in_secs
                .is_some_and(|secs| secs == 0 || secs > STANDING_DIRECTIVE_MAX_TTL_SECS)
            || input.idempotency_key.trim().is_empty()
        {
            return Err(refused(STANDING_DIRECTIVE_INVALID_REQUEST));
        }
        if let StandingDirectiveActor::Seat { node, .. } = actor {
            if input.provenance.is_none() {
                return Err(refused(STANDING_DIRECTIVE_INVALID_REQUEST));
            }
            if !self.standing_directive_covers(*node, input.scope)? {
                return Err(refused(STANDING_DIRECTIVE_NOT_AUTHORIZED));
            }
        }
        if !self.standing_directive_scope_exists(input.scope)? {
            return Err(refused(MANAGER_TIER_TARGET_UNKNOWN));
        }
        let scope = input.scope.as_ref_text();
        let author = actor.author();
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&serde_json::json!({
                "scope": scope,
                "text": input.text,
                "provenance": input.provenance,
                "expires_in_secs": input.expires_in_secs,
            }))?)
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let existing: Option<RawDirective> = tx
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM standing_directives
                      WHERE author=?1 AND idempotency_key=?2"
                ),
                params![author, input.idempotency_key],
                map_row,
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing.request_digest != digest {
                return Err(refused(STANDING_DIRECTIVE_IDEMPOTENCY_CONFLICT));
            }
            return Ok(StandingDirectiveWrite {
                directive: existing.into_directive()?,
                deduplicated: true,
            });
        }
        let active: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM standing_directives WHERE scope=?2 AND {ACTIVE_FILTER}"),
            params![stamp(now), scope],
            |row| row.get(0),
        )?;
        if usize::try_from(active).unwrap_or(usize::MAX) >= STANDING_DIRECTIVE_MAX_ACTIVE_PER_SCOPE
        {
            return Err(refused(STANDING_DIRECTIVE_LIMIT));
        }
        let id = Uuid::new_v4();
        let expires_at = input
            .expires_in_secs
            .map(|secs| now + Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX / 2)));
        tx.execute(
            "INSERT INTO standing_directives
                (id,scope,body,author,provenance,idempotency_key,request_digest,created_at,expires_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id.to_string(),
                scope,
                input.text,
                author,
                input.provenance,
                input.idempotency_key,
                digest,
                stamp(now),
                expires_at.map(stamp),
            ],
        )?;
        let row: RawDirective = tx.query_row(
            &format!("SELECT {COLUMNS} FROM standing_directives WHERE id=?1"),
            [id.to_string()],
            map_row,
        )?;
        tx.commit()?;
        Ok(StandingDirectiveWrite {
            directive: row.into_directive()?,
            deduplicated: false,
        })
    }

    /// Clear a directive. Clearing an already cleared one returns it
    /// unchanged. An expired but uncleared directive may still be cleared.
    ///
    /// # Errors
    /// `standing_directive_not_found`, `standing_directive_not_authorized`
    /// (a manager outside the scope's ancestors, or any manager clearing an
    /// operator-written directive) or a persistence error.
    pub fn standing_directive_clear(
        &self,
        actor: &StandingDirectiveActor,
        directive_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<StandingDirectiveWrite> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: RawDirective = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM standing_directives WHERE id=?1"),
                [directive_id.to_string()],
                map_row,
            )
            .optional()?
            .ok_or_else(|| refused(STANDING_DIRECTIVE_NOT_FOUND))?;
        if let StandingDirectiveActor::Seat { node, .. } = actor {
            let target = ManagerNodeRefV1::parse_ref_text(&row.scope)
                .ok_or_else(|| DaemonError::Store("invalid standing directive scope".into()))?;
            // An operator-written directive is the operator's to clear; a
            // manager that cannot see the scope gets "not found", not a hint.
            if !self.standing_directive_covers(*node, target)? {
                return Err(refused(STANDING_DIRECTIVE_NOT_FOUND));
            }
            if row.author == STANDING_DIRECTIVE_OPERATOR {
                return Err(refused(STANDING_DIRECTIVE_NOT_AUTHORIZED));
            }
        }
        if row.cleared_at.is_some() {
            return Ok(StandingDirectiveWrite {
                directive: row.into_directive()?,
                deduplicated: true,
            });
        }
        tx.execute(
            "UPDATE standing_directives SET cleared_at=?2, cleared_by=?3
              WHERE id=?1 AND cleared_at IS NULL",
            params![directive_id.to_string(), stamp(now), actor.author()],
        )?;
        let cleared: RawDirective = tx.query_row(
            &format!("SELECT {COLUMNS} FROM standing_directives WHERE id=?1"),
            [directive_id.to_string()],
            map_row,
        )?;
        tx.commit()?;
        Ok(StandingDirectiveWrite {
            directive: cleared.into_directive()?,
            deduplicated: false,
        })
    }

    /// The directives delivered to `scope` at `now`, oldest first.
    ///
    /// # Errors
    /// A persistence error.
    pub fn active_standing_directives(
        &self,
        scope: ManagerNodeRefV1,
        now: DateTime<Utc>,
    ) -> Result<Vec<StandingDirectiveV1>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM standing_directives
              WHERE scope=?2 AND {ACTIVE_FILTER} ORDER BY created_at, id"
        ))?;
        statement
            .query_map(params![stamp(now), scope.as_ref_text()], map_row)?
            .map(|row| row?.into_directive())
            .collect()
    }

    /// The directives delivered to the manager seat `caller` holds; empty for
    /// a session that holds none.
    ///
    /// # Errors
    /// A persistence error.
    pub fn standing_directives_for_seat(
        &self,
        caller: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Vec<StandingDirectiveV1>> {
        match self.standing_directive_seat_scope(caller)? {
            Some(scope) => self.active_standing_directives(scope, now),
            None => Ok(Vec::new()),
        }
    }

    /// Operator listing, oldest first, bounded to 256 rows.
    ///
    /// # Errors
    /// A persistence error.
    pub fn standing_directive_list(
        &self,
        scope: Option<ManagerNodeRefV1>,
        include_inactive: bool,
        now: DateTime<Utc>,
    ) -> Result<Vec<StandingDirectiveV1>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM standing_directives
              WHERE (?2 IS NULL OR scope=?2) AND (?3 OR ({ACTIVE_FILTER}))
              ORDER BY created_at, id LIMIT 256"
        ))?;
        statement
            .query_map(
                params![
                    stamp(now),
                    scope.map(|node| node.as_ref_text()),
                    include_inactive
                ],
                map_row,
            )?
            .map(|row| row?.into_directive())
            .collect()
    }
}

#[cfg(test)]
#[path = "standing_directives_tests.rs"]
mod tests;
