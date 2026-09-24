//! Bounded, read-only queries over one ready structural generation.
//! Traversal state is transient; the versioned database remains the only fact authority.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};
use uuid::Uuid;

use crate::{
    CodegraphError, CodegraphStore, EvidenceView, FactProvenance, MAX_EVIDENCE_PER_FACT,
    MAX_NAME_BYTES, MAX_PATH_BYTES, MAX_QUERY_LIMIT, MAX_QUERY_OUTPUT_BYTES, NodeKind, NodeView,
    ReadySnapshot, RelationKind, RelationView, Result, UnresolvedReferenceView, node_from_row,
    parse_provenance, parse_relation, parse_unresolved_kind, parse_uuid, read_snapshot,
    validate_path,
};

/// A caller-selected ready generation. Authorization of historical reads belongs to the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotSelector {
    CurrentReady,
    Generation(i64),
    Digest(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceMode {
    Strict,
    Exploratory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    ExactName,
    NameContains,
    ExactPath,
    /// FTS5 seed selection, available after the forward schema migration.
    Fts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFilter {
    pub provenance: ProvenanceMode,
    /// Empty means every relation kind.
    pub relation_kinds: Vec<RelationKind>,
}

impl Default for QueryFilter {
    fn default() -> Self {
        Self {
            provenance: ProvenanceMode::Strict,
            relation_kinds: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryLimits {
    pub max_results: usize,
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_relations: usize,
    pub max_frontier: usize,
    pub max_paths: usize,
    pub max_evidence_per_fact: usize,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub max_output_tokens: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_results: 32,
            max_depth: 3,
            max_nodes: 128,
            max_relations: 256,
            max_frontier: 64,
            max_paths: 4,
            max_evidence_per_fact: 8,
            timeout_ms: 500,
            max_output_bytes: 65_536,
            max_output_tokens: 8_192,
        }
    }
}

impl QueryLimits {
    /// Reject zero or above-ceiling values. A service can narrow these defaults further.
    ///
    /// # Errors
    /// Returns an input or hard-limit error for invalid limits.
    pub fn validate(self) -> Result<Self> {
        for (value, maximum) in [
            (self.max_results, MAX_QUERY_LIMIT),
            (self.max_depth, 8),
            (self.max_nodes, 512),
            (self.max_relations, 1024),
            (self.max_frontier, 256),
            (self.max_paths, 16),
            (self.max_evidence_per_fact, MAX_EVIDENCE_PER_FACT),
            (self.max_output_bytes, MAX_QUERY_OUTPUT_BYTES),
            (self.max_output_tokens, 8192),
        ] {
            if value == 0 {
                return Err(CodegraphError::InvalidInput(
                    "query limits must be positive".into(),
                ));
            }
            if value > maximum {
                return Err(CodegraphError::LimitExceeded {
                    requested: value,
                    maximum,
                });
            }
        }
        if self.timeout_ms == 0 || self.timeout_ms > 5_000 {
            return Err(CodegraphError::InvalidInput(
                "timeout_ms must be 1..=5000".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TruncationReason {
    Results,
    Depth,
    Nodes,
    Relations,
    Frontier,
    Paths,
    Evidence,
    Timeout,
    OutputBytes,
    OutputTokens,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryMeta {
    pub snapshot: ReadySnapshot,
    pub limits: QueryLimits,
    pub complete: bool,
    pub truncation: Vec<TruncationReason>,
    pub returned_nodes: usize,
    pub returned_relations: usize,
    pub returned_evidence: usize,
    pub estimated_output_bytes: usize,
    pub estimated_output_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResponse<T> {
    pub meta: QueryMeta,
    pub value: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphView {
    pub nodes: Vec<NodeView>,
    pub relations: Vec<RelationView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathView {
    pub graph: GraphView,
    /// Other equal-length shortest routes, in deterministic discovery order.
    pub alternatives: Vec<GraphView>,
    pub found: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainQueryView {
    /// `None` only when the node read itself exhausted the query deadline.
    pub node: Option<NodeView>,
    pub relations: Vec<RelationView>,
    pub unresolved: Vec<crate::UnresolvedReferenceView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactChange<T> {
    pub kind: ChangeKind,
    pub before: Option<T>,
    pub after: Option<T>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffView {
    pub nodes: Vec<FactChange<NodeView>>,
    pub relations: Vec<FactChange<RelationView>>,
}

/// One query context pins scope and generation. It never writes the database.
pub struct QuerySession {
    connection: Connection,
    snapshot: ReadySnapshot,
}

type Predecessors = BTreeMap<Uuid, Vec<(Uuid, Uuid)>>;

impl CodegraphStore {
    /// Resolve a ready generation once; later head flips cannot move this session.
    ///
    /// # Errors
    /// Returns an input error for an invalid selector, or a store error if the
    /// requested ready generation is missing or unreadable.
    pub fn query(&self, workspace_id: Uuid, selector: SnapshotSelector) -> Result<QuerySession> {
        // A separate WAL reader holds its own snapshot. Keeping the read
        // transaction open protects this session even if retention removes the
        // generation from a newer database version on another connection.
        let path = self
            .connection
            .path()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                CodegraphError::InvalidInput("query sessions require a file-backed store".into())
            })?;
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.execute_batch("BEGIN")?;
        let snapshot = match selector {
            SnapshotSelector::CurrentReady => {
                let generation: i64 = connection
                    .query_row(
                        "SELECT generation FROM cg_workspace_heads WHERE workspace_id=?1",
                        [workspace_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or(CodegraphError::NoReadySnapshot)?;
                read_snapshot(&connection, workspace_id, generation)?
            }
            SnapshotSelector::Generation(generation) if generation > 0 => {
                read_snapshot(&connection, workspace_id, generation)?
            }
            SnapshotSelector::Generation(_) => {
                return Err(CodegraphError::InvalidInput(
                    "generation must be positive".into(),
                ));
            }
            SnapshotSelector::Digest(digest) => {
                if digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                {
                    return Err(CodegraphError::InvalidInput(
                        "snapshot digest must be lowercase BLAKE3 hex".into(),
                    ));
                }
                let generation: i64 = connection.query_row(
                    "SELECT generation FROM cg_snapshots WHERE workspace_id=?1 AND snapshot_digest=?2 AND ready=1 ORDER BY generation DESC LIMIT 1",
                    params![workspace_id.to_string(), digest], |row| row.get(0),
                ).optional()?.ok_or_else(|| CodegraphError::InvalidInput("ready snapshot digest not found".into()))?;
                read_snapshot(&connection, workspace_id, generation)?
            }
        };
        if snapshot.project_id != self.project_id {
            return Err(CodegraphError::InvalidInput(
                "snapshot project mismatch".into(),
            ));
        }
        Ok(QuerySession {
            connection,
            snapshot,
        })
    }
}

struct Budget {
    limits: QueryLimits,
    started: Instant,
    truncation: BTreeSet<TruncationReason>,
    bytes: usize,
}

impl Budget {
    fn new(limits: QueryLimits) -> Result<Self> {
        Ok(Self {
            limits: limits.validate()?,
            started: Instant::now(),
            truncation: BTreeSet::new(),
            bytes: 0,
        })
    }
    fn timed_out(&mut self) -> bool {
        if self.started.elapsed() >= Duration::from_millis(self.limits.timeout_ms) {
            self.truncation.insert(TruncationReason::Timeout);
            true
        } else {
            false
        }
    }
    fn reserve(&mut self, bytes: usize) -> bool {
        let next = self.bytes.saturating_add(bytes);
        if next > self.limits.max_output_bytes {
            self.truncation.insert(TruncationReason::OutputBytes);
            return false;
        }
        // A byte is a conservative upper bound on UTF-8 token count for rendered data.
        if next > self.limits.max_output_tokens {
            self.truncation.insert(TruncationReason::OutputTokens);
            return false;
        }
        self.bytes = next;
        true
    }
    fn meta(
        &self,
        snapshot: &ReadySnapshot,
        nodes: usize,
        relations: usize,
        evidence: usize,
    ) -> QueryMeta {
        QueryMeta {
            snapshot: snapshot.clone(),
            limits: self.limits,
            complete: self.truncation.is_empty(),
            truncation: self.truncation.iter().copied().collect(),
            returned_nodes: nodes,
            returned_relations: relations,
            returned_evidence: evidence,
            estimated_output_bytes: self.bytes,
            estimated_output_tokens: self.bytes,
        }
    }
}

/// Stop a costly `SQLite` statement even when it has not produced its first row.
/// The guard restores the connection so later requests have independent limits.
struct StatementDeadline<'a> {
    connection: &'a Connection,
}

impl<'a> StatementDeadline<'a> {
    fn new(connection: &'a Connection, budget: &Budget) -> Self {
        let deadline = budget.started + Duration::from_millis(budget.limits.timeout_ms);
        connection.progress_handler(100, Some(move || Instant::now() >= deadline));
        Self { connection }
    }
}

impl Drop for StatementDeadline<'_> {
    fn drop(&mut self) {
        self.connection.progress_handler(0, None::<fn() -> bool>);
    }
}

fn interrupted(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, _) if failure.code == ErrorCode::OperationInterrupted)
}

fn read_or_timeout<T>(read: rusqlite::Result<T>, budget: &mut Budget) -> Result<Option<T>> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(error) if interrupted(&error) => {
            budget.truncation.insert(TruncationReason::Timeout);
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn node_cost(node: &NodeView) -> usize {
    192 + node.name.len()
        + node.span.path.len()
        + node.evidence.iter().map(evidence_cost).sum::<usize>()
}
fn relation_cost(relation: &RelationView) -> usize {
    192 + relation.evidence.iter().map(evidence_cost).sum::<usize>()
}
const fn evidence_cost(evidence: &EvidenceView) -> usize {
    192 + evidence.label.len()
        + evidence.span.path.len()
        + evidence.source_digest.len()
        + evidence.evidence_digest.len()
}

impl QuerySession {
    #[must_use]
    pub const fn snapshot(&self) -> &ReadySnapshot {
        &self.snapshot
    }

    fn node(
        &self,
        id: Uuid,
        budget: &mut Budget,
        mode: ProvenanceMode,
    ) -> Result<Option<NodeView>> {
        let mut node = read_or_timeout(self.connection.query_row(
            "SELECT node_id,kind,name,path,start_byte,end_byte,start_line,start_column,end_line,end_column,provenance FROM cg_nodes WHERE workspace_id=?1 AND generation=?2 AND node_id=?3",
            params![self.snapshot.workspace_id.to_string(), self.snapshot.generation, id.to_string()], node_from_row,
        ).optional(), budget)?.flatten();
        if let Some(ref mut node) = node {
            if mode == ProvenanceMode::Strict && node.provenance != FactProvenance::Extracted {
                return Ok(None);
            }
            self.node_evidence(node, budget)?;
        }
        Ok(node)
    }

    fn node_evidence(&self, node: &mut NodeView, budget: &mut Budget) -> Result<()> {
        node.evidence = self.evidence(node.id, "node", budget)?;
        Ok(())
    }

    fn relation_evidence(&self, relation: &mut RelationView, budget: &mut Budget) -> Result<()> {
        relation.evidence = self.evidence(relation.id, "relation", budget)?;
        Ok(())
    }

    fn evidence(
        &self,
        id: Uuid,
        fact_type: &str,
        budget: &mut Budget,
    ) -> Result<Vec<EvidenceView>> {
        let Some(mut stmt) = read_or_timeout(self.connection.prepare(
            "SELECT label,path,start_byte,end_byte,start_line,start_column,end_line,end_column,source_digest,evidence_digest
             FROM cg_evidence WHERE workspace_id=?1 AND generation=?2 AND fact_id=?3 AND fact_type=?4
             ORDER BY path,start_byte,end_byte,label,ordinal LIMIT ?5",
        ), budget)? else {
            return Ok(Vec::new());
        };
        let rows = stmt.query_map(
            params![
                self.snapshot.workspace_id.to_string(),
                self.snapshot.generation,
                id.to_string(),
                fact_type,
                budget.limits.max_evidence_per_fact + 1
            ],
            |row| {
                Ok(EvidenceView {
                    label: row.get(0)?,
                    span: crate::SourceSpan {
                        path: row.get(1)?,
                        start_byte: row.get(2)?,
                        end_byte: row.get(3)?,
                        start_line: row.get(4)?,
                        start_column: row.get(5)?,
                        end_line: row.get(6)?,
                        end_column: row.get(7)?,
                    },
                    source_digest: row.get(8)?,
                    evidence_digest: row.get(9)?,
                })
            },
        );
        let mut evidence = Vec::new();
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) if interrupted(&error) => {
                budget.truncation.insert(TruncationReason::Timeout);
                return Ok(evidence);
            }
            Err(error) => return Err(error.into()),
        };
        for row in rows {
            if budget.timed_out() {
                break;
            }
            if evidence.len() == budget.limits.max_evidence_per_fact {
                budget.truncation.insert(TruncationReason::Evidence);
                break;
            }
            match row {
                Ok(item) => evidence.push(item),
                Err(error) if interrupted(&error) => {
                    budget.truncation.insert(TruncationReason::Timeout);
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(evidence)
    }

    fn unresolved(&self, owner: Uuid, budget: &mut Budget) -> Result<Vec<UnresolvedReferenceView>> {
        let Some(mut stmt) = read_or_timeout(self.connection.prepare(
            "SELECT kind,owner_id,raw_target,path,start_byte,end_byte,start_line,start_column,end_line,end_column,source_digest,reference_digest
             FROM cg_unresolved_references WHERE workspace_id=?1 AND generation=?2 AND owner_id=?3
             ORDER BY path,start_byte,unresolved_id LIMIT ?4",
        ), budget)? else {
            return Ok(Vec::new());
        };
        let rows = stmt.query_map(
            params![
                self.snapshot.workspace_id.to_string(),
                self.snapshot.generation,
                owner.to_string(),
                budget.limits.max_results + 1
            ],
            |row| {
                Ok(UnresolvedReferenceView {
                    kind: parse_unresolved_kind(&row.get::<_, String>(0)?)?,
                    owner: parse_uuid(&row.get::<_, String>(1)?)?,
                    raw_target: row.get(2)?,
                    span: crate::SourceSpan {
                        path: row.get(3)?,
                        start_byte: row.get(4)?,
                        end_byte: row.get(5)?,
                        start_line: row.get(6)?,
                        start_column: row.get(7)?,
                        end_line: row.get(8)?,
                        end_column: row.get(9)?,
                    },
                    source_digest: row.get(10)?,
                    reference_digest: row.get(11)?,
                })
            },
        );
        let mut unresolved = Vec::new();
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) if interrupted(&error) => {
                budget.truncation.insert(TruncationReason::Timeout);
                return Ok(unresolved);
            }
            Err(error) => return Err(error.into()),
        };
        for row in rows {
            if budget.timed_out() {
                break;
            }
            if unresolved.len() == budget.limits.max_results {
                budget.truncation.insert(TruncationReason::Results);
                break;
            }
            let item = match row {
                Ok(item) => item,
                Err(error) if interrupted(&error) => {
                    budget.truncation.insert(TruncationReason::Timeout);
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            if !budget.reserve(192 + item.raw_target.len() + item.span.path.len()) {
                break;
            }
            unresolved.push(item);
        }
        Ok(unresolved)
    }

    /// Exact, substring, path, or FTS5 seed search.
    ///
    /// # Errors
    /// Returns an input, limit, or `SQLite` error for an invalid or failed read.
    #[allow(clippy::too_many_lines)] // Seed modes, SQL deadline and ordered partial rows share one budget.
    pub fn search(
        &self,
        mode: SearchMode,
        query: &str,
        filter: &QueryFilter,
        limits: QueryLimits,
    ) -> Result<QueryResponse<Vec<NodeView>>> {
        self.search_filtered(mode, query, &[], None, filter, limits)
    }

    /// Apply node-kind and workspace-relative path predicates before the
    /// result limit so filtered searches retain deterministic completeness.
    pub fn search_filtered(
        &self,
        mode: SearchMode,
        query: &str,
        node_kinds: &[NodeKind],
        path_prefix: Option<&str>,
        filter: &QueryFilter,
        limits: QueryLimits,
    ) -> Result<QueryResponse<Vec<NodeView>>> {
        let mut budget = Budget::new(limits)?;
        let _deadline = StatementDeadline::new(&self.connection, &budget);
        if query.is_empty()
            || query.len() > MAX_PATH_BYTES
            || (mode != SearchMode::ExactPath && query.len() > MAX_NAME_BYTES)
        {
            return Err(CodegraphError::InvalidInput(
                "search query is empty or too long".into(),
            ));
        }
        if mode == SearchMode::ExactPath {
            validate_path(query)?;
        }
        if node_kinds.len() > NodeKind::ALL.len() {
            return Err(CodegraphError::InvalidInput("too many node kinds".into()));
        }
        let path_prefix = path_prefix.map(|path| path.strip_suffix('/').unwrap_or(path));
        if let Some(path) = path_prefix {
            validate_path(path)?;
        }
        let pattern = match mode {
            SearchMode::ExactName | SearchMode::ExactPath => query.to_owned(),
            SearchMode::NameContains => format!("%{}%", escape_like(query)),
            SearchMode::Fts => {
                let terms = query.split_whitespace().collect::<Vec<_>>();
                if terms.is_empty()
                    || terms.len() > 8
                    || terms
                        .iter()
                        .any(|term| !term.chars().any(char::is_alphanumeric))
                {
                    return Err(CodegraphError::InvalidInput(
                        "FTS query needs 1..=8 searchable terms".into(),
                    ));
                }
                terms
                    .iter()
                    .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            }
        };
        let sql = match mode {
            SearchMode::ExactName => {
                "SELECT n.node_id,n.kind,n.name,n.path,n.start_byte,n.end_byte,n.start_line,n.start_column,n.end_line,n.end_column,n.provenance FROM cg_nodes n WHERE n.workspace_id=?1 AND n.generation=?2 AND n.name=?3 AND (?4=0 OR n.provenance='Extracted') ORDER BY n.path,n.start_byte,n.node_id LIMIT ?5"
            }
            SearchMode::ExactPath => {
                "SELECT n.node_id,n.kind,n.name,n.path,n.start_byte,n.end_byte,n.start_line,n.start_column,n.end_line,n.end_column,n.provenance FROM cg_nodes n WHERE n.workspace_id=?1 AND n.generation=?2 AND n.path=?3 AND (?4=0 OR n.provenance='Extracted') ORDER BY n.path,n.start_byte,n.node_id LIMIT ?5"
            }
            SearchMode::NameContains => {
                "SELECT n.node_id,n.kind,n.name,n.path,n.start_byte,n.end_byte,n.start_line,n.start_column,n.end_line,n.end_column,n.provenance FROM cg_nodes n WHERE n.workspace_id=?1 AND n.generation=?2 AND n.name LIKE ?3 ESCAPE '\\' AND (?4=0 OR n.provenance='Extracted') ORDER BY n.path,n.start_byte,n.node_id LIMIT ?5"
            }
            SearchMode::Fts => {
                "SELECT n.node_id,n.kind,n.name,n.path,n.start_byte,n.end_byte,n.start_line,n.start_column,n.end_line,n.end_column,n.provenance FROM cg_fts_nodes JOIN cg_nodes n ON n.workspace_id=cg_fts_nodes.workspace_id AND n.generation=cg_fts_nodes.generation AND n.node_id=cg_fts_nodes.node_id WHERE cg_fts_nodes MATCH ?3 AND n.workspace_id=?1 AND n.generation=?2 AND (?4=0 OR n.provenance='Extracted') ORDER BY n.path,n.start_byte,n.node_id LIMIT ?5"
            }
        };
        let kind_clause = if node_kinds.is_empty() {
            String::new()
        } else {
            let kinds = node_kinds
                .iter()
                .map(|kind| format!("'{}'", kind.as_str()))
                .collect::<Vec<_>>()
                .join(",");
            format!(" AND n.kind IN ({kinds})")
        };
        let sql = sql.replacen(
            " ORDER BY",
            &format!(
                "{kind_clause} AND (?6 IS NULL OR n.path=?6 OR n.path LIKE ?7 ESCAPE '\\') ORDER BY"
            ),
            1,
        );
        let prefix_pattern = path_prefix.map(|path| format!("{}/%", escape_like(path)));
        let Some(mut stmt) = read_or_timeout(self.connection.prepare(&sql), &mut budget)? else {
            return Ok(QueryResponse {
                meta: budget.meta(&self.snapshot, 0, 0, 0),
                value: Vec::new(),
            });
        };
        let rows = stmt.query_map(
            params![
                self.snapshot.workspace_id.to_string(),
                self.snapshot.generation,
                pattern,
                i64::from(filter.provenance == ProvenanceMode::Strict),
                budget.limits.max_results + 1,
                path_prefix,
                prefix_pattern,
            ],
            node_from_row,
        );
        let mut nodes = Vec::new();
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) if interrupted(&error) => {
                budget.truncation.insert(TruncationReason::Timeout);
                return Ok(QueryResponse {
                    meta: budget.meta(&self.snapshot, 0, 0, 0),
                    value: nodes,
                });
            }
            Err(error) => return Err(error.into()),
        };
        for row in rows {
            if budget.timed_out() {
                break;
            }
            let mut node = match row {
                Ok(node) => node,
                Err(error) if interrupted(&error) => {
                    budget.truncation.insert(TruncationReason::Timeout);
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            if nodes.len() >= budget.limits.max_results {
                budget.truncation.insert(TruncationReason::Results);
                break;
            }
            self.node_evidence(&mut node, &mut budget)?;
            if !budget.reserve(node_cost(&node)) {
                break;
            }
            nodes.push(node);
        }
        let evidence = nodes.iter().map(|node| node.evidence.len()).sum();
        let meta = budget.meta(&self.snapshot, nodes.len(), 0, evidence);
        Ok(QueryResponse { meta, value: nodes })
    }

    fn edges_for(
        &self,
        id: Uuid,
        direction: Direction,
        filter: &QueryFilter,
        budget: &mut Budget,
    ) -> Result<Vec<RelationView>> {
        let endpoint = match direction {
            Direction::Outgoing => "r.source_id=?3",
            Direction::Incoming => "r.target_id=?3",
            Direction::Both => "(r.source_id=?3 OR r.target_id=?3)",
        };
        let provenance = if filter.provenance == ProvenanceMode::Strict {
            "AND r.provenance='Extracted' AND src.provenance='Extracted' AND dst.provenance='Extracted'"
        } else {
            ""
        };
        let kinds = if filter.relation_kinds.is_empty() {
            String::new()
        } else {
            let items = filter
                .relation_kinds
                .iter()
                .map(|kind| format!("'{}'", kind.as_str()))
                .collect::<Vec<_>>()
                .join(",");
            format!(" AND r.kind IN ({items})")
        };
        let sql = format!(
            "SELECT r.relation_id,r.kind,r.source_id,r.target_id,r.provenance FROM cg_relations r \
             JOIN cg_nodes src ON src.workspace_id=r.workspace_id AND src.generation=r.generation AND src.node_id=r.source_id \
             JOIN cg_nodes dst ON dst.workspace_id=r.workspace_id AND dst.generation=r.generation AND dst.node_id=r.target_id \
             WHERE r.workspace_id=?1 AND r.generation=?2 AND {endpoint} {provenance} {kinds} \
             ORDER BY r.kind,r.source_id,r.target_id,r.relation_id LIMIT ?4"
        );
        let Some(mut stmt) = read_or_timeout(self.connection.prepare(&sql), budget)? else {
            return Ok(Vec::new());
        };
        let rows = stmt.query_map(
            params![
                self.snapshot.workspace_id.to_string(),
                self.snapshot.generation,
                id.to_string(),
                budget.limits.max_relations + 1
            ],
            relation_from_row,
        );
        let mut result = Vec::new();
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) if interrupted(&error) => {
                budget.truncation.insert(TruncationReason::Timeout);
                return Ok(result);
            }
            Err(error) => return Err(error.into()),
        };
        for row in rows {
            if budget.timed_out() {
                break;
            }
            if result.len() == budget.limits.max_relations {
                budget.truncation.insert(TruncationReason::Relations);
                break;
            }
            match row {
                Ok(relation) => result.push(relation),
                Err(error) if interrupted(&error) => {
                    budget.truncation.insert(TruncationReason::Timeout);
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(result)
    }

    /// Explain one relation with its exact evidence and both endpoint nodes.
    /// The relation and endpoints must all pass the pinned provenance policy.
    pub fn explain_relation(
        &self,
        id: Uuid,
        filter: &QueryFilter,
        limits: QueryLimits,
    ) -> Result<QueryResponse<GraphView>> {
        let mut budget = Budget::new(limits)?;
        let _deadline = StatementDeadline::new(&self.connection, &budget);
        let mut relation = read_or_timeout(
            self.connection
                .query_row(
                    "SELECT relation_id,kind,source_id,target_id,provenance FROM cg_relations WHERE workspace_id=?1 AND generation=?2 AND relation_id=?3",
                    params![
                        self.snapshot.workspace_id.to_string(),
                        self.snapshot.generation,
                        id.to_string()
                    ],
                    relation_from_row,
                )
                .optional(),
            &mut budget,
        )?
        .flatten();
        if budget.truncation.contains(&TruncationReason::Timeout) {
            return Ok(QueryResponse {
                meta: budget.meta(&self.snapshot, 0, 0, 0),
                value: GraphView::default(),
            });
        }
        let relation = relation.as_mut().ok_or_else(|| {
            CodegraphError::InvalidInput("relation absent from pinned snapshot".into())
        })?;
        if (filter.provenance == ProvenanceMode::Strict
            && relation.provenance != FactProvenance::Extracted)
            || (!filter.relation_kinds.is_empty()
                && !filter.relation_kinds.contains(&relation.kind))
        {
            return Err(CodegraphError::InvalidInput(
                "relation absent from provenance or kind filter".into(),
            ));
        }
        self.relation_evidence(relation, &mut budget)?;
        if !budget.reserve(relation_cost(relation)) {
            return Err(CodegraphError::LimitExceeded {
                requested: relation_cost(relation),
                maximum: limits.max_output_bytes.min(limits.max_output_tokens),
            });
        }
        let mut nodes = Vec::with_capacity(2);
        for endpoint in [relation.source, relation.target] {
            if nodes.iter().any(|node: &NodeView| node.id == endpoint) {
                continue;
            }
            let node = self
                .node(endpoint, &mut budget, filter.provenance)?
                .ok_or_else(|| {
                    CodegraphError::InvalidInput("relation endpoint unavailable".into())
                })?;
            if !budget.reserve(node_cost(&node)) {
                return Err(CodegraphError::LimitExceeded {
                    requested: node_cost(&node),
                    maximum: limits.max_output_bytes.min(limits.max_output_tokens),
                });
            }
            nodes.push(node);
        }
        let evidence =
            relation.evidence.len() + nodes.iter().map(|node| node.evidence.len()).sum::<usize>();
        Ok(QueryResponse {
            meta: budget.meta(&self.snapshot, nodes.len(), 1, evidence),
            value: GraphView {
                nodes,
                relations: vec![relation.clone()],
            },
        })
    }

    /// Explain one node, including exact evidence and unresolved sites.
    ///
    /// # Errors
    /// Returns an input, limit, or `SQLite` error if the node or read is invalid.
    pub fn explain(
        &self,
        id: Uuid,
        filter: &QueryFilter,
        limits: QueryLimits,
    ) -> Result<QueryResponse<ExplainQueryView>> {
        let mut budget = Budget::new(limits)?;
        let _deadline = StatementDeadline::new(&self.connection, &budget);
        let node = self.node(id, &mut budget, filter.provenance)?;
        if budget.truncation.contains(&TruncationReason::Timeout) {
            return Ok(QueryResponse {
                meta: budget.meta(&self.snapshot, 0, 0, 0),
                value: ExplainQueryView {
                    node: None,
                    relations: Vec::new(),
                    unresolved: Vec::new(),
                },
            });
        }
        let node = node.ok_or_else(|| {
            CodegraphError::InvalidInput(
                "node absent from pinned snapshot or provenance filter".into(),
            )
        })?;
        if !budget.reserve(node_cost(&node)) {
            return Err(CodegraphError::LimitExceeded {
                requested: node_cost(&node),
                maximum: budget
                    .limits
                    .max_output_bytes
                    .min(budget.limits.max_output_tokens),
            });
        }
        let mut relations = Vec::new();
        for mut relation in self.edges_for(id, Direction::Both, filter, &mut budget)? {
            if budget.timed_out() {
                break;
            }
            self.relation_evidence(&mut relation, &mut budget)?;
            if !budget.reserve(relation_cost(&relation)) {
                break;
            }
            relations.push(relation);
        }
        let unresolved = if budget.timed_out() {
            Vec::new()
        } else {
            self.unresolved(id, &mut budget)?
        };
        let evidence = node.evidence.len()
            + relations
                .iter()
                .map(|relation| relation.evidence.len())
                .sum::<usize>();
        let meta = budget.meta(&self.snapshot, 1, relations.len(), evidence);
        Ok(QueryResponse {
            meta,
            value: ExplainQueryView {
                node: Some(node),
                relations,
                unresolved,
            },
        })
    }

    #[allow(clippy::too_many_lines)] // One bounded BFS owns its frontier and output accounting.
    fn walk(
        &self,
        seeds: &[Uuid],
        direction: Direction,
        filter: &QueryFilter,
        limits: QueryLimits,
        target: Option<Uuid>,
    ) -> Result<(QueryResponse<GraphView>, Predecessors)> {
        let mut budget = Budget::new(limits)?;
        let _deadline = StatementDeadline::new(&self.connection, &budget);
        if seeds.is_empty() || seeds.len() > budget.limits.max_frontier {
            return Err(CodegraphError::InvalidInput(
                "seed count must fit the frontier limit".into(),
            ));
        }
        let mut graph = GraphView::default();
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        let mut predecessors = BTreeMap::new();
        let mut depths = BTreeMap::new();
        let mut ordered_seeds = seeds.to_vec();
        ordered_seeds.sort_unstable();
        ordered_seeds.dedup();
        for seed in ordered_seeds {
            let node = self.node(seed, &mut budget, filter.provenance)?;
            if budget.truncation.contains(&TruncationReason::Timeout) {
                break;
            }
            let node = node.ok_or_else(|| {
                CodegraphError::InvalidInput(format!(
                    "seed {seed} absent from pinned snapshot or provenance filter"
                ))
            })?;
            if graph.nodes.len() >= budget.limits.max_nodes {
                budget.truncation.insert(TruncationReason::Nodes);
                break;
            }
            if !budget.reserve(node_cost(&node)) {
                break;
            }
            graph.nodes.push(node);
            visited.insert(seed);
            depths.insert(seed, 0usize);
            queue.push_back((seed, 0usize));
        }
        let mut relation_ids = BTreeSet::new();
        while let Some((current, depth)) = queue.pop_front() {
            if budget.timed_out() {
                break;
            }
            if target == Some(current) {
                break;
            }
            if depth >= budget.limits.max_depth {
                // Only mark depth truncation if a traversable edge actually exists.
                if !self
                    .edges_for(current, direction, filter, &mut budget)?
                    .is_empty()
                {
                    budget.truncation.insert(TruncationReason::Depth);
                }
                continue;
            }
            for mut relation in self.edges_for(current, direction, filter, &mut budget)? {
                if budget.timed_out() {
                    break;
                }
                if relation_ids.contains(&relation.id) {
                    continue;
                }
                if graph.relations.len() >= budget.limits.max_relations {
                    budget.truncation.insert(TruncationReason::Relations);
                    break;
                }
                let other = if relation.source == current {
                    relation.target
                } else {
                    relation.source
                };
                let new_node = if visited.contains(&other) {
                    None
                } else {
                    self.node(other, &mut budget, filter.provenance)?
                };
                if budget.truncation.contains(&TruncationReason::Timeout) {
                    break;
                }
                if !visited.contains(&other) && new_node.is_none() {
                    continue;
                }
                if new_node.is_some() && graph.nodes.len() >= budget.limits.max_nodes {
                    budget.truncation.insert(TruncationReason::Nodes);
                    continue;
                }
                if new_node.is_some() && queue.len() >= budget.limits.max_frontier {
                    budget.truncation.insert(TruncationReason::Frontier);
                    continue;
                }
                self.relation_evidence(&mut relation, &mut budget)?;
                if budget.truncation.contains(&TruncationReason::Timeout) {
                    break;
                }
                let cost = relation_cost(&relation) + new_node.as_ref().map_or(0, node_cost);
                if !budget.reserve(cost) {
                    break;
                }
                relation_ids.insert(relation.id);
                if let Some(node) = new_node {
                    visited.insert(other);
                    depths.insert(other, depth + 1);
                    predecessors.insert(other, vec![(current, relation.id)]);
                    queue.push_back((other, depth + 1));
                    graph.nodes.push(node);
                } else if depths.get(&other) == Some(&(depth + 1)) {
                    predecessors
                        .entry(other)
                        .or_insert_with(Vec::new)
                        .push((current, relation.id));
                }
                graph.relations.push(relation);
            }
        }
        graph.nodes.sort_by(|a, b| {
            (&a.span.path, a.span.start_byte, a.id).cmp(&(&b.span.path, b.span.start_byte, b.id))
        });
        graph.relations.sort_by(relation_order);
        let evidence = graph
            .nodes
            .iter()
            .map(|node| node.evidence.len())
            .sum::<usize>()
            + graph
                .relations
                .iter()
                .map(|relation| relation.evidence.len())
                .sum::<usize>();
        let meta = budget.meta(
            &self.snapshot,
            graph.nodes.len(),
            graph.relations.len(),
            evidence,
        );
        Ok((QueryResponse { meta, value: graph }, predecessors))
    }

    /// Return a bounded directed neighborhood. `max_depth` controls hop count.
    ///
    /// # Errors
    /// Returns an input, limit, or `SQLite` error for an invalid or failed read.
    pub fn neighbors(
        &self,
        start: Uuid,
        direction: Direction,
        filter: &QueryFilter,
        limits: QueryLimits,
    ) -> Result<QueryResponse<GraphView>> {
        self.walk(&[start], direction, filter, limits, None)
            .map(|(response, _)| response)
    }

    /// Return a bounded subgraph from explicit seeds.
    ///
    /// # Errors
    /// Returns an input, limit, or `SQLite` error for an invalid or failed read.
    pub fn subgraph(
        &self,
        seeds: &[Uuid],
        direction: Direction,
        filter: &QueryFilter,
        limits: QueryLimits,
    ) -> Result<QueryResponse<GraphView>> {
        self.walk(seeds, direction, filter, limits, None)
            .map(|(response, _)| response)
    }

    /// Return the deterministic first shortest path under ordered BFS.
    ///
    /// # Errors
    /// Returns an input, limit, or `SQLite` error for an invalid or failed read.
    #[allow(clippy::too_many_lines)] // Shortest-route collection and bound accounting share one result.
    pub fn path(
        &self,
        from: Uuid,
        to: Uuid,
        direction: Direction,
        filter: &QueryFilter,
        limits: QueryLimits,
    ) -> Result<QueryResponse<PathView>> {
        let (mut response, predecessors) =
            self.walk(&[from], direction, filter, limits, Some(to))?;
        let found = (from == to && response.value.nodes.iter().any(|node| node.id == to))
            || predecessors.contains_key(&to);
        let mut alternatives = Vec::new();
        if found {
            let mut routes = Vec::new();
            collect_routes(
                to,
                from,
                &predecessors,
                &mut vec![to],
                &mut Vec::new(),
                &mut routes,
                response.meta.limits.max_paths + 1,
            );
            if routes.len() > response.meta.limits.max_paths {
                response.meta.complete = false;
                response.meta.truncation.push(TruncationReason::Paths);
                routes.truncate(response.meta.limits.max_paths);
            }
            let full = response.value.clone();
            let mut path_graphs = routes.into_iter().map(|(nodes, relations)| GraphView {
                nodes: full
                    .nodes
                    .iter()
                    .filter(|node| nodes.contains(&node.id))
                    .cloned()
                    .collect(),
                relations: full
                    .relations
                    .iter()
                    .filter(|relation| relations.contains(&relation.id))
                    .cloned()
                    .collect(),
            });
            response.value = path_graphs.next().unwrap_or_default();
            for graph in path_graphs {
                let extra = graph.nodes.iter().map(node_cost).sum::<usize>()
                    + graph.relations.iter().map(relation_cost).sum::<usize>();
                let next = response.meta.estimated_output_bytes.saturating_add(extra);
                if next > response.meta.limits.max_output_bytes {
                    response.meta.truncation.push(TruncationReason::OutputBytes);
                    break;
                }
                if next > response.meta.limits.max_output_tokens {
                    response
                        .meta
                        .truncation
                        .push(TruncationReason::OutputTokens);
                    break;
                }
                response.meta.estimated_output_bytes = next;
                response.meta.estimated_output_tokens = next;
                alternatives.push(graph);
            }
            response.meta.truncation.sort_unstable();
            response.meta.truncation.dedup();
            response.meta.complete = response.meta.truncation.is_empty();
            response.meta.returned_nodes = response.value.nodes.len()
                + alternatives.iter().map(|g| g.nodes.len()).sum::<usize>();
            response.meta.returned_relations = response.value.relations.len()
                + alternatives
                    .iter()
                    .map(|g| g.relations.len())
                    .sum::<usize>();
            response.meta.returned_evidence = response
                .value
                .nodes
                .iter()
                .map(|node| node.evidence.len())
                .sum::<usize>()
                + response
                    .value
                    .relations
                    .iter()
                    .map(|relation| relation.evidence.len())
                    .sum::<usize>()
                + alternatives
                    .iter()
                    .map(|graph| {
                        graph
                            .nodes
                            .iter()
                            .map(|node| node.evidence.len())
                            .sum::<usize>()
                            + graph
                                .relations
                                .iter()
                                .map(|relation| relation.evidence.len())
                                .sum::<usize>()
                    })
                    .sum::<usize>();
        }
        Ok(QueryResponse {
            meta: response.meta,
            value: PathView {
                graph: response.value,
                alternatives,
                found,
            },
        })
    }

    /// Reverse dependency reachability, preserving original edge direction.
    ///
    /// # Errors
    /// Returns an input, limit, or `SQLite` error for an invalid or failed read.
    pub fn impact(
        &self,
        changed: Uuid,
        filter: &QueryFilter,
        limits: QueryLimits,
    ) -> Result<QueryResponse<GraphView>> {
        self.neighbors(changed, Direction::Incoming, filter, limits)
    }

    /// Evidence-sensitive diff of two ready generations in the same workspace.
    ///
    /// # Errors
    /// Returns an input, limit, or `SQLite` error for mismatched scope or failed read.
    #[allow(clippy::too_many_lines)] // Both fact families share one bounded diff response.
    pub fn diff(
        &self,
        other: &Self,
        filter: &QueryFilter,
        limits: QueryLimits,
    ) -> Result<QueryResponse<DiffView>> {
        if self.snapshot.project_id != other.snapshot.project_id
            || self.snapshot.workspace_id != other.snapshot.workspace_id
        {
            return Err(CodegraphError::InvalidInput(
                "diff snapshots must share project and workspace".into(),
            ));
        }
        let mut budget = Budget::new(limits)?;
        let _deadline = StatementDeadline::new(&self.connection, &budget);
        let _other_deadline = StatementDeadline::new(&other.connection, &budget);
        let mut changes = DiffView::default();
        let node_ids = union_ids(
            self,
            other,
            "cg_nodes",
            "node_id",
            crate::MAX_FACTS * 2 + 1,
            &mut budget,
        )?
        .unwrap_or_default();
        if node_ids.len() > crate::MAX_FACTS * 2 {
            budget.truncation.insert(TruncationReason::Results);
        }
        for id in node_ids {
            if budget.timed_out() {
                break;
            }
            if changes.nodes.len() + changes.relations.len() >= budget.limits.max_results {
                budget.truncation.insert(TruncationReason::Results);
                break;
            }
            let before = self.node(id, &mut budget, filter.provenance)?;
            if budget.truncation.contains(&TruncationReason::Timeout) {
                break;
            }
            let after = other.node(id, &mut budget, filter.provenance)?;
            if budget.truncation.contains(&TruncationReason::Timeout) {
                break;
            }
            if let Some(kind) = change_kind(before.as_ref(), after.as_ref()) {
                let cost =
                    before.as_ref().map_or(0, node_cost) + after.as_ref().map_or(0, node_cost);
                if !budget.reserve(cost) {
                    break;
                }
                changes.nodes.push(FactChange {
                    kind,
                    before,
                    after,
                });
            }
        }
        let relation_ids = if budget.truncation.contains(&TruncationReason::Timeout) {
            Vec::new()
        } else {
            union_ids(
                self,
                other,
                "cg_relations",
                "relation_id",
                crate::MAX_FACTS * 2 + 1,
                &mut budget,
            )?
            .unwrap_or_default()
        };
        if relation_ids.len() > crate::MAX_FACTS * 2 {
            budget.truncation.insert(TruncationReason::Results);
        }
        for id in relation_ids {
            if budget.timed_out() {
                break;
            }
            if changes.nodes.len() + changes.relations.len() >= budget.limits.max_results {
                budget.truncation.insert(TruncationReason::Results);
                break;
            }
            let before = self.relation(id, filter, &mut budget)?;
            if budget.truncation.contains(&TruncationReason::Timeout) {
                break;
            }
            let after = other.relation(id, filter, &mut budget)?;
            if budget.truncation.contains(&TruncationReason::Timeout) {
                break;
            }
            if let Some(kind) = change_kind(before.as_ref(), after.as_ref()) {
                let cost = before.as_ref().map_or(0, relation_cost)
                    + after.as_ref().map_or(0, relation_cost);
                if !budget.reserve(cost) {
                    break;
                }
                changes.relations.push(FactChange {
                    kind,
                    before,
                    after,
                });
            }
        }
        let evidence = changes
            .nodes
            .iter()
            .map(|change| {
                change.before.as_ref().map_or(0, |node| node.evidence.len())
                    + change.after.as_ref().map_or(0, |node| node.evidence.len())
            })
            .sum::<usize>()
            + changes
                .relations
                .iter()
                .map(|change| {
                    change
                        .before
                        .as_ref()
                        .map_or(0, |relation| relation.evidence.len())
                        + change
                            .after
                            .as_ref()
                            .map_or(0, |relation| relation.evidence.len())
                })
                .sum::<usize>();
        let meta = budget.meta(
            &self.snapshot,
            changes.nodes.len(),
            changes.relations.len(),
            evidence,
        );
        Ok(QueryResponse {
            meta,
            value: changes,
        })
    }

    fn relation(
        &self,
        id: Uuid,
        filter: &QueryFilter,
        budget: &mut Budget,
    ) -> Result<Option<RelationView>> {
        let mut relation = read_or_timeout(self.connection.query_row(
            "SELECT r.relation_id,r.kind,r.source_id,r.target_id,r.provenance FROM cg_relations r \
             JOIN cg_nodes src ON src.workspace_id=r.workspace_id AND src.generation=r.generation AND src.node_id=r.source_id \
             JOIN cg_nodes dst ON dst.workspace_id=r.workspace_id AND dst.generation=r.generation AND dst.node_id=r.target_id \
             WHERE r.workspace_id=?1 AND r.generation=?2 AND r.relation_id=?3 \
             AND (?4=0 OR (r.provenance='Extracted' AND src.provenance='Extracted' AND dst.provenance='Extracted'))",
            params![self.snapshot.workspace_id.to_string(), self.snapshot.generation, id.to_string(), i64::from(filter.provenance == ProvenanceMode::Strict)], relation_from_row,
        ).optional(), budget)?.flatten();
        if let Some(ref mut item) = relation {
            if (filter.provenance == ProvenanceMode::Strict
                && item.provenance != FactProvenance::Extracted)
                || (!filter.relation_kinds.is_empty()
                    && !filter.relation_kinds.contains(&item.kind))
            {
                return Ok(None);
            }
            self.relation_evidence(item, budget)?;
        }
        Ok(relation)
    }
}

fn relation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RelationView> {
    Ok(RelationView {
        id: parse_uuid(&row.get::<_, String>(0)?)?,
        kind: parse_relation(&row.get::<_, String>(1)?)?,
        source: parse_uuid(&row.get::<_, String>(2)?)?,
        target: parse_uuid(&row.get::<_, String>(3)?)?,
        provenance: parse_provenance(&row.get::<_, String>(4)?)?,
        evidence: Vec::new(),
    })
}

fn relation_order(a: &RelationView, b: &RelationView) -> std::cmp::Ordering {
    (a.kind.as_str(), a.source, a.target, a.id).cmp(&(b.kind.as_str(), b.source, b.target, b.id))
}

fn collect_routes(
    cursor: Uuid,
    source: Uuid,
    predecessors: &Predecessors,
    nodes: &mut Vec<Uuid>,
    relations: &mut Vec<Uuid>,
    routes: &mut Vec<(BTreeSet<Uuid>, BTreeSet<Uuid>)>,
    cap: usize,
) {
    if routes.len() >= cap {
        return;
    }
    if cursor == source {
        routes.push((
            nodes.iter().copied().collect(),
            relations.iter().copied().collect(),
        ));
        return;
    }
    if let Some(steps) = predecessors.get(&cursor) {
        for (previous, relation) in steps {
            if routes.len() >= cap {
                break;
            }
            nodes.push(*previous);
            relations.push(*relation);
            collect_routes(
                *previous,
                source,
                predecessors,
                nodes,
                relations,
                routes,
                cap,
            );
            nodes.pop();
            relations.pop();
        }
    }
}

fn change_kind<T: PartialEq>(before: Option<&T>, after: Option<&T>) -> Option<ChangeKind> {
    match (before, after) {
        (None, Some(_)) => Some(ChangeKind::Added),
        (Some(_), None) => Some(ChangeKind::Removed),
        (Some(a), Some(b)) if a != b => Some(ChangeKind::Changed),
        _ => None,
    }
}

fn union_ids(
    first: &QuerySession,
    second: &QuerySession,
    table: &str,
    column: &str,
    limit: usize,
    budget: &mut Budget,
) -> Result<Option<Vec<Uuid>>> {
    // `table` and `column` are fixed internal literals at the only call sites.
    let Some(first_ids) = generation_ids(first, table, column, limit, budget)? else {
        return Ok(None);
    };
    let Some(second_ids) = generation_ids(second, table, column, limit, budget)? else {
        return Ok(None);
    };
    let mut ids = BTreeSet::new();
    for id in first_ids.into_iter().chain(second_ids) {
        if budget.timed_out() {
            return Ok(None);
        }
        ids.insert(id);
    }
    Ok(Some(ids.into_iter().take(limit).collect()))
}

fn generation_ids(
    session: &QuerySession,
    table: &str,
    column: &str,
    limit: usize,
    budget: &mut Budget,
) -> Result<Option<Vec<Uuid>>> {
    if budget.timed_out() {
        return Ok(None);
    }
    let sql = format!(
        "SELECT {column} FROM {table} WHERE workspace_id=?1 AND generation=?2 ORDER BY {column} LIMIT ?3"
    );
    let Some(mut stmt) = read_or_timeout(session.connection.prepare(&sql), budget)? else {
        return Ok(None);
    };
    let Some(rows) = read_or_timeout(
        stmt.query_map(
            params![
                session.snapshot.workspace_id.to_string(),
                session.snapshot.generation,
                limit
            ],
            |row| parse_uuid(&row.get::<_, String>(0)?),
        ),
        budget,
    )?
    else {
        return Ok(None);
    };
    let mut ids = Vec::new();
    for row in rows {
        if budget.timed_out() {
            return Ok(None);
        }
        let Some(id) = read_or_timeout(row, budget)? else {
            return Ok(None);
        };
        ids.push(id);
    }
    Ok(Some(ids))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{ExtractionContract, ExtractionMode, ExtractorIdentity};

    #[test]
    fn interrupted_generation_id_scan_records_timeout() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE cg_nodes(workspace_id TEXT, generation INTEGER, node_id TEXT);
             INSERT INTO cg_nodes VALUES ('00000000-0000-0000-0000-000000000000',1,'00000000-0000-0000-0000-000000000001');",
        ).unwrap();
        let session = QuerySession {
            connection,
            snapshot: ReadySnapshot {
                project_id: Uuid::nil(),
                repository_id: Uuid::nil(),
                workspace_id: Uuid::nil(),
                generation: 1,
                snapshot_digest: String::new(),
                graph_digest: String::new(),
                extraction: ExtractionContract {
                    mode: ExtractionMode::ExtractedV1_0,
                    extractor: ExtractorIdentity {
                        name: "test".into(),
                        version: "1".into(),
                    },
                },
            },
        };
        let mut budget = Budget::new(QueryLimits::default()).unwrap();
        session.connection.progress_handler(1, Some(|| true));
        let ids = generation_ids(&session, "cg_nodes", "node_id", 2, &mut budget).unwrap();
        session.connection.progress_handler(0, None::<fn() -> bool>);
        assert!(ids.is_none());
        assert_eq!(
            budget.meta(&session.snapshot, 0, 0, 0).truncation,
            vec![TruncationReason::Timeout]
        );
    }
}
