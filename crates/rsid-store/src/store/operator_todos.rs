//! The operator to-do list (#1747): storage, scope derivation, idempotency and
//! the evidence rule behind `AgentOperatorTodoUpsert`, `AgentOperatorTodoClose`
//! and the operator-only `OperatorTodoList/Add/Close`.
//!
//! Authority: any manager node seat (portfolio of any tier, a project's live
//! PM, an area seat) writes the list. The item's scope is derived from the
//! caller's seat (`Store::tier_caller_node`); no scope, project or author is
//! read from request JSON. A project or area seat touches only its own scope;
//! a portfolio seat may also close, by id, an item inside its own coverage (the
//! `global` root covers every item). An agent closes an item only
//! with recorded evidence, and only calls it done when it asserts that it
//! observed the thing being done (the table CHECKs repeat both rules, so no
//! other writer can bypass them). A closed row never changes (a trigger), so the
//! operator's undo (`OperatorTodoReopen`, #2950) writes a fresh open copy.

use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::manager_tier_routing::{MANAGER_TIER_TARGET_UNKNOWN, ManagerNodeRefV1};
use rsi_common::operator_todos::{
    AgentOperatorTodoCloseRequestV1, AgentOperatorTodoUpsertRequestV1, OPERATOR_TODO_CLOSED,
    OPERATOR_TODO_EVIDENCE_REQUIRED, OPERATOR_TODO_IDEMPOTENCY_CONFLICT,
    OPERATOR_TODO_INVALID_REQUEST, OPERATOR_TODO_LIMIT, OPERATOR_TODO_LIST_LIMIT,
    OPERATOR_TODO_MAX_KEY_BYTES, OPERATOR_TODO_MAX_OPEN_PER_SCOPE, OPERATOR_TODO_NOT_AUTHORIZED,
    OPERATOR_TODO_NOT_CLOSED, OPERATOR_TODO_NOT_FOUND, OPERATOR_TODO_NOT_IN_SCOPE,
    OPERATOR_TODO_OPERATOR, OPERATOR_TODO_REOPEN_GRACE_SECS,
    OPERATOR_TODO_REOPEN_NOT_OPERATOR_CLOSED, OPERATOR_TODO_REOPEN_WINDOW_ELAPSED,
    OPERATOR_TODO_SESSION_PREFIX, OPERATOR_TODO_UNDO_WINDOW_SECS, OperatorTodoAddRequestV1,
    OperatorTodoCloseRequestV1, OperatorTodoListResultV1, OperatorTodoReopenRequestV1,
    OperatorTodoStatusV1, OperatorTodoV1,
};
use rsi_common::portfolio_nodes::GLOBAL_TIER_LABEL;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{Store, portfolio_nodes};
use crate::error::{DaemonError, Result};

fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

fn stamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_time(text: &str) -> Result<DateTime<Utc>> {
    super::parse_timestamp(text)
        .map_err(|_| DaemonError::Store("invalid operator todo timestamp".into()))
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|_| DaemonError::Store("invalid operator todo identity".into()))
}

fn digest(value: &serde_json::Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

/// Who writes the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorTodoActor {
    /// The operator (the operator-only RPCs).
    Operator,
    /// A manager node seat, resolved from the caller's token by
    /// [`Store::operator_todo_agent_actor`].
    Seat {
        session: Uuid,
        /// `portfolio:<id>`, `project:<id>` or `area:<id>`.
        scope: String,
        /// The project of a project seat.
        project: Option<Uuid>,
        /// The seat is a portfolio node; it may close by id an item inside its
        /// coverage ([`Store::seat_covers_todo`]), not any item.
        portfolio: bool,
    },
}

impl OperatorTodoActor {
    fn scope(&self) -> String {
        match self {
            Self::Operator => OPERATOR_TODO_OPERATOR.to_string(),
            Self::Seat { scope, .. } => scope.clone(),
        }
    }

    fn author(&self) -> String {
        match self {
            Self::Operator => OPERATOR_TODO_OPERATOR.to_string(),
            Self::Seat { session, .. } => format!("{OPERATOR_TODO_SESSION_PREFIX}{session}"),
        }
    }
}

const COLUMNS: &str = "id,scope,project_id,todo_key,title,how,link,priority,added_by,status,\
    evidence,evidence_observed,closed_by,created_at,updated_at,closed_at";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawTodo> {
    Ok(RawTodo {
        id: row.get(0)?,
        scope: row.get(1)?,
        project_id: row.get(2)?,
        todo_key: row.get(3)?,
        title: row.get(4)?,
        how: row.get(5)?,
        link: row.get(6)?,
        priority: row.get(7)?,
        added_by: row.get(8)?,
        status: row.get(9)?,
        evidence: row.get(10)?,
        evidence_observed: row.get(11)?,
        closed_by: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
        closed_at: row.get(15)?,
    })
}

struct RawTodo {
    id: String,
    scope: String,
    project_id: Option<String>,
    todo_key: String,
    title: String,
    how: String,
    link: Option<String>,
    priority: i64,
    added_by: String,
    status: String,
    evidence: Option<String>,
    evidence_observed: i64,
    closed_by: Option<String>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
}

impl RawTodo {
    fn into_todo(self) -> Result<OperatorTodoV1> {
        Ok(OperatorTodoV1 {
            id: parse_uuid(&self.id)?,
            scope: self.scope,
            project_id: self.project_id.as_deref().map(parse_uuid).transpose()?,
            todo_key: self.todo_key,
            title: self.title,
            how: self.how,
            link: self.link,
            priority: u8::try_from(self.priority)
                .map_err(|_| DaemonError::Store("invalid operator todo priority".into()))?,
            added_by: self.added_by,
            status: OperatorTodoStatusV1::parse(&self.status)
                .ok_or_else(|| DaemonError::Store("invalid operator todo status".into()))?,
            evidence: self.evidence,
            evidence_observed: self.evidence_observed == 1,
            closed_by: self.closed_by,
            created_at: parse_time(&self.created_at)?,
            updated_at: parse_time(&self.updated_at)?,
            closed_at: self.closed_at.as_deref().map(parse_time).transpose()?,
        })
    }
}

/// The result of a write: the item, whether it was created and whether the
/// call replayed an earlier one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorTodoWrite {
    pub todo: OperatorTodoV1,
    pub created: bool,
    pub deduplicated: bool,
}

struct UpsertInput<'a> {
    /// `None`: the operator adds a fresh item.
    todo_key: Option<&'a str>,
    project: Option<Uuid>,
    title: &'a str,
    how: &'a str,
    link: Option<&'a str>,
    priority: u8,
    idempotency_key: &'a str,
}

/// Marker between an item's key and the id fragment of a reopened copy.
const REOPEN_KEY_MARK: &str = "~r";
const REOPEN_KEY_FRAGMENT: usize = 8;

/// The key of the open copy a reopen of `old_key` writes: the original key
/// (any earlier reopen suffix dropped) plus `~r` and the first characters of
/// the copy's id, so it stays unique inside the scope and inside the key cap.
fn reopened_key(old_key: &str, copy: Uuid) -> String {
    let fragment: String = copy
        .simple()
        .to_string()
        .chars()
        .take(REOPEN_KEY_FRAGMENT)
        .collect();
    let base = match old_key.rfind(REOPEN_KEY_MARK) {
        Some(at)
            if old_key.len() - at - REOPEN_KEY_MARK.len() == REOPEN_KEY_FRAGMENT
                && old_key[at + REOPEN_KEY_MARK.len()..]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()) =>
        {
            &old_key[..at]
        }
        _ => old_key,
    };
    let room = OPERATOR_TODO_MAX_KEY_BYTES - REOPEN_KEY_MARK.len() - REOPEN_KEY_FRAGMENT;
    let mut end = base.len().min(room);
    while !base.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{REOPEN_KEY_MARK}{fragment}", &base[..end])
}

/// How a close names its item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorTodoTarget {
    Id(Uuid),
    /// A key inside the actor's own scope.
    Key(String),
}

impl Store {
    /// The scope an agent writes to, derived from its seat. Never from
    /// request JSON.
    ///
    /// # Errors
    /// `operator_todo_not_authorized` for a caller that holds no manager node
    /// seat, or a persistence error.
    pub fn operator_todo_agent_actor(&self, caller: Uuid) -> Result<OperatorTodoActor> {
        let node = self
            .tier_caller_node(caller)?
            .ok_or_else(|| refused(OPERATOR_TODO_NOT_AUTHORIZED))?;
        let (project, portfolio) = match node {
            ManagerNodeRefV1::Project { project_id } => (Some(project_id), false),
            ManagerNodeRefV1::Portfolio { .. } => (None, true),
            ManagerNodeRefV1::Area { .. } => (None, false),
        };
        Ok(OperatorTodoActor::Seat {
            session: caller,
            scope: node.as_ref_text(),
            project,
            portfolio,
        })
    }

    /// Whether the portfolio seat at `seat_scope` may close `row` by id,
    /// derived from the node tree and never from the request (#1762). A
    /// non-portfolio seat covers nothing beyond its own scope; the active
    /// `global` root covers every item; any other portfolio node covers an
    /// item written by itself or a descendant node, a project seat or area
    /// seat of a project it covers, or an operator item bound to a project it
    /// covers. An item with no node or project (an unbound operator item) is
    /// the global root's alone.
    fn seat_covers_todo(&self, seat_scope: &str, row: &RawTodo) -> Result<bool> {
        let Some(seat @ ManagerNodeRefV1::Portfolio { node_id }) =
            ManagerNodeRefV1::parse_ref_text(seat_scope)
        else {
            return Ok(false);
        };
        let is_root = portfolio_nodes::node_grant_on(&self.conn, node_id)?
            .is_some_and(|record| record.parent_node_id.is_none());
        if is_root
            && portfolio_nodes::node_row_on(&self.conn, node_id)?
                .is_some_and(|node| node.tier_label == GLOBAL_TIER_LABEL)
        {
            return Ok(true);
        }
        let target = ManagerNodeRefV1::parse_ref_text(&row.scope).or_else(|| {
            let project = row
                .project_id
                .as_deref()
                .and_then(|p| Uuid::parse_str(p).ok());
            project.map(|project_id| ManagerNodeRefV1::Project { project_id })
        });
        let Some(target) = target else {
            return Ok(false);
        };
        match self.tier_ancestors(target) {
            Ok(chain) => Ok(chain.contains(&seat)),
            // A target that no longer exists is covered by no one.
            Err(DaemonError::InvalidParam(code)) if code == MANAGER_TIER_TARGET_UNKNOWN => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// `AgentOperatorTodoUpsert`: create the item `todo_key` in the caller's
    /// scope, or update an open one.
    ///
    /// # Errors
    /// `operator_todo_invalid_request`, `operator_todo_closed`,
    /// `operator_todo_limit`, `operator_todo_idempotency_conflict` or a
    /// persistence error.
    pub fn operator_todo_upsert(
        &self,
        actor: &OperatorTodoActor,
        request: &AgentOperatorTodoUpsertRequestV1,
    ) -> Result<OperatorTodoWrite> {
        request.validate().map_err(refused)?;
        let project = match actor {
            OperatorTodoActor::Seat { project, .. } => *project,
            OperatorTodoActor::Operator => None,
        };
        self.upsert_inner(
            actor,
            &UpsertInput {
                todo_key: Some(&request.todo_key),
                project,
                title: &request.title,
                how: &request.how,
                link: request.link.as_deref(),
                priority: request.priority.unwrap_or(3),
                idempotency_key: &request.idempotency_key,
            },
        )
    }

    /// `OperatorTodoAdd`: the operator writes a fresh item.
    ///
    /// # Errors
    /// `operator_todo_invalid_request`, `operator_todo_limit`,
    /// `operator_todo_idempotency_conflict` or a persistence error.
    pub fn operator_todo_add(
        &self,
        request: &OperatorTodoAddRequestV1,
    ) -> Result<OperatorTodoWrite> {
        request.validate().map_err(refused)?;
        self.upsert_inner(
            &OperatorTodoActor::Operator,
            &UpsertInput {
                todo_key: None,
                project: request.project_id,
                title: &request.title,
                how: &request.how,
                link: request.link.as_deref(),
                priority: request.priority.unwrap_or(3),
                idempotency_key: &request.idempotency_key,
            },
        )
    }

    /// A daemon-written, operator-scope item (#2451): one open item per
    /// `todo_key`, a replay under the same key changes nothing. It is the
    /// retained notice for a wake whose target is gone and whose project has
    /// no current manager, so the operator sees it in the ordinary to-do list.
    ///
    /// # Errors
    /// `operator_todo_invalid_request`, `operator_todo_closed`,
    /// `operator_todo_limit` or a persistence error.
    pub fn operator_todo_system_notice(
        &self,
        todo_key: &str,
        project: Option<Uuid>,
        title: &str,
        how: &str,
        priority: u8,
    ) -> Result<OperatorTodoWrite> {
        if todo_key.trim().is_empty()
            || todo_key.len() > rsi_common::operator_todos::OPERATOR_TODO_MAX_KEY_BYTES
            || title.trim().is_empty()
            || title.len() > rsi_common::operator_todos::OPERATOR_TODO_MAX_TITLE_BYTES
            || how.trim().is_empty()
            || how.len() > rsi_common::operator_todos::OPERATOR_TODO_MAX_HOW_BYTES
            || !(1..=4).contains(&priority)
        {
            return Err(refused(OPERATOR_TODO_INVALID_REQUEST));
        }
        self.upsert_inner(
            &OperatorTodoActor::Operator,
            &UpsertInput {
                todo_key: Some(todo_key),
                project,
                title,
                how,
                link: None,
                priority,
                idempotency_key: todo_key,
            },
        )
    }

    /// The keys of the open operator-scope items whose key starts with
    /// `prefix` (daemon-written notices, #2384).
    ///
    /// # Errors
    /// A persistence error.
    pub fn operator_todo_open_system_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT todo_key FROM operator_todos
             WHERE scope = ?1 AND status = 'open' AND substr(todo_key, 1, length(?2)) = ?2
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![OPERATOR_TODO_OPERATOR, prefix], |row| {
            row.get::<_, String>(0)
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Closes a daemon-written operator-scope item as done because the daemon
    /// observed the condition clear (#2384: a later cold-archive pass
    /// succeeded). An item that is already closed is left alone.
    ///
    /// # Errors
    /// `operator_todo_not_found` or a persistence error.
    pub fn operator_todo_system_resolve(&self, todo_key: &str, note: &str) -> Result<()> {
        match self.close_inner(
            &OperatorTodoActor::Operator,
            &OperatorTodoTarget::Key(todo_key.to_string()),
            OperatorTodoStatusV1::Done,
            Some(note),
            true,
            None,
        ) {
            Ok(_) => Ok(()),
            Err(error) if error.to_string().contains(OPERATOR_TODO_CLOSED) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn upsert_inner(
        &self,
        actor: &OperatorTodoActor,
        input: &UpsertInput<'_>,
    ) -> Result<OperatorTodoWrite> {
        let scope = actor.scope();
        let fingerprint = digest(&serde_json::json!({
            "op": "upsert",
            "todo_key": input.todo_key,
            "project": input.project,
            "title": input.title,
            "how": input.how,
            "link": input.link,
            "priority": input.priority,
        }))?;
        // #2475: a daemon-written notice may be raised inside the caller's own
        // transaction; then it commits or rolls back with that transaction.
        let tx = if self.conn.is_autocommit() {
            Some(Transaction::new_unchecked(
                &self.conn,
                TransactionBehavior::Immediate,
            )?)
        } else {
            None
        };
        if let Some(todo) = self.receipt_todo(&scope, input.idempotency_key, &fingerprint)? {
            return Ok(OperatorTodoWrite {
                todo,
                created: false,
                deduplicated: true,
            });
        }
        let now = stamp();
        let key = input
            .todo_key
            .map_or_else(|| format!("op-{}", Uuid::new_v4()), str::to_string);
        let existing: Option<RawTodo> = self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM operator_todos WHERE scope=?1 AND todo_key=?2"),
                params![scope, key],
                map_row,
            )
            .optional()?;
        let (id, created) = if let Some(existing) = existing {
            if existing.status != OperatorTodoStatusV1::Open.as_str() {
                return Err(refused(OPERATOR_TODO_CLOSED));
            }
            let unchanged = existing.title == input.title
                && existing.how == input.how
                && existing.link.as_deref() == input.link
                && existing.priority == i64::from(input.priority);
            if !unchanged {
                self.conn.execute(
                    "UPDATE operator_todos SET title=?2,how=?3,link=?4,priority=?5,updated_at=?6 WHERE id=?1",
                    params![existing.id, input.title, input.how, input.link, input.priority, now],
                )?;
            }
            (parse_uuid(&existing.id)?, false)
        } else {
            let open: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM operator_todos WHERE scope=?1 AND status='open'",
                [&scope],
                |row| row.get(0),
            )?;
            if open >= OPERATOR_TODO_MAX_OPEN_PER_SCOPE {
                return Err(refused(OPERATOR_TODO_LIMIT));
            }
            let id = Uuid::new_v4();
            self.conn.execute(
                "INSERT INTO operator_todos(id,scope,project_id,todo_key,title,how,link,priority,added_by,status,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'open',?10,?10)",
                params![
                    id.to_string(),
                    scope,
                    input.project.map(|p| p.to_string()),
                    key,
                    input.title,
                    input.how,
                    input.link,
                    input.priority,
                    actor.author(),
                    now
                ],
            )?;
            (id, true)
        };
        self.record_receipt(&scope, input.idempotency_key, &fingerprint, id, &now)?;
        let todo = self.load_todo(id)?;
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(OperatorTodoWrite {
            todo,
            created,
            deduplicated: false,
        })
    }

    /// `AgentOperatorTodoClose`.
    ///
    /// # Errors
    /// `operator_todo_invalid_request`, `operator_todo_evidence_required`,
    /// `operator_todo_not_found`, `operator_todo_not_in_scope`,
    /// `operator_todo_idempotency_conflict` or a persistence error. An item
    /// that is already closed is not an error (#2974): the reply is the
    /// recorded close, `deduplicated`.
    pub fn operator_todo_agent_close(
        &self,
        actor: &OperatorTodoActor,
        request: &AgentOperatorTodoCloseRequestV1,
    ) -> Result<OperatorTodoWrite> {
        request.validate().map_err(refused)?;
        let target = match (&request.todo_id, &request.todo_key) {
            (Some(id), None) => OperatorTodoTarget::Id(*id),
            (None, Some(key)) => OperatorTodoTarget::Key(key.clone()),
            _ => return Err(refused(OPERATOR_TODO_INVALID_REQUEST)),
        };
        self.close_inner(
            actor,
            &target,
            request.status,
            Some(&request.evidence),
            request.observed,
            Some(&request.idempotency_key),
        )
    }

    /// `OperatorTodoClose`: the operator closes any item.
    ///
    /// # Errors
    /// `operator_todo_invalid_request`, `operator_todo_not_found`,
    /// `operator_todo_closed` (an item closed with a different status) or a
    /// persistence error.
    pub fn operator_todo_operator_close(
        &self,
        request: &OperatorTodoCloseRequestV1,
    ) -> Result<OperatorTodoV1> {
        request.validate().map_err(refused)?;
        let write = self.close_inner(
            &OperatorTodoActor::Operator,
            &OperatorTodoTarget::Id(request.todo_id),
            request.status,
            request.note.as_deref(),
            false,
            None,
        )?;
        Ok(write.todo)
    }

    /// `OperatorTodoReopen` (#2950): undo the operator's own close of an item
    /// within the undo window.
    ///
    /// A closed row is immutable (the `operator_todos_closed_terminal`
    /// trigger), so the item comes back as a fresh open row in the same scope
    /// with the closed one's title, how, link, priority, project, author and
    /// creation time and a new key (`<key>~r<id fragment>`). The closed row is
    /// untouched, a manager that waited on its close was already woken, and a
    /// manager's own close stays final (only `closed_by = operator` reopens).
    /// The `operator_todo_receipts` row `reopen:<closed id>` -> new id is the
    /// history record and makes a repeat of the same undo answer with the same
    /// copy.
    ///
    /// # Errors
    /// `operator_todo_invalid_request`, `operator_todo_not_found`,
    /// `operator_todo_not_closed`, `operator_todo_reopen_not_operator_closed`,
    /// `operator_todo_reopen_window_elapsed`, `operator_todo_limit` or a
    /// persistence error.
    pub fn operator_todo_operator_reopen(
        &self,
        request: &OperatorTodoReopenRequestV1,
    ) -> Result<OperatorTodoWrite> {
        request.validate().map_err(refused)?;
        let principal = OPERATOR_TODO_OPERATOR;
        let receipt_key = format!("reopen:{}", request.todo_id);
        let fingerprint = digest(&serde_json::json!({
            "op": "reopen",
            "target": request.todo_id.to_string(),
        }))?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(todo) = self.receipt_todo(principal, &receipt_key, &fingerprint)? {
            return Ok(OperatorTodoWrite {
                todo,
                created: false,
                deduplicated: true,
            });
        }
        let row: RawTodo = self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM operator_todos WHERE id=?1"),
                [request.todo_id.to_string()],
                map_row,
            )
            .optional()?
            .ok_or_else(|| refused(OPERATOR_TODO_NOT_FOUND))?;
        if row.status == OperatorTodoStatusV1::Open.as_str() {
            return Err(refused(OPERATOR_TODO_NOT_CLOSED));
        }
        if row.closed_by.as_deref() != Some(OPERATOR_TODO_OPERATOR) {
            return Err(refused(OPERATOR_TODO_REOPEN_NOT_OPERATOR_CLOSED));
        }
        let closed_at = row
            .closed_at
            .as_deref()
            .map(parse_time)
            .transpose()?
            .ok_or_else(|| DaemonError::Store("operator todo closed without a time".into()))?;
        let allowed = chrono::Duration::seconds(
            OPERATOR_TODO_UNDO_WINDOW_SECS + OPERATOR_TODO_REOPEN_GRACE_SECS,
        );
        if Utc::now() - closed_at > allowed {
            return Err(refused(OPERATOR_TODO_REOPEN_WINDOW_ELAPSED));
        }
        let open: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM operator_todos WHERE scope=?1 AND status='open'",
            [&row.scope],
            |r| r.get(0),
        )?;
        if open >= OPERATOR_TODO_MAX_OPEN_PER_SCOPE {
            return Err(refused(OPERATOR_TODO_LIMIT));
        }
        let id = Uuid::new_v4();
        let now = stamp();
        self.conn.execute(
            "INSERT INTO operator_todos(id,scope,project_id,todo_key,title,how,link,priority,added_by,status,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'open',?10,?11)",
            params![
                id.to_string(),
                row.scope,
                row.project_id,
                reopened_key(&row.todo_key, id),
                row.title,
                row.how,
                row.link,
                row.priority,
                row.added_by,
                row.created_at,
                now
            ],
        )?;
        self.record_receipt(principal, &receipt_key, &fingerprint, id, &now)?;
        let todo = self.load_todo(id)?;
        tx.commit()?;
        Ok(OperatorTodoWrite {
            todo,
            created: true,
            deduplicated: false,
        })
    }

    fn close_inner(
        &self,
        actor: &OperatorTodoActor,
        target: &OperatorTodoTarget,
        status: OperatorTodoStatusV1,
        evidence: Option<&str>,
        observed: bool,
        idempotency_key: Option<&str>,
    ) -> Result<OperatorTodoWrite> {
        if status == OperatorTodoStatusV1::Open {
            return Err(refused(OPERATOR_TODO_INVALID_REQUEST));
        }
        // The evidence rule, ahead of the table CHECKs that repeat it.
        if matches!(actor, OperatorTodoActor::Seat { .. })
            && (evidence.is_none_or(|e| e.trim().is_empty())
                || (status == OperatorTodoStatusV1::Done && !observed))
        {
            return Err(refused(OPERATOR_TODO_EVIDENCE_REQUIRED));
        }
        let principal = actor.scope();
        let fingerprint = digest(&serde_json::json!({
            "op": "close",
            "target": match target {
                OperatorTodoTarget::Id(id) => id.to_string(),
                OperatorTodoTarget::Key(key) => format!("key:{key}"),
            },
            "status": status.as_str(),
            "evidence": evidence,
            "observed": observed,
        }))?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(key) = idempotency_key {
            if let Some(todo) = self.receipt_todo(&principal, key, &fingerprint)? {
                return Ok(OperatorTodoWrite {
                    todo,
                    created: false,
                    deduplicated: true,
                });
            }
        }
        let row: Option<RawTodo> = match target {
            OperatorTodoTarget::Id(id) => self
                .conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM operator_todos WHERE id=?1"),
                    [id.to_string()],
                    map_row,
                )
                .optional()?,
            OperatorTodoTarget::Key(key) => self
                .conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM operator_todos WHERE scope=?1 AND todo_key=?2"),
                    params![principal, key],
                    map_row,
                )
                .optional()?,
        };
        let row = row.ok_or_else(|| refused(OPERATOR_TODO_NOT_FOUND))?;
        if let OperatorTodoActor::Seat { .. } = actor {
            if row.scope != principal && !self.seat_covers_todo(&principal, &row)? {
                return Err(refused(OPERATOR_TODO_NOT_IN_SCOPE));
            }
        }
        if row.status != OperatorTodoStatusV1::Open.as_str() {
            // The operator's repeat of the same close is a no-op. So is a
            // manager seat's close of an item that is already closed (#2974):
            // the operator, or another seat, usually closed it first, the
            // seat's goal (the item is closed) holds, and the closed row is
            // immutable, so nothing is written and the reply carries the
            // recorded status, closer and evidence rather than the request's.
            if matches!(actor, OperatorTodoActor::Seat { .. })
                || (matches!(actor, OperatorTodoActor::Operator)
                    && row.status == status.as_str()
                    && row.closed_by.as_deref() == Some(OPERATOR_TODO_OPERATOR))
            {
                return Ok(OperatorTodoWrite {
                    todo: row.into_todo()?,
                    created: false,
                    deduplicated: true,
                });
            }
            return Err(refused(OPERATOR_TODO_CLOSED));
        }
        let now = stamp();
        self.conn.execute(
            "UPDATE operator_todos SET status=?2,evidence=?3,evidence_observed=?4,closed_by=?5,closed_at=?6,updated_at=?6 WHERE id=?1",
            params![
                row.id,
                status.as_str(),
                evidence.map(str::trim),
                i64::from(observed),
                actor.author(),
                now
            ],
        )?;
        let id = parse_uuid(&row.id)?;
        if let Some(key) = idempotency_key {
            self.record_receipt(&principal, key, &fingerprint, id, &now)?;
        }
        let todo = self.load_todo(id)?;
        tx.commit()?;
        Ok(OperatorTodoWrite {
            todo,
            created: false,
            deduplicated: false,
        })
    }

    /// `OperatorTodoList`: every open item (most urgent, then newest first)
    /// and the items closed at or after `closed_since`.
    ///
    /// # Errors
    /// A persistence error.
    pub fn operator_todo_list(
        &self,
        closed_since: Option<DateTime<Utc>>,
    ) -> Result<OperatorTodoListResultV1> {
        let limit = i64::try_from(OPERATOR_TODO_LIST_LIMIT + 1).unwrap_or(i64::MAX);
        // Keep each query at its execution sink so protected-DML scanning can
        // prove that listing the operator's items only reads operator_todos.
        let mut statement = self.conn.prepare(&format!(
            // sql-dynamic-ok: only the static COLUMNS list is interpolated
            "SELECT {COLUMNS} FROM operator_todos WHERE status='open'
             ORDER BY priority ASC,created_at DESC,id ASC LIMIT ?1"
        ))?;
        let mut open = statement
            .query_map(params![limit], map_row)?
            .map(|row| row?.into_todo())
            .collect::<Result<Vec<_>>>()?;
        let mut recently_closed = match closed_since {
            Some(since) => {
                let since = since.to_rfc3339_opts(SecondsFormat::Nanos, true);
                let mut statement = self.conn.prepare(&format!(
                    // sql-dynamic-ok: only the static COLUMNS list is interpolated
                    "SELECT {COLUMNS} FROM operator_todos WHERE status<>'open' AND closed_at>=?1
                     ORDER BY closed_at DESC,id ASC LIMIT ?2"
                ))?;
                statement
                    .query_map(params![since, limit], map_row)?
                    .map(|row| row?.into_todo())
                    .collect::<Result<Vec<_>>>()?
            }
            None => Vec::new(),
        };
        let truncated = open.len() > OPERATOR_TODO_LIST_LIMIT
            || recently_closed.len() > OPERATOR_TODO_LIST_LIMIT;
        open.truncate(OPERATOR_TODO_LIST_LIMIT);
        recently_closed.truncate(OPERATOR_TODO_LIST_LIMIT);
        Ok(OperatorTodoListResultV1 {
            open,
            recently_closed,
            truncated,
        })
    }

    /// Whether `owner` (a manager seat) may wait on the item `todo_id`: the
    /// same scope rule as an agent close (#1863). An unknown item, an item in
    /// another scope and a caller with no seat are one indistinguishable
    /// refusal, so a wake cannot probe items outside the caller's coverage.
    ///
    /// # Errors
    /// `wake_when_todo_not_found` or a persistence error.
    pub fn operator_todo_wake_authorize(&self, owner: Uuid, todo_id: Uuid) -> Result<()> {
        use rsi_common::wake_predicate::WAKE_WHEN_TODO_NOT_FOUND;
        let not_found = || refused(WAKE_WHEN_TODO_NOT_FOUND);
        let scope = match self.operator_todo_agent_actor(owner) {
            Ok(actor) => actor.scope(),
            Err(DaemonError::InvalidParam(code)) if code == OPERATOR_TODO_NOT_AUTHORIZED => {
                return Err(not_found());
            }
            Err(error) => return Err(error),
        };
        let row = self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM operator_todos WHERE id=?1"),
                [todo_id.to_string()],
                map_row,
            )
            .optional()?
            .ok_or_else(not_found)?;
        if row.scope != scope && !self.seat_covers_todo(&scope, &row)? {
            return Err(not_found());
        }
        Ok(())
    }

    /// The item as it is now, for a `todo_closed` wake predicate; `None` when
    /// no such item exists.
    ///
    /// # Errors
    /// A persistence error.
    pub fn operator_todo_by_id(&self, id: Uuid) -> Result<Option<OperatorTodoV1>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM operator_todos WHERE id=?1"),
                [id.to_string()],
                map_row,
            )
            .optional()?
            .map(RawTodo::into_todo)
            .transpose()
    }

    fn load_todo(&self, id: Uuid) -> Result<OperatorTodoV1> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM operator_todos WHERE id=?1"),
                [id.to_string()],
                map_row,
            )?
            .into_todo()
    }

    /// The item an earlier call with this key produced, `None` for a new key.
    fn receipt_todo(
        &self,
        principal: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<OperatorTodoV1>> {
        let found: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT request_fingerprint,todo_id FROM operator_todo_receipts WHERE principal=?1 AND idempotency_key=?2",
                params![principal, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match found {
            None => Ok(None),
            Some((recorded, _)) if recorded != fingerprint => {
                Err(refused(OPERATOR_TODO_IDEMPOTENCY_CONFLICT))
            }
            Some((_, id)) => Ok(Some(self.load_todo(parse_uuid(&id)?)?)),
        }
    }

    fn record_receipt(
        &self,
        principal: &str,
        key: &str,
        fingerprint: &str,
        todo: Uuid,
        now: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO operator_todo_receipts(principal,idempotency_key,request_fingerprint,todo_id,created_at)
             VALUES(?1,?2,?3,?4,?5)",
            params![principal, key, fingerprint, todo.to_string(), now],
        )?;
        Ok(())
    }
}

#[cfg(test)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[path = "operator_todos_tests.rs"]
mod tests;
